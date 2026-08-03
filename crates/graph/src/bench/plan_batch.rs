//! Plan-batch tail-cost benchmarks for the GAP-2026-07-17-001 headroom acceptance on the
//! Graph `execute_plan_update_batch` dynamic path.
//!
//! The plan-batch cutoff reserves `GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM` (2B)
//! plus `BATCH_DRAIN_BUDGET_ESTIMATE` (500M) for the tail after the final operation: response
//! construction (measured here) and the derived-index drain (inter-canister; covered by the
//! PocketIC boundary test, not measurable by canbench). The adversarial single-operation bench
//! (`bench_graph_canonical_segment_insert_vertex_with_large_property` in `bench/mod.rs`) bounds
//! the per-operation estimate consumed by the cutoff predicate. These benches record the
//! acceptance evidence: the measured tail must fit the 2.5B reserve with margin.

use canbench_rs::bench;
use candid::Encode;
use gleaph_graph_kernel::plan_exec::{ExecutePlanBatchResult, ExecutePlanResult};
use std::hint::black_box;

fn batch_result(count: usize, hot_forward_per_result: usize) -> ExecutePlanBatchResult {
    let results = (0..count)
        .map(|index| {
            Ok(ExecutePlanResult {
                row_count: 1,
                rows_blob: None,
                hot_forward_vertices: (0..hot_forward_per_result)
                    .map(|offset| (index + offset) as u32)
                    .collect(),
            })
        })
        .collect();
    ExecutePlanBatchResult {
        results,
        next_index: None,
    }
}

/// Encode a fully-completed 256-operation plan-batch response — the tail the 2B + 500M
/// bookkeeping/drain reserves must cover after the final operation.
#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_plan_batch_result_payload_256() -> canbench_rs::BenchResult {
    let result = batch_result(256, 1);
    canbench_rs::bench_fn(|| {
        let encoded = Encode!(black_box(&result)).expect("encode plan batch response");
        black_box(encoded)
    })
}

/// Adversarial response: 256 results each carrying a large hot-forward hub list.
#[cfg(feature = "canbench")]
#[bench(raw)]
fn bench_plan_batch_result_payload_256_large_hubs() -> canbench_rs::BenchResult {
    let result = batch_result(256, 128);
    canbench_rs::bench_fn(|| {
        let encoded = Encode!(black_box(&result)).expect("encode plan batch response");
        black_box(encoded)
    })
}
