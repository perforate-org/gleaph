//! Unit tests for `DeploymentTrustStore` and `ProvisionJobStore`.

use super::{DeploymentTrustStore, ProvisionJobStore};
use crate::canister::{
    artifact_publish_metadata_with_caller, artifact_upload_chunk_with_caller,
    release_activate_with_caller, release_publish_with_caller,
};
use crate::stable::artifact::ProvisionArtifactStore;
use crate::stable::memory::{
    StableActiveReleaseCell, StableReleaseManifestMap, init_active_release, init_release_manifest,
};
use crate::stable::memory::{StableArtifactAuditLogMap, init_artifact_audit_log};
use crate::stable::release::ProvisionReleaseStore;
use crate::types::{ArtifactAuditAction, ArtifactAuditEntry, ArtifactAuditOutcome};
use crate::types::{
    ArtifactChunk, ArtifactChunkKey, ArtifactId, ArtifactMetadata, ArtifactPublishMetadataArgs,
    ArtifactUploadChunkArgs, ArtifactUploadState, CanisterKind, ReleaseActivateArgs, ReleaseError,
    ReleaseId, ReleaseManifest, ReleasePublishArgs, sha256,
};
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

// === Artifact catalog store tests (Plan 0061a) ===============================

fn router_artifact_id(version: &str, sha: [u8; 32]) -> ArtifactId {
    ArtifactId::new(CanisterKind::Router, version.to_owned(), sha)
}

fn artifact_metadata(
    id: ArtifactId,
    byte_length: u64,
    chunk_hashes: Vec<[u8; 32]>,
) -> ArtifactMetadata {
    ArtifactMetadata {
        artifact_id: id,
        byte_length,
        chunk_hashes,
        created_at_ns: 1,
    }
}

/// (a) artifact_publish_metadata installs an immutable record.
#[test]
fn artifact_publish_metadata_installs_immutable_record() {
    super::reset_all_maps();
    let store = ProvisionArtifactStore::new();
    let sha = sha256(b"v1");
    let id = router_artifact_id("0.1.0", sha);
    let metadata = artifact_metadata(id.clone(), 4, vec![sha256(b"chunk0")]);
    let published = store.publish_metadata(metadata.clone()).unwrap();
    assert_eq!(published.artifact_id, id);
    assert_eq!(published.byte_length, 4);
    assert_eq!(store.get_metadata(&id).unwrap().artifact_id, id);
}

/// (b) artifact_publish_metadata rejects a conflicting publish of the same ArtifactId.
#[test]
fn artifact_publish_metadata_rejects_conflicting_publish() {
    super::reset_all_maps();
    let store = ProvisionArtifactStore::new();
    let sha = sha256(b"v1");
    let id = router_artifact_id("0.1.0", sha);
    let first = artifact_metadata(id.clone(), 4, vec![sha256(b"chunk0")]);
    store.publish_metadata(first).unwrap();

    let second = artifact_metadata(id.clone(), 8, vec![sha256(b"chunk0"), sha256(b"chunk1")]);
    let err = store.publish_metadata(second).unwrap_err();
    assert!(
        matches!(err, crate::types::ArtifactError::ConflictingMetadata { ref existing, ref requested } if *existing == id && *requested == id),
        "expected ConflictingMetadata, got {:?}",
        err
    );
}

