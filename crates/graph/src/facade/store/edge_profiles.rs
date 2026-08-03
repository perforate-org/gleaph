//! Edge inline property bytes updates (schema from router wire per ADR 0008).

use gleaph_graph_kernel::entry::{EdgeInlinePropertyProfile, EdgeLabelId};
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_lara::{
    labeled::{CanonicalEdgeOccurrence, LabeledOrientation},
    traits::CsrEdge,
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{catalog_edge_label_from_wire, validate_edge_inline_property_bytes_for_label};
use crate::facade::store::index_build_admission::{
    FencedTransition, PlannedBuildEnvelope, trap_build_fence, trap_post_fence_commit,
};
use crate::property::{
    PropertyValueChange, dispatch_property_index_ops_for_physical, inline_index_values,
};

impl GraphStore {
    pub(super) fn rekey_inline_scalar_index_for_move(
        &self,
        owner_vertex_id: ic_stable_lara::VertexId,
        moved: ic_stable_lara::labeled::EdgeSlotMove,
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        let old_handle = EdgeHandle::at_slot(owner_vertex_id, moved.label_id, moved.old_slot_index);
        let new_handle = EdgeHandle::at_slot(owner_vertex_id, moved.label_id, moved.new_slot_index);
        let Some((edge, _)) = self.lookup_edge_entry(new_handle)? else {
            return Ok(());
        };
        let mut old_planned = Vec::new();
        let mut new_planned = Vec::new();
        for (membership, property_id, value) in
            inline_index_values(moved.label_id.raw(), edge.edge_inline_property_bytes())
                .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })?
        {
            old_planned.extend(
                self.plan_index_build_admission([FencedTransition {
                    property_id,
                    prev: Some(&value),
                    new: None,
                    membership,
                }])
                .unwrap_or_else(trap_build_fence),
            );
            new_planned.extend(
                self.plan_index_build_admission([FencedTransition {
                    property_id,
                    prev: None,
                    new: Some(&value),
                    membership,
                }])
                .unwrap_or_else(trap_build_fence),
            );
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
        // The fence commits above are the first stable writes of this slot move, so the final
        // re-decode (identical to the pre-commit decode of the same live row) must trap rather
        // than return a recoverable error after the durable admission.
        for (membership, property_id, value) in
            inline_index_values(moved.label_id.raw(), edge.edge_inline_property_bytes())
                .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })
                .unwrap_or_else(trap_post_fence_commit)
        {
            dispatch_property_index_ops_for_physical(
                PropertyValueChange::edge(
                    owner_vertex_id,
                    moved.label_id.raw(),
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
                    moved.label_id.raw(),
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

    pub fn edge_label_inline_property_profile(
        &self,
        label: EdgeLabelId,
    ) -> Option<EdgeInlinePropertyProfile> {
        let profile =
            crate::edge_inline_property_schema::lookup_edge_inline_property_profile(label);
        if profile.required_byte_width() == 0 {
            None
        } else {
            Some(profile)
        }
    }

    /// Updates the inline edge-inline-property-bytes bytes at `handle`.
    pub(super) fn commit_update_edge_inline_property_at_handle(
        &self,
        handle: EdgeHandle,
        inline_property_bytes: &[u8],
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        self.commit_update_edge_inline_property_at_occurrence(
            handle.occurrence(LabeledOrientation::Forward),
            inline_property_bytes,
            mutation_id,
        )
    }

    pub(super) fn commit_update_edge_inline_property_at_occurrence(
        &self,
        occurrence: CanonicalEdgeOccurrence,
        inline_property_bytes: &[u8],
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        let catalog_label = catalog_edge_label_from_wire(occurrence.label_id);
        validate_edge_inline_property_bytes_for_label(catalog_label, inline_property_bytes)?;

        let counterpart = self.counterpart_edge_occurrence(occurrence)?;
        let canonical = ic_stable_lara::bidirectional::counterpart::canonical_from_counterpart(
            occurrence,
            counterpart,
        );
        let canonical = EdgeHandle::at_slot(
            canonical.owner_vertex_id,
            canonical.label_id,
            canonical.slot_index,
        );
        let canonical_occurrence = canonical.occurrence(LabeledOrientation::Forward);

        let (edge, _) =
            self.lookup_edge_entry(canonical)?
                .ok_or(GraphStoreError::EdgeNotFound {
                    owner_vertex_id: canonical.owner_vertex_id,
                    label_id: canonical.label_id,
                    slot_index: canonical.slot_index.raw(),
                })?;
        let new_edge = edge.clone().with_stored_inline_property_bytes(
            u16::try_from(inline_property_bytes.len()).map_err(|_| {
                GraphStoreError::InvalidEdgeInlinePropertyBytesWidth(inline_property_bytes.len())
            })?,
            inline_property_bytes,
        );
        let previous_index_values =
            inline_index_values(canonical.label_id.raw(), edge.edge_inline_property_bytes())
                .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })?;
        let new_index_values = inline_index_values(canonical.label_id.raw(), inline_property_bytes)
            .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })?;

        let update_occurrence = |occurrence: CanonicalEdgeOccurrence| {
            self.with_graph_mut(|graph| match occurrence.orientation {
                LabeledOrientation::Forward => graph.update_forward_edge_inline_property_at_slot(
                    occurrence.owner_vertex_id,
                    occurrence.label_id,
                    occurrence.slot_index.raw(),
                    new_edge.clone(),
                ),
                LabeledOrientation::Reverse => graph.update_reverse_edge_inline_property_at_slot(
                    occurrence.owner_vertex_id,
                    occurrence.label_id,
                    occurrence.slot_index.raw(),
                    new_edge.clone(),
                ),
            })
            .map_err(GraphStoreError::from)
        };

        let mut transitions = Vec::new();
        for (membership, property_id, previous) in &previous_index_values {
            let next = new_index_values
                .iter()
                .find(|(next_membership, next_id, _)| {
                    *next_membership == *membership && *next_id == *property_id
                })
                .map(|(_, _, value)| value);
            transitions.push(FencedTransition {
                property_id: *property_id,
                prev: Some(previous),
                new: next,
                membership: *membership,
            });
        }
        let planned: Vec<PlannedBuildEnvelope> = self.plan_index_build_admission(transitions)?;
        if !planned.is_empty() {
            self.commit_index_build_admission(
                mutation_id,
                self.edge_subject_for_handle(canonical)?,
                planned,
            );
        }

        // The fence commit (or the canonical update that follows) is the first stable write of
        // this mutation, so no recoverable error may be returned past this point: the IC would
        // keep the reserved outbox admission while skipping the canonical inline update. The
        // canonical handle and its counterpart were resolved live above; a trap rolls the whole
        // message back instead of exposing canonical state without its build-DML.
        assert!(
            update_occurrence(canonical_occurrence).unwrap_or_else(trap_post_fence_commit),
            "edge resolved live at fence plan must remain updatable within the same message"
        );
        let mirrored = if occurrence == canonical_occurrence {
            counterpart
        } else {
            occurrence
        };
        if mirrored != canonical_occurrence {
            assert!(
                update_occurrence(mirrored).unwrap_or_else(trap_post_fence_commit),
                "edge counterpart resolved live at fence plan must remain updatable within the same message"
            );
        }

        for (membership, property_id, previous) in previous_index_values {
            let next = new_index_values
                .iter()
                .find(|(next_membership, next_id, _)| {
                    *next_membership == membership && *next_id == property_id
                })
                .map(|(_, _, value)| value);
            dispatch_property_index_ops_for_physical(
                PropertyValueChange::edge(
                    canonical.owner_vertex_id,
                    canonical.label_id.raw(),
                    canonical.slot_index.raw(),
                    property_id,
                    Some(&previous),
                    next,
                ),
                membership,
            );
        }

        Ok(())
    }
}
