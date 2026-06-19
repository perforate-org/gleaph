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

    pub fn delete_edge_by_handle(&self, handle: EdgeHandle) -> Result<(), GraphStoreError> {
        self.commit_delete_edge_by_handle(handle)
    }
}
