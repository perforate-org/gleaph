//! GraphStore `edge_logical` implementation.

use super::super::stable::REMOTE_FORWARD_IN;
use gleaph_graph_kernel::entry::{Edge, EdgeLabelId, EdgeTarget, RemoteRefId};
use gleaph_graph_kernel::federation::LogicalVertexId;
use ic_stable_lara::VertexId;

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{
    build_edge_to_remote_with_payload_bytes, edge_matches_remote_target, edge_storage_label,
    lara_label, validate_edge_payload_bytes_for_label,
};

impl GraphStore {
    pub fn insert_directed_edge_to_logical(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_edge_to_logical_with_payload_bytes(
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
        self.insert_edge_to_logical_with_payload_bytes(
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
        self.insert_edge_to_logical_with_payload_bytes(
            source_vertex_id,
            target_logical_vertex_id,
            catalog_label,
            true,
            payload_bytes,
        )
    }

    fn insert_edge_to_logical_with_payload_bytes(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        catalog_label: Option<EdgeLabelId>,
        undirected: bool,
        payload_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_payload_bytes_for_label(self, catalog_label, payload_bytes)?;

        let remote_ref = self.ensure_remote_ref(target_logical_vertex_id);
        let label = lara_label(edge_storage_label(catalog_label, undirected));
        let payload_width = u16::try_from(payload_bytes.len())
            .map_err(|_| GraphStoreError::InvalidEdgePayloadWidth(payload_bytes.len()))?;
        let forward = build_edge_to_remote_with_payload_bytes(remote_ref, payload_bytes);
        self.with_graph_mut(|graph| {
            if payload_width != 0 {
                graph.ensure_forward_edge_payload_width(source_vertex_id, label, payload_width)?;
            }
            graph.insert_forward_out_edge(source_vertex_id, label, forward)
        })?;
        let handle = self
            .find_first_forward_handle_descending(source_vertex_id, label, |edge| {
                edge_matches_remote_target(edge, remote_ref, payload_bytes)
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        self.register_remote_forward_in(handle, remote_ref);
        self.commit_logical_edge_insert(
            source_vertex_id,
            target_logical_vertex_id,
            true,
            catalog_label,
            undirected,
            payload_bytes,
            handle,
        )?;
        Ok(handle)
    }

    pub(crate) fn register_remote_forward_in(&self, handle: EdgeHandle, remote_ref: RemoteRefId) {
        REMOTE_FORWARD_IN.with_borrow_mut(|index| {
            index.insert(
                remote_ref,
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
    }

    pub(crate) fn unregister_remote_forward_in_for_out_edge(
        &self,
        source_vertex_id: VertexId,
        edge: &Edge,
    ) {
        let Some(EdgeTarget::Remote(remote_ref)) = edge.edge_target() else {
            return;
        };
        REMOTE_FORWARD_IN.with_borrow_mut(|index| {
            index.remove(
                remote_ref,
                source_vertex_id,
                edge.label_id,
                edge.edge_slot_index.raw(),
            );
        });
    }

    pub(super) fn unregister_remote_forward_in_for_handle(&self, handle: EdgeHandle) {
        let label = handle.label_id;
        let mut edges = self
            .directed_out_edges(handle.owner_vertex_id)
            .unwrap_or_default();
        edges.extend(
            self.undirected_edges(handle.owner_vertex_id)
                .unwrap_or_default(),
        );
        for edge in edges {
            if edge.label_id != label.raw() || edge.edge_slot_index.raw() != handle.slot_index {
                continue;
            }
            self.unregister_remote_forward_in_for_out_edge(handle.owner_vertex_id, &edge);
            return;
        }
    }
}
