//! Unit tests for `DeploymentTrustStore` and `ProvisionJobStore`.

use super::{DeploymentTrustStore, ProvisionJobStore};
use crate::types::{
    DeploymentBinding, JobState, ProvisionIntentLockMarker, ProvisionJobRecord,
    ProvisionJobRequestKey, ProvisionableResourceKind, ProvisioningIntentKey, ResourceJobEntry,
};
use candid::Principal;
use ic_stable_structures::Storable;

fn test_principal(id: u8) -> Principal {
    Principal::from_slice(&[id; 29])
}

fn test_deployment_binding(deployment_id: &str, router_id: u8, gov_id: u8) -> DeploymentBinding {
    DeploymentBinding {
        deployment_id: deployment_id.to_owned(),
        router_principal: test_principal(router_id),
        governance_principal: test_principal(gov_id),
        binding_version: 1,
    }
}

fn test_record_with_resources(
    request_id: &str,
    deployment_id: &str,
    fingerprint: &str,
    resource_keys: &[(&str, ProvisionableResourceKind)],
) -> ProvisionJobRecord {
    let intent_key = if resource_keys.is_empty() {
        ProvisioningIntentKey {
            deployment_id: deployment_id.to_owned(),
            resource_kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "__empty".to_owned(),
        }
    } else {
        ProvisioningIntentKey {
            deployment_id: deployment_id.to_owned(),
            resource_kind: resource_keys[0].1,
            logical_resource_key: resource_keys[0].0.to_owned(),
        }
    };
    ProvisionJobRecord {
        request_id: request_id.to_owned(),
        deployment_id: deployment_id.to_owned(),
        request_fingerprint: fingerprint.to_owned(),
        intent_key,
        reserved_graph_id: None,
        graph_name: "test-graph".to_owned(),
        authorized_caller: test_principal(0),
        release_id: "r1".to_owned(),
        router_callback_principal: test_principal(1),
        resources: resource_keys
            .iter()
            .map(|(key, kind)| ResourceJobEntry {
                resource_kind: *kind,
                logical_resource_key: key.to_string(),
                canister_id: None,
                artifact_hash: None,
            })
            .collect(),
        current_state: JobState::Submitted,
        active_resource_index: 0,
        completed_effect_count: 0,
        accepted_registry_version: None,
        created_at_ns: 1_700_000_000_000_000_000,
        last_transition_ns: 0,
    }
}

fn test_record(request_id: &str, deployment_id: &str, fingerprint: &str) -> ProvisionJobRecord {
    test_record_with_resources(
        request_id,
        deployment_id,
        fingerprint,
        &[("shard-1", ProvisionableResourceKind::GraphShard)],
    )
}

#[test]
fn test_get_or_install_creates_and_returns() {
    super::reset_all_maps();
    let store = DeploymentTrustStore::new();
    let binding = test_deployment_binding("d1", 10, 20);
    let first = store.get_or_install(binding.clone());
    assert_eq!(first, binding);
    let second = store.get_or_install(binding.clone());
    assert_eq!(second, binding);
    assert_eq!(store.get("d1"), Some(binding));
}

#[test]
#[should_panic(expected = "DeploymentBinding mismatch")]
fn test_get_or_install_mismatch_panics() {
    super::reset_all_maps();
    let store = DeploymentTrustStore::new();
    let binding = test_deployment_binding("d1", 10, 20);
    store.get_or_install(binding);
    let mismatch = test_deployment_binding("d1", 11, 20);
    store.get_or_install(mismatch);
}

#[test]
fn test_update_by_governance() {
    super::reset_all_maps();
    let store = DeploymentTrustStore::new();
    let binding = test_deployment_binding("d1", 10, 20);
    store.get_or_install(binding.clone());
    let updated = DeploymentBinding {
        router_principal: test_principal(30),
        ..binding.clone()
    };
    assert!(store.update(binding.governance_principal, updated).is_ok());
    assert_eq!(
        store.get("d1").unwrap().router_principal,
        test_principal(30)
    );
}

#[test]
fn test_update_by_non_governance_fails() {
    super::reset_all_maps();
    let store = DeploymentTrustStore::new();
    let binding = test_deployment_binding("d1", 10, 20);
    store.get_or_install(binding.clone());
    let updated = DeploymentBinding {
        router_principal: test_principal(30),
        ..binding.clone()
    };
    assert_eq!(
        store.update(test_principal(99), updated),
        Err(super::TrustUpdateError::NotAuthorized)
    );
}

