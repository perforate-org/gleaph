//! GraphStore `edge_insert` implementation.

use gleaph_graph_kernel::entry::{
    Edge, EdgeInlinePropertyBytes, EdgeLabelId, EdgeSlotIndex, VertexRef,
};
use gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy;
use ic_stable_lara::{VertexId, labeled::EdgePlacementPolicy, traits::CsrEdge};

use super::GraphStore;
use super::adjacency::{EdgeInsertSpec, journal_edge_insert};
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{
    build_edge_to, canonical_undirected_owner, edge_matches_local_neighbor, edge_storage_label,
    lara_label, validate_edge_inline_property_bytes_for_label,
};
use crate::edge_inline_property_schema::resolved_edge_label_with;

/// Maps the resolved ordering policy to the storage-owned LARA placement enum
/// at the mutation boundary (ADR 0052 §4). An undeclared/unknown label resolves
/// to the `Unordered` default (ADR 0052 §1).
fn lara_edge_placement(ordering: Option<EdgeOrderingPolicy>) -> EdgePlacementPolicy {
    match ordering {
        None | Some(EdgeOrderingPolicy::Unordered) => EdgePlacementPolicy::Unordered,
        Some(EdgeOrderingPolicy::Insertion) => EdgePlacementPolicy::Insertion,
    }
}

impl GraphStore {
    fn edge_inline_property_width_u16(
        inline_property_bytes: &[u8],
    ) -> Result<u16, GraphStoreError> {
        u16::try_from(inline_property_bytes.len()).map_err(|_| {
            GraphStoreError::InvalidEdgeInlinePropertyBytesWidth(inline_property_bytes.len())
        })
    }

    pub(crate) fn validate_catalog_edge_label(
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
        self.insert_directed_edge_with_inline_property_bytes(
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            &[],
        )
    }

    pub(crate) fn insert_directed_edge_with_inline_property_bytes(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_property_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(source_vertex_id)?;
        self.ensure_vertex_id(target_vertex_id)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_inline_property_bytes_for_label(catalog_label, inline_property_bytes)?;

        let label = lara_label(edge_storage_label(catalog_label, false));
        let inline_property_width = Self::edge_inline_property_width_u16(inline_property_bytes)?;
        let placement = lara_edge_placement(
            catalog_label
                .and_then(|label| resolved_edge_label_with(None, label).map(|l| l.ordering)),
        );
        let forward = build_edge_to(target_vertex_id)
            .with_stored_inline_property_bytes(inline_property_width, inline_property_bytes);
        let reverse = Edge {
            target: VertexRef::local(source_vertex_id),
            edge_slot_index: EdgeSlotIndex::from_raw(0),
            label_id: 0,
            inline_property: EdgeInlinePropertyBytes::EMPTY,
        }
        .with_stored_inline_property_bytes(inline_property_width, inline_property_bytes);
        let locations = self.with_graph_mut(|graph| {
            if inline_property_width != 0 {
                graph.ensure_directed_edge_inline_property_width(
                    source_vertex_id,
                    target_vertex_id,
                    label,
                    inline_property_width,
                )?;
            }
            graph.insert_directed_edge_with_locations(
                source_vertex_id,
                target_vertex_id,
                label,
                forward,
                reverse,
                placement,
            )
        })?;
        let canonical = if let Some(location) = locations.forward {
            EdgeHandle::at_slot(source_vertex_id, label, location.logical_slot)
        } else {
            self.find_first_forward_handle(source_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target_vertex_id, inline_property_bytes)
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id: source_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?
        };
        self.commit_directed_edge_insert(EdgeInsertSpec {
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            undirected: false,
            inline_property_bytes,
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
        self.insert_undirected_edge_with_inline_property_bytes(
            endpoint_a,
            endpoint_b,
            catalog_label,
            &[],
        )
    }

    pub(crate) fn insert_undirected_edge_with_inline_property_bytes(
        &self,
        endpoint_a: VertexId,
        endpoint_b: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_property_bytes: &[u8],
    ) -> Result<EdgeHandle, GraphStoreError> {
        self.ensure_vertex_id(endpoint_a)?;
        self.ensure_vertex_id(endpoint_b)?;
        Self::validate_catalog_edge_label(catalog_label)?;
        validate_edge_inline_property_bytes_for_label(catalog_label, inline_property_bytes)?;

        let label = lara_label(edge_storage_label(catalog_label, true));
        let inline_property_width = Self::edge_inline_property_width_u16(inline_property_bytes)?;
        let placement = lara_edge_placement(
            catalog_label
                .and_then(|label| resolved_edge_label_with(None, label).map(|l| l.ordering)),
        );
        let edge_ab = build_edge_to(endpoint_b)
            .with_stored_inline_property_bytes(inline_property_width, inline_property_bytes);
        let edge_ba = build_edge_to(endpoint_a)
            .with_stored_inline_property_bytes(inline_property_width, inline_property_bytes);
        let locations = self.with_graph_mut(|graph| {
            if inline_property_width != 0 {
                graph.ensure_undirected_edge_inline_property_width(
                    endpoint_a,
                    endpoint_b,
                    label,
                    inline_property_width,
                )?;
            }
            graph.insert_undirected_deferred_with_locations(
                endpoint_a, endpoint_b, label, edge_ab, edge_ba, placement,
            )
        })?;
        let owner_vertex_id = canonical_undirected_owner(endpoint_a, endpoint_b);
        let target = if owner_vertex_id == endpoint_a {
            endpoint_b
        } else {
            endpoint_a
        };
        let canonical_location = if owner_vertex_id == endpoint_a {
            locations.forward
        } else {
            locations.reverse
        };
        let canonical = if let Some(location) = canonical_location {
            EdgeHandle::at_slot(owner_vertex_id, label, location.logical_slot)
        } else {
            self.find_first_forward_handle(owner_vertex_id, label, |edge| {
                edge_matches_local_neighbor(edge, target, inline_property_bytes)
            })?
            .ok_or(GraphStoreError::EdgeNotFound {
                owner_vertex_id,
                label_id: label,
                slot_index: u32::MAX,
            })?
        };
        self.commit_undirected_edge_insert(EdgeInsertSpec {
            source_vertex_id: owner_vertex_id,
            target_vertex_id: target,
            catalog_label,
            undirected: true,
            inline_property_bytes,
            canonical,
        })?;
        Ok(canonical)
    }

    pub(crate) fn insert_directed_edge_with_inline_property_bytes_journal(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        inline_property_bytes: &[u8],
        canonical: EdgeHandle,
    ) -> Result<(), GraphStoreError> {
        journal_edge_insert(
            self,
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            false,
            inline_property_bytes,
            canonical,
        )
    }
}