/// (e) artifact_upload_chunk promotes to Verified on full SHA-256 match and rejects post-verify uploads.
#[test]
fn artifact_upload_chunk_promotes_to_verified_on_full_match() {
    super::reset_all_maps();
    use crate::canister::{
        artifact_publish_metadata_with_caller, artifact_upload_chunk_with_caller,
    };
    use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;
    use crate::types::{ArtifactError, ArtifactUploadChunkArgs, ArtifactUploadState};
    use candid::Principal;

    let gov = Principal::from_slice(&[1; 29]);
    ProvisionBootstrapAuthStore::new().set_authority(crate::types::BootstrapAuthorityRecord {
        governance_principal: gov,
        binding_version_at_seed: 1,
        seeded_at_ns: 1,
    });

    let chunk0 = b"aaaa";
    let chunk1 = b"bbbb";
    let full = [chunk0.as_slice(), chunk1.as_slice()].concat();
    let full_sha = sha256(&full);
    let id = router_artifact_id("0.2.0", full_sha);

    artifact_publish_metadata_with_caller(
        gov,
        crate::types::ArtifactPublishMetadataArgs {
            canister_kind: CanisterKind::Router,
            semantic_version: "0.2.0".to_owned(),
            sha256: full_sha,
            byte_length: full.len() as u64,
            chunk_hashes: vec![sha256(chunk0), sha256(chunk1)],
        },
        1,
    )
    .unwrap();

    artifact_upload_chunk_with_caller(
        gov,
        ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 0,
            bytes: chunk0.to_vec(),
        },
        2,
    )
    .unwrap();

    let verified = artifact_upload_chunk_with_caller(
        gov,
        ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 1,
            bytes: chunk1.to_vec(),
        },
        3,
    )
    .unwrap();
    assert!(matches!(
        verified.state,
        ArtifactUploadState::Verified { verified_at_ns: 3 }
    ));

    // Region 7 entry deleted.
    let artifact_store = ProvisionArtifactStore::new();
    assert_eq!(artifact_store.get_upload(&id), None);
    // Region 8 chunks retained as verified canonical.
    assert!(
        artifact_store
            .get_chunk(&ArtifactChunkKey {
                artifact_id: id.clone(),
                chunk_index: 0
            })
            .is_some()
    );
    assert!(
        artifact_store
            .get_chunk(&ArtifactChunkKey {
                artifact_id: id.clone(),
                chunk_index: 1
            })
            .is_some()
    );

    // Oracle-hardened: post-verify upload attempt must fail with ConflictingMetadata.
    let post_verify = artifact_upload_chunk_with_caller(
        gov,
        ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 0,
            bytes: chunk0.to_vec(),
        },
        4,
    );
    assert!(
        matches!(post_verify, Err(ArtifactError::ConflictingMetadata { .. })),
        "expected ConflictingMetadata after verify, got {:?}",
        post_verify
    );
    // Region 7 remains absent and region 8 unchanged.
    assert_eq!(artifact_store.get_upload(&id), None);
    assert_eq!(
        artifact_store
            .get_chunk(&ArtifactChunkKey {
                artifact_id: id.clone(),
                chunk_index: 0
            })
            .unwrap()
            .bytes,
        chunk0.to_vec()
    );
}

/// (f) artifact_upload_chunk full SHA-256 mismatch returns error, preserves Failed state, cleans chunks.
#[test]
fn artifact_upload_chunk_full_sha256_mismatch_returns_error_and_preserves_state() {
    super::reset_all_maps();
    use crate::canister::{
        artifact_publish_metadata_with_caller, artifact_upload_chunk_with_caller,
    };
    use crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore;
    use crate::types::{ArtifactError, ArtifactUploadChunkArgs, ArtifactUploadState};
    use candid::Principal;

    let gov = Principal::from_slice(&[1; 29]);
    ProvisionBootstrapAuthStore::new().set_authority(crate::types::BootstrapAuthorityRecord {
        governance_principal: gov,
        binding_version_at_seed: 1,
        seeded_at_ns: 1,
    });

    let chunk0 = b"aaaa";
    let chunk1 = b"bbbb";
    let bad_full_sha = sha256(b"not-the-real-bytes");
    let id = router_artifact_id("0.3.0", bad_full_sha);

    artifact_publish_metadata_with_caller(
        gov,
        crate::types::ArtifactPublishMetadataArgs {
            canister_kind: CanisterKind::Router,
            semantic_version: "0.3.0".to_owned(),
            sha256: bad_full_sha,
            byte_length: 8,
            chunk_hashes: vec![sha256(chunk0), sha256(chunk1)],
        },
        1,
    )
    .unwrap();

    artifact_upload_chunk_with_caller(
        gov,
        ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 0,
            bytes: chunk0.to_vec(),
        },
        2,
    )
    .unwrap();

    let err = artifact_upload_chunk_with_caller(
        gov,
        ArtifactUploadChunkArgs {
            artifact_id: id.clone(),
            chunk_index: 1,
            bytes: chunk1.to_vec(),
        },
        3,
    )
    .unwrap_err();
    assert!(
        matches!(err, ArtifactError::FullSha256Mismatch { artifact_id: ref aid, .. } if *aid == id),
        "expected FullSha256Mismatch, got {:?}",
        err
    );

    let artifact_store = ProvisionArtifactStore::new();
    let upload = artifact_store.get_upload(&id).unwrap();
    assert!(matches!(upload.state, ArtifactUploadState::Failed { .. }));
    assert!(
        artifact_store
            .get_chunk(&ArtifactChunkKey {
                artifact_id: id.clone(),
                chunk_index: 0
            })
            .is_none()
    );
    assert!(
        artifact_store
            .get_chunk(&ArtifactChunkKey {
                artifact_id: id.clone(),
                chunk_index: 1
            })
            .is_none()
    );
}

