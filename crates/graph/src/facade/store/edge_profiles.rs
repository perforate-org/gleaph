//! Edge inline property bytes updates (schema from router wire per ADR 0008).

use gleaph_graph_kernel::entry::{EdgeInlinePropertyProfile, EdgeLabelId};
use ic_stable_lara::{
    labeled::{CanonicalEdgeOccurrence, LabeledOrientation},
    traits::CsrEdge,
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{catalog_edge_label_from_wire, validate_edge_inline_property_bytes_for_label};

impl GraphStore {
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
    ) -> Result<(), GraphStoreError> {
        self.commit_update_edge_inline_property_at_occurrence(
            handle.occurrence(LabeledOrientation::Forward),
            inline_property_bytes,
        )
    }

    pub(super) fn commit_update_edge_inline_property_at_occurrence(
        &self,
        occurrence: CanonicalEdgeOccurrence,
        inline_property_bytes: &[u8],
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

        if !update_occurrence(canonical_occurrence)? {
            return Err(GraphStoreError::EdgeNotFound {
                owner_vertex_id: occurrence.owner_vertex_id,
                label_id: occurrence.label_id,
                slot_index: occurrence.slot_index.raw(),
            });
        }
        let mirrored = if occurrence == canonical_occurrence {
            counterpart
        } else {
            occurrence
        };
        if mirrored != canonical_occurrence && !update_occurrence(mirrored)? {
            return Err(GraphStoreError::EdgeNotFound {
                owner_vertex_id: mirrored.owner_vertex_id,
                label_id: mirrored.label_id,
                slot_index: mirrored.slot_index.raw(),
            });
        }

        Ok(())
    }
}
