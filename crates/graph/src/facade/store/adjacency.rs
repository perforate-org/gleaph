//! Adjacency storage domain: canonical edge writes and their journal.

use gleaph_graph_kernel::entry::EdgeLabelId;
use gleaph_graph_kernel::entry::{EdgeOrientation, VertexId};
use gleaph_graph_kernel::plan_exec::MutationId;

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use crate::facade::store::index_build_admission::{FencedTransition, trap_post_fence_commit};
use crate::property::{
    PropertyValueChange, dispatch_inline_index_removals, dispatch_property_index_ops_for_physical,
    inline_index_values,
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
    /// Directed edge: journal and inline index postings.
    pub(super) fn commit_directed_edge_insert(
        &self,
        spec: EdgeInsertSpec<'_>,
    ) -> Result<(), GraphStoreError> {
        self.journal_edge_insert_and_index(spec)
    }

    /// Undirected edge: canonical journal and inline index postings.
    pub(super) fn commit_undirected_edge_insert(
        &self,
        canonical: EdgeInsertSpec<'_>,
    ) -> Result<(), GraphStoreError> {
        self.journal_edge_insert_and_index(canonical)
    }

    /// Removes a canonical edge (both halves) and its derived sidecars.
    pub(super) fn commit_delete_edge_by_handle(
        &self,
        handle: EdgeHandle,
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        let canonical = self
            .scan_only_canonical_edge_handle(handle, EdgeOrientation::Forward)
            .map_err(GraphStoreError::from)?;
        self.ensure_vertex_id(canonical.owner_vertex_id)
            .map_err(GraphStoreError::from)?;
        let edge =
            self.find_outgoing_edge_record(canonical)?
                .ok_or(GraphStoreError::EdgeNotFound {
                    owner_vertex_id: canonical.owner_vertex_id,
                    label_id: canonical.label_id,
                    slot_index: canonical.slot_index.raw(),
                })?;
        // Fence plan BEFORE the canonical removal: every INLINE and sidecar property removal on
        // the deleted edge is admitted (Building) or rejected (Sealing) before any stable write.
        // With no Building/Sealing membership the plan is empty, so skip the INLINE decode and
        // sidecar resolution entirely (the common case with no in-flight index build).
        let planned = if crate::index::catalog_context::has_non_active_membership() {
            let inline_values =
                inline_index_values(canonical.label_id.raw(), edge.edge_inline_property_bytes())
                    .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })?;
            let mut sidecar_values: Vec<(
                crate::index::catalog_context::IndexMembershipRef,
                gleaph_graph_kernel::entry::PropertyId,
                gleaph_gql::Value,
            )> = Vec::new();
            GraphStore::for_each_indexed_edge_property_value_on_edge(
                canonical.owner_vertex_id,
                canonical.label_id.raw(),
                canonical.slot_index.raw(),
                |membership, pid, value| sidecar_values.push((membership, pid, value)),
            );
            let mut transitions = Vec::with_capacity(inline_values.len() + sidecar_values.len());
            for (membership, property_id, value) in &inline_values {
                transitions.push(FencedTransition {
                    property_id: *property_id,
                    prev: Some(value),
                    new: None,
                    membership: *membership,
                });
            }
            for (membership, pid, value) in &sidecar_values {
                transitions.push(FencedTransition {
                    property_id: *pid,
                    prev: Some(value),
                    new: None,
                    membership: *membership,
                });
            }
            self.plan_index_build_admission(transitions)?
        } else {
            Vec::new()
        };
        if !planned.is_empty() {
            self.commit_index_build_admission(
                mutation_id,
                self.edge_subject_for_handle(canonical)?,
                planned,
            );
        }
        // The fence commit (or the canonical removal that follows) is the first stable write of
        // this mutation, so no recoverable error may be returned past this point: the IC would
        // keep the reserved outbox admission while skipping the canonical removal. Every failure
        // below is unreachable by invariant (the live edge row and counterpart were resolved
        // above and nothing changes between that resolution and the removal), so a trap rolls
        // the whole message back instead of exposing canonical state without its build-DML.
        dispatch_inline_index_removals(
            canonical.owner_vertex_id,
            canonical.label_id.raw(),
            canonical.slot_index.raw(),
            edge.edge_inline_property_bytes(),
        )
        .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })
        .unwrap_or_else(trap_post_fence_commit);
        self.commit_clear_edge_sidecars_at_canonical(canonical);
        // One call removes both halves and moves no other edge; a bucket it leaves as sparse as
        // it is full is listed for compaction, which the drain below may take (ADR 0136,
        // Decisions 4 and 6).
        let removed = crate::facade::topology::change::remove_edge(
            canonical.occurrence(EdgeOrientation::Forward),
        )
        .map_err(GraphStoreError::from)
        .unwrap_or_else(trap_post_fence_commit);
        assert!(
            removed,
            "edge resolved live at fence plan must remain removable within the same message"
        );
        self.drain_deferred_maintenance()
            .unwrap_or_else(trap_post_fence_commit);
        Ok(())
    }

    fn journal_edge_insert_and_index(
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
        // The caller has already committed the index-build fence (the first stable write), so no
        // recoverable error may be returned past this point: the IC would keep the reserved
        // outbox admission while skipping the canonical edge insert. The inline decode re-decodes
        // the exact bytes the fence plan already decoded (deterministic).
        for (membership, property_id, value) in
            inline_index_values(spec.canonical.label_id.raw(), spec.inline_property_bytes)
                .map_err(|detail| GraphStoreError::FederatedExpandPayload { detail })
                .unwrap_or_else(trap_post_fence_commit)
        {
            dispatch_property_index_ops_for_physical(
                PropertyValueChange::edge(
                    spec.canonical.owner_vertex_id,
                    spec.canonical.label_id.raw(),
                    spec.canonical.slot_index.raw(),
                    property_id,
                    None,
                    Some(&value),
                ),
                membership,
            );
        }
        Ok(())
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
