//! Provision canister ingress handler foundation (ADR 0035 Slice 3).
//!
//! These are plain `pub(crate)` functions with explicit caller injection so unit tests can
//! drive every authorization and idempotency branch. Callable canister endpoints
//! (`#[init]`/`#[query]`/`#[update]` annotations) remain a follow-up slice.

use candid::{CandidType, Encode, Principal};
use serde::{Deserialize, Serialize};
use sha2::Digest;
use std::collections::HashSet;

use crate::stable::artifact::ProvisionArtifactStore;
use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;
use crate::stable::release::ProvisionReleaseStore;
use crate::stable::store::{DeploymentGrantStore, ProvisionJobStore};
use crate::types::{
    ArtifactChunk, ArtifactChunkKey, ArtifactError, ArtifactId, ArtifactMetadata,
    ArtifactPublishMetadataArgs, ArtifactUpload, ArtifactUploadChunkArgs, ArtifactUploadState,
    BootstrapAuthAction, BootstrapAuthEntry, CanisterKind, CreatedResource, JobState,
    LogicalResource, MAX_ARTIFACT_BYTES, MAX_ARTIFACT_CHUNKS,
    MAX_ARTIFACT_SEMANTIC_VERSION_LEN, ProvisionAdminError, ProvisionJobRecord,
    ProvisionJobRequestKey, ProvisionRequest, ProvisionResult, ProvisionResultOutcome,
    ReleaseActivateArgs, ReleaseActivateResult, ReleaseError, ReleaseId, ReleaseManifest,
    ReleasePublishArgs, ResourceJobEntry, RouterRegistrationAck, RouterRegistrationAckResponse,
    UpsertDeploymentGrantArgs, sha256, state_name,
};
use crate::types::{
    ArtifactAuditAction, ArtifactAuditEntry, ArtifactAuditOutcome, InstallError,
    ReleaseInstallArgs, ReleaseInstallResult,
};

pub mod handlers;
pub mod init;

/// Append one artifact/release audit row to PROVISION_ARTIFACT_AUDIT_LOG (MemoryId 11).
#[allow(clippy::too_many_arguments)]
fn append_artifact_audit(
    caller: Principal,
    action: ArtifactAuditAction,
    artifact_id: Option<ArtifactId>,
    release_id: Option<ReleaseId>,
    target_canister: Option<Principal>,
    outcome: ArtifactAuditOutcome,
    reason: Option<String>,
    timestamp_ns: u64,
) {
    let entry = ArtifactAuditEntry {
        caller,
        action,
        artifact_id,
        release_id,
        deployment_id: None,
        target_canister,
        timestamp_ns,
        outcome,
        reason,
    };
    ProvisionArtifactStore::new().append_audit_entry(entry);
}