#[test]
fn test_insert_idempotent_same_fingerprint() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record("req-1", "dep-1", "fp-1");
    let first = store.insert_or_idempotent(record.clone()).unwrap();
    let second = store.insert_or_idempotent(record).unwrap();
    assert_eq!(first.request_id, "req-1");
    assert_eq!(second.request_id, "req-1");
    assert_eq!(first.request_fingerprint, "fp-1");
}

#[test]
fn test_insert_conflict_different_fingerprint() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record("req-1", "dep-1", "fp-1");
    store.insert_or_idempotent(record).unwrap();
    let conflict = test_record("req-1", "dep-1", "fp-2");
    assert_eq!(
        store.insert_or_idempotent(conflict),
        Err(super::JobInsertError::Conflict)
    );
}

#[test]
fn test_advance_state_valid_transition() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record("req-1", "dep-1", "fp-1");
    let key = ProvisionJobRequestKey::new("req-1", "dep-1");
    store.insert_or_idempotent(record).unwrap();
    assert!(
        store
            .advance_state(&key, JobState::Reserved, None, 100)
            .is_ok()
    );
    let updated = store.get_by_request_key(&key).unwrap();
    assert_eq!(updated.current_state, JobState::Reserved);
    assert_eq!(updated.last_transition_ns, 100);
}

#[test]
fn test_advance_state_full_machine() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record("req-1", "dep-1", "fp-1");
    let key = ProvisionJobRequestKey::new("req-1", "dep-1");
    store.insert_or_idempotent(record).unwrap();
    let steps = [
        JobState::Reserved,
        JobState::CreatePending,
        JobState::CanisterCreated,
        JobState::InstallPending,
        JobState::Installed,
        JobState::RouterRegistrationPending,
        JobState::RouterAckPending,
        JobState::Completed,
    ];
    for (i, step) in steps.iter().enumerate() {
        let now = 100 + i as u64;
        assert!(
            store
                .advance_state(&key, step.clone(), Some(i), now)
                .is_ok(),
            "failed at step {:?}",
            step
        );
    }
    let final_record = store.get_by_request_key(&key).unwrap();
    assert_eq!(final_record.current_state, JobState::Completed);
    assert_eq!(final_record.active_resource_index, steps.len() - 1);
    assert_eq!(final_record.completed_effect_count, 3);
}

#[test]
fn test_advance_state_invalid_transition() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record("req-1", "dep-1", "fp-1");
    let key = ProvisionJobRequestKey::new("req-1", "dep-1");
    store.insert_or_idempotent(record).unwrap();
    assert_eq!(
        store.advance_state(&key, JobState::Completed, None, 100),
        Err(super::JobAdvanceError::InvalidTransition)
    );
    let unchanged = store.get_by_request_key(&key).unwrap();
    assert_eq!(unchanged.current_state, JobState::Submitted);
    assert_eq!(unchanged.last_transition_ns, 0);
}

#[test]
fn test_advance_state_to_failed() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record("req-1", "dep-1", "fp-1");
    let key = ProvisionJobRequestKey::new("req-1", "dep-1");
    store.insert_or_idempotent(record).unwrap();
    assert!(
        store
            .advance_state(
                &key,
                JobState::Failed {
                    reason: "boom".to_owned()
                },
                None,
                42
            )
            .is_ok()
    );
    let failed = store.get_by_request_key(&key).unwrap();
    assert_eq!(
        failed.current_state,
        JobState::Failed {
            reason: "boom".to_owned()
        }
    );
    assert_eq!(failed.last_transition_ns, 42);
}

#[test]
fn test_advance_state_failed_terminal() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record("req-1", "dep-1", "fp-1");
    let key = ProvisionJobRequestKey::new("req-1", "dep-1");
    store.insert_or_idempotent(record).unwrap();
    store
        .advance_state(
            &key,
            JobState::Failed {
                reason: "boom".to_owned(),
            },
            None,
            1,
        )
        .unwrap();
    assert_eq!(
        store.advance_state(
            &key,
            JobState::Failed {
                reason: "x".to_owned()
            },
            None,
            2
        ),
        Err(super::JobAdvanceError::InvalidTransition)
    );
    assert_eq!(
        store.advance_state(&key, JobState::Reserved, None, 2),
        Err(super::JobAdvanceError::InvalidTransition)
    );
}

#[test]
fn test_intent_lock_acquire_and_release() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let key = ProvisioningIntentKey {
        deployment_id: "dep-1".to_owned(),
        resource_kind: ProvisionableResourceKind::GraphShard,
        logical_resource_key: "shard-1".to_owned(),
    };
    assert!(store.acquire_intent_lock(key.clone()));
    assert!(store.intent_locked(&key));
    assert!(store.release_intent_lock(&key));
    assert!(!store.intent_locked(&key));
}

