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

/// Instruction-budget constants owned by `gleaph-instruction-budget` (ADR 0060). Re-exported here
/// so existing callers and the ADR 0042 ownership statement migrate without a churn pass.
pub use gleaph_instruction_budget::{
    GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM,
    GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION, MAX_DYNAMIC_UPDATE_INSTRUCTIONS,
    MAX_QUERY_CALL_INSTRUCTIONS, MAX_TIMER_MAINTENANCE_INSTRUCTIONS, MAX_UPDATE_CALL_INSTRUCTIONS,
    ROUTER_BATCH_CHUNK_WORK_INSTRUCTION_ESTIMATE, ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM,
    TIMER_MAINTENANCE_INSTRUCTION_HEADROOM, UPDATE_CALL_INSTRUCTION_HEADROOM,
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