/// (i) artifact stable layout uses separate MemoryIds for catalog, upload, and chunks.
#[test]
fn artifact_stable_layout_uses_separate_memory_ids() {
    super::reset_all_maps();
    use crate::stable::memory::{
        StableArtifactCatalogMap, StableArtifactChunksMap, StableArtifactUploadMap,
        init_artifact_catalog, init_artifact_chunks, init_artifact_upload,
    };

    let mut catalog: StableArtifactCatalogMap = init_artifact_catalog();
    let mut upload: StableArtifactUploadMap = init_artifact_upload();
    let mut chunks: StableArtifactChunksMap = init_artifact_chunks();

    let id_a = router_artifact_id("1.0.0", sha256(b"a"));
    let id_b = router_artifact_id("1.1.0", sha256(b"b"));
    catalog.insert(id_a.clone(), artifact_metadata(id_a.clone(), 1, vec![]));
    upload.insert(
        id_b.clone(),
        crate::types::ArtifactUpload {
            artifact_id: id_b.clone(),
            state: ArtifactUploadState::Receiving,
            received_chunks: std::collections::BTreeSet::new(),
            started_at_ns: 1,
            verified_at_ns: None,
        },
    );
    chunks.insert(
        ArtifactChunkKey {
            artifact_id: id_a.clone(),
            chunk_index: 0,
        },
        ArtifactChunk { bytes: vec![0xAB] },
    );

    // Cross-region reads must not corrupt each other (R1).
    assert!(catalog.get(&id_a).is_some());
    assert!(upload.get(&id_b).is_some());
    assert!(
        chunks
            .get(&ArtifactChunkKey {
                artifact_id: id_a.clone(),
                chunk_index: 0
            })
            .is_some()
    );
    assert!(catalog.get(&id_b).is_none());
    assert!(upload.get(&id_a).is_none());
    assert!(
        chunks
            .get(&ArtifactChunkKey {
                artifact_id: id_b,
                chunk_index: 0
            })
            .is_none()
    );
}

/// (j) Storable round-trip for all stable artifact key/value types.
#[test]
fn artifact_metadata_round_trip_stable_encoding() {
    use ic_stable_structures::Storable;

    let id = router_artifact_id("2.0.0", sha256(b"rt"));
    let metadata = artifact_metadata(id.clone(), 12, vec![sha256(b"c0"), sha256(b"c1")]);
    let decoded = ArtifactMetadata::from_bytes(metadata.into_bytes().into());
    assert_eq!(decoded.artifact_id, id);
    assert_eq!(decoded.byte_length, 12);
    assert_eq!(decoded.chunk_hashes.len(), 2);

    let upload = crate::types::ArtifactUpload {
        artifact_id: id.clone(),
        state: ArtifactUploadState::Receiving,
        received_chunks: [0u32, 1].into_iter().collect(),
        started_at_ns: 5,
        verified_at_ns: Some(10),
    };
    let upload_decoded = crate::types::ArtifactUpload::from_bytes(upload.into_bytes().into());
    assert_eq!(upload_decoded.artifact_id, id);
    assert_eq!(upload_decoded.received_chunks.len(), 2);

    let chunk = ArtifactChunk {
        bytes: vec![1, 2, 3, 4],
    };
    let chunk_decoded = ArtifactChunk::from_bytes(chunk.into_bytes().into());
    assert_eq!(chunk_decoded.bytes, vec![1, 2, 3, 4]);

    let id_decoded = ArtifactId::from_bytes(id.clone().into_bytes().into());
    assert_eq!(id_decoded, id);

    let chunk_key = ArtifactChunkKey {
        artifact_id: id.clone(),
        chunk_index: 7,
    };
    let chunk_key_decoded = ArtifactChunkKey::from_bytes(chunk_key.into_bytes().into());
    assert_eq!(chunk_key_decoded.artifact_id, id);
    assert_eq!(chunk_key_decoded.chunk_index, 7);
}

