//! Pending vertex-purge set: vertices tombstoned by a `DETACH DELETE` whose
//! incident edges are still being removed by deferred maintenance (ADR 0021).
//!
//! A tombstoned vertex mid-purge can still have surviving back-edges at its
//! neighbours until the purge drains. The read path consults this set to hide
//! those edges, preserving the ADR 0017 invariant in its refined form: a
//! tombstoned vertex has no *visible* incident edges.

use ic_stable_lara::VertexId;

use super::GraphStore;
use crate::facade::stable::PENDING_VERTEX_PURGES;

impl GraphStore {
    /// Whether any vertex is mid-purge. When `false`, the edge-read gate is inert
    /// and callers skip per-edge filtering entirely.
    pub(crate) fn has_pending_vertex_purges(&self) -> bool {
        PENDING_VERTEX_PURGES.with_borrow(|set| !set.is_empty())
    }

    /// Whether `vertex_id` is tombstoned and still mid-purge.
    pub(crate) fn vertex_is_pending_purge(&self, vertex_id: VertexId) -> bool {
        PENDING_VERTEX_PURGES.with_borrow(|set| set.contains(u32::from(vertex_id)))
    }

    /// Marks `vertex_id` as mid-purge. Idempotent.
    pub(crate) fn mark_vertex_pending_purge(&self, vertex_id: VertexId) {
        PENDING_VERTEX_PURGES.with_borrow(|set| {
            let _ = set.insert(u32::from(vertex_id));
        });
    }

    /// Clears `vertex_id` from the pending-purge set once its purge completes.
    /// Idempotent.
    pub(crate) fn clear_vertex_pending_purge(&self, vertex_id: VertexId) {
        PENDING_VERTEX_PURGES.with_borrow(|set| {
            let _ = set.clear(u32::from(vertex_id));
        });
    }
}

/// Read-gate predicate: whether an edge whose counterpart is `vertex_id` must be
/// hidden because that vertex is tombstoned and still mid-purge (ADR 0021).
///
/// A single thread-local borrow per call, short-circuiting on the empty set so
/// steady state (no in-flight purge) costs one branch. Stateless entry point so
/// the query executor's `ExpandDst::from_edge` chokepoint can gate every
/// edge-yield without threading a `GraphStore`.
pub(crate) fn vertex_hidden_by_pending_purge(vertex_id: VertexId) -> bool {
    PENDING_VERTEX_PURGES.with_borrow(|set| !set.is_empty() && set.contains(u32::from(vertex_id)))
}
