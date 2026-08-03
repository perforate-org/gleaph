//! GraphStore `delete` implementation.

use ic_stable_lara::VertexId;

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
    pub fn delete_vertex(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.commit_delete_detached_vertex(vertex_id, 0)
    }

    pub fn detach_delete_vertex(&self, vertex_id: VertexId) -> Result<(), GraphStoreError> {
        self.commit_detach_delete_vertex(vertex_id, 0)
    }

    pub fn delete_edge_by_handle(&self, handle: EdgeHandle) -> Result<(), GraphStoreError> {
        self.commit_delete_edge_by_handle(handle, 0)
    }

    /// Delete a vertex under an explicit canonical mutation identity for the fence.
    pub(crate) fn delete_vertex_with_mutation_id(
        &self,
        vertex_id: VertexId,
        mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    ) -> Result<(), GraphStoreError> {
        self.commit_delete_detached_vertex(vertex_id, mutation_id)
    }

    /// Detach-delete a vertex under an explicit canonical mutation identity for the fence.
    pub(crate) fn detach_delete_vertex_with_mutation_id(
        &self,
        vertex_id: VertexId,
        mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    ) -> Result<(), GraphStoreError> {
        self.commit_detach_delete_vertex(vertex_id, mutation_id)
    }

    /// Delete an edge under an explicit canonical mutation identity for the fence.
    pub(crate) fn delete_edge_by_handle_with_mutation_id(
        &self,
        handle: EdgeHandle,
        mutation_id: gleaph_graph_kernel::plan_exec::MutationId,
    ) -> Result<(), GraphStoreError> {
        self.commit_delete_edge_by_handle(handle, mutation_id)
    }
}
