//! Vertex delete domain: clear derived sidecars and commit graph row removal.

use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledError, VertexId,
    labeled::OutEdgeOrder,
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use crate::facade::GRAPH_MAX_SYNC_DETACH_DELETE_DEGREE;

impl GraphStore {
    /// Detached vertex delete: clear sidecars, remove CSR row, drain maintenance.
    pub(super) fn commit_delete_detached_vertex(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        self.assert_local_vertex_writable(vertex_id)?;
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        if self.vertex_has_incident_edges(vertex_id)? {
            return Err(GraphStoreError::VertexNotDetached { vertex_id });
        }
        self.commit_prepare_vertex_sidecars_for_delete(vertex_id)?;
        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))?;
        self.drain_deferred_maintenance()
    }

    /// Detach-delete: clear vertex sidecars, remove CSR row, clear incident edge sidecars.
    pub(super) fn commit_detach_delete_vertex(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        self.commit_detach_delete_vertex_bounded(vertex_id, GRAPH_MAX_SYNC_DETACH_DELETE_DEGREE)
    }

    /// Detach-delete with an explicit synchronous incident-degree ceiling.
    ///
    /// ADR 0021 Stage 0 safety floor: incident-edge removal here is O(degree) and
    /// synchronous (each removal also touches the neighbour's counterpart row and
    /// clears that edge's sidecars). Above `max_incident_degree` we return a
    /// recoverable error *before any mutation* rather than risk an instruction
    /// trap that would leave the vertex permanently undeletable. Stage 2 makes
    /// this resumable and removes the ceiling.
    pub(crate) fn commit_detach_delete_vertex_bounded(
        &self,
        vertex_id: VertexId,
        max_incident_degree: u64,
    ) -> Result<(), GraphStoreError> {
        self.assert_local_vertex_writable(vertex_id)?;
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        let incident_degree = self.vertex_incident_degree(vertex_id)?;
        if incident_degree > max_incident_degree {
            return Err(GraphStoreError::VertexDeleteTooLarge {
                vertex_id,
                incident_degree,
                limit: max_incident_degree,
            });
        }
        self.commit_prepare_vertex_sidecars_for_delete(vertex_id)?;

        let to_clear = self.collect_incident_edge_handles_for_delete(vertex_id)?;
        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))?;
        for handle in to_clear {
            self.commit_clear_edge_sidecars(handle);
        }
        self.drain_deferred_maintenance()
    }

    /// Property and label sidecars before a vertex CSR row is removed.
    pub(super) fn commit_prepare_vertex_sidecars_for_delete(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        self.commit_clear_vertex_properties(vertex_id);

        let vertex = self.vertex(vertex_id).ok_or_else(|| {
            GraphStoreError::Graph(DeferredBidirectionalLabeledError::VertexOutOfRange {
                vid: vertex_id,
                len: self.vertex_count(),
            })
        })?;
        // Label sidecars live in `VERTEX_LABELS`; the CSR row is unchanged. Do not call
        // `set_vertex` here: it mirrors the forward row into reverse and would corrupt
        // reverse-only locator state for this `VertexId`.
        self.commit_clear_vertex_labels(vertex_id, vertex)
    }

    fn collect_incident_edge_handles_for_delete(
        &self,
        vertex_id: VertexId,
    ) -> Result<Vec<EdgeHandle>, GraphStoreError> {
        let mut to_clear = Vec::new();
        let mut push_out = |edge: Edge| {
            let owner = self.edge_sidecar_owner_from_out_row(vertex_id, &edge);
            to_clear.push(EdgeHandle {
                owner_vertex_id: owner,
                label_id: LaraLabelId::from_raw(edge.label_id),
                slot_index: edge.edge_slot_index.raw(),
            });
        };
        self.for_each_directed_out_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            push_out(edge);
        })?;
        self.for_each_undirected_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            push_out(edge);
        })?;
        self.for_each_directed_in_edges(vertex_id, OutEdgeOrder::Ascending, |edge| {
            let owner = self.edge_sidecar_owner_from_in_row(vertex_id, &edge);
            to_clear.push(EdgeHandle {
                owner_vertex_id: owner,
                label_id: LaraLabelId::from_raw(edge.label_id),
                slot_index: edge.edge_slot_index.raw(),
            });
        })?;
        to_clear.sort_unstable_by_key(|h| {
            (u32::from(h.owner_vertex_id), h.label_id.raw(), h.slot_index)
        });
        to_clear.dedup_by_key(|h| (u32::from(h.owner_vertex_id), h.label_id.raw(), h.slot_index));
        Ok(to_clear)
    }
}
