//! GraphStore `edge_insert` implementation.

use gleaph_graph_kernel::entry::{Edge, EdgeLabelId, EdgeSlotIndex, VertexRef};
use ic_stable_lara::{VertexId, traits::CsrEdge};

use super::GraphStore;
use super::adjacency::{EdgeInsertSpec, journal_edge_insert};
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{
    build_edge_to, build_edge_to_with_payload_bytes, canonical_undirected_owner,
    edge_matches_local_neighbor, edge_storage_label, lara_label,
    validate_edge_payload_bytes_for_label,
};

impl GraphStore {
    fn edge_payload_width_u16(payload_bytes: &[u8]) -> Result<u16, GraphStoreError> {
        u16::try_from(payload_bytes.len())
            .map_err(|_| GraphStoreError::InvalidEdgePayloadWidth(payload_bytes.len()))
    }

    pub(super) fn validate_catalog_edge_label(
        label: Option<EdgeLabelId>,
    ) -> Result<(), GraphStoreError> {
        if let Some(id) = label
            && id.raw() != 0
            && !id.is_catalog_allocatable()
        {
            return Err(GraphStoreError::InvalidEdgeLabelId(id));
        }
        Ok(())
    }

    pub fn insert_directed_edge(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_directed_edge_with_payload_bytes(
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            &[],
        )
    }

    pub(crate) fn insert_directed_edge_with_payload_bytes(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        payload_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        self.ensure_vertex_id(target_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_payload_bytes_for_label(catalog_label, payload_bytes)?;

        let label = lara_label(edge_storage_label(catalog_label, false));
        let payload_width = Self::edge_payload_width_u16(payload_bytes)?;
        let forward = if payload_bytes.is_empty() {
            build_edge_to(target_vertex_id)
        } else {
            build_edge_to_with_payload_bytes(target_vertex_id, payload_bytes)
        };
        let reverse = if payload_bytes.is_empty() {
            Edge {
                target: VertexRef::local(source_vertex_id),
                edge_slot_index: EdgeSlotIndex::from_raw(0),
                label_id: 0,
                payload: gleaph_graph_kernel::entry::EdgePayload::EMPTY,
            }
        } else {
            build_edge_to_with_payload_bytes(source_vertex_id, payload_bytes)
        };
        self.with_graph_mut(|graph| {
            if payload_width != 0 {
                graph.ensure_directed_edge_payload_width(
                    source_vertex_id,
                    target_vertex_id,
                    label,
                    payload_width,
                )?;
            }
            graph.insert_directed_edge(source_vertex_id, target_vertex_id, label, forward, reverse)
        })?;
        let canonical = self
            .find_first_forward_handle_descending(source_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target_vertex_id, payload_bytes)
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        self.commit_directed_edge_insert(EdgeInsertSpec {
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            undirected: false,
            payload_bytes,
            canonical,
        })?;
        Ok(canonical)
    }

    pub fn insert_undirected_edge(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_undirected_edge_with_payload_bytes(endpoint_a, endpoint_b, catalog_label, &[])
    }

    pub(crate) fn insert_undirected_edge_with_payload_bytes(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        catalog_label: Option<EdgeLabelId>,
        payload_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(endpoint_a)?;
        self.ensure_vertex_id(endpoint_b)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_payload_bytes_for_label(catalog_label, payload_bytes)?;

        let label = lara_label(edge_storage_label(catalog_label, true));
        let payload_width = Self::edge_payload_width_u16(payload_bytes)?;
        let edge_ab = build_edge_to_with_payload_bytes(endpoint_b, payload_bytes);
        let edge_ba = build_edge_to_with_payload_bytes(endpoint_a, payload_bytes);
        self.with_graph_mut(|graph| {
            if payload_width != 0 {
                graph.ensure_undirected_edge_payload_width(
                    endpoint_a,
                    endpoint_b,
                    label,
                    payload_width,
                )?;
            }
            graph.insert_undirected_deferred(endpoint_a, endpoint_b, label, edge_ab, edge_ba)
        })?;
        let owner_vertex_id = canonical_undirected_owner(endpoint_a, endpoint_b);
        let target = if owner_vertex_id == endpoint_a {
            endpoint_b
        } else {
            endpoint_a
        };
        let canonical = self
            .find_first_forward_handle_descending(owner_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target, payload_bytes)
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        let alias_vertex_id = if owner_vertex_id == endpoint_a {
            endpoint_b
        } else {
            endpoint_a
        };
        let alias = self
            .find_first_forward_handle_descending(alias_vertex_id, label, |edge| {
                if payload_bytes.is_empty() {
                    edge.neighbor_vid() == owner_vertex_id
                } else {
                    edge_matches_local_neighbor(edge, owner_vertex_id, payload_bytes)
                }
            })?
            .map(|alias_handle| EdgeInsertSpec {
                source_vertex_id: alias_vertex_id,
                target_vertex_id: owner_vertex_id,
                catalog_label,
                undirected: true,
                payload_bytes,
                canonical: alias_handle,
            });
        self.commit_undirected_edge_insert(
            EdgeInsertSpec {
                source_vertex_id: owner_vertex_id,
                target_vertex_id: target,
                catalog_label,
                undirected: true,
                payload_bytes,
                canonical,
            },
            alias,
        )?;
        Ok(canonical)
    }

    pub(crate) fn insert_directed_edge_with_payload_bytes_journal(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        payload_bytes: &[u8],
        canonical: EdgeHandle,
    ) -> Result<(), GraphStoreError> {
        journal_edge_insert(
            self,
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            false,
            payload_bytes,
            canonical,
        )
    }
}
