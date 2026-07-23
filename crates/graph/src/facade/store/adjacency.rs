//! Adjacency storage domain: canonical edge writes plus derived alias, journal, and maintenance.

use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeTarget, TaggedEdgeLabelId};
use ic_stable_lara::{VertexId, labeled::LabeledOrientation, traits::CsrEdge};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

/// Local edge insert journal payload after LARA reports the canonical handle.
pub(super) struct EdgeInsertSpec<'a> {
    pub source_vertex_id: VertexId,
    pub target_vertex_id: VertexId,
    pub catalog_label: Option<EdgeLabelId>,
    pub undirected: bool,
    pub inline_value_bytes: &'a [u8],
    pub canonical: EdgeHandle,
}

impl GraphStore {
    /// Directed edge: optional reverse alias, journal, deferred maintenance.
    pub(super) fn commit_directed_edge_insert(
        &self,
        spec: EdgeInsertSpec<'_>,
        exact_alias: Option<EdgeHandle>,
    ) -> Result<(), GraphStoreError> {
        let alias = if let Some(alias) = exact_alias {
            Some(alias)
        } else {
            self.find_reverse_alias_for_canonical(
                spec.canonical,
                spec.target_vertex_id,
                spec.source_vertex_id,
            )?
        };
        if let Some(alias) = alias {
            self.insert_edge_alias(alias, spec.canonical, true);
        }
        self.journal_and_maintain_edge_insert(spec)
    }

    /// Undirected edge: optional alias on the non-owner endpoint, then canonical journal.
    pub(super) fn commit_undirected_edge_insert(
        &self,
        canonical: EdgeInsertSpec<'_>,
        alias: Option<EdgeInsertSpec<'_>>,
    ) -> Result<(), GraphStoreError> {
        if let Some(alias_spec) = alias {
            self.insert_edge_alias(alias_spec.canonical, canonical.canonical, false);
            journal_edge_insert(
                self,
                alias_spec.source_vertex_id,
                alias_spec.target_vertex_id,
                alias_spec.catalog_label,
                alias_spec.undirected,
                alias_spec.inline_value_bytes,
                alias_spec.canonical,
            )?;
        }
        self.journal_and_maintain_edge_insert(canonical)
    }

    /// Remove a canonical edge, its alias row if present, derived sidecars, and maintenance queue.
    pub(super) fn commit_delete_edge_by_handle(
        &self,
        handle: EdgeHandle,
    ) -> Result<(), GraphStoreError> {
        let canonical = self.canonical_edge_handle_for_sidecar(handle);
        self.ensure_vertex_id(canonical.owner_vertex_id)
            .map_err(GraphStoreError::from)?;
        let is_undirected = TaggedEdgeLabelId::from_raw(canonical.label_id.raw()).is_undirected();
        let alias = self.alias_for_canonical_edge(canonical);
        self.commit_clear_edge_sidecars(handle);
        let removal = self.with_graph_mut(|graph| {
            graph.remove_forward_edge_at_slot_with_move(
                canonical.owner_vertex_id,
                canonical.label_id,
                canonical.slot_index,
            )
        })?;
        let removal = removal.ok_or(GraphStoreError::EdgeNotFound {
            owner_vertex_id: canonical.owner_vertex_id,
            label_id: canonical.label_id,
            slot_index: canonical.slot_index,
        })?;
        Self::apply_edge_slot_moves(
            LabeledOrientation::Forward,
            canonical.owner_vertex_id,
            removal.moves,
        );
        let edge = removal.removed;
        let Some(EdgeTarget::Local(neighbor)) = edge.edge_target() else {
            self.drain_deferred_maintenance()?;
            return Ok(());
        };
        if is_undirected {
            if let Some((alias_vertex_id, alias_slot_index, _)) = alias {
                let removal = self.with_graph_mut(|graph| {
                    graph.remove_forward_edge_at_slot_with_move(
                        alias_vertex_id,
                        canonical.label_id,
                        alias_slot_index,
                    )
                })?;
                Self::apply_edge_slot_moves(
                    LabeledOrientation::Forward,
                    alias_vertex_id,
                    removal.into_iter().flat_map(|removal| removal.moves),
                );
            } else {
                let removal = self.with_graph_mut(|graph| {
                    graph.remove_forward_edge_matching_with_move(
                        neighbor,
                        canonical.label_id,
                        |candidate| {
                            candidate.neighbor_vid() == canonical.owner_vertex_id
                                && candidate.edge_inline_value_bytes()
                                    == edge.edge_inline_value_bytes()
                        },
                    )
                })?;
                Self::apply_edge_slot_moves(
                    LabeledOrientation::Forward,
                    neighbor,
                    removal.into_iter().flat_map(|removal| removal.moves),
                );
            }
        } else if let Some((alias_vertex_id, alias_slot_index, reverse_in)) = alias {
            debug_assert!(
                reverse_in,
                "directed aliases should point at reverse-IN rows"
            );
            let removal = self.with_graph_mut(|graph| {
                graph.remove_reverse_edge_at_slot_with_move(
                    alias_vertex_id,
                    canonical.label_id,
                    alias_slot_index,
                )
            })?;
            Self::apply_edge_slot_moves(
                LabeledOrientation::Reverse,
                alias_vertex_id,
                removal.into_iter().flat_map(|removal| removal.moves),
            );
        } else {
            self.remove_reverse_edge_for_canonical_directed(
                neighbor,
                canonical.owner_vertex_id,
                canonical.label_id,
                canonical.slot_index,
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
            spec.inline_value_bytes,
            spec.canonical,
        )?;
        self.run_post_edge_insert_maintenance()
    }
}

pub(super) fn journal_edge_insert(
    _store: &GraphStore,
    _source_vertex_id: VertexId,
    _target_vertex_id: VertexId,
    _catalog_label: Option<EdgeLabelId>,
    _undirected: bool,
    _inline_value_bytes: &[u8],
    _canonical: EdgeHandle,
) -> Result<(), GraphStoreError> {
    Ok(())
}
