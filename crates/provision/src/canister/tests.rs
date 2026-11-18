//! Unit tests for the Provision ingress handlers.

use super::{
    DictRelayCall, ProvisionAcceptResponse, ProvisionIngressError, ProvisionQueryError,
    ProvisionResult, ProvisionResultOutcome, accept_envelope_with_caller,
    accept_envelope_with_caller_and_cycles, admin_finalize_dict_catalog_with_caller,
    admin_upload_dict_catalog_chunk_with_caller, artifact_get_status,
    artifact_publish_metadata_with_caller, artifact_upload_chunk_with_caller,
    build_record_from_request, complete_graph_registration_with_caller, dict_catalog_key_for_kind,
    dict_relay_call_log_for_test, query_job_with_caller, record_to_result,
    release_activate_with_caller, release_get_active, release_install_with_caller,
    release_publish_with_caller, reset_dict_relay_script_for_test, script_dict_relay_call,
    set_force_cleanup_error_for_test, upsert_deployment_grant_with_caller,
};
use crate::canister::init;
use crate::stable::artifact::ProvisionArtifactStore;
use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;
use crate::stable::store::{
    DeploymentGrantStore, ProvisionJobStore, reopen_provisioning_regions_for_test, reset_all_maps,
};
use gleaph_graph_kernel::provisioning::DeploymentId;

use crate::types::{
    AdmissionCharge, ArtifactError, ArtifactId, ArtifactPublishMetadataArgs,
    ArtifactUploadChunkArgs, BootstrapAuthAction, BootstrapAuthorityRecord, CanisterKind,
    ControllerMode, DeploymentGrantScope, DeploymentOwnerUpdateError, DictCatalogFinalizeArgs,
    DictCatalogKey, DictCatalogUploadChunkArgs, GrantCreditArgs, GrantCreditError,
    InitializeDeploymentArgs, InitializeDeploymentError, InstallError, JobState, LogicalResource,
    ProvisionAdminError, ProvisionJobRequestKey, ProvisionRequest, ProvisionableResource,
    ProvisioningIntentKey, ReleaseActivateArgs, ReleaseError, ReleaseId, ReleaseInstallArgs,
    ReleasePublishArgs, ReverseCreditArgs, ReverseCreditError, RouterRegistrationAck,
    RouterRegistrationAckResponse, SelfCustodyTransferError, ServiceConfig,
    ServiceConfigUpdateError, ServiceLedger, TakeOwnershipError, UpsertDeploymentGrantArgs, sha256,
};
use candid::{Encode, Principal};
use gleaph_graph_kernel::federation::{ShardId, TextIndexId};
use gleaph_graph_kernel::provisioning::dictionary::{
    DictKind, DictState, DictStatus, MAX_DICT_COMPRESSED_CHUNK_BYTES,
};
use gleaph_graph_kernel::provisioning::init_args::TextCanisterInitArgs;
use std::future::Future;
use std::task::{Context, Poll, Waker};

fn pid(id: u8) -> Principal {
    Principal::from_slice(&[id; 29])
}

fn gov_principal() -> Principal {
    pid(100)
}

fn router_principal() -> Principal {
    pid(10)
}

fn other_principal() -> Principal {
    pid(20)
}

/// The default `OwnDeployment` issuer: `deployment_id = caller`.
fn deployment_issuer() -> Principal {
    router_principal()
}

/// The deployment id text of [`deployment_issuer`].
fn dep_id() -> String {
    deployment_issuer().to_text()
}

/// A second, independent issuer (its own deployment).
fn other_issuer() -> Principal {
    other_principal()
}

fn other_dep_id() -> String {
    other_issuer().to_text()
}

fn test_resource(logical_resource: LogicalResource) -> ProvisionableResource {
    ProvisionableResource { logical_resource }
}

/// Test-only deployment id from a short label. Production ids are Account principals; tests use
/// stable labels so fixtures stay readable.
fn test_deployment_id(label: &str) -> DeploymentId {
    let principal = Principal::from_text(label).unwrap_or_else(|_| {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(label.as_bytes());
        let mut bytes = [0u8; 29];
        bytes.copy_from_slice(&digest[..29]);
        Principal::from_slice(&bytes)
    });
    DeploymentId::new(principal)
}

fn test_request_id(label: &str) -> [u8; 32] {
    let mut id = [0u8; 32];
    let bytes = label.as_bytes();
    let n = bytes.len().min(32);
    id[..n].copy_from_slice(&bytes[..n]);
    id
}

trait TestDeploymentId {
    fn deployment(self) -> DeploymentId;
}
impl TestDeploymentId for DeploymentId {
    fn deployment(self) -> DeploymentId {
        self
    }
}
impl TestDeploymentId for &str {
    fn deployment(self) -> DeploymentId {
        test_deployment_id(self)
    }
}
impl TestDeploymentId for &String {
    fn deployment(self) -> DeploymentId {
        test_deployment_id(self.as_str())
    }
}

fn test_request<D: TestDeploymentId>(
    deployment_id: D,
    request_id: &str,
    _fingerprint: &str,
    resources: Vec<ProvisionableResource>,
) -> ProvisionRequest {
    let deployment_id = deployment_id.deployment();
    let intent_key = if resources.is_empty() {
        ProvisioningIntentKey::new(deployment_id, LogicalResource::GraphShard(ShardId::new(0)))
    } else {
        ProvisioningIntentKey::new(deployment_id, resources[0].logical_resource)
    };
    ProvisionRequest {
        deployment_id,
        request_id: test_request_id(request_id),
        intent_key,
        reserved_graph_id: None,
        graph_name: "test-graph".to_owned(),
        requested_resources: resources.clone(),
        install_args: resources.iter().map(|_| vec![0u8; 0]).collect(),
        authorized_caller: pid(30),
        release_id: "r1".to_owned(),
        controller_mode: ControllerMode::SelfCustody,
    }
}

fn init_and_grant(caller: Principal) -> (DeploymentGrantStore, ProvisionJobStore) {
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let deployment_store = DeploymentGrantStore::new();
    deployment_store.insert(caller);
    (deployment_store, ProvisionJobStore::new())
}

fn advance_to_registration_pending(
    store: &ProvisionJobStore,
    key: &ProvisionJobRequestKey,
    mut now_ns: u64,
) {
    let steps = [
        JobState::Reserved,
        JobState::CreatePending,
        JobState::CanisterCreated,
        JobState::InstallPending,
        JobState::Installed,
        JobState::RouterRegistrationPending,
    ];
    for step in &steps {
        let current = store.get_by_request_key(key).unwrap().current_state;
        if current == JobState::RouterRegistrationPending {
            break;
        }
        if current == *step {
            continue;
        }
        store
            .advance_state(key, step.clone(), None, now_ns)
            .unwrap();
        now_ns += 1;
    }
}

// === accept_envelope =========================================================

#[test]
fn test_provision_accept_wrong_caller_rejected() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let result = block_on(accept_envelope_with_caller(
        other_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ));
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
    assert!(
        store
            .get_by_request(&test_request_id("req-a"), test_deployment_id(&dep_id()))
            .is_none()
    );
}

#[test]
fn test_provision_accept_ungranted_issuer_rejected() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let deployment_store = DeploymentGrantStore::new();
    let store = ProvisionJobStore::new();
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    // The issuer is not in the grant set: rejected regardless of deployment_id.
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ));
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
}

#[test]
fn test_provision_accept_rejects_envelope_with_inconsistent_intent_deployment() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let mut req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    req.intent_key.deployment_id = test_deployment_id(&other_dep_id());
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ));
    assert!(matches!(
        result,
        Err(ProvisionIngressError::InvalidResources { .. })
    ));
}

#[test]
fn test_provision_accept_rejects_anonymous_authorized_caller() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let mut req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    req.authorized_caller = Principal::anonymous();
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ));
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
}

/// A single request may provision several resources. Each resource runs its own linear cycle,
/// then the job becomes registration-pending; every effect is counted and the last index is the
/// active one. Before the per-resource re-entry transition, resources after the first silently
/// skipped every state advance.
#[test]
fn multi_resource_job_advances_each_resource_and_records_every_effect() {
    reset_all_maps();
    seed_active_release();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-multi",
        "fp-multi",
        vec![
            test_resource(LogicalResource::GraphShard(ShardId::new(0))),
            test_resource(LogicalResource::GraphShard(ShardId::new(1))),
        ],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => assert_eq!(created_resources.len(), 2),
        other => panic!("expected Accepted, got {other:?}"),
    }
    let record = store
        .get_by_request(&test_request_id("req-multi"), test_deployment_id(&dep_id()))
        .expect("job record");
    assert_eq!(record.current_state, JobState::RouterRegistrationPending);
    assert_eq!(record.active_resource_index, 1);
    assert_eq!(record.completed_effect_count, 5);
    assert!(
        record
            .resources
            .iter()
            .all(|resource| resource.canister_id.is_some() && resource.artifact_hash.is_some()),
        "both resources record their created canister and installed artifact"
    );
}

#[test]
fn test_provision_own_issuer_cannot_name_another_deployment() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&other_dep_id()),
        "req-other",
        "fp-other",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ));
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
}

#[test]
fn test_provision_relay_issuer_can_name_another_deployment() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let deployment_store = DeploymentGrantStore::new();
    let store = ProvisionJobStore::new();
    let account = pid(200);
    deployment_store.insert_with_scope(account, DeploymentGrantScope::Relay);
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-relay",
        "fp-relay",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let result = block_on(accept_envelope_with_caller(
        account,
        &store,
        &deployment_store,
        req,
        1,
    ))
    .expect("a relay may issue for the named account");
    assert!(matches!(result, ProvisionAcceptResponse::Accepted { .. }));
}

#[test]
fn test_provision_accept_idempotent_replay_returns_existing() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req.clone(),
        1,
    ))
    .unwrap();
    let replay = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        2,
    ))
    .unwrap();
    match replay {
        ProvisionAcceptResponse::Replay { job_view, .. } => {
            assert_eq!(job_view.request_id, test_request_id("req-a"));
            assert_eq!(job_view.deployment_id, test_deployment_id(&dep_id()));
            assert_eq!(job_view.state, "Reserved");
        }
        _ => panic!("expected Replay, got {:?}", replay),
    }
    let record = store
        .get_by_request(&test_request_id("req-a"), test_deployment_id(&dep_id()))
        .unwrap();
    assert_eq!(record.current_state, JobState::Reserved);
}

#[test]
fn test_provision_accept_same_content_is_idempotent_replay() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req1 = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req1,
        1,
    ))
    .unwrap();
    // Same graph_name + resources => same content-hash request_id => idempotent replay.
    let req2 = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-b",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req2,
        2,
    ))
    .unwrap();
    assert!(
        matches!(result, ProvisionAcceptResponse::Replay { .. }),
        "same content must be an idempotent replay, got {result:?}"
    );
    let record = store
        .get_by_request(&test_request_id("req-a"), test_deployment_id(&dep_id()))
        .unwrap();
    assert_eq!(record.request_id, test_request_id("req-a"));
}

#[test]
fn test_provision_accept_different_content_yields_distinct_request_ids() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    // Different resources => different content-hash request_id => both admitted independently.
    let req1 = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let req2 = test_request(
        test_deployment_id(&dep_id()),
        "req-b",
        "fp-b",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(1)))],
    );
    assert_ne!(req1.request_id, req2.request_id);
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req1,
        1,
    ))
    .unwrap();
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req2,
        2,
    ))
    .unwrap();
    assert!(
        store
            .get_by_request(&test_request_id("req-a"), test_deployment_id(&dep_id()))
            .is_some()
    );
    assert!(
        store
            .get_by_request(&test_request_id("req-b"), test_deployment_id(&dep_id()))
            .is_some()
    );
}

#[test]
fn test_provision_no_partial_writes_on_lock_failure() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    // Pre-lock the only intent.
    let held_key = ProvisioningIntentKey::new(
        test_deployment_id(&dep_id()),
        LogicalResource::GraphShard(ShardId::new(0)),
    );
    assert!(store.acquire_intent_lock(held_key.clone()));

    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ));
    assert_eq!(result, Err(ProvisionIngressError::IntentLockHeld));

    // No canonical record and no derived Map 2 entries remain.
    assert!(
        store
            .get_by_request(&test_request_id("req-a"), test_deployment_id(&dep_id()))
            .is_none()
    );
    assert!(!store.has_live_job_for_deployment(test_deployment_id(&dep_id())));
    // Pre-held lock is untouched.
    assert!(store.intent_locked(&held_key));
}

#[test]
fn test_provision_accept_empty_resources_rejected() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(dep_id().as_str(), "req-empty", "fp-empty", vec![]);
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ));
    assert_eq!(
        result,
        Err(ProvisionIngressError::InvalidResources {
            reason: "requested_resources is empty".to_owned()
        })
    );
}

#[test]
fn test_provision_accept_duplicate_resources_rejected() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-dup",
        "fp-dup",
        vec![
            test_resource(LogicalResource::GraphShard(ShardId::new(0))),
            test_resource(LogicalResource::GraphShard(ShardId::new(0))),
        ],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ));
    assert!(
        matches!(
            result,
            Err(ProvisionIngressError::InvalidResources { ref reason }) if reason.contains("duplicate")
        ),
        "expected duplicate resource error, got {:?}",
        result
    );
}

// === query_job ===============================================================

#[test]
fn test_provision_query_wrong_caller_rejected() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let result = query_job_with_caller(
        other_principal(),
        &store,
        &deployment_store,
        test_request_id("req-a"),
        test_deployment_id(&dep_id()),
    );
    assert_eq!(result, Err(ProvisionQueryError::NotAuthorized));
}

#[test]
fn test_provision_query_returns_redacted_view() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let view = query_job_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        test_request_id("req-a"),
        test_deployment_id(&dep_id()),
    )
    .unwrap();
    assert_eq!(view.request_id, test_request_id("req-a"));
    assert_eq!(view.state_name, "Reserved");
    assert!(
        !view.is_authorized_caller,
        "the issuer reading the job is not its authorized caller"
    );
}

#[test]
fn test_provision_query_authorized_caller_sees_itself() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let authorized = req.authorized_caller;
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    deployment_store.insert_with_scope(authorized, DeploymentGrantScope::Relay);
    let view = query_job_with_caller(
        authorized,
        &store,
        &deployment_store,
        test_request_id("req-a"),
        test_deployment_id(&dep_id()),
    )
    .unwrap();
    assert!(view.is_authorized_caller);
}

#[test]
fn test_provision_query_grant_holder_is_not_the_authorized_caller() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let observer = pid(201);
    deployment_store.insert_with_scope(observer, DeploymentGrantScope::Relay);
    let view = query_job_with_caller(
        observer,
        &store,
        &deployment_store,
        test_request_id("req-a"),
        test_deployment_id(&dep_id()),
    )
    .unwrap();
    assert!(
        !view.is_authorized_caller,
        "a grant holder reading the job is not the job's authorized caller"
    );
}

