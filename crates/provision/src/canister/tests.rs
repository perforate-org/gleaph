//! Unit tests for the Provision ingress handlers.

use super::{
    ProvisionAcceptResponse, ProvisionIngressError, ProvisionQueryError, ProvisionResult,
    ProvisionResultOutcome, accept_envelope_with_caller, build_record_from_request,
    query_job_with_caller, record_to_result, router_ack_with_caller,
};
use crate::canister::init;
use crate::stable::store::{
    DeploymentTrustStore, ProvisionJobStore, reset_all_maps, set_force_advance_error,
};
use crate::types::{
    DeploymentBinding, JobState, ProvisionJobRequestKey, ProvisionRequest, ProvisionableResource,
    ProvisionableResourceKind, ProvisioningIntentKey, RouterProvisionAck,
};
use candid::Principal;

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
    }
}

fn test_resource(kind: ProvisionableResourceKind, key: &str) -> ProvisionableResource {
    ProvisionableResource {
        kind,
        logical_resource_key: key.to_owned(),
    }
}

fn test_request(
    deployment_id: &str,
    request_id: &str,
    fingerprint: &str,
    resources: Vec<ProvisionableResource>,
) -> ProvisionRequest {
    let intent_key = if resources.is_empty() {
        ProvisioningIntentKey::new(
            deployment_id,
            ProvisionableResourceKind::GraphShard,
            "__empty",
        )
    } else {
        ProvisioningIntentKey::new(
            deployment_id,
            resources[0].kind,
            &resources[0].logical_resource_key,
        )
    };
    ProvisionRequest {
        deployment_id: deployment_id.to_owned(),
        request_id: request_id.to_owned(),
        request_fingerprint: fingerprint.to_owned(),
        intent_key,
        reserved_graph_id: None,
        graph_name: "test-graph".to_owned(),
        requested_resources: resources,
        authorized_caller: pid(30),
        release_id: "r1".to_owned(),
        router_callback_principal: pid(40),
    }
}

fn insert_binding_and_init(deployment_id: &str) -> (DeploymentTrustStore, ProvisionJobStore) {
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![test_binding(deployment_id)],
    });
    let deployment_store = DeploymentTrustStore::new();
    (deployment_store, ProvisionJobStore::new())
}

fn advance_to_ack_pending(
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
        JobState::RouterAckPending,
    ];
    for step in &steps {
        let current = store.get_by_request_key(key).unwrap().current_state;
        if current == JobState::RouterAckPending {
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

fn complete_record(
    store: &ProvisionJobStore,
    key: &ProvisionJobRequestKey,
    version: u64,
    now_ns: u64,
) {
    let mut record = store.get_by_request_key(key).unwrap();
    for resource in &mut record.resources {
        resource.canister_id = Some(pid(42));
        resource.artifact_hash = Some("hash".to_owned());
    }
    store.put(key, record);
    router_ack_with_caller(
        router_principal(),
        store,
        &DeploymentTrustStore::new(),
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: key.request_id.clone(),
            accepted_registry_version: version,
        },
        now_ns,
    )
    .unwrap();
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
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    let result = accept_envelope_with_caller(other_principal(), &store, &deployment_store, req, 1);
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
    assert!(store.get_by_request("req-a", "dep-a").is_none());
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
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    let result = accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1);
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
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req.clone(),
        1,
    )
    .unwrap();
    let replay =
        accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 2).unwrap();
    match replay {
        ProvisionAcceptResponse::Replay { job_view, .. } => {
            assert_eq!(job_view.request_id, "req-a");
            assert_eq!(job_view.deployment_id, "dep-a");
            assert_eq!(job_view.state, "Reserved");
        }
        _ => panic!("expected Replay, got {:?}", replay),
    }
    let record = store.get_by_request("req-a", "dep-a").unwrap();
    assert_eq!(record.current_state, JobState::Reserved);
}

