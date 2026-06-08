//! GraphStore `delete` implementation.

use gleaph_graph_kernel::entry::{Edge, EdgeTarget, TaggedEdgeLabelId};
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
        let canonical = self.canonical_edge_handle_for_sidecar(handle);
        self.ensure_vertex_id(canonical.owner_vertex_id)
            .map_err(GraphStoreError::from)?;
        let is_undirected = TaggedEdgeLabelId::from_raw(canonical.label_id.raw()).is_undirected();
        let alias = self.alias_for_canonical_edge(canonical);
        self.clear_edge_sidecars(handle);
        self.unregister_remote_forward_in_for_handle(canonical);
        let edge = self.with_graph_mut(|graph| {
            graph.remove_forward_edge_at_slot(
                canonical.owner_vertex_id,
                canonical.label_id,
                canonical.slot_index,
            )
        })?;
        let edge = edge.ok_or(GraphStoreError::EdgeNotFound {
            owner_vertex_id: canonical.owner_vertex_id,
            label_id: canonical.label_id,
            slot_index: canonical.slot_index,
        })?;
        let Some(EdgeTarget::Local(neighbor)) = edge.edge_target() else {
            self.drain_deferred_maintenance()?;
            return Ok(());
        };
        if is_undirected {
            if let Some((alias_vertex_id, alias_slot_index, _)) = alias {
                self.with_graph_mut(|graph| {
                    graph.remove_forward_edge_at_slot(
                        alias_vertex_id,
                        canonical.label_id,
                        alias_slot_index,
                    )
                })?;
            } else {
                self.with_graph_mut(|graph| {
                    graph.remove_directed_deferred(
                        neighbor,
                        canonical.owner_vertex_id,
                        edge.with_neighbor_vid(canonical.owner_vertex_id),
                    )
                })?;
            }
        } else if let Some((alias_vertex_id, alias_slot_index, reverse_in)) = alias {
            debug_assert!(
                reverse_in,
                "directed aliases should point at reverse-IN rows"
            );
            self.with_graph_mut(|graph| {
                graph.remove_reverse_edge_at_slot(
                    alias_vertex_id,
                    canonical.label_id,
                    alias_slot_index,
                )
            })?;
        } else {
            self.remove_reverse_edge_for_canonical_directed(
                neighbor,
                canonical.owner_vertex_id,
                canonical.label_id,
                canonical.slot_index,
            )?;
        }
        self.drain_deferred_maintenance()?;
        Ok(())
    }
}