#[test]
fn test_provision_query_ungranted_issuer_rejected() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let deployment_store = DeploymentGrantStore::new();
    let store = ProvisionJobStore::new();
    let result = query_job_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        test_request_id("req-a"),
        test_deployment_id("dep-missing"),
    );
    assert_eq!(result, Err(ProvisionQueryError::NotAuthorized));
}

// === complete_graph_registration ============================================

#[test]
fn registration_ack_authenticates_before_exact_lookup() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let result = complete_graph_registration_with_caller(
        other_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: test_deployment_id(&dep_id()),
            request_id: test_request_id("missing"),
        },
        1,
    );
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
}

#[test]
fn registration_ack_wrong_router_rejected() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), test_deployment_id(&dep_id()));
    advance_to_registration_pending(&store, &key, 10);
    let result = complete_graph_registration_with_caller(
        other_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: test_deployment_id(&dep_id()),
            request_id: test_request_id("req-a"),
        },
        20,
    );
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
}

#[test]
fn registration_ack_invalid_state() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    // Record is in Reserved after accept.
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: test_deployment_id(&dep_id()),
            request_id: test_request_id("req-a"),
        },
        2,
    );
    assert_eq!(result, Err(ProvisionIngressError::InvalidState));
}

#[test]
fn registration_ack_fresh_co_writes_completed_and_owned_row_release() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), test_deployment_id(&dep_id()));
    advance_to_registration_pending(&store, &key, 10);
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: test_deployment_id(&dep_id()),
            request_id: test_request_id("req-a"),
        },
        20,
    )
    .unwrap();
    assert_eq!(result, RouterRegistrationAckResponse::Applied);
    let record = store
        .get_by_request(&test_request_id("req-a"), test_deployment_id(&dep_id()))
        .unwrap();
    assert_eq!(record.current_state, JobState::Completed);
    assert_eq!(
        store.assert_intent_to_request_for_test(
            test_deployment_id(&dep_id()),
            LogicalResource::GraphShard(ShardId::new(0)),
        ),
        None
    );
    assert!(!store.intent_locked(&ProvisioningIntentKey::new(
        test_deployment_id(&dep_id()),
        LogicalResource::GraphShard(ShardId::new(0))
    )));
}

#[test]
fn registration_ack_fresh_requires_map2_owner_and_map3_presence() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), test_deployment_id(&dep_id()));
    advance_to_registration_pending(&store, &key, 10);
    // Release the lock behind the store's back.
    let lock_key = ProvisioningIntentKey::new(
        test_deployment_id(&dep_id()),
        LogicalResource::GraphShard(ShardId::new(0)),
    );
    assert!(store.release_intent_lock(&lock_key));
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: test_deployment_id(&dep_id()),
            request_id: test_request_id("req-a"),
        },
        20,
    );
    assert_eq!(result, Err(ProvisionIngressError::InvalidState));
}

#[test]
fn registration_ack_idempotent_replay() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), test_deployment_id(&dep_id()));
    advance_to_registration_pending(&store, &key, 10);
    let ack = RouterRegistrationAck {
        deployment_id: test_deployment_id(&dep_id()),
        request_id: test_request_id("req-a"),
    };
    let first = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        ack.clone(),
        20,
    )
    .unwrap();
    let second = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        ack,
        21,
    )
    .unwrap();
    assert_eq!(first, RouterRegistrationAckResponse::Applied);
    assert_eq!(second, RouterRegistrationAckResponse::Replay);
}

#[test]
fn registration_ack_completed_replay_returns_replay() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), test_deployment_id(&dep_id()));
    advance_to_registration_pending(&store, &key, 10);
    store.complete_graph_registration(&key, 30).unwrap();
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: test_deployment_id(&dep_id()),
            request_id: test_request_id("req-a"),
        },
        31,
    )
    .unwrap();
    assert_eq!(result, RouterRegistrationAckResponse::Replay);
}

#[test]
fn registration_ack_completed_replay_preserves_new_foreign_rows() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), test_deployment_id(&dep_id()));
    advance_to_registration_pending(&store, &key, 10);
    assert_eq!(
        store.complete_graph_registration(&key, 20),
        Ok(RouterRegistrationAckResponse::Applied)
    );

    let intent = ProvisioningIntentKey::new(
        test_deployment_id(&dep_id()),
        LogicalResource::GraphShard(ShardId::new(0)),
    );
    let foreign =
        ProvisionJobRequestKey::new(&test_request_id("foreign"), test_deployment_id(&dep_id()));
    store.set_intent_owner_for_test(intent.clone(), Some(foreign.clone()));
    store.set_intent_lock_for_test(intent.clone(), true);

    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: test_deployment_id(&dep_id()),
            request_id: test_request_id("req-a"),
        },
        21,
    );
    assert_eq!(result, Ok(RouterRegistrationAckResponse::Replay));
    assert_eq!(
        store.assert_intent_to_request_for_test(
            test_deployment_id(&dep_id()),
            LogicalResource::GraphShard(ShardId::new(0)),
        ),
        Some(foreign)
    );
    assert!(store.intent_locked(&intent));
}

#[test]
fn registration_ack_ungranted_caller_rejected_before_lookup() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let deployment_store = DeploymentGrantStore::new();
    let store = ProvisionJobStore::new();
    // Insert a job record without granting the caller.
    let record = build_record_from_request(
        test_request(
            "dep-orphan",
            "req-o",
            "fp-o",
            vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
        ),
        1,
        1,
    );
    store.insert_or_idempotent(record).unwrap();
    // The router principal is not in the grant set: rejected before any job lookup.
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: test_deployment_id("dep-orphan"),
            request_id: test_request_id("req-o"),
        },
        2,
    );
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
}

#[test]
fn test_provision_accept_envelope_fresh_admission_reports_accepted() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            job_view,
            intent_lock_count,
            created_resources,
        } => {
            assert_eq!(job_view.request_id, test_request_id("req-a"));
            assert_eq!(job_view.deployment_id, test_deployment_id(&dep_id()));
            assert_eq!(job_view.state, "Reserved");
            assert_eq!(intent_lock_count, 1);
            assert!(
                created_resources.is_empty(),
                "no release seeded -> no deploy"
            );
        }
        ProvisionAcceptResponse::Replay { .. } => {
            panic!("fresh admission must report Accepted, not Replay")
        }
    }
}

#[test]
fn test_provision_accept_envelope_replay_reports_replay() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req.clone(),
        1,
    ))
    .unwrap();
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        2,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Replay {
            job_view,
            intent_lock_count,
            created_resources,
        } => {
            assert_eq!(job_view.request_id, test_request_id("req-a"));
            assert_eq!(job_view.state, "Reserved");
            assert_eq!(intent_lock_count, 1);
            assert!(
                created_resources.is_empty(),
                "no release seeded -> no deploy"
            );
        }
        ProvisionAcceptResponse::Accepted { .. } => {
            panic!("replay must report Replay, not Accepted")
        }
    }
}

#[test]
fn test_provision_accept_envelope_allows_any_granted_issuer() {
    reset_all_maps();
    // Two independent issuers, both granted: the Account (other_principal) for its first-Router
    // issuance and the Router (router_principal) for graph resources. Each is its own deployment.
    let (deployment_store, store) = init_and_grant(other_issuer());
    deployment_store.insert(deployment_issuer());
    let req = test_request(
        test_deployment_id(&other_dep_id()),
        "req-boot",
        "fp-boot",
        vec![test_resource(LogicalResource::Router)],
    );
    // Any granted issuer may request issuance for its own deployment.
    let result = block_on(accept_envelope_with_caller(
        other_issuer(),
        &store,
        &deployment_store,
        req.clone(),
        1,
    ))
    .unwrap();
    assert!(
        matches!(result, ProvisionAcceptResponse::Accepted { .. }),
        "granted issuer must be accepted for its own deployment"
    );
    // A caller that is not granted (and whose deployment_id does not match) is rejected.
    let err = block_on(accept_envelope_with_caller(
        gov_principal(),
        &store,
        &deployment_store,
        req,
        2,
    ))
    .unwrap_err();
    assert_eq!(err, ProvisionIngressError::NotAuthorized);
}

#[test]
fn test_granted_agent_may_issue_under_account_deployment() {
    reset_all_maps();
    // Agent pattern: a Relay grant (the Account canister) may request issuance under the
    // deployment id it names (the user's account principal). An OwnDeployment grant cannot.
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let deployment_store = DeploymentGrantStore::new();
    let store = ProvisionJobStore::new();
    deployment_store.insert_with_scope(deployment_issuer(), DeploymentGrantScope::Relay);
    let req = test_request(
        test_deployment_id(&other_dep_id()),
        "req-boot",
        "fp-boot",
        vec![test_resource(LogicalResource::Router)],
    );
    let result = block_on(accept_envelope_with_caller(
        deployment_issuer(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    assert!(
        matches!(result, ProvisionAcceptResponse::Accepted { .. }),
        "a granted issuer may issue under the deployment id it names"
    );
    assert!(
        store
            .get_by_request(
                &test_request_id("req-boot"),
                test_deployment_id(&other_dep_id())
            )
            .is_some()
    );
}

#[test]
fn test_provision_wrong_impl_returning_failed_for_admission_would_fail() {
    // Adversarial test: a wrong implementation of accept_envelope that returns a
    // terminal ProvisionResult with Failed{reason} for a fresh admission would not
    // compile because the return type is ProvisionAcceptResponse, not ProvisionResult.
    // This test documents that the type system enforces the contract.
    fn _type_boundary() {
        // The compiler rejects any expression of type ProvisionResult here.
        // let _: ProvisionAcceptResponse = ProvisionResult { ... }; // would fail
    }
    // Runtime check: admission never fabricates Failed.
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    assert!(
        matches!(result, ProvisionAcceptResponse::Accepted { .. }),
        "admission must never return a fabricated terminal result; got {:?}",
        result
    );
}

#[test]
fn test_provision_adversarial_lock_conflict_preserves_existing_derived_index() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());

    // Job A: seed and admit so its intent lock and derived index entry exist.
    let req_a = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let _a = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req_a,
        1,
    ))
    .unwrap();

    let key_a =
        ProvisionJobRequestKey::new(&test_request_id("req-a"), test_deployment_id(&dep_id()));
    let intent_key = ProvisioningIntentKey::new(
        test_deployment_id(&dep_id()),
        LogicalResource::GraphShard(ShardId::new(0)),
    );

    // Job B: same deployment, same resource, different request_id.
    let req_b = test_request(
        test_deployment_id(&dep_id()),
        "req-b",
        "fp-b",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req_b,
        2,
    ));
    assert_eq!(result, Err(ProvisionIngressError::IntentLockHeld));

    // A's canonical record is unchanged.
    let record_a = store
        .get_by_request(&test_request_id("req-a"), test_deployment_id(&dep_id()))
        .unwrap();
    assert_eq!(record_a.request_id, test_request_id("req-a"));

    // A's lock survives.
    assert!(store.intent_locked(&intent_key));

    // The derived index maps R1.intent to A.key before the conflict is attempted.
    assert_eq!(
        store.assert_intent_to_request_for_test(
            test_deployment_id(&dep_id()),
            LogicalResource::GraphShard(ShardId::new(0)),
        ),
        Some(key_a.clone()),
        "derived index must map R1.intent to A.key before B is attempted"
    );

    // After B is rejected, the same intent still resolves to A.key; B never overwrote the derived row.
    assert_eq!(
        store.assert_intent_to_request_for_test(
            test_deployment_id(&dep_id()),
            LogicalResource::GraphShard(ShardId::new(0)),
        ),
        Some(key_a.clone()),
        "derived index must still map R1.intent to A.key after B is rejected"
    );

    // B leaves no canonical or derived row.
    assert_eq!(
        store.get_by_request(&test_request_id("req-b"), test_deployment_id(&dep_id())),
        None,
        "B must not leave a canonical row"
    );
    assert_eq!(
        store.get_by_request_key(&ProvisionJobRequestKey::new(
            &test_request_id("req-b"),
            test_deployment_id(&dep_id())
        )),
        None,
        "B must not leave a canonical row via its composite key"
    );
}

#[test]
fn registration_ack_membership_only_and_exact_key() {
    reset_all_maps();

    // Grant two independent issuers; each names its own deployment in its envelopes.
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let deployment_store = DeploymentGrantStore::new();
    deployment_store.insert(deployment_issuer());
    deployment_store.insert(other_issuer());
    let store = ProvisionJobStore::new();

    // A1: (r1, d1) and A2: (r1, d2), both await Router registration completion.
    let req1 = test_request(
        test_deployment_id(&dep_id()),
        "r1",
        "fp-1",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(1)))],
    );
    let req2 = test_request(
        test_deployment_id(&other_dep_id()),
        "r1",
        "fp-2",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(2)))],
    );
    block_on(accept_envelope_with_caller(
        deployment_issuer(),
        &store,
        &deployment_store,
        req1,
        1,
    ))
    .unwrap();
    block_on(accept_envelope_with_caller(
        other_issuer(),
        &store,
        &deployment_store,
        req2,
        2,
    ))
    .unwrap();

    let key1 = ProvisionJobRequestKey::new(&test_request_id("r1"), test_deployment_id(&dep_id()));
    let key2 =
        ProvisionJobRequestKey::new(&test_request_id("r1"), test_deployment_id(&other_dep_id()));
    advance_to_registration_pending(&store, &key1, 10);
    advance_to_registration_pending(&store, &key2, 20);

    // Authorization is the grant scope; an `OwnDeployment` issuer may only ack its own
    // canonical (request_id, deployment_id) key, so D1's issuer acks exactly A1.
    let result = complete_graph_registration_with_caller(
        deployment_issuer(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: test_deployment_id(&dep_id()),
            request_id: test_request_id("r1"),
        },
        31,
    );
    assert_eq!(result.unwrap(), RouterRegistrationAckResponse::Applied);
    let a1_after = store
        .get_by_request(&test_request_id("r1"), test_deployment_id(&dep_id()))
        .unwrap();
    assert_eq!(a1_after.current_state, JobState::Completed);
    // A2 is untouched.
    let a2_after = store
        .get_by_request(&test_request_id("r1"), test_deployment_id(&other_dep_id()))
        .unwrap();
    assert_eq!(a2_after.current_state, JobState::RouterRegistrationPending);
}

#[test]
fn test_provision_init_seeds_authority_and_survives_upgrade() {
    reset_all_maps();

    // Bootstrap init seeds the durable authority singleton.
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });

    // Simulate an upgrade: the singleton was written to stable memory, so a fresh store sees it.
    let auth_store = ProvisionBootstrapAuthStore::new();
    assert_eq!(
        auth_store.get_authority().unwrap().governance_principal,
        gov_principal()
    );
}

