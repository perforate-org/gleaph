//! Filter and limit pushdown optimizations.
//!
//! These optimizations move operators earlier in the pipeline when safe,
//! reducing the number of rows that flow through expensive later stages.

mod filter;
mod fusion;
mod limit;
mod predicate;
mod shortest_path;
mod vars;

pub use filter::{apply_filter_pushdown, can_push_filter_to_stage};
pub use fusion::{apply_ev_fusion, apply_late_project};
pub use limit::{apply_limit_pushdown, apply_topk_fusion};
pub use predicate::apply_predicate_reordering;
pub use shortest_path::apply_shortest_path_binding_pruning;
pub use vars::{collect_variables, collect_variables_ref};
