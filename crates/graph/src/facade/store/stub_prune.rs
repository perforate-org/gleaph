//! Source-local edge removal for forwarding-stub cleanup (no counterpart mutation).

use super::super::stable::{EDGE_ALIASES, EDGE_PROPERTIES};
use crate::index::edge_equal;
use ic_stable_lara::{VertexId, labeled::BucketLabelKey as LaraLabelId, traits::CsrEdgeTombstone};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
    pub(crate) fn clear_stub_local_edge_sidecars(&self, handle: EdgeHandle) {
        edge_equal::remove_all_for_edge(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index,
        );
        EDGE_PROPERTIES.with_borrow_mut(|store| {
            store.remove_all_for_edge(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
        EDGE_ALIASES.with_borrow_mut(|aliases| {
            aliases.remove(
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
    }

    /// Tombstone a forward edge on `owner` and clear owner-local sidecars only.
    pub(crate) fn prune_stub_forward_edge_at_slot(
        &self,
        owner: VertexId,
        label: LaraLabelId,
        slot: u32,
    ) -> Result<bool, GraphStoreError> {
        let handle = EdgeHandle::at_slot(owner, label, slot);
        let Some(edge) = self
            .find_outgoing_edge_record(handle)?
            .filter(|e| !e.is_tombstone_edge())
        else {
            return Ok(false);
        };
        self.clear_stub_local_edge_sidecars(handle);
        self.unregister_remote_forward_in_for_handle(handle);
        self.with_graph_mut(|graph| {
            graph
                .remove_forward_edge_at_slot(
                    handle.owner_vertex_id,
                    handle.label_id,
                    handle.slot_index,
                )
                .map_err(GraphStoreError::from)
        })?;
        let _ = edge;
        Ok(true)
    }

    /// Tombstone a reverse edge on `row_vertex_id` and clear owner-local sidecars only.
    pub(crate) fn prune_stub_reverse_edge_at_slot(
        &self,
        row_vertex_id: VertexId,
        label: LaraLabelId,
        slot: u32,
    ) -> Result<bool, GraphStoreError> {
        let handle = EdgeHandle::at_slot(row_vertex_id, label, slot);
        self.clear_stub_local_edge_sidecars(handle);
        let removed = self.with_graph_mut(|graph| {
            graph
                .remove_reverse_edge_at_slot(
                    handle.owner_vertex_id,
                    handle.label_id,
                    handle.slot_index,
                )
                .map_err(GraphStoreError::from)
        })?;
        Ok(removed.is_some_and(|e| !e.is_tombstone_edge()))
    }
}
