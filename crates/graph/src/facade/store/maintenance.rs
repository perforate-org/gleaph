//! GraphStore maintenance: driving the topology work list (ADR 0136, Decision 6).
//!
//! The work list holds the buckets to compact and the vertices to detach; each step is one
//! bounded store call that hands back what it changed. This module takes steps until the
//! instruction budget's headroom is reached, and applies each step to the sidecars and indexes
//! before taking the next: a compaction's slot moves re-key the moved edges' properties and
//! postings, a detach step's removed edges lose theirs, and a vertex whose detach finished
//! leaves the pending-purge set.
use crate::facade::topology::change::CompactionSide;
use crate::facade::topology::work::WorkStep;
use gleaph_graph_kernel::entry::VertexId;

use super::GraphStore;
use super::error::GraphStoreError;
use super::helpers::clear_removed_edge;
use crate::facade::store::index_build_admission::trap_post_fence_commit;

/// How much maintenance one pass may do.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MaintenanceBudget {
    /// Instructions the pass may use from its start; `0` for no cap (native builds, which have
    /// no instruction counter).
    pub max_instructions: u64,
    /// Headroom kept free: no step starts once the pass is within it of `max_instructions`.
    pub reserve_instructions: u64,
    /// At most this many store steps (one compaction or detach step each), when given.
    pub max_steps: Option<u32>,
}

/// What one maintenance pass did.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MaintenanceReport {
    /// Store steps taken.
    pub processed_work_items: u32,
    /// Entries left on the work list after the pass.
    pub remaining_queue_len: u64,
    /// Whether the instruction budget stopped the pass with work left.
    pub instruction_budget_exhausted: bool,
    /// Instructions the pass used.
    pub instructions_used: u64,
}

/// Caller-supplied vertices to compact after a bulk ingest batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BulkIngestFinalizeSpec {
    /// Vertices whose directed out-edge and undirected buckets should be compacted.
    pub forward_vertices: Vec<VertexId>,
    /// Vertices whose directed in-edge buckets should be compacted.
    pub reverse_vertices: Vec<VertexId>,
}

/// Result of listing and/or draining for [`GraphStore::finalize_bulk_ingest`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct BulkIngestFinalizeReport {
    /// Maintenance work performed by the drain pass (if any).
    pub maintenance: MaintenanceReport,
    /// Forward vertices examined on this call (each bucket with a hole is listed).
    pub queued_forward: u32,
    /// Reverse vertices examined on this call (each bucket with a hole is listed).
    pub queued_reverse: u32,
}

impl GraphStore {
    /// Takes work list steps within `budget`, applying each before the next.
    pub fn run_maintenance_best_effort(
        &self,
        budget: MaintenanceBudget,
    ) -> Result<MaintenanceReport, GraphStoreError> {
        let baseline = gleaph_instruction_budget::instruction_counter();
        let mut report = MaintenanceReport::default();
        loop {
            if budget
                .max_steps
                .is_some_and(|max| report.processed_work_items >= max)
            {
                break;
            }
            report.instructions_used =
                gleaph_instruction_budget::instruction_counter().saturating_sub(baseline);
            if budget.max_instructions > 0
                && gleaph_instruction_budget::should_cutoff(
                    budget.max_instructions,
                    report.instructions_used,
                    0,
                    budget.reserve_instructions,
                    0,
                )
            {
                report.instruction_budget_exhausted =
                    crate::facade::topology::change::maintenance_queue_len() > 0;
                break;
            }
            let Some(step) = crate::facade::topology::work::step()? else {
                break;
            };
            report.processed_work_items = report.processed_work_items.saturating_add(1);
            self.apply_work_step(step);
        }
        report.instructions_used =
            gleaph_instruction_budget::instruction_counter().saturating_sub(baseline);
        report.remaining_queue_len = crate::facade::topology::change::maintenance_queue_len();
        Ok(report)
    }

    /// Applies one work list step. The store has already changed, so a failure to follow it
    /// traps and rolls the whole message back.
    fn apply_work_step(&self, step: WorkStep) {
        match step {
            WorkStep::Compacted(moves) => {
                Self::apply_slot_moves(moves, 0.into()).unwrap_or_else(trap_post_fence_commit);
            }
            WorkStep::Detached { removed, deleted } => {
                for edge in &removed {
                    clear_removed_edge(*self, edge);
                }
                if let Some(vertex_id) = deleted {
                    self.clear_vertex_pending_purge(vertex_id);
                }
            }
        }
    }

    pub fn run_timer_maintenance_tick(&self) -> Result<MaintenanceReport, GraphStoreError> {
        self.run_maintenance_best_effort(crate::facade::timer_lara_maintenance_budget())
    }

