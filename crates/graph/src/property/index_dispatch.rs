//! Routes derived index operations to federated vertex or local edge backends.

use gleaph_graph_kernel::entry::PropertyEntity;

use super::{PropertyValueChange, index_ops_for_value_change};

/// Applies index-maintenance operations implied by a primary-store property change.
pub(crate) fn dispatch_property_index_ops(change: PropertyValueChange<'_>) {
    let indexed = match change.entity {
        PropertyEntity::Vertex(_) => {
            crate::index::registry::is_vertex_property_indexed(change.property_id)
        }
        PropertyEntity::Edge { label_id, .. } => {
            crate::index::registry::should_maintain_edge_posting(label_id, change.property_id)
        }
    };
    if !indexed {
        return;
    }
    let ops = index_ops_for_value_change(change.property_id, change.prev, change.new);
    match change.entity {
        PropertyEntity::Vertex(vertex_id) => {
            for op in ops {
                crate::index::pending::push_vertex_index_op(vertex_id, op);
            }
        }
        PropertyEntity::Edge {
            owner_vertex_id,
            label_id,
            slot_index,
        } => {
            for op in ops {
                crate::index::edge_pending::push_edge_index_op(
                    owner_vertex_id,
                    label_id,
                    slot_index,
                    op,
                );
            }
        }
    }
}
