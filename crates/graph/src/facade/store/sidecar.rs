//! GraphStore sidecar coordination: edge properties plus local indexes.

use super::super::stable::{EDGE_PROPERTIES, GRAPH, REMOTE_FORWARD_IN};
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledError, VertexId,
    labeled::{EdgeSlotMove, LabeledOrientation},
    traits::CsrEdge,
};

use super::GraphStore;
use super::handle::EdgeHandle;
use super::helpers::canonical_undirected_owner;
use gleaph_graph_kernel::entry::{Edge, EdgeTarget};

impl GraphStore {
    pub(super) fn vertex_has_incident_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.has_incident_edges(vertex_id))
    }

    pub(super) fn edge_sidecar_owner_from_out_row(
        &self,
        endpoint: VertexId,
        edge: &Edge,
    ) -> VertexId {
        if self.edge_is_undirected(endpoint, edge).unwrap_or(false) {
            canonical_undirected_owner(endpoint, edge.neighbor_vid())
        } else {
            endpoint
        }
    }

    pub(super) fn clear_edge_sidecars(&self, handle: EdgeHandle) {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        self.commit_clear_edge_local_indexes(handle);
        EDGE_PROPERTIES.with_borrow_mut(|store| {
            store.remove_all_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
    }

    pub(super) fn move_edge_sidecars_for_compaction(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
    ) {
        let label_id = moved.label_id.raw();
        match orientation {
            LabeledOrientation::Forward => {
                let moved_properties = EDGE_PROPERTIES.with_borrow_mut(|store| {
                    store
                        .move_all_for_edge(
                            owner_vertex_id,
                            label_id,
                            moved.old_slot_index,
                            moved.new_slot_index,
                        )
                        .expect("stored edge property values remain encodable")
                });
                Self::commit_move_edge_local_indexes_for_compaction(
                    orientation,
                    owner_vertex_id,
                    moved,
                    &moved_properties,
                );
                let label = LaraLabelId::from_raw(label_id);
                let _ = GRAPH.with_borrow(|graph| {
                    graph.for_each_out_edges_for_label_unchecked(owner_vertex_id, label, |edge| {
                        if edge.edge_slot_index.raw() != moved.new_slot_index {
                            return;
                        }
                        let Some(EdgeTarget::Remote(remote_ref)) = edge.edge_target() else {
                            return;
                        };
                        REMOTE_FORWARD_IN.with_borrow_mut(|index| {
                            index.move_slot(
                                remote_ref,
                                owner_vertex_id,
                                label_id,
                                moved.old_slot_index,
                                moved.new_slot_index,
                            );
                        });
                    })
                });
            }
            LabeledOrientation::Reverse => {
                Self::commit_move_edge_local_indexes_for_compaction(
                    orientation,
                    owner_vertex_id,
                    moved,
                    &[],
                );
            }
        }
    }
}
