//! Local index domain; edge property postings live on graph-index (ADR 0009).

use crate::index::edge_pending;
use ic_stable_lara::VertexId;

use super::GraphStore;
use super::handle::EdgeHandle;
impl GraphStore {
    pub(super) fn commit_remove_all_edge_equality_postings(
        &self,
        _owner_vertex_id: VertexId,
        _label_id: u16,
        _slot_index: u32,
    ) {
        // Edge equality postings live on graph-index (ADR 0009); removals enqueue via edge_pending.
    }

    pub(super) fn commit_clear_edge_local_indexes_at_canonical(&self, handle: EdgeHandle) {
        edge_pending::enqueue_removals_for_edge(
            handle.owner_vertex_id,
            handle.label_id.raw(),
            handle.slot_index.raw(),
        );
    }
}
