//! Batch placement and ordered mutation benchmarks for ADR 0049.
//!
//! Planning benches keep setup and canonical writes outside their measured closure. Mutation
//! benches include the corresponding canonical adjacency, label-delta, and journal work so the
//! ordered partition comparison is not a setup-only proxy.

use crate::facade::EdgeHandle;
use crate::facade::mutation_executor::GraphMutationExecutor;
use crate::facade::{BatchEdgeInput, GraphStore};
use canbench_rs::bench;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{
    EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, EdgeLabelId, PropertyId,
};
use ic_stable_lara::{VertexId, labeled::LabeledOrientation};
use std::hint::black_box;

fn label_id(name: &str) -> EdgeLabelId {
    crate::test_labels::edge_label_id_for_name(name)
}

fn install_width_profile(label: EdgeLabelId, width: u16) {
    let encoding = match width {
        0 => EdgeInlinePropertyEncoding::RawU8,
        1 => EdgeInlinePropertyEncoding::RawU8,
        2 => EdgeInlinePropertyEncoding::RawU16,
        4 => EdgeInlinePropertyEncoding::RawU32,
        8 => EdgeInlinePropertyEncoding::RawU64,
        _ => EdgeInlinePropertyEncoding::RawBytes,
    };
    crate::test_labels::install_test_edge_inline_property_profile(
        label,
        EdgeInlinePropertyProfile {
            byte_width: width,
            encoding,
        },
    );
}

fn make_vertices(store: &GraphStore, count: u32) -> Vec<VertexId> {
    (0..count)
        .map(|_| store.insert_vertex().expect("vertex"))
        .collect()
}

fn setup_directed_edges(
    count: usize,
    width: u16,
) -> (GraphStore, EdgeLabelId, Vec<BatchEdgeInput>) {
    let store = GraphStore::new();
    let label = label_id(if width == 0 {
        "BenchCleanSlabDirectedW0"
    } else {
        "BenchCleanSlabDirectedW8"
    });
    install_width_profile(label, width);
    let inline_property_bytes = if width == 0 {
        vec![]
    } else {
        vec![0u8; width as usize]
    };
    let mut sources = Vec::with_capacity(count);
    let mut targets = Vec::with_capacity(count);
    for _ in 0..count {
        sources.push(store.insert_vertex().expect("src"));
        targets.push(store.insert_vertex().expect("dst"));
    }
    for (i, &src) in sources.iter().enumerate() {
        store.prepare_clean_slab_dir_buckets(src, targets[i], label, width);
    }
    let input: Vec<BatchEdgeInput> = sources
        .iter()
        .zip(&targets)
        .map(|(&s, &t)| BatchEdgeInput {
            source_vertex_id: s,
            target_vertex_id: t,
            catalog_label: Some(label),
            directed: true,
            inline_property_bytes: inline_property_bytes.clone(),
            initial_edge_properties: Vec::new(),
        })
        .collect();
    (store, label, input)
}

fn setup_128_directed_edges(width: u16) -> (GraphStore, EdgeLabelId, Vec<BatchEdgeInput>) {
    setup_directed_edges(128, width)
}

fn setup_fan_out_directed_edges(
    count: usize,
    width: u16,
) -> (GraphStore, EdgeLabelId, Vec<BatchEdgeInput>) {
    let store = GraphStore::new();
    let label = label_id(if width == 0 {
        "BenchFanOutDirectedW0"
    } else {
        "BenchFanOutDirectedW8"
    });
    install_width_profile(label, width);
    let source = store.insert_vertex().expect("source");
    let targets = make_vertices(
        &store,
        count.try_into().expect("fan-out benchmark count fits u32"),
    );
    let inline_property_bytes = if width == 0 {
        Vec::new()
    } else {
        vec![0u8; width as usize]
    };
    for &target in &targets {
        store.prepare_clean_slab_dir_buckets(source, target, label, width);
        store.prepare_clean_slab_dir_buckets(target, source, label, width);
    }
    let input = targets
        .into_iter()
        .map(|target| BatchEdgeInput {
            source_vertex_id: source,
            target_vertex_id: target,
            catalog_label: Some(label),
            directed: true,
            inline_property_bytes: inline_property_bytes.clone(),
            initial_edge_properties: Vec::new(),
        })
        .collect();
    (store, label, input)
}

