//! Sidecar coordination: derived edge state cleared or moved across domains.

use ic_stable_lara::{
    DeferredBidirectionalLabeledError, VertexId,
    labeled::{EdgeSlotMove, LabeledOrientation},
    traits::CsrEdge,
};

use super::GraphStore;
use super::handle::EdgeHandle;
use super::helpers::canonical_undirected_owner;
use crate::facade::stable::GRAPH;
use gleaph_graph_kernel::entry::Edge;

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

    pub(super) fn commit_clear_edge_sidecars(&self, handle: EdgeHandle) {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        self.commit_clear_edge_local_indexes(handle);
        self.commit_remove_all_edge_properties(handle);
    }

    pub(super) fn clear_edge_sidecars(&self, handle: EdgeHandle) {
        self.commit_clear_edge_sidecars(handle);
    }

    pub(super) fn commit_move_edge_sidecars_for_compaction(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
    ) {
        match orientation {
            LabeledOrientation::Forward => {
                let moved_properties =
                    GraphStore::commit_move_edge_properties_for_compaction(owner_vertex_id, moved);
                GraphStore::commit_move_edge_local_indexes_for_compaction(
                    orientation,
                    owner_vertex_id,
                    moved,
                    &moved_properties,
                );
            }
            LabeledOrientation::Reverse => {
                GraphStore::commit_move_edge_local_indexes_for_compaction(
                    orientation,
                    owner_vertex_id,
                    moved,
                    &[],
                );
            }
        }
    }

    pub(super) fn move_edge_sidecars_for_compaction(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
    ) {
        Self::commit_move_edge_sidecars_for_compaction(orientation, owner_vertex_id, moved);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;

    #[test]
    fn commit_clear_edge_sidecars_removes_properties_and_local_indexes() {
        let store = GraphStore::new();
        let a = store.insert_vertex().expect("a");
        let b = store.insert_vertex().expect("b");
        let handle = store.insert_directed_edge(a, b, None).expect("edge");
        let property = store
            .get_or_insert_property_id("weight")
            .expect("property id");
        store
            .set_edge_property(handle, property, Value::Int64(7))
            .expect("set property");
        assert_eq!(store.edge_property(handle, property), Some(Value::Int64(7)));

        store.commit_clear_edge_sidecars(handle);

        assert_eq!(store.edge_property(handle, property), None);
        assert!(store.edge_properties(handle).is_empty());
    }
}
