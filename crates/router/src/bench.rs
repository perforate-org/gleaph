//! Phase 8 stable-memory layout benchmarks (ADR 0007 §6).
//!
//! Run from `crates/router`: `canbench` (see `canbench.yml`).

use crate::facade::stable::memory;
use canbench_rs::bench;
use std::hint::black_box;

fn router_stable_reopen_round() {
    black_box(memory::init_controllers());
    black_box(memory::init_graphs());
    black_box(memory::init_shards());
    black_box(memory::init_shard_by_graph());
    black_box(memory::init_placements());
    black_box(memory::init_vertex_label_catalog());
    black_box(memory::init_edge_label_catalog());
    black_box(memory::init_property_catalog());
    black_box(memory::init_auth_state());
    black_box(memory::init_vertex_label_stats());
    black_box(memory::init_edge_label_stats());
    black_box(memory::init_vertex_label_live_by_shard());
    black_box(memory::init_edge_label_live_by_shard());
    black_box(memory::init_mutation_counter());
    black_box(memory::init_applied_label_telemetry());
    black_box(memory::init_mutation_by_client_key());
    black_box(memory::init_label_backfill_state());
    black_box(memory::init_property_backfill_state());
}

#[bench(raw)]
fn bench_layout_router_stable_reopen_touch() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("layout_router_reopen");
        router_stable_reopen_round();
    })
}
