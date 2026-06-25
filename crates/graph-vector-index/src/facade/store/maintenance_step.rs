//! Vector-canister-owned maintenance execution state machine (ADR 0031 Slice 10).
//!
//! The Router owns the maintenance *policy* and forwards a [`VectorMaintenanceStepRequest`] snapshot
//! (thresholds + per-step budgets); this module owns the *execution* state. One
//! [`VectorIndexStore::admin_vector_maintenance_step`] call advances **at most one bounded unit** of
//! work and returns a [`VectorMaintenanceStepResult`], then stops — there is no internal loop.
//!
//! Only the page-health **scan** phase is persisted here (`VECTOR_MAINTENANCE_STATE`); once a rebuild
//! starts, `VECTOR_REBUILD_STATE` is the source of truth and this resets to `Idle`. The step stops at
//! `ReadyToPublish`: publish (and abort) stay explicit, separately-forwarded operations.
//!
//! **Persistence.** `VECTOR_MAINTENANCE_STATE` is stable execution state and survives upgrade (it
//! holds the mid-orchestration scan cursor + merged counters); it is cleared only on canister
//! init/reset, or set back to `Idle` by the explicit [`VectorIndexStore::admin_vector_maintenance_reset`].

use super::VectorIndexStore;
use super::rebuild::{partition_health_summary, rebuild_state_of};
use super::recommend_partition_maintenance;
use crate::facade::stable::{VECTOR_INDEX_DEFS, VECTOR_MAINTENANCE_STATE};
use crate::records::{RawMaintenanceState, VectorRebuildStateRecord};
use candid::{Decode, Encode, Principal};
use gleaph_graph_kernel::vector_index::{
    VectorIndexError, VectorMaintenanceFailure, VectorMaintenanceRecommendation,
    VectorMaintenanceState, VectorMaintenanceStepRequest, VectorMaintenanceStepResult,
    VectorPartitionPageHealth,
};

/// Upper bound on the persisted [`VectorMaintenanceFailure::message`] so the durable maintenance
/// state size never depends on a downstream error string (mirrors the Slice 7/8 rebuild-state
/// byte-cap discipline). Truncated on a UTF-8 char boundary.
const MAX_MAINTENANCE_FAILURE_MESSAGE_BYTES: usize = 256;

/// Reads the current maintenance state for an index (`Idle` when none is recorded).
fn maintenance_state_of(index_id: u32) -> VectorMaintenanceState {
    VECTOR_MAINTENANCE_STATE
        .with_borrow(|m| m.get(&index_id))
        .map(|raw| Decode!(&raw.0, VectorMaintenanceState).expect("decode VectorMaintenanceState"))
        .unwrap_or_default()
}

/// Persists a maintenance state, removing the row entirely for `Idle` so an inactive index keeps no
/// durable maintenance bytes.
fn put_maintenance_state(index_id: u32, state: &VectorMaintenanceState) {
    if matches!(state, VectorMaintenanceState::Idle) {
        VECTOR_MAINTENANCE_STATE.with_borrow_mut(|m| m.remove(&index_id));
    } else {
        let bytes = Encode!(state).expect("encode VectorMaintenanceState");
        VECTOR_MAINTENANCE_STATE
            .with_borrow_mut(|m| m.insert(index_id, RawMaintenanceState(bytes)));
    }
}

/// Truncates a failure message to [`MAX_MAINTENANCE_FAILURE_MESSAGE_BYTES`] on a char boundary.
fn bounded_message(mut message: String) -> String {
    if message.len() <= MAX_MAINTENANCE_FAILURE_MESSAGE_BYTES {
        return message;
    }
    let mut end = MAX_MAINTENANCE_FAILURE_MESSAGE_BYTES;
    while end > 0 && !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    message
}

/// Additively merges one scan step's `partial` into the running `merged` counters; the
/// `index_id`/`index_version` scope is repeated (take the partial's value).
fn merge_health(merged: &mut VectorPartitionPageHealth, partial: &VectorPartitionPageHealth) {
    merged.index_id = partial.index_id;
    merged.index_version = partial.index_version;
    merged.page_count = merged.page_count.saturating_add(partial.page_count);
    merged.total_rows = merged.total_rows.saturating_add(partial.total_rows);
    merged.physical_live_rows = merged
        .physical_live_rows
        .saturating_add(partial.physical_live_rows);
    merged.tombstoned_rows = merged
        .tombstoned_rows
        .saturating_add(partial.tombstoned_rows);
}

