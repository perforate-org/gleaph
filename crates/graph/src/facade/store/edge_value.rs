//! Inline edge value updates (CSR value store).

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{catalog_edge_label_from_wire, validate_edge_value_bytes_for_label};
use crate::facade::migration::incremental::journal_edge_value_changed;

impl GraphStore {
    /// Updates the inline edge-value payload at `handle` and journals when the owner is migrating.
    pub(crate) fn update_edge_value_at_handle(
        &self,
        handle: EdgeHandle,
        value_bytes: &[u8],
    ) -> Result<(), GraphStoreError> {
        let catalog_label = catalog_edge_label_from_wire(handle.label_id);
        validate_edge_value_bytes_for_label(self, catalog_label, value_bytes)?;

        let (edge, _) = self
            .lookup_edge_entry(handle)?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: handle.owner_vertex_id,
                label_id: handle.label_id,
                slot_index: handle.slot_index,
            })?;
        let new_edge = edge.with_value_bytes(value_bytes);

        let forward = self.canonical_edge_handle(handle);
        let mut updated = self
            .with_graph_mut(|graph| {
                graph.update_forward_edge_value_at_slot(
                    forward.owner_vertex_id,
                    forward.label_id,
                    forward.slot_index,
                    new_edge,
                )
            })
            .map_err(GraphStoreError::from)?;
        if !updated {
            let reverse = self.canonical_reverse_in_edge_handle(handle);
            updated = self
                .with_graph_mut(|graph| {
                    graph.update_reverse_edge_value_at_slot(
                        reverse.owner_vertex_id,
                        reverse.label_id,
                        reverse.slot_index,
                        new_edge,
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

        journal_edge_value_changed(self, handle, value_bytes)?;
        Ok(())
    }
}