// === Release manifest + active release tests (Plan 0061b) ================

fn release_test_principal() -> Principal {
    Principal::from_slice(&[1; 29])
}

fn release_seed_bootstrap() {
    crate::stable::bootstrap_auth::ProvisionBootstrapAuthStore::new().set_authority(
        crate::types::BootstrapAuthorityRecord {
            governance_principal: release_test_principal(),
            binding_version_at_seed: 1,
            seeded_at_ns: 1,
        },
    );
}

fn mk_artifact_id(kind: CanisterKind, version: &str, full_sha: [u8; 32]) -> ArtifactId {
    ArtifactId::new(kind, version.to_owned(), full_sha)
}

fn publish_verified_artifact(kind: CanisterKind, version: &str, chunks: Vec<&[u8]>) -> ArtifactId {
    let full: Vec<u8> = chunks.iter().flat_map(|c| c.iter().copied()).collect();
    let full_sha = sha256(&full);
    let chunk_hashes: Vec<[u8; 32]> = chunks.iter().map(|c| sha256(c)).collect();
    let id = mk_artifact_id(kind.clone(), version, full_sha);

    artifact_publish_metadata_with_caller(
        release_test_principal(),
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
            release_test_principal(),
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

fn mk_release_id(name: &str) -> ReleaseId {
    ReleaseId(name.to_owned())
}

/// (a) release_publish installs an immutable manifest.
#[test]
fn release_publish_installs_immutable_manifest() {
    super::reset_all_maps();
    release_seed_bootstrap();

    let r = mk_release_id("release-a");
    let ids = vec![
        publish_verified_artifact(CanisterKind::Router, "0.1.0", vec![b"r0"]),
        publish_verified_artifact(CanisterKind::Graph, "0.1.0", vec![b"g0"]),
        publish_verified_artifact(CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]),
        publish_verified_artifact(CanisterKind::VectorIndex, "0.1.0", vec![b"v0"]),
    ];

    let result = release_publish_with_caller(
        release_test_principal(),
        ReleasePublishArgs {
            release_id: r.clone(),
            artifact_ids: ids.clone(),
        },
        100,
    )
    .unwrap();

    assert_eq!(result.release_id, r);
    assert_eq!(result.router_artifact, ids[0]);
    assert_eq!(result.graph_artifact, ids[1]);
    assert_eq!(result.property_index_artifact, ids[2]);
    assert_eq!(result.vector_index_artifact, ids[3]);

    let stored = ProvisionReleaseStore::new().get_manifest(&r).unwrap();
    assert_eq!(stored.release_id, r);
    assert_eq!(stored.router_artifact, ids[0]);
}

/// (b) release_publish rejects a conflicting publish of the same ReleaseId.
#[test]
fn release_publish_rejects_conflicting_publish() {
    super::reset_all_maps();
    release_seed_bootstrap();

    let r = mk_release_id("release-b");
    let ids = vec![
        publish_verified_artifact(CanisterKind::Router, "0.1.0", vec![b"r0"]),
        publish_verified_artifact(CanisterKind::Graph, "0.1.0", vec![b"g0"]),
        publish_verified_artifact(CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]),
        publish_verified_artifact(CanisterKind::VectorIndex, "0.1.0", vec![b"v0"]),
    ];

    release_publish_with_caller(
        release_test_principal(),
        ReleasePublishArgs {
            release_id: r.clone(),
            artifact_ids: ids.clone(),
        },
        100,
    )
    .unwrap();

    let second = release_publish_with_caller(
        release_test_principal(),
        ReleasePublishArgs {
            release_id: r.clone(),
            artifact_ids: ids,
        },
        101,
    )
    .unwrap_err();

    assert!(
        matches!(
            second,
            ReleaseError::ConflictingRelease { ref existing, ref requested }
            if *existing == r && *requested == r
        ),
        "expected ConflictingRelease, got {:?}",
        second
    );
}