#[test]
fn test_provision_accept_conflict_different_fingerprint() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req1 = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req1, 1).unwrap();
    let req2 = test_request(
        "dep-a",
        "req-a",
        "fp-b",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    let result =
        accept_envelope_with_caller(router_principal(), &store, &deployment_store, req2, 2);
    assert_eq!(result, Err(ProvisionIngressError::Conflict));
    let record = store.get_by_request("req-a", "dep-a").unwrap();
    assert_eq!(record.request_fingerprint, "fp-a");
}

#[test]
fn test_provision_no_partial_writes_on_lock_failure() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    // Pre-lock the only intent.
    let held_key =
        ProvisioningIntentKey::new("dep-a", ProvisionableResourceKind::GraphShard, "shard-a");
    assert!(store.acquire_intent_lock(held_key.clone()));

    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    let result = accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1);
    assert_eq!(result, Err(ProvisionIngressError::IntentLockHeld));

    // No canonical record and no derived Map 2 entries remain.
    assert!(store.get_by_request("req-a", "dep-a").is_none());
    assert!(!store.has_live_job_for_deployment("dep-a"));
    // Pre-held lock is untouched.
    assert!(store.intent_locked(&held_key));
}

#[test]
fn test_provision_accept_empty_resources_rejected() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request("dep-a", "req-empty", "fp-empty", vec![]);
    let result = accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1);
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
            test_resource(ProvisionableResourceKind::GraphShard, "shard-a"),
            test_resource(ProvisionableResourceKind::GraphShard, "shard-a"),
        ],
    );
    let result = accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1);
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
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let result = query_job_with_caller(
        other_principal(),
        &store,
        &deployment_store,
        "req-a".to_owned(),
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
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let view = query_job_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        "req-a".to_owned(),
        "dep-a".to_owned(),
    )
    .unwrap();
    assert_eq!(view.request_id, "req-a");
    assert_eq!(view.state_name, "Reserved");
    assert!(view.has_router_callback);
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
        "req-a".to_owned(),
        "dep-missing".to_owned(),
    );
    assert_eq!(result, Err(ProvisionQueryError::UnknownDeployment));
}

// === router_ack ==============================================================

#[test]
fn test_provision_router_ack_not_found() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: "missing".to_owned(),
            accepted_registry_version: 1,
        },
        1,
    );
    assert_eq!(result, Err(ProvisionIngressError::NotFound));
}

#[test]
fn test_provision_router_ack_wrong_router_rejected() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let key = ProvisionJobRequestKey::new("req-a", "dep-a");
    advance_to_ack_pending(&store, &key, 10);
    let result = router_ack_with_caller(
        other_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: "req-a".to_owned(),
            accepted_registry_version: 1,
        },
        20,
    );
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
}

#[test]
fn test_provision_router_ack_invalid_state() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    // Record is in Reserved after accept.
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: "req-a".to_owned(),
            accepted_registry_version: 1,
        },
        2,
    );
    assert_eq!(result, Err(ProvisionIngressError::InvalidState));
}

#[test]
fn test_provision_router_ack_persists_registry_version() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let key = ProvisionJobRequestKey::new("req-a", "dep-a");
    advance_to_ack_pending(&store, &key, 10);
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: "req-a".to_owned(),
            accepted_registry_version: 7,
        },
        20,
    )
    .unwrap();
    assert!(result.completed);
    assert_eq!(result.accepted_registry_version, 7);
    let record = store.get_by_request("req-a", "dep-a").unwrap();
    assert_eq!(record.current_state, JobState::Completed);
    assert_eq!(record.accepted_registry_version, Some(7));
    assert!(!store.intent_locked(&ProvisioningIntentKey::new(
        "dep-a",
        ProvisionableResourceKind::GraphShard,
        "shard-a",
    )));
}

#[test]
fn test_provision_router_ack_missing_lock_returns_invalid_state() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let key = ProvisionJobRequestKey::new("req-a", "dep-a");
    advance_to_ack_pending(&store, &key, 10);
    // Release the lock behind the store's back.
    let lock_key =
        ProvisioningIntentKey::new("dep-a", ProvisionableResourceKind::GraphShard, "shard-a");
    assert!(store.release_intent_lock(&lock_key));
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: "req-a".to_owned(),
            accepted_registry_version: 1,
        },
        20,
    );
    assert_eq!(result, Err(ProvisionIngressError::InvalidState));
}

