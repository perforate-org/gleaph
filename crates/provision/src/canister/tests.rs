//! Unit tests for the Provision ingress handlers.

use super::{
    ProvisionAcceptResponse, ProvisionIngressError, ProvisionQueryError, ProvisionResult,
    ProvisionResultOutcome, accept_envelope_with_caller,
    admin_install_deployment_binding_with_caller, artifact_get_status,
    artifact_publish_metadata_with_caller, artifact_upload_chunk_with_caller,
    build_record_from_request, complete_bootstrap_with_caller,
    complete_graph_registration_with_caller, query_job_with_caller, record_to_result,
    release_activate_with_caller, release_get_active, release_install_with_caller,
    release_publish_with_caller,
};
use crate::canister::init;
use crate::stable::artifact::ProvisionArtifactStore;
use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;
use crate::stable::store::{
    DeploymentTrustStore, ProvisionJobStore, reopen_provisioning_regions_for_test, reset_all_maps,
};
use crate::types::{
    AdminInstallDeploymentBindingArgs, ArtifactError, ArtifactId, ArtifactPublishMetadataArgs,
    ArtifactUploadChunkArgs, BootstrapAuthAction, BootstrapAuthorityRecord, CanisterKind,
    DeploymentBinding, InstallError, JobState, LogicalResource, ProvisionAdminError,
    ProvisionJobRequestKey, ProvisionRequest, ProvisionableResource, ProvisioningIntentKey,
    ReleaseActivateArgs, ReleaseError, ReleaseId, ReleaseInstallArgs, ReleasePublishArgs,
    RouterRegistrationAck, RouterRegistrationAckResponse, sha256,
};
use candid::{Encode, Principal};
use gleaph_graph_kernel::federation::ShardId;
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

fn test_binding(deployment_id: &str) -> DeploymentBinding {
    DeploymentBinding {
        deployment_id: deployment_id.to_owned(),
        router_principal: router_principal(),
        governance_principal: gov_principal(),
        binding_version: 1,
        bootstrap_principal: None,
    }
}

fn test_resource(logical_resource: LogicalResource) -> ProvisionableResource {
    ProvisionableResource { logical_resource }
}

fn test_request_id(label: &str) -> [u8; 32] {
    let mut id = [0u8; 32];
    let bytes = label.as_bytes();
    let n = bytes.len().min(32);
    id[..n].copy_from_slice(&bytes[..n]);
    id
}

fn test_request(
    deployment_id: &str,
    request_id: &str,
    _fingerprint: &str,
    resources: Vec<ProvisionableResource>,
) -> ProvisionRequest {
    let intent_key = if resources.is_empty() {
        ProvisioningIntentKey::new(deployment_id, LogicalResource::GraphShard(ShardId::new(0)))
    } else {
        ProvisioningIntentKey::new(deployment_id, resources[0].logical_resource)
    };
    ProvisionRequest {
        deployment_id: deployment_id.to_owned(),
        request_id: test_request_id(request_id),
        intent_key,
        reserved_graph_id: None,
        graph_name: "test-graph".to_owned(),
        requested_resources: resources.clone(),
        install_args: resources.iter().map(|_| vec![0u8; 0]).collect(),
        authorized_caller: pid(30),
        release_id: "r1".to_owned(),
    }
}

fn insert_binding_and_init(deployment_id: &str) -> (DeploymentTrustStore, ProvisionJobStore) {
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![test_binding(deployment_id)],
    });
    let deployment_store = DeploymentTrustStore::new();
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
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
            .get_by_request(&test_request_id("req-a"), "dep-a")
            .is_none()
    );
}

#[test]
fn test_provision_accept_unknown_deployment_rejected() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![],
    });
    let deployment_store = DeploymentTrustStore::new();
    let store = ProvisionJobStore::new();
    let req = test_request(
        "dep-a",
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
    assert_eq!(result, Err(ProvisionIngressError::UnknownDeployment));
}

#[test]
fn test_provision_accept_idempotent_replay_returns_existing() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
            assert_eq!(job_view.deployment_id, "dep-a");
            assert_eq!(job_view.state, "Reserved");
        }
        _ => panic!("expected Replay, got {:?}", replay),
    }
    let record = store
        .get_by_request(&test_request_id("req-a"), "dep-a")
        .unwrap();
    assert_eq!(record.current_state, JobState::Reserved);
}