#[test]
fn accept_same_key_altered_envelope_conflicts_without_second_effect() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![
            test_resource(LogicalResource::GraphShard(ShardId::new(0))),
            test_resource(LogicalResource::PropertyIndex(
                gleaph_graph_kernel::federation::IndexClusterId::new(0),
            )),
        ],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req.clone(),
        1,
    ))
    .unwrap();
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), test_deployment_id(&dep_id()));
    let before_maps = store.provisioning_maps_snapshot_for_test();
    let before_effect_count = store
        .get_by_request_key(&key)
        .unwrap()
        .completed_effect_count;
    type ProvisionFieldMutator = fn(&mut ProvisionRequest);
    let changed_fields: [(&str, ProvisionFieldMutator); 6] = [
        ("intent", |altered| {
            altered.intent_key = ProvisioningIntentKey::new(
                test_deployment_id(&dep_id()),
                LogicalResource::PropertyIndex(
                    gleaph_graph_kernel::federation::IndexClusterId::new(0),
                ),
            );
        }),
        ("reserved graph", |altered| {
            altered.reserved_graph_id = Some(gleaph_graph_kernel::entry::GraphId::from_raw(77));
        }),
        ("resources", |altered| {
            altered.requested_resources[1] = test_resource(LogicalResource::VectorIndex(
                gleaph_graph_kernel::federation::VectorIndexId::new(9),
            ));
        }),
        ("install bytes", |altered| {
            altered.install_args[0] = vec![0xAA, 0xBB];
        }),
        ("authorized caller", |altered| {
            altered.authorized_caller = pid(31);
        }),
        ("release id", |altered| {
            altered.release_id = "r2".to_owned();
        }),
    ];

    for (field, mutate) in changed_fields {
        let mut altered = req.clone();
        mutate(&mut altered);
        let result = block_on(accept_envelope_with_caller(
            router_principal(),
            &store,
            &deployment_store,
            altered,
            2,
        ));
        assert_eq!(
            result,
            Err(ProvisionIngressError::Conflict),
            "changed {field} must conflict at the same canonical key"
        );
        assert_eq!(
            store.provisioning_maps_snapshot_for_test(),
            before_maps,
            "changed {field} must preserve Maps 1, 2, and 3 exactly"
        );
        assert_eq!(
            store
                .get_by_request_key(&key)
                .unwrap()
                .completed_effect_count,
            before_effect_count,
            "changed {field} must not add a management effect"
        );
    }
}

#[test]
fn registration_ack_completed_then_retry_returns_replay() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), test_deployment_id(&dep_id()));
    advance_to_registration_pending(&store, &key, 10);
    let ack = RouterRegistrationAck {
        deployment_id: test_deployment_id(&dep_id()),
        request_id: test_request_id("req-a"),
    };
    let first = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        ack.clone(),
        20,
    )
    .unwrap();
    let second = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        ack,
        21,
    )
    .unwrap();
    assert_eq!(first, RouterRegistrationAckResponse::Applied);
    assert_eq!(second, RouterRegistrationAckResponse::Replay);
}

// === helpers / record_to_result / get_by_request_id ==========================

#[test]
fn test_provision_record_to_result_reserved_state_returns_err() {
    let record = build_record_from_request(
        test_request(
            test_deployment_id(&dep_id()),
            "req-a",
            "fp-a",
            vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
        ),
        1,
        1,
    );
    let mut record = record;
    record.current_state = JobState::Reserved;
    assert_eq!(
        record_to_result(&record),
        Err(ProvisionIngressError::InvalidState),
        "a non-terminal job must not be mapped to a terminal ProvisionResult"
    );

    // Adversarial: a wrong impl that fabricates Ok(Failed { reason }) for Reserved
    // would not satisfy this assertion, because the helper contract now returns Err.
    let wrong_result = ProvisionResult {
        request_id: record.request_id,
        release_id: record.release_id.clone(),
        created_resources: vec![],
        terminal_outcome: ProvisionResultOutcome::Failed {
            reason: "job not yet terminal: Reserved".to_owned(),
        },
    };
    assert_ne!(
        Ok(wrong_result),
        record_to_result(&record),
        "wrong impl returning a fabricated terminal result for Reserved must fail"
    );
}

#[test]
fn test_provision_record_to_result_completed_with_missing_canister_id() {
    let mut record = build_record_from_request(
        test_request(
            test_deployment_id(&dep_id()),
            "req-a",
            "fp-a",
            vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
        ),
        1,
        1,
    );
    record.current_state = JobState::Completed;
    assert_eq!(
        record_to_result(&record),
        Err(ProvisionIngressError::ResultMappingError)
    );
}

#[test]
fn test_provision_get_by_request_exact_key_lookup() {
    reset_all_maps();
    let store = ProvisionJobStore::new();
    let record_a = build_record_from_request(
        test_request(
            test_deployment_id(&dep_id()),
            "req-a",
            "fp-a",
            vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
        ),
        1,
        1,
    );
    let record_b = build_record_from_request(
        test_request(
            "dep-b",
            "req-b",
            "fp-b",
            vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
        ),
        1,
        1,
    );
    store.insert_or_idempotent(record_a.clone()).unwrap();
    store.insert_or_idempotent(record_b).unwrap();
    assert_eq!(
        store.get_by_request(&test_request_id("missing"), test_deployment_id(&dep_id())),
        None
    );
    assert_eq!(
        store.get_by_request(&test_request_id("req-a"), test_deployment_id(&dep_id())),
        Some(record_a)
    );
}

#[test]
fn test_provision_error_variant_coverage_map() {
    // Every variant of every error enum must be reachable by a dedicated test name.
    fn ingress_name(e: ProvisionIngressError) -> &'static str {
        match e {
            ProvisionIngressError::NotAuthorized => "test_provision_accept_wrong_caller_rejected",
            ProvisionIngressError::UnknownDeployment => {
                "test_provision_accept_unknown_deployment_rejected"
            }
            ProvisionIngressError::Conflict => {
                "test_provision_accept_conflict_different_fingerprint"
            }
            ProvisionIngressError::NotFound => "registration_ack_not_found",
            ProvisionIngressError::InvalidState => "registration_ack_invalid_state",
            ProvisionIngressError::StateAdvanceFailed => {
                "test_provision_router_ack_state_advance_failed_returns_state_advance_failed"
            }
            ProvisionIngressError::ResultMappingError => {
                "test_provision_record_to_result_completed_with_missing_canister_id"
            }
            ProvisionIngressError::IntentLockHeld => {
                "test_provision_no_partial_writes_on_lock_failure"
            }
            ProvisionIngressError::RouterAlreadyIssued { .. } => {
                "singleton_issuance_blocks_a_completed_router"
            }
            ProvisionIngressError::RouterIssuanceInProgress => {
                "singleton_router_issuance_rejects_a_second_request_for_the_same_deployment"
            }
            ProvisionIngressError::InsufficientCycles { .. } => {
                "funded_admission_rejects_insufficient_cycles"
            }
            ProvisionIngressError::InvalidResources { .. } => {
                "test_provision_accept_duplicate_resources_rejected"
            }
        }
    }
    fn query_name(e: ProvisionQueryError) -> &'static str {
        match e {
            ProvisionQueryError::NotAuthorized => "test_provision_query_wrong_caller_rejected",
            ProvisionQueryError::UnknownDeployment => {
                "test_provision_query_unknown_deployment_returns_not_found"
            }
            ProvisionQueryError::NotFound => "test_provision_query_not_found",
        }
    }

    // Construct each variant once to prove the match arms are exhaustive.
    assert!(!ingress_name(ProvisionIngressError::NotAuthorized).is_empty());
    assert!(!ingress_name(ProvisionIngressError::UnknownDeployment).is_empty());
    assert!(!ingress_name(ProvisionIngressError::Conflict).is_empty());
    assert!(!ingress_name(ProvisionIngressError::NotFound).is_empty());
    assert!(!ingress_name(ProvisionIngressError::InvalidState).is_empty());
    assert!(!ingress_name(ProvisionIngressError::StateAdvanceFailed).is_empty());
    assert!(!ingress_name(ProvisionIngressError::ResultMappingError).is_empty());
    assert!(!ingress_name(ProvisionIngressError::IntentLockHeld).is_empty());
    assert!(
        !ingress_name(ProvisionIngressError::RouterAlreadyIssued {
            request_id: [1u8; 32]
        })
        .is_empty()
    );
    assert!(!ingress_name(ProvisionIngressError::RouterIssuanceInProgress).is_empty());
    assert!(
        !ingress_name(ProvisionIngressError::InsufficientCycles {
            required: 10,
            available: 1
        })
        .is_empty()
    );
    assert!(
        !ingress_name(ProvisionIngressError::InvalidResources {
            reason: String::new()
        })
        .is_empty()
    );

    assert!(!query_name(ProvisionQueryError::NotAuthorized).is_empty());
    assert!(!query_name(ProvisionQueryError::UnknownDeployment).is_empty());
    assert!(!query_name(ProvisionQueryError::NotFound).is_empty());
}

#[test]
fn test_provision_own_issuer_cannot_query_another_deployment() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&other_dep_id()),
        "req-query-other",
        "fp-query-other",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        deployment_issuer(),
        &store,
        &deployment_store,
        req.clone(),
        1,
    ))
    .expect_err("an own issuer cannot accept another deployment");
    let result = query_job_with_caller(
        deployment_issuer(),
        &store,
        &deployment_store,
        req.request_id,
        test_deployment_id(&other_dep_id()),
    );
    assert_eq!(result, Err(ProvisionQueryError::NotAuthorized));
}

#[test]
fn test_provision_query_not_found() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let result = query_job_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        test_request_id("missing"),
        test_deployment_id(&dep_id()),
    );
    assert_eq!(result, Err(ProvisionQueryError::NotFound));
}

// === upsert_deployment_grant =========================================

fn upsert_args(issuer: Principal) -> UpsertDeploymentGrantArgs {
    UpsertDeploymentGrantArgs {
        issuer,
        scope: DeploymentGrantScope::OwnDeployment,
    }
}

fn upsert_relay_args(issuer: Principal) -> UpsertDeploymentGrantArgs {
    UpsertDeploymentGrantArgs {
        issuer,
        scope: DeploymentGrantScope::Relay,
    }
}

#[test]
fn upsert_deployment_grant_records_the_relay_scope() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let account = pid(200);
    upsert_deployment_grant_with_caller(gov_principal(), upsert_relay_args(account), 1)
        .expect("governance seeds the relay");
    assert_eq!(
        DeploymentGrantStore::new().scope(&account),
        Some(DeploymentGrantScope::Relay)
    );
}

#[test]
fn upsert_deployment_grant_by_governance_is_idempotent() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let bootstrap = gov_principal();
    let deployment_store = DeploymentGrantStore::new();
    let issuer = pid(10);

    let first = upsert_deployment_grant_with_caller(bootstrap, upsert_args(issuer), 1).unwrap();
    assert_eq!(first.action, BootstrapAuthAction::Upsert);
    assert_eq!(first.caller, bootstrap);

    // Idempotent: upserting again succeeds with no state change.
    let second = upsert_deployment_grant_with_caller(bootstrap, upsert_args(issuer), 2).unwrap();
    assert_eq!(second.action, BootstrapAuthAction::Upsert);
    assert!(deployment_store.contains(&issuer));
    assert!(!deployment_store.contains(&pid(11)));
}

#[test]
fn upsert_deployment_grant_rejects_anonymous_issuer() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let auth_store = ProvisionBootstrapAuthStore::new();
    let err = upsert_deployment_grant_with_caller(
        gov_principal(),
        upsert_args(Principal::anonymous()),
        1,
    )
    .unwrap_err();
    assert!(matches!(err, ProvisionAdminError::Unauthorized));
    let latest = auth_store.latest(gov_principal()).unwrap();
    assert_eq!(latest.action, BootstrapAuthAction::RejectUnauthorized);
}

#[test]
fn upsert_deployment_grant_by_non_governance_returns_unauthorized_with_reject_audit() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let auth_store = ProvisionBootstrapAuthStore::new();

    let err = upsert_deployment_grant_with_caller(other_principal(), upsert_args(pid(10)), 1)
        .unwrap_err();
    assert_eq!(err, ProvisionAdminError::Unauthorized);
    let latest = auth_store.latest(other_principal()).unwrap();
    assert_eq!(latest.action, BootstrapAuthAction::RejectUnauthorized);
    assert_eq!(latest.deployment_id, Some(pid(10).to_text()));
}

#[test]
fn upsert_deployment_grant_with_no_bootstrap_authority_returns_not_seeded_with_reject_audit() {
    reset_all_maps();
    let auth_store = ProvisionBootstrapAuthStore::new();

    let err = upsert_deployment_grant_with_caller(other_principal(), upsert_args(pid(10)), 1)
        .unwrap_err();
    assert_eq!(err, ProvisionAdminError::NoBootstrapAuthority);
    let latest = auth_store.latest(other_principal()).unwrap();
    assert_eq!(latest.action, BootstrapAuthAction::RejectNotSeeded);
}

#[test]
fn upsert_deployment_grant_audit_log_survives_handler_return_path() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    });
    let bootstrap = gov_principal();
    let auth_store = ProvisionBootstrapAuthStore::new();

    let entry = upsert_deployment_grant_with_caller(bootstrap, upsert_args(deployment_issuer()), 1)
        .unwrap();
    assert_eq!(entry.action, BootstrapAuthAction::Upsert);

    let history = auth_store.history(bootstrap);
    assert!(
        history
            .iter()
            .any(|e| e.action == BootstrapAuthAction::Upsert)
    );
    assert_eq!(auth_store.latest(bootstrap), Some(entry));
    // The grant is durably persisted.
    assert!(DeploymentGrantStore::new().contains(&deployment_issuer()));
}

#[test]
fn bootstrap_authority_singleton_survives_upgrade() {
    reset_all_maps();
    let args = init::ProvisionInitArgs {
        governance_principal: gov_principal(),
    };
    init::init(args.clone());
    let auth_store = ProvisionBootstrapAuthStore::new();
    let first = auth_store.get_authority().unwrap();

    // Simulate a canister upgrade/re-init: stable memory persists; init re-runs with the same args.
    init::init(args);
    let second = auth_store.get_authority().unwrap();
    assert_eq!(first, second);
}

// === Artifact catalog handler tests (Plan 0061a) =============================

fn gov() -> Principal {
    Principal::from_slice(&[100; 29])
}

fn artifact_id_router(version: &str, full_sha: [u8; 32]) -> ArtifactId {
    ArtifactId::new(CanisterKind::Router, version.to_owned(), full_sha)
}

fn seed_bootstrap() {
    crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore::new().set_authority(
        BootstrapAuthorityRecord {
            governance_principal: gov(),
            seeded_at_ns: 1,
        },
    );
}

fn publish_args(
    version: &str,
    chunks: Vec<&[u8]>,
) -> (ArtifactPublishMetadataArgs, ArtifactId, Vec<[u8; 32]>) {
    let full: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let full_sha = sha256(&full);
    let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|c| sha256(c)).collect();
    let id = artifact_id_router(version, full_sha);
    (
        ArtifactPublishMetadataArgs {
            canister_kind: CanisterKind::Router,
            semantic_version: version.to_owned(),
            sha256: full_sha,
            byte_length: full.len() as u64,
            chunk_hashes: chunk_hashes.clone(),
        },
        id,
        chunk_hashes,
    )
}

