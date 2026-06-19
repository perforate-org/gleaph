//! GraphStore `delete` implementation.

use ic_stable_lara::VertexId;

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
    pub fn delete_vertex(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.commit_delete_detached_vertex(vertex_id)
    }

    pub fn detach_delete_vertex(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.commit_detach_delete_vertex(vertex_id)
    }

    /// `detach_delete_vertex` with an explicit synchronous incident-degree ceiling
    /// (ADR 0021 Stage 0). The public path uses the production limit.
    pub(crate) fn detach_delete_vertex_bounded(
        &self,
        vertex_id: VertexId,
        max_incident_degree: u64,
    ) -> Result<(), GraphStoreError> {
        self.commit_detach_delete_vertex_bounded(vertex_id, max_incident_degree)
    }

    pub fn delete_edge_by_handle(&self, handle: EdgeHandle) -> Result<(), GraphStoreError> {
        self.commit_delete_edge_by_handle(handle)
    }
}
