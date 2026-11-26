//! GraphStore `vertex` implementation.
use crate::facade::topology::vertex::VertexState;
use crate::index::federation_routing;
use gleaph_graph_kernel::entry::VertexId;
use gleaph_graph_kernel::entry::{Edge, EdgeTarget};
use gleaph_graph_kernel::federation::{ElementIdEncodingKey, GlobalVertexId, ShardId};
use gleaph_graph_kernel::path::GraphPathVertexId;

use super::GraphStore;
use super::error::GraphStoreError;
use super::helpers::canonical_undirected_owner;

impl GraphStore {
    /// How many vertices the shard holds, deleted ones included: ids below it exist.
    pub fn vertex_count(&self) -> u32 {
        crate::facade::topology::vertex::count()
    }

    pub fn insert_vertex(&self) -> Result<VertexId, GraphStoreError> {
        crate::facade::topology::vertex::add().map_err(GraphStoreError::from)
    }

    /// Adds `count` vertices under one GraphStore mutation boundary and returns their ids in
    /// order. Labels, properties and indexes are separate phases.
    pub(crate) fn insert_vertices(&self, count: u32) -> Result<Vec<VertexId>, GraphStoreError> {
        crate::facade::topology::vertex::add_many(count).map_err(GraphStoreError::from)
    }

    /// `vertex_id`'s state; `None` when no vertex has that id.
    pub(crate) fn vertex_state(&self, vertex_id: VertexId) -> Option<VertexState> {
        crate::facade::topology::vertex::state(vertex_id)
    }

    /// Whether a vertex has this id, deleted or not.
    pub fn vertex_exists(&self, vertex_id: VertexId) -> bool {
        u32::from(vertex_id) < self.vertex_count()
    }

    pub fn global_vertex_id(&self, vertex_id: VertexId) -> Option<GlobalVertexId> {
        let local = federation_routing::local_vertex_id_raw(vertex_id);
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

    /// Whether the vertex exists and is neither deleted nor being deleted.
    pub(crate) fn is_vertex_live(&self, vertex_id: VertexId) -> bool {
        self.vertex_state(vertex_id)
            .is_some_and(VertexState::is_live)
    }

    /// Resolve a global id to a live local handle when this shard is authoritative.
    pub(crate) fn resolve_local_vertex(&self, global: GlobalVertexId) -> Option<VertexId> {
        let home_shard = self
            .federation_routing()
            .map(|r| r.shard_id)
            .unwrap_or(ShardId::new(0));
        if global.shard_id != home_shard {
            return None;
        }
        let local = VertexId::from(global.local_vertex_id);
        self.is_vertex_live(local).then_some(local)
    }

    pub(crate) fn assert_local_vertex_writable(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        if !self.is_vertex_live(vertex_id) {
            return Err(GraphStoreError::VertexTombstoned);
        }
        Ok(())
    }

    pub(crate) fn path_vertex_element_id(
        &self,
        key: &ElementIdEncodingKey,
        vertex_id: VertexId,
    ) -> Option<GraphPathVertexId> {
        self.global_vertex_id(vertex_id)
            .map(|id| GraphPathVertexId::from_global(key, id))
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

    #[cfg(test)]
    pub(crate) fn register_logical_vertex_mapping(
        &self,
        _vertex_id: VertexId,
        _global_vertex_id: GlobalVertexId,
    ) {
    }
}
