//! GraphStore logical-edge public API (delegates to remote-ref domain commits).

use gleaph_graph_kernel::entry::EdgeLabelId;
use gleaph_graph_kernel::federation::LogicalVertexId;
use ic_stable_lara::VertexId;

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

impl GraphStore {
    pub fn insert_directed_edge_to_logical(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.commit_insert_edge_to_logical(
            source_vertex_id,
            target_logical_vertex_id,
            catalog_label,
            false,
            &[],
        )
    }

    pub(crate) fn insert_directed_edge_to_logical_with_payload_bytes(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        catalog_label: Option<EdgeLabelId>,
        payload_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.commit_insert_edge_to_logical(
            source_vertex_id,
            target_logical_vertex_id,
            catalog_label,
            false,
            payload_bytes,
        )
    }

    pub(crate) fn insert_undirected_edge_to_logical_with_payload_bytes(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        catalog_label: Option<EdgeLabelId>,
        payload_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.commit_insert_edge_to_logical(
            source_vertex_id,
            target_logical_vertex_id,
            catalog_label,
            true,
            payload_bytes,
        )
    }
}
