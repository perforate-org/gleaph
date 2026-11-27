//! Sidecar coordination: derived edge state cleared or moved across domains.

use crate::facade::topology::TopologyError;
use crate::facade::topology::change::EdgeSlotMove;
use gleaph_graph_kernel::entry::{
    EdgeOrder, EdgeOrientation, EdgeSlotIndex, PropertyId, TaggedEdgeLabelId, VertexId,
};
use gleaph_graph_kernel::plan_exec::MutationId;

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use crate::facade::store::index_build_admission::{FencedTransition, trap_build_fence};
use crate::index::catalog_context::IndexMembershipRef;
use crate::property::{
    IndexedInlineValue, InlineIndexing, PropertyValueChange,
    dispatch_property_index_ops_for_physical,
};
use std::collections::BTreeMap;

impl GraphStore {
    /// Relocates every property of one edge that moved between slots, given the indexed values
    /// its inline bytes hold at the new slot (read for the whole step by
    /// [`Self::apply_edge_slot_moves`]).
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
    fn relocate_edge_properties_for_move(
        &self,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
        inline_values: Vec<IndexedInlineValue>,
        memberships: &mut EdgeMemberships,
        mutation_id: MutationId,
    ) {
        let old_handle = EdgeHandle::at_slot(owner_vertex_id, moved.label_id, moved.old_slot_index);
        let new_handle = EdgeHandle::at_slot(owner_vertex_id, moved.label_id, moved.new_slot_index);
        let label_id = moved.label_id.raw();

        // Uniform enumeration of the moved edge's indexed property values. Physical storage is
        // hidden: inline values ride the LARA row (read at its new slot), sidecar rows are
        // relocated below and their values are read as a by-product of the move.
        let mut indexed_values = inline_values;
        for (property_id, value) in Self::commit_move_edge_properties(owner_vertex_id, moved) {
            for &membership in memberships.of(label_id, property_id) {
                indexed_values.push((membership, property_id, value.clone()));
            }
        }
        if indexed_values.is_empty() {
            return;
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
        // the batched flush.
        for (membership, property_id, value) in indexed_values {
            for (slot_index, prev, new) in [
                (moved.old_slot_index, Some(&value), None),
                (moved.new_slot_index, None, Some(&value)),
            ] {
                dispatch_property_index_ops_for_physical(
                    PropertyValueChange::edge(
                        owner_vertex_id,
                        label_id,
                        slot_index,
                        property_id,
                        prev,
                        new,
                    ),
                    membership,
                );
            }
        }
    }

    /// Applies the canonical slot moves one compaction step made
    /// ([`crate::facade::topology::work::WorkStep::Compacted`]), from the lowest old position.
    pub(super) fn apply_slot_moves(
        moves: crate::facade::topology::change::SlotMoves,
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        let (vertex_id, moves) = moves.into_parts();
        Self::apply_edge_slot_moves(vertex_id, &moves, mutation_id)
    }

    /// Relocates each moved canonical half's properties and postings, in the order given. The
    /// moves are one bucket's, so of one label: what indexes it is resolved once, and the inline
    /// values at the new slots are read in one batch, only when an index takes them. Read one
    /// edge at a time, with the label resolved for each, they cost about 4K instructions a move
    /// (`timer_maintenance_single_step_hub_2048`) for a label no index takes.
    fn apply_edge_slot_moves(
        owner_vertex_id: VertexId,
        moves: &[EdgeSlotMove],
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        let Some(label) = moves.first().map(|moved| moved.label_id) else {
            return Ok(());
        };
        debug_assert!(
            moves.iter().all(|moved| moved.label_id == label),
            "one step's moves are one bucket's"
        );
        let mut inline_values = moved_inline_values(owner_vertex_id, label, moves)?;
        let mut memberships = EdgeMemberships::default();
        let store = GraphStore::new();
        for (index, &moved) in moves.iter().enumerate() {
            let values = inline_values
                .as_mut()
                .map(|values| core::mem::take(&mut values[index]))
                .unwrap_or_default();
            store.relocate_edge_properties_for_move(
                owner_vertex_id,
                moved,
                values,
                &mut memberships,
                mutation_id,
            );
        }
        Ok(())
    }

    pub(super) fn vertex_has_incident_edges(
        &self,
        vertex_id: VertexId,
    ) -> Result<bool, TopologyError> {
        crate::facade::topology::read::has_incident_edges(vertex_id)
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

/// The indexed inline values at each move's new slot, in the moves' order; `None` when no index
/// takes `label`'s inline values.
fn moved_inline_values(
    owner_vertex_id: VertexId,
    label: TaggedEdgeLabelId,
    moves: &[EdgeSlotMove],
) -> Result<Option<Vec<Vec<IndexedInlineValue>>>, GraphStoreError> {
    let Some(indexing) = InlineIndexing::of(label.raw()) else {
        return Ok(None);
    };
    let slots: Vec<EdgeSlotIndex> = moves
        .iter()
        .map(|moved| EdgeSlotIndex::from_raw(moved.new_slot_index))
        .collect();
    let mut bytes_at = BTreeMap::new();
    crate::facade::topology::read::read_slots(
        owner_vertex_id,
        EdgeOrientation::Forward,
        label,
        &slots,
        EdgeOrder::Ascending,
        |edge| {
            bytes_at.insert(
                edge.edge_slot_index.raw(),
                edge.edge_inline_property_bytes().to_vec(),
            );
        },
    )?;
    moves
        .iter()
        .map(|moved| {
            bytes_at
                .get(&moved.new_slot_index)
                .map_or_else(|| Ok(Vec::new()), |bytes| indexing.values(bytes))
                .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })
        })
        .collect::<Result<_, _>>()
        .map(Some)
}

/// The edge index memberships of each property of one label, looked up once per property.
#[derive(Default)]
struct EdgeMemberships(BTreeMap<PropertyId, Vec<IndexMembershipRef>>);

impl EdgeMemberships {
    fn of(&mut self, label_id: u16, property_id: PropertyId) -> &[IndexMembershipRef] {
        self.0.entry(property_id).or_insert_with(|| {
            crate::index::catalog_context::edge_index_memberships(label_id, property_id)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::Value;
    use gleaph_graph_kernel::entry::EdgeOrientation;

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
                handle.occurrence(EdgeOrientation::Forward),
                property,
                Value::Int64(7),
            )
            .expect("set property");
        assert_eq!(
            store
                .edge_property(handle.occurrence(EdgeOrientation::Forward), property)
                .unwrap(),
            Some(Value::Int64(7))
        );

        store.commit_clear_edge_sidecars(handle);

        assert_eq!(
            store
                .edge_property(handle.occurrence(EdgeOrientation::Forward), property)
                .unwrap(),
            None
        );
        assert!(
            store
                .edge_properties(handle.occurrence(EdgeOrientation::Forward))
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
                    handle.occurrence(EdgeOrientation::Forward),
                    property,
                    Value::Int64(index as i64),
                )
                .expect("set property");
        }

        store.commit_clear_edge_sidecars(handles[0]);
        GraphStore::apply_edge_slot_moves(
            owner,
            &[
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
            0.into(),
        )
        .expect("rekey inline index slots");

        assert_eq!(
            store
                .edge_property(
                    EdgeHandle::at_slot(owner, handles[0].label_id, 0)
                        .occurrence(EdgeOrientation::Forward),
                    property
                )
                .unwrap(),
            Some(Value::Int64(1))
        );
        assert_eq!(
            store
                .edge_property(
                    EdgeHandle::at_slot(owner, handles[0].label_id, 1)
                        .occurrence(EdgeOrientation::Forward),
                    property
                )
                .unwrap(),
            Some(Value::Int64(2))
        );
        assert_eq!(
            store
                .edge_property(
                    EdgeHandle::at_slot(owner, handles[0].label_id, 2)
                        .occurrence(EdgeOrientation::Forward),
                    property
                )
                .unwrap(),
            None
        );
    }
}
