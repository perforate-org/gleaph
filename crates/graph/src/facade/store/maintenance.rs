//! GraphStore `maintenance` implementation.

use super::super::stable::GRAPH;
use ic_stable_lara::{
    MaintenanceBudget, VertexId,
    labeled::{LabeledBidirectionalMaintenanceReport, LabeledOrientation},
};

use super::GraphStore;
use super::error::GraphStoreError;
use super::helpers::GraphSidecarMoveObserver;

/// Caller-supplied vertices to compact after a tombstone-free bulk ingest batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BulkIngestFinalizeSpec {
    /// Forward out-adjacency vertices whose edge spans should be compacted.
    pub forward_vertices: Vec<VertexId>,
    /// Reverse in-adjacency vertices whose edge spans should be compacted.
    pub reverse_vertices: Vec<VertexId>,
}

/// Result of enqueue and/or drain for [`GraphStore::finalize_bulk_ingest`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BulkIngestFinalizeReport {
    /// Maintenance work performed by the drain pass (if any).
    pub maintenance: LabeledBidirectionalMaintenanceReport,
    /// Forward vertices enqueued for `CompactVertexEdgeSpan` on this call.
    pub queued_forward: u32,
    /// Reverse vertices enqueued for `CompactVertexEdgeSpan` on this call.
    pub queued_reverse: u32,
}

