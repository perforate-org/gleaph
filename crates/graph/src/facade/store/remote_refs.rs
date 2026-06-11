//! Remote reference domain: logical vertex handles and forward-in edge index.

use super::super::stable::remote_forward_in::RemoteForwardInKey;
use super::super::stable::{REMOTE_FORWARD_IN, REMOTE_VERTEX_REFS};
use gleaph_graph_kernel::entry::{Edge, EdgeLabelId, EdgeTarget, RemoteRefId};
use gleaph_graph_kernel::federation::GlobalVertexId;
use ic_stable_lara::{VertexId, labeled::EdgeSlotMove};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{
    build_edge_to_remote_with_payload_bytes, edge_matches_remote_target, edge_storage_label,
    lara_label, validate_edge_payload_bytes_for_label,
};

impl GraphStore {
    pub fn ensure_remote_ref(&self, vertex_id: GlobalVertexId) -> RemoteRefId {
        REMOTE_VERTEX_REFS.with_borrow_mut(|table| table.ensure_remote_ref(vertex_id))
    }

    pub fn global_vertex_for_remote_ref(&self, remote_ref: RemoteRefId) -> Option<GlobalVertexId> {
        REMOTE_VERTEX_REFS.with_borrow(|table| table.global_vertex_id(remote_ref))
    }

    pub fn remote_ref_for_vertex(&self, vertex_id: GlobalVertexId) -> Option<RemoteRefId> {
        REMOTE_VERTEX_REFS.with_borrow(|table| table.remote_ref_for_vertex(vertex_id))
    }

    /// Deprecated alias for migration in call sites.
    #[inline]
    pub fn remote_ref_for_logical(&self, vertex_id: GlobalVertexId) -> Option<RemoteRefId> {
        self.remote_ref_for_vertex(vertex_id)
    }

    pub(crate) fn remote_forward_in_index_populated(&self) -> bool {
        REMOTE_FORWARD_IN.with_borrow(|index| !index.is_empty())
    }

    pub(crate) fn remote_forward_in_keys_for_ref(
        &self,
        remote_ref: RemoteRefId,
    ) -> Vec<RemoteForwardInKey> {
        REMOTE_FORWARD_IN.with_borrow(|index| {
            let mut keys = Vec::new();
            index.for_each_for_remote_ref(remote_ref, |key| keys.push(key));
            keys
        })
    }

    pub(crate) fn has_remote_forward_in_postings(&self, remote_ref: RemoteRefId) -> bool {
        REMOTE_FORWARD_IN.with_borrow(|index| index.has_postings_for(remote_ref))
    }

    /// Insert a forward edge to a remote logical vertex and register derived forward-in state.
    pub(super) fn commit_insert_edge_to_logical(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: GlobalVertexId,
        catalog_label: Option<EdgeLabelId>,
        undirected: bool,
        payload_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_payload_bytes_for_label(self, catalog_label, payload_bytes)?;

        let remote_ref = self.ensure_remote_ref(target_vertex_id);
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
        self.commit_register_remote_forward_in(handle, remote_ref);
        self.commit_logical_edge_insert(
            source_vertex_id,
            target_vertex_id,
            true,
            catalog_label,
            undirected,
            payload_bytes,
            handle,
        )?;
        Ok(handle)
    }

    pub(super) fn commit_register_remote_forward_in(
        &self,
        handle: EdgeHandle,
        remote_ref: RemoteRefId,
    ) {
        REMOTE_FORWARD_IN.with_borrow_mut(|index| {
            index.insert(
                remote_ref,
                handle.owner_vertex_id,
                handle.label_id.raw(),
                handle.slot_index,
            );
        });
    }

    pub(crate) fn register_remote_forward_in(&self, handle: EdgeHandle, remote_ref: RemoteRefId) {
        self.commit_register_remote_forward_in(handle, remote_ref);
    }

    pub(super) fn commit_unregister_remote_forward_in_for_out_edge(
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

    pub(super) fn commit_move_remote_forward_in_for_compaction(
        &self,
        owner_vertex_id: VertexId,
        label_id: u16,
        moved: EdgeSlotMove,
    ) {
        use super::super::stable::GRAPH;
        use ic_stable_lara::{BucketLabelKey as LaraLabelId, traits::CsrEdge};

        let label = LaraLabelId::from_raw(label_id);
        let _ = GRAPH.with_borrow(|graph| {
            graph.for_each_out_edges_for_label_unchecked(owner_vertex_id, label, |edge| {
                if edge.edge_slot_index.raw() != moved.new_slot_index {
                    return;
                }
                let Some(EdgeTarget::Remote(remote_ref)) = edge.edge_target() else {
                    return;
                };
                REMOTE_FORWARD_IN.with_borrow_mut(|index| {
                    index.move_slot(
                        remote_ref,
                        owner_vertex_id,
                        label_id,
                        moved.old_slot_index,
                        moved.new_slot_index,
                    );
                });
            })
        });
    }

    pub(crate) fn unregister_remote_forward_in_for_out_edge(
        &self,
        source_vertex_id: VertexId,
        edge: &Edge,
    ) {
        self.commit_unregister_remote_forward_in_for_out_edge(source_vertex_id, edge);
    }

    pub(super) fn commit_unregister_remote_forward_in_for_handle(&self, handle: EdgeHandle) {
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
            self.commit_unregister_remote_forward_in_for_out_edge(handle.owner_vertex_id, &edge);
            return;
        }
    }

    pub(super) fn unregister_remote_forward_in_for_handle(&self, handle: EdgeHandle) {
        self.commit_unregister_remote_forward_in_for_handle(handle);
    }
}
