//! Batch placement and ordered mutation benchmarks for ADR 0049.
//!
//! Planning benches keep setup and canonical writes outside their measured closure. Mutation
//! benches include the corresponding canonical adjacency, label-delta, and journal work so the
//! ordered partition comparison is not a setup-only proxy.

use crate::facade::mutation_executor::GraphMutationExecutor;
use crate::facade::{BatchEdgeInput, GraphStore};
use canbench_rs::bench;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{
    EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile, EdgeLabelId, PropertyId,
};
use gleaph_graph_kernel::plan_exec::GraphMutationRequestIdentityV1;
use ic_stable_lara::{VertexId, labeled::LabeledOrientation};
use std::collections::BTreeSet;
use std::hint::black_box;

const LABEL_NAMES: [&str; 4] = [
    "BenchBatchDirected",
    "BenchBatchUndirected",
    "BenchBatchSelfLoop",
    "BenchBatchFanOut",
];

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

fn build_directed_input(
    vertices: &[VertexId],
    label: EdgeLabelId,
    width: u16,
    count: usize,
) -> Vec<BatchEdgeInput> {
    let value = if width == 0 {
        Vec::new()
    } else {
        vec![0u8; width as usize]
    };
    let n = vertices.len();
    let max_unique = n.saturating_mul(n.saturating_sub(1));
    let count = count.min(max_unique);
    (0..count)
        .map(|i| {
            let a = i / n.saturating_sub(1);
            let b = i % n.saturating_sub(1);
            let target = if b >= a { b + 1 } else { b };
            BatchEdgeInput {
                source_vertex_id: vertices[a],
                target_vertex_id: vertices[target],
                catalog_label: Some(label),
                directed: true,
                inline_property_bytes: value.clone(),
                initial_edge_properties: Vec::new(),
            }
        })
        .collect()
}

fn build_undirected_input(
    vertices: &[VertexId],
    label: EdgeLabelId,
    width: u16,
    count: usize,
) -> Vec<BatchEdgeInput> {
    let value = if width == 0 {
        Vec::new()
    } else {
        vec![0u8; width as usize]
    };
    let n = vertices.len();
    let max_unique = n.saturating_mul(n.saturating_sub(1)) / 2;
    let count = count.min(max_unique);
    (0..count)
        .map(|mut i| {
            // Map linear i to unique unordered pair (a, b) with a < b.
            let mut a = 0usize;
            while i >= n - a - 1 {
                i -= n - a - 1;
                a += 1;
            }
            let b = a + 1 + i;
            BatchEdgeInput {
                source_vertex_id: vertices[a],
                target_vertex_id: vertices[b],
                catalog_label: Some(label),
                directed: false,
                inline_property_bytes: value.clone(),
                initial_edge_properties: Vec::new(),
            }
        })
        .collect()
}

fn build_self_loop_input(
    vertices: &[VertexId],
    label: EdgeLabelId,
    width: u16,
    count: usize,
) -> Vec<BatchEdgeInput> {
    let value = if width == 0 {
        Vec::new()
    } else {
        vec![0u8; width as usize]
    };
    let count = count.min(vertices.len());
    (0..count)
        .map(|i| BatchEdgeInput {
            source_vertex_id: vertices[i],
            target_vertex_id: vertices[i],
            catalog_label: Some(label),
            directed: false,
            inline_property_bytes: value.clone(),
            initial_edge_properties: Vec::new(),
        })
        .collect()
}

fn build_fan_out_input(
    vertices: &[VertexId],
    label: EdgeLabelId,
    width: u16,
    count: usize,
) -> Vec<BatchEdgeInput> {
    let value = if width == 0 {
        Vec::new()
    } else {
        vec![0u8; width as usize]
    };
    let hub = vertices[0];
    let count = count.min(vertices.len().saturating_sub(1));
    (0..count)
        .map(|i| BatchEdgeInput {
            source_vertex_id: hub,
            target_vertex_id: vertices[i + 1],
            catalog_label: Some(label),
            directed: true,
            inline_property_bytes: value.clone(),
            initial_edge_properties: Vec::new(),
        })
        .collect()
}