impl GraphStore {
    pub fn run_maintenance_best_effort(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<LabeledBidirectionalMaintenanceReport, GraphStoreError> {
        let mut observer = GraphSidecarMoveObserver;
        let report = GRAPH
            .with_borrow(|graph| {
                graph.maintenance_with_edge_slot_move_observer(budget, &mut observer)
            })
            .map_err(GraphStoreError::from)?;
        Ok(report)
    }

    pub fn run_timer_maintenance_tick(
        &self,
    ) -> Result<LabeledBidirectionalMaintenanceReport, GraphStoreError> {
        self.run_maintenance_best_effort(crate::facade::timer_lara_maintenance_budget())
    }

    /// Number of pending deferred-maintenance work items in the stable queue.
    pub(crate) fn maintenance_queue_len(&self) -> u64 {
        GRAPH.with_borrow(|graph| graph.maintenance_queue_len())
    }

    /// Drains queued LARA maintenance after delete-style mutations under the
    /// delete budget, then arms the maintenance timer if work remains (ADR 0020).
    ///
    /// On non-wasm builds the budget is unlimited (full drain) and the arm is a
    /// no-op, so tests observe fully reclaimed state.
    pub(crate) fn drain_deferred_maintenance(&self) -> Result<(), GraphStoreError> {
        self.drain_deferred_maintenance_with_budget(crate::facade::delete_maintenance_budget())?;
        crate::facade::maintenance_timer::arm_if_needed();
        Ok(())
    }

    pub(crate) fn drain_deferred_maintenance_with_budget(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<(), GraphStoreError> {
        self.run_maintenance_best_effort(budget)?;
        Ok(())
    }

    /// Drains maintenance queued by the LARA insert path (`mark_compact_dense`, etc.).
    ///
    /// Does not enqueue extra compaction work: forcing `CompactVertexEdgeSpan` on every
    /// insert breaks tombstoned buckets (see `valued_insert_after_delete_*` tests).
    pub(crate) fn run_post_edge_insert_maintenance(&self) -> Result<(), GraphStoreError> {
        self.drain_deferred_maintenance_with_budget(
            crate::facade::post_edge_insert_maintenance_budget(),
        )?;
        crate::facade::maintenance_timer::arm_if_needed();
        Ok(())
    }

    /// Enqueues `CompactVertexEdgeSpan` for tombstone-free bulk ingest vertices.
    ///
    /// The caller must guarantee no live tombstones on the listed spans; see
    /// `design/storage/bulk-ingest-finalize.md`.
    pub fn enqueue_bulk_ingest_finalize(
        &self,
        spec: &BulkIngestFinalizeSpec,
    ) -> Result<BulkIngestFinalizeReport, GraphStoreError> {
        let (queued_forward, queued_reverse) = self.enqueue_bulk_ingest_finalize_vertices(spec)?;
        // Enqueue-only path: no inline drain, so arm the timer to drain the
        // queued compaction even if no explicit finalize drain follows (ADR 0020).
        crate::facade::maintenance_timer::arm_if_needed();
        Ok(BulkIngestFinalizeReport {
            maintenance: LabeledBidirectionalMaintenanceReport::default(),
            queued_forward,
            queued_reverse,
        })
    }

    /// Drains the deferred maintenance queue under the bulk-ingest finalize budget,
    /// then arms the maintenance timer if work remains (ADR 0020).
    pub fn run_bulk_ingest_finalize_drain(
        &self,
    ) -> Result<LabeledBidirectionalMaintenanceReport, GraphStoreError> {
        let report = self
            .run_maintenance_best_effort(crate::facade::bulk_ingest_finalize_maintenance_budget())?;
        crate::facade::maintenance_timer::arm_if_needed();
        Ok(report)
    }

    /// Enqueues span compaction for listed vertices, then runs one finalize drain pass.
    pub fn finalize_bulk_ingest(
        &self,
        spec: &BulkIngestFinalizeSpec,
    ) -> Result<BulkIngestFinalizeReport, GraphStoreError> {
        let (queued_forward, queued_reverse) = self.enqueue_bulk_ingest_finalize_vertices(spec)?;
        let maintenance = self.run_bulk_ingest_finalize_drain()?;
        Ok(BulkIngestFinalizeReport {
            maintenance,
            queued_forward,
            queued_reverse,
        })
    }

    fn enqueue_bulk_ingest_finalize_vertices(
        &self,
        spec: &BulkIngestFinalizeSpec,
    ) -> Result<(u32, u32), GraphStoreError> {
        let forward_vertices = dedup_vertex_ids(&spec.forward_vertices);
        let reverse_vertices = dedup_vertex_ids(&spec.reverse_vertices);
        let mut queued_forward = 0u32;
        let mut queued_reverse = 0u32;
        self.with_graph_mut(|graph| {
            for vid in forward_vertices {
                graph
                    .mark_compact_vertex_edge_span(LabeledOrientation::Forward, vid, 0)
                    .map_err(GraphStoreError::from)?;
                queued_forward += 1;
            }
            for vid in reverse_vertices {
                graph
                    .mark_compact_vertex_edge_span(LabeledOrientation::Reverse, vid, 0)
                    .map_err(GraphStoreError::from)?;
                queued_reverse += 1;
            }
            Ok::<(), GraphStoreError>(())
        })?;
        Ok((queued_forward, queued_reverse))
    }
}

fn dedup_vertex_ids(vertices: &[VertexId]) -> Vec<VertexId> {
    let mut out = vertices.to_vec();
    out.sort_by_key(|vid| u32::from(*vid));
    out.dedup();
    out
}

#[cfg(test)]
mod tests {
    use gleaph_graph_kernel::entry::{EdgePayloadProfile, EdgeWeightProfile, WeightEncoding};
    use ic_stable_lara::{OutEdgeOrder, labeled::LabeledEdgePayloadBatchScratch};

    use super::*;
    use crate::test_labels::install_test_edge_payload_profile;

    fn setup_converging_hub_without_tombstones(store: &GraphStore) -> VertexId {
        let src = store.insert_vertex().expect("src");
        let hub = store.insert_vertex().expect("hub");
        let label = crate::test_labels::edge_label_id_for_name("BulkFinalizeRoad");
        install_test_edge_payload_profile(
            label,
            EdgePayloadProfile::from(EdgeWeightProfile {
                encoding: WeightEncoding::RawU16,
            }),
        );

        let mut prefixes = Vec::new();
        for _ in 0..48 {
            prefixes.push(store.insert_vertex().expect("prefix"));
        }
        for &prefix in &prefixes {
            store
                .insert_directed_edge_with_payload_bytes(
                    prefix,
                    hub,
                    Some(label),
                    &1u16.to_le_bytes(),
                )
                .expect("prefix->hub");
        }
        for (i, &prefix) in prefixes.iter().enumerate() {
            store
                .insert_directed_edge_with_payload_bytes(
                    src,
                    prefix,
                    Some(label),
                    &((i % 10) as u16 + 1).to_le_bytes(),
                )
                .expect("src->prefix");
        }
        src
    }

    #[test]
    fn finalize_bulk_ingest_makes_hot_forward_span_dense_eligible() {
        let store = GraphStore::new();
        let src = setup_converging_hub_without_tombstones(&store);
        let road = crate::test_labels::edge_label_id_for_name("BulkFinalizeRoad");

        let report = store
            .finalize_bulk_ingest(&BulkIngestFinalizeSpec {
                forward_vertices: vec![src],
                reverse_vertices: vec![],
            })
            .expect("finalize");
        assert_eq!(report.queued_forward, 1);
        assert_eq!(report.queued_reverse, 0);

        let mut scratch = LabeledEdgePayloadBatchScratch::default();
        let mut dense = None;
        store
            .visit_directed_out_edge_payload_batches_for_label(
                src,
                road,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| dense = Some(batch.dense),
            )
            .expect("payload batches");
        assert_eq!(dense, Some(true));
    }

    #[test]
    fn enqueue_bulk_ingest_finalize_dedupes_vertex_lists() {
        let store = GraphStore::new();
        let src = store.insert_vertex().expect("src");
        let report = store
            .enqueue_bulk_ingest_finalize(&BulkIngestFinalizeSpec {
                forward_vertices: vec![src, src],
                reverse_vertices: vec![],
            })
            .expect("enqueue");
        assert_eq!(report.queued_forward, 1);
    }
}