// Re-export the shared Candid wire surface from the neutral graph-kernel crate.
// These types are single-sourced in `gleaph_graph_kernel::provisioning::wire` so the
// Router canister can decode provisioning responses without depending on this crate.
pub use gleaph_graph_kernel::provisioning::wire::{
    ProvisionAcceptResponse, ProvisionIngressError, ProvisionIngressResult, ProvisionJobSummary,
    RouterRegistrationAckResult,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionQueryError {
    NotAuthorized,
    UnknownDeployment,
    NotFound,
}

// === Wire views ==============================================================

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionJobView {
    pub request_id: [u8; 32],
    pub deployment_id: String,
    pub reserved_graph_id: Option<gleaph_graph_kernel::entry::GraphId>,
    pub graph_name: String,
    pub state_name: String,
    pub active_resource_index: u32,
    pub completed_effect_count: u32,
    pub resources: Vec<ResourceJobView>,
    pub is_authorized_caller: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ResourceJobView {
    pub logical_resource: LogicalResource,
    pub canister_id: Option<Principal>,
    pub artifact_hash: Option<[u8; 32]>,
}

// === Helpers =================================================================

/// Read the durable governance authority principal, if seeded.
pub(crate) fn authority_governance() -> Option<Principal> {
    ProvisionBootstrapAuthStore::new()
        .get_authority()
        .map(|record| record.governance_principal)
}

pub(crate) fn build_record_from_request(req: ProvisionRequest, now_ns: u64) -> ProvisionJobRecord {
    let immutable_request_digest =
        sha256(&Encode!(&req).expect("encode immutable ProvisionRequest comparison envelope"));
    ProvisionJobRecord {
        request_id: req.request_id,
        deployment_id: req.deployment_id,
        intent_key: req.intent_key,
        reserved_graph_id: req.reserved_graph_id,
        graph_name: req.graph_name,
        authorized_caller: req.authorized_caller,
        release_id: req.release_id,
        immutable_request_digest,
        resources: req
            .requested_resources
            .into_iter()
            .map(|r| ResourceJobEntry {
                logical_resource: r.logical_resource,
                canister_id: None,
                artifact_hash: None,
            })
            .collect(),
        current_state: JobState::Submitted,
        active_resource_index: 0,
        completed_effect_count: 0,
        created_at_ns: now_ns,
        last_transition_ns: now_ns,
    }
}

/// Map a canonical `ProvisionJobRecord` to the terminal `ProvisionResult` envelope.
///
/// `ProvisionResult` is reserved for terminal outcomes only. A non-terminal state is
/// not a valid input to this mapper; it returns `Err(InvalidState)` so callers cannot
/// accidentally forge a terminal result for a job that is still in progress.
pub(crate) fn record_to_result(
    record: &ProvisionJobRecord,
) -> Result<ProvisionResult, ProvisionIngressError> {
    match &record.current_state {
        JobState::Completed => {
            let created_resources: Result<Vec<CreatedResource>, ProvisionIngressError> = record
                .resources
                .iter()
                .map(|r| {
                    let canister_id = r
                        .canister_id
                        .ok_or(ProvisionIngressError::ResultMappingError)?;
                    let artifact_hash = r
                        .artifact_hash
                        .ok_or(ProvisionIngressError::ResultMappingError)?;
                    Ok(CreatedResource {
                        logical_resource: r.logical_resource,
                        canister_id,
                        artifact_hash,
                    })
                })
                .collect();
            Ok(ProvisionResult {
                request_id: record.request_id,
                release_id: record.release_id.clone(),
                created_resources: created_resources?,
                terminal_outcome: ProvisionResultOutcome::Installed,
            })
        }
        JobState::Failed { reason } => Ok(ProvisionResult {
            request_id: record.request_id,
            release_id: record.release_id.clone(),
            created_resources: vec![],
            terminal_outcome: ProvisionResultOutcome::Failed {
                reason: reason.clone(),
            },
        }),
        _other => Err(ProvisionIngressError::InvalidState),
    }
}

pub(crate) fn build_job_summary(record: &ProvisionJobRecord) -> ProvisionJobSummary {
    ProvisionJobSummary {
        request_id: record.request_id,
        deployment_id: record.deployment_id.clone(),
        state: state_name(&record.current_state).to_owned(),
        active_resource_index: record.active_resource_index as u32,
        completed_effect_count: record.completed_effect_count,
    }
}

fn build_job_view(record: &ProvisionJobRecord, _caller: Principal) -> ProvisionJobView {
    ProvisionJobView {
        request_id: record.request_id,
        deployment_id: record.deployment_id.clone(),
        reserved_graph_id: record.reserved_graph_id,
        graph_name: record.graph_name.clone(),
        state_name: state_name(&record.current_state).to_owned(),
        active_resource_index: record.active_resource_index as u32,
        completed_effect_count: record.completed_effect_count,
        resources: record
            .resources
            .iter()
            .map(|r| ResourceJobView {
                logical_resource: r.logical_resource,
                canister_id: r.canister_id,
                artifact_hash: r.artifact_hash,
            })
            .collect(),
        is_authorized_caller: record.authorized_caller != Principal::anonymous(),
    }
}

// === Handlers ================================================================

pub(crate) async fn accept_envelope_with_caller(
    caller: Principal,
    store: &ProvisionJobStore,
    deployment_store: &DeploymentGrantStore,
    req: ProvisionRequest,
    now_ns: u64,
) -> Result<ProvisionAcceptResponse, ProvisionIngressError> {
    // 1. Authenticate first (Step 5A). Unauthorized callers never reach the store.
    //
    // A deployment grant authorizes one issuer principal. The issuer names the deployment in
    // the envelope (the Account canister issues under the user's account principal; the Router
    // issues under its own). Authorization is set membership: `caller ∈ grants`.
    if !deployment_store.contains(&caller) {
        return Err(ProvisionIngressError::NotAuthorized);
    }
    let governance_principal = authority_governance().ok_or(ProvisionIngressError::InvalidState)?;

    // 2. Validate requested_resources and install_args alignment.
    if req.requested_resources.is_empty() {
        return Err(ProvisionIngressError::InvalidResources {
            reason: "requested_resources is empty".to_owned(),
        });
    }
    if req.install_args.len() != req.requested_resources.len() {
        return Err(ProvisionIngressError::InvalidResources {
            reason: "install_args length does not match requested_resources".to_owned(),
        });
    }
    let mut seen = HashSet::new();
    for resource in &req.requested_resources {
        if !seen.insert(resource.logical_resource) {
            return Err(ProvisionIngressError::InvalidResources {
                reason: format!("duplicate resource: {:?}", resource.logical_resource),
            });
        }
    }
    let canonical_intent_present = req
        .requested_resources
        .iter()
        .any(|resource| resource.logical_resource == req.intent_key.logical_resource);
    if !canonical_intent_present {
        return Err(ProvisionIngressError::InvalidResources {
            reason: "envelope intent_key is not represented in requested_resources".to_owned(),
        });
    }

    // 3. Single store boundary: preflights locks, co-writes job + derived rows + locks,
    // and advances the fresh record to Reserved atomically.
    let record = build_record_from_request(req.clone(), now_ns);
    let outcome = match store.insert_with_intent_locks(record, now_ns) {
        Ok(crate::stable::store::InsertWithLocksOutcome::InsertedFresh(updated)) => {
            // 4. Async deploy: drive Reserved -> CreatePending -> CanisterCreated ->
            //    InstallPending -> Installed for each resource, recording canister_id and
            //    artifact_hash, then advance to RouterRegistrationPending.
            let created = deploy_job_resources(store, &req, governance_principal, now_ns).await;
            let updated = store
                .get_by_request_key(&ProvisionJobRequestKey::new(
                    &req.request_id,
                    &req.deployment_id,
                ))
                .unwrap_or(updated);
            ProvisionAcceptResponse::Accepted {
                job_view: build_job_summary(&updated),
                intent_lock_count: store.intent_lock_count_for_record(&updated) as u32,
                created_resources: created,
            }
        }
        Ok(crate::stable::store::InsertWithLocksOutcome::IdempotentReplay(existing)) => {
            // A replay of an already-admitted request returns the existing job view with
            // whatever resources are already recorded; no new deploy is driven.
            let created = existing
                .resources
                .iter()
                .filter_map(|r| {
                    Some(CreatedResource {
                        logical_resource: r.logical_resource,
                        canister_id: r.canister_id?,
                        artifact_hash: r.artifact_hash?,
                    })
                })
                .collect();
            ProvisionAcceptResponse::Replay {
                job_view: build_job_summary(&existing),
                intent_lock_count: store.intent_lock_count_for_record(&existing) as u32,
                created_resources: created,
            }
        }
        Err(crate::stable::store::InsertWithLocksError::Conflict) => {
            return Err(ProvisionIngressError::Conflict);
        }
        Err(crate::stable::store::InsertWithLocksError::IntentLockHeld) => {
            return Err(ProvisionIngressError::IntentLockHeld);
        }
    };
    Ok(outcome)
}

/// Drive one job's resources through the create/install state machine. Each resource is
/// processed in sequence: advance to `CreatePending`, call `create_canister`, record the
/// canister id (advancing to `CanisterCreated`), advance to `InstallPending`, install the
/// release artifact, record its hash (advancing to `Installed`). After the last resource,
/// advance to `RouterRegistrationPending`. Returns the created resources in
/// `requested_resources` order.
///
/// A management-canister failure at any step aborts the remaining resources and leaves the job
/// in a non-terminal state (the created prefix is preserved for reconciliation). The caller's
/// `accept_envelope` still returns `Accepted` with whatever was created.
async fn deploy_job_resources(
    store: &ProvisionJobStore,
    req: &ProvisionRequest,
    governance_principal: Principal,
    now_ns: u64,
) -> Vec<CreatedResource> {
    let mut created = Vec::with_capacity(req.requested_resources.len());

    // No active release configured: abort before any remote effect, leaving the job `Reserved`.
    // This is also the path unit tests hit (no release seeded), so the state machine stays
    // driveable without a management call.
    if ProvisionReleaseStore::new().get_active().is_none() {
        return created;
    }

    let key = ProvisionJobRequestKey::new(&req.request_id, &req.deployment_id);

    for (index, resource) in req.requested_resources.iter().enumerate() {
        // CanisterKind from the logical resource.
        let kind = match resource.logical_resource {
            LogicalResource::Router => CanisterKind::Router,
            LogicalResource::GraphShard(_) => CanisterKind::Graph,
            LogicalResource::PropertyIndex(_) => CanisterKind::PropertyIndex,
            LogicalResource::VectorIndex(_) => CanisterKind::VectorCanister,
            LogicalResource::TextIndex(_) => CanisterKind::TextCanister,
        };

        // Advance Reserved/CreatePending -> CreatePending (skipped on the first resource which
        // is already Reserved).
        let _ = store.advance_state(&key, JobState::CreatePending, Some(index), now_ns);
        // create_canister with controllers [Provision, governance].
        let canister_id = match create_canister_call(governance_principal).await {
            Some(id) => id,
            None => return created,
        };

        store.set_resource_canister_id(&key, index, canister_id);
        let _ = store.advance_state(&key, JobState::CanisterCreated, Some(index), now_ns);

        // Auto-grant: a freshly issued Router is authorized to request issuance for its own
        // deployment (it issues the graph resources afterwards). Every deploy is independent:
        // the issuer set simply gains one entry per issued Router.
        let is_router = kind == CanisterKind::Router;

        // Install the release artifact for this kind.
        let _ = store.advance_state(&key, JobState::InstallPending, Some(index), now_ns);
        let install_result = install_resource(kind, canister_id, &req.install_args[index]).await;
        let artifact_hash = match install_result {
            Ok(hash) => hash,
            Err(_) => {
                let _ = store.advance_state(
                    &key,
                    JobState::Failed {
                        reason: format!("install failed for resource {index}"),
                    },
                    None,
                    now_ns,
                );
                return created;
            }
        };

        store.set_resource_artifact_hash(&key, index, artifact_hash);
        let _ = store.advance_state(&key, JobState::Installed, Some(index), now_ns);

        // Auto-grant: a freshly issued Router is authorized to request issuance for its own
        // deployment (it issues the graph resources afterwards). Every deploy is independent:
        // the issuer set simply gains one entry per issued Router.
        if is_router {
            DeploymentGrantStore::new().insert(canister_id);
        }

        created.push(CreatedResource {
            logical_resource: resource.logical_resource,
            canister_id,
            artifact_hash,
        });
    }

    // All resources installed.
    let _ = store.advance_state(&key, JobState::RouterRegistrationPending, None, now_ns);
    created
}

/// Install the release artifact for one resource into an already-created canister. Returns the
/// artifact's full SHA-256 on success.
async fn install_resource(
    kind: CanisterKind,
    target_canister_id: Principal,
    install_args: &[u8],
) -> Result<[u8; 32], InstallError> {
    let release_store = ProvisionReleaseStore::new();
    let artifact_store = ProvisionArtifactStore::new();

    let active_release_id = release_store
        .get_active()
        .ok_or(InstallError::NoActiveRelease)?;
    let manifest = release_store
        .get_manifest(&active_release_id)
        .ok_or(InstallError::NoActiveRelease)?;

    let artifact_id = match kind {
        CanisterKind::Router => &manifest.router_artifact,
        CanisterKind::Graph => &manifest.graph_artifact,
        CanisterKind::PropertyIndex => &manifest.property_index_artifact,
        CanisterKind::VectorCanister => &manifest.vector_canister_artifact,
        CanisterKind::TextCanister => &manifest.text_canister_artifact,
    };

    let metadata = artifact_store
        .get_metadata(artifact_id)
        .ok_or(InstallError::ArtifactNotFound(artifact_id.clone()))?;

    // Re-validate via the durable verified flag (O(1)); avoids re-scanning/re-hashing the chunks.
    if !artifact_store.is_verified(artifact_id) {
        return Err(InstallError::ArtifactNotVerified(artifact_id.clone()));
    }

    // Upload each chunk one at a time, reading it from stable memory immediately before sending.
    // The whole WASM is never materialized as one `Vec<u8>` and no chunk is cloned wholesale.
    let chunk_count = metadata.chunk_hashes.len() as u32;
    let mut chunk_hashes = Vec::with_capacity(chunk_count as usize);
    for i in 0..chunk_count {
        let chunk = artifact_store
            .get_chunk(&ArtifactChunkKey {
                storage_id: metadata.storage_id,
                chunk_index: i,
            })
            .ok_or_else(|| InstallError::ArtifactNotVerified(artifact_id.clone()))?;
        let hash = install_upload_chunk(target_canister_id, chunk.bytes).await?;
        chunk_hashes.push(hash);
    }

    install_chunked_code_call(
        target_canister_id,
        chunk_hashes,
        metadata.artifact_id.sha256,
        install_args.to_vec(),
    )
    .await?;

    Ok(metadata.artifact_id.sha256)
}

pub(crate) fn query_job_with_caller(
    caller: Principal,
    store: &ProvisionJobStore,
    deployment_store: &DeploymentGrantStore,
    request_id: [u8; 32],
    deployment_id: String,
) -> Result<ProvisionJobView, ProvisionQueryError> {
    // Authorized: any granted issuer, or the governance authority.
    if !deployment_store.contains(&caller) && authority_governance() != Some(caller) {
        return Err(ProvisionQueryError::NotAuthorized);
    }
    let record = store
        .get_by_request(&request_id, &deployment_id)
        .ok_or(ProvisionQueryError::NotFound)?;
    Ok(build_job_view(&record, caller))
}

pub(crate) fn complete_graph_registration_with_caller(
    caller: Principal,
    store: &ProvisionJobStore,
    deployment_store: &DeploymentGrantStore,
    ack: RouterRegistrationAck,
    now_ns: u64,
) -> Result<RouterRegistrationAckResponse, ProvisionIngressError> {
    // Only a granted issuer (a Router) completes registration.
    if !deployment_store.contains(&caller) {
        return Err(ProvisionIngressError::NotAuthorized);
    }

    let key = ProvisionJobRequestKey::new(&ack.request_id, &ack.deployment_id);
    store
        .complete_graph_registration(&key, now_ns)
        .map_err(|error| match error {
            crate::stable::store::CompleteGraphRegistrationError::NotFound => {
                ProvisionIngressError::NotFound
            }
            crate::stable::store::CompleteGraphRegistrationError::InvalidState => {
                ProvisionIngressError::InvalidState
            }
        })
}

// === upsert_deployment_grant =========================================

/// Governance-only upsert of a deployment grant: authorize `args.issuer` to request issuance
/// for its own deployment. Idempotent. The bootstrap authority singleton must be seeded.
pub(crate) fn upsert_deployment_grant_with_caller(
    caller: Principal,
    args: UpsertDeploymentGrantArgs,
    now_ns: u64,
) -> Result<BootstrapAuthEntry, ProvisionAdminError> {
    use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;
    use crate::stable::store::DeploymentGrantStore;

    let auth_store = ProvisionBootstrapAuthStore::new();
    let deployment_store = DeploymentGrantStore::new();

    // The authority must be seeded; only the governance principal may upsert grants.
    let authority = match auth_store.get_authority() {
        Some(record) => record,
        None => {
            let entry = BootstrapAuthEntry {
                caller,
                deployment_id: Some(args.issuer.to_text()),
                action: BootstrapAuthAction::RejectNotSeeded,
                timestamp_ns: now_ns,
            };
            auth_store.put_record(caller, entry);
            return Err(ProvisionAdminError::NoBootstrapAuthority);
        }
    };
    if caller != authority.governance_principal || args.issuer == Principal::anonymous() {
        let entry = BootstrapAuthEntry {
            caller,
            deployment_id: Some(args.issuer.to_text()),
            action: BootstrapAuthAction::RejectUnauthorized,
            timestamp_ns: now_ns,
        };
        auth_store.put_record(caller, entry);
        return Err(ProvisionAdminError::Unauthorized);
    }

    deployment_store.insert(args.issuer);
    let entry = BootstrapAuthEntry {
        caller,
        deployment_id: Some(args.issuer.to_text()),
        action: BootstrapAuthAction::Upsert,
        timestamp_ns: now_ns,
    };
    auth_store.put_record(caller, entry.clone());
    Ok(entry)
}

// === Artifact catalog handlers (ADR 0036 Slice 8a) =============================

/// Publish immutable artifact metadata. Governance-only.
#[allow(clippy::result_large_err)]
pub(crate) fn artifact_publish_metadata_with_caller(
    caller: Principal,
    args: ArtifactPublishMetadataArgs,
    now_ns: u64,
) -> Result<ArtifactMetadata, ArtifactError> {
    use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;

    let auth_store = ProvisionBootstrapAuthStore::new();
    let authority = auth_store
        .get_authority()
        .ok_or(ArtifactError::Unauthorized)?
        .governance_principal;
    if caller != authority {
        return Err(ArtifactError::Unauthorized);
    }

    // Explicit 5-variant allowlist; Provision self-upgrade is forbidden.
    if !matches!(
        args.canister_kind,
        CanisterKind::Router
            | CanisterKind::Graph
            | CanisterKind::PropertyIndex
            | CanisterKind::VectorCanister
            | CanisterKind::TextCanister
    ) {
        return Err(ArtifactError::NotProvision(args.canister_kind));
    }

    // Explicit bounds on identity and payload. These reject a malformed artifact before any
    // stable write, so a huge `byte_length` can never drive a `Vec::with_capacity` allocation.
    if args.semantic_version.len() > MAX_ARTIFACT_SEMANTIC_VERSION_LEN {
        return Err(ArtifactError::SemanticVersionTooLong {
            max: MAX_ARTIFACT_SEMANTIC_VERSION_LEN as u32,
        });
    }
    if args.byte_length > MAX_ARTIFACT_BYTES {
        return Err(ArtifactError::ArtifactTooLarge {
            byte_length: args.byte_length,
            max: MAX_ARTIFACT_BYTES,
        });
    }
    if args.chunk_hashes.is_empty() {
        return Err(ArtifactError::TooManyChunks {
            declared: 0,
            max: MAX_ARTIFACT_CHUNKS,
        });
    }
    if args.chunk_hashes.len() as u32 > MAX_ARTIFACT_CHUNKS {
        return Err(ArtifactError::TooManyChunks {
            declared: args.chunk_hashes.len() as u32,
            max: MAX_ARTIFACT_CHUNKS,
        });
    }

    let artifact_id = ArtifactId::new(args.canister_kind, args.semantic_version, args.sha256);
    let store = ProvisionArtifactStore::new();
    // `storage_id` is assigned by `publish_metadata` (the internal chunk-store prefix).
    let metadata = ArtifactMetadata {
        artifact_id: artifact_id.clone(),
        storage_id: crate::types::ArtifactStorageId(0),
        byte_length: args.byte_length,
        chunk_hashes: args.chunk_hashes,
        created_at_ns: now_ns,
        verified: false,
    };

    let result = store.publish_metadata(metadata);
    match &result {
        Ok(m) => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::PublishArtifact,
                Some(m.artifact_id.clone()),
                None,
                None,
                ArtifactAuditOutcome::Success,
                None,
                now_ns,
            );
        }
        Err(e) => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::PublishArtifact,
                Some(artifact_id),
                None,
                None,
                if matches!(e, ArtifactError::Unauthorized) {
                    ArtifactAuditOutcome::Rejected
                } else {
                    ArtifactAuditOutcome::Failed
                },
                Some(format!("{e:?}")),
                now_ns,
            );
        }
    }
    result
}

