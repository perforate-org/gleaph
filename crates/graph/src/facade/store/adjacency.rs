//! Adjacency storage domain: canonical edge writes plus derived alias, journal, and maintenance.

use gleaph_graph_kernel::entry::EdgeLabelId;
use gleaph_graph_kernel::federation::LogicalVertexId;
use ic_stable_lara::VertexId;

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

    /// Remote/logical edge: journal and deferred maintenance after forward-in registration.
    pub(super) fn commit_logical_edge_insert(
        &self,
        source_vertex_id: VertexId,
        target_logical_vertex_id: LogicalVertexId,
        target_is_remote: bool,
        catalog_label: Option<EdgeLabelId>,
        undirected: bool,
        payload_bytes: &[u8],
        handle: EdgeHandle,
    ) -> Result<(), GraphStoreError> {
        journal_edge_insert_to_logical(
            self,
            source_vertex_id,
            target_logical_vertex_id,
            target_is_remote,
            catalog_label,
            undirected,
            payload_bytes,
            handle,
        )?;
        self.run_post_edge_insert_maintenance()
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

pub(super) fn journal_edge_insert_to_logical(
    _store: &GraphStore,
    _source_vertex_id: VertexId,
    _target_logical_vertex_id: LogicalVertexId,
    _target_is_remote: bool,
    _catalog_label: Option<EdgeLabelId>,
    _undirected: bool,
    _payload_bytes: &[u8],
    _source_handle: EdgeHandle,
) -> Result<(), GraphStoreError> {
    Ok(())
}
