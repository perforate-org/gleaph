//! Provision canister ingress handler foundation (ADR 0035 Slice 3).
//!
//! These are plain `pub(crate)` functions with explicit caller injection so unit tests can
//! drive every authorization and idempotency branch. Callable canister endpoints
//! (`#[init]`/`#[query]`/`#[update]` annotations) remain a follow-up slice.

use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use crate::canister::init::binding_from_admin_args;
use crate::stable::artifact::ProvisionArtifactStore;
use crate::stable::release::ProvisionReleaseStore;
use crate::stable::store::{DeploymentTrustStore, ProvisionJobStore};
use crate::types::{
    AdminInstallDeploymentBindingArgs, ArtifactChunk, ArtifactChunkKey, ArtifactError, ArtifactId,
    ArtifactMetadata, ArtifactPublishMetadataArgs, ArtifactUpload, ArtifactUploadChunkArgs,
    ArtifactUploadState, BootstrapAuthAction, BootstrapAuthEntry, CanisterKind, CreatedResource,
    JobState, ProvisionAdminError, ProvisionJobRecord, ProvisionJobRequestKey, ProvisionRequest,
    ProvisionResult, ProvisionResultOutcome, ProvisionableResourceKind, ProvisioningIntentKey,
    ReleaseActivateArgs, ReleaseActivateResult, ReleaseError, ReleaseId, ReleaseManifest,
    ReleasePublishArgs, ResourceJobEntry, RouterProvisionAck, sha256, state_name,
};

pub mod handlers;
pub mod init;

// Re-export the shared Candid wire surface from the neutral graph-kernel crate.
// These types are single-sourced in `gleaph_graph_kernel::provisioning::wire` so the
// Router canister can decode `accept_envelope` responses without depending on this crate.
pub use gleaph_graph_kernel::provisioning::wire::{
    ProvisionAcceptResponse, ProvisionIngressError, ProvisionIngressResult, ProvisionJobSummary,
};

/// Candid wire Result for `router_ack`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum RouterAckResult {
    Ok(ProvisionRouterAckResult),
    Err(ProvisionIngressError),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ProvisionQueryError {
    NotAuthorized,
    UnknownDeployment,
    NotFound,
}