/// Upload one artifact chunk. Governance-only. Verifies per-chunk hash immediately and runs full
/// SHA-256 verification once every declared chunk has been received.
#[allow(clippy::result_large_err)]
pub(crate) fn artifact_upload_chunk_with_caller(
    caller: Principal,
    args: ArtifactUploadChunkArgs,
    now_ns: u64,
) -> Result<ArtifactUpload, ArtifactError> {
    use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;

    let auth_store = ProvisionBootstrapAuthStore::new();
    let authority = match auth_store.get_authority() {
        Some(record) => record.governance_principal,
        None => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::UploadChunk,
                Some(args.artifact_id.clone()),
                None,
                None,
                ArtifactAuditOutcome::Rejected,
                Some("bootstrap authority not seeded".to_owned()),
                now_ns,
            );
            return Err(ArtifactError::Unauthorized);
        }
    };
    if caller != authority {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::UploadChunk,
            Some(args.artifact_id.clone()),
            None,
            None,
            ArtifactAuditOutcome::Rejected,
            Some("caller is not bootstrap governance principal".to_owned()),
            now_ns,
        );
        return Err(ArtifactError::Unauthorized);
    }

    let artifact_store = ProvisionArtifactStore::new();
    let metadata = match artifact_store.get_metadata(&args.artifact_id) {
        Some(m) => m,
        None => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::UploadChunk,
                Some(args.artifact_id.clone()),
                None,
                None,
                ArtifactAuditOutcome::Rejected,
                Some("artifact metadata not found".to_owned()),
                now_ns,
            );
            return Err(ArtifactError::UnknownArtifact(args.artifact_id.clone()));
        }
    };

    let chunk_count = metadata.chunk_hashes.len() as u32;
    if args.chunk_index >= chunk_count {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::UploadChunk,
            Some(args.artifact_id.clone()),
            None,
            None,
            ArtifactAuditOutcome::Rejected,
            Some(format!(
                "chunk index {} out of range (declared {})",
                args.chunk_index, chunk_count
            )),
            now_ns,
        );
        return Err(ArtifactError::ChunkOutOfRange {
            artifact_id: args.artifact_id.clone(),
            chunk_index: args.chunk_index,
            declared: chunk_count,
        });
    }
    let expected_chunk_hash = metadata.chunk_hashes[args.chunk_index as usize];
    if sha256(&args.bytes) != expected_chunk_hash {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::UploadChunk,
            Some(args.artifact_id.clone()),
            None,
            None,
            ArtifactAuditOutcome::Rejected,
            Some(format!("chunk hash mismatch at index {}", args.chunk_index)),
            now_ns,
        );
        return Err(ArtifactError::ChunkHashMismatch {
            artifact_id: args.artifact_id.clone(),
            chunk_index: args.chunk_index,
        });
    }
    // Pre-write rejection guards.
    if let Some(upload) = artifact_store.get_upload(&args.artifact_id)
        && matches!(upload.state, ArtifactUploadState::Failed { .. })
    {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::UploadChunk,
            Some(args.artifact_id.clone()),
            None,
            None,
            ArtifactAuditOutcome::Rejected,
            Some("artifact upload is in Failed state".to_owned()),
            now_ns,
        );
        return Err(ArtifactError::ChunkHashMismatch {
            artifact_id: args.artifact_id.clone(),
            chunk_index: args.chunk_index,
        });
    }

    // Derived verified predicate: the durable metadata flag is set once on the final chunk after
    // full SHA-256 verification, so this is O(1) and never re-scans or re-hashes the chunk store.
    if artifact_store.is_verified(&args.artifact_id) {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::UploadChunk,
            Some(args.artifact_id.clone()),
            None,
            None,
            ArtifactAuditOutcome::Rejected,
            Some("artifact already verified".to_owned()),
            now_ns,
        );
        return Err(ArtifactError::ConflictingMetadata {
            existing: args.artifact_id.clone(),
            requested: args.artifact_id.clone(),
        });
    }

    // Stage the chunk in region 8.
    let chunk_key = ArtifactChunkKey {
        storage_id: metadata.storage_id,
        chunk_index: args.chunk_index,
    };
    artifact_store.put_chunk(chunk_key, ArtifactChunk { bytes: args.bytes });

    // Update mutable upload progress in region 7.
    let mut upload = artifact_store.get_or_create_upload(&args.artifact_id, now_ns);
    upload.received_chunks.insert(args.chunk_index);

    if upload.received_chunks.len() < metadata.chunk_hashes.len() {
        upload.state = ArtifactUploadState::Receiving;
        artifact_store.put_upload(&args.artifact_id, upload.clone());
        append_artifact_audit(
            caller,
            ArtifactAuditAction::UploadChunk,
            Some(args.artifact_id.clone()),
            None,
            None,
            ArtifactAuditOutcome::Success,
            None,
            now_ns,
        );
        return Ok(upload);
    }

    // All chunks received: run full SHA-256 verification, streaming each chunk through the hasher
    // so the whole WASM is never materialized as a single `Vec<u8>`.
    upload.state = ArtifactUploadState::Verifying;
    artifact_store.put_upload(&args.artifact_id, upload.clone());

    let mut hasher = sha2::Sha256::new();
    let mut stream_ok = true;
    for i in 0..chunk_count {
        let key = ArtifactChunkKey {
            storage_id: metadata.storage_id,
            chunk_index: i,
        };
        match artifact_store.get_chunk(&key) {
            Some(chunk) => hasher.update(&chunk.bytes),
            None => {
                stream_ok = false;
                break;
            }
        }
    }
    if !stream_ok {
        // A chunk is missing after we just staged all of them: treat as verification failure.
        artifact_store.remove_all_chunks(metadata.storage_id);
        let reason = "chunk store incomplete during verification".to_owned();
        upload.state = ArtifactUploadState::Failed {
            reason: reason.clone(),
        };
        artifact_store.put_upload(&args.artifact_id, upload.clone());
        append_artifact_audit(
            caller,
            ArtifactAuditAction::VerifyArtifact,
            Some(args.artifact_id.clone()),
            None,
            None,
            ArtifactAuditOutcome::Failed,
            Some(reason),
            now_ns,
        );
        return Err(ArtifactError::FullSha256Mismatch {
            artifact_id: args.artifact_id.clone(),
            expected: metadata.artifact_id.sha256,
            actual: [0u8; 32],
        });
    }

    let full_sha: [u8; 32] = hasher.finalize().into();
    if full_sha != metadata.artifact_id.sha256 {
        // Verification failure: remove all staged chunks and mark upload Failed.
        artifact_store.remove_all_chunks(metadata.storage_id);
        let reason = format!(
            "full SHA-256 mismatch: expected {}, got {}",
            hex_string(&metadata.artifact_id.sha256),
            hex_string(&full_sha)
        );
        upload.state = ArtifactUploadState::Failed {
            reason: reason.clone(),
        };
        artifact_store.put_upload(&args.artifact_id, upload.clone());
        append_artifact_audit(
            caller,
            ArtifactAuditAction::VerifyArtifact,
            Some(args.artifact_id.clone()),
            None,
            None,
            ArtifactAuditOutcome::Failed,
            Some(reason),
            now_ns,
        );
        return Err(ArtifactError::FullSha256Mismatch {
            artifact_id: args.artifact_id.clone(),
            expected: metadata.artifact_id.sha256,
            actual: full_sha,
        });
    }

    // Verification success: persist the verified flag, promote region 8 chunks to verified
    // canonical, and reclaim region 7.
    artifact_store.mark_verified(&args.artifact_id);
    upload.state = ArtifactUploadState::Verified {
        verified_at_ns: now_ns,
    };
    upload.verified_at_ns = Some(now_ns);
    artifact_store.remove_upload(&args.artifact_id);
    append_artifact_audit(
        caller,
        ArtifactAuditAction::UploadChunk,
        Some(args.artifact_id.clone()),
        None,
        None,
        ArtifactAuditOutcome::Success,
        None,
        now_ns,
    );
    append_artifact_audit(
        caller,
        ArtifactAuditAction::VerifyArtifact,
        Some(args.artifact_id.clone()),
        None,
        None,
        ArtifactAuditOutcome::Success,
        None,
        now_ns,
    );
    Ok(upload)
}

