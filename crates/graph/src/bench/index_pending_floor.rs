//! Exact Graph-owned index-floor query and owner-write benchmarks (ADR 0029).
//!
//! Fixture construction, stable writes, floor assertions, and cleanup stay outside the measured
//! closure.

use crate::facade::{GraphStore, RepairPostingOp};
use canbench_rs::bench;
use std::hint::black_box;

fn vertex_insert(vertex_id: u32) -> RepairPostingOp {
    RepairPostingOp::Label {
        remove: false,
        label_id: 1,
        vertex_id,
    }
}

fn clear_fixture(store: &GraphStore) {
    for (seq, _) in store.derived_index_outbox_peek(usize::MAX) {
        store.derived_index_outbox_remove(seq);
    }
    for (seq, _) in store.repair_journal_peek(usize::MAX) {
        store.repair_journal_remove(seq);
    }
}

fn seeded_store(rows: usize) -> GraphStore {
    let store = GraphStore::new();
    clear_fixture(&store);
    store.derived_index_outbox_append(
        1,
        (0..rows).map(|vertex_id| vertex_insert(vertex_id as u32)),
    );
    assert_eq!(store.derived_index_outbox_len(), rows as u64);
    assert_eq!(store.index_pending_min_mutation_id(), Some(1));
    store
}

fn seeded_outbox_store(rows: usize) -> GraphStore {
    let store = seeded_store(rows);
    assert_eq!(store.derived_index_outbox_len(), rows as u64);
    store
}

fn seeded_repair_store(rows: usize) -> GraphStore {
    let store = GraphStore::new();
    clear_fixture(&store);
    store.repair_journal_append(
        1,
        (0..rows).map(|vertex_id| vertex_insert(vertex_id as u32)),
    );
    assert_eq!(store.repair_journal_len(), rows as u64);
    assert_eq!(store.index_pending_min_mutation_id(), Some(1));
    store
}

#[bench(raw)]
fn bench_index_pending_floor_query_1() -> canbench_rs::BenchResult {
    let store = seeded_store(1);
    let result = canbench_rs::bench_fn(|| black_box(store.index_pending_min_mutation_id()));
    clear_fixture(&store);
    result
}

#[bench(raw)]
fn bench_index_pending_floor_query_64() -> canbench_rs::BenchResult {
    let store = seeded_store(64);
    let result = canbench_rs::bench_fn(|| black_box(store.index_pending_min_mutation_id()));
    clear_fixture(&store);
    result
}

#[bench(raw)]
fn bench_index_pending_floor_query_4096() -> canbench_rs::BenchResult {
    let store = seeded_store(4_096);
    let result = canbench_rs::bench_fn(|| black_box(store.index_pending_min_mutation_id()));
    clear_fixture(&store);
    result
}

#[bench(raw)]
fn bench_index_pending_floor_write_outbox_1() -> canbench_rs::BenchResult {
    let store = seeded_outbox_store(4_096);
    let result = canbench_rs::bench_fn(|| {
        store.derived_index_outbox_append(11, [vertex_insert(4_096)]);
    });
    assert_eq!(store.derived_index_outbox_len(), 4_097);
    assert_eq!(store.index_pending_min_mutation_id(), Some(1));
    clear_fixture(&store);
    result
}

#[bench(raw)]
fn bench_index_pending_floor_write_repair_1() -> canbench_rs::BenchResult {
    let store = seeded_repair_store(4_096);
    let result = canbench_rs::bench_fn(|| {
        store.repair_journal_append(13, [vertex_insert(4_096)]);
    });
    assert_eq!(store.repair_journal_len(), 4_097);
    assert_eq!(store.index_pending_min_mutation_id(), Some(1));
    clear_fixture(&store);
    result
}