#[test]
fn test_intent_lock_blocks_second_acquirer() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let key = ProvisioningIntentKey {
        deployment_id: "dep-1".to_owned(),
        resource_kind: ProvisionableResourceKind::GraphShard,
        logical_resource_key: "shard-1".to_owned(),
    };
    assert!(store.acquire_intent_lock(key.clone()));
    assert!(!store.acquire_intent_lock(key.clone()));
}

#[test]
fn test_acquire_intent_locks_for_record_multi_resource() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record_with_resources(
        "req-1",
        "dep-1",
        "fp-1",
        &[
            ("shard-1", ProvisionableResourceKind::GraphShard),
            ("idx-1", ProvisionableResourceKind::PropertyIndex),
            ("vec-1", ProvisionableResourceKind::VectorIndex),
        ],
    );
    assert_eq!(store.acquire_intent_locks_for_record(&record).unwrap(), 3);
    for (name, kind) in &[
        ("shard-1", ProvisionableResourceKind::GraphShard),
        ("idx-1", ProvisionableResourceKind::PropertyIndex),
        ("vec-1", ProvisionableResourceKind::VectorIndex),
    ] {
        let lock_key = ProvisioningIntentKey {
            deployment_id: "dep-1".to_owned(),
            resource_kind: *kind,
            logical_resource_key: name.to_string(),
        };
        assert!(store.intent_locked(&lock_key));
    }
}

#[test]
fn test_acquire_intent_locks_for_record_already_held() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record_with_resources(
        "req-1",
        "dep-1",
        "fp-1",
        &[
            ("shard-1", ProvisionableResourceKind::GraphShard),
            ("idx-1", ProvisionableResourceKind::PropertyIndex),
        ],
    );
    let held_key = ProvisioningIntentKey {
        deployment_id: "dep-1".to_owned(),
        resource_kind: ProvisionableResourceKind::PropertyIndex,
        logical_resource_key: "idx-1".to_owned(),
    };
    assert!(store.acquire_intent_lock(held_key.clone()));
    assert_eq!(
        store.acquire_intent_locks_for_record(&record),
        Err(super::IntentLockAcquireError::AlreadyHeld)
    );
    // First resource's marker must have been rolled back (no partial leakage).
    let first_key = ProvisioningIntentKey {
        deployment_id: "dep-1".to_owned(),
        resource_kind: ProvisionableResourceKind::GraphShard,
        logical_resource_key: "shard-1".to_owned(),
    };
    assert!(!store.intent_locked(&first_key));
    // Re-try after clearing the held key succeeds.
    assert!(store.release_intent_lock(&held_key));
    assert_eq!(store.acquire_intent_locks_for_record(&record).unwrap(), 2);
}

#[test]
fn test_clear_intent_locks_for_record_releases_all() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record_with_resources(
        "req-1",
        "dep-1",
        "fp-1",
        &[
            ("shard-1", ProvisionableResourceKind::GraphShard),
            ("idx-1", ProvisionableResourceKind::PropertyIndex),
            ("vec-1", ProvisionableResourceKind::VectorIndex),
        ],
    );
    assert_eq!(store.acquire_intent_locks_for_record(&record).unwrap(), 3);
    assert_eq!(store.clear_intent_locks_for_record(&record), 3);
    for (name, kind) in &[
        ("shard-1", ProvisionableResourceKind::GraphShard),
        ("idx-1", ProvisionableResourceKind::PropertyIndex),
        ("vec-1", ProvisionableResourceKind::VectorIndex),
    ] {
        let lock_key = ProvisioningIntentKey {
            deployment_id: "dep-1".to_owned(),
            resource_kind: *kind,
            logical_resource_key: name.to_string(),
        };
        assert!(!store.intent_locked(&lock_key));
    }
}

#[test]
fn test_advance_state_persists_last_transition_ns() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record("req-1", "dep-1", "fp-1");
    let key = ProvisionJobRequestKey::new("req-1", "dep-1");
    store.insert_or_idempotent(record).unwrap();
    store
        .advance_state(&key, JobState::Reserved, None, 42)
        .unwrap();
    assert_eq!(
        store.get_by_request_key(&key).unwrap().last_transition_ns,
        42
    );
    store
        .advance_state(&key, JobState::CreatePending, None, 100)
        .unwrap();
    assert_eq!(
        store.get_by_request_key(&key).unwrap().last_transition_ns,
        100
    );
}

