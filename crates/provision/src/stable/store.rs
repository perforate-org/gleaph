//! Provision canister stable-memory store facades.

#[cfg(test)]
use super::artifact::reset_artifact_maps;
#[cfg(test)]
use super::bootstrap_auth::reset_bootstrap_auth_maps;
use super::memory::{
    StableDeploymentTrustMap, StableJobByDeploymentMap, StableJobByRequestMap,
    StableJobIntentLockMap, init_deployment_trust, init_job_by_deployment, init_job_by_request,
    init_job_intent_lock,
};
use crate::types::{
    DeploymentBinding, JobState, ProvisionIntentLockMarker, ProvisionJobRecord,
    ProvisionJobRequestKey, ProvisioningIntentKey, is_legal_transition, is_terminal_state,
};
use candid::Principal;
use std::cell::RefCell;

#[cfg(test)]
mod tests;

thread_local! {
    static DEPLOYMENT_TRUST: RefCell<StableDeploymentTrustMap> =
        RefCell::new(init_deployment_trust());
    static JOB_BY_REQUEST: RefCell<StableJobByRequestMap> =
        RefCell::new(init_job_by_request());
    static JOB_BY_DEPLOYMENT: RefCell<StableJobByDeploymentMap> =
        RefCell::new(init_job_by_deployment());
    static INTENT_LOCK: RefCell<StableJobIntentLockMap> =
        RefCell::new(init_job_intent_lock());
}

/// Test-only helper to clear all provision stable maps. Must be called at the start of any test
/// that mutates stable state to avoid thread-local interference from other tests on the same thread.
#[cfg(test)]
pub(crate) fn reset_all_maps() {
    DEPLOYMENT_TRUST.with_borrow_mut(|map| map.clear_new());
    JOB_BY_REQUEST.with_borrow_mut(|map| map.clear_new());
    JOB_BY_DEPLOYMENT.with_borrow_mut(|map| map.clear_new());
    INTENT_LOCK.with_borrow_mut(|map| map.clear_new());
    reset_bootstrap_auth_maps();
    reset_artifact_maps();
    set_force_advance_error(false);
}

/// Deployment trust binding store (stable region 0).
#[derive(Clone, Copy, Debug, Default)]
pub struct DeploymentTrustStore;

impl DeploymentTrustStore {
    pub fn new() -> Self {
        Self
    }

    pub fn get(&self, deployment_id: &str) -> Option<DeploymentBinding> {
        DEPLOYMENT_TRUST.with_borrow(|map| map.get(&deployment_id.to_owned()))
    }

    /// Governance-agnostic install or overwrite. Used by the admin_install handler after
    /// it has already authorized the caller. Never panics and never checks governance.
    pub fn admin_upsert(&self, binding: DeploymentBinding) -> DeploymentBinding {
        DEPLOYMENT_TRUST.with_borrow_mut(|map| {
            map.insert(binding.deployment_id.clone(), binding.clone());
        });
        binding
    }

    /// Idempotent bootstrap install. Panics on mismatching binding for the same deployment_id.
    pub fn get_or_install(&self, binding: DeploymentBinding) -> DeploymentBinding {
        DEPLOYMENT_TRUST.with_borrow_mut(|map| match map.get(&binding.deployment_id) {
            Some(existing) => {
                if existing.router_principal != binding.router_principal
                    || existing.governance_principal != binding.governance_principal
                    || existing.binding_version != binding.binding_version
                {
                    panic!(
                        "DeploymentBinding mismatch for deployment_id={}",
                        binding.deployment_id
                    );
                }
                existing
            }
            None => {
                map.insert(binding.deployment_id.clone(), binding.clone());
                binding
            }
        })
    }

    /// Governance-only update. Returns NotFound if no binding exists, NotAuthorized if the caller
    /// is not the stored governance principal.
    pub fn update(
        &self,
        caller: Principal,
        binding: DeploymentBinding,
    ) -> Result<(), TrustUpdateError> {
        DEPLOYMENT_TRUST.with_borrow_mut(|map| {
            let existing = map
                .get(&binding.deployment_id)
                .ok_or(TrustUpdateError::NotFound)?;
            if existing.governance_principal != caller {
                return Err(TrustUpdateError::NotAuthorized);
            }
            map.insert(binding.deployment_id.clone(), binding);
            Ok(())
        })
    }
}

