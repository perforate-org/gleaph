//! Vertex delete domain: clear derived sidecars and commit graph row removal.

use ic_stable_lara::{DeferredBidirectionalLabeledError, VertexId};

use super::GraphStore;
use super::error::GraphStoreError;

impl GraphStore {
    /// Detached vertex delete: clear sidecars, remove CSR row, drain maintenance.
    pub(super) fn commit_delete_detached_vertex(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        self.assert_local_vertex_writable(vertex_id)?;
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        if self.vertex_has_incident_edges(vertex_id)? {
            return Err(GraphStoreError::VertexNotDetached { vertex_id });
        }
        self.commit_prepare_vertex_sidecars_for_delete(vertex_id)?;
        self.with_graph_mut(|graph| graph.delete_vertex_deferred(vertex_id))?;
        self.drain_deferred_maintenance()
    }

    /// Detach-delete: resumable tombstone-first vertex purge (ADR 0021 Stage 2).
    ///
    /// Clears the vertex's own sidecars, marks it pending-purge so the read gate
    /// hides its surviving back-edges, tombstones both orientation rows in O(1)
    /// (preserving buckets), then enqueues a [`MaintenanceWorkItem::DeleteVertex`]
    /// purge and drains it under the delete budget. The incident-edge sidecars are
    /// cleared incrementally by [`GraphDeleteEdgeObserver`] as the purge drains
    /// each edge, and the vertex leaves the pending-purge set when the purge
    /// completes. Super-node deletes that exceed the per-message budget spill to
    /// the maintenance timer instead of trapping, so the legacy synchronous
    /// degree ceiling is gone.
    ///
    /// [`MaintenanceWorkItem::DeleteVertex`]: ic_stable_lara::labeled::MaintenanceWorkItem
    /// [`GraphDeleteEdgeObserver`]: super::helpers::GraphDeleteEdgeObserver
    pub(super) fn commit_detach_delete_vertex(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        self.assert_local_vertex_writable(vertex_id)?;
        self.ensure_vertex_id(vertex_id)
            .map_err(GraphStoreError::from)?;
        self.commit_prepare_vertex_sidecars_for_delete(vertex_id)?;
        // Gate before tombstone: if marking fails we must not tombstone, or the
        // vertex's surviving incident edges would be visible as ghost edges
        // (ADR 0021).
        self.mark_vertex_pending_purge(vertex_id)?;
        self.with_graph_mut(|graph| graph.begin_vertex_delete_deferred(vertex_id))?;
        self.drain_deferred_maintenance()
    }

    /// Property and label sidecars before a vertex CSR row is removed.
    pub(super) fn commit_prepare_vertex_sidecars_for_delete(
        &self,
        vertex_id: VertexId,
    ) -> Result<(), GraphStoreError> {
        self.commit_clear_vertex_properties(vertex_id);
        self.commit_clear_vertex_embeddings(vertex_id);

        let vertex = self.vertex(vertex_id).ok_or_else(|| {
            GraphStoreError::Graph(DeferredBidirectionalLabeledError::VertexOutOfRange {
                vid: vertex_id,
                len: self.vertex_count(),
            })
        })?;
        // Label sidecars live in `VERTEX_LABELS`; the CSR row is unchanged. Do not call
        // `set_vertex` here: it mirrors the forward row into reverse and would corrupt
        // reverse-only locator state for this `VertexId`.
        self.commit_clear_vertex_labels(vertex_id, vertex)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::vertex_hidden_by_pending_purge;
    use ic_stable_lara::{MaintenanceBudget, traits::CsrEdge};

    fn one_step_delete_budget() -> MaintenanceBudget {
        MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: Some(1),
        }
    }

    fn neighbors_pointing_to(store: &GraphStore, neighbors: &[VertexId], hub: VertexId) -> usize {
        neighbors
            .iter()
            .filter(|&&n| {
                store
                    .directed_out_edges(n)
                    .expect("out edges")
                    .iter()
                    .any(|e| e.neighbor_vid() == hub)
            })
            .count()
    }

    /// ADR 0021 Stage 2: a tombstone-first `DETACH DELETE` whose purge is only
    /// partially drained must keep its surviving back-edges *physically present*
    /// yet *gated out* of reads, then reconcile both once fully drained.
    #[test]
    fn partial_purge_gates_surviving_back_edges_then_full_drain_reconciles() {
        let store = GraphStore::new();
        let hub = store.insert_vertex().expect("hub");
        let neighbors: Vec<VertexId> = (0..6)
            .map(|_| {
                let n = store.insert_vertex().expect("n");
                store.insert_directed_edge(n, hub, None).expect("n->hub");
                n
            })
            .collect();
        assert_eq!(neighbors_pointing_to(&store, &neighbors, hub), 6);

        // Tombstone-first start without draining the purge to completion.
        store
            .commit_prepare_vertex_sidecars_for_delete(hub)
            .expect("prepare hub sidecars");
        store
            .mark_vertex_pending_purge(hub)
            .expect("mark pending purge");
        store
            .with_graph_mut(|graph| graph.begin_vertex_delete_deferred(hub))
            .expect("begin resumable delete");

        // One delete step: at least one back-edge survives physically.
        store
            .run_maintenance_best_effort(one_step_delete_budget())
            .expect("partial purge step");
        assert!(
            store.vertex_is_pending_purge(hub),
            "hub stays pending while incident edges drain"
        );
        assert!(
            neighbors_pointing_to(&store, &neighbors, hub) > 0,
            "partial purge must leave surviving back-edges physically present"
        );
        // The read gate keys off the pending set, so the executor hides hub.
        assert!(vertex_hidden_by_pending_purge(hub));

        // Drain the rest: pending clears and every back-edge is purged.
        store
            .drain_deferred_maintenance_with_budget(
                crate::facade::bulk_ingest_finalize_maintenance_budget(),
            )
            .expect("full drain");
        assert!(!store.vertex_is_pending_purge(hub));
        assert!(!vertex_hidden_by_pending_purge(hub));
        assert_eq!(
            neighbors_pointing_to(&store, &neighbors, hub),
            0,
            "full purge removes every incident back-edge"
        );
    }

    /// Regression: deleting a vertex whose incident edges are payload-free directed
    /// in-edges from distinct sources must drain every back-edge. The reverse-branch
    /// purge previously matched the neighbor's forward edge by slot and spun forever
    /// (ADR 0021).
    #[test]
    fn detach_delete_hub_with_no_inline_value_in_edges_drains_every_back_edge() {
        let store = GraphStore::new();
        let hub = store.insert_vertex().expect("hub");
        let neighbors: Vec<VertexId> = (0..8)
            .map(|_| {
                let n = store.insert_vertex().expect("n");
                store.insert_directed_edge(n, hub, None).expect("n->hub");
                n
            })
            .collect();
        assert_eq!(neighbors_pointing_to(&store, &neighbors, hub), 8);

        store.detach_delete_vertex(hub).expect("detach delete hub");

        assert!(!store.is_vertex_live(hub), "hub is tombstoned after purge");
        assert!(!store.vertex_is_pending_purge(hub));
        assert_eq!(
            neighbors_pointing_to(&store, &neighbors, hub),
            0,
            "no neighbor keeps a dangling forward edge to the deleted hub"
        );
    }
}