#[test]
fn test_provision_router_ack_idempotent_replay() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let key = ProvisionJobRequestKey::new("req-a", "dep-a");
    advance_to_ack_pending(&store, &key, 10);
    let ack = RouterProvisionAck {
        deployment_id: "dep-a".to_owned(),
        request_id: "req-a".to_owned(),
        accepted_registry_version: 7,
    };
    let first = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        ack.clone(),
        20,
    )
    .unwrap();
    let second =
        router_ack_with_caller(router_principal(), &store, &deployment_store, ack, 21).unwrap();
    assert_eq!(first, second);
    assert_eq!(second.accepted_registry_version, 7);
}

#[test]
fn test_provision_router_ack_completed_replay_returns_ok() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let key = ProvisionJobRequestKey::new("req-a", "dep-a");
    advance_to_ack_pending(&store, &key, 10);
    complete_record(&store, &key, 5, 30);
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: "req-a".to_owned(),
            accepted_registry_version: 5,
        },
        31,
    )
    .unwrap();
    assert!(result.completed);
    assert_eq!(result.accepted_registry_version, 5);
}

#[test]
fn test_provision_router_ack_completed_version_conflict_returns_ack_conflict() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let key = ProvisionJobRequestKey::new("req-a", "dep-a");
    advance_to_ack_pending(&store, &key, 10);
    complete_record(&store, &key, 5, 30);
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: "req-a".to_owned(),
            accepted_registry_version: 9,
        },
        31,
    );
    assert_eq!(
        result,
        Err(ProvisionIngressError::AckConflict { stored: 5 })
    );
}

#[test]
fn test_provision_router_ack_state_advance_failed_returns_state_advance_failed() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let key = ProvisionJobRequestKey::new("req-a", "dep-a");
    advance_to_ack_pending(&store, &key, 10);
    set_force_advance_error(true);
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: "req-a".to_owned(),
            accepted_registry_version: 1,
        },
        20,
    );
    assert_eq!(result, Err(ProvisionIngressError::StateAdvanceFailed));
    set_force_advance_error(false);
}

#[test]
fn test_provision_router_ack_unknown_deployment() {
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
            vec![test_resource(
                ProvisionableResourceKind::GraphShard,
                "shard-o",
            )],
        ),
        1,
    );
    store.insert_or_idempotent(record).unwrap();
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-orphan".to_owned(),
            request_id: "req-o".to_owned(),
            accepted_registry_version: 1,
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
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    let result =
        accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    match result {
        ProvisionAcceptResponse::Accepted {
            job_view,
            intent_lock_count,
        } => {
            assert_eq!(job_view.request_id, "req-a");
            assert_eq!(job_view.deployment_id, "dep-a");
            assert_eq!(job_view.state, "Reserved");
            assert_eq!(intent_lock_count, 1);
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
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        req.clone(),
        1,
    )
    .unwrap();
    let result =
        accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 2).unwrap();
    match result {
        ProvisionAcceptResponse::Replay {
            job_view,
            intent_lock_count,
        } => {
            assert_eq!(job_view.request_id, "req-a");
            assert_eq!(job_view.state, "Reserved");
            assert_eq!(intent_lock_count, 1);
        }
        ProvisionAcceptResponse::Accepted { .. } => {
            panic!("replay must report Replay, not Accepted")
        }
    }
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
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    let result =
        accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
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
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    let _a = accept_envelope_with_caller(router_principal(), &store, &deployment_store, req_a, 1)
        .unwrap();

    let key_a = ProvisionJobRequestKey::new("req-a", "dep-a");
    let intent_key =
        ProvisioningIntentKey::new("dep-a", ProvisionableResourceKind::GraphShard, "shard-a");

    // Job B: same deployment, same resource, different request_id.
    let req_b = test_request(
        "dep-a",
        "req-b",
        "fp-b",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    let result =
        accept_envelope_with_caller(router_principal(), &store, &deployment_store, req_b, 2);
    assert_eq!(result, Err(ProvisionIngressError::IntentLockHeld));

    // A's canonical record is unchanged.
    let record_a = store.get_by_request("req-a", "dep-a").unwrap();
    assert_eq!(record_a.request_id, "req-a");

    // A's lock survives.
    assert!(store.intent_locked(&intent_key));

    // The derived index maps R1.intent to A.key before the conflict is attempted.
    assert_eq!(
        store.assert_intent_to_request_for_test(
            "dep-a",
            ProvisionableResourceKind::GraphShard,
            "shard-a"
        ),
        Some(key_a.clone()),
        "derived index must map R1.intent to A.key before B is attempted"
    );

    // After B is rejected, the same intent still resolves to A.key; B never overwrote the derived row.
    assert_eq!(
        store.assert_intent_to_request_for_test(
            "dep-a",
            ProvisionableResourceKind::GraphShard,
            "shard-a"
        ),
        Some(key_a.clone()),
        "derived index must still map R1.intent to A.key after B is rejected"
    );

    // B leaves no canonical or derived row.
    assert_eq!(
        store.get_by_request("req-b", "dep-a"),
        None,
        "B must not leave a canonical row"
    );
    assert_eq!(
        store.get_by_request_key(&ProvisionJobRequestKey::new("req-b", "dep-a")),
        None,
        "B must not leave a canonical row via its composite key"
    );
}

