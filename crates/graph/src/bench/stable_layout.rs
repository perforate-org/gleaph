//! Phase 8 stable-memory layout benchmarks (ADR 0007 §6).

use crate::facade::bench_stable_layout::{
    edge_profile_label, install_edge_profile_fixtures, read_weight_via_store,
};
use crate::facade::bench_stable_reopen_touch;
use canbench_rs::bench;

#[bench(raw)]
fn bench_layout_graph_stable_reopen_touch() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("layout_graph_reopen");
        bench_stable_reopen_touch();
    })
}

#[bench(raw)]
fn bench_layout_edge_weight_profile_read() -> canbench_rs::BenchResult {
    install_edge_profile_fixtures();
    let label = edge_profile_label();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("layout_edge_profile_read");
        read_weight_via_store(label);
    })
}
