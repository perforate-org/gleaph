//! GraphStore `edge_insert` implementation.

use gleaph_graph_kernel::entry::{Edge, EdgeLabelId, EdgeSlotIndex, VertexRef};
use ic_stable_lara::{VertexId, traits::CsrEdge};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{
    build_edge_to, build_edge_to_with_value_bytes, canonical_undirected_owner,
    edge_matches_local_neighbor, edge_storage_label, lara_label, validate_edge_value_bytes,
};

impl GraphStore {
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
        self.ensure_vertex_id(source_vertex_id)?;
        self.ensure_vertex_id(target_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;

        let label = lara_label(edge_storage_label(catalog_label, false));
        let forward = build_edge_to(target_vertex_id);
        let reverse = Edge {
            target: VertexRef::local(source_vertex_id),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            value: gleaph_graph_kernel::entry::EdgeValuePayload::EMPTY,
        };
        self.with_graph_mut(|graph| {
            graph.insert_directed_edge(source_vertex_id, target_vertex_id, label, forward, reverse)
        })?;
        let canonical = self
            .find_first_forward_handle_descending(source_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target_vertex_id, &[])
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        if let Some(alias) =
            self.find_reverse_alias_for_canonical(canonical, target_vertex_id, source_vertex_id)?
        {
            self.insert_edge_alias(alias, canonical, true);
        }
        Ok(canonical)
    }

    pub(crate) fn insert_directed_edge_with_inline_value(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_value: u16,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.insert_directed_edge_with_value_bytes(
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            &inline_value.to_le_bytes(),
        )
    }

    pub(crate) fn insert_directed_edge_with_value_bytes(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        value_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        self.ensure_vertex_id(target_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_value_bytes(value_bytes)?;

        let label = lara_label(edge_storage_label(catalog_label, false));
        let forward = build_edge_to_with_value_bytes(target_vertex_id, value_bytes);
        // Reverse CSR rows only store the source id; edge values live on the forward owner.
        let reverse = Edge {
            target: VertexRef::local(source_vertex_id),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            value: gleaph_graph_kernel::entry::EdgeValuePayload::EMPTY,
        };
        self.with_graph_mut(|graph| {
            graph.insert_directed_edge(source_vertex_id, target_vertex_id, label, forward, reverse)
        })?;
        let canonical = self
            .find_first_forward_handle_descending(source_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target_vertex_id, value_bytes)
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?;
        if let Some(alias) =
            self.find_reverse_alias_for_canonical(canonical, target_vertex_id, source_vertex_id)?
        {
            self.insert_edge_alias(alias, canonical, true);
        }
        Ok(canonical)
    }

    pub fn insert_undirected_edge(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        catalog_label: Option<EdgeLabelId>,
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(endpoint_a)?;
        self.ensure_vertex_id(endpoint_b)?;
        Self::validate_catalog_edge_label(catalog_label)?;

        let label = lara_label(edge_storage_label(catalog_label, true));
        let edge_ab = build_edge_to(endpoint_b);
        let edge_ba = build_edge_to(endpoint_a);
        self.with_graph_mut(|graph| {
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
                edge_matches_local_neighbor(edge, target, &[])
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
        if let Some(alias) =
            self.find_first_forward_handle_descending(alias_vertex_id, label, |edge| {
                edge.neighbor_vid() == owner_vertex_id
            })?
        {
            self.insert_edge_alias(alias, canonical, false);
        }
        Ok(canonical)
    }

    pub(crate) fn insert_undirected_edge_with_value_bytes(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        catalog_label: Option<EdgeLabelId>,
        value_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(endpoint_a)?;
        self.ensure_vertex_id(endpoint_b)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_value_bytes(value_bytes)?;

        let label = lara_label(edge_storage_label(catalog_label, true));
        let edge_ab = build_edge_to_with_value_bytes(endpoint_b, value_bytes);
        let edge_ba = build_edge_to_with_value_bytes(endpoint_a, value_bytes);
        self.with_graph_mut(|graph| {
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
                edge_matches_local_neighbor(edge, target, value_bytes)
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
        if let Some(alias) =
            self.find_first_forward_handle_descending(alias_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, owner_vertex_id, value_bytes)
            })?
        {
            self.insert_edge_alias(alias, canonical, false);
        }
        Ok(canonical)
    }
}
