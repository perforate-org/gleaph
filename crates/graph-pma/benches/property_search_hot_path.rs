//! Hot-path benchmarks for **property equality** reads served from stable-memory-backed indexes.
//!
//! Complements `docs/graph-pma-target-design.md` (Internet Computer: full property search stack).

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use gleaph_gql::Value;
use gleaph_graph_kernel::NodeId;
use gleaph_graph_pma::{RewriteGraphPma, RewriteVecMemory};

fn setup_flushed_uid_index(entry_count: usize) -> (RewriteVecMemory, RewriteGraphPma) {
    let memory = RewriteVecMemory::default();
    let mut facade = RewriteGraphPma::bootstrap_empty(&memory).expect("bootstrap");
    for i in 0..entry_count {
        let id = NodeId::try_from((i + 1) as u64).expect("NodeId");
        facade
            .set_node_property_value(id, "uid", &Value::Text(format!("u{i}")))
            .expect("set uid");
    }
    facade
        .try_write_all_to_stable_memory(&memory)
        .expect("flush");
    (memory, facade)
}

fn bench_stable_memory_uid_eq_scan(c: &mut Criterion) {
    let (memory, facade) = setup_flushed_uid_index(512);
    let needle = Value::Text("u256".into());

    c.bench_function("stable_mem_scan_node_uid_eq_512", |b| {
        b.iter(|| {
            black_box(
                facade
                    .try_scan_node_ids_by_property_eq_from_stable_memory(
                        black_box(&memory),
                        "uid",
                        black_box(&needle),
                    )
                    .expect("scan"),
            )
        });
    });
}

fn bench_stable_memory_uid_eq_scaling(c: &mut Criterion) {
    let mut group = c.benchmark_group("stable_mem_scan_node_uid_eq_scaling");
    for n in [64usize, 256, 1024] {
        let (memory, facade) = setup_flushed_uid_index(n);
        let needle = Value::Text(format!("u{}", n / 2));
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                black_box(
                    facade
                        .try_scan_node_ids_by_property_eq_from_stable_memory(
                            black_box(&memory),
                            "uid",
                            black_box(&needle),
                        )
                        .expect("scan"),
                )
            });
        });
    }
    group.finish();
}

criterion_group!(
    benches,
    bench_stable_memory_uid_eq_scan,
    bench_stable_memory_uid_eq_scaling,
);
criterion_main!(benches);
