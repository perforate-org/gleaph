//! Property value encoding and index-maintenance events.

mod change;
mod dotted_path;
mod index_dispatch;
mod index_key;
mod inline_dispatch;
mod persisted;

pub(crate) use change::{PropertyIndexOp, PropertyValueChange, index_ops_for_value_change};
pub(crate) use dotted_path::{nested_leaf_posting_value, record_value_at_dotted_path};
pub(crate) use index_dispatch::{
    commit_property_index_ops, dispatch_property_index_ops,
    dispatch_property_index_ops_for_physical, dispatch_vertex_property_index_ops_bulk,
    index_build_subject_for_change, preflight_property_index_ops, vertex_posting_transitions,
};
pub(crate) use index_key::sortable_index_key;
pub(crate) use inline_dispatch::{dispatch_inline_index_removals, inline_index_values};
pub(crate) use persisted::{ensure_persistable, ensure_property_id};
