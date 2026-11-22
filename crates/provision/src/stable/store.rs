//! Provision canister stable-memory store facades.

#[cfg(test)]
use super::artifact::reset_artifact_audit_log;
#[cfg(test)]
use super::artifact::reset_artifact_maps;
#[cfg(test)]
use super::bootstrap_auth::reset_bootstrap_auth_maps;
#[cfg(test)]
use super::dict_catalog::reset_dict_catalog_maps;
use super::memory::{
    StableBillingGrantMap, StableDeploymentGrantMap, StableDeploymentRecordMap,
    StableJobByDeploymentMap, StableJobByRequestMap, StableJobIntentLockMap,
    StableResourceDeletionMap, StableServiceConfigCell, StableServiceLedgerCell,
    StableSingletonIssuanceMap, StableTerminationMap, StableWithdrawalMap, init_billing_grants,
    init_deployment_grants, init_deployment_records, init_job_by_deployment, init_job_by_request,
    init_job_intent_lock, init_resource_deletions, init_service_config, init_service_ledger,
    init_singleton_issuance, init_terminations, init_withdrawals,
};
#[cfg(test)]
use super::release::reset_release_maps;
use gleaph_graph_kernel::provisioning::DeploymentId;

use crate::types::{
    AdmissionCharge, BillingGrantKey, BillingGrantRecord, ControllerMode, DeploymentGrantScope,
    DeploymentRecord, GrantCreditError, GrantCreditResult, IssuanceRecord, JobState,
    LogicalResource, ProvisionIntentLockMarker, ProvisionJobRecord, ProvisionJobRequestKey,
    ProvisioningIntentKey, ResourceDeletionRecord, ResourceDeletionState, ReverseCreditError,
    ReverseCreditResult, RouterRegistrationAckResponse, ServiceConfig, ServiceLedger,
    WithdrawalKey, WithdrawalKind, WithdrawalRecord, WithdrawalStatus, is_legal_transition,
    is_terminal_state,
};
use candid::Principal;
use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};

#[cfg(test)]
mod tests;

thread_local! {
    static DEPLOYMENT_GRANTS: RefCell<StableDeploymentGrantMap> =
        RefCell::new(init_deployment_grants());
    static JOB_BY_REQUEST: RefCell<StableJobByRequestMap> =
        RefCell::new(init_job_by_request());
    static JOB_BY_DEPLOYMENT: RefCell<StableJobByDeploymentMap> =
        RefCell::new(init_job_by_deployment());
    static INTENT_LOCK: RefCell<StableJobIntentLockMap> =
        RefCell::new(init_job_intent_lock());
    static SINGLETON_ISSUANCE: RefCell<StableSingletonIssuanceMap> =
        RefCell::new(init_singleton_issuance());
    static DEPLOYMENT_RECORDS: RefCell<StableDeploymentRecordMap> =
        RefCell::new(init_deployment_records());
    static SERVICE_CONFIG: RefCell<StableServiceConfigCell> =
        RefCell::new(init_service_config());
    static SERVICE_LEDGER: RefCell<StableServiceLedgerCell> =
        RefCell::new(init_service_ledger());
    static BILLING_GRANTS: RefCell<StableBillingGrantMap> = RefCell::new(init_billing_grants());
    static WITHDRAWALS: RefCell<StableWithdrawalMap> = RefCell::new(init_withdrawals());
    static TERMINATIONS: RefCell<StableTerminationMap> = RefCell::new(init_terminations());
    static RESOURCE_DELETIONS: RefCell<StableResourceDeletionMap> =
        RefCell::new(init_resource_deletions());
}

/// Test-only helper to clear all provision stable maps. Must be called at the start of any test
/// that mutates stable state to avoid thread-local interference from other tests on the same thread.
#[cfg(test)]
pub(crate) fn reset_all_maps() {
    DEPLOYMENT_GRANTS.with_borrow_mut(|grants| grants.clear_new());
    JOB_BY_REQUEST.with_borrow_mut(|map| map.clear_new());
    JOB_BY_DEPLOYMENT.with_borrow_mut(|map| map.clear_new());
    INTENT_LOCK.with_borrow_mut(|map| map.clear_new());
    SINGLETON_ISSUANCE.with_borrow_mut(|map| map.clear_new());
    DEPLOYMENT_RECORDS.with_borrow_mut(|map| map.clear_new());
    SERVICE_CONFIG.with_borrow_mut(|cell| {
        let _ = cell.set(ServiceConfig::default());
    });
    SERVICE_LEDGER.with_borrow_mut(|cell| {
        let _ = cell.set(ServiceLedger::default());
    });
    BILLING_GRANTS.with_borrow_mut(|map| map.clear_new());
    WITHDRAWALS.with_borrow_mut(|map| map.clear_new());
    TERMINATIONS.with_borrow_mut(|map| map.clear_new());
    RESOURCE_DELETIONS.with_borrow_mut(|map| map.clear_new());
    reset_bootstrap_auth_maps();
    reset_artifact_maps();
    reset_release_maps();
    reset_artifact_audit_log();
    reset_dict_catalog_maps();
    set_force_advance_error(false);
}