    /// Number of entries on the maintenance work list.
    pub(crate) fn maintenance_queue_len(&self) -> u64 {
        crate::facade::topology::change::maintenance_queue_len()
    }

    /// Takes work list steps after delete-style mutations under the delete budget, then arms the
    /// maintenance timer if work remains (ADR 0020).
    ///
    /// On non-wasm builds the budget is unlimited (full drain) and the arm is a no-op, so tests
    /// observe fully reclaimed state.
    pub(crate) fn drain_deferred_maintenance(&self) -> Result<(), GraphStoreError> {
        self.run_maintenance_best_effort(crate::facade::delete_maintenance_budget())?;
        crate::facade::maintenance_timer::arm_if_needed();
        Ok(())
    }

    /// Takes work list steps once, at the end of an update message, then arms the timer if work
    /// remains.
    pub fn drain_maintenance_at_message_end(&self) -> Result<(), GraphStoreError> {
        self.run_maintenance_best_effort(crate::facade::ic_budget::timer_lara_maintenance_budget())?;
        crate::facade::maintenance_timer::arm_if_needed();
        Ok(())
    }

    /// Lists compaction of every bucket with a hole of the listed vertices. A bulk load and
    /// fresh inserts leave none, so this usually lists nothing; see
    /// `design/storage/bulk-ingest-finalize.md`.
    pub fn enqueue_bulk_ingest_finalize(
        &self,
        spec: &BulkIngestFinalizeSpec,
    ) -> Result<BulkIngestFinalizeReport, GraphStoreError> {
        let (queued_forward, queued_reverse) = self.enqueue_bulk_ingest_finalize_vertices(spec)?;
        // List-only path: no inline drain, so arm the timer to take the listed work even if no
        // explicit finalize drain follows (ADR 0020).
        crate::facade::maintenance_timer::arm_if_needed();
        Ok(BulkIngestFinalizeReport {
            maintenance: MaintenanceReport::default(),
            queued_forward,
            queued_reverse,
        })
    }

    /// Takes work list steps under the bulk-ingest finalize budget, then arms the maintenance
    /// timer if work remains (ADR 0020).
    pub fn run_bulk_ingest_finalize_drain(&self) -> Result<MaintenanceReport, GraphStoreError> {
        let report = self
            .run_maintenance_best_effort(crate::facade::bulk_ingest_finalize_maintenance_budget())?;
        crate::facade::maintenance_timer::arm_if_needed();
        Ok(report)
    }

    /// Lists compaction for the listed vertices, then runs one finalize drain pass.
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
        let list = |vertices: &[VertexId], side| -> Result<u32, GraphStoreError> {
            let vertices = dedup_vertex_ids(vertices);
            for &vid in &vertices {
                crate::facade::topology::change::list_compaction_of_holes(vid, side)?;
            }
            Ok(u32::try_from(vertices.len()).unwrap_or(u32::MAX))
        };
        Ok((
            list(&spec.forward_vertices, CompactionSide::Forward)?,
            list(&spec.reverse_vertices, CompactionSide::Reverse)?,
        ))
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
    use crate::facade::topology::EdgeInlinePropertyBatchScratch;
    use gleaph_graph_kernel::entry::EdgeOrder;
    use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};

    use super::*;
    use crate::test_labels::install_test_edge_inline_property_profile;

    fn setup_converging_hub_without_tombstones(store: &GraphStore) -> VertexId {
        let src = store.insert_vertex().expect("src");
        let hub = store.insert_vertex().expect("hub");
        let label = crate::test_labels::edge_label_id_for_name("BulkFinalizeRoad");
        install_test_edge_inline_property_profile(
            label,
            EdgeInlinePropertyProfile {
                byte_width: 2,
                encoding: EdgeInlinePropertyEncoding::RawU16,
            },
        );

        let mut prefixes = Vec::new();
        for _ in 0..48 {
            prefixes.push(store.insert_vertex().expect("prefix"));
        }
        for &prefix in &prefixes {
            store
                .insert_directed_edge_with_inline_property_bytes(
                    prefix,
                    hub,
                    Some(label),
                    &1u16.to_le_bytes(),
                )
                .expect("prefix->hub");
        }
        for (i, &prefix) in prefixes.iter().enumerate() {
            store
                .insert_directed_edge_with_inline_property_bytes(
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
    fn finalize_bulk_ingest_preserves_hot_forward_inline_properties() {
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

        let mut scratch = EdgeInlinePropertyBatchScratch::default();
        let mut edge_count = 0;
        store
            .visit_directed_out_edge_inline_property_batches_for_label(
                src,
                road,
                EdgeOrder::Descending,
                &mut scratch,
                |batch| edge_count += batch.edges.len(),
            )
            .expect("inline property batches");
        assert_eq!(edge_count, 48);
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