/// (c) release_publish rejects an incomplete manifest (not exactly 4 ArtifactIds).
#[test]
fn release_publish_rejects_incomplete_manifest() {
    super::reset_all_maps();
    release_seed_bootstrap();

    let r = mk_release_id("release-c");
    let ids = vec![
        publish_verified_artifact(CanisterKind::Router, "0.1.0", vec![b"r0"]),
        publish_verified_artifact(CanisterKind::Graph, "0.1.0", vec![b"g0"]),
        publish_verified_artifact(CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]),
    ];

    let err = release_publish_with_caller(
        release_test_principal(),
        ReleasePublishArgs {
            release_id: r.clone(),
            artifact_ids: ids,
        },
        100,
    )
    .unwrap_err();

    assert!(
        matches!(err, ReleaseError::IncompleteManifest { release_id: ref rid, .. } if *rid == r),
        "expected IncompleteManifest, got {:?}",
        err
    );
}

/// (d) release_publish rejects non-unique per kind.
#[test]
fn release_publish_rejects_non_unique_per_kind() {
    super::reset_all_maps();
    release_seed_bootstrap();

    let router_a = publish_verified_artifact(CanisterKind::Router, "0.1.0", vec![b"ra"]);
    let router_b = publish_verified_artifact(CanisterKind::Router, "0.2.0", vec![b"rb"]);
    let graph = publish_verified_artifact(CanisterKind::Graph, "0.1.0", vec![b"g0"]);
    let prop = publish_verified_artifact(CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]);

    let r = mk_release_id("release-d");
    let err = release_publish_with_caller(
        release_test_principal(),
        ReleasePublishArgs {
            release_id: r.clone(),
            artifact_ids: vec![router_a.clone(), router_b.clone(), graph, prop],
        },
        100,
    )
    .unwrap_err();

    assert!(
        matches!(
            err,
            ReleaseError::NotUniquePerKind {
                release_id: ref rid,
                kind: CanisterKind::Router,
                ref conflicting
            }
            if *rid == r && conflicting.len() == 2
        ),
        "expected NotUniquePerKind Router, got {:?}",
        err
    );
}

/// (e) release_publish rejects an unknown artifact reference.
#[test]
fn release_publish_rejects_unknown_artifact() {
    super::reset_all_maps();
    release_seed_bootstrap();

    let known = vec![
        publish_verified_artifact(CanisterKind::Router, "0.1.0", vec![b"r0"]),
        publish_verified_artifact(CanisterKind::Graph, "0.1.0", vec![b"g0"]),
        publish_verified_artifact(CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]),
    ];
    let unknown = mk_artifact_id(CanisterKind::VectorIndex, "0.9.0", sha256(b"missing"));

    let r = mk_release_id("release-e");
    let err = release_publish_with_caller(
        release_test_principal(),
        ReleasePublishArgs {
            release_id: r.clone(),
            artifact_ids: [known, vec![unknown.clone()]].concat(),
        },
        100,
    )
    .unwrap_err();

    assert!(
        matches!(err, ReleaseError::ArtifactNotFound(ref aid) if *aid == unknown),
        "expected ArtifactNotFound, got {:?}",
        err
    );
}

