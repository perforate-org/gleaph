//! Graph timer-maintenance benchmarks for the GAP-2026-07-17-001 headroom acceptance on the
//! LARA deferred-maintenance timer path (ADR 0020).
//!
//! `run_timer_maintenance_tick` drains the deferred LARA queue under
//! `timer_lara_maintenance_budget()`: `MAX_TIMER_MAINTENANCE_INSTRUCTIONS` (32B) with
//! `TIMER_MAINTENANCE_INSTRUCTION_HEADROOM` (100M). The bounded loop
//! (`DeferredBidirectionalLabeledLaraGraph::maintenance_with_observers`) runs
//! `should_cutoff(32B, used, 0, 100M, 0)` before each work item, so the maximum work between
//! budget checks is one work item plus the report. These benches measure the whole timer tick
//! on a dense overflow-backed hub (the compaction backlog a load path enqueues) and isolate a
//! single work item under the same budget — the per-check cost the 100M reserve must cover with
//! margin.

use crate::facade::GraphStore;
use canbench_rs::bench;
use gleaph_graph_kernel::entry::{
    EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, EdgeLabelId,
};
use ic_stable_lara::labeled::LabeledOrientation;
use std::hint::black_box;

const TIMER_MAINTENANCE_HUB_EDGES: usize = 2048;

fn timer_maintenance_label() -> EdgeLabelId {
    let label = crate::test_labels::edge_label_id_for_name("TimerMaintenanceHub");
    crate::test_labels::install_test_edge_inline_property_profile(
        label,
        EdgeInlinePropertyProfile {
            byte_width: 8,
            encoding: EdgeInlinePropertyEncoding::RawU64,
        },
    );
    label
}

/// A hub with `TIMER_MAINTENANCE_HUB_EDGES` labeled edges (8-byte inline property). The dense
/// insert path enqueues `CompactDenseLabeledVertexMaintenance` for the overflow-backed hub
/// span; an explicit mark makes the queued compaction deterministic for the bench.
fn seed_timer_maintenance_hub(store: &GraphStore) {
    let label = timer_maintenance_label();
    let hub = store.insert_vertex().expect("hub");
    for index in 0..TIMER_MAINTENANCE_HUB_EDGES {
        let target = store.insert_vertex().expect("target");
        let bytes = (index as u64).to_le_bytes();
        store
            .insert_directed_edge_with_inline_property_bytes(hub, target, Some(label), &bytes)
            .expect("edge");
    }
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_dense_labeled_vertex_maintenance(
                LabeledOrientation::Forward,
                hub,
                &GraphStore::maintenance_policy_for_label,
            )
            .expect("mark dense maintenance");
    });
    assert!(
        store.maintenance_queue_len() > 0,
        "timer-maintenance fixture must queue compaction work"
    );
}

/// The full timer tick draining a dense 2048-edge hub — the whole `run_timer_maintenance_tick`
/// pass under `timer_lara_maintenance_budget()`.
#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_graph_timer_maintenance_tick_hub_2048() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    seed_timer_maintenance_hub(&store);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("timer_maintenance_tick_hub_2048");
        let report = store.run_timer_maintenance_tick().expect("tick");
        black_box(report);
    })
}

/// One work item under the timer budget (`max_work_items: 1` on
/// `timer_lara_maintenance_budget()`) — the maximum work between the loop's budget checks,
/// which the 100M reserve must cover with margin.
#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_graph_timer_maintenance_single_step_hub_2048() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    seed_timer_maintenance_hub(&store);
    let mut budget = crate::facade::timer_lara_maintenance_budget();
    budget.max_work_items = Some(1);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("timer_maintenance_single_step_hub_2048");
        let report = store
            .run_maintenance_best_effort(budget)
            .expect("single maintenance step");
        black_box(report);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_maintenance_fixture_queues_and_drains() {
        let store = GraphStore::new();
        seed_timer_maintenance_hub(&store);
        assert!(store.maintenance_queue_len() > 0);
        let report = store.run_timer_maintenance_tick().expect("tick");
        assert_eq!(report.remaining_queue_len(), 0);
        assert!(
            report.work.processed_work_items > 0,
            "timer-maintenance fixture must produce measurable compaction work"
        );
    }
}