/// Query the current mutable upload state. Any caller.
pub(crate) fn artifact_get_status(artifact_id: ArtifactId) -> Option<ArtifactUpload> {
    let store = ProvisionArtifactStore::new();
    store.get_upload(&artifact_id)
}

// === Release manifest + active release handlers (ADR 0036 Slice 8b) ===========

fn require_bootstrap_authority(caller: Principal) -> Result<Principal, ReleaseError> {
    use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;

    let auth_store = ProvisionBootstrapAuthStore::new();
    let authority = auth_store
        .get_authority()
        .ok_or(ReleaseError::NoBootstrapAuthority)?
        .governance_principal;
    if caller != authority {
        return Err(ReleaseError::Unauthorized);
    }
    Ok(authority)
}

/// Canonicalize a `Vec<ArtifactId>` into the five-field release manifest.
fn build_release_manifest(
    release_id: ReleaseId,
    artifact_ids: Vec<ArtifactId>,
    artifact_store: &ProvisionArtifactStore,
) -> Result<ReleaseManifest, ReleaseError> {
    if artifact_ids.len() != 5 {
        return Err(ReleaseError::IncompleteManifest {
            release_id,
            missing: vec![],
        });
    }

    use std::collections::BTreeMap;
    let mut by_kind: BTreeMap<CanisterKind, ArtifactId> = BTreeMap::new();
    for artifact_id in &artifact_ids {
        if !matches!(
            artifact_id.canister_kind,
            CanisterKind::Router
                | CanisterKind::Graph
                | CanisterKind::PropertyIndex
                | CanisterKind::VectorCanister
                | CanisterKind::TextCanister
        ) {
            return Err(ReleaseError::ProvisionKindForbidden(artifact_id.clone()));
        }
        if artifact_store.get_metadata(artifact_id).is_none() {
            return Err(ReleaseError::ArtifactNotFound(artifact_id.clone()));
        }
        if let Some(existing) =
            by_kind.insert(artifact_id.canister_kind.clone(), artifact_id.clone())
        {
            return Err(ReleaseError::NotUniquePerKind {
                release_id: release_id.clone(),
                kind: artifact_id.canister_kind.clone(),
                conflicting: vec![existing, artifact_id.clone()],
            });
        }
    }

    let required = [
        CanisterKind::Router,
        CanisterKind::Graph,
        CanisterKind::PropertyIndex,
        CanisterKind::VectorCanister,
        CanisterKind::TextCanister,
    ];
    let mut missing = Vec::new();
    for kind in &required {
        if !by_kind.contains_key(kind) {
            missing.push(
                by_kind
                    .get(kind)
                    .cloned()
                    .unwrap_or_else(|| ArtifactId::new(kind.clone(), "".to_owned(), [0u8; 32])),
            );
        }
    }
    if !missing.is_empty() {
        return Err(ReleaseError::IncompleteManifest {
            release_id,
            missing,
        });
    }

    Ok(ReleaseManifest {
        release_id,
        router_artifact: by_kind.remove(&CanisterKind::Router).unwrap(),
        graph_artifact: by_kind.remove(&CanisterKind::Graph).unwrap(),
        property_index_artifact: by_kind.remove(&CanisterKind::PropertyIndex).unwrap(),
        vector_canister_artifact: by_kind.remove(&CanisterKind::VectorCanister).unwrap(),
        text_canister_artifact: by_kind.remove(&CanisterKind::TextCanister).unwrap(),
    })
}

