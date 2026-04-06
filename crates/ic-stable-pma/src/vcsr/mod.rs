//! VCSR edge region (`M_e`) and PMA metadata algorithms.

mod edge_store;
mod pma_meta;

pub use edge_store::VcsrEdgeStore;
pub use pma_meta::{
    calculate_positions_v1, calculate_positions_v1_window, density_deltas, floor_log2_u32,
    pma_tree_index, rebalance_decision, rebalance_weighted, rebalance_weighted_window,
    recount_segment_actual_column, recount_segment_actual_from_degrees, recount_segment_total,
    recount_segment_total_column, RebalanceDecision, LOW_0, LOW_H, UP_0, UP_H,
};