/// Durable job/receipt store (stable regions 1–3).
#[derive(Clone, Copy, Debug, Default)]
pub struct ProvisionJobStore;

impl ProvisionJobStore {
    pub fn new() -> Self {
        Self
    }

    /// Insert a job record idempotently. Same request_id + same fingerprint returns the existing
    /// record. Same request_id + different fingerprint returns Conflict.
    pub fn insert_or_idempotent(
        &self,
        record: ProvisionJobRecord,
    ) -> Result<ProvisionJobRecord, JobInsertError> {
        let key = ProvisionJobRequestKey::new(&record.request_id, &record.deployment_id);
        let existing = JOB_BY_REQUEST.with_borrow(|map| map.get(&key));
        if let Some(existing) = existing {
            if existing.request_fingerprint != record.request_fingerprint {
                return Err(JobInsertError::Conflict);
            }
            return Ok(existing);
        }
        JOB_BY_REQUEST.with_borrow_mut(|map| map.insert(key, record.clone()));
        JOB_BY_DEPLOYMENT.with_borrow_mut(|map| {
            for resource in &record.resources {
                let intent_key = ProvisioningIntentKey {
                    deployment_id: record.deployment_id.clone(),
                    resource_kind: resource.resource_kind,
                    logical_resource_key: resource.logical_resource_key.clone(),
                };
                map.insert(
                    intent_key,
                    ProvisionJobRequestKey::new(&record.request_id, &record.deployment_id),
                );
            }
        });
        Ok(record)
    }

    /// Preflight all lock conflicts, then in one block co-write Map 1, Map 2, and Map 3.
    /// Same-key same-fingerprint returns the existing record without any write.
    /// Same-key different-fingerprint returns Conflict. Any held intent returns
    /// IntentLockHeld before any canonical or derived mutation.
    pub fn insert_with_intent_locks(
        &self,
        record: ProvisionJobRecord,
        now_ns: u64,
    ) -> Result<InsertWithLocksOutcome, InsertWithLocksError> {
        let key = ProvisionJobRequestKey::new(&record.request_id, &record.deployment_id);

        // 1. Idempotency / conflict pre-check (read-only so far).
        let existing = JOB_BY_REQUEST.with_borrow(|map| map.get(&key));
        if let Some(existing) = existing {
            if existing.request_fingerprint != record.request_fingerprint {
                return Err(InsertWithLocksError::Conflict);
            }
            return Ok(InsertWithLocksOutcome::IdempotentReplay(existing));
        }

        // 2. Lock preflight (read-only so far).
        for resource in &record.resources {
            let intent_key = ProvisioningIntentKey {
                deployment_id: record.deployment_id.clone(),
                resource_kind: resource.resource_kind,
                logical_resource_key: resource.logical_resource_key.clone(),
            };
            if INTENT_LOCK.with_borrow(|map| map.contains_key(&intent_key)) {
                return Err(InsertWithLocksError::IntentLockHeld);
            }
        }

        // 3. Co-write canonical, derived, and lock markers in one block.
        JOB_BY_REQUEST.with_borrow_mut(|map| {
            map.insert(key.clone(), record.clone());
        });
        JOB_BY_DEPLOYMENT.with_borrow_mut(|map| {
            for resource in &record.resources {
                let intent_key = ProvisioningIntentKey {
                    deployment_id: record.deployment_id.clone(),
                    resource_kind: resource.resource_kind,
                    logical_resource_key: resource.logical_resource_key.clone(),
                };
                map.insert(intent_key, key.clone());
            }
        });
        INTENT_LOCK.with_borrow_mut(|map| {
            for resource in &record.resources {
                let intent_key = ProvisioningIntentKey {
                    deployment_id: record.deployment_id.clone(),
                    resource_kind: resource.resource_kind,
                    logical_resource_key: resource.logical_resource_key.clone(),
                };
                map.insert(intent_key, ProvisionIntentLockMarker);
            }
        });

        // 4. Advance the fresh record to Reserved inside the same logical boundary.
        let mut advanced = record.clone();
        advanced.current_state = JobState::Reserved;
        advanced.last_transition_ns = now_ns;
        JOB_BY_REQUEST.with_borrow_mut(|map| map.insert(key, advanced.clone()));

        Ok(InsertWithLocksOutcome::InsertedFresh(advanced))
    }