/// Publish an immutable release manifest. Governance-only.
#[allow(clippy::result_large_err)]
pub(crate) fn release_publish_with_caller(
    caller: Principal,
    args: ReleasePublishArgs,
    _now_ns: u64,
) -> Result<ReleaseManifest, ReleaseError> {
    if let Err(e) = require_bootstrap_authority(caller) {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::PublishRelease,
            None,
            Some(args.release_id.clone()),
            None,
            ArtifactAuditOutcome::Rejected,
            Some(format!("{e:?}")),
            _now_ns,
        );
        return Err(e);
    }

    let artifact_store = ProvisionArtifactStore::new();
    let manifest =
        match build_release_manifest(args.release_id.clone(), args.artifact_ids, &artifact_store) {
            Ok(m) => m,
            Err(e) => {
                append_artifact_audit(
                    caller,
                    ArtifactAuditAction::PublishRelease,
                    None,
                    Some(args.release_id.clone()),
                    None,
                    ArtifactAuditOutcome::Rejected,
                    Some(format!("{e:?}")),
                    _now_ns,
                );
                return Err(e);
            }
        };

    let release_store = ProvisionReleaseStore::new();
    let result = release_store.publish_manifest(manifest);
    match &result {
        Ok(m) => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::PublishRelease,
                None,
                Some(m.release_id.clone()),
                None,
                ArtifactAuditOutcome::Success,
                None,
                _now_ns,
            );
        }
        Err(e) => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::PublishRelease,
                None,
                Some(args.release_id),
                None,
                ArtifactAuditOutcome::Rejected,
                Some(format!("{e:?}")),
                _now_ns,
            );
        }
    }
    result
}

