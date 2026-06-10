//! GraphStore `delete` implementation.

use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, VertexId, labeled::OutEdgeOrder, traits::CsrEdge,
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
    pub fn delete_vertex(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.assert_local_vertex_writable(vertex_id)?;
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        if self.vertex_has_incident_edges(vertex_id)? {
            return Err(GraphStoreError::VertexNotDetached { vertex_id });
        }
        self.clear_vertex_stable_payloads_before_graph_delete(vertex_id)?;
        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))?;
        self.drain_deferred_maintenance()?;
        Ok(())
    }

    pub fn detach_delete_vertex(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.assert_local_vertex_writable(vertex_id)?;
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        self.clear_vertex_stable_payloads_before_graph_delete(vertex_id)?;

        let mut to_clear: Vec<EdgeHandle> = Vec::new();
        let mut push_out = |edge: Edge| {
            self.unregister_remote_forward_in_for_out_edge(vertex_id, &edge);
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

        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))?;
        for handle in to_clear {
            self.clear_edge_sidecars(handle);
        }
        self.drain_deferred_maintenance()?;
        Ok(())
    }

    pub fn delete_edge_by_handle(&self, handle: EdgeHandle) -> Result<(), GraphStoreError> {
        self.commit_delete_edge_by_handle(handle)
    }
}
