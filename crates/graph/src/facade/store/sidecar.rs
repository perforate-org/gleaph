//! Sidecar coordination: derived edge state cleared or moved across domains.

use ic_stable_lara::{
    DeferredBidirectionalLabeledError, VertexId,
    labeled::{EdgeSlotMove, LabeledOrientation},
};

use super::GraphStore;
use super::handle::EdgeHandle;
use crate::facade::stable::GRAPH;

impl GraphStore {
    pub(super) fn apply_edge_slot_moves(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moves: impl IntoIterator<Item = EdgeSlotMove>,
    ) {
        for moved in moves {
            Self::move_edge_sidecars(orientation, owner_vertex_id, moved);
        }
    }

    pub(super) fn vertex_has_incident_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.has_incident_edges(vertex_id))
    }

    pub(super) fn commit_clear_edge_sidecars(&self, handle: EdgeHandle) {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        self.commit_clear_edge_local_indexes(handle);
        self.commit_remove_all_edge_properties(handle);
    }

    pub(super) fn clear_edge_sidecars(&self, handle: EdgeHandle) {
        self.commit_clear_edge_sidecars(handle);
    }

    pub(super) fn commit_move_edge_sidecars(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
    ) {
        match orientation {
            LabeledOrientation::Forward => {
                let moved_properties =
                    GraphStore::commit_move_edge_properties(owner_vertex_id, moved);
                GraphStore::commit_move_edge_local_indexes(
                    orientation,
                    owner_vertex_id,
                    moved,
                    &moved_properties,
                );
            }
            LabeledOrientation::Reverse => {
                GraphStore::commit_move_edge_local_indexes(
                    orientation,
                    owner_vertex_id,
                    moved,
                    &[],
                );
            }
        }
    }

    pub(super) fn move_edge_sidecars(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
    ) {
        Self::commit_move_edge_sidecars(orientation, owner_vertex_id, moved);
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

    #[test]
    fn ordered_delete_move_batch_shifts_sidecars_without_overwrite() {
        let store = GraphStore::new();
        let owner = store.insert_vertex().expect("owner");
        let targets = [
            store.insert_vertex().expect("target 0"),
            store.insert_vertex().expect("target 1"),
            store.insert_vertex().expect("target 2"),
        ];
        let handles = targets.map(|target| {
            store
                .insert_directed_edge(owner, target, None)
                .expect("edge")
        });
        let property = store
            .get_or_insert_property_id("weight")
            .expect("property id");
        for (index, handle) in handles.into_iter().enumerate() {
            store
                .set_edge_property(handle, property, Value::Int64(index as i64))
                .expect("set property");
        }

        store.commit_clear_edge_sidecars(handles[0]);
        GraphStore::apply_edge_slot_moves(
            LabeledOrientation::Forward,
            owner,
            [
                EdgeSlotMove {
                    label_id: handles[0].label_id,
                    old_slot_index: 1,
                    new_slot_index: 0,
                },
                EdgeSlotMove {
                    label_id: handles[0].label_id,
                    old_slot_index: 2,
                    new_slot_index: 1,
                },
            ],
        );

        assert_eq!(
            store.edge_property(EdgeHandle::at_slot(owner, handles[0].label_id, 0), property),
            Some(Value::Int64(1))
        );
        assert_eq!(
            store.edge_property(EdgeHandle::at_slot(owner, handles[0].label_id, 1), property),
            Some(Value::Int64(2))
        );
        assert_eq!(
            store.edge_property(EdgeHandle::at_slot(owner, handles[0].label_id, 2), property),
            None
        );
    }
}
