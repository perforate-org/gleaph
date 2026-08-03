//! Local index domain; edge property postings live on graph-index (ADR 0009).

use crate::facade::store::index_build_admission::{FencedTransition, trap_build_fence};
use crate::index::edge_pending;
use crate::property::{PropertyValueChange, dispatch_property_index_ops};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::PropertyId;
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_lara::{
    VertexId,
    labeled::{EdgeSlotMove, LabeledOrientation},
};

use super::GraphStore;
use super::error::GraphStoreError;
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
        &self,
        orientation: LabeledOrientation,
        owner_vertex_id: VertexId,
        moved: EdgeSlotMove,
        moved_properties: &[(PropertyId, Value)],
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        let label_id = moved.label_id.raw();
        match orientation {
            LabeledOrientation::Forward => {
                // Plan the old-slot removals and new-slot insertions as distinct subject
                // transitions, then bind each subject before the canonical slot move commits.
                let old_handle =
                    EdgeHandle::at_slot(owner_vertex_id, moved.label_id, moved.old_slot_index);
                let new_handle =
                    EdgeHandle::at_slot(owner_vertex_id, moved.label_id, moved.new_slot_index);
                let mut old_planned = Vec::new();
                let mut new_planned = Vec::new();
                for (property_id, value) in moved_properties {
                    for membership in crate::index::catalog_context::edge_index_memberships(
                        label_id,
                        *property_id,
                    ) {
                        old_planned.extend(
                            self.plan_index_build_admission([FencedTransition {
                                property_id: *property_id,
                                prev: Some(value),
                                new: None,
                                membership,
                            }])
                            .unwrap_or_else(trap_build_fence),
                        );
                        new_planned.extend(
                            self.plan_index_build_admission([FencedTransition {
                                property_id: *property_id,
                                prev: None,
                                new: Some(value),
                                membership,
                            }])
                            .unwrap_or_else(trap_build_fence),
                        );
                    }
                }
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
        Ok(())
    }
}