/// (c) artifact_publish_metadata rejects non-bootstrap caller.
#[test]
fn artifact_publish_metadata_rejects_non_bootstrap_caller() {
    reset_all_maps();
    seed_bootstrap();
    let (args, id, _) = publish_args("0.1.0", vec![b"chunk0"]);
    let err = artifact_publish_metadata_with_caller(other_principal(), args, 1).unwrap_err();
    assert_eq!(err, ArtifactError::Unauthorized);
    assert!(ProvisionArtifactStore::new().get_metadata(&id).is_none());
}

/// (d) artifact_upload_chunk validates chunk hash.
#[test]
fn artifact_upload_chunk_validates_chunk_hash() {
    reset_all_maps();
    seed_bootstrap();
    let (args, id, _) = publish_args("0.1.0", vec![b"chunk0"]);
    artifact_publish_metadata_with_caller(gov(), args, 1).unwrap();

    let err = artifact_upload_chunk_with_caller(
        gov(),
        ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 0,
            bytes: b"wrong".to_vec(),
        },
        2,
    )
    .unwrap_err();
    assert!(
        matches!(err, ArtifactError::ChunkHashMismatch { artifact_id: ref aid, chunk_index: 0 } if **aid == id),
        "expected ChunkHashMismatch, got {:?}",
        err
    );

    // Region 7 stays Receiving (no upload entry created because no chunk was staged).
    assert!(ProvisionArtifactStore::new().get_upload(&id).is_none());
}

/// (g) artifact_upload_chunk rejects out-of-range chunk index.
#[test]
fn artifact_upload_chunk_rejects_out_of_range_index() {
    reset_all_maps();
    seed_bootstrap();
    let (args, id, _) = publish_args("0.1.0", vec![b"chunk0"]);
    artifact_publish_metadata_with_caller(gov(), args, 1).unwrap();

    let err = artifact_upload_chunk_with_caller(
        gov(),
        ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 5,
            bytes: b"chunk0".to_vec(),
        },
        2,
    )
    .unwrap_err();
    assert!(
        matches!(err, ArtifactError::ChunkOutOfRange { artifact_id: ref aid, chunk_index: 5, declared: 1 } if **aid == id),
        "expected ChunkOutOfRange, got {:?}",
        err
    );
}

/// (h) artifact_get_status returns None for verified artifact (and after rejected post-verify attempt).
#[test]
fn artifact_get_status_returns_none_for_verified_artifact() {
    reset_all_maps();
    seed_bootstrap();
    let (args, id, _) = publish_args("0.2.0", vec![b"aaaa", b"bbbb"]);
    artifact_publish_metadata_with_caller(gov(), args, 1).unwrap();

    artifact_upload_chunk_with_caller(
        gov(),
        ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 0,
            bytes: b"aaaa".to_vec(),
        },
        2,
    )
    .unwrap();
    assert!(artifact_get_status(id.clone()).is_some());

    // Complete verification.
    artifact_upload_chunk_with_caller(
        gov(),
        ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 1,
            bytes: b"bbbb".to_vec(),
        },
        3,
    )
    .unwrap();

    // Region 7 was deleted on verify success.
    assert_eq!(artifact_get_status(id.clone()), None);

    // Rejected post-verify upload also does not recreate region 7.
    let rejected = artifact_upload_chunk_with_caller(
        gov(),
        ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 0,
            bytes: b"aaaa".to_vec(),
        },
        4,
    );
    assert!(matches!(
        rejected,
        Err(ArtifactError::ConflictingMetadata { .. })
    ));
    assert_eq!(artifact_get_status(id), None);
}

// === Release manifest + active release handler tests (Plan 0061b) ==========

fn release_id(name: &str) -> ReleaseId {
    ReleaseId(name.to_owned())
}

fn artifact_id_for_kind(kind: CanisterKind, version: &str, full_sha: [u8; 32]) -> ArtifactId {
    ArtifactId::new(kind, version.to_owned(), full_sha)
}

fn publish_verified_artifact_for_release(
    kind: CanisterKind,
    version: &str,
    chunks: Vec<&[u8]>,
) -> ArtifactId {
    let full: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let full_sha = sha256(&full);
    let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|c| sha256(c)).collect();
    let id = artifact_id_for_kind(kind.clone(), version, full_sha);

    artifact_publish_metadata_with_caller(
        gov(),
        ArtifactPublishMetadataArgs {
            canister_kind: kind,
            semantic_version: version.to_owned(),
            sha256: full_sha,
            byte_length: full.len() as u64,
            chunk_hashes: chunk_hashes.clone(),
        },
        1,
    )
    .unwrap();

    for (i, chunk) in chunks.iter().enumerate() {
        artifact_upload_chunk_with_caller(
            gov(),
            ArtifactUploadChunkArgs {
                artifact_id: id.clone(),
                chunk_index: i as u32,
                bytes: chunk.to_vec(),
            },
            2 + i as u64,
        )
        .unwrap();
    }
    id
}

fn publish_compatible_release(r: ReleaseId) {
    let ids = vec![
        publish_verified_artifact_for_release(CanisterKind::Router, "0.1.0", vec![b"r0"]),
        publish_verified_artifact_for_release(CanisterKind::Graph, "0.1.0", vec![b"g0"]),
        publish_verified_artifact_for_release(CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]),
        publish_verified_artifact_for_release(CanisterKind::VectorCanister, "0.1.0", vec![b"v0"]),
        publish_verified_artifact_for_release(CanisterKind::TextCanister, "0.1.0", vec![b"t0"]),
        publish_verified_artifact_for_release(CanisterKind::Inference, "0.1.0", vec![b"i0"]),
    ];
    release_publish_with_caller(
        gov(),
        ReleasePublishArgs {
            release_id: r,
            artifact_ids: ids,
        },
        100,
    )
    .unwrap();
}

/// (h) release_activate rejects a non-bootstrap caller and leaves the active pointer unchanged.
#[test]
fn release_activate_rejects_non_bootstrap_caller() {
    reset_all_maps();
    seed_bootstrap();

    let r = release_id("release-h");
    publish_compatible_release(r.clone());

    let err = release_activate_with_caller(
        other_principal(),
        ReleaseActivateArgs { release_id: r },
        200,
    )
    .unwrap_err();

    assert_eq!(err, ReleaseError::Unauthorized);
    assert!(release_get_active().is_none());
}

/// (i) release_get_active returns None initially and Some after activation with sentinel fields.
#[test]
fn release_get_active_returns_none_when_no_release() {
    reset_all_maps();
    assert!(release_get_active().is_none());

    seed_bootstrap();
    let r = release_id("release-i");
    publish_compatible_release(r.clone());

    release_activate_with_caller(
        gov_principal(),
        ReleaseActivateArgs {
            release_id: r.clone(),
        },
        200,
    )
    .unwrap();

    let active = release_get_active().unwrap();
    assert_eq!(active.release_id, r);
    assert_eq!(active.activated_at_ns, 0);
    assert_eq!(active.previous_release_id, None);
}

/// Block on an immediately-ready future without adding an async runtime dependency.
fn block_on<F: Future>(f: F) -> F::Output {
    let waker = Waker::noop();
    let mut cx = Context::from_waker(waker);
    let mut f = std::pin::pin!(f);
    match f.as_mut().poll(&mut cx) {
        Poll::Ready(v) => v,
        Poll::Pending => panic!("future not ready"),
    }
}

fn install_target() -> Principal {
    Principal::from_slice(&[0xDD; 29])
}

/// (e) release_install rejects no active release and writes audit entry (R4).
#[test]
fn release_install_rejects_no_active_release() {
    reset_all_maps();
    seed_bootstrap();
    let args = ReleaseInstallArgs {
        target_canister_kind: CanisterKind::Router,
        target_canister_id: Some(install_target()),
        install_args: vec![],
        registry_version: 1,
    };
    let result = block_on(release_install_with_caller(gov_principal(), args, 100));
    assert_eq!(result, Err(InstallError::NoActiveRelease));
    let history = ProvisionArtifactStore::new().audit_history(gov_principal());
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].action,
        crate::types::ArtifactAuditAction::InstallRelease
    );
    assert_eq!(
        history[0].outcome,
        crate::types::ArtifactAuditOutcome::Failed
    );
}

/// (f) release_install rejects unauthorized caller and writes audit entry (R4).
#[test]
fn release_install_rejects_unauthorized_caller() {
    reset_all_maps();
    seed_bootstrap();
    let args = ReleaseInstallArgs {
        target_canister_kind: CanisterKind::Router,
        target_canister_id: Some(install_target()),
        install_args: vec![],
        registry_version: 1,
    };
    let caller = other_principal();
    let result = block_on(release_install_with_caller(caller, args, 100));
    assert_eq!(result, Err(InstallError::Unauthorized));
    let history = ProvisionArtifactStore::new().audit_history(caller);
    assert_eq!(history.len(), 1);
    assert_eq!(
        history[0].action,
        crate::types::ArtifactAuditAction::InstallRelease
    );
    assert_eq!(
        history[0].outcome,
        crate::types::ArtifactAuditOutcome::Rejected
    );
}

/// (g) release_install rejects unverified artifact and writes audit entry (R4).
#[test]
fn release_install_rejects_unverified_artifact() {
    reset_all_maps();
    seed_bootstrap();
    let r = release_id("release-g-install");
    let router = publish_verified_artifact_for_release(CanisterKind::Router, "0.1.0", vec![b"r0"]);
    let graph = publish_verified_artifact_for_release(CanisterKind::Graph, "0.1.0", vec![b"g0"]);
    let prop =
        publish_verified_artifact_for_release(CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]);
    let vector =
        publish_verified_artifact_for_release(CanisterKind::VectorCanister, "0.1.0", vec![b"v0"]);
    let text =
        publish_verified_artifact_for_release(CanisterKind::TextCanister, "0.1.0", vec![b"t0"]);
    let inference =
        publish_verified_artifact_for_release(CanisterKind::Inference, "0.1.0", vec![b"i0"]);
    release_publish_with_caller(
        gov_principal(),
        ReleasePublishArgs {
            release_id: r.clone(),
            artifact_ids: vec![router, graph, prop, vector.clone(), text, inference],
        },
        100,
    )
    .unwrap();
    release_activate_with_caller(
        gov_principal(),
        ReleaseActivateArgs {
            release_id: r.clone(),
        },
        200,
    )
    .unwrap();

    // Remove the verified chunks for the vector artifact to make it unverified at install time.
    let vector_storage_id = ProvisionArtifactStore::new()
        .storage_id_of(&vector)
        .expect("vector storage id");
    ProvisionArtifactStore::new().remove_all_chunks(vector_storage_id);

    let args = ReleaseInstallArgs {
        target_canister_kind: CanisterKind::VectorCanister,
        target_canister_id: Some(install_target()),
        install_args: vec![],
        registry_version: 1,
    };
    let result = block_on(release_install_with_caller(gov_principal(), args, 300));
    assert_eq!(
        result,
        Err(InstallError::ArtifactNotVerified(vector.clone()))
    );
    let history = ProvisionArtifactStore::new().audit_history(gov_principal());
    let activate = history
        .iter()
        .find(|e| e.action == crate::types::ArtifactAuditAction::ActivateRelease)
        .unwrap();
    assert_eq!(
        activate.outcome,
        crate::types::ArtifactAuditOutcome::Success
    );
    let install = history
        .iter()
        .find(|e| e.action == crate::types::ArtifactAuditAction::InstallRelease)
        .unwrap();
    assert_eq!(install.outcome, crate::types::ArtifactAuditOutcome::Failed);
    assert_eq!(install.artifact_id, Some(vector));
}

/// (h) release_install audit log survives successful handler return.
#[test]
fn release_install_audit_log_survives_handler_return() {
    reset_all_maps();
    seed_bootstrap();
    let r = release_id("release-h-install");
    publish_compatible_release(r.clone());
    release_activate_with_caller(
        gov_principal(),
        ReleaseActivateArgs {
            release_id: r.clone(),
        },
        200,
    )
    .unwrap();

    let target = install_target();
    let args = ReleaseInstallArgs {
        target_canister_kind: CanisterKind::Router,
        target_canister_id: Some(target),
        install_args: vec![],
        registry_version: 1,
    };
    let result = block_on(release_install_with_caller(gov_principal(), args, 300)).unwrap();
    assert_eq!(result.release_id, r);
    assert_eq!(result.target_canister_id, target);
    assert_eq!(result.installed_chunks, 1);

    let history = ProvisionArtifactStore::new().audit_history(gov_principal());
    let install = history
        .iter()
        .find(|e| e.action == crate::types::ArtifactAuditAction::InstallRelease)
        .unwrap();
    assert_eq!(install.outcome, crate::types::ArtifactAuditOutcome::Success);
    assert_eq!(install.target_canister, Some(target));
}