fn setup_two_parallel_directed_edges_per_bucket(
    bucket_count: usize,
) -> (GraphStore, EdgeLabelId, Vec<BatchEdgeInput>) {
    let store = GraphStore::new();
    let label = label_id("BenchParallelDirectedW0");
    install_width_profile(label, 0);
    let sources = make_vertices(
        &store,
        bucket_count
            .try_into()
            .expect("parallel benchmark bucket count fits u32"),
    );
    let targets = make_vertices(
        &store,
        bucket_count
            .try_into()
            .expect("parallel benchmark bucket count fits u32"),
    );
    let mut input = Vec::with_capacity(bucket_count * 2);
    for (&source, &target) in sources.iter().zip(&targets) {
        store.prepare_clean_slab_dir_buckets(source, target, label, 0);
        store.prepare_clean_slab_dir_buckets(target, source, label, 0);
        for _ in 0..2 {
            input.push(BatchEdgeInput {
                source_vertex_id: source,
                target_vertex_id: target,
                catalog_label: Some(label),
                directed: true,
                inline_property_bytes: Vec::new(),
                initial_edge_properties: Vec::new(),
            });
        }
    }
    (store, label, input)
}

#[bench(raw)]
fn bench_scalar_directed_128_width_0() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_128_directed_edges(0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("scalar_directed_128_w0");
        for edge in &input {
            store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
        }
    })
}

#[bench(raw)]
fn bench_scalar_directed_128_width_8() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_128_directed_edges(8);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("scalar_directed_128_w8");
        for edge in &input {
            store
                .insert_directed_edge_with_inline_property_bytes(
                    edge.source_vertex_id,
                    edge.target_vertex_id,
                    Some(label),
                    &edge.inline_property_bytes,
                )
                .expect("scalar insert");
        }
    })
}

#[bench(raw)]
fn bench_scalar_directed_128_sidecar_4_bulk() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_128_directed_edges(0);
    let properties = [
        (PropertyId::from_raw(9_901), Value::Int64(1)),
        (PropertyId::from_raw(9_902), Value::Int64(2)),
        (PropertyId::from_raw(9_903), Value::Int64(3)),
        (PropertyId::from_raw(9_904), Value::Int64(4)),
    ];
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("scalar_directed_128_sidecar_4_bulk");
        for edge in &input {
            GraphMutationExecutor::insert_directed_edge_with(
                &store,
                edge.source_vertex_id,
                edge.target_vertex_id,
                Some(label),
                properties.iter().cloned(),
                0,
            )
            .expect("scalar sidecar insert");
        }
    })
}

#[bench(raw)]
fn bench_scalar_directed_128_sidecar_4_per_property() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_128_directed_edges(0);
    let properties = [
        (PropertyId::from_raw(9_901), Value::Int64(1)),
        (PropertyId::from_raw(9_902), Value::Int64(2)),
        (PropertyId::from_raw(9_903), Value::Int64(3)),
        (PropertyId::from_raw(9_904), Value::Int64(4)),
    ];
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("scalar_directed_128_sidecar_4_per_property");
        for edge in &input {
            let handle = store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
            for (property_id, value) in properties.iter().cloned() {
                store
                    .set_edge_property(
                        handle.occurrence(LabeledOrientation::Forward),
                        property_id,
                        value,
                    )
                    .expect("scalar sidecar property");
            }
        }
    })
}

#[bench(raw)]
fn bench_scalar_directed_1024_width_0() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_directed_edges(1024, 0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("scalar_directed_1024_w0");
        for edge in &input {
            store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
        }
    })
}

#[bench(raw)]
fn bench_scalar_directed_1024_width_8() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_directed_edges(1024, 8);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("scalar_directed_1024_w8");
        for edge in &input {
            store
                .insert_directed_edge_with_inline_property_bytes(
                    edge.source_vertex_id,
                    edge.target_vertex_id,
                    Some(label),
                    &edge.inline_property_bytes,
                )
                .expect("scalar insert");
        }
    })
}