/// Atomically activate a release after re-validating its artifacts. Governance-only.
#[allow(clippy::result_large_err)]
pub(crate) fn release_activate_with_caller(
    caller: Principal,
    args: ReleaseActivateArgs,
    now_ns: u64,
) -> Result<ReleaseActivateResult, ReleaseError> {
    if let Err(e) = require_bootstrap_authority(caller) {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::ActivateRelease,
            None,
            Some(args.release_id.clone()),
            None,
            ArtifactAuditOutcome::Rejected,
            Some(format!("{e:?}")),
            now_ns,
        );
        return Err(e);
    }

    let release_store = ProvisionReleaseStore::new();
    let manifest = match release_store.get_manifest(&args.release_id) {
        Some(m) => m,
        None => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::ActivateRelease,
                None,
                Some(args.release_id.clone()),
                None,
                ArtifactAuditOutcome::Rejected,
                Some("release manifest not found".to_owned()),
                now_ns,
            );
            return Err(ReleaseError::UnknownRelease(args.release_id.clone()));
        }
    };

    // Re-validate every referenced artifact against the derived verified predicate.
    let artifact_store = ProvisionArtifactStore::new();
    for artifact_id in [
        &manifest.router_artifact,
        &manifest.graph_artifact,
        &manifest.property_index_artifact,
        &manifest.vector_canister_artifact,
        &manifest.text_canister_artifact,
    ] {
        if !matches!(
            artifact_id.canister_kind,
            CanisterKind::Router
                | CanisterKind::Graph
                | CanisterKind::PropertyIndex
                | CanisterKind::VectorCanister
                | CanisterKind::TextCanister
        ) {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::ActivateRelease,
                Some((*artifact_id).clone()),
                Some(manifest.release_id.clone()),
                None,
                ArtifactAuditOutcome::Rejected,
                Some(format!(
                    "forbidden canister kind: {:?}",
                    artifact_id.canister_kind
                )),
                now_ns,
            );
            return Err(ReleaseError::ProvisionKindForbidden((*artifact_id).clone()));
        }
        if artifact_store.get_metadata(artifact_id).is_none() {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::ActivateRelease,
                Some((*artifact_id).clone()),
                Some(manifest.release_id.clone()),
                None,
                ArtifactAuditOutcome::Rejected,
                Some("artifact metadata not found".to_owned()),
                now_ns,
            );
            return Err(ReleaseError::ArtifactNotFound((*artifact_id).clone()));
        }

        // Re-validate via the durable verified flag (set once on the final upload chunk after full
        // SHA-256 verification). This is O(1) and avoids re-reading/re-hashing the whole artifact.
        if !artifact_store.is_verified(artifact_id) {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::ActivateRelease,
                Some((*artifact_id).clone()),
                Some(manifest.release_id.clone()),
                None,
                ArtifactAuditOutcome::Rejected,
                Some("artifact chunks missing or incomplete".to_owned()),
                now_ns,
            );
            return Err(ReleaseError::ArtifactNotVerified((*artifact_id).clone()));
        }
    }

    let previous_release_id = release_store.get_active();
    release_store.set_active(args.release_id.clone());

    let result = ReleaseActivateResult {
        release_id: args.release_id,
        activated_at_ns: now_ns,
        previous_release_id,
    };
    append_artifact_audit(
        caller,
        ArtifactAuditAction::ActivateRelease,
        None,
        Some(result.release_id.clone()),
        None,
        ArtifactAuditOutcome::Success,
        None,
        now_ns,
    );
    Ok(result)
}

