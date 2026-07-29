//! Property value encoding and index-maintenance events.

mod change;
mod index_dispatch;
mod index_key;
mod inline_dispatch;
mod persisted;

pub(crate) use change::{PropertyIndexOp, PropertyValueChange, index_ops_for_value_change};
pub(crate) use index_dispatch::{
    dispatch_property_index_ops, dispatch_vertex_property_index_ops_bulk,
};
pub(crate) use index_key::sortable_index_key;
pub(crate) use inline_dispatch::{dispatch_inline_scalar_index_removal, inline_scalar_index_value};
pub(crate) use persisted::{ensure_persistable, ensure_property_id};