#[test]
fn test_storable_round_trip() {
    super::reset_all_maps();
    let record = test_record("req-1", "dep-1", "fp-1");
    let bytes = record.into_bytes();
    let decoded = ProvisionJobRecord::from_bytes(bytes.into());
    assert_eq!(decoded.request_id, "req-1");
    assert_eq!(decoded.current_state, JobState::Submitted);
}

#[test]
fn test_marker_is_zero_bytes() {
    super::reset_all_maps();
    let marker = ProvisionIntentLockMarker;
    assert!(marker.to_bytes().is_empty());
    assert_eq!(
        ProvisionIntentLockMarker::BOUND,
        ic_stable_structures::storable::Bound::Unbounded
    );
}

#[test]
fn test_set_resource_canister_id_persists_target_and_preserves_siblings() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record_with_resources(
        "req-set",
        "dep-set",
        "fp-set",
        &[
            ("shard-0", ProvisionableResourceKind::GraphShard),
            ("idx-0", ProvisionableResourceKind::PropertyIndex),
            ("vec-0", ProvisionableResourceKind::VectorIndex),
        ],
    );
    let key = ProvisionJobRequestKey::new("req-set", "dep-set");
    store.insert_or_idempotent(record).unwrap();

    let canister_id = test_principal(42);
    store.set_resource_canister_id(&key, 1, canister_id);

    let updated = store.get_by_request_key(&key).unwrap();
    assert_eq!(updated.resources[0].canister_id, None);
    assert_eq!(updated.resources[1].canister_id, Some(canister_id));
    assert_eq!(updated.resources[2].canister_id, None);
    assert_eq!(updated.resources[1].logical_resource_key, "idx-0");
}

#[test]
#[should_panic(expected = "set_resource_canister_id: record not found")]
fn test_set_resource_canister_id_missing_record_panics() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let key = ProvisionJobRequestKey::new("req-missing", "dep-missing");
    store.set_resource_canister_id(&key, 0, test_principal(7));
}

#[test]
#[should_panic]
fn test_set_resource_canister_id_out_of_bounds_panics() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record("req-bounds", "dep-bounds", "fp-bounds");
    let key = ProvisionJobRequestKey::new("req-bounds", "dep-bounds");
    store.insert_or_idempotent(record).unwrap();
    store.set_resource_canister_id(&key, 5, test_principal(7));
}

#[test]
fn test_insert_writes_job_by_deployment_derived_index() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record_with_resources(
        "req-derived",
        "dep-derived",
        "fp-derived",
        &[
            ("shard-0", ProvisionableResourceKind::GraphShard),
            ("idx-0", ProvisionableResourceKind::PropertyIndex),
        ],
    );
    let expected_key = ProvisionJobRequestKey::new("req-derived", "dep-derived");
    store.insert_or_idempotent(record).unwrap();

    super::JOB_BY_DEPLOYMENT.with_borrow(|map| {
        let shard_intent = ProvisioningIntentKey {
            deployment_id: "dep-derived".to_owned(),
            resource_kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-0".to_owned(),
        };
        let idx_intent = ProvisioningIntentKey {
            deployment_id: "dep-derived".to_owned(),
            resource_kind: ProvisionableResourceKind::PropertyIndex,
            logical_resource_key: "idx-0".to_owned(),
        };
        assert_eq!(map.get(&shard_intent), Some(expected_key.clone()));
        assert_eq!(map.get(&idx_intent), Some(expected_key));
    });
}

#[test]
fn test_job_by_deployment_derived_index_has_no_cross_leakage() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();
    let record = test_record_with_resources(
        "req-leak",
        "dep-leak",
        "fp-leak",
        &[("shard-0", ProvisionableResourceKind::GraphShard)],
    );
    store.insert_or_idempotent(record).unwrap();

    super::JOB_BY_DEPLOYMENT.with_borrow(|map| {
        let wrong_deployment = ProvisioningIntentKey {
            deployment_id: "other-dep".to_owned(),
            resource_kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "shard-0".to_owned(),
        };
        let wrong_intent = ProvisioningIntentKey {
            deployment_id: "dep-leak".to_owned(),
            resource_kind: ProvisionableResourceKind::GraphShard,
            logical_resource_key: "different-key".to_owned(),
        };
        assert_eq!(map.get(&wrong_deployment), None);
        assert_eq!(map.get(&wrong_intent), None);
    });
}