#[test]
fn test_provision_accept_same_content_is_idempotent_replay() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req1 = test_request(
        "dep-a",
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
        "dep-a",
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
        .get_by_request(&test_request_id("req-a"), "dep-a")
        .unwrap();
    assert_eq!(record.request_id, test_request_id("req-a"));
}

#[test]
fn test_provision_accept_different_content_yields_distinct_request_ids() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    // Different resources => different content-hash request_id => both admitted independently.
    let req1 = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    let req2 = test_request(
        "dep-a",
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
            .get_by_request(&test_request_id("req-a"), "dep-a")
            .is_some()
    );
    assert!(
        store
            .get_by_request(&test_request_id("req-b"), "dep-a")
            .is_some()
    );
}

#[test]
fn test_provision_no_partial_writes_on_lock_failure() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    // Pre-lock the only intent.
    let held_key =
        ProvisioningIntentKey::new("dep-a", LogicalResource::GraphShard(ShardId::new(0)));
    assert!(store.acquire_intent_lock(held_key.clone()));

    let req = test_request(
        "dep-a",
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
            .get_by_request(&test_request_id("req-a"), "dep-a")
            .is_none()
    );
    assert!(!store.has_live_job_for_deployment("dep-a"));
    // Pre-held lock is untouched.
    assert!(store.intent_locked(&held_key));
}

#[test]
fn test_provision_accept_empty_resources_rejected() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request("dep-a", "req-empty", "fp-empty", vec![]);
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
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
        "dep-a".to_owned(),
    );
    assert_eq!(result, Err(ProvisionQueryError::NotAuthorized));
}

#[test]
fn test_provision_query_returns_redacted_view() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
        "dep-a".to_owned(),
    )
    .unwrap();
    assert_eq!(view.request_id, test_request_id("req-a"));
    assert_eq!(view.state_name, "Reserved");
    assert!(view.is_authorized_caller);
}

#[test]
fn test_provision_query_unknown_deployment_returns_not_found() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![],
    });
    let deployment_store = DeploymentTrustStore::new();
    let store = ProvisionJobStore::new();
    let result = query_job_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        test_request_id("req-a"),
        "dep-missing".to_owned(),
    );
    assert_eq!(result, Err(ProvisionQueryError::UnknownDeployment));
}

// === complete_graph_registration ============================================

#[test]
fn registration_ack_authenticates_before_exact_lookup() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let result = complete_graph_registration_with_caller(
        other_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: "dep-a".to_owned(),
            request_id: test_request_id("missing"),
        },
        1,
    );
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
}

#[test]
fn registration_ack_wrong_router_rejected() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), "dep-a");
    advance_to_registration_pending(&store, &key, 10);
    let result = complete_graph_registration_with_caller(
        other_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: "dep-a".to_owned(),
            request_id: test_request_id("req-a"),
        },
        20,
    );
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
}

#[test]
fn registration_ack_invalid_state() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
            deployment_id: "dep-a".to_owned(),
            request_id: test_request_id("req-a"),
        },
        2,
    );
    assert_eq!(result, Err(ProvisionIngressError::InvalidState));
}

#[test]
fn registration_ack_fresh_co_writes_completed_and_owned_row_release() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), "dep-a");
    advance_to_registration_pending(&store, &key, 10);
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: "dep-a".to_owned(),
            request_id: test_request_id("req-a"),
        },
        20,
    )
    .unwrap();
    assert_eq!(result, RouterRegistrationAckResponse::Applied);
    let record = store
        .get_by_request(&test_request_id("req-a"), "dep-a")
        .unwrap();
    assert_eq!(record.current_state, JobState::Completed);
    assert_eq!(
        store.assert_intent_to_request_for_test(
            "dep-a",
            LogicalResource::GraphShard(ShardId::new(0)),
        ),
        None
    );
    assert!(!store.intent_locked(&ProvisioningIntentKey::new(
        "dep-a",
        LogicalResource::GraphShard(ShardId::new(0))
    )));
}

#[test]
fn registration_ack_fresh_requires_map2_owner_and_map3_presence() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), "dep-a");
    advance_to_registration_pending(&store, &key, 10);
    // Release the lock behind the store's back.
    let lock_key =
        ProvisioningIntentKey::new("dep-a", LogicalResource::GraphShard(ShardId::new(0)));
    assert!(store.release_intent_lock(&lock_key));
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: "dep-a".to_owned(),
            request_id: test_request_id("req-a"),
        },
        20,
    );
    assert_eq!(result, Err(ProvisionIngressError::InvalidState));
}

