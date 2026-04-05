//! Hot-path benchmarks for **property equality** reads served from stable-memory-backed indexes.
//!
//! Complements `docs/graph-pma-target-design.md` (Internet Computer: full property search stack).
//!
//! **Long `encoded_value`:** the `*_long_blob` benches stress B-tree key comparison on wide
//! byte payloads. For `wasm32-unknown-unknown`, enable SIMD128 with:
//! `RUSTFLAGS='-C target-feature=+simd128' cargo bench -p gleaph-graph-pma --bench property_search_hot_path`.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use gleaph_gql::Value;
use gleaph_graph_kernel::NodeId;
use gleaph_graph_pma::{GraphPma, GraphPmaVecMemory};

fn encoded_blob_value(entry_index: usize, len: usize) -> Value {
    let mut v = vec![0u8; len];
    if len > 0 {
        v[0] = (entry_index % 256) as u8;
    }
    if len > 1 {
        v[len - 1] = ((entry_index >> 8) % 256) as u8;
    }
    Value::Bytes(v)
}

fn setup_flushed_blob_prop_index(
    entry_count: usize,
    value_len: usize,
) -> (GraphPmaVecMemory, GraphPma) {
    let memory = GraphPmaVecMemory::default();
    let mut facade = GraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
    for i in 0..entry_count {
        let id = NodeId::try_from((i + 1) as u64).expect("NodeId");
        let val = encoded_blob_value(i, value_len);
        facade
            .set_node_property_value(id, "blob", &val)
            .expect("set blob");
    }
    facade
        .try_write_all_to_stable_memory(&memory)
        .expect("flush");
    (memory, facade)
}

fn setup_flushed_uid_index(entry_count: usize) -> (GraphPmaVecMemory, GraphPma) {
    let memory = GraphPmaVecMemory::default();
    let mut facade = GraphPma::bootstrap_empty(memory.clone()).expect("bootstrap");
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

fn bench_stable_memory_long_blob_eq_scan(c: &mut Criterion) {
    const BLOB_LEN: usize = 512;
    let (memory, facade) = setup_flushed_blob_prop_index(512, BLOB_LEN);
    let needle = encoded_blob_value(256, BLOB_LEN);

    c.bench_function("stable_mem_scan_node_blob_eq_512x512B", |b| {
        b.iter(|| {
            black_box(
                facade
                    .try_scan_node_ids_by_property_eq_from_stable_memory(
                        black_box(&memory),
                        "blob",
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
    bench_stable_memory_long_blob_eq_scan,
    bench_stable_memory_uid_eq_scaling,
);
criterion_main!(benches);
