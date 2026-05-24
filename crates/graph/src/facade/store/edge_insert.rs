//! GraphStore `edge_insert` implementation.

use gleaph_graph_kernel::entry::{Edge, EdgeLabelId, EdgeSlotIndex, VertexRef};
use ic_stable_lara::{VertexId, traits::CsrEdge};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use super::helpers::{
    build_edge_to, build_edge_to_with_value_bytes, canonical_undirected_owner,
    edge_matches_local_neighbor, edge_storage_label, lara_label,
    validate_edge_value_bytes_for_label,
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
        validate_edge_value_bytes_for_label(self, catalog_label, &[])?;

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
        journal_edge_insert(
            self,
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            false,
            &[],
            canonical,
        )?;
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
        validate_edge_value_bytes_for_label(self, catalog_label, value_bytes)?;

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
        journal_edge_insert(
            self,
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            false,
            value_bytes,
            canonical,
        )?;
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
        validate_edge_value_bytes_for_label(self, catalog_label, &[])?;

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
            journal_edge_insert(
                self,
                alias_vertex_id,
                owner_vertex_id,
                catalog_label,
                true,
                &[],
                alias,
            )?;
        }
        journal_edge_insert(
            self,
            owner_vertex_id,
            target,
            catalog_label,
            true,
            &[],
            canonical,
        )?;
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
        validate_edge_value_bytes_for_label(self, catalog_label, value_bytes)?;

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
            journal_edge_insert(
                self,
                alias_vertex_id,
                owner_vertex_id,
                catalog_label,
                true,
                value_bytes,
                alias,
            )?;
        }
        journal_edge_insert(
            self,
            owner_vertex_id,
            target,
            catalog_label,
            true,
            value_bytes,
            canonical,
        )?;
        Ok(canonical)
    }

    pub(crate) fn insert_directed_edge_with_value_bytes_journal(
        &self,
        source_vertex_id: VertexId,
        target_vertex_id: VertexId,
        catalog_label: Option<EdgeLabelId>,
        value_bytes: &[u8],
        canonical: EdgeHandle,
    ) -> Result<(), GraphStoreError> {
        journal_edge_insert(
            self,
            source_vertex_id,
            target_vertex_id,
            catalog_label,
            false,
            value_bytes,
            canonical,
        )
    }
}

pub(super) fn journal_edge_insert(
    store: &GraphStore,
    source_vertex_id: VertexId,
    target_vertex_id: VertexId,
    catalog_label: Option<EdgeLabelId>,
    undirected: bool,
    value_bytes: &[u8],
    canonical: EdgeHandle,
) -> Result<(), GraphStoreError> {
    use crate::facade::migration::incremental::{
        maybe_journal_migration_op, migration_wire_handle,
    };
    use gleaph_graph_kernel::federation::MigrationJournalOp;

    if let Some(target_logical_vertex_id) = store.logical_vertex_id(target_vertex_id) {
        journal_edge_insert_to_logical(
            store,
            source_vertex_id,
            target_logical_vertex_id,
            false,
            catalog_label,
            undirected,
            value_bytes,
            canonical,
        )?;
    }

    if let Some(predecessor_logical_vertex_id) = store.logical_vertex_id(source_vertex_id) {
        if let Some(reverse) =
            store.find_reverse_alias_for_canonical(canonical, target_vertex_id, source_vertex_id)?
        {
            maybe_journal_migration_op(
                store,
                target_vertex_id,
                MigrationJournalOp::InReverseAdded {
                    source_handle: migration_wire_handle(
                        target_vertex_id,
                        reverse.label_id,
                        reverse.slot_index,
                    ),
                    predecessor_logical_vertex_id,
                    predecessor_is_remote: source_vertex_id != target_vertex_id,
                    catalog_label,
                    canonical_source_handle: migration_wire_handle(
                        source_vertex_id,
                        canonical.label_id,
                        canonical.slot_index,
                    ),
                    value_bytes: value_bytes.to_vec(),
                },
            )?;
        }
    }
    Ok(())
}

pub(super) fn journal_edge_insert_to_logical(
    store: &GraphStore,
    source_vertex_id: VertexId,
    target_logical_vertex_id: gleaph_graph_kernel::federation::LogicalVertexId,
    target_is_remote: bool,
    catalog_label: Option<EdgeLabelId>,
    undirected: bool,
    value_bytes: &[u8],
    source_handle: EdgeHandle,
) -> Result<(), GraphStoreError> {
    use crate::facade::migration::incremental::{
        maybe_journal_migration_op, migration_wire_handle,
    };
    use gleaph_graph_kernel::federation::MigrationJournalOp;

    maybe_journal_migration_op(
        store,
        source_vertex_id,
        MigrationJournalOp::OutEdgeAdded {
            catalog_label,
            undirected,
            value_bytes: value_bytes.to_vec(),
            target_logical_vertex_id,
            target_is_remote,
            source_handle: migration_wire_handle(
                source_vertex_id,
                source_handle.label_id,
                source_handle.slot_index,
            ),
        },
    )
}
