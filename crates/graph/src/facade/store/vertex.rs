//! GraphStore `vertex` implementation.

use super::super::stable::GRAPH;
use crate::index::placement;
use gleaph_graph_kernel::entry::{Edge, EdgeTarget, Vertex};
use gleaph_graph_kernel::federation::{
    CommitVertexPlacementArgs, ElementIdEncodingKey, GlobalVertexId, ShardId,
};
use gleaph_graph_kernel::path::GraphPathVertexId;
use ic_stable_lara::{DeferredBidirectionalLabeledError, VertexCount, VertexId, traits::CsrEdge};

use super::GraphStore;
use super::error::GraphStoreError;
use super::helpers::canonical_undirected_owner;

impl GraphStore {
    pub fn vertex_count(&self) -> VertexCount {
        GRAPH.with_borrow(|graph| graph.vertex_count())
    }

    pub fn insert_vertex(&self) -> Result<VertexId, GraphStoreError> {
        #[cfg(any(not(target_family = "wasm"), feature = "canbench"))]
        {
            pollster::block_on(self.insert_vertex_row(Vertex::default()))
        }
        #[cfg(all(target_family = "wasm", not(feature = "canbench")))]
        {
            ic_cdk::trap("insert_vertex: use insert_vertex_row().await on wasm");
        }
    }

    pub async fn insert_vertex_row(&self, vertex: Vertex) -> Result<VertexId, GraphStoreError> {
        let vertex_id = self
            .with_graph_mut(|graph| graph.push_vertex_row(vertex.into()))
            .map_err(GraphStoreError::from)?;

        if let Some(routing) = self.federation_routing() {
            placement::commit_vertex_placement(
                routing.router_canister,
                CommitVertexPlacementArgs {
                    local_vertex_id: placement::local_vertex_id_raw(vertex_id),
                },
            )
            .await?;
        }

        Ok(vertex_id)
    }

    pub fn global_vertex_id(&self, vertex_id: VertexId) -> Option<GlobalVertexId> {
        let local = placement::local_vertex_id_raw(vertex_id);
        let shard_id = self
            .federation_routing()
            .map(|r| r.shard_id)
            .unwrap_or(ShardId::new(0));
        Some(GlobalVertexId::new(shard_id, local))
    }

    /// Deprecated alias for migration in call sites.
    #[inline]
    pub fn logical_vertex_id(&self, vertex_id: VertexId) -> Option<GlobalVertexId> {
        self.global_vertex_id(vertex_id)
    }

    pub(crate) fn assert_local_vertex_writable(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        if self.vertex(vertex_id).is_some_and(|v| v.is_tombstone()) {
            return Err(GraphStoreError::VertexTombstoned);
        }
        let Some(routing) = self.federation_routing() else {
            return Ok(());
        };
        let Some(global_vertex_id) = self.global_vertex_id(vertex_id) else {
            return Ok(());
        };
        #[cfg(target_family = "wasm")]
        let placement = {
            let _ = global_vertex_id;
            let local = placement::local_vertex_id_raw(vertex_id);
            gleaph_graph_kernel::federation::VertexPlacement::Active(
                gleaph_graph_kernel::federation::PhysicalVertexLocation::new(
                    routing.shard_id,
                    local,
                ),
            )
        };
        #[cfg(not(target_family = "wasm"))]
        let placement = pollster::block_on(placement::resolve_placement(
            routing.router_canister,
            global_vertex_id,
        ))?;
        let _ = placement;
        Ok(())
    }

    pub(crate) fn path_vertex_element_id(&self, vertex_id: VertexId) -> Option<GraphPathVertexId> {
        self.global_vertex_id(vertex_id)
            .map(|id| GraphPathVertexId::from_global(&ElementIdEncodingKey::standalone(), id))
    }

    pub(crate) fn edge_sidecar_owner_from_in_row(&self, dst: VertexId, edge: &Edge) -> VertexId {
        if self.edge_is_undirected(dst, edge).unwrap_or(false) {
            canonical_undirected_owner(dst, edge.neighbor_vid())
        } else {
            edge.neighbor_vid()
        }
    }

    pub fn edge_target(&self, edge: &Edge) -> Option<EdgeTarget> {
        edge.edge_target()
    }

    pub(crate) fn push_unplaced_vertex_row(
        &self,
        vertex: Vertex,
    ) -> Result<VertexId, DeferredBidirectionalLabeledError> {
        self.with_graph_mut(|graph| graph.push_vertex_row(vertex.into()))
    }

    #[cfg(test)]
    pub(crate) fn register_logical_vertex_mapping(
        &self,
        _vertex_id: VertexId,
        _global_vertex_id: GlobalVertexId,
    ) {
    }
}