/// The production owner lifecycle survives a Map 1/2/3 reopen, replays without repeating effects,
/// completes with exact owned-row release, and leaves a later request's rows untouched.
#[test]
fn provision_owner_durable_lifecycle_reopens_and_replays_without_repeating_effects() {
    reset_all_maps();
    seed_bootstrap();
    let r = release_id("release-deploy");
    publish_compatible_release(r.clone());
    release_activate_with_caller(gov_principal(), ReleaseActivateArgs { release_id: r }, 200)
        .unwrap();

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-deploy",
        "fp-deploy",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let expected_digest = sha256(&candid::Encode!(&req).unwrap());
    let resp = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req.clone(),
        1,
    ))
    .unwrap();
    let accepted_resources = match resp {
        ProvisionAcceptResponse::Accepted {
            job_view,
            created_resources,
            intent_lock_count,
        } => {
            assert_eq!(
                job_view.state, "RouterRegistrationPending",
                "deploy must advance to RouterRegistrationPending"
            );
            assert_eq!(job_view.completed_effect_count, 3);
            assert_eq!(intent_lock_count, 1);
            assert_eq!(created_resources.len(), 1);
            let created = &created_resources[0];
            assert_eq!(
                created.logical_resource,
                LogicalResource::GraphShard(ShardId::new(0))
            );
            assert_ne!(created.canister_id, Principal::anonymous());
            assert!(!created.artifact_hash.is_empty());
            created_resources
        }
        other => panic!("expected Accepted with created resources, got {other:?}"),
    };

    let pending_record = store
        .get_by_request(
            &test_request_id("req-deploy"),
            test_deployment_id(&dep_id()),
        )
        .unwrap();
    assert_eq!(
        pending_record.current_state,
        JobState::RouterRegistrationPending
    );
    assert_eq!(pending_record.completed_effect_count, 3);
    assert_eq!(pending_record.immutable_request_digest, expected_digest);
    assert_eq!(pending_record.resources.len(), accepted_resources.len());
    assert_eq!(
        pending_record.resources[0].logical_resource,
        accepted_resources[0].logical_resource
    );
    assert_eq!(
        pending_record.resources[0].canister_id,
        Some(accepted_resources[0].canister_id)
    );
    assert_eq!(
        pending_record.resources[0].artifact_hash,
        Some(accepted_resources[0].artifact_hash)
    );
    let pending_maps = store.provisioning_maps_snapshot_for_test();

    reopen_provisioning_regions_for_test();
    let reopened_store = ProvisionJobStore::new();
    let reopened_deployment_store = DeploymentGrantStore::new();
    assert_eq!(
        reopened_store.provisioning_maps_snapshot_for_test(),
        pending_maps,
        "Maps 1, 2, and 3 must reopen over the same stable bytes"
    );

    let replay = block_on(accept_envelope_with_caller(
        router_principal(),
        &reopened_store,
        &reopened_deployment_store,
        req.clone(),
        2,
    ))
    .unwrap();
    match replay {
        ProvisionAcceptResponse::Replay {
            job_view,
            intent_lock_count,
            created_resources,
        } => {
            assert_eq!(job_view.state, "RouterRegistrationPending");
            assert_eq!(job_view.completed_effect_count, 3);
            assert_eq!(intent_lock_count, 1);
            assert_eq!(created_resources, accepted_resources);
        }
        other => panic!("expected durable admission Replay, got {other:?}"),
    }
    assert_eq!(
        reopened_store.provisioning_maps_snapshot_for_test(),
        pending_maps,
        "admission replay must not repeat an effect or rewrite durable state"
    );

    let ack = RouterRegistrationAck {
        deployment_id: test_deployment_id(&dep_id()),
        request_id: test_request_id("req-deploy"),
    };
    assert_eq!(
        complete_graph_registration_with_caller(
            router_principal(),
            &reopened_store,
            &reopened_deployment_store,
            ack.clone(),
            3,
        ),
        Ok(RouterRegistrationAckResponse::Applied)
    );
    let completed_record = reopened_store
        .get_by_request(
            &test_request_id("req-deploy"),
            test_deployment_id(&dep_id()),
        )
        .unwrap();
    assert_eq!(completed_record.current_state, JobState::Completed);
    assert_eq!(completed_record.resources, pending_record.resources);
    assert_eq!(completed_record.completed_effect_count, 3);
    assert_eq!(completed_record.immutable_request_digest, expected_digest);
    let intent_key = ProvisioningIntentKey::new(
        test_deployment_id(&dep_id()),
        LogicalResource::GraphShard(ShardId::new(0)),
    );
    assert_eq!(
        reopened_store.assert_intent_to_request_for_test(
            test_deployment_id(&dep_id()),
            LogicalResource::GraphShard(ShardId::new(0)),
        ),
        None,
        "Applied must release the exact Map 2 owner"
    );
    assert!(
        !reopened_store.intent_locked(&intent_key),
        "Applied must release Map 3"
    );

    let mut later_req = req;
    later_req.request_id = test_request_id("later-request");
    let later_key =
        ProvisionJobRequestKey::new(&later_req.request_id, test_deployment_id(&dep_id()));
    let later_accept = block_on(accept_envelope_with_caller(
        router_principal(),
        &reopened_store,
        &reopened_deployment_store,
        later_req,
        4,
    ))
    .unwrap();
    assert!(matches!(
        later_accept,
        ProvisionAcceptResponse::Accepted { .. }
    ));
    assert_eq!(
        reopened_store.assert_intent_to_request_for_test(
            test_deployment_id(&dep_id()),
            LogicalResource::GraphShard(ShardId::new(0)),
        ),
        Some(later_key)
    );
    assert!(reopened_store.intent_locked(&intent_key));
    let before_completed_replay = reopened_store.provisioning_maps_snapshot_for_test();

    assert_eq!(
        complete_graph_registration_with_caller(
            router_principal(),
            &reopened_store,
            &reopened_deployment_store,
            ack,
            5,
        ),
        Ok(RouterRegistrationAckResponse::Replay)
    );
    assert_eq!(
        reopened_store.provisioning_maps_snapshot_for_test(),
        before_completed_replay,
        "Completed replay must not inspect, release, or rewrite a later request's Map 2/3 rows"
    );
}

#[test]
fn initial_canister_cycles_budget_is_one_trillion() {
    assert_eq!(super::initial_canister_cycles(), 1_000_000_000_000);
}

// === Dictionary relay tests (plan 0335 todo 3) ================================

fn relay_catalog_key() -> DictCatalogKey {
    DictCatalogKey {
        kind: "ipadic".to_owned(),
        version: "2.7.0".to_owned(),
    }
}

fn korean_catalog_key() -> DictCatalogKey {
    DictCatalogKey {
        kind: "korean".to_owned(),
        version: "2.1.1-20180720".to_owned(),
    }
}

/// Seed a Finalized dictionary catalog entry holding `chunks` (one independent frame per
/// row, each digest-verified on upload); returns (raw_digest, raw_len).
fn seed_finalized_catalog(chunks: &[&[u8]]) -> (u128, u64) {
    seed_finalized_catalog_with_key(&relay_catalog_key(), chunks)
}

/// Key-parameterized seeding: same framed ingress path for any catalog entry, so the
/// ipadic + korean coexistence legs share one helper (plan 0341).
fn seed_finalized_catalog_with_key(key: &DictCatalogKey, chunks: &[&[u8]]) -> (u128, u64) {
    seed_bootstrap();
    for (i, chunk) in chunks.iter().enumerate() {
        admin_upload_dict_catalog_chunk_with_caller(
            gov(),
            DictCatalogUploadChunkArgs {
                key: key.clone(),
                chunk_index: i as u32,
                frame_digest: xxhash_rust::xxh3::xxh3_128(chunk),
                bytes: chunk.to_vec(),
            },
            300 + i as u64,
        )
        .expect("catalog chunk upload");
    }
    let raw_digest = 0x1234_5678_9abc_def0_1111_2222_3333_4444u128;
    let raw_len = 100;
    admin_finalize_dict_catalog_with_caller(
        gov(),
        DictCatalogFinalizeArgs {
            key: key.clone(),
            raw_digest,
            raw_len,
        },
        400,
    )
    .expect("catalog finalize");
    (raw_digest, raw_len)
}

/// Router-shaped text install args: controller = issuing Router, Provision pinned as the
/// dictionary-relay caller (plan 0335 authorization), admission-canonical kinds
/// (plan 0343): 0/2 carry [Japanese], 3 carries [Korean], 1/None carry nothing.
/// Tests needing non-canonical kinds use `text_install_args_with_kinds`.
fn text_install_args(analyzer_id: Option<u32>) -> Vec<u8> {
    let kinds = match analyzer_id {
        Some(0) | Some(2) => Some(vec![DictKind::Japanese]),
        Some(3) => Some(vec![DictKind::Korean]),
        _ => None,
    };
    text_install_args_with_kinds(analyzer_id, kinds)
}

/// Non-canonical kinds path: explicit kind selection for opt-in edge legs.
fn text_install_args_with_kinds(analyzer_id: Option<u32>, kinds: Option<Vec<DictKind>>) -> Vec<u8> {
    Encode!(&TextCanisterInitArgs {
        controller: Some(router_principal()),
        analyzer_id,
        dict_relay_caller: Some(pid(200)),
        kinds,
    })
    .expect("encode TextCanisterInitArgs")
}

/// Seed + activate the compatible release so `install_resource` succeeds on the native stubs.
fn seed_active_release() {
    seed_bootstrap();
    let r = release_id("relay-release");
    publish_compatible_release(r.clone());
    release_activate_with_caller(gov_principal(), ReleaseActivateArgs { release_id: r }, 500)
        .expect("activate release");
}

fn scripted_status(state: DictState, digest: Option<u128>) -> Option<Vec<u8>> {
    Some(
        Encode!(&DictStatus {
            state,
            digest,
            len: 0,
        })
        .expect("encode status reply"),
    )
}

fn relay_request(
    resources: Vec<ProvisionableResource>,
    install_args: Vec<Vec<u8>>,
) -> ProvisionRequest {
    let mut req = test_request(dep_id().as_str(), "relay-req", "fp-relay", resources);
    req.install_args = install_args;
    req
}

fn relay_job_state(store: &ProvisionJobStore) -> JobState {
    store
        .get_by_request_key(&ProvisionJobRequestKey::new(
            &test_request_id("relay-req"),
            test_deployment_id(&dep_id()),
        ))
        .expect("relay job record")
        .current_state
}

/// (1) Happy path: a dictionary-required text resource streams every catalog frame (one
/// relay call per frame, byte-exact) and finalizes; the job completes.
#[test]
fn dict_relay_success_streams_catalog_chunks_and_finalizes() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    let chunks: [&[u8]; 2] = [&[0xA0; 300], &[0xB1; 200]];
    let (raw_digest, raw_len) = seed_finalized_catalog(&chunks);
    seed_active_release();

    // Fresh canister status, one upload reply PER FRAME (frame i = relay call i, plan
    // 0342), then finalize.
    script_dict_relay_call(scripted_status(DictState::Absent, None));
    for chunk in &chunks {
        script_dict_relay_call(Some(
            Encode!(&Ok::<u64, String>(chunk.len() as u64)).unwrap(),
        ));
    }
    script_dict_relay_call(Some(
        Encode!(&Ok::<DictStatus, String>(DictStatus {
            state: DictState::Finalized,
            digest: Some(raw_digest),
            len: raw_len,
        }))
        .unwrap(),
    ));

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(
        vec![test_resource(LogicalResource::TextIndex(TextIndexId::new(
            0,
        )))],
        vec![text_install_args(Some(0))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => {
            assert_eq!(created_resources.len(), 1);
            assert!(matches!(
                created_resources[0].logical_resource,
                LogicalResource::TextIndex(_)
            ));
        }
        ProvisionAcceptResponse::Replay { .. } => {
            panic!("fresh admission must report Accepted")
        }
    }
    assert_eq!(relay_job_state(&store), JobState::RouterRegistrationPending);
    // Frame i == catalog row i == relay call i: two frames, two calls, byte-exact each.
    assert_eq!(
        dict_relay_call_log_for_test(),
        vec![
            DictRelayCall::Status,
            DictRelayCall::Upload {
                bytes: chunks[0].to_vec()
            },
            DictRelayCall::Upload {
                bytes: chunks[1].to_vec()
            },
            DictRelayCall::Finalize,
        ]
    );
}

/// (1b) The relay streams ONE call per catalog frame and the transferred bytes are
/// byte-identical to the catalog frames (frame i bytes == relay call i bytes, plan 0342).
#[test]
fn dict_relay_streams_one_call_per_catalog_frame() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    // 7 frames of 307,200 bytes (all <= the relay cap; each frame is an independent row).
    let row = [0xC3u8; 300 * 1024]; // 307,200 bytes per stored frame
    let chunks: Vec<Vec<u8>> = (0..7)
        .map(|i| {
            let mut r = row;
            r[0] = i as u8; // distinguish frames inside the stream
            r.to_vec()
        })
        .collect();
    let refs: Vec<&[u8]> = chunks.iter().map(|c| c.as_slice()).collect();
    let (raw_digest, raw_len) = seed_finalized_catalog(&refs);
    seed_active_release();

    script_dict_relay_call(scripted_status(DictState::Absent, None));
    for chunk in &chunks {
        script_dict_relay_call(Some(
            Encode!(&Ok::<u64, String>(chunk.len() as u64)).unwrap(),
        ));
    }
    script_dict_relay_call(Some(
        Encode!(&Ok::<DictStatus, String>(DictStatus {
            state: DictState::Finalized,
            digest: Some(raw_digest),
            len: raw_len,
        }))
        .unwrap(),
    ));

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(
        vec![test_resource(LogicalResource::TextIndex(TextIndexId::new(
            0,
        )))],
        vec![text_install_args(Some(0))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => {
            assert_eq!(created_resources.len(), 1);
        }
        _ => panic!("expected Accepted"),
    }

    let log = dict_relay_call_log_for_test();
    let uploads: Vec<&Vec<u8>> = log
        .iter()
        .filter_map(|call| match call {
            DictRelayCall::Upload { bytes } => Some(bytes),
            _ => None,
        })
        .collect();
    // Call count == frame count, and call i carries frame i byte-exactly.
    assert_eq!(uploads.len(), chunks.len());
    for (k, frame) in chunks.iter().enumerate() {
        assert_eq!(
            uploads[k], frame,
            "relay call {k} must be frame {k} byte-exact"
        );
    }
    assert_eq!(log.first(), Some(&DictRelayCall::Status));
    assert_eq!(log.last(), Some(&DictRelayCall::Finalize));
}

/// (1c) Boundary: a frame of exactly the relay cap (the new catalog row cap) streams as
/// one relay call, byte-exact.
#[test]
fn dict_relay_max_cap_frame_passes_through_as_one_call() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    let chunks: [&[u8]; 1] = [&[0xD4u8; MAX_DICT_COMPRESSED_CHUNK_BYTES]];
    let (raw_digest, raw_len) = seed_finalized_catalog(&chunks);
    seed_active_release();

    script_dict_relay_call(scripted_status(DictState::Absent, None));
    script_dict_relay_call(Some(
        Encode!(&Ok::<u64, String>(MAX_DICT_COMPRESSED_CHUNK_BYTES as u64)).unwrap(),
    ));
    script_dict_relay_call(Some(
        Encode!(&Ok::<DictStatus, String>(DictStatus {
            state: DictState::Finalized,
            digest: Some(raw_digest),
            len: raw_len,
        }))
        .unwrap(),
    ));

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(
        vec![test_resource(LogicalResource::TextIndex(TextIndexId::new(
            0,
        )))],
        vec![text_install_args(Some(0))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => {
            assert_eq!(created_resources.len(), 1);
        }
        _ => panic!("expected Accepted"),
    }

    let log = dict_relay_call_log_for_test();
    let uploads: Vec<&Vec<u8>> = log
        .iter()
        .filter_map(|call| match call {
            DictRelayCall::Upload { bytes } => Some(bytes),
            _ => None,
        })
        .collect();
    assert_eq!(uploads.len(), 1);
    assert_eq!(
        uploads[0], chunks[0],
        "the max-cap frame must transfer byte-exact"
    );
    assert_eq!(log.len(), 3);
    assert_eq!(log.last(), Some(&DictRelayCall::Finalize));
}

/// (2) A non-dictionary-required analyzer skips the relay entirely: zero seam calls even
/// though the catalog entry exists, and the job completes normally.
#[test]
fn dict_relay_skipped_for_non_dictionary_analyzer() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    seed_finalized_catalog(&[&[0xA0; 300]]);
    seed_active_release();

    // No script armed: any relay seam call would fail the job; a correct skip completes it.
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(
        vec![test_resource(LogicalResource::TextIndex(TextIndexId::new(
            0,
        )))],
        vec![text_install_args(Some(1))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => {
            assert_eq!(created_resources.len(), 1);
        }
        _ => panic!("expected Accepted"),
    }
    assert!(dict_relay_call_log_for_test().is_empty());
    assert_eq!(relay_job_state(&store), JobState::RouterRegistrationPending);
}