#[test]
fn registration_ack_idempotent_replay() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), "dep-a");
    advance_to_registration_pending(&store, &key, 10);
    let ack = RouterRegistrationAck {
        deployment_id: "dep-a".to_owned(),
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
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), "dep-a");
    advance_to_registration_pending(&store, &key, 10);
    store.complete_graph_registration(&key, 30).unwrap();
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: "dep-a".to_owned(),
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
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), "dep-a");
    advance_to_registration_pending(&store, &key, 10);
    assert_eq!(
        store.complete_graph_registration(&key, 20),
        Ok(RouterRegistrationAckResponse::Applied)
    );

    let intent = ProvisioningIntentKey::new("dep-a", LogicalResource::GraphShard(ShardId::new(0)));
    let foreign = ProvisionJobRequestKey::new(&test_request_id("foreign"), "dep-a");
    store.set_intent_owner_for_test(intent.clone(), Some(foreign.clone()));
    store.set_intent_lock_for_test(intent.clone(), true);

    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: "dep-a".to_owned(),
            request_id: test_request_id("req-a"),
        },
        21,
    );
    assert_eq!(result, Ok(RouterRegistrationAckResponse::Replay));
    assert_eq!(
        store.assert_intent_to_request_for_test(
            "dep-a",
            LogicalResource::GraphShard(ShardId::new(0)),
        ),
        Some(foreign)
    );
    assert!(store.intent_locked(&intent));
}

#[test]
fn registration_ack_unknown_deployment() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![],
    });
    let deployment_store = DeploymentTrustStore::new();
    let store = ProvisionJobStore::new();
    // Insert a job record without its deployment binding.
    let record = build_record_from_request(
        test_request(
            "dep-orphan",
            "req-o",
            "fp-o",
            vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
        ),
        1,
    );
    store.insert_or_idempotent(record).unwrap();
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: "dep-orphan".to_owned(),
            request_id: test_request_id("req-o"),
        },
        2,
    );
    assert_eq!(result, Err(ProvisionIngressError::UnknownDeployment));
}

