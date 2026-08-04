//! Sidecar coordination: derived edge state cleared or moved across domains.

use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_lara::{
    DeferredBidirectionalLabeledError, VertexId,
    labeled::{EdgeSlotMove, LabeledOrientation},
    traits::CsrEdge,
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use crate::facade::stable::GRAPH;
use crate::facade::store::index_build_admission::{FencedTransition, trap_build_fence};
use crate::index::catalog_context::IndexMembershipRef;
use crate::property::{
    PropertyValueChange, dispatch_property_index_ops_for_physical, inline_index_values,
};

impl GraphStore {
    /// Relocates every property of one edge that moved between slots.
    ///
    /// Conceptual layer: an edge slot move carries all of the edge's properties with it,
    /// whether they physically live inline in the LARA row (ADR 0008 inline profile) or as
    /// sidecar rows in `EDGE_PROPERTIES`. This function makes that invariant explicit: it
    /// physically relocates the sidecar rows and then re-points every indexed property value
    /// (inline and sidecar alike) from the old slot to the new slot through the ordinary
    /// property-change fence and dispatch. The derived property index is one consumer of those
    /// property changes; the caller does not need to know which properties are inline and which
    /// are sidecars.
    ///
    /// The canonical LARA slot move has already happened when this runs, so a fence rejection
    /// must trap and let the IC roll the whole message back.
    pub(super) fn relocate_edge_properties_for_move(
        &self,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        let old_handle = EdgeHandle::at_slot(owner_vertex_id, moved.label_id, moved.old_slot_index);
        let new_handle = EdgeHandle::at_slot(owner_vertex_id, moved.label_id, moved.new_slot_index);
        let label_id = moved.label_id.raw();

        // Uniform enumeration of the moved edge's indexed property values. Physical storage is
        // hidden: inline bytes ride the LARA row (read at its new slot), sidecar rows are
        // relocated below and their values are read as a by-product of the move.
        let mut indexed_values: Vec<(IndexMembershipRef, PropertyId, Value)> = Vec::new();
        if let Some((edge, _)) = self.lookup_edge_entry(new_handle)? {
            indexed_values.extend(
                inline_index_values(label_id, edge.edge_inline_property_bytes())
                    .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })?,
            );
        }
        for (property_id, value) in Self::commit_move_edge_properties(owner_vertex_id, moved) {
            for membership in
                crate::index::catalog_context::edge_index_memberships(label_id, property_id)
            {
                indexed_values.push((membership, property_id, value.clone()));
            }
        }

        // Fence-plan the old-slot removals and new-slot insertions as distinct subject
        // transitions; a Sealing membership rejects here and traps (post-canonical-move).
        let mut old_planned = Vec::new();
        let mut new_planned = Vec::new();
        for (membership, property_id, value) in &indexed_values {
            old_planned.extend(
                self.plan_index_build_admission([FencedTransition {
                    property_id: *property_id,
                    prev: Some(value),
                    new: None,
                    membership: *membership,
                }])
                .unwrap_or_else(trap_build_fence),
            );
            new_planned.extend(
                self.plan_index_build_admission([FencedTransition {
                    property_id: *property_id,
                    prev: None,
                    new: Some(value),
                    membership: *membership,
                }])
                .unwrap_or_else(trap_build_fence),
            );
        }

        // Bind each subject and commit (infallible; the outbox admission is a stable write).
        if !old_planned.is_empty() {
            self.commit_index_build_admission(
                mutation_id,
                self.edge_subject_for_handle(old_handle)
                    .unwrap_or_else(trap_build_fence),
                old_planned,
            );
        }
        if !new_planned.is_empty() {
            self.commit_index_build_admission(
                mutation_id,
                self.edge_subject_for_handle(new_handle)
                    .unwrap_or_else(trap_build_fence),
                new_planned,
            );
        }

        // Active postings follow the same property-change semantics; the index consumes them at
        // the batched flush. Re-iterating the identical enumeration after the fence commit must
        // not fail.
        for (membership, property_id, value) in indexed_values {
            dispatch_property_index_ops_for_physical(
                PropertyValueChange::edge(
                    owner_vertex_id,
                    label_id,
                    moved.old_slot_index,
                    property_id,
                    Some(&value),
                    None,
                ),
                membership,
            );
            dispatch_property_index_ops_for_physical(
                PropertyValueChange::edge(
                    owner_vertex_id,
                    label_id,
                    moved.new_slot_index,
                    property_id,
                    None,
                    Some(&value),
                ),
                membership,
            );
        }
        Ok(())
    }

    /// Applies every edge slot move produced by one canonical removal.
    ///
    /// Forward (canonical) moves relocate the edge's properties and their derived postings;
    /// reverse moves are owned by LARA and carry no canonical sidecars or postings.
    pub(super) fn apply_edge_slot_moves(
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moves: impl IntoIterator<Item = EdgeSlotMove>,
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        if orientation == LabeledOrientation::Forward {
            for moved in moves {
                GraphStore::new().relocate_edge_properties_for_move(
                    owner_vertex_id,
                    moved,
                    mutation_id,
                )?;
            }
        }
        Ok(())
    }

    pub(super) fn vertex_has_incident_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<bool, DeferredBidirectionalLabeledError> {
        GRAPH.with_borrow(|graph| graph.has_incident_edges(vertex_id))
    }

    pub(super) fn commit_clear_edge_sidecars(&self, handle: EdgeHandle) {
        let handle = self.canonical_edge_handle_for_sidecar(handle);
        self.commit_clear_edge_sidecars_at_canonical(handle);
    }

    pub(super) fn commit_clear_edge_sidecars_at_canonical(&self, handle: EdgeHandle) {
        self.commit_clear_edge_local_indexes_at_canonical(handle);
        self.commit_remove_all_edge_properties_at_canonical(handle);
    }

    pub(super) fn clear_edge_sidecars(&self, handle: EdgeHandle) {
        self.commit_clear_edge_sidecars(handle);
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
            .set_edge_property(
                handle.occurrence(LabeledOrientation::Forward),
                property,
                Value::Int64(7),
            )
            .expect("set property");
        assert_eq!(
            store
                .edge_property(handle.occurrence(LabeledOrientation::Forward), property)
                .unwrap(),
            Some(Value::Int64(7))
        );

        store.commit_clear_edge_sidecars(handle);

        assert_eq!(
            store
                .edge_property(handle.occurrence(LabeledOrientation::Forward), property)
                .unwrap(),
            None
        );
        assert!(
            store
                .edge_properties(handle.occurrence(LabeledOrientation::Forward))
                .unwrap()
                .is_empty()
        );
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
                .set_edge_property(
                    handle.occurrence(LabeledOrientation::Forward),
                    property,
                    Value::Int64(index as i64),
                )
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
            0,
        )
        .expect("rekey inline index slots");

        assert_eq!(
            store
                .edge_property(
                    EdgeHandle::at_slot(owner, handles[0].label_id, 0)
                        .occurrence(LabeledOrientation::Forward),
                    property
                )
                .unwrap(),
            Some(Value::Int64(1))
        );
        assert_eq!(
            store
                .edge_property(
                    EdgeHandle::at_slot(owner, handles[0].label_id, 1)
                        .occurrence(LabeledOrientation::Forward),
                    property
                )
                .unwrap(),
            Some(Value::Int64(2))
        );
        assert_eq!(
            store
                .edge_property(
                    EdgeHandle::at_slot(owner, handles[0].label_id, 2)
                        .occurrence(LabeledOrientation::Forward),
                    property
                )
                .unwrap(),
            None
        );
    }
}