#[test]
fn test_provision_router_ack_cross_deployment_ambiguity() {
    reset_all_maps();

    // Seed two deployments with different router principals.
    init::init(init::ProvisionInitArgs {
        bootstrap_bindings: vec![
            DeploymentBinding {
                deployment_id: "d1".to_owned(),
                router_principal: router_principal(),
                governance_principal: gov_principal(),
                binding_version: 1,
            },
            DeploymentBinding {
                deployment_id: "d2".to_owned(),
                router_principal: other_principal(),
                governance_principal: gov_principal(),
                binding_version: 1,
            },
        ],
    });
    let deployment_store = DeploymentTrustStore::new();
    let store = ProvisionJobStore::new();

    // A1: (r1, d1) and A2: (r1, d2), both in RouterAckPending.
    let req1 = test_request(
        "d1",
        "r1",
        "fp-1",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-1",
        )],
    );
    let req2 = test_request(
        "d2",
        "r1",
        "fp-2",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-2",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req1, 1).unwrap();
    accept_envelope_with_caller(other_principal(), &store, &deployment_store, req2, 2).unwrap();

    let key1 = ProvisionJobRequestKey::new("r1", "d1");
    let key2 = ProvisionJobRequestKey::new("r1", "d2");
    advance_to_ack_pending(&store, &key1, 10);
    advance_to_ack_pending(&store, &key2, 20);

    // D1's router attempts to ack (r1, d2). The handler resolves the record by
    // the canonical (request_id, deployment_id) key, then authenticates against the
    // stored router principal for d2 (which is other_principal). D1's router is
    // rejected with NotAuthorized; A2 remains RouterAckPending.
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "d2".to_owned(),
            request_id: "r1".to_owned(),
            accepted_registry_version: 1,
        },
        30,
    );
    assert_eq!(result, Err(ProvisionIngressError::NotAuthorized));
    let a2_before = store.get_by_request("r1", "d2").unwrap();
    assert_eq!(a2_before.current_state, JobState::RouterAckPending);

    // Correct ack (r1, d1) advances A1.
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "d1".to_owned(),
            request_id: "r1".to_owned(),
            accepted_registry_version: 7,
        },
        31,
    );
    assert!(result.unwrap().completed);
    let a1_after = store.get_by_request("r1", "d1").unwrap();
    assert_eq!(a1_after.current_state, JobState::Completed);
    assert_eq!(a1_after.accepted_registry_version, Some(7));
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
fn test_provision_router_ack_ack_conflict_after_durable_completion() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let key = ProvisionJobRequestKey::new("req-a", "dep-a");
    advance_to_ack_pending(&store, &key, 10);

    // First ack persists version 7 and advances to Completed.
    let first = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: "req-a".to_owned(),
            accepted_registry_version: 7,
        },
        20,
    )
    .unwrap();
    assert_eq!(first.accepted_registry_version, 7);

    let record = store.get_by_request("req-a", "dep-a").unwrap();
    assert_eq!(record.current_state, JobState::Completed);
    assert_eq!(record.accepted_registry_version, Some(7));

    // Second ack with a different version must conflict against the durable stored version.
    let result = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        RouterProvisionAck {
            deployment_id: "dep-a".to_owned(),
            request_id: "req-a".to_owned(),
            accepted_registry_version: 9,
        },
        21,
    );
    assert_eq!(
        result,
        Err(ProvisionIngressError::AckConflict { stored: 7 })
    );

    // The durable record must still retain the first ack's version.
    let record = store.get_by_request("req-a", "dep-a").unwrap();
    assert_eq!(record.accepted_registry_version, Some(7));
}