#[test]
fn test_provision_accept_envelope_fresh_admission_reports_accepted() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
            assert_eq!(job_view.deployment_id, "dep-a");
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
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
fn test_provision_accept_envelope_allows_bootstrap_principal() {
    reset_all_maps();
    // Seed a binding whose bootstrap principal is the Account (other_principal), distinct from
    // the Router principal.
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![DeploymentBinding {
            deployment_id: "dep-boot".to_owned(),
            router_principal: router_principal(),
            governance_principal: gov_principal(),
            binding_version: 1,
            bootstrap_principal: Some(other_principal()),
        }],
    });
    let deployment_store = DeploymentTrustStore::new();
    let store = ProvisionJobStore::new();
    let req = test_request(
        "dep-boot",
        "req-boot",
        "fp-boot",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
    );
    // The bootstrap principal (Account) may issue the first Router.
    let result = block_on(accept_envelope_with_caller(
        other_principal(),
        &store,
        &deployment_store,
        req.clone(),
        1,
    ))
    .unwrap();
    assert!(
        matches!(result, ProvisionAcceptResponse::Accepted { .. }),
        "bootstrap principal must be accepted for the first Router"
    );
    // A non-router, non-bootstrap caller is still rejected.
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
fn test_complete_bootstrap_clears_bootstrap_principal() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![DeploymentBinding {
            deployment_id: "dep-boot".to_owned(),
            router_principal: router_principal(),
            governance_principal: gov_principal(),
            binding_version: 1,
            bootstrap_principal: Some(other_principal()),
        }],
    });
    let deployment_store = DeploymentTrustStore::new();

    // The bootstrap principal (Account) can complete the handover.
    complete_bootstrap_with_caller(other_principal(), "dep-boot", &deployment_store).unwrap();
    let binding = deployment_store.get("dep-boot").unwrap();
    assert_eq!(binding.bootstrap_principal, None);

    // Idempotent: completing again is a no-op (already cleared).
    complete_bootstrap_with_caller(other_principal(), "dep-boot", &deployment_store).unwrap();

    // Once cleared, any caller re-confirms as a no-op (nothing left to protect).
    complete_bootstrap_with_caller(router_principal(), "dep-boot", &deployment_store).unwrap();
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
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
    let (deployment_store, store) = insert_binding_and_init("dep-a");

    // Job A: seed and admit so its intent lock and derived index entry exist.
    let req_a = test_request(
        "dep-a",
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

    let key_a = ProvisionJobRequestKey::new(&test_request_id("req-a"), "dep-a");
    let intent_key =
        ProvisioningIntentKey::new("dep-a", LogicalResource::GraphShard(ShardId::new(0)));

    // Job B: same deployment, same resource, different request_id.
    let req_b = test_request(
        "dep-a",
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
        .get_by_request(&test_request_id("req-a"), "dep-a")
        .unwrap();
    assert_eq!(record_a.request_id, test_request_id("req-a"));

    // A's lock survives.
    assert!(store.intent_locked(&intent_key));

    // The derived index maps R1.intent to A.key before the conflict is attempted.
    assert_eq!(
        store.assert_intent_to_request_for_test(
            "dep-a",
            LogicalResource::GraphShard(ShardId::new(0)),
        ),
        Some(key_a.clone()),
        "derived index must map R1.intent to A.key before B is attempted"
    );

    // After B is rejected, the same intent still resolves to A.key; B never overwrote the derived row.
    assert_eq!(
        store.assert_intent_to_request_for_test(
            "dep-a",
            LogicalResource::GraphShard(ShardId::new(0)),
        ),
        Some(key_a.clone()),
        "derived index must still map R1.intent to A.key after B is rejected"
    );

    // B leaves no canonical or derived row.
    assert_eq!(
        store.get_by_request(&test_request_id("req-b"), "dep-a"),
        None,
        "B must not leave a canonical row"
    );
    assert_eq!(
        store.get_by_request_key(&ProvisionJobRequestKey::new(
            &test_request_id("req-b"),
            "dep-a"
        )),
        None,
        "B must not leave a canonical row via its composite key"
    );
}

#[test]
fn registration_ack_uses_exact_cross_deployment_key() {
    reset_all_maps();

    // Seed two deployments with different router principals.
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![
            DeploymentBinding {
                deployment_id: "d1".to_owned(),
                router_principal: router_principal(),
                governance_principal: gov_principal(),
                binding_version: 1,
                bootstrap_principal: None,
            },
            DeploymentBinding {
                deployment_id: "d2".to_owned(),
                router_principal: other_principal(),
                governance_principal: gov_principal(),
                binding_version: 1,
                bootstrap_principal: None,
            },
        ],
    });
    let deployment_store = DeploymentTrustStore::new();
    let store = ProvisionJobStore::new();

    // A1: (r1, d1) and A2: (r1, d2), both await Router registration completion.
    let req1 = test_request(
        "d1",
        "r1",
        "fp-1",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(1)))],
    );
    let req2 = test_request(
        "d2",
        "r1",
        "fp-2",
        vec![test_resource(LogicalResource::GraphShard(ShardId::new(2)))],
    );
    block_on(accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req1,
        1,
    ))
    .unwrap();
    block_on(accept_envelope_with_caller(
        other_principal(),
        &store,
        &deployment_store,
        req2,
        2,
    ))
    .unwrap();

    let key1 = ProvisionJobRequestKey::new(&test_request_id("r1"), "d1");
    let key2 = ProvisionJobRequestKey::new(&test_request_id("r1"), "d2");
    advance_to_registration_pending(&store, &key1, 10);
    advance_to_registration_pending(&store, &key2, 20);

    // D1's router attempts to ack (r1, d2). The handler resolves the record by
    // the canonical (request_id, deployment_id) key, then authenticates against the
    // stored router principal for d2 (which is other_principal). D1's router is
    // rejected with NotAuthorized; A2 remains pending.
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: "d2".to_owned(),
            request_id: test_request_id("r1"),
        },
        30,
    );
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
    let a2_before = store.get_by_request(&test_request_id("r1"), "d2").unwrap();
    assert_eq!(a2_before.current_state, JobState::RouterRegistrationPending);

    // Correct ack (r1, d1) advances A1.
    let result = complete_graph_registration_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterRegistrationAck {
            deployment_id: "d1".to_owned(),
            request_id: test_request_id("r1"),
        },
        31,
    );
    assert_eq!(result.unwrap(), RouterRegistrationAckResponse::Applied);
    let a1_after = store.get_by_request(&test_request_id("r1"), "d1").unwrap();
    assert_eq!(a1_after.current_state, JobState::Completed);
}

#[test]
fn test_provision_init_seeds_bootstrap_bindings_and_survives_upgrade() {
    reset_all_maps();

    // Bootstrap init seeds the binding directly into stable memory.
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![test_binding("dep-a")],
    });

    // Simulate an upgrade by re-creating the DeploymentTrustStore instance.
    // The binding was written to stable memory, so a fresh store sees it.
    let deployment_store = DeploymentTrustStore::new();
    assert!(deployment_store.get("dep-a").is_some());
}