/// (f) release_activate atomically swaps the active pointer and preserves existing jobs.
#[test]
fn release_activate_atomically_swaps_pointer_and_preserves_jobs() {
    super::reset_all_maps();
    release_seed_bootstrap();

    // Seed a pre-existing job in the job regions (R9 non-retroactivity witness).
    let job_store = ProvisionJobStore::new();
    let pre_job = test_record("req-f", "dep-f", "fp-f");
    job_store.insert_or_idempotent(pre_job.clone()).unwrap();

    let r1 = mk_release_id("release-f1");
    let r2 = mk_release_id("release-f2");
    let ids1 = vec![
        publish_verified_artifact(CanisterKind::Router, "0.1.0", vec![b"r0"]),
        publish_verified_artifact(CanisterKind::Graph, "0.1.0", vec![b"g0"]),
        publish_verified_artifact(CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]),
        publish_verified_artifact(CanisterKind::VectorIndex, "0.1.0", vec![b"v0"]),
    ];
    let ids2 = vec![
        publish_verified_artifact(CanisterKind::Router, "0.2.0", vec![b"r1"]),
        publish_verified_artifact(CanisterKind::Graph, "0.2.0", vec![b"g1"]),
        publish_verified_artifact(CanisterKind::PropertyIndex, "0.2.0", vec![b"p1"]),
        publish_verified_artifact(CanisterKind::VectorIndex, "0.2.0", vec![b"v1"]),
    ];

    release_publish_with_caller(
        release_test_principal(),
        ReleasePublishArgs {
            release_id: r1.clone(),
            artifact_ids: ids1,
        },
        100,
    )
    .unwrap();
    release_publish_with_caller(
        release_test_principal(),
        ReleasePublishArgs {
            release_id: r2.clone(),
            artifact_ids: ids2,
        },
        101,
    )
    .unwrap();

    let first = release_activate_with_caller(
        release_test_principal(),
        ReleaseActivateArgs {
            release_id: r1.clone(),
        },
        200,
    )
    .unwrap();
    assert_eq!(first.release_id, r1);
    assert_eq!(first.previous_release_id, None);

    let second = release_activate_with_caller(
        release_test_principal(),
        ReleaseActivateArgs {
            release_id: r2.clone(),
        },
        201,
    )
    .unwrap();
    assert_eq!(second.release_id, r2);
    assert_eq!(second.previous_release_id, Some(r1.clone()));

    // Non-retroactivity: the pre-existing job is untouched.
    assert_eq!(job_store.get_by_request("req-f", "dep-f"), Some(pre_job));
}

/// (g) release_activate rejects an unverified artifact and leaves the active pointer unchanged.
#[test]
fn release_activate_rejects_unverified_artifact() {
    super::reset_all_maps();
    release_seed_bootstrap();

    let r = mk_release_id("release-g");
    let router = publish_verified_artifact(CanisterKind::Router, "0.1.0", vec![b"r0"]);
    let graph = publish_verified_artifact(CanisterKind::Graph, "0.1.0", vec![b"g0"]);
    let prop = publish_verified_artifact(CanisterKind::PropertyIndex, "0.1.0", vec![b"p0"]);
    // Publish metadata but do not upload chunks for the vector artifact.
    let unverified_sha = sha256(b"never-uploaded");
    let unverified = mk_artifact_id(CanisterKind::VectorIndex, "0.9.0", unverified_sha);
    artifact_publish_metadata_with_caller(
        release_test_principal(),
        ArtifactPublishMetadataArgs {
            canister_kind: CanisterKind::VectorIndex,
            semantic_version: "0.9.0".to_owned(),
            sha256: unverified_sha,
            byte_length: 14,
            chunk_hashes: vec![sha256(b"never-uploaded")],
        },
        1,
    )
    .unwrap();

    release_publish_with_caller(
        release_test_principal(),
        ReleasePublishArgs {
            release_id: r.clone(),
            artifact_ids: vec![router, graph, prop, unverified.clone()],
        },
        100,
    )
    .unwrap();

    let err = release_activate_with_caller(
        release_test_principal(),
        ReleaseActivateArgs {
            release_id: r.clone(),
        },
        200,
    )
    .unwrap_err();

    assert!(
        matches!(err, ReleaseError::ArtifactNotVerified(ref aid) if *aid == unverified),
        "expected ArtifactNotVerified, got {:?}",
        err
    );
    assert_eq!(ProvisionReleaseStore::new().get_active(), None);
}