/// (3) Idempotent short-circuit: a canister already Finalized with the catalog's raw digest
/// is not re-appended — only the status probe is issued.
#[test]
fn dict_relay_short_circuits_on_finalized_match() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    let (raw_digest, _raw_len) = seed_finalized_catalog(&[&[0xA0; 300]]);
    seed_active_release();
    script_dict_relay_call(scripted_status(DictState::Finalized, Some(raw_digest)));

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(
        vec![test_resource(LogicalResource::TextIndex(TextIndexId::new(
            0,
        )))],
        vec![text_install_args(Some(2))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => {
            assert_eq!(created_resources.len(), 1);
        }
        _ => panic!("expected Accepted"),
    }
    assert_eq!(relay_job_state(&store), JobState::RouterRegistrationPending);
    assert_eq!(dict_relay_call_log_for_test(), vec![DictRelayCall::Status]);
}

/// (4) Relay call failure is fail-closed: the created prefix is stopped/deleted, the intent
/// claims are released, and one terminal Failed transition remains observable.
#[test]
fn dict_relay_failure_cleans_up_and_returns_no_created() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    seed_finalized_catalog(&[&[0xA0; 300]]);
    seed_active_release();
    script_dict_relay_call(scripted_status(DictState::Absent, None));
    script_dict_relay_call(None); // transport-level upload failure

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(
        vec![
            test_resource(LogicalResource::GraphShard(ShardId::new(0))),
            test_resource(LogicalResource::TextIndex(TextIndexId::new(0))),
        ],
        vec![vec![], text_install_args(Some(2))],
    );
    let request_id = req.request_id;
    let deployment_id = req.deployment_id.clone();
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => {
            // The shard created before the failing text resource was cleaned up.
            assert!(created_resources.is_empty());
        }
        _ => panic!("expected Accepted with an empty created list"),
    }
    assert_eq!(
        relay_job_state(&store),
        JobState::Failed {
            reason: "dictionary relay failed for resource 1".to_owned()
        }
    );
    let record = store
        .get_by_request(&request_id, deployment_id)
        .expect("failed record retained");
    assert_eq!(store.intent_lock_count_for_record(&record), 0);
    assert!(
        store
            .assert_intent_to_request_for_test(
                deployment_id,
                LogicalResource::GraphShard(ShardId::new(0))
            )
            .is_none()
    );
    assert_eq!(
        dict_relay_call_log_for_test(),
        vec![
            DictRelayCall::Status,
            DictRelayCall::Upload {
                bytes: vec![0xA0; 300]
            }
        ]
    );
}

/// A management-call failure during cleanup leaves the job `CleanupPending` with its
/// intent claims intact; replaying the same request resumes and terminalizes it.
#[test]
fn cleanup_failure_stays_pending_and_replay_resumes() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    seed_finalized_catalog(&[&[0xA0; 300]]);
    seed_active_release();
    script_dict_relay_call(scripted_status(DictState::Absent, None));
    script_dict_relay_call(None);

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(
        vec![
            test_resource(LogicalResource::GraphShard(ShardId::new(0))),
            test_resource(LogicalResource::TextIndex(TextIndexId::new(0))),
        ],
        vec![vec![], text_install_args(Some(2))],
    );
    let request_id = req.request_id;
    let deployment_id = req.deployment_id.clone();

    set_force_cleanup_error_for_test(true);
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req.clone(),
        600,
    ))
    .unwrap();
    assert!(matches!(result, ProvisionAcceptResponse::Accepted { .. }));
    let record = store
        .get_by_request(&request_id, deployment_id)
        .expect("job retained");
    assert!(matches!(
        record.current_state,
        JobState::CleanupPending { .. }
    ));
    assert!(store.intent_lock_count_for_record(&record) > 0);

    set_force_cleanup_error_for_test(false);
    let replay = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        700,
    ))
    .unwrap();
    match replay {
        ProvisionAcceptResponse::Replay {
            created_resources,
            job_view,
            ..
        } => {
            assert!(created_resources.is_empty());
            assert_eq!(job_view.state, "Failed");
        }
        _ => panic!("expected Replay"),
    }
    let record = store
        .get_by_request(&request_id, deployment_id)
        .expect("job retained");
    assert!(matches!(record.current_state, JobState::Failed { .. }));
    assert_eq!(store.intent_lock_count_for_record(&record), 0);
}

/// (5) A catalog entry that has not reached Finalized fails the relay closed BEFORE any
/// text-canister call.
#[test]
fn dict_relay_requires_finalized_catalog_entry() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    seed_bootstrap();
    // Uploading entry: chunk ingested, finalize never called.
    admin_upload_dict_catalog_chunk_with_caller(
        gov(),
        DictCatalogUploadChunkArgs {
            key: relay_catalog_key(),
            chunk_index: 0,
            frame_digest: xxhash_rust::xxh3::xxh3_128(&[0xA0u8; 300]),
            bytes: vec![0xA0; 300],
        },
        300,
    )
    .expect("catalog chunk upload");
    seed_active_release();

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(
        vec![test_resource(LogicalResource::TextIndex(TextIndexId::new(
            0,
        )))],
        vec![text_install_args(Some(0))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => {
            assert!(created_resources.is_empty());
        }
        _ => panic!("expected Accepted with an empty created list"),
    }
    assert_eq!(
        relay_job_state(&store),
        JobState::Failed {
            reason: "dictionary relay failed for resource 0".to_owned()
        }
    );
    assert!(dict_relay_call_log_for_test().is_empty());
}

/// (7) Plan 0343: kind→catalog-key mapping — Japanese resolves to the pinned ipadic 2.7.0
/// container, Korean to the pinned mecab-ko-dic 2.1.1-20180720 container. Total over
/// `DictKind`: no fallback arm exists.
#[test]
fn dict_catalog_key_mapping_per_kind() {
    assert_eq!(
        dict_catalog_key_for_kind(DictKind::Japanese),
        relay_catalog_key()
    );
    assert_eq!(
        dict_catalog_key_for_kind(DictKind::Korean),
        korean_catalog_key()
    );
}

/// (9) Plan 0343: an analyzer-0 resource with no selected kinds (None and Some([])
/// alike) streams ZERO dict calls and completes — the dictionary-less default.
#[test]
fn dict_relay_skipped_for_empty_kinds() {
    for kinds in [None, Some(vec![])] {
        reset_all_maps();
        reset_dict_relay_script_for_test();
        seed_finalized_catalog(&[&[0xA0; 300]]);
        seed_active_release();

        // No script armed: any relay seam call would fail the job; a correct skip
        // completes it.
        let (deployment_store, store) = init_and_grant(deployment_issuer());
        let req = relay_request(
            vec![test_resource(LogicalResource::TextIndex(TextIndexId::new(
                0,
            )))],
            vec![text_install_args_with_kinds(Some(0), kinds)],
        );
        let result = block_on(accept_envelope_with_caller(
            router_principal(),
            &store,
            &deployment_store,
            req,
            600,
        ))
        .unwrap();
        match result {
            ProvisionAcceptResponse::Accepted {
                created_resources, ..
            } => {
                assert_eq!(created_resources.len(), 1);
            }
            _ => panic!("expected Accepted"),
        }
        assert!(dict_relay_call_log_for_test().is_empty());
        assert_eq!(relay_job_state(&store), JobState::RouterRegistrationPending);
    }
}

/// (10) Plan 0343案b: multi-kind selection fails the relay in preflight, BEFORE any
/// text-canister call — no partial single-kind stream is emitted.
#[test]
fn dict_relay_multi_kind_fails_preflight_with_zero_calls() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    seed_finalized_catalog(&[&[0xA0; 300]]);
    seed_finalized_catalog_with_key(&korean_catalog_key(), &[&[0xE0; 300]]);
    seed_active_release();

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(
        vec![test_resource(LogicalResource::TextIndex(TextIndexId::new(
            0,
        )))],
        vec![text_install_args_with_kinds(
            Some(0),
            Some(vec![DictKind::Japanese, DictKind::Korean]),
        )],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => {
            assert!(created_resources.is_empty());
        }
        _ => panic!("expected Accepted with an empty created list"),
    }
    assert_eq!(
        relay_job_state(&store),
        JobState::Failed {
            reason: "dictionary relay failed for resource 0".to_owned()
        }
    );
    assert!(dict_relay_call_log_for_test().is_empty());
}

/// (8) Plan 0341: an analyzer-3 text resource relays the korean frames (its own digests)
/// and never touches the ipadic entry.
#[test]
fn dict_relay_analyzer_3_streams_korean_frames_only() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    let ipadic_chunks: [&[u8]; 1] = [&[0xA0; 300]];
    let korean_chunks: [&[u8]; 2] = [&[0xE0; 300], &[0xE1; 200]];
    seed_finalized_catalog_with_key(&relay_catalog_key(), &ipadic_chunks);
    let (raw_digest, raw_len) =
        seed_finalized_catalog_with_key(&korean_catalog_key(), &korean_chunks);
    seed_active_release();

    script_dict_relay_call(scripted_status(DictState::Absent, None));
    for chunk in &korean_chunks {
        script_dict_relay_call(Some(
            Encode!(&Ok::<u64, String>(chunk.len() as u64)).unwrap(),
        ));
    }
    script_dict_relay_call(Some(
        Encode!(&Ok::<DictStatus, String>(DictStatus {
            state: DictState::Finalized,
            digest: Some(raw_digest),
            len: raw_len,
        }))
        .unwrap(),
    ));

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let req = relay_request(
        vec![test_resource(LogicalResource::TextIndex(TextIndexId::new(
            0,
        )))],
        vec![text_install_args(Some(3))],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => {
            assert_eq!(created_resources.len(), 1);
        }
        _ => panic!("expected Accepted"),
    }
    assert_eq!(relay_job_state(&store), JobState::RouterRegistrationPending);
    assert_eq!(
        dict_relay_call_log_for_test(),
        vec![
            DictRelayCall::Status,
            DictRelayCall::Upload {
                bytes: korean_chunks[0].to_vec()
            },
            DictRelayCall::Upload {
                bytes: korean_chunks[1].to_vec()
            },
            DictRelayCall::Finalize,
        ]
    );
}

/// (6) Undecodable text install args fail the relay closed before any call.
#[test]
fn dict_relay_rejects_undecodable_install_args() {
    reset_all_maps();
    reset_dict_relay_script_for_test();
    seed_finalized_catalog(&[&[0xA0; 300]]);
    seed_active_release();

    let (deployment_store, store) = init_and_grant(deployment_issuer());
    // Empty bytes cannot decode as TextCanisterInitArgs.
    let req = relay_request(
        vec![test_resource(LogicalResource::TextIndex(TextIndexId::new(
            0,
        )))],
        vec![vec![]],
    );
    let result = block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req,
        600,
    ))
    .unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            created_resources, ..
        } => {
            assert!(created_resources.is_empty());
        }
        _ => panic!("expected Accepted with an empty created list"),
    }
    assert_eq!(
        relay_job_state(&store),
        JobState::Failed {
            reason: "dictionary relay failed for resource 0".to_owned()
        }
    );
    assert!(dict_relay_call_log_for_test().is_empty());
}

#[test]
fn singleton_router_issuance_rejects_a_second_request_for_the_same_deployment() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(other_issuer());
    let first = test_request(
        test_deployment_id(&other_dep_id()),
        "req-router-1",
        "fp-router-1",
        vec![test_resource(LogicalResource::Router)],
    );
    block_on(accept_envelope_with_caller(
        other_issuer(),
        &store,
        &deployment_store,
        first,
        1,
    ))
    .expect("first Router issuance");

    let second = test_request(
        test_deployment_id(&other_dep_id()),
        "req-router-2",
        "fp-router-2",
        vec![test_resource(LogicalResource::Router)],
    );
    let error = block_on(accept_envelope_with_caller(
        other_issuer(),
        &store,
        &deployment_store,
        second,
        2,
    ))
    .expect_err("a second Router issuance must be rejected");
    assert_eq!(error, ProvisionIngressError::RouterIssuanceInProgress);
}

// === deployment contract ====================================================

#[test]
fn first_issuance_creates_a_self_custody_record() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let req = test_request(
        deployment_id,
        "req-contract",
        "fp-contract",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let owner = req.authorized_caller;
    let result = block_on(accept_envelope_with_caller(
        deployment_issuer(),
        &store,
        &deployment_store,
        req,
        1,
    ));
    assert!(result.is_ok(), "{result:?}");
    let record = store.deployment_record(deployment_id).expect("record");
    assert_eq!(record.owner, owner);
    assert_eq!(record.controller_mode, ControllerMode::SelfCustody);
    assert_eq!(record.operator, None);
    assert_eq!(record.credit_cycles, 0);
}

#[test]
fn managed_issuance_requires_the_service_operator() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let mut req = test_request(
        test_deployment_id(&dep_id()),
        "req-managed-missing",
        "fp-managed-missing",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    req.controller_mode = ControllerMode::Managed;
    assert_eq!(
        block_on(accept_envelope_with_caller(
            deployment_issuer(),
            &store,
            &deployment_store,
            req,
            1,
        )),
        Err(ProvisionIngressError::InvalidState)
    );
    assert!(
        store
            .deployment_record(test_deployment_id(&dep_id()))
            .is_none()
    );
}