fn run_plan(store: &GraphStore, input: &[BatchEdgeInput]) {
    black_box(store.plan_batch_edge_insertion(input).expect("plan"));
}

#[bench(raw)]
fn bench_batch_plan_directed_128_width_0() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let label = label_id(LABEL_NAMES[0]);
        install_width_profile(label, 0);
        let vertices = make_vertices(&store, 32);
        let input = build_directed_input(&vertices, label, 0, 128);
        let _scope = canbench_rs::bench_scope("plan_directed_128_w0");
        run_plan(&store, &input);
    })
}

#[bench(raw)]
fn bench_batch_plan_directed_128_width_8_existing() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let label = label_id(LABEL_NAMES[0]);
        install_width_profile(label, 8);
        let vertices = make_vertices(&store, 32);
        // Seed a few edges so the planner must read existing bucket occupancy.
        for i in 0..8 {
            store
                .insert_directed_edge_with_inline_property_bytes(
                    vertices[0],
                    vertices[1 + (i % 31)],
                    Some(label),
                    &[0u8; 8],
                )
                .expect("seed edge");
        }
        let input = build_directed_input(&vertices, label, 8, 128);
        let _scope = canbench_rs::bench_scope("plan_directed_128_w8_existing");
        run_plan(&store, &input);
    })
}

#[bench(raw)]
fn bench_batch_plan_undirected_64_width_1() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let label = label_id(LABEL_NAMES[1]);
        install_width_profile(label, 1);
        let vertices = make_vertices(&store, 32);
        let input = build_undirected_input(&vertices, label, 1, 64);
        let _scope = canbench_rs::bench_scope("plan_undirected_64_w1");
        run_plan(&store, &input);
    })
}

#[bench(raw)]
fn bench_batch_plan_self_loop_32_width_4() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let label = label_id(LABEL_NAMES[2]);
        install_width_profile(label, 4);
        let vertices = make_vertices(&store, 8);
        let input = build_self_loop_input(&vertices, label, 4, 32);
        let _scope = canbench_rs::bench_scope("plan_self_loop_32_w4");
        run_plan(&store, &input);
    })
}

#[bench(raw)]
fn bench_batch_plan_fan_out_256_width_0() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let label = label_id(LABEL_NAMES[3]);
        install_width_profile(label, 0);
        let vertices = make_vertices(&store, 64);
        let input = build_fan_out_input(&vertices, label, 0, 256);
        let _scope = canbench_rs::bench_scope("plan_fan_out_256_w0");
        run_plan(&store, &input);
    })
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