    pub fn get_by_request(
        &self,
        request_id: &str,
        deployment_id: &str,
    ) -> Option<ProvisionJobRecord> {
        JOB_BY_REQUEST
            .with_borrow(|map| map.get(&ProvisionJobRequestKey::new(request_id, deployment_id)))
    }

    pub fn get_by_request_key(&self, key: &ProvisionJobRequestKey) -> Option<ProvisionJobRecord> {
        JOB_BY_REQUEST.with_borrow(|map| map.get(key))
    }

    /// Advance the job state. Caller supplies `now_ns` (matches router pattern; tests pass an
    /// explicit value). `active_resource_index` is written only when `Some`.
    pub fn advance_state(
        &self,
        key: &ProvisionJobRequestKey,
        next: JobState,
        active_resource_index: Option<usize>,
        now_ns: u64,
    ) -> Result<(), JobAdvanceError> {
        use JobState::*;
        JOB_BY_REQUEST.with_borrow_mut(|map| {
            let mut record = map.get(key).ok_or(JobAdvanceError::NotFound)?;
            #[cfg(test)]
            {
                let force_error = FORCE_ADVANCE_ERROR.with_borrow_mut(|f| {
                    let old = *f;
                    *f = false;
                    old
                });
                if force_error {
                    return Err(JobAdvanceError::InvalidTransition);
                }
            }
            if !is_legal_transition(&record.current_state, &next) {
                return Err(JobAdvanceError::InvalidTransition);
            }
            record.current_state = next.clone();
            record.last_transition_ns = now_ns;
            if let Some(idx) = active_resource_index {
                record.active_resource_index = idx;
            }
            // completed_effect_count increment rule (P3-B, provisional):
            // +1 on the three remote-effect commits, +0 otherwise.
            if matches!(
                next,
                CreatePending | InstallPending | RouterRegistrationPending
            ) {
                record.completed_effect_count = record
                    .completed_effect_count
                    .checked_add(1)
                    .expect("completed_effect_count overflow");
            }
            map.insert(key.clone(), record);
            Ok(())
        })
    }

    pub fn set_resource_canister_id(
        &self,
        key: &ProvisionJobRequestKey,
        resource_index: usize,
        canister_id: Principal,
    ) {
        JOB_BY_REQUEST.with_borrow_mut(|map| {
            let mut record = map
                .get(key)
                .expect("set_resource_canister_id: record not found");
            record.resources[resource_index].canister_id = Some(canister_id);
            map.insert(key.clone(), record);
        });
    }

    pub fn intent_locked(&self, lock_key: &ProvisioningIntentKey) -> bool {
        INTENT_LOCK.with_borrow(|map| map.contains_key(lock_key))
    }

    pub fn acquire_intent_lock(&self, lock_key: ProvisioningIntentKey) -> bool {
        INTENT_LOCK.with_borrow_mut(|map| {
            if map.contains_key(&lock_key) {
                return false;
            }
            map.insert(lock_key, ProvisionIntentLockMarker);
            true
        })
    }

    pub fn release_intent_lock(&self, lock_key: &ProvisioningIntentKey) -> bool {
        INTENT_LOCK.with_borrow_mut(|map| map.remove(lock_key).is_some())
    }

    /// Derive one `ProvisioningIntentKey` per resource in `record.resources` and insert markers.
    /// Returns the number acquired. If any intent is already held, roll back all markers acquired
    /// so far and return `Err(IntentLockAcquireError::AlreadyHeld)` (P2-C).
    pub fn acquire_intent_locks_for_record(
        &self,
        record: &ProvisionJobRecord,
    ) -> Result<usize, IntentLockAcquireError> {
        INTENT_LOCK.with_borrow_mut(|map| {
            let mut acquired = Vec::with_capacity(record.resources.len());
            for resource in &record.resources {
                let lock_key = ProvisioningIntentKey {
                    deployment_id: record.deployment_id.clone(),
                    resource_kind: resource.resource_kind,
                    logical_resource_key: resource.logical_resource_key.clone(),
                };
                if map.contains_key(&lock_key) {
                    // Roll back partial acquisitions.
                    for acquired_key in &acquired {
                        map.remove(acquired_key);
                    }
                    return Err(IntentLockAcquireError::AlreadyHeld);
                }
                map.insert(lock_key.clone(), ProvisionIntentLockMarker);
                acquired.push(lock_key);
            }
            Ok(acquired.len())
        })
    }