/// Read the active release id, if any. Any caller.
pub(crate) fn release_get_active() -> Option<ReleaseActivateResult> {
    let release_store = ProvisionReleaseStore::new();
    release_store
        .get_active()
        .map(|release_id| ReleaseActivateResult {
            release_id,
            activated_at_ns: 0,
            previous_release_id: None,
        })
}

/// Return the artifact audit history for the caller. Governance-only.
#[allow(clippy::result_large_err)]
pub(crate) fn artifact_audit_history_with_caller(
    caller: Principal,
) -> Result<Vec<ArtifactAuditEntry>, ArtifactError> {
    use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;

    let auth_store = ProvisionBootstrapAuthStore::new();
    let authority = auth_store
        .get_authority()
        .ok_or(ArtifactError::Unauthorized)?
        .governance_principal;
    if caller != authority {
        return Err(ArtifactError::Unauthorized);
    }
    Ok(ProvisionArtifactStore::new().audit_history(caller))
}

// === Release install handler (ADR 0036 Slice 8c) ===========

const MAX_INSTALL_CHUNK_BYTES: usize = 1024 * 1024;

// A newly created canister starts with no cycles after the management-canister creation fee is
// charged. Keep a bounded install budget on the target so its first memory growth and install can
// complete. The amount is intentionally local to this deployment step; ADR 0038's durable cycle
// reservation and allocation policy remain proposed.
const INITIAL_CANISTER_CYCLES: u128 = 1_000_000_000_000;

#[inline]
const fn initial_canister_cycles() -> u128 {
    INITIAL_CANISTER_CYCLES
}

/// Create a canister with controllers `[Provision, governance]`. Returns the new canister id.
/// On wasm this calls the IC management canister; on native (unit tests) it synthesizes a
/// deterministic principal so the state machine can be exercised without a management call.
#[cfg(target_family = "wasm")]
async fn create_canister_call(governance_principal: Principal) -> Option<Principal> {
    use ic_cdk_management_canister::{
        CanisterSettings, CreateCanisterArgs, create_canister_with_extra_cycles,
    };
    let args = CreateCanisterArgs {
        settings: Some(CanisterSettings {
            controllers: Some(vec![ic_cdk::api::canister_self(), governance_principal]),
            ..CanisterSettings::default()
        }),
    };
    match create_canister_with_extra_cycles(&args, initial_canister_cycles()).await {
        Ok(id) => Some(id.canister_id),
        Err(_) => None,
    }
}

#[cfg(not(target_family = "wasm"))]
async fn create_canister_call(_governance_principal: Principal) -> Option<Principal> {
    static COUNTER: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(1);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let mut bytes = [0u8; 29];
    bytes[..4].copy_from_slice(&n.to_le_bytes());
    Some(Principal::from_slice(&bytes))
}

#[cfg(target_family = "wasm")]
async fn install_upload_chunk(
    target_canister_id: Principal,
    chunk_bytes: Vec<u8>,
) -> Result<Vec<u8>, InstallError> {
    if chunk_bytes.len() > MAX_INSTALL_CHUNK_BYTES {
        return Err(InstallError::ManagementCanisterCallFailed(format!(
            "chunk exceeds {} bytes",
            MAX_INSTALL_CHUNK_BYTES
        )));
    }
    use ic_cdk_management_canister::{UploadChunkArgs, upload_chunk};
    let arg = UploadChunkArgs {
        canister_id: target_canister_id,
        chunk: chunk_bytes,
    };
    match upload_chunk(&arg).await {
        Ok(result) => Ok(result.hash),
        Err(err) => Err(InstallError::ManagementCanisterCallFailed(format!(
            "upload_chunk: {err:?}"
        ))),
    }
}

#[cfg(not(target_family = "wasm"))]
async fn install_upload_chunk(
    _target_canister_id: Principal,
    chunk_bytes: Vec<u8>,
) -> Result<Vec<u8>, InstallError> {
    if chunk_bytes.len() > MAX_INSTALL_CHUNK_BYTES {
        return Err(InstallError::ManagementCanisterCallFailed(format!(
            "chunk exceeds {} bytes",
            MAX_INSTALL_CHUNK_BYTES
        )));
    }
    Ok(sha256(&chunk_bytes).to_vec())
}

#[cfg(target_family = "wasm")]
async fn install_chunked_code_call(
    target_canister_id: Principal,
    chunk_hashes: Vec<Vec<u8>>,
    wasm_module_hash: [u8; 32],
    install_args: Vec<u8>,
) -> Result<(), InstallError> {
    use ic_cdk_management_canister::{
        CanisterInstallMode, ChunkHash, InstallChunkedCodeArgs, install_chunked_code,
    };
    let arg = InstallChunkedCodeArgs {
        mode: CanisterInstallMode::Install,
        target_canister: target_canister_id,
        store_canister: Some(target_canister_id),
        chunk_hashes_list: chunk_hashes
            .into_iter()
            .map(|h| ChunkHash { hash: h })
            .collect(),
        wasm_module_hash: wasm_module_hash.to_vec(),
        arg: install_args,
    };
    match install_chunked_code(&arg).await {
        Ok(()) => Ok(()),
        Err(err) => Err(InstallError::ManagementCanisterCallFailed(format!(
            "install_chunked_code: {err:?}"
        ))),
    }
}

#[cfg(not(target_family = "wasm"))]
async fn install_chunked_code_call(
    _target_canister_id: Principal,
    _chunk_hashes: Vec<Vec<u8>>,
    _wasm_module_hash: [u8; 32],
    _install_args: Vec<u8>,
) -> Result<(), InstallError> {
    Ok(())
}

