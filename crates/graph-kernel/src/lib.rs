#[cfg(feature = "canbench")]
mod bench;

pub mod bidirectional_catalog;
pub mod canonical_export;
pub mod edge_inline_property_profile_store;
pub mod entry;
pub mod federation;
pub mod gql_dialect;
pub mod index;
pub mod path;
pub mod plan_exec;
pub mod scoped_name_catalog;
pub mod stable_layout;
pub mod stable_memory;
pub mod vector_index;

pub mod provisioning;

/// ICP's per-message instruction ceiling for update calls, heartbeats, and timers.
pub const MAX_UPDATE_CALL_INSTRUCTIONS: u64 = 40_000_000_000;

/// Conservative dynamic query budget used by bounded Graph query execution.
pub const MAX_QUERY_CALL_INSTRUCTIONS: u64 = 5_000_000_000;

/// Instruction headroom reserved below the update-call ceiling for final response and
/// bookkeeping work. Dynamic update budgets must be derived from this value.
pub const UPDATE_CALL_INSTRUCTION_HEADROOM: u64 = 5_000_000_000;

/// Safe dynamic update budget after reserving [`UPDATE_CALL_INSTRUCTION_HEADROOM`].
pub const MAX_DYNAMIC_UPDATE_INSTRUCTIONS: u64 =
    MAX_UPDATE_CALL_INSTRUCTIONS - UPDATE_CALL_INSTRUCTION_HEADROOM;

/// Conservative cost estimate used only for pre-dispatch batch sizing when an endpoint has no
/// resumable per-operation cursor. Measured operation cost remains authoritative for continuation
/// paths.
pub const GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION: u64 = 500_000_000;

/// Headroom reserved by Graph's dynamic batch cutoff for final bookkeeping.
pub const GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM: u64 = 2_000_000_000;

/// Headroom reserved by Router's dynamic batch cutoff for dispatch and finalization.
pub const ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM: u64 = 4_000_000_000;

/// Conservative per-message cap for Graph's deferred LARA maintenance timer.
pub const MAX_TIMER_MAINTENANCE_INSTRUCTIONS: u64 = 32_000_000_000;

/// Reserved instruction headroom inside the timer maintenance cap.
pub const TIMER_MAINTENANCE_INSTRUCTION_HEADROOM: u64 = 100_000_000;

const _: () = {
    assert!(MAX_QUERY_CALL_INSTRUCTIONS < MAX_UPDATE_CALL_INSTRUCTIONS);
    assert!(GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM < MAX_UPDATE_CALL_INSTRUCTIONS);
    assert!(ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM < MAX_UPDATE_CALL_INSTRUCTIONS);
    assert!(TIMER_MAINTENANCE_INSTRUCTION_HEADROOM < MAX_TIMER_MAINTENANCE_INSTRUCTIONS);
    assert!(MAX_TIMER_MAINTENANCE_INSTRUCTIONS < MAX_UPDATE_CALL_INSTRUCTIONS);
};

#[cfg(test)]
mod execution_limit_tests {
    use super::*;

    #[test]
    fn dynamic_update_budget_leaves_shared_headroom() {
        assert_eq!(
            MAX_DYNAMIC_UPDATE_INSTRUCTIONS + UPDATE_CALL_INSTRUCTION_HEADROOM,
            MAX_UPDATE_CALL_INSTRUCTIONS
        );
    }
}
