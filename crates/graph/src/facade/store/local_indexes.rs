//! Local index domain; edge property postings live on graph-index (ADR 0009).

use crate::index::edge_pending;
use crate::property::{PropertyValueChange, dispatch_property_index_ops};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use ic_stable_lara::{
    VertexId,
    labeled::{EdgeSlotMove, LabeledOrientation},
};

use super::GraphStore;
use super::handle::EdgeHandle;
impl GraphStore {
    pub(super) fn commit_remove_all_edge_equality_postings(
        &self,
        _owner_vertex_id: VertexId,
        _label_id: u16,
        _slot_index: u32,
    ) {
        // Edge equality postings live on graph-index (ADR 0009); removals enqueue via edge_pending.
    }

    pub(super) fn commit_clear_edge_local_indexes_at_canonical(&self, handle: EdgeHandle) {
        edge_pending::enqueue_removals_for_edge(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index.raw(),
        );
    }

    pub(super) fn commit_move_edge_local_indexes(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
        moved_properties: &[(PropertyId, Value)],
    ) {
        let label_id = moved.label_id.raw();
        match orientation {
            LabeledOrientation::Forward => {
                for (property_id, value) in moved_properties {
                    dispatch_property_index_ops(PropertyValueChange::edge(
                        owner_vertex_id,
                        label_id,
                        moved.old_slot_index,
                        *property_id,
                        Some(value),
                        None,
                    ));
                    dispatch_property_index_ops(PropertyValueChange::edge(
                        owner_vertex_id,
                        label_id,
                        moved.new_slot_index,
                        *property_id,
                        None,
                        Some(value),
                    ));
                }
            }
            LabeledOrientation::Reverse => {
                // Reverse slot movement is owned by LARA. Canonical Graph sidecars and
                // property postings are forward-owned and do not move with reverse rows.
            }
        }
    }
}