#[test]
fn managed_issuance_snapshots_the_operator() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let operator = other_principal();
    store
        .set_service_config(ServiceConfig {
            operator: Some(operator),
            ..ServiceConfig::default()
        })
        .unwrap();
    let mut req = test_request(
        test_deployment_id(&dep_id()),
        "req-managed",
        "fp-managed",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    req.controller_mode = ControllerMode::Managed;
    block_on(accept_envelope_with_caller(
        deployment_issuer(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let record = store
        .deployment_record(test_deployment_id(&dep_id()))
        .expect("record");
    assert_eq!(record.controller_mode, ControllerMode::Managed);
    assert_eq!(record.operator, Some(operator));
}

#[test]
fn a_later_request_cannot_change_the_controller_mode() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let operator = other_principal();
    store
        .set_service_config(ServiceConfig {
            operator: Some(operator),
            ..ServiceConfig::default()
        })
        .unwrap();
    let first = test_request(
        test_deployment_id(&dep_id()),
        "req-self-first",
        "fp-self-first",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        deployment_issuer(),
        &store,
        &deployment_store,
        first,
        1,
    ))
    .unwrap();

    let mut second = test_request(
        test_deployment_id(&dep_id()),
        "req-managed-second",
        "fp-managed-second",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    second.controller_mode = ControllerMode::Managed;
    assert_eq!(
        block_on(accept_envelope_with_caller(
            deployment_issuer(),
            &store,
            &deployment_store,
            second,
            2,
        )),
        Err(ProvisionIngressError::Conflict)
    );
}

#[test]
fn controller_set_is_provision_plus_one_deployment_controller() {
    let provision = pid(1);
    let deployment = pid(2);
    let inference_router = pid(3);
    assert_eq!(
        super::canister_controllers(provision, deployment, None),
        vec![provision, deployment]
    );
    let with_router = super::canister_controllers(provision, deployment, Some(inference_router));
    assert_eq!(with_router, vec![provision, deployment, inference_router]);
    assert_eq!(
        super::canister_controllers(provision, deployment, Some(deployment)),
        vec![provision, deployment],
        "an extra controller equal to the deployment controller is not duplicated"
    );
}

// === service config ========================================================

#[test]
fn only_governance_can_update_service_config_with_increasing_versions() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let config = ServiceConfig {
        version: 2,
        platform_fee_bps: 100,
        reserve_bps: 50,
        ..ServiceConfig::default()
    };
    assert_eq!(
        super::set_service_config_with_caller(other_principal(), &store, config),
        Err(ServiceConfigUpdateError::Unauthorized)
    );
    assert_eq!(
        super::set_service_config_with_caller(gov_principal(), &store, config),
        Ok(config)
    );

    let stale = ServiceConfig {
        version: 2,
        ..ServiceConfig::default()
    };
    assert_eq!(
        super::set_service_config_with_caller(gov_principal(), &store, stale),
        Err(ServiceConfigUpdateError::StaleVersion)
    );

    let invalid = ServiceConfig {
        version: 3,
        platform_fee_bps: 9_000,
        reserve_bps: 1_001,
        ..ServiceConfig::default()
    };
    assert_eq!(
        super::set_service_config_with_caller(gov_principal(), &store, invalid),
        Err(ServiceConfigUpdateError::InvalidConfig)
    );
    assert_eq!(store.service_config().version, 2);
}

#[test]
fn a_job_snapshots_the_service_config_version_it_accepted_under() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    super::set_service_config_with_caller(
        gov_principal(),
        &store,
        ServiceConfig {
            version: 2,
            ..ServiceConfig::default()
        },
    )
    .unwrap();
    let req = test_request(
        test_deployment_id(&dep_id()),
        "req-config-version",
        "fp-config-version",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller(
        deployment_issuer(),
        &store,
        &deployment_store,
        req,
        1,
    ))
    .unwrap();
    let record = store
        .get_by_request(
            &test_request_id("req-config-version"),
            test_deployment_id(&dep_id()),
        )
        .expect("job");
    assert_eq!(record.service_config_version, 2);
}

#[test]
fn funded_admission_rejects_insufficient_cycles() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let required = super::initial_canister_cycles();
    let attached = required - 1;
    let req = test_request(
        deployment_id,
        "req-underfunded",
        "fp-underfunded",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    assert_eq!(
        block_on(accept_envelope_with_caller_and_cycles(
            deployment_issuer(),
            &store,
            &deployment_store,
            req,
            1,
            attached,
            true,
        )),
        Err(ProvisionIngressError::InsufficientCycles {
            required,
            available: attached,
        })
    );
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .expect("record")
            .credit_cycles,
        0,
        "the failed deposit is rolled back before the handler can accept it"
    );
    assert!(
        store
            .get_by_request(&test_request_id("req-underfunded"), deployment_id)
            .is_none()
    );
}

#[test]
fn funded_admission_reserves_one_initial_balance_per_resource() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let required = super::initial_canister_cycles();
    let req = test_request(
        deployment_id,
        "req-funded",
        "fp-funded",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let result = block_on(accept_envelope_with_caller_and_cycles(
        deployment_issuer(),
        &store,
        &deployment_store,
        req,
        1,
        required * 2,
        true,
    ));
    assert!(result.is_ok(), "{result:?}");
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .expect("record")
            .credit_cycles,
        required * 2,
        "no active release creates no canister, so the reserve is returned"
    );
}

#[test]
fn a_funded_replay_keeps_the_attached_prepayment() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let required = super::initial_canister_cycles();
    let req = test_request(
        deployment_id,
        "req-funded-replay",
        "fp-funded-replay",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let first = block_on(accept_envelope_with_caller_and_cycles(
        deployment_issuer(),
        &store,
        &deployment_store,
        req.clone(),
        1,
        required,
        true,
    ));
    assert!(matches!(
        first,
        Ok(ProvisionAcceptResponse::Accepted { .. })
    ));
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .credit_cycles,
        required,
        "no active release refunds the reserve"
    );

    let second = block_on(accept_envelope_with_caller_and_cycles(
        deployment_issuer(),
        &store,
        &deployment_store,
        req,
        2,
        required,
        true,
    ));
    assert!(matches!(second, Ok(ProvisionAcceptResponse::Replay { .. })));
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .credit_cycles,
        required * 2,
        "the first reserve was refunded as no-effect; the replay only adds the new prepayment"
    );
}

#[test]
fn a_completed_replay_does_not_require_a_fresh_balance() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let req = test_request(
        deployment_id,
        "req-completed",
        "fp-completed",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let key = ProvisionJobRequestKey::new(&req.request_id, deployment_id);
    let mut record = super::build_record_from_request(req.clone(), 1, 1);
    record.current_state = JobState::Completed;
    store.put(&key, record);
    store
        .ensure_deployment_record(
            deployment_id,
            req.authorized_caller,
            ControllerMode::SelfCustody,
            None,
        )
        .unwrap();

    let result = block_on(accept_envelope_with_caller_and_cycles(
        deployment_issuer(),
        &store,
        &deployment_store,
        req,
        2,
        0,
        true,
    ));
    assert!(matches!(result, Ok(ProvisionAcceptResponse::Replay { .. })));
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .credit_cycles,
        0
    );
}

#[test]
fn a_created_canister_blocks_the_no_effect_refund() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let req = test_request(
        deployment_id,
        "req-created-no-refund",
        "fp-created-no-refund",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let required = super::initial_canister_cycles();
    let charge = AdmissionCharge {
        cost: required,
        ..AdmissionCharge::default()
    };
    let mut record = super::build_record_from_request(req.clone(), 1, 1);
    record.current_state = JobState::Failed {
        reason: "synthetic".to_owned(),
    };
    record.admission_charge = charge;
    store.insert_with_intent_locks(record, 1).unwrap();
    let key = ProvisionJobRequestKey::new(&req.request_id, deployment_id);
    store.set_resource_canister_id(&key, 0, pid(55));
    store
        .ensure_deployment_record(
            deployment_id,
            req.authorized_caller,
            ControllerMode::SelfCustody,
            None,
        )
        .unwrap();
    store.credit_deployment(deployment_id, required).unwrap();
    super::refund_admission_if_no_canister(&store, &req, charge, true);
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .credit_cycles,
        required,
        "a created canister consumed its initial balance"
    );
}

#[test]
fn owner_can_take_a_managed_deployment_into_self_custody() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(21);
    store
        .ensure_deployment_record(
            deployment_id,
            owner,
            ControllerMode::Managed,
            Some(other_principal()),
        )
        .unwrap();
    let record = block_on(super::take_ownership_with_caller(
        owner,
        &store,
        deployment_id,
    ))
    .expect("handover");
    assert_eq!(record.controller_mode, ControllerMode::SelfCustody);
    assert_eq!(record.operator, None);
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .controller_mode,
        ControllerMode::SelfCustody
    );
}

#[test]
fn only_the_owner_can_take_ownership() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(22);
    store
        .ensure_deployment_record(
            deployment_id,
            owner,
            ControllerMode::Managed,
            Some(other_principal()),
        )
        .unwrap();
    assert_eq!(
        block_on(super::take_ownership_with_caller(
            other_principal(),
            &store,
            deployment_id
        )),
        Err(TakeOwnershipError::Unauthorized)
    );
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .controller_mode,
        ControllerMode::Managed
    );
}

#[test]
fn take_ownership_rejects_unknown_and_already_self_custody() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    assert_eq!(
        block_on(super::take_ownership_with_caller(
            pid(23),
            &store,
            test_deployment_id("dep-missing")
        )),
        Err(TakeOwnershipError::UnknownDeployment)
    );

    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(24);
    store
        .ensure_deployment_record(deployment_id, owner, ControllerMode::SelfCustody, None)
        .unwrap();
    assert_eq!(
        block_on(super::take_ownership_with_caller(
            owner,
            &store,
            deployment_id
        )),
        Err(TakeOwnershipError::AlreadySelfCustody)
    );
}

#[test]
fn deployment_canisters_are_collected_across_jobs() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let req = test_request(
        deployment_id,
        "req-canisters",
        "fp-canisters",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    store
        .ensure_deployment_record(
            deployment_id,
            req.authorized_caller,
            ControllerMode::Managed,
            Some(other_principal()),
        )
        .unwrap();
    store
        .insert_with_intent_locks(super::build_record_from_request(req.clone(), 1, 1), 1)
        .unwrap();
    let key = ProvisionJobRequestKey::new(&req.request_id, deployment_id);
    store.set_resource_canister_id(&key, 0, pid(60));
    assert_eq!(store.deployment_canisters(deployment_id), vec![pid(60)]);

    let mut failed = store.get_by_request_key(&key).unwrap();
    failed.current_state = JobState::Failed {
        reason: "synthetic".to_owned(),
    };
    store.put(&key, failed);
    assert!(
        store.deployment_canisters(deployment_id).is_empty(),
        "a failed job's cleaned-up canister must not block a later handover"
    );
}

fn managed_deployment_with_two_canisters(
    store: &ProvisionJobStore,
    deployment_id: gleaph_graph_kernel::provisioning::DeploymentId,
    owner: Principal,
    operator: Principal,
) -> (ProvisionJobRequestKey, Principal, Principal) {
    super::reset_update_settings_test_state();
    store
        .ensure_deployment_record(
            deployment_id,
            owner,
            ControllerMode::Managed,
            Some(operator),
        )
        .unwrap();
    let req = test_request(
        deployment_id,
        "req-handover",
        "fp-handover",
        vec![
            test_resource(LogicalResource::GraphShard(ShardId::new(0))),
            test_resource(LogicalResource::GraphShard(ShardId::new(1))),
        ],
    );
    store
        .insert_with_intent_locks(super::build_record_from_request(req.clone(), 1, 1), 1)
        .unwrap();
    let key = ProvisionJobRequestKey::new(&req.request_id, deployment_id);
    let first = pid(61);
    let second = pid(62);
    store.set_resource_canister_id(&key, 0, first);
    store.set_resource_canister_id(&key, 1, second);
    (key, first, second)
}

#[test]
fn take_ownership_moves_every_canister_to_the_owner() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(63);
    let operator = other_principal();
    let (_key, first, second) =
        managed_deployment_with_two_canisters(&store, deployment_id, owner, operator);

    let record = block_on(super::take_ownership_with_caller(
        owner,
        &store,
        deployment_id,
    ))
    .expect("handover");
    assert_eq!(record.controller_mode, ControllerMode::SelfCustody);
    assert_eq!(record.operator, None);
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .controller_mode,
        ControllerMode::SelfCustody
    );

    let self_id = super::provision_self_principal();
    let calls = super::update_settings_calls();
    assert_eq!(calls.len(), 2);
    assert_eq!(calls[0], (first, vec![self_id, owner]));
    assert_eq!(calls[1], (second, vec![self_id, owner]));
}

#[test]
fn a_failed_controller_update_rolls_back_and_keeps_managed_mode() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(64);
    let operator = other_principal();
    let (_key, first, second) =
        managed_deployment_with_two_canisters(&store, deployment_id, owner, operator);
    super::fail_update_settings_for(second);

    assert_eq!(
        block_on(super::take_ownership_with_caller(
            owner,
            &store,
            deployment_id
        )),
        Err(TakeOwnershipError::ManagementCallFailed)
    );
    let record = store.deployment_record(deployment_id).unwrap();
    assert_eq!(record.controller_mode, ControllerMode::Managed);
    assert_eq!(record.operator, Some(operator));

    let self_id = super::provision_self_principal();
    let calls = super::update_settings_calls();
    assert_eq!(
        calls,
        vec![
            (first, vec![self_id, owner]),
            (second, vec![self_id, owner]),
            (first, vec![self_id, operator]),
        ],
        "the already-changed canister is returned to the operator"
    );
}

#[test]
fn a_failed_rollback_reports_the_unconfirmed_canisters() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(65);
    let operator = other_principal();
    let (_key, first, second) =
        managed_deployment_with_two_canisters(&store, deployment_id, owner, operator);
    // Call 0: first target succeeds. Call 1: second target fails. Call 2: first rollback fails.
    super::fail_update_settings_on_calls(&[1, 2]);

    assert_eq!(
        block_on(super::take_ownership_with_caller(
            owner,
            &store,
            deployment_id
        )),
        Err(TakeOwnershipError::RollbackFailed {
            canisters: vec![first]
        })
    );
    let record = store.deployment_record(deployment_id).unwrap();
    assert_eq!(record.controller_mode, ControllerMode::Managed);
    assert_eq!(record.operator, Some(operator));
    let self_id = super::provision_self_principal();
    assert!(
        !super::update_settings_calls()
            .iter()
            .any(|(canister, controllers)| *canister == second
                && controllers == &vec![self_id, operator]),
        "the failed target canister is not rolled back as if the update applied"
    );
}

#[test]
fn governance_can_repair_a_managed_deployment_owner() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let old_owner = pid(70);
    let new_owner = pid(71);
    store
        .ensure_deployment_record(
            deployment_id,
            old_owner,
            ControllerMode::Managed,
            Some(other_principal()),
        )
        .unwrap();
    let record =
        super::set_deployment_owner_with_caller(gov_principal(), &store, deployment_id, new_owner)
            .unwrap();
    assert_eq!(record.owner, new_owner);
    assert_eq!(
        store.deployment_record(deployment_id).unwrap().owner,
        new_owner
    );
}

#[test]
fn owner_repair_requires_governance_and_a_managed_record() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    assert_eq!(
        super::set_deployment_owner_with_caller(
            deployment_issuer(),
            &store,
            deployment_id,
            pid(72),
        ),
        Err(DeploymentOwnerUpdateError::Unauthorized)
    );
    store
        .ensure_deployment_record(
            deployment_id,
            pid(73),
            ControllerMode::Managed,
            Some(other_principal()),
        )
        .unwrap();
    assert_eq!(
        super::set_deployment_owner_with_caller(
            gov_principal(),
            &store,
            deployment_id,
            Principal::anonymous(),
        ),
        Err(DeploymentOwnerUpdateError::AnonymousOwner)
    );
    assert_eq!(
        super::set_deployment_owner_with_caller(
            gov_principal(),
            &store,
            test_deployment_id("dep-owner-missing"),
            pid(74),
        ),
        Err(DeploymentOwnerUpdateError::UnknownDeployment)
    );

    let self_id = test_deployment_id("dep-owner-self");
    store
        .ensure_deployment_record(self_id, pid(75), ControllerMode::SelfCustody, None)
        .unwrap();
    assert_eq!(
        super::set_deployment_owner_with_caller(gov_principal(), &store, self_id, pid(76)),
        Err(DeploymentOwnerUpdateError::SelfCustody)
    );
}