/// (j) release stable layout uses separate MemoryIds for manifest and active pointer.
#[test]
fn release_stable_layout_uses_separate_memory_ids() {
    super::reset_all_maps();

    let mut manifest_map: StableReleaseManifestMap = init_release_manifest();
    let mut active_cell: StableActiveReleaseCell = init_active_release();

    let r = mk_release_id("release-j");
    manifest_map.insert(
        r.clone(),
        ReleaseManifest {
            release_id: r.clone(),
            router_artifact: mk_artifact_id(CanisterKind::Router, "0.0.0", sha256(b"j-r")),
            graph_artifact: mk_artifact_id(CanisterKind::Graph, "0.0.0", sha256(b"j-g")),
            property_index_artifact: mk_artifact_id(
                CanisterKind::PropertyIndex,
                "0.0.0",
                sha256(b"j-p"),
            ),
            vector_index_artifact: mk_artifact_id(
                CanisterKind::VectorIndex,
                "0.0.0",
                sha256(b"j-v"),
            ),
        },
    );
    active_cell.set(Some(r.clone()));

    assert_eq!(manifest_map.get(&r).unwrap().release_id, r);
    assert_eq!(active_cell.get().clone(), Some(r));
}

/// (a) artifact audit append assigns monotonic sequence per principal.
#[test]
fn artifact_audit_append_assigns_monotonic_sequence() {
    super::reset_all_maps();
    let store = ProvisionArtifactStore::new();
    let p = test_principal(1);
    for i in 0..3 {
        store.append_audit_entry(ArtifactAuditEntry {
            caller: p,
            action: ArtifactAuditAction::PublishArtifact,
            artifact_id: None,
            release_id: None,
            deployment_id: None,
            target_canister: None,
            timestamp_ns: 100 + i as u64,
            outcome: ArtifactAuditOutcome::Success,
            reason: None,
        });
    }
    let history = store.audit_history(p);
    assert_eq!(history.len(), 3);
    assert_eq!(history[0].timestamp_ns, 100);
    assert_eq!(history[1].timestamp_ns, 101);
    assert_eq!(history[2].timestamp_ns, 102);
}

/// (b) artifact audit append uses independent per-principal counters.
#[test]
fn artifact_audit_append_uses_independent_per_principal_counter() {
    super::reset_all_maps();
    let store = ProvisionArtifactStore::new();
    let a = test_principal(1);
    let b = test_principal(2);
    for p in [a, b] {
        for i in 0..2 {
            store.append_audit_entry(ArtifactAuditEntry {
                caller: p,
                action: ArtifactAuditAction::UploadChunk,
                artifact_id: None,
                release_id: None,
                deployment_id: None,
                target_canister: None,
                timestamp_ns: 200 + i as u64,
                outcome: ArtifactAuditOutcome::Success,
                reason: None,
            });
        }
    }
    let history_a = store.audit_history(a);
    let history_b = store.audit_history(b);
    assert_eq!(history_a.len(), 2);
    assert_eq!(history_b.len(), 2);
    assert_eq!(history_a[0].timestamp_ns, 200);
    assert_eq!(history_a[1].timestamp_ns, 201);
    assert_eq!(history_b[0].timestamp_ns, 200);
    assert_eq!(history_b[1].timestamp_ns, 201);
}

/// (c) artifact audit cap enforces LRU eviction by sequence (R5 strict).
#[test]
fn artifact_audit_cap_enforces_eviction() {
    use crate::stable::artifact::ARTIFACT_AUDIT_LOG_PER_PRINCIPAL_CAP;
    super::reset_all_maps();
    let store = ProvisionArtifactStore::new();
    let p = test_principal(3);
    let cap = ARTIFACT_AUDIT_LOG_PER_PRINCIPAL_CAP;
    for i in 0..=cap {
        store.append_audit_entry(ArtifactAuditEntry {
            caller: p,
            action: ArtifactAuditAction::PublishRelease,
            artifact_id: None,
            release_id: Some(ReleaseId(format!("r-{}", i))),
            deployment_id: None,
            target_canister: None,
            timestamp_ns: 300 + i as u64,
            outcome: ArtifactAuditOutcome::Success,
            reason: None,
        });
    }
    let history = store.audit_history(p);
    assert_eq!(history.len(), cap, "oldest entry must be evicted");
    assert_eq!(history[0].release_id, Some(ReleaseId("r-1".to_owned())));
    assert_eq!(
        history[cap - 1].release_id,
        Some(ReleaseId(format!("r-{}", cap)))
    );
}

