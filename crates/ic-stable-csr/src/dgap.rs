//! DGAP edge bundle (`M_e`) and PMA metadata (segment density, rebalance), following the reference `graph.h` layout.
//!
//! Persistent layout of the two `M_e` memories is documented under [`crate::layout::dgap`] and
//! [`DgapGraphMemories`] (ASCII diagrams, `ic-stable_structures`-style).

mod dgap_graph_memories;
mod edge_pma_stride;
mod edge_store;
mod pma_meta;

pub use dgap_graph_memories::DgapGraphMemories;
pub use edge_store::{DgapEdgeStore, NeighborhoodIter};
pub use crate::layout::dgap::SegmentEdgeCounts;
pub use pma_meta::{
    LOW_0, LOW_H, RebalanceDecision, UP_0, UP_H, calculate_positions_v1,
    calculate_positions_v1_window, density_deltas, floor_log2_u32, pma_tree_index,
    rebalance_decision, rebalance_weighted, rebalance_weighted_window,
    recount_segment_actual_column, recount_segment_actual_from_degrees, recount_segment_edge_counts_column,
    recount_segment_total, recount_segment_total_column,
};