fn setup_mixed_ordered_partition_fixture() -> (GraphStore, Vec<BatchEdgeInput>, BTreeSet<u32>) {
    let store = GraphStore::new();
    let label = label_id("BenchMixedOrderedPartitionW0");
    install_width_profile(label, 0);
    let multi_bucket_count = 64usize;
    let singleton_count = 128usize;
    let vertices = make_vertices(
        &store,
        ((multi_bucket_count + singleton_count) * 2)
            .try_into()
            .expect("mixed benchmark vertex count fits u32"),
    );
    let mut input = Vec::with_capacity(multi_bucket_count * 2 + singleton_count);
    let mut batch_ordinals = BTreeSet::new();
    for pair in 0..multi_bucket_count {
        let source = vertices[pair * 2];
        let target = vertices[pair * 2 + 1];
        store.prepare_clean_slab_dir_buckets(source, target, label, 0);
        store.prepare_clean_slab_dir_buckets(target, source, label, 0);
        for _ in 0..2 {
            batch_ordinals.insert(input.len() as u32);
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
    let singleton_start = multi_bucket_count;
    for index in 0..singleton_count {
        let source = vertices[(singleton_start + index) * 2];
        let target = vertices[(singleton_start + index) * 2 + 1];
        store.prepare_clean_slab_dir_buckets(source, target, label, 0);
        store.prepare_clean_slab_dir_buckets(target, source, label, 0);
        input.push(BatchEdgeInput {
            source_vertex_id: source,
            target_vertex_id: target,
            catalog_label: Some(label),
            directed: true,
            inline_property_bytes: Vec::new(),
            initial_edge_properties: Vec::new(),
        });
    }
    (store, input, batch_ordinals)
}

#[bench(raw)]
fn bench_clean_slab_directed_128_width_0() -> canbench_rs::BenchResult {
    let (store, _label, input) = setup_128_directed_edges(0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("clean_slab_directed_128_w0");
        let result = store
            .try_insert_batch_edges_clean_slab(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
    })
}

#[bench(raw)]
fn bench_clean_slab_directed_128_width_0_with_locations() -> canbench_rs::BenchResult {
    let (store, _label, input) = setup_128_directed_edges(0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("clean_slab_directed_128_w0_with_locations");
        let result = store
            .try_insert_batch_edges_clean_slab_with_locations(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
    })
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
fn bench_clean_slab_directed_128_width_8() -> canbench_rs::BenchResult {
    let (store, _label, input) = setup_128_directed_edges(8);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("clean_slab_directed_128_w8");
        let result = store
            .try_insert_batch_edges_clean_slab(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
    })
}

#[bench(raw)]
fn bench_clean_slab_directed_128_width_8_with_locations() -> canbench_rs::BenchResult {
    let (store, _label, input) = setup_128_directed_edges(8);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("clean_slab_directed_128_w8_with_locations");
        let result = store
            .try_insert_batch_edges_clean_slab_with_locations(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
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
fn bench_clean_slab_directed_1024_width_0() -> canbench_rs::BenchResult {
    let (store, _label, input) = setup_directed_edges(1024, 0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("clean_slab_directed_1024_w0");
        let result = store
            .try_insert_batch_edges_clean_slab(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
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
fn bench_clean_slab_directed_1024_width_8() -> canbench_rs::BenchResult {
    let (store, _label, input) = setup_directed_edges(1024, 8);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("clean_slab_directed_1024_w8");
        let result = store
            .try_insert_batch_edges_clean_slab(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
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
fn bench_clean_slab_fan_out_128_width_0() -> canbench_rs::BenchResult {
    let (store, _label, input) = setup_fan_out_directed_edges(128, 0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("clean_slab_fan_out_128_w0");
        let result = store
            .try_insert_batch_edges_clean_slab(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
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
fn bench_clean_slab_fan_out_1024_width_0() -> canbench_rs::BenchResult {
    let (store, _label, input) = setup_fan_out_directed_edges(1024, 0);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("clean_slab_fan_out_1024_w0");
        let result = store
            .try_insert_batch_edges_clean_slab(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
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
fn bench_clean_slab_two_parallel_per_bucket_128_width_0() -> canbench_rs::BenchResult {
    let (store, _label, input) = setup_two_parallel_directed_edges_per_bucket(64);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("clean_slab_two_parallel_per_bucket_128_w0");
        let result = store
            .try_insert_batch_edges_clean_slab(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
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

#[bench(raw)]
fn bench_ordered_partitioned_mixed_256_width_0() -> canbench_rs::BenchResult {
    let (store, input, batch_ordinals) = setup_mixed_ordered_partition_fixture();
    let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
        canonical_encoding_version: 1,
        graph_request_fingerprint: [0x53; 32],
        logical_item_count: input.len() as u32,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("ordered_partitioned_mixed_256_w0");
        black_box(
            store
                .execute_ordered_edge_batch_partitioned(
                    5_300_001,
                    identity.clone(),
                    &input,
                    &batch_ordinals,
                )
                .expect("partitioned ordered batch"),
        );
    })
}

#[bench(raw)]
fn bench_ordered_partitioned_mixed_256_width_0_with_planner() -> canbench_rs::BenchResult {
    let (store, input, _batch_ordinals) = setup_mixed_ordered_partition_fixture();
    let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
        canonical_encoding_version: 1,
        graph_request_fingerprint: [0x56; 32],
        logical_item_count: input.len() as u32,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("ordered_partitioned_mixed_256_w0_with_planner");
        let classification = store
            .classify_batch_edge_insertion(&input)
            .expect("ordered classification");
        let summary = store
            .plan_batch_edge_insertion_with_classification(&input, &classification)
            .expect("ordered plan");
        black_box(
            store
                .execute_ordered_edge_batch_partitioned_with_intents(
                    5_300_004,
                    identity.clone(),
                    &input,
                    summary.logical_ordinals_requiring_batch(),
                    Some(&classification.intents),
                )
                .expect("planned partitioned ordered batch"),
        );
    })
}

#[bench(raw)]
fn bench_ordered_all_batch_mixed_256_width_0() -> canbench_rs::BenchResult {
    let (store, input, _batch_ordinals) = setup_mixed_ordered_partition_fixture();
    let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
        canonical_encoding_version: 1,
        graph_request_fingerprint: [0x54; 32],
        logical_item_count: input.len() as u32,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("ordered_all_batch_mixed_256_w0");
        black_box(
            store
                .execute_ordered_edge_batch_clean_slab(5_300_002, identity.clone(), &input)
                .expect("all-batch ordered batch"),
        );
    })
}

#[bench(raw)]
fn bench_ordered_all_batch_mixed_256_width_0_with_planner() -> canbench_rs::BenchResult {
    let (store, input, _batch_ordinals) = setup_mixed_ordered_partition_fixture();
    let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
        canonical_encoding_version: 1,
        graph_request_fingerprint: [0x57; 32],
        logical_item_count: input.len() as u32,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("ordered_all_batch_mixed_256_w0_with_planner");
        store
            .plan_batch_edge_insertion(&input)
            .expect("ordered plan");
        black_box(
            store
                .execute_ordered_edge_batch_clean_slab(5_300_005, identity.clone(), &input)
                .expect("planned all-batch ordered batch"),
        );
    })
}

#[bench(raw)]
fn bench_ordered_all_batch_mixed_256_width_0_with_classifier() -> canbench_rs::BenchResult {
    let (store, mixed_input, _batch_ordinals) = setup_mixed_ordered_partition_fixture();
    let input = mixed_input[..128].to_vec();
    let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
        canonical_encoding_version: 1,
        graph_request_fingerprint: [0x58; 32],
        logical_item_count: input.len() as u32,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("ordered_all_batch_mixed_256_w0_with_classifier");
        let classification = store
            .classify_batch_edge_insertion(&input)
            .expect("ordered classification");
        assert_eq!(
            classification.logical_ordinals_with_multi_runs.len(),
            input.len()
        );
        black_box(
            store
                .execute_ordered_edge_batch_clean_slab_with_intents(
                    5_300_006,
                    identity.clone(),
                    &input,
                    &classification.intents,
                )
                .expect("classified all-batch ordered batch"),
        );
    })
}

#[bench(raw)]
fn bench_ordered_all_scalar_mixed_256_width_0() -> canbench_rs::BenchResult {
    let (store, input, _batch_ordinals) = setup_mixed_ordered_partition_fixture();
    let identity = GraphMutationRequestIdentityV1::OrderedEdgeBatch {
        canonical_encoding_version: 1,
        graph_request_fingerprint: [0x55; 32],
        logical_item_count: input.len() as u32,
    };
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("ordered_all_scalar_mixed_256_w0");
        black_box(store.execute_ordered_edge_batch_scalar_fallback(
            5_300_003,
            identity.clone(),
            &input,
        ));
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