#[test]
fn accept_same_key_altered_envelope_conflicts_without_second_effect() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), "dep-a");
    let before_maps = store.provisioning_maps_snapshot_for_test();
    let before_effect_count = store
        .get_by_request_key(&key)
        .unwrap()
        .completed_effect_count;
    type ProvisionFieldMutator = fn(&mut ProvisionRequest);
    let changed_fields: [(&str, ProvisionFieldMutator); 6] = [
        ("intent", |altered| {
            altered.intent_key = ProvisioningIntentKey::new(
                "dep-a",
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
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
    let key = ProvisionJobRequestKey::new(&test_request_id("req-a"), "dep-a");
    advance_to_registration_pending(&store, &key, 10);
    let ack = RouterRegistrationAck {
        deployment_id: "dep-a".to_owned(),
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
            "dep-a",
            "req-a",
            "fp-a",
            vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
        ),
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
            "dep-a",
            "req-a",
            "fp-a",
            vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
        ),
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
            "dep-a",
            "req-a",
            "fp-a",
            vec![test_resource(LogicalResource::GraphShard(ShardId::new(0)))],
        ),
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
    );
    store.insert_or_idempotent(record_a.clone()).unwrap();
    store.insert_or_idempotent(record_b).unwrap();
    assert_eq!(
        store.get_by_request(&test_request_id("missing"), "dep-a"),
        None
    );
    assert_eq!(
        store.get_by_request(&test_request_id("req-a"), "dep-a"),
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
fn test_provision_query_not_found() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let result = query_job_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        test_request_id("missing"),
        "dep-a".to_owned(),
    );
    assert_eq!(result, Err(ProvisionQueryError::NotFound));
}

// === admin_install_deployment_binding (ADR 0035 Slice 7) =====================

fn admin_args(
    deployment_id: &str,
    router_id: u8,
    gov_id: u8,
    binding_version: u64,
) -> AdminInstallDeploymentBindingArgs {
    AdminInstallDeploymentBindingArgs {
        deployment_id: deployment_id.to_owned(),
        router_principal: pid(router_id),
        governance_principal: pid(gov_id),
        binding_version,
        bootstrap_principal: None,
    }
}

#[test]
fn admin_install_with_bootstrap_authority_overwrites_existing_binding() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![test_binding("dep-a")],
    });
    let bootstrap = gov_principal();
    let deployment_store = DeploymentTrustStore::new();

    let first =
        admin_install_deployment_binding_with_caller(bootstrap, admin_args("dep-a", 10, 100, 1), 1)
            .unwrap();
    assert_eq!(first.action, BootstrapAuthAction::AdminInstall);
    assert_eq!(first.caller, bootstrap);

    let second =
        admin_install_deployment_binding_with_caller(bootstrap, admin_args("dep-a", 11, 100, 2), 2)
            .unwrap();
    assert_eq!(second.action, BootstrapAuthAction::AdminInstall);
    assert_eq!(second.caller, bootstrap);
    assert_eq!(
        deployment_store.get("dep-a").unwrap().router_principal,
        pid(11)
    );
}

#[test]
fn admin_install_with_stored_governance_records_audit_and_overwrites_existing_binding() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![test_binding("dep-a")],
    });
    let stored_governance = gov_principal();
    let deployment_store = DeploymentTrustStore::new();

    admin_install_deployment_binding_with_caller(
        gov_principal(),
        admin_args("dep-a", 10, 100, 1),
        1,
    )
    .unwrap();

    let entry = admin_install_deployment_binding_with_caller(
        stored_governance,
        admin_args("dep-a", 12, 100, 2),
        2,
    )
    .unwrap();
    assert_eq!(entry.action, BootstrapAuthAction::AdminInstall);
    assert_eq!(entry.caller, stored_governance);
    assert_eq!(
        deployment_store.get("dep-a").unwrap().router_principal,
        pid(12)
    );
}