fn self_custody_deployment_with_two_canisters(
    store: &ProvisionJobStore,
    deployment_id: gleaph_graph_kernel::provisioning::DeploymentId,
    owner: Principal,
) -> (Principal, Principal) {
    let (_key, first, second) =
        managed_deployment_with_two_canisters(store, deployment_id, owner, other_principal());
    store.mark_self_custody(deployment_id).unwrap();
    (first, second)
}

#[test]
fn governance_transfers_self_custody_with_controller_handover() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(70);
    let new_owner = pid(71);
    let (first, second) = self_custody_deployment_with_two_canisters(&store, deployment_id, owner);

    let record = block_on(super::transfer_self_custody_with_caller(
        gov_principal(),
        &store,
        deployment_id,
        new_owner,
    ))
    .unwrap();
    assert_eq!(record.owner, new_owner);
    assert_eq!(record.controller_mode, ControllerMode::SelfCustody);
    let self_id = super::provision_self_principal();
    assert_eq!(
        super::update_settings_calls(),
        vec![
            (first, vec![self_id, new_owner]),
            (second, vec![self_id, new_owner])
        ]
    );
}

#[test]
fn self_custody_transfer_rejects_non_governance_and_wrong_modes() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(72);
    self_custody_deployment_with_two_canisters(&store, deployment_id, owner);
    assert_eq!(
        block_on(super::transfer_self_custody_with_caller(
            owner,
            &store,
            deployment_id,
            pid(73)
        )),
        Err(SelfCustodyTransferError::Unauthorized)
    );
    assert_eq!(
        block_on(super::transfer_self_custody_with_caller(
            gov_principal(),
            &store,
            deployment_id,
            owner
        )),
        Err(SelfCustodyTransferError::SameOwner)
    );

    let managed_id = test_deployment_id("dep-managed-transfer");
    managed_deployment_with_two_canisters(&store, managed_id, owner, other_principal());
    assert_eq!(
        block_on(super::transfer_self_custody_with_caller(
            gov_principal(),
            &store,
            managed_id,
            pid(74)
        )),
        Err(SelfCustodyTransferError::Managed)
    );
}

#[test]
fn a_failed_self_custody_handover_keeps_the_old_owner() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(75);
    let new_owner = pid(76);
    let (first, second) = self_custody_deployment_with_two_canisters(&store, deployment_id, owner);
    super::fail_update_settings_for(second);

    assert_eq!(
        block_on(super::transfer_self_custody_with_caller(
            gov_principal(),
            &store,
            deployment_id,
            new_owner
        )),
        Err(SelfCustodyTransferError::ManagementCallFailed)
    );
    assert_eq!(store.deployment_record(deployment_id).unwrap().owner, owner);
    let self_id = super::provision_self_principal();
    assert_eq!(
        super::update_settings_calls(),
        vec![
            (first, vec![self_id, new_owner]),
            (second, vec![self_id, new_owner]),
            (first, vec![self_id, owner])
        ]
    );
}

#[test]
fn a_fee_bearing_no_effect_admission_refunds_the_whole_charge() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    store
        .set_service_config(ServiceConfig {
            version: 2,
            platform_fee_bps: 1_000,
            reserve_bps: 500,
            ..ServiceConfig::default()
        })
        .unwrap();
    let cost = super::initial_canister_cycles();
    let fee = cost * 1_000 / 10_000;
    let reserve = cost * 500 / 10_000;
    let total = cost + fee + reserve;
    let req = test_request(
        deployment_id,
        "req-fee-no-effect",
        "fp-fee-no-effect",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    block_on(accept_envelope_with_caller_and_cycles(
        deployment_issuer(),
        &store,
        &deployment_store,
        req,
        1,
        total * 2,
        true,
    ))
    .unwrap();
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .credit_cycles,
        total * 2,
        "the whole charge is returned when no canister was created"
    );
    assert_eq!(
        store.service_ledger(),
        ServiceLedger::default(),
        "fee and reserve are released with the no-effect admission"
    );
    let job = store
        .get_by_request(&test_request_id("req-fee-no-effect"), deployment_id)
        .expect("job");
    assert_eq!(job.admission_charge.cost, cost);
    assert_eq!(job.admission_charge.fee, fee);
    assert_eq!(job.admission_charge.reserve, reserve);
    assert!(job.admission_refunded);
}

#[test]
fn billing_grants_require_the_configured_authority_and_are_idempotent() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    store
        .ensure_deployment_record(deployment_id, pid(70), ControllerMode::SelfCustody, None)
        .unwrap();
    let args = GrantCreditArgs {
        deployment_id,
        amount: 100,
        idempotency_key: [9u8; 32],
    };
    assert_eq!(
        super::grant_credit_with_caller(pid(71), &store, args.clone(), 1),
        Err(GrantCreditError::BillingAuthorityNotConfigured)
    );

    store
        .set_service_config(ServiceConfig {
            version: 2,
            billing_authority: Some(pid(71)),
            ..ServiceConfig::default()
        })
        .unwrap();
    assert_eq!(
        super::grant_credit_with_caller(pid(72), &store, args.clone(), 2),
        Err(GrantCreditError::Unauthorized)
    );
    let first = super::grant_credit_with_caller(pid(71), &store, args.clone(), 3).unwrap();
    assert!(!first.replayed);
    assert_eq!(first.credit_cycles, 100);
    let replay = super::grant_credit_with_caller(pid(71), &store, args.clone(), 4).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.credit_cycles, 100);

    let mut conflicting = args;
    conflicting.amount = 101;
    assert_eq!(
        super::grant_credit_with_caller(pid(71), &store, conflicting, 5),
        Err(GrantCreditError::IdempotencyConflict)
    );
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .credit_cycles,
        100
    );
}

#[test]
fn billing_grants_fail_closed_for_unknown_or_zero() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let authority = pid(73);
    store
        .set_service_config(ServiceConfig {
            version: 2,
            billing_authority: Some(authority),
            ..ServiceConfig::default()
        })
        .unwrap();
    assert_eq!(
        super::grant_credit_with_caller(
            authority,
            &store,
            GrantCreditArgs {
                deployment_id: test_deployment_id("dep-billing-missing"),
                amount: 10,
                idempotency_key: [1u8; 32],
            },
            1,
        ),
        Err(GrantCreditError::UnknownDeployment)
    );
    let deployment_id = test_deployment_id(&dep_id());
    store
        .ensure_deployment_record(deployment_id, pid(74), ControllerMode::SelfCustody, None)
        .unwrap();
    assert_eq!(
        super::grant_credit_with_caller(
            authority,
            &store,
            GrantCreditArgs {
                deployment_id,
                amount: 0,
                idempotency_key: [2u8; 32],
            },
            2,
        ),
        Err(GrantCreditError::AmountZero)
    );
}

#[test]
fn an_owner_can_initialize_a_deployment_before_the_first_job() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_store = DeploymentGrantStore::new();
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(70);
    let args = InitializeDeploymentArgs {
        deployment_id,
        owner,
        controller_mode: ControllerMode::SelfCustody,
    };
    let record =
        super::initialize_deployment_with_caller(owner, &store, &deployment_store, args).unwrap();
    assert_eq!(record.owner, owner);
    assert_eq!(record.credit_cycles, 0);
    assert_eq!(
        super::initialize_deployment_with_caller(owner, &store, &deployment_store, args).unwrap(),
        record,
        "the same owner and mode is idempotent"
    );
    store
        .grant_credit(
            crate::types::BillingGrantKey {
                authority: deployment_issuer(),
                idempotency_key: [9u8; 32],
            },
            deployment_id,
            1_000,
            1,
        )
        .unwrap();
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .credit_cycles,
        1_000
    );
}

#[test]
fn initialization_authorities_are_owner_relay_or_own_deployment() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_store = DeploymentGrantStore::new();
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(71);
    let args = InitializeDeploymentArgs {
        deployment_id,
        owner,
        controller_mode: ControllerMode::SelfCustody,
    };
    assert_eq!(
        super::initialize_deployment_with_caller(pid(72), &store, &deployment_store, args),
        Err(InitializeDeploymentError::Unauthorized)
    );
    // Relay grant holder initializes for an arbitrary owner.
    assert!(
        super::initialize_deployment_with_caller(
            deployment_issuer(),
            &store,
            &deployment_store,
            args
        )
        .is_ok()
    );
}

#[test]
fn initialization_rejects_owner_or_mode_changes() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(73);
    super::initialize_deployment_with_caller(
        owner,
        &store,
        &deployment_store,
        InitializeDeploymentArgs {
            deployment_id,
            owner,
            controller_mode: ControllerMode::SelfCustody,
        },
    )
    .unwrap();
    assert_eq!(
        super::initialize_deployment_with_caller(
            deployment_issuer(),
            &store,
            &deployment_store,
            InitializeDeploymentArgs {
                deployment_id,
                owner: pid(74),
                controller_mode: ControllerMode::SelfCustody,
            }
        ),
        Err(InitializeDeploymentError::OwnerMismatch)
    );
    store
        .set_service_config(ServiceConfig {
            version: 4,
            operator: Some(other_principal()),
            ..ServiceConfig::default()
        })
        .unwrap();
    assert_eq!(
        super::initialize_deployment_with_caller(
            deployment_issuer(),
            &store,
            &deployment_store,
            InitializeDeploymentArgs {
                deployment_id,
                owner,
                controller_mode: ControllerMode::Managed,
            }
        ),
        Err(InitializeDeploymentError::ControllerModeMismatch)
    );
}

#[test]
fn managed_initialization_needs_the_service_operator() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_store = DeploymentGrantStore::new();
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(75);
    let args = InitializeDeploymentArgs {
        deployment_id,
        owner,
        controller_mode: ControllerMode::Managed,
    };
    assert_eq!(
        super::initialize_deployment_with_caller(owner, &store, &deployment_store, args),
        Err(InitializeDeploymentError::MissingOperator)
    );
    store
        .set_service_config(ServiceConfig {
            version: 3,
            operator: Some(other_principal()),
            ..ServiceConfig::default()
        })
        .unwrap();
    let record =
        super::initialize_deployment_with_caller(owner, &store, &deployment_store, args).unwrap();
    assert_eq!(record.controller_mode, ControllerMode::Managed);
    assert_eq!(record.operator, Some(other_principal()));
}

#[test]
fn a_pre_registered_deployment_can_be_funded_and_issued() {
    reset_all_maps();
    let (deployment_store, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let owner = pid(76);
    super::initialize_deployment_with_caller(
        owner,
        &store,
        &deployment_store,
        InitializeDeploymentArgs {
            deployment_id,
            owner,
            controller_mode: ControllerMode::SelfCustody,
        },
    )
    .unwrap();
    let required = super::initial_canister_cycles();
    store
        .grant_credit(
            crate::types::BillingGrantKey {
                authority: deployment_issuer(),
                idempotency_key: [10u8; 32],
            },
            deployment_id,
            required * 2,
            1,
        )
        .unwrap();
    let mut req = test_request(
        deployment_id,
        "req-preregistered",
        "fp-preregistered",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    req.authorized_caller = owner;
    let result = block_on(accept_envelope_with_caller_and_cycles(
        deployment_issuer(),
        &store,
        &deployment_store,
        req,
        2,
        0,
        true,
    ));
    assert!(matches!(
        result,
        Ok(ProvisionAcceptResponse::Accepted { .. })
    ));
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .credit_cycles,
        required * 2,
        "the no-effect admission returns the reserve, leaving the grant intact"
    );
}

#[test]
fn reverse_credit_requires_the_configured_billing_authority() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let billing = pid(77);
    store
        .set_service_config(ServiceConfig {
            version: 2,
            billing_authority: Some(billing),
            ..ServiceConfig::default()
        })
        .unwrap();
    store
        .ensure_deployment_record(deployment_id, pid(78), ControllerMode::SelfCustody, None)
        .unwrap();
    let key = crate::types::BillingGrantKey {
        authority: billing,
        idempotency_key: [11u8; 32],
    };
    store.grant_credit(key, deployment_id, 1_000, 1).unwrap();
    assert_eq!(
        super::reverse_credit_with_caller(
            other_principal(),
            &store,
            ReverseCreditArgs {
                grant_authority: billing,
                idempotency_key: [11u8; 32],
            },
            2,
        ),
        Err(ReverseCreditError::Unauthorized)
    );
    let result = super::reverse_credit_with_caller(
        billing,
        &store,
        ReverseCreditArgs {
            grant_authority: billing,
            idempotency_key: [11u8; 32],
        },
        3,
    )
    .unwrap();
    assert_eq!(result.amount, 1_000);
    assert_eq!(result.credit_cycles, 0);
}

#[test]
fn a_rotated_billing_authority_can_reverse_an_older_grant() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let deployment_id = test_deployment_id(&dep_id());
    let old_authority = pid(79);
    let new_authority = pid(80);
    store
        .ensure_deployment_record(deployment_id, pid(81), ControllerMode::SelfCustody, None)
        .unwrap();
    store
        .grant_credit(
            crate::types::BillingGrantKey {
                authority: old_authority,
                idempotency_key: [12u8; 32],
            },
            deployment_id,
            1_000,
            1,
        )
        .unwrap();
    store
        .set_service_config(ServiceConfig {
            version: 2,
            billing_authority: Some(new_authority),
            ..ServiceConfig::default()
        })
        .unwrap();
    super::reverse_credit_with_caller(
        new_authority,
        &store,
        ReverseCreditArgs {
            grant_authority: old_authority,
            idempotency_key: [12u8; 32],
        },
        2,
    )
    .unwrap();
    assert_eq!(
        store
            .deployment_record(deployment_id)
            .unwrap()
            .credit_cycles,
        0
    );
}

#[test]
fn reverse_credit_rejects_an_anonymous_grant_authority() {
    reset_all_maps();
    let (_, store) = init_and_grant(deployment_issuer());
    let billing = pid(82);
    store
        .set_service_config(ServiceConfig {
            version: 2,
            billing_authority: Some(billing),
            ..ServiceConfig::default()
        })
        .unwrap();
    assert_eq!(
        super::reverse_credit_with_caller(
            billing,
            &store,
            ReverseCreditArgs {
                grant_authority: Principal::anonymous(),
                idempotency_key: [13u8; 32],
            },
            1,
        ),
        Err(ReverseCreditError::AnonymousGrantAuthority)
    );
}

#[test]
fn admission_quote_matches_the_current_configuration() {
    reset_all_maps();
    let store = ProvisionJobStore::new();
    store
        .set_service_config(ServiceConfig {
            version: 4,
            platform_fee_bps: 1_000,
            reserve_bps: 500,
            ..ServiceConfig::default()
        })
        .unwrap();
    let quote = super::quote_admission(2).expect("quote");
    let cost = super::initial_canister_cycles() * 2;
    assert_eq!(quote.resource_count, 2);
    assert_eq!(quote.cost, cost);
    assert_eq!(quote.fee, cost * 1_000 / 10_000);
    assert_eq!(quote.reserve, cost * 500 / 10_000);
    assert_eq!(quote.total, quote.cost + quote.fee + quote.reserve);
    assert_eq!(quote.service_config_version, 4);
}