/// (d) artifact audit history returns bounded range in sequence order.
#[test]
fn artifact_audit_history_returns_bounded_range() {
    super::reset_all_maps();
    let store = ProvisionArtifactStore::new();
    let p = test_principal(4);
    for i in 0..10 {
        store.append_audit_entry(ArtifactAuditEntry {
            caller: p,
            action: ArtifactAuditAction::VerifyArtifact,
            artifact_id: None,
            release_id: None,
            deployment_id: None,
            target_canister: None,
            timestamp_ns: 400 + i as u64,
            outcome: ArtifactAuditOutcome::Success,
            reason: None,
        });
    }
    let history = store.audit_history(p);
    assert_eq!(history.len(), 10);
    for (i, entry) in history.iter().enumerate() {
        assert_eq!(entry.timestamp_ns, 400 + i as u64);
    }
}

/// (i) artifact audit stable layout uses separate MemoryId 11 (R1 carryover).
#[test]
fn artifact_audit_stable_layout_uses_separate_memory_id() {
    super::reset_all_maps();
    let mut audit_map: StableArtifactAuditLogMap = init_artifact_audit_log();
    let mut catalog_map = crate::stable::memory::init_artifact_catalog();

    let p = test_principal(5);
    let entry = ArtifactAuditEntry {
        caller: p,
        action: ArtifactAuditAction::InstallRelease,
        artifact_id: None,
        release_id: Some(ReleaseId("r-i".to_owned())),
        deployment_id: None,
        target_canister: None,
        timestamp_ns: 500,
        outcome: ArtifactAuditOutcome::Success,
        reason: None,
    };
    audit_map.insert((p, 0), entry.clone());

    let artifact_id = mk_artifact_id(CanisterKind::Router, "0.0.0", sha256(b"i"));
    let metadata = ArtifactMetadata {
        artifact_id: artifact_id.clone(),
        byte_length: 1,
        chunk_hashes: vec![sha256(b"i")],
        created_at_ns: 501,
    };
    catalog_map.insert(artifact_id.clone(), metadata.clone());

    assert_eq!(audit_map.get(&(p, 0)).unwrap(), entry);
    assert_eq!(catalog_map.get(&artifact_id).unwrap(), metadata);
}

/// (j) Storable round-trip for artifact audit types (R10 carryover).
#[test]
fn artifact_audit_round_trip_stable_encoding() {
    let entry = ArtifactAuditEntry {
        caller: test_principal(7),
        action: ArtifactAuditAction::ActivateRelease,
        artifact_id: Some(mk_artifact_id(CanisterKind::Graph, "0.1.0", sha256(b"j"))),
        release_id: Some(ReleaseId("r-j".to_owned())),
        deployment_id: Some("dep-j".to_owned()),
        target_canister: Some(test_principal(8)),
        timestamp_ns: 600,
        outcome: ArtifactAuditOutcome::Rejected,
        reason: Some("round-trip reason".to_owned()),
    };
    let encoded = entry.to_bytes();
    let decoded = ArtifactAuditEntry::from_bytes(encoded);
    assert_eq!(decoded, entry);

    for action in [
        ArtifactAuditAction::PublishArtifact,
        ArtifactAuditAction::UploadChunk,
        ArtifactAuditAction::VerifyArtifact,
        ArtifactAuditAction::PublishRelease,
        ArtifactAuditAction::ActivateRelease,
        ArtifactAuditAction::InstallRelease,
    ] {
        let encoded = action.to_bytes();
        assert_eq!(ArtifactAuditAction::from_bytes(encoded), action);
    }

    for outcome in [
        ArtifactAuditOutcome::Success,
        ArtifactAuditOutcome::Rejected,
        ArtifactAuditOutcome::Failed,
    ] {
        let encoded = outcome.to_bytes();
        assert_eq!(ArtifactAuditOutcome::from_bytes(encoded), outcome);
    }
}