/// Install the artifact matching `args.target_canister_kind` into `args.target_canister_id`.
/// Governance-only. Cross-canister upload_chunk + install_chunked_code.
#[allow(clippy::result_large_err)]
pub(crate) async fn release_install_with_caller(
    caller: Principal,
    args: ReleaseInstallArgs,
    now_ns: u64,
) -> Result<ReleaseInstallResult, InstallError> {
    use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;

    let artifact_store = ProvisionArtifactStore::new();
    let release_store = ProvisionReleaseStore::new();

    let authority = match ProvisionBootstrapAuthStore::new().get_authority() {
        Some(record) => record.governance_principal,
        None => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::InstallRelease,
                None,
                None,
                None,
                ArtifactAuditOutcome::Rejected,
                Some("bootstrap authority not seeded".to_owned()),
                now_ns,
            );
            return Err(InstallError::NoBootstrapAuthority);
        }
    };
    if caller != authority {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::InstallRelease,
            None,
            None,
            None,
            ArtifactAuditOutcome::Rejected,
            Some("caller is not bootstrap governance principal".to_owned()),
            now_ns,
        );
        return Err(InstallError::Unauthorized);
    }

    if !matches!(
        args.target_canister_kind,
        CanisterKind::Router
            | CanisterKind::Graph
            | CanisterKind::PropertyIndex
            | CanisterKind::VectorCanister
            | CanisterKind::TextCanister
    ) {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::InstallRelease,
            None,
            None,
            None,
            ArtifactAuditOutcome::Rejected,
            Some(format!(
                "forbidden target canister kind: {:?}",
                args.target_canister_kind
            )),
            now_ns,
        );
        return Err(InstallError::TargetCanisterKindForbidden(
            args.target_canister_kind,
        ));
    }

    let target_canister_id = match args.target_canister_id {
        Some(id) => id,
        None => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::InstallRelease,
                None,
                None,
                None,
                ArtifactAuditOutcome::Rejected,
                Some("target_canister_id must be provided explicitly".to_owned()),
                now_ns,
            );
            return Err(InstallError::ManagementCanisterCallFailed(
                "target_canister_id is required".to_owned(),
            ));
        }
    };

    let active_release_id = match release_store.get_active() {
        Some(id) => id,
        None => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::InstallRelease,
                None,
                None,
                None,
                ArtifactAuditOutcome::Failed,
                Some("no active release".to_owned()),
                now_ns,
            );
            return Err(InstallError::NoActiveRelease);
        }
    };

    let manifest = match release_store.get_manifest(&active_release_id) {
        Some(m) => m,
        None => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::InstallRelease,
                None,
                Some(active_release_id.clone()),
                None,
                ArtifactAuditOutcome::Failed,
                Some("active release manifest not found".to_owned()),
                now_ns,
            );
            return Err(InstallError::NoActiveRelease);
        }
    };

    let artifact_id = match args.target_canister_kind {
        CanisterKind::Router => &manifest.router_artifact,
        CanisterKind::Graph => &manifest.graph_artifact,
        CanisterKind::PropertyIndex => &manifest.property_index_artifact,
        CanisterKind::VectorCanister => &manifest.vector_canister_artifact,
        CanisterKind::TextCanister => &manifest.text_canister_artifact,
    };

    let metadata = match artifact_store.get_metadata(artifact_id) {
        Some(m) => m,
        None => {
            append_artifact_audit(
                caller,
                ArtifactAuditAction::InstallRelease,
                Some(artifact_id.clone()),
                Some(manifest.release_id.clone()),
                Some(target_canister_id),
                ArtifactAuditOutcome::Failed,
                Some("artifact metadata not found".to_owned()),
                now_ns,
            );
            return Err(InstallError::ArtifactNotFound(artifact_id.clone()));
        }
    };

    // Re-validate via the durable verified flag (set once on the final upload chunk). This is O(1)
    // and does not re-scan or re-hash the chunk store.
    if !artifact_store.is_verified(artifact_id) {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::InstallRelease,
            Some(artifact_id.clone()),
            Some(manifest.release_id.clone()),
            Some(target_canister_id),
            ArtifactAuditOutcome::Failed,
            Some("artifact chunks missing or incomplete".to_owned()),
            now_ns,
        );
        return Err(InstallError::ArtifactNotVerified(artifact_id.clone()));
    }

    // Upload each chunk to the target one at a time, reading each chunk from stable memory and
    // uploading it immediately. We never materialize the whole WASM as a single `Vec<u8>` nor
    // clone every chunk, so peak heap stays ~one chunk. The per-chunk stable read is O(1).
    let chunk_count = metadata.chunk_hashes.len() as u32;
    let mut chunk_hashes = Vec::with_capacity(chunk_count as usize);
    for i in 0..chunk_count {
        let chunk = match artifact_store.get_chunk(&ArtifactChunkKey {
            storage_id: metadata.storage_id,
            chunk_index: i,
        }) {
            Some(c) => c,
            None => {
                append_artifact_audit(
                    caller,
                    ArtifactAuditAction::InstallRelease,
                    Some(artifact_id.clone()),
                    Some(manifest.release_id.clone()),
                    Some(target_canister_id),
                    ArtifactAuditOutcome::Failed,
                    Some("artifact chunks missing or incomplete".to_owned()),
                    now_ns,
                );
                return Err(InstallError::ArtifactNotVerified(artifact_id.clone()));
            }
        };
        let hash = match install_upload_chunk(target_canister_id, chunk.bytes).await {
            Ok(h) => h,
            Err(e) => {
                append_artifact_audit(
                    caller,
                    ArtifactAuditAction::InstallRelease,
                    Some(artifact_id.clone()),
                    Some(manifest.release_id.clone()),
                    Some(target_canister_id),
                    ArtifactAuditOutcome::Failed,
                    Some(format!("{e:?}")),
                    now_ns,
                );
                return Err(e);
            }
        };
        chunk_hashes.push(hash);
    }

    if let Err(e) = install_chunked_code_call(
        target_canister_id,
        chunk_hashes,
        metadata.artifact_id.sha256,
        args.install_args,
    )
    .await
    {
        append_artifact_audit(
            caller,
            ArtifactAuditAction::InstallRelease,
            Some(artifact_id.clone()),
            Some(manifest.release_id.clone()),
            Some(target_canister_id),
            ArtifactAuditOutcome::Failed,
            Some(format!("{e:?}")),
            now_ns,
        );
        return Err(e);
    }

    let result = ReleaseInstallResult {
        release_id: manifest.release_id.clone(),
        target_canister_id,
        installed_chunks: chunk_count,
        install_chunked_code_hash: metadata.artifact_id.sha256,
        installed_at_ns: now_ns,
    };
    append_artifact_audit(
        caller,
        ArtifactAuditAction::InstallRelease,
        Some(artifact_id.clone()),
        Some(result.release_id.clone()),
        Some(target_canister_id),
        ArtifactAuditOutcome::Success,
        None,
        now_ns,
    );
    Ok(result)
}

fn hex_string(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests;
