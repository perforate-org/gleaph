//! Edge inline payload updates (schema from router wire per ADR 0008).

use gleaph_graph_kernel::entry::{EdgeLabelId, EdgePayloadProfile, EdgeTarget, EdgeWeightProfile};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{catalog_edge_label_from_wire, validate_edge_payload_bytes_for_label};
use ic_stable_lara::traits::CsrEdge;

impl GraphStore {
    pub fn edge_label_weight_profile(&self, label: EdgeLabelId) -> Option<EdgeWeightProfile> {
        let profile = crate::edge_payload_schema::lookup_edge_payload_profile(label);
        profile.to_weight_profile()
    }

    pub fn edge_label_payload_profile(&self, label: EdgeLabelId) -> Option<EdgePayloadProfile> {
        let profile = crate::edge_payload_schema::lookup_edge_payload_profile(label);
        if profile.required_byte_width() == 0 {
            None
        } else {
            Some(profile)
        }
    }

    /// Updates the inline edge-payload bytes at `handle`.
    pub(super) fn commit_update_edge_payload_at_handle(
        &self,
        handle: EdgeHandle,
        payload_bytes: &[u8],
    ) -> Result<(), GraphStoreError> {
        let catalog_label = catalog_edge_label_from_wire(handle.label_id);
        validate_edge_payload_bytes_for_label(catalog_label, payload_bytes)?;

        let reverse_canonical = self.canonical_reverse_in_edge_handle(handle);
        let forward = if reverse_canonical != handle {
            reverse_canonical
        } else {
            self.canonical_edge_handle(handle)
        };

        let (edge, _) = self
            .lookup_edge_entry(forward)?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: forward.owner_vertex_id,
                label_id: forward.label_id,
                slot_index: forward.slot_index,
            })?;
        let new_edge = edge.with_payload_bytes(payload_bytes);

        let mut updated = self
            .with_graph_mut(|graph| {
                graph.update_forward_edge_payload_at_slot(
                    forward.owner_vertex_id,
                    forward.label_id,
                    forward.slot_index,
                    new_edge.clone(),
                )
            })
            .map_err(GraphStoreError::from)?;
        if updated {
            if forward.label_id.is_directed() {
                if let Some(EdgeTarget::Local(target)) = edge.edge_target()
                    && let Some(reverse) = self.find_reverse_alias_for_canonical(
                        forward,
                        target,
                        forward.owner_vertex_id,
                    )?
                {
                    updated |= self
                        .with_graph_mut(|graph| {
                            graph.update_reverse_edge_payload_at_slot(
                                reverse.owner_vertex_id,
                                reverse.label_id,
                                reverse.slot_index,
                                new_edge.clone(),
                            )
                        })
                        .map_err(GraphStoreError::from)?;
                }
            } else if let Some(EdgeTarget::Local(alias_owner)) = edge.edge_target()
                && alias_owner != forward.owner_vertex_id
                && let Some(alias) = self.find_first_forward_handle_descending(
                    alias_owner,
                    forward.label_id,
                    |edge| edge.neighbor_vid() == forward.owner_vertex_id,
                )?
            {
                updated |= self
                    .with_graph_mut(|graph| {
                        graph.update_forward_edge_payload_at_slot(
                            alias.owner_vertex_id,
                            alias.label_id,
                            alias.slot_index,
                            new_edge.clone(),
                        )
                    })
                    .map_err(GraphStoreError::from)?;
            }
        } else {
            let reverse = self.canonical_reverse_in_edge_handle(handle);
            updated = self
                .with_graph_mut(|graph| {
                    graph.update_reverse_edge_payload_at_slot(
                        reverse.owner_vertex_id,
                        reverse.label_id,
                        reverse.slot_index,
                        new_edge.clone(),
                    )
                })
                .map_err(GraphStoreError::from)?;
        }
        if !updated {
            return Err(GraphStoreError::EdgeNotFound {
                owner_vertex_id: handle.owner_vertex_id,
                label_id: handle.label_id,
                slot_index: handle.slot_index,
            });
        }

        Ok(())
    }
}