/// Re-open the canonical job map and both derived intent regions over their existing stable
/// memories, mirroring a canister upgrade without clearing any persisted rows.
#[cfg(test)]
pub(crate) fn reopen_provisioning_regions_for_test() {
    DEPLOYMENT_GRANTS.with(|slot| slot.replace(init_deployment_grants()));
    JOB_BY_REQUEST.with(|slot| slot.replace(init_job_by_request()));
    JOB_BY_DEPLOYMENT.with(|slot| slot.replace(init_job_by_deployment()));
    INTENT_LOCK.with(|slot| slot.replace(init_job_intent_lock()));
    SINGLETON_ISSUANCE.with(|slot| slot.replace(init_singleton_issuance()));
    DEPLOYMENT_RECORDS.with(|slot| slot.replace(init_deployment_records()));
    SERVICE_CONFIG.with(|slot| slot.replace(init_service_config()));
    SERVICE_LEDGER.with(|slot| slot.replace(init_service_ledger()));
    BILLING_GRANTS.with(|slot| slot.replace(init_billing_grants()));
    WITHDRAWALS.with(|slot| slot.replace(init_withdrawals()));
    TERMINATIONS.with(|slot| slot.replace(init_terminations()));
    RESOURCE_DELETIONS.with(|slot| slot.replace(init_resource_deletions()));
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WithdrawalReservation {
    Reserved,
    Replayed(u128),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum WithdrawalStoreError {
    UnknownWithdrawal,
    NotPending,
    Pending,
    AmountZero,
    InsufficientLedger,
    LedgerOverflow,
    IdempotencyConflict,
    PreviousAttemptFailed,
}

/// Deployment grant store (stable region 0): each authorized issuer and the deployment
/// scope it may name. `OwnDeployment` is the Router case; `Relay` is the Account case.
#[derive(Clone, Copy, Debug, Default)]
pub struct DeploymentGrantStore;

impl DeploymentGrantStore {
    pub fn new() -> Self {
        Self
    }

    /// True if `issuer` is authorized to request issuance.
    pub fn contains(&self, issuer: &Principal) -> bool {
        DEPLOYMENT_GRANTS.with_borrow(|grants| grants.contains_key(issuer))
    }

    /// The scope granted to `issuer`, if authorized.
    pub fn scope(&self, issuer: &Principal) -> Option<DeploymentGrantScope> {
        DEPLOYMENT_GRANTS.with_borrow(|grants| grants.get(issuer))
    }

    /// Authorize `issuer` for its own deployment. Idempotent.
    pub fn insert(&self, issuer: Principal) {
        self.insert_with_scope(issuer, DeploymentGrantScope::OwnDeployment);
    }

    /// Authorize `issuer` with an explicit scope. Idempotent and rewrites the scope.
    pub fn insert_with_scope(&self, issuer: Principal, scope: DeploymentGrantScope) {
        DEPLOYMENT_GRANTS.with_borrow_mut(|grants| {
            grants.insert(issuer, scope);
        });
    }

    /// Revoke `issuer`. Returns true if the grant was present.
    pub fn remove(&self, issuer: &Principal) -> bool {
        DEPLOYMENT_GRANTS.with_borrow_mut(|grants| grants.remove(issuer).is_some())
    }
}

/// Durable job/receipt store (stable regions 1–3).
#[derive(Clone, Copy, Debug, Default)]
pub struct ProvisionJobStore;

/// Test-only snapshot of the three provisioning maps, captured for before/after
/// comparisons in canister tests.
#[cfg(test)]
type ProvisioningMapsSnapshot = (
    Vec<(ProvisionJobRequestKey, ProvisionJobRecord)>,
    Vec<(ProvisioningIntentKey, ProvisionJobRequestKey)>,
    Vec<ProvisioningIntentKey>,
);

/// Intent keys whose resource has one lifetime issuance. Router is the only one today; the
/// ledger is keyed generically so a future singleton does not need a second table.
fn singleton_intent_keys(
    record: &ProvisionJobRecord,
) -> impl Iterator<Item = ProvisioningIntentKey> + '_ {
    record
        .resources
        .iter()
        .filter(|resource| resource.logical_resource == LogicalResource::Router)
        .map(|resource| ProvisioningIntentKey {
            deployment_id: record.deployment_id,
            logical_resource: resource.logical_resource,
        })
}

/// Check a pending Router issuance against the durable ledger. A ledger row without a canonical
/// job is corruption, not absence: fail closed rather than allowing a second issuance.
fn check_singleton_issuance(record: &ProvisionJobRecord) -> Result<(), InsertWithLocksError> {
    for intent_key in singleton_intent_keys(record) {
        let Some(existing) = SINGLETON_ISSUANCE.with_borrow(|map| map.get(&intent_key)) else {
            continue;
        };
        let existing_key = ProvisionJobRequestKey::new(&existing.request_id, record.deployment_id);
        match JOB_BY_REQUEST.with_borrow(|map| map.get(&existing_key)) {
            Some(job) if job.current_state == JobState::Completed => {
                return Err(InsertWithLocksError::RouterAlreadyIssued {
                    request_id: existing.request_id,
                });
            }
            // A failed job leaves its canonical record for replay but no longer owns the
            // singleton; a retry may overwrite the ledger row.
            Some(job) if matches!(job.current_state, JobState::Failed { .. }) => continue,
            Some(_) => return Err(InsertWithLocksError::RouterIssuanceInProgress),
            None => return Err(InsertWithLocksError::RouterIssuanceIndexCorrupt),
        }
    }
    Ok(())
}

fn reserve_singleton_issuance(record: &ProvisionJobRecord) {
    SINGLETON_ISSUANCE.with_borrow_mut(|map| {
        for intent_key in singleton_intent_keys(record) {
            map.insert(
                intent_key,
                IssuanceRecord {
                    request_id: record.request_id,
                },
            );
        }
    });
}

fn release_singleton_issuance(record: &ProvisionJobRecord) {
    SINGLETON_ISSUANCE.with_borrow_mut(|map| {
        for intent_key in singleton_intent_keys(record) {
            if map
                .get(&intent_key)
                .is_some_and(|issuance| issuance.request_id == record.request_id)
            {
                map.remove(&intent_key);
            }
        }
    });
}

impl ProvisionJobStore {
    pub fn new() -> Self {
        Self
    }

    #[cfg(test)]
    pub(crate) fn provisioning_maps_snapshot_for_test(&self) -> ProvisioningMapsSnapshot {
        let jobs = JOB_BY_REQUEST.with_borrow(|map| {
            map.iter()
                .map(|entry| (entry.key().clone(), entry.value()))
                .collect()
        });
        let owners = JOB_BY_DEPLOYMENT.with_borrow(|map| {
            map.iter()
                .map(|entry| (entry.key().clone(), entry.value()))
                .collect()
        });
        let markers =
            INTENT_LOCK.with_borrow(|map| map.iter().map(|entry| entry.key().clone()).collect());
        (jobs, owners, markers)
    }

    /// Insert a job record idempotently. Same request_id returns the existing record.
    pub fn insert_or_idempotent(
        &self,
        record: ProvisionJobRecord,
    ) -> Result<ProvisionJobRecord, JobInsertError> {
        let key = ProvisionJobRequestKey::new(&record.request_id, record.deployment_id);
        let existing = JOB_BY_REQUEST.with_borrow(|map| map.get(&key));
        if let Some(existing) = existing {
            return Ok(existing);
        }
        check_singleton_issuance(&record)
            .map_err(|_| crate::stable::store::JobInsertError::Conflict)?;
        JOB_BY_REQUEST.with_borrow_mut(|map| map.insert(key, record.clone()));
        JOB_BY_DEPLOYMENT.with_borrow_mut(|map| {
            for resource in &record.resources {
                let intent_key = ProvisioningIntentKey {
                    deployment_id: record.deployment_id.clone(),
                    logical_resource: resource.logical_resource,
                };
                map.insert(
                    intent_key,
                    ProvisionJobRequestKey::new(&record.request_id, record.deployment_id),
                );
            }
        });
        reserve_singleton_issuance(&record);
        Ok(record)
    }

    /// Preflight all lock conflicts, then in one block co-write Map 1, Map 2, and Map 3.
    /// Same-key returns the existing record without any write. Any held intent returns
    /// IntentLockHeld before any canonical or derived mutation.
    pub fn insert_with_intent_locks(
        &self,
        record: ProvisionJobRecord,
        now_ns: u64,
    ) -> Result<InsertWithLocksOutcome, InsertWithLocksError> {
        let key = ProvisionJobRequestKey::new(&record.request_id, record.deployment_id);

        // 1. Idempotency pre-check (read-only so far).
        let existing = JOB_BY_REQUEST.with_borrow(|map| map.get(&key));
        if let Some(existing) = existing {
            return if existing.immutable_request_digest == record.immutable_request_digest {
                Ok(InsertWithLocksOutcome::IdempotentReplay(existing))
            } else {
                Err(InsertWithLocksError::Conflict)
            };
        }

        // 2. Singleton issuance preflight, then derived owner/locks (read-only so far).
        check_singleton_issuance(&record)?;
        for resource in &record.resources {
            let intent_key = ProvisioningIntentKey {
                deployment_id: record.deployment_id.clone(),
                logical_resource: resource.logical_resource,
            };
            if JOB_BY_DEPLOYMENT.with_borrow(|map| map.contains_key(&intent_key))
                || INTENT_LOCK.with_borrow(|map| map.contains_key(&intent_key))
            {
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
                    logical_resource: resource.logical_resource,
                };
                map.insert(intent_key, key.clone());
            }
        });
        INTENT_LOCK.with_borrow_mut(|map| {
            for resource in &record.resources {
                let intent_key = ProvisioningIntentKey {
                    deployment_id: record.deployment_id.clone(),
                    logical_resource: resource.logical_resource,
                };
                map.insert(intent_key, ProvisionIntentLockMarker);
            }
        });

        reserve_singleton_issuance(&record);

        // 4. Advance the fresh record to Reserved inside the same logical boundary.
        let mut advanced = record.clone();
        advanced.current_state = JobState::Reserved;
        advanced.last_transition_ns = now_ns;
        JOB_BY_REQUEST.with_borrow_mut(|map| map.insert(key, advanced.clone()));

        Ok(InsertWithLocksOutcome::InsertedFresh(advanced))
    }

    /// All canonical job records for one deployment, oldest first. Used by the operator
    /// deployment-jobs view; the stable map is keyed by request, so this is a filtered scan.
    pub fn jobs_for_deployment(&self, deployment_id: DeploymentId) -> Vec<ProvisionJobRecord> {
        let mut records: Vec<ProvisionJobRecord> = JOB_BY_REQUEST.with_borrow(|map| {
            map.iter()
                .filter(|entry| entry.value().deployment_id == deployment_id)
                .map(|entry| entry.value())
                .collect()
        });
        records.sort_by_key(|record| (record.created_at_ns, record.request_id));
        records
    }

    /// Every job still in `CleanupPending`, oldest first. Used by the bounded cleanup sweep; like
    /// [`Self::jobs_for_deployment`] this is a filtered scan of the request-keyed map.
    pub fn cleanup_pending_jobs(&self) -> Vec<ProvisionJobRecord> {
        let mut records: Vec<ProvisionJobRecord> = JOB_BY_REQUEST.with_borrow(|map| {
            map.iter()
                .map(|entry| entry.value())
                .filter(|record| matches!(record.current_state, JobState::CleanupPending { .. }))
                .collect()
        });
        records.sort_by_key(|record| (record.created_at_ns, record.request_id));
        records
    }

    /// The issuance job that owns `intent` (Map 2) together with its canonical record.
    pub fn job_for_intent(
        &self,
        intent: &ProvisioningIntentKey,
    ) -> Option<(ProvisionJobRequestKey, ProvisionJobRecord)> {
        let key = JOB_BY_DEPLOYMENT.with_borrow(|map| map.get(intent))?;
        let record = self.get_by_request_key(&key)?;
        Some((key, record))
    }

    /// Every job of `deployment_id` that issued `logical_resource`, with the canister each
    /// recorded for it. Scans the canonical map because a completed job no longer owns its Map 2
    /// row (ADR 0037 §Resource deletion looks up issued canisters after registration completes).
    pub fn jobs_issuing(
        &self,
        deployment_id: DeploymentId,
        logical_resource: LogicalResource,
    ) -> Vec<(ProvisionJobRecord, Option<Principal>)> {
        JOB_BY_REQUEST.with_borrow(|map| {
            map.iter()
                .map(|entry| entry.value())
                .filter(|record| record.deployment_id == deployment_id)
                .filter_map(|record| {
                    let canister = record
                        .resources
                        .iter()
                        .find(|resource| resource.logical_resource == logical_resource)?
                        .canister_id;
                    Some((record, canister))
                })
                .collect()
        })
    }

    /// The deletion recorded for `canister`, if any (ADR 0037 §Resource deletion).
    pub fn resource_deletion(&self, canister: &Principal) -> Option<ResourceDeletionRecord> {
        RESOURCE_DELETIONS.with_borrow(|map| map.get(canister))
    }

    /// Records the deletion of `canister` as `DeletePending` before its first management call.
    /// A record that already exists is kept and returned unchanged, so a retry resumes the
    /// deletion it started.
    pub fn begin_resource_deletion(
        &self,
        canister: Principal,
        deployment_id: DeploymentId,
        logical_resource: LogicalResource,
        now_ns: u64,
    ) -> ResourceDeletionRecord {
        RESOURCE_DELETIONS.with_borrow_mut(|map| {
            if let Some(existing) = map.get(&canister) {
                return existing;
            }
            let record = ResourceDeletionRecord {
                deployment_id,
                logical_resource,
                state: ResourceDeletionState::DeletePending,
                requested_at_ns: now_ns,
            };
            map.insert(canister, record.clone());
            record
        })
    }

    /// Marks the deletion of `canister` done and, in the same step, releases the deleted
    /// intent's Map 2 and Map 3 rows when they still belong to the job that issued `canister`,
    /// so the intent can be issued again. Returns `false` when the deletion was already done.
    ///
    /// # Panics
    ///
    /// When no deletion was recorded for `canister`: [`Self::begin_resource_deletion`] always
    /// runs before the management calls this follows.
    pub fn finish_resource_deletion(&self, canister: Principal, now_ns: u64) -> bool {
        let mut record = self
            .resource_deletion(&canister)
            .expect("finish_resource_deletion follows begin_resource_deletion");
        if matches!(record.state, ResourceDeletionState::Deleted { .. }) {
            return false;
        }
        let intent = ProvisioningIntentKey {
            deployment_id: record.deployment_id,
            logical_resource: record.logical_resource,
        };
        let owned_by_deleted_canister = self.job_for_intent(&intent).is_some_and(|(_, job)| {
            job.resources.iter().any(|resource| {
                resource.logical_resource == intent.logical_resource
                    && resource.canister_id == Some(canister)
            })
        });
        if owned_by_deleted_canister {
            JOB_BY_DEPLOYMENT.with_borrow_mut(|map| map.remove(&intent));
            INTENT_LOCK.with_borrow_mut(|map| map.remove(&intent));
        }
        record.state = ResourceDeletionState::Deleted {
            deleted_at_ns: now_ns,
        };
        RESOURCE_DELETIONS.with_borrow_mut(|map| map.insert(canister, record));
        true
    }

    fn is_deleted_canister(&self, canister: &Principal) -> bool {
        self.resource_deletion(canister)
            .is_some_and(|record| matches!(record.state, ResourceDeletionState::Deleted { .. }))
    }

    pub fn get_by_request(
        &self,
        request_id: &[u8; 32],
        deployment_id: DeploymentId,
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

    pub fn set_resource_artifact_hash(
        &self,
        key: &ProvisionJobRequestKey,
        resource_index: usize,
        artifact_hash: [u8; 32],
    ) {
        JOB_BY_REQUEST.with_borrow_mut(|map| {
            let mut record = map
                .get(key)
                .expect("set_resource_artifact_hash: record not found");
            record.resources[resource_index].artifact_hash = Some(artifact_hash);
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
                    logical_resource: resource.logical_resource,
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
                    logical_resource: resource.logical_resource,
                };
                if map.remove(&lock_key).is_some() {
                    count += 1;
                }
            }
            count
        })
    }

    /// Release both the intent locks and the derived Map 2 ownership for a finished
    /// (failed-and-cleaned) job, keeping the canonical job record for idempotent replay.
    pub fn release_intent_claims_for_record(&self, record: &ProvisionJobRecord) -> usize {
        release_singleton_issuance(record);
        let released = self.clear_intent_locks_for_record(record);
        JOB_BY_DEPLOYMENT.with_borrow_mut(|map| {
            for resource in &record.resources {
                map.remove(&ProvisioningIntentKey {
                    deployment_id: record.deployment_id.clone(),
                    logical_resource: resource.logical_resource,
                });
            }
        });
        released
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
            release_singleton_issuance(record);
            JOB_BY_DEPLOYMENT.with_borrow_mut(|map| {
                for resource in &record.resources {
                    let intent_key = ProvisioningIntentKey {
                        deployment_id: record.deployment_id.clone(),
                        logical_resource: resource.logical_resource,
                    };
                    map.remove(&intent_key);
                }
            });
        }
        record
    }

    /// Rebuild the singleton ledger from canonical Router jobs. Used by an explicit admin
    /// repair, never by the admission path. Failed jobs are excluded.
    pub fn rebuild_singleton_issuance(&self) -> usize {
        let mut rows: Vec<(ProvisioningIntentKey, IssuanceRecord)> = Vec::new();
        JOB_BY_REQUEST.with_borrow(|map| {
            for entry in map.iter() {
                let record = entry.value();
                if matches!(record.current_state, JobState::Failed { .. }) {
                    continue;
                }
                for intent_key in singleton_intent_keys(&record) {
                    rows.push((
                        intent_key,
                        IssuanceRecord {
                            request_id: record.request_id,
                        },
                    ));
                }
            }
        });
        SINGLETON_ISSUANCE.with_borrow_mut(|map| {
            map.clear_new();
            for (intent_key, issuance) in rows {
                map.insert(intent_key, issuance);
            }
        });
        self.singleton_issuance_count()
    }

    /// Read the deployment-scoped contract. `None` means "not configured", not zero credit.
    pub fn deployment_record(&self, deployment_id: DeploymentId) -> Option<DeploymentRecord> {
        DEPLOYMENT_RECORDS.with_borrow(|map| map.get(&deployment_id))
    }

    /// Current suspension record for one deployment, if any.
    pub fn deployment_termination(
        &self,
        deployment_id: DeploymentId,
    ) -> Option<crate::types::DeploymentTermination> {
        TERMINATIONS.with_borrow(|map| map.get(&deployment_id))
    }

    /// All live suspensions, oldest first. Used by the bounded grace sweep.
    pub fn suspended_terminations(&self) -> Vec<crate::types::DeploymentTermination> {
        TERMINATIONS.with_borrow(|map| {
            map.iter()
                .map(|entry| entry.value())
                .filter(|record| {
                    matches!(
                        record.state,
                        crate::types::DeploymentTerminationState::Suspended
                    )
                })
                .collect()
        })
    }

    pub fn put_deployment_termination(&self, record: crate::types::DeploymentTermination) {
        TERMINATIONS.with_borrow_mut(|map| {
            map.insert(record.deployment_id, record);
        });
    }

    /// Every canister recorded for a deployment, across retries and resource kinds, except a
    /// canister deleted on its own. Used only by explicit ownership handover, which is rare and
    /// must be complete.
    pub fn deployment_canisters(&self, deployment_id: DeploymentId) -> Vec<Principal> {
        self.deployment_canister_records_inner(deployment_id, false)
            .into_iter()
            .map(|record| record.canister)
            .collect()
    }

    /// Every recorded canister, including ones referenced only by a Failed job, except a
    /// canister deleted on its own. `active` is true when at least one non-Failed job still
    /// records the canister; a canister seen by both an active and a failed job is active.
    pub fn deployment_canister_records(
        &self,
        deployment_id: DeploymentId,
    ) -> Vec<crate::types::DeploymentCanister> {
        self.deployment_canister_records_inner(deployment_id, true)
    }

    fn deployment_canister_records_inner(
        &self,
        deployment_id: DeploymentId,
        include_failed: bool,
    ) -> Vec<crate::types::DeploymentCanister> {
        let mut canisters = BTreeMap::new();
        JOB_BY_REQUEST.with_borrow(|map| {
            for entry in map.iter() {
                let record = entry.value();
                if record.deployment_id != deployment_id {
                    continue;
                }
                let active = !matches!(record.current_state, JobState::Failed { .. });
                if !include_failed && !active {
                    continue;
                }
                for resource in &record.resources {
                    // A canister deleted on its own (ADR 0037 §Resource deletion) no longer
                    // exists: no handover, termination, or repair may act on it.
                    if let Some(canister_id) = resource.canister_id
                        && !self.is_deleted_canister(&canister_id)
                    {
                        let entry = canisters.entry(canister_id).or_insert(false);
                        *entry |= active;
                    }
                }
            }
        });
        canisters
            .into_iter()
            .map(|(canister, active)| crate::types::DeploymentCanister { canister, active })
            .collect()
    }

    /// Governance repair for a Managed deployment's owner. SelfCustody is rejected because its
    /// owner also controls the canisters and must go through a controller handover.
    pub fn set_deployment_owner(
        &self,
        deployment_id: DeploymentId,
        new_owner: Principal,
    ) -> Result<DeploymentRecord, DeploymentRecordError> {
        if new_owner == Principal::anonymous() {
            return Err(DeploymentRecordError::AnonymousOwner);
        }
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            let mut record = map
                .get(&deployment_id)
                .ok_or(DeploymentRecordError::UnknownDeployment)?;
            if record.controller_mode == ControllerMode::SelfCustody {
                return Err(DeploymentRecordError::AlreadySelfCustody);
            }
            record.owner = new_owner;
            map.insert(deployment_id, record);
            Ok(record)
        })
    }

    /// Commit a SelfCustody owner change. The caller moves the canister controllers first.
    pub fn set_self_custody_owner(
        &self,
        deployment_id: DeploymentId,
        new_owner: Principal,
    ) -> Result<DeploymentRecord, DeploymentRecordError> {
        if new_owner == Principal::anonymous() {
            return Err(DeploymentRecordError::AnonymousOwner);
        }
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            let mut record = map
                .get(&deployment_id)
                .ok_or(DeploymentRecordError::UnknownDeployment)?;
            if record.controller_mode != ControllerMode::SelfCustody {
                return Err(DeploymentRecordError::NotSelfCustody);
            }
            if record.owner == new_owner {
                return Err(DeploymentRecordError::SameOwner);
            }
            record.owner = new_owner;
            map.insert(deployment_id, record);
            Ok(record)
        })
    }

    /// Commit the controller handover in durable state. The caller updates the canisters first and
    /// calls this only after every controller update succeeded.
    pub fn mark_self_custody(
        &self,
        deployment_id: DeploymentId,
    ) -> Result<DeploymentRecord, DeploymentRecordError> {
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            let mut record = map
                .get(&deployment_id)
                .ok_or(DeploymentRecordError::UnknownDeployment)?;
            if record.controller_mode == ControllerMode::SelfCustody {
                return Err(DeploymentRecordError::AlreadySelfCustody);
            }
            record.controller_mode = ControllerMode::SelfCustody;
            record.operator = None;
            map.insert(deployment_id, record);
            Ok(record)
        })
    }

    /// Create the record on first use and return the durable value. A later request with a different
    /// controller mode is rejected; the operator is the snapshot taken at first issuance.
    pub fn ensure_deployment_record(
        &self,
        deployment_id: DeploymentId,
        owner: Principal,
        controller_mode: ControllerMode,
        operator: Option<Principal>,
    ) -> Result<DeploymentRecord, DeploymentRecordError> {
        validate_controller(owner, controller_mode, operator)?;
        if let Some(existing) = self.deployment_record(deployment_id) {
            if existing.controller_mode != controller_mode {
                return Err(DeploymentRecordError::ControllerModeMismatch);
            }
            return Ok(existing);
        }
        let record = DeploymentRecord {
            owner,
            controller_mode,
            operator,
            credit_cycles: 0,
        };
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            map.insert(deployment_id, record);
        });
        Ok(record)
    }

    /// Pre-register a deployment before its first job. Idempotent only for an exact owner, mode and
    /// operator match; any later divergence is a conflict. The record starts with zero credit.
    pub fn initialize_deployment(
        &self,
        deployment_id: DeploymentId,
        owner: Principal,
        controller_mode: ControllerMode,
        operator: Option<Principal>,
    ) -> Result<DeploymentRecord, DeploymentRecordError> {
        validate_controller(owner, controller_mode, operator)?;
        if let Some(existing) = self.deployment_record(deployment_id) {
            if existing.owner != owner {
                return Err(DeploymentRecordError::OwnerMismatch);
            }
            if existing.controller_mode != controller_mode {
                return Err(DeploymentRecordError::ControllerModeMismatch);
            }
            if existing.operator != operator {
                return Err(DeploymentRecordError::OperatorMismatch);
            }
            return Ok(existing);
        }
        let record = DeploymentRecord {
            owner,
            controller_mode,
            operator,
            credit_cycles: 0,
        };
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            map.insert(deployment_id, record);
        });
        Ok(record)
    }

    /// Add cycles to a configured deployment. The arithmetic is checked; a credit can only be
    /// granted to an existing record, never to an arbitrary id.
    pub fn credit_deployment(
        &self,
        deployment_id: DeploymentId,
        amount: u128,
    ) -> Result<u128, DeploymentRecordError> {
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            let mut record = map
                .get(&deployment_id)
                .ok_or(DeploymentRecordError::UnknownDeployment)?;
            record.credit_cycles = record
                .credit_cycles
                .checked_add(amount)
                .ok_or(DeploymentRecordError::CreditOverflow)?;
            map.insert(deployment_id, record);
            Ok(record.credit_cycles)
        })
    }

    /// Deduct cycles from a configured deployment. Debits are checked and fail closed; a job must
    /// not mutate resources after an insufficient-credit rejection.
    pub fn debit_deployment(
        &self,
        deployment_id: DeploymentId,
        amount: u128,
    ) -> Result<u128, DeploymentRecordError> {
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            let mut record = map
                .get(&deployment_id)
                .ok_or(DeploymentRecordError::UnknownDeployment)?;
            record.credit_cycles = record
                .credit_cycles
                .checked_sub(amount)
                .ok_or(DeploymentRecordError::InsufficientCredit)?;
            map.insert(deployment_id, record);
            Ok(record.credit_cycles)
        })
    }

    pub fn service_ledger(&self) -> ServiceLedger {
        SERVICE_LEDGER.with_borrow(|cell| *cell.get())
    }

    pub fn withdrawal(&self, key: &WithdrawalKey) -> Option<WithdrawalRecord> {
        WITHDRAWALS.with_borrow(|map| map.get(key))
    }

    /// Reserve `amount` from one ledger pool and record a `Pending` withdrawal. This is the
    /// atomic no-await step: a concurrent withdrawal can never observe the same ledger cycles.
    pub fn reserve_withdrawal(
        &self,
        key: WithdrawalKey,
        kind: WithdrawalKind,
        destination: Principal,
        amount: u128,
    ) -> Result<WithdrawalReservation, WithdrawalStoreError> {
        if amount == 0 {
            return Err(WithdrawalStoreError::AmountZero);
        }
        if let Some(record) = self.withdrawal(&key) {
            if record.kind != kind || record.destination != destination || record.amount != amount {
                return Err(WithdrawalStoreError::IdempotencyConflict);
            }
            return match record.status {
                WithdrawalStatus::Completed => Ok(WithdrawalReservation::Replayed(record.amount)),
                WithdrawalStatus::Pending => Err(WithdrawalStoreError::Pending),
                WithdrawalStatus::Failed(_) => Err(WithdrawalStoreError::PreviousAttemptFailed),
            };
        }
        let ledger = self.service_ledger();
        let (platform_fees, compensation_reserve) = match kind {
            WithdrawalKind::PlatformFees => (
                ledger
                    .platform_fees
                    .checked_sub(amount)
                    .ok_or(WithdrawalStoreError::InsufficientLedger)?,
                ledger.compensation_reserve,
            ),
            WithdrawalKind::CompensationReserve => (
                ledger.platform_fees,
                ledger
                    .compensation_reserve
                    .checked_sub(amount)
                    .ok_or(WithdrawalStoreError::InsufficientLedger)?,
            ),
        };
        SERVICE_LEDGER.with_borrow_mut(|cell| {
            let _ = cell.set(ServiceLedger {
                platform_fees,
                compensation_reserve,
            });
        });
        WITHDRAWALS.with_borrow_mut(|map| {
            map.insert(
                key,
                WithdrawalRecord {
                    kind,
                    destination,
                    amount,
                    status: WithdrawalStatus::Pending,
                },
            );
        });
        Ok(WithdrawalReservation::Reserved)
    }

    /// Finalize a successful transfer. Idempotent: a `Completed` record returns without a write.
    pub fn complete_withdrawal(&self, key: &WithdrawalKey) -> Result<(), WithdrawalStoreError> {
        WITHDRAWALS.with_borrow_mut(|map| {
            let mut record = map
                .get(key)
                .ok_or(WithdrawalStoreError::UnknownWithdrawal)?;
            match record.status {
                WithdrawalStatus::Completed => Ok(()),
                WithdrawalStatus::Pending => {
                    record.status = WithdrawalStatus::Completed;
                    map.insert(*key, record);
                    Ok(())
                }
                WithdrawalStatus::Failed(_) => Err(WithdrawalStoreError::NotPending),
            }
        })
    }

    /// Release a failed transfer back to its ledger pool and record the failure.
    pub fn fail_withdrawal(
        &self,
        key: &WithdrawalKey,
        error: String,
    ) -> Result<(), WithdrawalStoreError> {
        WITHDRAWALS.with_borrow_mut(|map| {
            let mut record = map
                .get(key)
                .ok_or(WithdrawalStoreError::UnknownWithdrawal)?;
            match record.status {
                WithdrawalStatus::Completed => Err(WithdrawalStoreError::NotPending),
                WithdrawalStatus::Failed(_) => Ok(()),
                WithdrawalStatus::Pending => {
                    let ledger = self.service_ledger();
                    let (platform_fees, compensation_reserve) = match record.kind {
                        WithdrawalKind::PlatformFees => (
                            ledger
                                .platform_fees
                                .checked_add(record.amount)
                                .ok_or(WithdrawalStoreError::LedgerOverflow)?,
                            ledger.compensation_reserve,
                        ),
                        WithdrawalKind::CompensationReserve => (
                            ledger.platform_fees,
                            ledger
                                .compensation_reserve
                                .checked_add(record.amount)
                                .ok_or(WithdrawalStoreError::LedgerOverflow)?,
                        ),
                    };
                    SERVICE_LEDGER.with_borrow_mut(|cell| {
                        let _ = cell.set(ServiceLedger {
                            platform_fees,
                            compensation_reserve,
                        });
                    });
                    record.status = WithdrawalStatus::Failed(error);
                    map.insert(*key, record);
                    Ok(())
                }
            }
        })
    }

    /// Withhold the full admission charge from the deployment credit and accrue its non-cost parts.
    /// All three values are validated before any write.
    pub fn reserve_admission(
        &self,
        deployment_id: DeploymentId,
        charge: AdmissionCharge,
    ) -> Result<(), DeploymentRecordError> {
        let total = charge
            .total()
            .map_err(|_| DeploymentRecordError::AdmissionAccountingOverflow)?;
        let mut record = self
            .deployment_record(deployment_id)
            .ok_or(DeploymentRecordError::UnknownDeployment)?;
        record.credit_cycles = record
            .credit_cycles
            .checked_sub(total)
            .ok_or(DeploymentRecordError::InsufficientCredit)?;
        let ledger = self.service_ledger();
        let platform_fees = ledger
            .platform_fees
            .checked_add(charge.fee)
            .ok_or(DeploymentRecordError::AdmissionAccountingOverflow)?;
        let compensation_reserve = ledger
            .compensation_reserve
            .checked_add(charge.reserve)
            .ok_or(DeploymentRecordError::AdmissionAccountingOverflow)?;
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            map.insert(deployment_id, record);
        });
        SERVICE_LEDGER.with_borrow_mut(|cell| {
            let _ = cell.set(ServiceLedger {
                platform_fees,
                compensation_reserve,
            });
        });
        Ok(())
    }

    /// Undo a reservation that never reached a canonical job (insert failure).
    pub fn release_reservation(
        &self,
        deployment_id: DeploymentId,
        charge: AdmissionCharge,
    ) -> Result<(), DeploymentRecordError> {
        let total = charge
            .total()
            .map_err(|_| DeploymentRecordError::AdmissionAccountingOverflow)?;
        let mut record = self
            .deployment_record(deployment_id)
            .ok_or(DeploymentRecordError::UnknownDeployment)?;
        record.credit_cycles = record
            .credit_cycles
            .checked_add(total)
            .ok_or(DeploymentRecordError::AdmissionAccountingOverflow)?;
        let ledger = self.service_ledger();
        let platform_fees = ledger
            .platform_fees
            .checked_sub(charge.fee)
            .ok_or(DeploymentRecordError::AdmissionAccountingUnderflow)?;
        let compensation_reserve = ledger
            .compensation_reserve
            .checked_sub(charge.reserve)
            .ok_or(DeploymentRecordError::AdmissionAccountingUnderflow)?;
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            map.insert(deployment_id, record);
        });
        SERVICE_LEDGER.with_borrow_mut(|cell| {
            let _ = cell.set(ServiceLedger {
                platform_fees,
                compensation_reserve,
            });
        });
        Ok(())
    }

    /// Refund a reserved admission whose job created no canister. The job flag makes this
    /// idempotent: a second call returns `Ok(false)` without touching the ledger.
    pub fn refund_admission(
        &self,
        key: &ProvisionJobRequestKey,
        charge: AdmissionCharge,
    ) -> Result<bool, DeploymentRecordError> {
        let mut job = self
            .get_by_request_key(key)
            .ok_or(DeploymentRecordError::UnknownDeployment)?;
        if job.admission_refunded {
            return Ok(false);
        }
        if job.admission_charge != charge {
            return Err(DeploymentRecordError::AdmissionChargeMismatch);
        }
        let total = charge
            .total()
            .map_err(|_| DeploymentRecordError::AdmissionAccountingOverflow)?;
        let mut record = self
            .deployment_record(job.deployment_id)
            .ok_or(DeploymentRecordError::UnknownDeployment)?;
        record.credit_cycles = record
            .credit_cycles
            .checked_add(total)
            .ok_or(DeploymentRecordError::AdmissionAccountingOverflow)?;
        let ledger = self.service_ledger();
        let platform_fees = ledger
            .platform_fees
            .checked_sub(charge.fee)
            .ok_or(DeploymentRecordError::AdmissionAccountingUnderflow)?;
        let compensation_reserve = ledger
            .compensation_reserve
            .checked_sub(charge.reserve)
            .ok_or(DeploymentRecordError::AdmissionAccountingUnderflow)?;
        job.admission_refunded = true;
        let deployment_id = job.deployment_id;
        JOB_BY_REQUEST.with_borrow_mut(|map| {
            map.insert(key.clone(), job);
        });
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            map.insert(deployment_id, record);
        });
        SERVICE_LEDGER.with_borrow_mut(|cell| {
            let _ = cell.set(ServiceLedger {
                platform_fees,
                compensation_reserve,
            });
        });
        Ok(true)
    }

    /// All grant rows for one deployment, in stable-key order.
    pub fn billing_grants_for_deployment(
        &self,
        deployment_id: DeploymentId,
    ) -> Vec<(BillingGrantKey, BillingGrantRecord)> {
        BILLING_GRANTS.with_borrow(|map| {
            map.iter()
                .filter(|entry| entry.value().deployment_id == deployment_id)
                .map(|entry| (entry.key().clone(), entry.value()))
                .collect()
        })
    }

    pub fn billing_grant(&self, key: &BillingGrantKey) -> Option<BillingGrantRecord> {
        BILLING_GRANTS.with_borrow(|map| map.get(key))
    }

    /// Apply one billing credit grant. The grant key is idempotent: an exact replay returns the
    /// current credit without adding it again; the same key with a different payload conflicts.
    /// The record and grant row are validated before either is written.
    pub fn grant_credit(
        &self,
        key: BillingGrantKey,
        deployment_id: DeploymentId,
        amount: u128,
        now_ns: u64,
    ) -> Result<GrantCreditResult, GrantCreditError> {
        if amount == 0 {
            return Err(GrantCreditError::AmountZero);
        }
        if let Some(existing) = self.billing_grant(&key) {
            if existing.deployment_id != deployment_id || existing.amount != amount {
                return Err(GrantCreditError::IdempotencyConflict);
            }
            let credit_cycles = self
                .deployment_record(deployment_id)
                .ok_or(GrantCreditError::UnknownDeployment)?
                .credit_cycles;
            return Ok(GrantCreditResult {
                deployment_id,
                amount,
                credit_cycles,
                replayed: true,
            });
        }
        let mut record = self
            .deployment_record(deployment_id)
            .ok_or(GrantCreditError::UnknownDeployment)?;
        record.credit_cycles = record
            .credit_cycles
            .checked_add(amount)
            .ok_or(GrantCreditError::CreditOverflow)?;
        let grant = BillingGrantRecord {
            deployment_id,
            amount,
            credited_at_ns: now_ns,
            reversed_at_ns: None,
        };
        BILLING_GRANTS.with_borrow_mut(|map| {
            map.insert(key, grant);
        });
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            map.insert(deployment_id, record);
        });
        Ok(GrantCreditResult {
            deployment_id,
            amount,
            credit_cycles: record.credit_cycles,
            replayed: false,
        })
    }

    /// Reverse one full billing grant. The grant's credit is pooled, so a reversal is allowed only
    /// when the deployment still holds the full amount. A second call is an idempotent replay.
    pub fn reverse_grant(
        &self,
        key: &BillingGrantKey,
        now_ns: u64,
    ) -> Result<ReverseCreditResult, ReverseCreditError> {
        let mut grant = self
            .billing_grant(key)
            .ok_or(ReverseCreditError::UnknownGrant)?;
        let mut record = self
            .deployment_record(grant.deployment_id)
            .ok_or(ReverseCreditError::InvalidState)?;
        if grant.reversed_at_ns.is_some() {
            return Ok(ReverseCreditResult {
                deployment_id: grant.deployment_id,
                amount: grant.amount,
                credit_cycles: record.credit_cycles,
                replayed: true,
            });
        }
        record.credit_cycles = record
            .credit_cycles
            .checked_sub(grant.amount)
            .ok_or(ReverseCreditError::InsufficientCredit)?;
        grant.reversed_at_ns = Some(now_ns);
        BILLING_GRANTS.with_borrow_mut(|map| {
            map.insert(key.clone(), grant);
        });
        DEPLOYMENT_RECORDS.with_borrow_mut(|map| {
            map.insert(grant.deployment_id, record);
        });
        Ok(ReverseCreditResult {
            deployment_id: grant.deployment_id,
            amount: grant.amount,
            credit_cycles: record.credit_cycles,
            replayed: false,
        })
    }

    pub fn service_config(&self) -> ServiceConfig {
        SERVICE_CONFIG.with_borrow(|cell| *cell.get())
    }

    /// Governance-only in the caller; the store validates rate bounds.
    pub fn set_service_config(&self, config: ServiceConfig) -> Result<(), ServiceConfigError> {
        if u32::from(config.platform_fee_bps) + u32::from(config.reserve_bps) > 10_000 {
            return Err(ServiceConfigError::RatesExceedTenThousandBps);
        }
        if let Some(operator) = config.operator
            && operator == Principal::anonymous()
        {
            return Err(ServiceConfigError::AnonymousOperator);
        }
        if let Some(authority) = config.billing_authority
            && authority == Principal::anonymous()
        {
            return Err(ServiceConfigError::AnonymousBillingAuthority);
        }
        SERVICE_CONFIG.with_borrow_mut(|cell| {
            let _ = cell.set(config);
        });
        Ok(())
    }

    pub fn singleton_issuance_count(&self) -> usize {
        SINGLETON_ISSUANCE.with_borrow(|map| map.len() as usize)
    }

    /// Test-only lookup: return the `ProvisionJobRequestKey` stored in `JOB_BY_DEPLOYMENT`
    /// for the derived intent `(deployment_id, logical_resource)`, if any.
    #[cfg(test)]
    pub(crate) fn assert_intent_to_request_for_test(
        &self,
        deployment_id: DeploymentId,
        logical_resource: crate::types::LogicalResource,
    ) -> Option<ProvisionJobRequestKey> {
        let intent_key = ProvisioningIntentKey {
            deployment_id,
            logical_resource,
        };
        JOB_BY_DEPLOYMENT.with_borrow(|map| map.get(&intent_key))
    }

    #[cfg(test)]
    pub(crate) fn set_intent_owner_for_test(
        &self,
        intent_key: ProvisioningIntentKey,
        owner: Option<ProvisionJobRequestKey>,
    ) {
        JOB_BY_DEPLOYMENT.with_borrow_mut(|map| match owner {
            Some(owner) => {
                map.insert(intent_key, owner);
            }
            None => {
                map.remove(&intent_key);
            }
        });
    }

    #[cfg(test)]
    pub(crate) fn set_intent_lock_for_test(
        &self,
        intent_key: ProvisioningIntentKey,
        present: bool,
    ) {
        INTENT_LOCK.with_borrow_mut(|map| {
            if present {
                map.insert(intent_key, ProvisionIntentLockMarker);
            } else {
                map.remove(&intent_key);
            }
        });
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
                        logical_resource: resource.logical_resource,
                    };
                    map.contains_key(&intent_key)
                })
                .count()
        })
    }

    /// Complete one Graph registration after Router catalog reconciliation.
    ///
    /// Fresh completion preflights every Map 2 owner and Map 3 marker before the first mutation,
    /// then writes Map 1 `Completed` and removes exactly those owned derived rows. Completed replay
    /// does not inspect or mutate Map 2/3, so rows acquired later by another request survive.
    pub fn complete_graph_registration(
        &self,
        key: &ProvisionJobRequestKey,
        now_ns: u64,
    ) -> Result<RouterRegistrationAckResponse, CompleteGraphRegistrationError> {
        let mut record = JOB_BY_REQUEST
            .with_borrow(|map| map.get(key))
            .ok_or(CompleteGraphRegistrationError::NotFound)?;

        if record.current_state == JobState::Completed {
            return Ok(RouterRegistrationAckResponse::Replay);
        }
        if record.current_state != JobState::RouterRegistrationPending {
            return Err(CompleteGraphRegistrationError::InvalidState);
        }

        let intent_keys: Vec<ProvisioningIntentKey> = record
            .resources
            .iter()
            .map(|resource| ProvisioningIntentKey {
                deployment_id: record.deployment_id.clone(),
                logical_resource: resource.logical_resource,
            })
            .collect();
        let exact_rows_present = intent_keys.iter().all(|intent_key| {
            JOB_BY_DEPLOYMENT.with_borrow(|map| map.get(intent_key).as_ref() == Some(key))
                && INTENT_LOCK.with_borrow(|map| map.contains_key(intent_key))
        });
        if !exact_rows_present {
            return Err(CompleteGraphRegistrationError::InvalidState);
        }

        record.current_state = JobState::Completed;
        record.last_transition_ns = now_ns;
        JOB_BY_REQUEST.with_borrow_mut(|map| {
            map.insert(key.clone(), record);
        });
        JOB_BY_DEPLOYMENT.with_borrow_mut(|map| {
            for intent_key in &intent_keys {
                if map.get(intent_key).as_ref() == Some(key) {
                    map.remove(intent_key);
                }
            }
        });
        INTENT_LOCK.with_borrow_mut(|map| {
            for intent_key in &intent_keys {
                map.remove(intent_key);
            }
        });

        Ok(RouterRegistrationAckResponse::Applied)
    }

    /// True if any non-terminal job exists for `deployment_id`.
    ///
    /// This is a defensive bootstrap check; it scans the canonical job map because the
    /// primary key is `(request_id, deployment_id)`.
    pub fn has_live_job_for_deployment(&self, deployment_id: DeploymentId) -> bool {
        JOB_BY_REQUEST.with_borrow(|map| {
            map.iter().any(|entry| {
                entry.value().deployment_id == deployment_id
                    && !is_terminal_state(&entry.value().current_state)
            })
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum InsertWithLocksOutcome {
    InsertedFresh(ProvisionJobRecord),
    IdempotentReplay(ProvisionJobRecord),
}

fn validate_controller(
    owner: Principal,
    controller_mode: ControllerMode,
    operator: Option<Principal>,
) -> Result<(), DeploymentRecordError> {
    if owner == Principal::anonymous() {
        return Err(DeploymentRecordError::AnonymousOwner);
    }
    match (controller_mode, operator) {
        (ControllerMode::Managed, Some(operator)) if operator != Principal::anonymous() => Ok(()),
        (ControllerMode::Managed, _) => Err(DeploymentRecordError::MissingOperator),
        (ControllerMode::SelfCustody, None) => Ok(()),
        (ControllerMode::SelfCustody, Some(_)) => Err(DeploymentRecordError::UnexpectedOperator),
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeploymentRecordError {
    AnonymousOwner,
    MissingOperator,
    UnexpectedOperator,
    ControllerModeMismatch,
    OwnerMismatch,
    OperatorMismatch,
    UnknownDeployment,
    CreditOverflow,
    InsufficientCredit,
    AlreadySelfCustody,
    NotSelfCustody,
    SameOwner,
    AdmissionAccountingOverflow,
    AdmissionAccountingUnderflow,
    AdmissionChargeMismatch,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceConfigError {
    RatesExceedTenThousandBps,
    AnonymousOperator,
    AnonymousBillingAuthority,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InsertWithLocksError {
    Conflict,
    IntentLockHeld,
    RouterAlreadyIssued { request_id: [u8; 32] },
    RouterIssuanceInProgress,
    RouterIssuanceIndexCorrupt,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CompleteGraphRegistrationError {
    NotFound,
    InvalidState,
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