#[bench(raw)]
fn bench_scalar_fan_out_128_width_0() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_fan_out_directed_edges(128, 0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("scalar_fan_out_128_w0");
        for edge in &input {
            store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
        }
    })
}

#[bench(raw)]
fn bench_scalar_fan_out_1024_width_0() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_fan_out_directed_edges(1024, 0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("scalar_fan_out_1024_w0");
        for edge in &input {
            store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
        }
    })
}

#[bench(raw)]
fn bench_scalar_two_parallel_per_bucket_128_width_0() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_two_parallel_directed_edges_per_bucket(64);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("scalar_two_parallel_per_bucket_128_w0");
        for edge in &input {
            store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
        }
    })
}

fn setup_mate_lookup_fixture() -> (GraphStore, crate::facade::EdgeHandle, VertexId, VertexId) {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label = label_id("BenchScanOnlyMate");
    let handle = store
        .insert_directed_edge(source, target, Some(label))
        .expect("edge");
    (store, handle, source, target)
}

#[bench(raw)]
fn bench_edge_mate_counterpart_lookup() -> canbench_rs::BenchResult {
    let (store, handle, _source, _target) = setup_mate_lookup_fixture();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("edge_mate_counterpart_lookup");
        black_box(store.canonical_edge_handle(handle));
    })
}

#[bench(raw)]
fn bench_edge_mate_scan_only_rank_lookup() -> canbench_rs::BenchResult {
    let (store, handle, _source, _target) = setup_mate_lookup_fixture();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("edge_mate_scan_only_rank_lookup");
        black_box(
            store
                .scan_only_canonical_edge_handle(handle, LabeledOrientation::Forward)
                .expect("scan-only canonical handle"),
        );
    })
}

#[bench(raw)]
fn bench_edge_mate_counterpart_scan() -> canbench_rs::BenchResult {
    let (store, handle, _source, _target) = setup_mate_lookup_fixture();
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("edge_mate_counterpart_scan");
        black_box(
            store
                .counterpart_edge_occurrence(handle.occurrence(LabeledOrientation::Forward))
                .expect("counterpart scan"),
        );
    })
}

/// Fan-in: every edge lands in the hub's reverse bucket, so one bucket takes `count` rows.
///
/// This is the shape a batch would amortize over - one bucket, many rows - and the shape the
/// clean-slab batch declines today, because a bucket created inside the batch gets a one-slot
/// window. Comparing the scalar path against the same inserts under a shadow window is therefore
/// the comparison that decides whether the shadow can replace the batch's geometry.
fn setup_fan_in_directed_edges(
    count: usize,
    width: u16,
) -> (GraphStore, EdgeLabelId, Vec<BatchEdgeInput>) {
    let store = GraphStore::new();
    let label = label_id(if width == 0 {
        "BenchFanInDirectedW0"
    } else {
        "BenchFanInDirectedW8"
    });
    install_width_profile(label, width);
    let inline_property_bytes = if width == 0 {
        vec![]
    } else {
        vec![0u8; width as usize]
    };
    let hub = store.insert_vertex().expect("hub");
    let sources = make_vertices(
        &store,
        count.try_into().expect("fan-in benchmark count fits u32"),
    );
    for &source in &sources {
        store.prepare_clean_slab_dir_buckets(source, hub, label, width);
    }
    let input = sources
        .into_iter()
        .map(|source| BatchEdgeInput {
            source_vertex_id: source,
            target_vertex_id: hub,
            catalog_label: Some(label),
            directed: true,
            inline_property_bytes: inline_property_bytes.clone(),
            initial_edge_properties: Vec::new(),
        })
        .collect();
    (store, label, input)
}

/// The reference path: one scalar insert per edge, no overlay.
#[bench(raw)]
fn bench_scalar_fan_in_128_width_0() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_fan_in_directed_edges(128, 0);
    canbench_rs::bench_fn(|| {
        for edge in &input {
            store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
        }
        black_box(input.len());
    })
}

/// Fan-in at a smaller width, to separate "append into one bucket" from "grow that bucket".
#[bench(raw)]
fn bench_scalar_fan_in_8_width_0() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_fan_in_directed_edges(8, 0);
    canbench_rs::bench_fn(|| {
        for edge in &input {
            store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
        }
        black_box(input.len());
    })
}