impl VectorIndexStore {
    /// Advances one bounded unit of vector-index maintenance (ADR 0031 Slice 10). Router-guarded
    /// `#[update]`. The Router snapshots its policy + budgets into `req`; this performs at most one
    /// scan step, rebuild step, or cleanup step and returns the outcome, stopping at `ReadyToPublish`.
    ///
    /// Failure handling: a downstream execution error transitions the maintenance state to `Failed`
    /// (a no-op until [`admin_vector_maintenance_reset`](Self::admin_vector_maintenance_reset)); an
    /// invalid policy or unknown index is returned as `Err` without persisting `Failed`.
    pub fn admin_vector_maintenance_step(
        &self,
        caller: Principal,
        index_id: u32,
        req: VectorMaintenanceStepRequest,
    ) -> Result<VectorMaintenanceStepResult, VectorIndexError> {
        self.assert_router_caller(caller)?;
        let def = VECTOR_INDEX_DEFS
            .with_borrow(|defs| defs.get(&index_id))
            .ok_or(VectorIndexError::UnknownIndex)?;

        // 0. A prior step failed: no-op until an explicit reset.
        if let VectorMaintenanceState::Failed(failure) = maintenance_state_of(index_id) {
            return Ok(VectorMaintenanceStepResult::Failed(failure));
        }

        // 1. Rebuild in flight (its own state machine owns the phase): drive/observe one unit.
        let rebuild = rebuild_state_of(index_id);
        match rebuild {
            VectorRebuildStateRecord::Sampling { .. }
            | VectorRebuildStateRecord::Training { .. }
            | VectorRebuildStateRecord::Building { .. } => {
                return Ok(
                    match self.admin_vector_rebuild_step(caller, index_id, req.rebuild_max_subjects)
                    {
                        Ok(status) => VectorMaintenanceStepResult::RebuildAdvanced(status),
                        Err(e) => self.fail_maintenance(index_id, e),
                    },
                );
            }
            VectorRebuildStateRecord::ReadyToPublish { .. } => {
                // Publish is explicit; surface the status and stop here.
                return Ok(VectorMaintenanceStepResult::AwaitingPublish(
                    self.admin_vector_rebuild_status(caller, index_id)?,
                ));
            }
            VectorRebuildStateRecord::Cleaning { .. }
            | VectorRebuildStateRecord::Aborting { .. } => {
                return Ok(
                    match self.admin_vector_rebuild_cleanup_step(
                        caller,
                        index_id,
                        req.cleanup_max_work,
                    ) {
                        Ok(status) => VectorMaintenanceStepResult::CleanupAdvanced(status),
                        Err(e) => self.fail_maintenance(index_id, e),
                    },
                );
            }
            VectorRebuildStateRecord::Failed { .. } => {
                // The rebuild itself failed; surface its status so the operator aborts it explicitly.
                return Ok(VectorMaintenanceStepResult::RebuildAdvanced(
                    self.admin_vector_rebuild_status(caller, index_id)?,
                ));
            }
            VectorRebuildStateRecord::Idle => {}
        }

        // 2. No rebuild in flight: advance the page-health scan (start one if idle).
        let (cursor, exhausted, mut merged) = match maintenance_state_of(index_id) {
            VectorMaintenanceState::Idle => (None, false, VectorPartitionPageHealth::default()),
            VectorMaintenanceState::Scanning {
                cursor,
                exhausted,
                merged,
            } => (cursor, exhausted, merged),
            // Handled in step 0.
            VectorMaintenanceState::Failed(failure) => {
                return Ok(VectorMaintenanceStepResult::Failed(failure));
            }
        };

        if !exhausted {
            return Ok(self.scan_one_step(caller, index_id, cursor, merged, &req));
        }

        // 3. Scan exhausted: generation guard before recommending. An active-version flip after the
        // scan exhausted (no cursor left to scope-check) is caught here; restart instead of judging
        // against stale `merged` page health.
        if merged.index_id != index_id || merged.index_version != def.active_index_version {
            let restart = VectorMaintenanceState::Scanning {
                cursor: None,
                exhausted: false,
                merged: VectorPartitionPageHealth::default(),
            };
            put_maintenance_state(index_id, &restart);
            return Ok(VectorMaintenanceStepResult::Scanning { exhausted: false });
        }

        // Recompute the O(nlist) head-only skew summary server-side; never trust caller-attested skew.
        let summary = partition_health_summary(index_id, def.nlist, def.active_index_version);
        merged.index_id = index_id;
        let recommendation = recommend_partition_maintenance(&summary, &merged, &req.policy)?;
        match recommendation {
            VectorMaintenanceRecommendation::Healthy => {
                put_maintenance_state(index_id, &VectorMaintenanceState::Idle);
                Ok(VectorMaintenanceStepResult::Healthy)
            }
            VectorMaintenanceRecommendation::RebuildRecommended
            | VectorMaintenanceRecommendation::RebuildRequired => {
                // `nlist=1` indexes must pass an explicit `target_nlist`; otherwise default to the
                // current `nlist` (rejected by the rebuild start when degenerate).
                let nlist = req.target_nlist.unwrap_or(def.nlist);
                match self.admin_start_vector_rebuild(caller, index_id, nlist, req.sample_limit) {
                    Ok(()) => {
                        // The rebuild state machine now drives; clear the scan state.
                        put_maintenance_state(index_id, &VectorMaintenanceState::Idle);
                        Ok(VectorMaintenanceStepResult::RebuildStarted(recommendation))
                    }
                    Err(e) => Ok(self.fail_maintenance(index_id, e)),
                }
            }
        }
    }

