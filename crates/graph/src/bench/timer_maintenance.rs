//! Graph timer-maintenance benchmarks for the GAP-2026-07-17-001 headroom acceptance on the
//! maintenance timer path (ADR 0020, ADR 0136 Decision 6).
//!
//! `run_timer_maintenance_tick` takes work list steps under `timer_lara_maintenance_budget()`:
//! `MAX_TIMER_MAINTENANCE_INSTRUCTIONS` (32B) with `TIMER_MAINTENANCE_INSTRUCTION_HEADROOM`
//! (100M). The loop (`GraphStore::run_maintenance_best_effort`) runs
//! `should_cutoff(32B, used, 0, 100M, 0)` before each step, so the most work between budget
//! checks is one store step (one compaction page, or one detach step) and its sidecar updates.
//! These benches measure the whole tick on a hub whose bucket half its removals left listed for
//! compaction, and isolate one step under the same budget: the per-check cost the 100M reserve
//! must cover with margin.

use crate::facade::GraphStore;
use canbench_rs::bench;
use gleaph_graph_kernel::entry::{
    EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, EdgeLabelId, EdgeOrientation,
};
use std::hint::black_box;

const TIMER_MAINTENANCE_HUB_EDGES: usize = 2048;

fn timer_maintenance_label(name: &str) -> EdgeLabelId {
    let label = crate::test_labels::edge_label_id_for_name(name);
    crate::test_labels::install_test_edge_inline_property_profile(
        label,
        EdgeInlinePropertyProfile {
            byte_width: 8,
            encoding: EdgeInlinePropertyEncoding::RawU64,
        },
    );
    label
}

/// A hub with `TIMER_MAINTENANCE_HUB_EDGES` labeled edges (8-byte inline property), every other
/// one removed in the store alone, so the hub's bucket holds as many holes as live edges and is
/// listed for compaction, with nothing drained yet. Each target's emptied in bucket is compacted
/// at once by its removal, so the list holds the hub's bucket alone. Its two half-empty pages
/// fit one, so one step packs its 1,024 live edges into a run, every one moved: lara-graph's
/// compaction and the sidecars following the moves (about 1.9M and 0.5M instructions).
fn seed_timer_maintenance_hub(store: &GraphStore) {
    seed_hub_of(store, timer_maintenance_label("TimerMaintenanceHub"));
}

/// [`seed_timer_maintenance_hub`] with its label's inline value under an Active edge index and
/// the index routed, so that each moved edge queues its posting's move. The catalog is installed
/// while the returned guard lives.
fn seed_indexed_timer_maintenance_hub(
    store: &GraphStore,
) -> crate::index::catalog_context::CatalogGuard {
    use gleaph_graph_kernel::index::{
        EdgeIndexDirection, IndexMaintenancePhase, IndexedEdgeMembership, IndexedPropertyCatalog,
        PhysicalIndexId,
    };
    store
        .set_federation_routing(Some(crate::facade::FederationRouting {
            router_canister: candid::Principal::management_canister(),
            index_canister: candid::Principal::management_canister(),
            shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
            vector_canister: None,
            text_targets: Default::default(),
        }))
        .expect("route the index");
    let label = timer_maintenance_label("TimerMaintenanceIndexedHub");
    let property = gleaph_graph_kernel::entry::PropertyId::from_raw(TIMER_MAINTENANCE_PROPERTY);
    crate::test_labels::install_test_edge_inline_property(label, property);
    let catalog = crate::index::catalog_context::enter(IndexedPropertyCatalog {
        edge_indexes: vec![IndexedEdgeMembership {
            physical_index_id: PhysicalIndexId::new(TIMER_MAINTENANCE_INDEX).expect("index id"),
            catalog_epoch: 1,
            phase: IndexMaintenancePhase::Active,
            label_id: label.raw(),
            property_id: property.raw(),
            direction: EdgeIndexDirection::Outgoing,
            field_path: String::new(),
        }],
        ..Default::default()
    });
    seed_hub_of(store, label);
    // The inserts' and removals' postings are not the step's.
    crate::index::edge_pending::clear_pending();
    catalog
}

/// The inline property and index of the indexed hub's label.
const TIMER_MAINTENANCE_PROPERTY: u32 = 7_701;
const TIMER_MAINTENANCE_INDEX: u64 = 7_702;

fn seed_hub_of(store: &GraphStore, label: EdgeLabelId) {
    let hub = store.insert_vertex().expect("hub");
    let mut handles = Vec::with_capacity(TIMER_MAINTENANCE_HUB_EDGES);
    for index in 0..TIMER_MAINTENANCE_HUB_EDGES {
        let target = store.insert_vertex().expect("target");
        let bytes = (index as u64).to_le_bytes();
        handles.push(
            store
                .insert_directed_edge_with_inline_property_bytes(hub, target, Some(label), &bytes)
                .expect("edge"),
        );
    }
    for handle in handles.iter().step_by(2) {
        assert!(
            crate::facade::topology::change::remove_edge(
                handle.occurrence(EdgeOrientation::Forward)
            )
            .expect("remove"),
            "seeded edge is live"
        );
    }
    assert!(
        store.maintenance_queue_len() > 0,
        "timer-maintenance fixture must list compaction work"
    );
}

/// The full timer tick compacting a 2048-position hub bucket half of whose edges were removed:
/// the whole `run_timer_maintenance_tick` pass under `timer_lara_maintenance_budget()`.
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

/// One step under the timer budget (`max_steps: 1` on `timer_lara_maintenance_budget()`): the
/// most work between the loop's budget checks, which the 100M reserve must cover with margin.
#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_graph_timer_maintenance_single_step_hub_2048() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    seed_timer_maintenance_hub(&store);
    let mut budget = crate::facade::timer_lara_maintenance_budget();
    budget.max_steps = Some(1);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("timer_maintenance_single_step_hub_2048");
        let report = store
            .run_maintenance_best_effort(budget)
            .expect("single maintenance step");
        black_box(report);
    })
}

/// [`bench_graph_timer_maintenance_single_step_hub_2048`] on a label whose inline value an
/// Active edge index takes: the step reads the moved edges' values once and queues each
/// posting's removal and insert, and the flush sends each pair as one move
/// (`index_moves_to_mutations`).
#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_graph_timer_maintenance_single_step_indexed_hub_2048() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let _catalog = seed_indexed_timer_maintenance_hub(&store);
    let mut budget = crate::facade::timer_lara_maintenance_budget();
    budget.max_steps = Some(1);
    let sent = core::cell::Cell::new(0);
    let result = canbench_rs::bench_fn(|| {
        {
            let _scope = canbench_rs::bench_scope("timer_maintenance_single_step_indexed_hub_2048");
            black_box(
                store
                    .run_maintenance_best_effort(budget)
                    .expect("single maintenance step"),
            );
        }
        let _scope = canbench_rs::bench_scope("index_moves_to_mutations");
        let pending = crate::index::edge_pending::take_pending();
        let mutations =
            crate::index::edge_pending::to_index_mutations(&pending).expect("Active postings");
        sent.set(mutations.len());
        black_box(mutations);
    });
    assert_eq!(
        sent.get(),
        TIMER_MAINTENANCE_HUB_EDGES / 2,
        "one move per moved posting"
    );
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timer_maintenance_fixture_lists_and_drains() {
        let store = GraphStore::new();
        seed_timer_maintenance_hub(&store);
        let report = store.run_timer_maintenance_tick().expect("tick");
        assert_eq!(report.remaining_queue_len, 0);
        assert!(
            report.processed_work_items > 0,
            "timer-maintenance fixture must produce measurable compaction work"
        );
    }
}