/// Fan-in at a middle width, to see whether the per-append cost grows with the bucket.
#[bench(raw)]
fn bench_scalar_fan_in_32_width_0() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_fan_in_directed_edges(32, 0);
    canbench_rs::bench_fn(|| {
        for edge in &input {
            store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
        }
        black_box(input.len());
    })
}

/// Fan-out with the same message shape: 128 buckets, one edge each.
#[bench(raw)]
fn bench_scalar_directed_128_width_0_message_drain() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_directed_edges(128, 0);
    canbench_rs::bench_fn(|| {
        for edge in &input {
            store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
        }
        store
            .drain_maintenance_at_message_end()
            .expect("boundary drain");
        black_box(input.len());
    })
}

/// The message shape the write path now uses: insert every edge, then drain maintenance once.
///
/// Before the boundary drain, each insert drained the queue itself and a 128-edge fan-in cost
/// 27.90 M instructions, 85% of it in that drain.
#[bench(raw)]
fn bench_scalar_fan_in_128_width_0_message_drain() -> canbench_rs::BenchResult {
    let (store, label, input) = setup_fan_in_directed_edges(128, 0);
    canbench_rs::bench_fn(|| {
        for edge in &input {
            store
                .insert_directed_edge(edge.source_vertex_id, edge.target_vertex_id, Some(label))
                .expect("scalar insert");
        }
        let drain = canbench_rs::bench_scope("message_boundary_drain");
        store
            .drain_maintenance_at_message_end()
            .expect("boundary drain");
        drop(drain);
        black_box(input.len());
    })
}

/// One bucket's worth of edges from a single source, each carrying a fixed-width inline value.
///
/// Deleting them one at a time from the front is the shape the values-span cost was measured on. It is **not** a suffix
/// shift any more: ADR 0099 makes a delete write no value (the row is tombstoned where it lives and a reader never asks a
/// tombstoned slot for one), so the planner that copied the trailing suffix - and the quadratic cost it caused
/// (GAP-LARA-INLINE-DELETE-COST: 454 K instructions per edge at width 8, 128 rows) - is gone. The fixture is kept as the
/// regression shape for the per-delete cost, which should now be flat in the bucket size.
fn setup_single_bucket_inline_deletes(count: u32, width: u16) -> (GraphStore, Vec<EdgeHandle>) {
    let store = GraphStore::new();
    let label = label_id("BenchDeleteInline");
    install_width_profile(label, width);
    let source = store.insert_vertex().expect("source");
    let targets = make_vertices(&store, count);
    let bytes = vec![0u8; usize::from(width)];
    let handles = targets
        .into_iter()
        .map(|target| {
            store
                .insert_directed_edge_with_inline_property_bytes(
                    source,
                    target,
                    Some(label),
                    &bytes,
                )
                .expect("insert")
        })
        .collect();
    (store, handles)
}

fn delete_leading_edges(store: &GraphStore, handles: &[EdgeHandle]) {
    for handle in handles {
        store.delete_edge_by_handle(*handle).expect("delete");
    }
}

#[bench(raw)]
fn bench_delete_w0_k128() -> canbench_rs::BenchResult {
    let (store, handles) = setup_single_bucket_inline_deletes(128, 0);
    canbench_rs::bench_fn(|| delete_leading_edges(&store, &handles))
}

#[bench(raw)]
fn bench_delete_w8_k8() -> canbench_rs::BenchResult {
    let (store, handles) = setup_single_bucket_inline_deletes(8, 8);
    canbench_rs::bench_fn(|| delete_leading_edges(&store, &handles))
}

#[bench(raw)]
fn bench_delete_w8_k32() -> canbench_rs::BenchResult {
    let (store, handles) = setup_single_bucket_inline_deletes(32, 8);
    canbench_rs::bench_fn(|| delete_leading_edges(&store, &handles))
}

#[bench(raw)]
fn bench_delete_w8_k128() -> canbench_rs::BenchResult {
    let (store, handles) = setup_single_bucket_inline_deletes(128, 8);
    canbench_rs::bench_fn(|| delete_leading_edges(&store, &handles))
}
