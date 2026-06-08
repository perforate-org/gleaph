//! Instruction budgets for Internet Computer execution contexts.
//!
//! See [`IC_CANISTER_MESSAGE_INSTRUCTION_LIMIT`] for the platform ceiling this
//! module is aligned with.

use ic_stable_lara::MaintenanceBudget;

/// Documented ICP limit for **instructions per update call / heartbeat / timer**.
///
/// Source: [ICP Cycles Costs — Resource limits](https://docs.internetcomputer.org/references/cycles-costs/#resource-limits)
/// (“Instructions per update call / heartbeat / timer: 40 billion”). If the
/// network documentation changes, update this constant to match.
pub const IC_CANISTER_MESSAGE_INSTRUCTION_LIMIT: u64 = 40_000_000_000;

/// Conservative per-tick cap for LARA deferred maintenance under timer/heartbeat.
///
/// Set below [`IC_CANISTER_MESSAGE_INSTRUCTION_LIMIT`] to leave headroom for
/// non-LARA work in the same message (serialization, logging, etc.).
pub const GRAPH_TIMER_LARA_MAX_INSTRUCTIONS: u64 = 32_000_000_000;

/// Reserved instruction headroom checked against [`GRAPH_TIMER_LARA_MAX_INSTRUCTIONS`]
/// inside LARA's maintenance loop (see `ic-stable-lara` `MaintenanceBudget`).
pub const GRAPH_TIMER_LARA_RESERVE_INSTRUCTIONS: u64 = 100_000_000;

/// [`MaintenanceBudget`] suited for **timer / heartbeat** draining of the
/// deferred LARA queue in production canisters.
///
/// Delete paths call [`crate::GraphStore::drain_deferred_maintenance`] with
/// [`unlimited_lara_maintenance_budget`]. Edge inserts use
/// [`post_edge_insert_maintenance_budget`] instead (timer cap on canisters).
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
/// Used after destructive mutations on native targets and in tests where the
/// instruction counter is unused.
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
    fn timer_budget_is_positive_and_below_ic_limit() {
        let b = timer_lara_maintenance_budget();
        assert!(b.max_instructions > 0);
        assert!(b.max_instructions < IC_CANISTER_MESSAGE_INSTRUCTION_LIMIT);
        assert!(b.reserve_instructions < b.max_instructions);
    }
}