#[test]
fn test_insert_with_intent_locks_preserves_existing_derived_index_on_conflict() {
    super::reset_all_maps();
    let store = ProvisionJobStore::new();

    // Job A seeds the canonical record, derived index, and intent lock.
    let record_a = test_record_with_resources(
        "req-a",
        "dep-a",
        "fp-a",
        &[("shard-a", ProvisionableResourceKind::GraphShard)],
    );
    let key_a = ProvisionJobRequestKey::new("req-a", "dep-a");
    store.insert_with_intent_locks(record_a, 1).unwrap();
    assert_eq!(
        store.assert_intent_to_request_for_test(
            "dep-a",
            ProvisionableResourceKind::GraphShard,
            "shard-a"
        ),
        Some(key_a.clone()),
        "derived index must map R1.intent to A.key after seeding"
    );

    // Job B conflicts on the held intent; the store boundary must leave A's derived row intact.
    let record_b = test_record_with_resources(
        "req-b",
        "dep-a",
        "fp-b",
        &[("shard-a", ProvisionableResourceKind::GraphShard)],
    );
    assert!(matches!(
        store.insert_with_intent_locks(record_b, 2),
        Err(super::InsertWithLocksError::IntentLockHeld)
    ));
    assert_eq!(
        store.assert_intent_to_request_for_test(
            "dep-a",
            ProvisionableResourceKind::GraphShard,
            "shard-a"
        ),
        Some(key_a),
        "derived index must still map R1.intent to A.key after B is rejected"
    );
    assert_eq!(
        store.get_by_request("req-b", "dep-a"),
        None,
        "B must not leave a canonical row"
    );
}

// === admin_upsert + BootstrapAuth facade (ADR 0035 Slice 7) ================

use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;
use crate::types::{BootstrapAuthAction, BootstrapAuthorityRecord};

fn admin_binding(
    deployment_id: &str,
    router_id: u8,
    gov_id: u8,
    version: u64,
) -> DeploymentBinding {
    DeploymentBinding {
        deployment_id: deployment_id.to_owned(),
        router_principal: test_principal(router_id),
        governance_principal: test_principal(gov_id),
        binding_version: version,
    }
}

#[test]
fn admin_upsert_overwrites_existing_binding_without_panic() {
    super::reset_all_maps();
    let store = DeploymentTrustStore::new();
    let first = admin_binding("d1", 10, 20, 1);
    let second = admin_binding("d1", 11, 21, 2);
    assert_eq!(store.admin_upsert(first), admin_binding("d1", 10, 20, 1));
    assert_eq!(store.admin_upsert(second), admin_binding("d1", 11, 21, 2));
    let persisted = store.get("d1").unwrap();
    assert_eq!(persisted.router_principal, test_principal(11));
    assert_eq!(persisted.governance_principal, test_principal(21));
    assert_eq!(persisted.binding_version, 2);
}

#[test]
fn bootstrap_auth_facade_uses_separate_memory_ids() {
    super::reset_all_maps();
    let auth_store = ProvisionBootstrapAuthStore::new();
    let gov = test_principal(42);
    let record = BootstrapAuthorityRecord {
        governance_principal: gov,
        binding_version_at_seed: 7,
        seeded_at_ns: 100,
    };
    auth_store.set_authority(record.clone());
    auth_store.put_record(
        gov,
        crate::types::BootstrapAuthEntry {
            caller: gov,
            deployment_id: Some("dep-a".to_owned()),
            action: BootstrapAuthAction::InitialSeed,
            timestamp_ns: 100,
            registry_version: Some(7),
        },
    );

    assert_eq!(auth_store.get_authority(), Some(record));
    let history = auth_store.history(gov);
    assert_eq!(history.len(), 1);
    assert_eq!(history[0].action, BootstrapAuthAction::InitialSeed);
    // A bug that placed both structures on one MemoryId would likely corrupt one of these reads.
    assert_eq!(
        auth_store.get_authority().unwrap().binding_version_at_seed,
        7
    );
}

#[test]
fn stable_cell_singleton_writes_overwrite_previous_value() {
    super::reset_all_maps();
    let auth_store = ProvisionBootstrapAuthStore::new();
    let first = BootstrapAuthorityRecord {
        governance_principal: test_principal(1),
        binding_version_at_seed: 1,
        seeded_at_ns: 1,
    };
    let second = BootstrapAuthorityRecord {
        governance_principal: test_principal(2),
        binding_version_at_seed: 2,
        seeded_at_ns: 2,
    };
    auth_store.set_authority(first);
    assert_eq!(
        auth_store.get_authority().unwrap().governance_principal,
        test_principal(1)
    );
    auth_store.set_authority(second);
    assert_eq!(
        auth_store.get_authority().unwrap().governance_principal,
        test_principal(2)
    );
}