    /// One bounded page-health scan step: merges the partial, persists progress (or restarts on a
    /// stale cursor), and returns `Scanning { exhausted }`.
    fn scan_one_step(
        &self,
        caller: Principal,
        index_id: u32,
        cursor: Option<Vec<u8>>,
        mut merged: VectorPartitionPageHealth,
        req: &VectorMaintenanceStepRequest,
    ) -> VectorMaintenanceStepResult {
        match self.admin_vector_partition_health_step(caller, index_id, cursor, req.scan_max_pages)
        {
            Ok(step) => {
                merge_health(&mut merged, &step.partial);
                let next = VectorMaintenanceState::Scanning {
                    cursor: if step.exhausted { None } else { step.cursor },
                    exhausted: step.exhausted,
                    merged,
                };
                put_maintenance_state(index_id, &next);
                VectorMaintenanceStepResult::Scanning {
                    exhausted: step.exhausted,
                }
            }
            // The active version changed mid-scan (cursor scope no longer matches): restart cleanly.
            Err(VectorIndexError::InvalidStatsCursor) => {
                let restart = VectorMaintenanceState::Scanning {
                    cursor: None,
                    exhausted: false,
                    merged: VectorPartitionPageHealth::default(),
                };
                put_maintenance_state(index_id, &restart);
                VectorMaintenanceStepResult::Scanning { exhausted: false }
            }
            Err(e) => self.fail_maintenance(index_id, e),
        }
    }

    /// Records a bounded `Failed` maintenance state and returns the matching step result.
    fn fail_maintenance(&self, index_id: u32, e: VectorIndexError) -> VectorMaintenanceStepResult {
        let failure = VectorMaintenanceFailure {
            code: e,
            message: bounded_message(e.to_string()),
        };
        put_maintenance_state(index_id, &VectorMaintenanceState::Failed(failure.clone()));
        VectorMaintenanceStepResult::Failed(failure)
    }

    /// Reports the current maintenance execution state (ADR 0031 Slice 10). Router-guarded `#[query]`.
    /// An index with no recorded state reports `Idle`.
    pub fn admin_vector_maintenance_status(
        &self,
        caller: Principal,
        index_id: u32,
    ) -> Result<VectorMaintenanceState, VectorIndexError> {
        self.assert_router_caller(caller)?;
        Ok(maintenance_state_of(index_id))
    }

    /// Resets the maintenance execution state to `Idle` from any state, including `Failed` (ADR 0031
    /// Slice 10). Router-guarded `#[update]`. This is the only recovery path for a `Failed`
    /// maintenance state. It does **not** touch `VECTOR_REBUILD_STATE`: aborting an in-flight rebuild
    /// remains the explicit `admin_abort_vector_rebuild`.
    pub fn admin_vector_maintenance_reset(
        &self,
        caller: Principal,
        index_id: u32,
    ) -> Result<(), VectorIndexError> {
        self.assert_router_caller(caller)?;
        put_maintenance_state(index_id, &VectorMaintenanceState::Idle);
        Ok(())
    }
}
