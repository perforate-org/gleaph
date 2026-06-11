//! Adjacency storage domain: canonical edge writes plus derived alias, journal, and maintenance.

use gleaph_graph_kernel::entry::{EdgeLabelId, EdgeTarget, TaggedEdgeLabelId};
use ic_stable_lara::{VertexId, traits::CsrEdge};

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;

/// Local edge insert journal payload after LARA reports the canonical handle.
pub(super) struct EdgeInsertSpec<'a> {
    pub source_vertex_id: VertexId,
    pub target_vertex_id: VertexId,
    pub catalog_label: Option<EdgeLabelId>,
    pub undirected: bool,
    pub payload_bytes: &'a [u8],
    pub canonical: EdgeHandle,
}

impl GraphStore {
    /// Directed edge: optional reverse alias, journal, deferred maintenance.
    pub(super) fn commit_directed_edge_insert(
        &self,
        spec: EdgeInsertSpec<'_>,
    ) -> Result<(), GraphStoreError> {
        if let Some(alias) = self.find_reverse_alias_for_canonical(
            spec.canonical,
            spec.target_vertex_id,
            spec.source_vertex_id,
        )? {
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
                alias_spec.payload_bytes,
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
        let edge = self.with_graph_mut(|graph| {
            graph.remove_forward_edge_at_slot(
                canonical.owner_vertex_id,
                canonical.label_id,
                canonical.slot_index,
            )
        })?;
        let edge = edge.ok_or(GraphStoreError::EdgeNotFound {
            owner_vertex_id: canonical.owner_vertex_id,
            label_id: canonical.label_id,
            slot_index: canonical.slot_index,
        })?;
        let Some(EdgeTarget::Local(neighbor)) = edge.edge_target() else {
            self.drain_deferred_maintenance()?;
            return Ok(());
        };
        if is_undirected {
            if let Some((alias_vertex_id, alias_slot_index, _)) = alias {
                self.with_graph_mut(|graph| {
                    graph.remove_forward_edge_at_slot(
                        alias_vertex_id,
                        canonical.label_id,
                        alias_slot_index,
                    )
                })?;
            } else {
                self.with_graph_mut(|graph| {
                    graph.remove_directed_deferred(
                        neighbor,
                        canonical.owner_vertex_id,
                        edge.with_neighbor_vid(canonical.owner_vertex_id),
                    )
                })?;
            }
        } else if let Some((alias_vertex_id, alias_slot_index, reverse_in)) = alias {
            debug_assert!(
                reverse_in,
                "directed aliases should point at reverse-IN rows"
            );
            self.with_graph_mut(|graph| {
                graph.remove_reverse_edge_at_slot(
                    alias_vertex_id,
                    canonical.label_id,
                    alias_slot_index,
                )
            })?;
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
            spec.payload_bytes,
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
    _payload_bytes: &[u8],
    _canonical: EdgeHandle,
) -> Result<(), GraphStoreError> {
    Ok(())
}