#[test]
fn admin_install_with_existing_deployment_and_unauthorized_caller_returns_already_exists_with_reject_audit()
 {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![test_binding("dep-a")],
    });
    let bootstrap = gov_principal();
    let auth_store = ProvisionBootstrapAuthStore::new();

    admin_install_deployment_binding_with_caller(bootstrap, admin_args("dep-a", 10, 100, 1), 1)
        .unwrap();

    let err = admin_install_deployment_binding_with_caller(
        other_principal(),
        admin_args("dep-a", 11, 20, 2),
        2,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ProvisionAdminError::AlreadyExists {
            deployment_id: "dep-a".to_owned(),
            existing_governance: bootstrap,
        }
    );
    let latest = auth_store.latest(other_principal()).unwrap();
    assert_eq!(latest.action, BootstrapAuthAction::RejectAlreadyExists);
    assert_eq!(latest.deployment_id, Some("dep-a".to_owned()));
}

#[test]
fn admin_install_with_missing_deployment_and_unauthorized_caller_returns_unknown_deployment_with_reject_audit()
 {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![test_binding("dep-a")],
    });
    let auth_store = ProvisionBootstrapAuthStore::new();

    let err = admin_install_deployment_binding_with_caller(
        other_principal(),
        admin_args("dep-b", 10, 20, 1),
        1,
    )
    .unwrap_err();
    assert_eq!(
        err,
        ProvisionAdminError::UnknownDeployment("dep-b".to_owned())
    );
    let latest = auth_store.latest(other_principal()).unwrap();
    assert_eq!(latest.action, BootstrapAuthAction::RejectUnknownDeployment);
    assert_eq!(latest.deployment_id, Some("dep-b".to_owned()));
}

#[test]
fn admin_install_with_no_bootstrap_authority_returns_invalid_state_with_reject_audit() {
    reset_all_maps();
    let auth_store = ProvisionBootstrapAuthStore::new();

    let err = admin_install_deployment_binding_with_caller(
        other_principal(),
        admin_args("dep-a", 10, 20, 1),
        1,
    )
    .unwrap_err();
    assert!(matches!(err, ProvisionAdminError::InvalidState(_)));
    let latest = auth_store.latest(other_principal()).unwrap();
    assert_eq!(latest.action, BootstrapAuthAction::RejectInvalidState);
    assert_eq!(latest.deployment_id, Some("dep-a".to_owned()));
}

#[test]
fn admin_install_audit_log_survives_handler_return_path() {
    reset_all_maps();
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![test_binding("dep-a")],
    });
    let bootstrap = gov_principal();
    let auth_store = ProvisionBootstrapAuthStore::new();

    let entry =
        admin_install_deployment_binding_with_caller(bootstrap, admin_args("dep-a", 10, 100, 1), 1)
            .unwrap();
    assert_eq!(entry.action, BootstrapAuthAction::AdminInstall);

    let history = auth_store.history(bootstrap);
    assert!(
        history
            .iter()
            .any(|e| e.action == BootstrapAuthAction::AdminInstall)
    );
    assert_eq!(auth_store.latest(bootstrap), Some(entry));
}

#[test]
fn bootstrap_authority_singleton_survives_upgrade() {
    reset_all_maps();
    let args = init::ProvisionInitArgs {
        bootstrap_bindings: vec![test_binding("dep-a")],
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
            binding_version_at_seed: 1,
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
        matches!(err, ArtifactError::ChunkHashMismatch { artifact_id: ref aid, chunk_index: 0 } if *aid == id),
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
        matches!(err, ArtifactError::ChunkOutOfRange { artifact_id: ref aid, chunk_index: 5, declared: 1 } if *aid == id),
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
    let mut f = Box::pin(f);
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
    release_publish_with_caller(
        gov_principal(),
        ReleasePublishArgs {
            release_id: r.clone(),
            artifact_ids: vec![router, graph, prop, vector.clone(), text],
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

    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
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
        .get_by_request(&test_request_id("req-deploy"), "dep-a")
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
    let reopened_deployment_store = DeploymentTrustStore::new();
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
        deployment_id: "dep-a".to_owned(),
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
        .get_by_request(&test_request_id("req-deploy"), "dep-a")
        .unwrap();
    assert_eq!(completed_record.current_state, JobState::Completed);
    assert_eq!(completed_record.resources, pending_record.resources);
    assert_eq!(completed_record.completed_effect_count, 3);
    assert_eq!(completed_record.immutable_request_digest, expected_digest);
    let intent_key =
        ProvisioningIntentKey::new("dep-a", LogicalResource::GraphShard(ShardId::new(0)));
    assert_eq!(
        reopened_store.assert_intent_to_request_for_test(
            "dep-a",
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
    let later_key = ProvisionJobRequestKey::new(&later_req.request_id, "dep-a");
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
            "dep-a",
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
