//! Adjacency storage domain: canonical edge writes, journal, and maintenance.

use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeTarget, TaggedEdgeLabelId};
use ic_stable_lara::{VertexId, labeled::LabeledOrientation, traits::CsrEdge};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use crate::property::{
    PropertyValueChange, dispatch_inline_scalar_index_removal, dispatch_property_index_ops,
    inline_scalar_index_value,
};

/// Local edge insert journal payload after LARA reports the canonical handle.
pub(super) struct EdgeInsertSpec<'a> {
    pub source_vertex_id: VertexId,
    pub target_vertex_id: VertexId,
    pub catalog_label: Option<EdgeLabelId>,
    pub undirected: bool,
    pub inline_property_bytes: &'a [u8],
    pub canonical: EdgeHandle,
}

impl GraphStore {
    /// Directed edge: journal and deferred maintenance.
    pub(super) fn commit_directed_edge_insert(
        &self,
        spec: EdgeInsertSpec<'_>,
    ) -> Result<(), GraphStoreError> {
        self.journal_and_maintain_edge_insert(spec)
    }

    /// Undirected edge: canonical journal and deferred maintenance.
    pub(super) fn commit_undirected_edge_insert(
        &self,
        canonical: EdgeInsertSpec<'_>,
    ) -> Result<(), GraphStoreError> {
        self.journal_and_maintain_edge_insert(canonical)
    }

    /// Remove a canonical edge, its counterpart row, derived sidecars, and maintenance queue.
    pub(super) fn commit_delete_edge_by_handle(
        &self,
        handle: EdgeHandle,
    ) -> Result<(), GraphStoreError> {
        let canonical = self
            .scan_only_canonical_edge_handle(handle, LabeledOrientation::Forward)
            .map_err(GraphStoreError::from)?;
        self.ensure_vertex_id(canonical.owner_vertex_id)
            .map_err(GraphStoreError::from)?;
        let is_undirected = TaggedEdgeLabelId::from_raw(canonical.label_id.raw()).is_undirected();
        let edge =
            self.find_outgoing_edge_record(canonical)?
                .ok_or(GraphStoreError::EdgeNotFound {
                    owner_vertex_id: canonical.owner_vertex_id,
                    label_id: canonical.label_id,
                    slot_index: canonical.slot_index.raw(),
                })?;
        let counterpart =
            if matches!(edge.edge_target(), Some(EdgeTarget::Local(_))) {
                Some(self.counterpart_edge_occurrence(
                    canonical.occurrence(LabeledOrientation::Forward),
                )?)
            } else {
                None
            };
        dispatch_inline_scalar_index_removal(
            canonical.owner_vertex_id,
            canonical.label_id.raw(),
            canonical.slot_index.raw(),
            edge.edge_inline_property_bytes(),
        )
        .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })?;
        self.commit_clear_edge_sidecars_at_canonical(canonical);
        let removal = self.with_graph_mut(|graph| {
            graph.remove_forward_edge_at_slot_with_move(
                canonical.owner_vertex_id,
                canonical.label_id,
                canonical.slot_index.raw(),
            )
        })?;
        let removal = removal.ok_or(GraphStoreError::EdgeNotFound {
            owner_vertex_id: canonical.owner_vertex_id,
            label_id: canonical.label_id,
            slot_index: canonical.slot_index.raw(),
        })?;
        Self::apply_edge_slot_moves(
            LabeledOrientation::Forward,
            canonical.owner_vertex_id,
            removal.moves,
        )?;
        let Some(counterpart) = counterpart else {
            self.drain_deferred_maintenance()?;
            return Ok(());
        };
        if is_undirected {
            if counterpart.owner_vertex_id != canonical.owner_vertex_id {
                let removal = self.with_graph_mut(|graph| {
                    graph.remove_forward_edge_at_slot_with_move(
                        counterpart.owner_vertex_id,
                        counterpart.label_id,
                        counterpart.slot_index.raw(),
                    )
                })?;
                Self::apply_edge_slot_moves(
                    LabeledOrientation::Forward,
                    counterpart.owner_vertex_id,
                    removal.into_iter().flat_map(|removal| removal.moves),
                )?;
            }
        } else {
            debug_assert_eq!(counterpart.orientation, LabeledOrientation::Reverse);
            let removal = self.with_graph_mut(|graph| {
                graph.remove_reverse_edge_at_slot_with_move(
                    counterpart.owner_vertex_id,
                    counterpart.label_id,
                    counterpart.slot_index.raw(),
                )
            })?;
            Self::apply_edge_slot_moves(
                LabeledOrientation::Reverse,
                counterpart.owner_vertex_id,
                removal.into_iter().flat_map(|removal| removal.moves),
            )?;
        }
        self.drain_deferred_maintenance()
    }

    fn journal_and_maintain_edge_insert(
        &self,
        spec: EdgeInsertSpec<'_>,
    ) -> Result<(), GraphStoreError> {
        journal_edge_insert(
            self,
            spec.source_vertex_id,
            spec.target_vertex_id,
            spec.catalog_label,
            spec.undirected,
            spec.inline_property_bytes,
            spec.canonical,
        )?;
        if let Some((property_id, value)) =
            inline_scalar_index_value(spec.canonical.label_id.raw(), spec.inline_property_bytes)
                .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })?
        {
            dispatch_property_index_ops(PropertyValueChange::edge(
                spec.canonical.owner_vertex_id,
                spec.canonical.label_id.raw(),
                spec.canonical.slot_index.raw(),
                property_id,
                None,
                Some(&value),
            ));
        }
        self.run_post_edge_insert_maintenance()
    }
}

pub(super) fn journal_edge_insert(
    _store: &GraphStore,
    _source_vertex_id: VertexId,
    _target_vertex_id: VertexId,
    _catalog_label: Option<EdgeLabelId>,
    _undirected: bool,
    _inline_property_bytes: &[u8],
    _canonical: EdgeHandle,
) -> Result<(), GraphStoreError> {
    Ok(())
}
