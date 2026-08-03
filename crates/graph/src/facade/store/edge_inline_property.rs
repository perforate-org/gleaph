//! Inline edge inline property bytes update public API (delegates to edge-profile domain commits).

use super::GraphStore;
use super::error::GraphStoreError;
use super::handle::EdgeHandle;
use gleaph_graph_kernel::plan_exec::MutationId;
use ic_stable_lara::labeled::CanonicalEdgeOccurrence;

impl GraphStore {
    /// Updates the inline property bytes at `handle`.
    pub(crate) fn update_edge_inline_property_at_handle(
        &self,
        handle: EdgeHandle,
        inline_property_bytes: &[u8],
    ) -> Result<(), GraphStoreError> {
        self.commit_update_edge_inline_property_at_handle(handle, inline_property_bytes, 0)
    }

    /// Updates the inline property bytes at `handle` under an explicit canonical mutation identity.
    pub(crate) fn update_edge_inline_property_at_handle_with_mutation_id(
        &self,
        handle: EdgeHandle,
        inline_property_bytes: &[u8],
        mutation_id: MutationId,
    ) -> Result<(), GraphStoreError> {
        self.commit_update_edge_inline_property_at_handle(
            handle,
            inline_property_bytes,
            mutation_id,
        )
    }

    /// Updates inline property bytes for an orientation-aware live occurrence.
    ///
    /// `EdgeHandle` intentionally carries no orientation, so reverse-store callers
    /// must use this entry point rather than guessing from a logical slot.
    pub(crate) fn update_edge_inline_property_at_occurrence(
        &self,
        occurrence: CanonicalEdgeOccurrence,
        inline_property_bytes: &[u8],
    ) -> Result<(), GraphStoreError> {
        self.commit_update_edge_inline_property_at_occurrence(occurrence, inline_property_bytes, 0)
    }
}
