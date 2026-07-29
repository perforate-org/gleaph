//! Graph-owned maintenance budgets derived from the shared execution limits in
//! `gleaph-graph-kernel`.

use gleaph_graph_kernel::{
    MAX_TIMER_MAINTENANCE_INSTRUCTIONS, TIMER_MAINTENANCE_INSTRUCTION_HEADROOM,
};
use ic_stable_lara::MaintenanceBudget;

/// Conservative per-tick cap for LARA deferred maintenance under timer/heartbeat.
///
/// Set below the shared update-call ceiling to leave headroom for non-LARA work.
pub const GRAPH_TIMER_LARA_MAX_INSTRUCTIONS: u64 = MAX_TIMER_MAINTENANCE_INSTRUCTIONS;

/// Reserved instruction headroom checked inside LARA's maintenance loop.
pub const GRAPH_TIMER_LARA_RESERVE_INSTRUCTIONS: u64 = TIMER_MAINTENANCE_INSTRUCTION_HEADROOM;

/// Delay before the next deferred-maintenance tick when a tick filled its
/// instruction budget (backlog under pressure). Floor set by single-threaded
/// fairness, not the platform: `ic-cdk-timers` resolution is block-rate, so
/// sub-second delays are not meaningful. See [ADR 0020].
///
/// [ADR 0020]: ../../../../design/adr/0020-deferred-maintenance-timer-drain.md
#[cfg(any(test, target_family = "wasm"))]
pub const MAINTENANCE_TIMER_FLOOR_DELAY: core::time::Duration = core::time::Duration::from_secs(1);

/// Delay before the next deferred-maintenance tick when a backlog remains but
/// the tick did not exhaust its budget (small tail). See [ADR 0020].
///
/// [ADR 0020]: ../../../../design/adr/0020-deferred-maintenance-timer-drain.md
#[cfg(any(test, target_family = "wasm"))]
pub const MAINTENANCE_TIMER_RELAXED_DELAY: core::time::Duration =
    core::time::Duration::from_secs(5);

/// [`MaintenanceBudget`] suited for **timer** draining of the deferred LARA
/// queue in production canisters (ADR 0020).
///
/// On canisters, the delete, edge-insert, and finalize paths all bound their
/// inline drain with this budget and arm the maintenance timer to finish the
/// remainder. Native builds drain fully via `unlimited_lara_maintenance_budget`.
#[inline]
pub const fn timer_lara_maintenance_budget() -> MaintenanceBudget {
    MaintenanceBudget {
        max_instructions: GRAPH_TIMER_LARA_MAX_INSTRUCTIONS,
        reserve_instructions: GRAPH_TIMER_LARA_RESERVE_INSTRUCTIONS,
        checkpoint_every: 1,
        max_work_items: None,
        max_segments: None,
        max_delete_edge_steps: None,
    }
}

/// Drains the full deferred maintenance queue (no instruction cap).
///
/// Native-only: tests and local benches drain fully (the instruction counter is
/// unused off-canister). On canisters, bounded budgets plus the maintenance
/// timer (ADR 0020) replace unbounded inline drains.
#[cfg(not(target_family = "wasm"))]
#[inline]
pub const fn unlimited_lara_maintenance_budget() -> MaintenanceBudget {
    MaintenanceBudget {
        max_instructions: 0,
        reserve_instructions: 0,
        checkpoint_every: 1,
        max_work_items: None,
        max_segments: None,
        max_delete_edge_steps: None,
    }
}

/// Best-effort maintenance budget for explicit bulk-ingest finalize drain passes.
///
/// On canisters, uses the timer budget so finalize can share the same per-message
/// instruction envelope as heartbeat maintenance. Native builds drain fully.
#[cfg(target_family = "wasm")]
#[inline]
pub const fn bulk_ingest_finalize_maintenance_budget() -> MaintenanceBudget {
    timer_lara_maintenance_budget()
}

#[cfg(not(target_family = "wasm"))]
#[inline]
pub const fn bulk_ingest_finalize_maintenance_budget() -> MaintenanceBudget {
    unlimited_lara_maintenance_budget()
}

/// Maintenance budget for delete-style mutations (vertex/edge delete).
///
/// On canisters, bounded by the timer budget so a single delete message cannot
/// trap on a large reclamation backlog; the maintenance timer (ADR 0020) drains
/// the remainder. Native builds drain fully so tests observe reclaimed state.
#[cfg(target_family = "wasm")]
#[inline]
pub const fn delete_maintenance_budget() -> MaintenanceBudget {
    timer_lara_maintenance_budget()
}

#[cfg(not(target_family = "wasm"))]
#[inline]
pub const fn delete_maintenance_budget() -> MaintenanceBudget {
    unlimited_lara_maintenance_budget()
}

/// Best-effort maintenance budget after a local edge insert.
///
/// On canisters, uses the timer budget so bulk ingest in one update message does
/// not trap while still reclaiming overflow on hot vertices incrementally. Native
/// builds drain fully so tests and local benches see dense-eligible buckets.
#[cfg(target_family = "wasm")]
#[inline]
pub const fn post_edge_insert_maintenance_budget() -> MaintenanceBudget {
    timer_lara_maintenance_budget()
}

#[cfg(not(target_family = "wasm"))]
#[inline]
pub const fn post_edge_insert_maintenance_budget() -> MaintenanceBudget {
    unlimited_lara_maintenance_budget()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dynamic_update_budget_leaves_platform_headroom() {
        assert_eq!(
            gleaph_graph_kernel::MAX_DYNAMIC_UPDATE_INSTRUCTIONS
                + gleaph_graph_kernel::UPDATE_CALL_INSTRUCTION_HEADROOM,
            gleaph_graph_kernel::MAX_UPDATE_CALL_INSTRUCTIONS
        );
    }

    #[test]
    fn timer_budget_is_positive_and_below_ic_limit() {
        let b = timer_lara_maintenance_budget();
        assert!(b.max_instructions > 0);
        assert!(b.max_instructions < gleaph_graph_kernel::MAX_UPDATE_CALL_INSTRUCTIONS);
        assert!(b.reserve_instructions < b.max_instructions);
    }
}
