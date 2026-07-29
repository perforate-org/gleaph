//! Routes derived index operations to federated vertex or local edge backends.

use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{PropertyEntity, PropertyId};
use ic_stable_lara::VertexId;

use super::{PropertyValueChange, index_ops_for_value_change};

/// Applies index-maintenance operations implied by a primary-store property change.
pub(crate) fn dispatch_property_index_ops(change: PropertyValueChange<'_>) {
    let indexed = match change.entity {
        PropertyEntity::Vertex(_) => {
            crate::index::catalog_context::is_vertex_property_indexed(change.property_id)
        }
        PropertyEntity::Edge { label_id, .. } => {
            crate::index::catalog_context::should_maintain_edge_posting(
                label_id,
                change.property_id,
            )
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

/// Dispatches vertex property changes while borrowing the pending queue once per batch.
pub(crate) fn dispatch_vertex_property_index_ops_bulk<'a>(
    changes: &[(VertexId, PropertyId, Option<&'a Value>, &'a Value)],
) {
    let mut pending = Vec::new();
    for (vertex_id, property_id, previous, value) in changes {
        if !crate::index::catalog_context::is_vertex_property_indexed(*property_id) {
            continue;
        }
        pending.push((
            *vertex_id,
            index_ops_for_value_change(*property_id, *previous, Some(*value)),
        ));
    }
    for (vertex_id, ops) in pending {
        crate::index::pending::push_vertex_index_ops(vertex_id, ops);
    }
}