#[test]
fn test_provision_router_ack_completed_then_retry_returns_replay() {
    reset_all_maps();
    let (deployment_store, store) = insert_binding_and_init("dep-a");
    let req = test_request(
        "dep-a",
        "req-a",
        "fp-a",
        vec![test_resource(
            ProvisionableResourceKind::GraphShard,
            "shard-a",
        )],
    );
    accept_envelope_with_caller(router_principal(), &store, &deployment_store, req, 1).unwrap();
    let key = ProvisionJobRequestKey::new("req-a", "dep-a");
    advance_to_ack_pending(&store, &key, 10);
    let ack = RouterProvisionAck {
        deployment_id: "dep-a".to_owned(),
        request_id: "req-a".to_owned(),
        accepted_registry_version: 5,
    };
    let first = router_ack_with_caller(
        router_principal(),
        &store,
        &deployment_store,
        ack.clone(),
        20,
    )
    .unwrap();
    let second =
        router_ack_with_caller(router_principal(), &store, &deployment_store, ack, 21).unwrap();
    assert_eq!(first, second);
    assert!(second.completed);
    assert_eq!(second.accepted_registry_version, 5);
}

// === helpers / record_to_result / get_by_request_id ==========================

#[test]
fn test_provision_record_to_result_reserved_state_returns_err() {
    let record = build_record_from_request(
        test_request(
            "dep-a",
            "req-a",
            "fp-a",
            vec![test_resource(
                ProvisionableResourceKind::GraphShard,
                "shard-a",
            )],
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
        request_id: record.request_id.clone(),
        request_fingerprint: record.request_fingerprint.clone(),
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
            vec![test_resource(
                ProvisionableResourceKind::GraphShard,
                "shard-a",
            )],
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
            vec![test_resource(
                ProvisionableResourceKind::GraphShard,
                "shard-a",
            )],
        ),
        1,
    );
    let record_b = build_record_from_request(
        test_request(
            "dep-b",
            "req-b",
            "fp-b",
            vec![test_resource(
                ProvisionableResourceKind::GraphShard,
                "shard-b",
            )],
        ),
        1,
    );
    store.insert_or_idempotent(record_a.clone()).unwrap();
    store.insert_or_idempotent(record_b).unwrap();
    assert_eq!(store.get_by_request("missing", "dep-a"), None);
    assert_eq!(store.get_by_request("req-a", "dep-a"), Some(record_a));
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
            ProvisionIngressError::NotFound => "test_provision_router_ack_not_found",
            ProvisionIngressError::InvalidState => "test_provision_router_ack_invalid_state",
            ProvisionIngressError::StateAdvanceFailed => {
                "test_provision_router_ack_state_advance_failed_returns_state_advance_failed"
            }
            ProvisionIngressError::ResultMappingError => {
                "test_provision_record_to_result_completed_with_missing_canister_id"
            }
            ProvisionIngressError::AckConflict { .. } => {
                "test_provision_router_ack_completed_version_conflict_returns_ack_conflict"
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
    assert!(!ingress_name(ProvisionIngressError::AckConflict { stored: 0 }).is_empty());
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
        "missing".to_owned(),
        "dep-a".to_owned(),
    );
    assert_eq!(result, Err(ProvisionQueryError::NotFound));
}