    /// Release every intent lock derived from `record.resources`. Returns the count released.
    pub fn clear_intent_locks_for_record(&self, record: &ProvisionJobRecord) -> usize {
        INTENT_LOCK.with_borrow_mut(|map| {
            let mut count = 0usize;
            for resource in &record.resources {
                let lock_key = ProvisioningIntentKey {
                    deployment_id: record.deployment_id.clone(),
                    resource_kind: resource.resource_kind,
                    logical_resource_key: resource.logical_resource_key.clone(),
                };
                if map.remove(&lock_key).is_some() {
                    count += 1;
                }
            }
            count
        })
    }

    /// Overwrite the canonical job record without touching the derived index.
    pub fn put(&self, key: &ProvisionJobRequestKey, record: ProvisionJobRecord) {
        JOB_BY_REQUEST.with_borrow_mut(|map| {
            map.insert(key.clone(), record);
        });
    }

    /// Remove the canonical record and sweep every derived Map 2 entry owned by its resources.
    pub fn remove(&self, key: &ProvisionJobRequestKey) -> Option<ProvisionJobRecord> {
        let record = JOB_BY_REQUEST.with_borrow_mut(|map| map.remove(key));
        if let Some(ref record) = record {
            JOB_BY_DEPLOYMENT.with_borrow_mut(|map| {
                for resource in &record.resources {
                    let intent_key = ProvisioningIntentKey {
                        deployment_id: record.deployment_id.clone(),
                        resource_kind: resource.resource_kind,
                        logical_resource_key: resource.logical_resource_key.clone(),
                    };
                    map.remove(&intent_key);
                }
            });
        }
        record
    }

    /// Test-only lookup: return the `ProvisionJobRequestKey` stored in `JOB_BY_DEPLOYMENT`
    /// for the derived intent `(deployment_id, resource_kind, logical_resource_key)`, if any.
    #[cfg(test)]
    pub(crate) fn assert_intent_to_request_for_test(
        &self,
        deployment_id: &str,
        resource_kind: crate::types::ProvisionableResourceKind,
        logical_resource_key: &str,
    ) -> Option<ProvisionJobRequestKey> {
        let intent_key = ProvisioningIntentKey {
            deployment_id: deployment_id.to_owned(),
            resource_kind,
            logical_resource_key: logical_resource_key.to_owned(),
        };
        JOB_BY_DEPLOYMENT.with_borrow(|map| map.get(&intent_key))
    }

    /// Count how many of the intent locks derived from `record.resources` are currently held.
    pub fn intent_lock_count_for_record(&self, record: &ProvisionJobRecord) -> usize {
        INTENT_LOCK.with_borrow(|map| {
            record
                .resources
                .iter()
                .filter(|resource| {
                    let intent_key = ProvisioningIntentKey {
                        deployment_id: record.deployment_id.clone(),
                        resource_kind: resource.resource_kind,
                        logical_resource_key: resource.logical_resource_key.clone(),
                    };
                    map.contains_key(&intent_key)
                })
                .count()
        })
    }

    /// True if any non-terminal job exists for `deployment_id`.
    ///
    /// This is a defensive bootstrap check; it scans the canonical job map because the
    /// primary key is `(request_id, deployment_id)`.
    pub fn has_live_job_for_deployment(&self, deployment_id: &str) -> bool {
        JOB_BY_REQUEST.with_borrow(|map| {
            map.iter().any(|entry| {
                entry.value().deployment_id == deployment_id
                    && !is_terminal_state(&entry.value().current_state)
            })
        })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrustUpdateError {
    NotFound,
    NotAuthorized,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertWithLocksOutcome {
    InsertedFresh(ProvisionJobRecord),
    IdempotentReplay(ProvisionJobRecord),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertWithLocksError {
    Conflict,
    IntentLockHeld,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobInsertError {
    Conflict,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum JobAdvanceError {
    NotFound,
    InvalidTransition,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IntentLockAcquireError {
    AlreadyHeld,
}

#[cfg(test)]
thread_local! {
    static FORCE_ADVANCE_ERROR: RefCell<bool> = const { RefCell::new(false) };
}

#[cfg(test)]
pub(crate) fn set_force_advance_error(force: bool) {
    FORCE_ADVANCE_ERROR.with_borrow_mut(|f| *f = force);
}
