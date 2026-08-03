//! Ordered-batch and Atomic-admission benchmarks for ADR 0060.
//!
//! The `Resumable` ordered-batch path executes operations one at a time through
//! `resumable_prefix_len` (a per-operation `should_cutoff` check plus `OpCostTracker`
//! learning), while the `Atomic` path executes the same operations as one bulk write.
//! The paired benches below quantify that op-by-op loop overhead at the same operation
//! count (256, matching the ordered edge benches in `batch_placement.rs`).
//!
//! The Atomic preflight bench verifies the admission effect on a 1024-operation request:
//! with the conservative seed estimate (`GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION`
//! = 500M, admitting at most 70 operations) the preflight rejects it, while with a
//! measured per-operation estimate it admits it. `bench_atomic_insert_max_receipt`
//! measures receipt encoding only, which the preflight does not affect.

use crate::canister::handlers::resumable_prefix_len;
use crate::facade::GraphStore;
use crate::facade::mutation_executor::insert_vertices_with;
use canbench_rs::bench;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{PropertyId, VertexLabelId};
use gleaph_graph_kernel::{
    GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION, MAX_DYNAMIC_UPDATE_INSTRUCTIONS,
};
use gleaph_instruction_budget::preflight_operation_count;
use std::hint::black_box;

const BENCH_VERTEX_LABEL: u16 = 90;
const BENCH_VERTEX_COUNT: usize = 256;

fn vertex_fixture(count: usize) -> Vec<(Vec<VertexLabelId>, Vec<(PropertyId, Value)>)> {
    let label = VertexLabelId::from_raw(BENCH_VERTEX_LABEL);
    (0..count)
        .map(|_| (vec![label], Vec::<(PropertyId, Value)>::new()))
        .collect()
}

/// Whole-batch vertex insertion of 256 vertices in one call — the `Atomic` ordered-batch
/// write shape (ADR 0060 Decision 2).
#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_ordered_vertex_whole_batch_256() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let vertices = vertex_fixture(BENCH_VERTEX_COUNT);
    canbench_rs::bench_fn(|| {
        let ids = insert_vertices_with(black_box(&store), vertices.clone()).expect("insert");
        black_box(ids)
    })
}

/// The same 256 vertices inserted one at a time through the `Resumable` prefix loop
/// (`should_cutoff` + `OpCostTracker` per operation). The instruction source is a stub
/// that never exhausts the budget, so the loop runs to completion and the bench measures
/// the full op-by-op cost.
#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_ordered_vertex_resumable_prefix_256() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let vertices = vertex_fixture(BENCH_VERTEX_COUNT);
    canbench_rs::bench_fn(|| {
        let committed = resumable_prefix_len(
            vertices.len(),
            || 0,
            |index| {
                let ids = insert_vertices_with(&store, vec![vertices[index].clone()])
                    .expect("insert one");
                black_box(ids);
            },
        );
        black_box(committed)
    })
}

/// Atomic-mode preflight admission for a 1024-operation request at both estimates. The
/// seed estimate rejects it; a measured per-operation estimate admits it. The assertion
/// pair pins the ADR 0060 claim that admission changes with the measured estimate while
/// the receipt-encoding cost (see `bench_atomic_insert_max_receipt`) does not.
#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_atomic_insert_1024_preflight_effect() -> canbench_rs::BenchResult {
    const MAX_VERTEX_OPERATIONS: usize = 1024;
    const MEASURED_PER_OP_ESTIMATE: u64 = 30_000_000;
    canbench_rs::bench_fn(|| {
        let seed = preflight_operation_count(
            black_box(MAX_VERTEX_OPERATIONS),
            GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION,
            MAX_DYNAMIC_UPDATE_INSTRUCTIONS,
        );
        assert!(
            seed.is_err(),
            "the 500M seed estimate must reject a 1024-operation request"
        );
        let measured = preflight_operation_count(
            black_box(MAX_VERTEX_OPERATIONS),
            MEASURED_PER_OP_ESTIMATE,
            MAX_DYNAMIC_UPDATE_INSTRUCTIONS,
        );
        assert!(
            measured.is_ok(),
            "a measured estimate below 35B/1024 must admit 1024 operations"
        );
        black_box((seed, measured))
    })
}