// === Wire views ==============================================================

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionJobView {
    pub request_id: String,
    pub deployment_id: String,
    pub request_fingerprint: String,
    pub reserved_graph_id: Option<gleaph_graph_kernel::entry::GraphId>,
    pub graph_name: String,
    pub state_name: String,
    pub active_resource_index: u32,
    pub completed_effect_count: u32,
    pub accepted_registry_version: Option<u64>,
    pub resources: Vec<ResourceJobView>,
    pub is_authorized_caller: bool,
    pub has_router_callback: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ResourceJobView {
    pub resource_kind: ProvisionableResourceKind,
    pub logical_resource_key: String,
    pub canister_id: Option<Principal>,
    pub artifact_hash: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionRouterAckResult {
    pub completed: bool,
    pub accepted_registry_version: u64,
}

// === Helpers =================================================================

pub(crate) fn build_record_from_request(req: ProvisionRequest, now_ns: u64) -> ProvisionJobRecord {
    ProvisionJobRecord {
        request_id: req.request_id,
        deployment_id: req.deployment_id,
        request_fingerprint: req.request_fingerprint,
        intent_key: req.intent_key,
        reserved_graph_id: req.reserved_graph_id,
        graph_name: req.graph_name,
        authorized_caller: req.authorized_caller,
        release_id: req.release_id,
        router_callback_principal: req.router_callback_principal,
        resources: req
            .requested_resources
            .into_iter()
            .map(|r| ResourceJobEntry {
                resource_kind: r.kind,
                logical_resource_key: r.logical_resource_key,
                canister_id: None,
                artifact_hash: None,
            })
            .collect(),
        current_state: JobState::Submitted,
        active_resource_index: 0,
        completed_effect_count: 0,
        accepted_registry_version: None,
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
                        .clone()
                        .ok_or(ProvisionIngressError::ResultMappingError)?;
                    Ok(CreatedResource {
                        kind: r.resource_kind,
                        canister_id,
                        artifact_hash,
                    })
                })
                .collect();
            Ok(ProvisionResult {
                request_id: record.request_id.clone(),
                request_fingerprint: record.request_fingerprint.clone(),
                release_id: record.release_id.clone(),
                created_resources: created_resources?,
                terminal_outcome: ProvisionResultOutcome::Installed,
            })
        }
        JobState::Failed { reason } => Ok(ProvisionResult {
            request_id: record.request_id.clone(),
            request_fingerprint: record.request_fingerprint.clone(),
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
        request_id: record.request_id.clone(),
        deployment_id: record.deployment_id.clone(),
        state: state_name(&record.current_state).to_owned(),
        active_resource_index: record.active_resource_index as u32,
        completed_effect_count: record.completed_effect_count,
        accepted_registry_version: record.accepted_registry_version,
    }
}

fn build_job_view(record: &ProvisionJobRecord, _caller: Principal) -> ProvisionJobView {
    ProvisionJobView {
        request_id: record.request_id.clone(),
        deployment_id: record.deployment_id.clone(),
        request_fingerprint: record.request_fingerprint.clone(),
        reserved_graph_id: record.reserved_graph_id,
        graph_name: record.graph_name.clone(),
        state_name: state_name(&record.current_state).to_owned(),
        active_resource_index: record.active_resource_index as u32,
        completed_effect_count: record.completed_effect_count,
        accepted_registry_version: record.accepted_registry_version,
        resources: record
            .resources
            .iter()
            .map(|r| ResourceJobView {
                resource_kind: r.resource_kind,
                logical_resource_key: r.logical_resource_key.clone(),
                canister_id: r.canister_id,
                artifact_hash: r.artifact_hash.clone(),
            })
            .collect(),
        is_authorized_caller: record.authorized_caller != Principal::anonymous(),
        has_router_callback: record.router_callback_principal != Principal::anonymous(),
    }
}

// === Handlers ================================================================

pub(crate) fn accept_envelope_with_caller(
    caller: Principal,
    store: &ProvisionJobStore,
    deployment_store: &DeploymentTrustStore,
    req: ProvisionRequest,
    now_ns: u64,
) -> Result<ProvisionAcceptResponse, ProvisionIngressError> {
    // 1. Authenticate first (Step 5A). Unauthorized callers never reach the store.
    let binding = deployment_store
        .get(&req.deployment_id)
        .ok_or(ProvisionIngressError::UnknownDeployment)?;
    if caller != binding.router_principal {
        return Err(ProvisionIngressError::NotAuthorized);
    }

    // 2. Validate requested_resources.
    if req.requested_resources.is_empty() {
        return Err(ProvisionIngressError::InvalidResources {
            reason: "requested_resources is empty".to_owned(),
        });
    }
    let mut seen = HashSet::new();
    for resource in &req.requested_resources {
        if !seen.insert((resource.kind, resource.logical_resource_key.clone())) {
            return Err(ProvisionIngressError::InvalidResources {
                reason: format!(
                    "duplicate resource: {:?}/{}",
                    resource.kind, resource.logical_resource_key
                ),
            });
        }
    }
    let canonical_intent_present = req.requested_resources.iter().any(|resource| {
        resource.kind == req.intent_key.resource_kind
            && resource.logical_resource_key == req.intent_key.logical_resource_key
    });
    if !canonical_intent_present {
        return Err(ProvisionIngressError::InvalidResources {
            reason: "envelope intent_key is not represented in requested_resources".to_owned(),
        });
    }

    // 3. Single store boundary: preflights locks, co-writes job + derived rows + locks,
    // and advances the fresh record to Reserved atomically.
    let record = build_record_from_request(req, now_ns);
    match store.insert_with_intent_locks(record, now_ns) {
        Ok(crate::stable::store::InsertWithLocksOutcome::InsertedFresh(updated)) => {
            Ok(ProvisionAcceptResponse::Accepted {
                job_view: build_job_summary(&updated),
                intent_lock_count: store.intent_lock_count_for_record(&updated) as u32,
            })
        }
        Ok(crate::stable::store::InsertWithLocksOutcome::IdempotentReplay(existing)) => {
            Ok(ProvisionAcceptResponse::Replay {
                job_view: build_job_summary(&existing),
                intent_lock_count: store.intent_lock_count_for_record(&existing) as u32,
            })
        }
        Err(crate::stable::store::InsertWithLocksError::Conflict) => {
            Err(ProvisionIngressError::Conflict)
        }
        Err(crate::stable::store::InsertWithLocksError::IntentLockHeld) => {
            Err(ProvisionIngressError::IntentLockHeld)
        }
    }
}

pub(crate) fn query_job_with_caller(
    caller: Principal,
    store: &ProvisionJobStore,
    deployment_store: &DeploymentTrustStore,
    request_id: String,
    deployment_id: String,
) -> Result<ProvisionJobView, ProvisionQueryError> {
    let binding = deployment_store
        .get(&deployment_id)
        .ok_or(ProvisionQueryError::UnknownDeployment)?;
    let record = store
        .get_by_request(&request_id, &deployment_id)
        .ok_or(ProvisionQueryError::NotFound)?;
    if caller != binding.router_principal && caller != binding.governance_principal {
        return Err(ProvisionQueryError::NotAuthorized);
    }
    Ok(build_job_view(&record, caller))
}

pub(crate) fn router_ack_with_caller(
    caller: Principal,
    store: &ProvisionJobStore,
    deployment_store: &DeploymentTrustStore,
    ack: RouterProvisionAck,
    now_ns: u64,
) -> Result<ProvisionRouterAckResult, ProvisionIngressError> {
    let mut record = store
        .get_by_request(&ack.request_id, &ack.deployment_id)
        .ok_or(ProvisionIngressError::NotFound)?;
    let key = ProvisionJobRequestKey::new(&ack.request_id, &ack.deployment_id);

    let binding = deployment_store
        .get(&ack.deployment_id)
        .ok_or(ProvisionIngressError::UnknownDeployment)?;
    if caller != binding.router_principal {
        return Err(ProvisionIngressError::NotAuthorized);
    }

    // Idempotent replay branches before the fresh-ack path.
    if record.current_state == JobState::Completed {
        match record.accepted_registry_version {
            Some(stored) if stored == ack.accepted_registry_version => {
                return Ok(ProvisionRouterAckResult {
                    completed: true,
                    accepted_registry_version: stored,
                });
            }
            Some(stored) => {
                return Err(ProvisionIngressError::AckConflict { stored });
            }
            None => return Err(ProvisionIngressError::InvalidState),
        }
    }

    if !matches!(record.current_state, JobState::RouterAckPending) {
        return Err(ProvisionIngressError::InvalidState);
    }

    // Preflight the lock invariant before any durable write. A RouterAckPending
    // record must have all of its intent locks held; a missing lock indicates
    // state corruption, not a recoverable flow.
    for resource in &record.resources {
        let lock_key = ProvisioningIntentKey {
            deployment_id: record.deployment_id.clone(),
            resource_kind: resource.resource_kind,
            logical_resource_key: resource.logical_resource_key.clone(),
        };
        if !store.intent_locked(&lock_key) {
            return Err(ProvisionIngressError::InvalidState);
        }
    }

    record.accepted_registry_version = Some(ack.accepted_registry_version);
    store.put(&key, record.clone());

    store
        .advance_state(&key, JobState::Completed, None, now_ns)
        .map_err(|_| ProvisionIngressError::StateAdvanceFailed)?;

    let _released = store.clear_intent_locks_for_record(&record);

    Ok(ProvisionRouterAckResult {
        completed: true,
        accepted_registry_version: ack.accepted_registry_version,
    })
}

// === admin_install_deployment_binding (ADR 0035 Slice 7) =========

pub(crate) fn admin_install_deployment_binding_with_caller(
    caller: Principal,
    args: AdminInstallDeploymentBindingArgs,
    now_ns: u64,
) -> Result<BootstrapAuthEntry, ProvisionAdminError> {
    use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;
    use crate::stable::store::DeploymentTrustStore;

    let auth_store = ProvisionBootstrapAuthStore::new();
    let deployment_store = DeploymentTrustStore::new();

    // (1) Read the durable bootstrap authority singleton. If it has not been seeded,
    //     every install attempt is an InvalidState and must still leave a Reject audit row.
    let authority = match auth_store.get_authority() {
        Some(record) => record,
        None => {
            let entry = BootstrapAuthEntry {
                caller,
                deployment_id: Some(args.deployment_id.clone()),
                action: BootstrapAuthAction::RejectInvalidState,
                timestamp_ns: now_ns,
                registry_version: Some(args.binding_version),
            };
            auth_store.put_record(caller, entry);
            return Err(ProvisionAdminError::InvalidState(
                "bootstrap authority not seeded".to_owned(),
            ));
        }
    };

    let deployment_id = args.deployment_id.clone();
    let new_binding = binding_from_admin_args(args);

    if let Some(existing) = deployment_store.get(&deployment_id) {
        // (2) Existing deployment: authorize either the bootstrap authority or the stored
        //     governance principal. Anyone else is rejected with AlreadyExists.
        if caller == authority.governance_principal || caller == existing.governance_principal {
            let entry = BootstrapAuthEntry {
                caller,
                deployment_id: Some(deployment_id),
                action: BootstrapAuthAction::AdminInstall,
                timestamp_ns: now_ns,
                registry_version: Some(new_binding.binding_version),
            };
            auth_store.put_record(caller, entry.clone());
            deployment_store.admin_upsert(new_binding);
            Ok(entry)
        } else {
            let entry = BootstrapAuthEntry {
                caller,
                deployment_id: Some(deployment_id.clone()),
                action: BootstrapAuthAction::RejectAlreadyExists,
                timestamp_ns: now_ns,
                registry_version: Some(new_binding.binding_version),
            };
            auth_store.put_record(caller, entry);
            Err(ProvisionAdminError::AlreadyExists {
                deployment_id,
                existing_governance: existing.governance_principal,
            })
        }
    } else if caller == authority.governance_principal {
        // (3) New deployment: only the bootstrap authority may install.
        let entry = BootstrapAuthEntry {
            caller,
            deployment_id: Some(deployment_id),
            action: BootstrapAuthAction::AdminInstall,
            timestamp_ns: now_ns,
            registry_version: Some(new_binding.binding_version),
        };
        auth_store.put_record(caller, entry.clone());
        deployment_store.admin_upsert(new_binding);
        Ok(entry)
    } else {
        let entry = BootstrapAuthEntry {
            caller,
            deployment_id: Some(deployment_id.clone()),
            action: BootstrapAuthAction::RejectUnknownDeployment,
            timestamp_ns: now_ns,
            registry_version: Some(new_binding.binding_version),
        };
        auth_store.put_record(caller, entry);
        Err(ProvisionAdminError::UnknownDeployment(deployment_id))
    }
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

    // Explicit 4-variant allowlist; Provision self-upgrade is forbidden.
    if !matches!(
        args.canister_kind,
        CanisterKind::Router
            | CanisterKind::Graph
            | CanisterKind::PropertyIndex
            | CanisterKind::VectorIndex
    ) {
        return Err(ArtifactError::NotProvision(args.canister_kind));
    }

    let artifact_id = ArtifactId::new(args.canister_kind, args.semantic_version, args.sha256);
    let store = ProvisionArtifactStore::new();
    let metadata = ArtifactMetadata {
        artifact_id: artifact_id.clone(),
        byte_length: args.byte_length,
        chunk_hashes: args.chunk_hashes,
        created_at_ns: now_ns,
    };

    store.publish_metadata(metadata)
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
    let authority = auth_store
        .get_authority()
        .ok_or(ArtifactError::Unauthorized)?
        .governance_principal;
    if caller != authority {
        return Err(ArtifactError::Unauthorized);
    }

    let artifact_store = ProvisionArtifactStore::new();
    let metadata = artifact_store
        .get_metadata(&args.artifact_id)
        .ok_or(ArtifactError::UnknownArtifact(args.artifact_id.clone()))?;

    let chunk_count = metadata.chunk_hashes.len() as u32;
    if args.chunk_index >= chunk_count {
        return Err(ArtifactError::ChunkOutOfRange {
            artifact_id: args.artifact_id.clone(),
            chunk_index: args.chunk_index,
            declared: chunk_count,
        });
    }

    let expected_chunk_hash = metadata.chunk_hashes[args.chunk_index as usize];
    if sha256(&args.bytes) != expected_chunk_hash {
        return Err(ArtifactError::ChunkHashMismatch {
            artifact_id: args.artifact_id.clone(),
            chunk_index: args.chunk_index,
        });
    }

    // Pre-write rejection guards.
    if let Some(upload) = artifact_store.get_upload(&args.artifact_id)
        && matches!(upload.state, ArtifactUploadState::Failed { .. })
    {
        return Err(ArtifactError::ChunkHashMismatch {
            artifact_id: args.artifact_id.clone(),
            chunk_index: args.chunk_index,
        });
    }

    // Derived verified predicate: if all declared chunks exist in region 8 and their concatenated
    // SHA-256 matches the published metadata, the artifact is already verified.
    let existing_chunks = artifact_store.chunks_in_order(&args.artifact_id, chunk_count);
    if existing_chunks.len() == chunk_count as usize {
        let mut full = Vec::with_capacity(metadata.byte_length as usize);
        for chunk in &existing_chunks {
            full.extend_from_slice(&chunk.bytes);
        }
        if sha256(&full) == metadata.artifact_id.sha256 {
            return Err(ArtifactError::ConflictingMetadata {
                existing: args.artifact_id.clone(),
                requested: args.artifact_id.clone(),
            });
        }
    }

    // Stage the chunk in region 8.
    let chunk_key = ArtifactChunkKey {
        artifact_id: args.artifact_id.clone(),
        chunk_index: args.chunk_index,
    };
    artifact_store.put_chunk(chunk_key, ArtifactChunk { bytes: args.bytes });

    // Update mutable upload progress in region 7.
    let mut upload = artifact_store.get_or_create_upload(&args.artifact_id, now_ns);
    upload.received_chunks.insert(args.chunk_index);

    if upload.received_chunks.len() < metadata.chunk_hashes.len() {
        upload.state = ArtifactUploadState::Receiving;
        artifact_store.put_upload(&args.artifact_id, upload.clone());
        return Ok(upload);
    }

    // All chunks received: run full SHA-256 verification.
    upload.state = ArtifactUploadState::Verifying;
    artifact_store.put_upload(&args.artifact_id, upload.clone());

    let staged_chunks = artifact_store.chunks_in_order(&args.artifact_id, chunk_count);
    let mut full_bytes = Vec::with_capacity(metadata.byte_length as usize);
    for chunk in &staged_chunks {
        full_bytes.extend_from_slice(&chunk.bytes);
    }

    if sha256(&full_bytes) != metadata.artifact_id.sha256 {
        // Verification failure: remove all staged chunks and mark upload Failed.
        artifact_store.remove_all_chunks(&args.artifact_id);
        let actual = sha256(&full_bytes);
        upload.state = ArtifactUploadState::Failed {
            reason: format!(
                "full SHA-256 mismatch: expected {}, got {}",
                hex_string(&metadata.artifact_id.sha256),
                hex_string(&actual)
            ),
        };
        artifact_store.put_upload(&args.artifact_id, upload.clone());
        return Err(ArtifactError::FullSha256Mismatch {
            artifact_id: args.artifact_id.clone(),
            expected: metadata.artifact_id.sha256,
            actual,
        });
    }

    // Verification success: promote region 8 chunks to verified canonical and reclaim region 7.
    upload.state = ArtifactUploadState::Verified {
        verified_at_ns: now_ns,
    };
    upload.verified_at_ns = Some(now_ns);
    artifact_store.remove_upload(&args.artifact_id);
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

/// Canonicalize a `Vec<ArtifactId>` into the four-field release manifest.
fn build_release_manifest(
    release_id: ReleaseId,
    artifact_ids: Vec<ArtifactId>,
    artifact_store: &ProvisionArtifactStore,
) -> Result<ReleaseManifest, ReleaseError> {
    if artifact_ids.len() != 4 {
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
                | CanisterKind::VectorIndex
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
        CanisterKind::VectorIndex,
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
        vector_index_artifact: by_kind.remove(&CanisterKind::VectorIndex).unwrap(),
    })
}

/// Publish an immutable release manifest. Governance-only.
#[allow(clippy::result_large_err)]
pub(crate) fn release_publish_with_caller(
    caller: Principal,
    args: ReleasePublishArgs,
    _now_ns: u64,
) -> Result<ReleaseManifest, ReleaseError> {
    require_bootstrap_authority(caller)?;

    let artifact_store = ProvisionArtifactStore::new();
    let manifest = build_release_manifest(args.release_id, args.artifact_ids, &artifact_store)?;

    let release_store = ProvisionReleaseStore::new();
    release_store.publish_manifest(manifest)
}

/// Atomically activate a release after re-validating its artifacts. Governance-only.
#[allow(clippy::result_large_err)]
pub(crate) fn release_activate_with_caller(
    caller: Principal,
    args: ReleaseActivateArgs,
    now_ns: u64,
) -> Result<ReleaseActivateResult, ReleaseError> {
    require_bootstrap_authority(caller)?;

    let release_store = ProvisionReleaseStore::new();
    let manifest = release_store
        .get_manifest(&args.release_id)
        .ok_or(ReleaseError::UnknownRelease(args.release_id.clone()))?;

    // Re-validate every referenced artifact against the derived verified predicate.
    let artifact_store = ProvisionArtifactStore::new();
    for artifact_id in [
        &manifest.router_artifact,
        &manifest.graph_artifact,
        &manifest.property_index_artifact,
        &manifest.vector_index_artifact,
    ] {
        if artifact_id.canister_kind == CanisterKind::Router
            || artifact_id.canister_kind == CanisterKind::Graph
            || artifact_id.canister_kind == CanisterKind::PropertyIndex
            || artifact_id.canister_kind == CanisterKind::VectorIndex
        {
            // defensive: these are the only allowed kinds
        } else {
            return Err(ReleaseError::ProvisionKindForbidden((*artifact_id).clone()));
        }
        let metadata = artifact_store
            .get_metadata(artifact_id)
            .ok_or(ReleaseError::ArtifactNotFound((*artifact_id).clone()))?;

        let chunk_count = metadata.chunk_hashes.len() as u32;
        let staged = artifact_store.chunks_in_order(artifact_id, chunk_count);
        if staged.len() != chunk_count as usize {
            return Err(ReleaseError::ArtifactNotVerified((*artifact_id).clone()));
        }
        let mut full_bytes = Vec::with_capacity(metadata.byte_length as usize);
        for chunk in &staged {
            full_bytes.extend_from_slice(&chunk.bytes);
        }
        if sha256(&full_bytes) != metadata.artifact_id.sha256 {
            return Err(ReleaseError::ArtifactNotVerified((*artifact_id).clone()));
        }
    }

    let previous_release_id = release_store.get_active();
    release_store.set_active(args.release_id.clone());

    Ok(ReleaseActivateResult {
        release_id: args.release_id,
        activated_at_ns: now_ns,
        previous_release_id,
    })
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

fn hex_string(bytes: &[u8; 32]) -> String {
    bytes.iter().map(|b| format!("{:02x}", b)).collect()
}

#[cfg(test)]
mod tests;
