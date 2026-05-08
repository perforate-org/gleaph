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
/// Synchronous [`crate::GraphStore`] mutation paths that
/// call `drain_deferred_maintenance` keep `max_instructions: 0` so a single
/// message still fully drains when the instruction counter is unused (native
/// tests) or when the graph is small enough not to trap.
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
