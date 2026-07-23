//! Read-only batch placement planning probes for ADR 0045.
//!
//! These benchmarks measure only intent expansion and placement-summary
//! construction. No canonical write occurs, so `stable_memory_increase` should
//! remain zero. Setup (vertex creation, edge seeding, and input vector
//! construction) is outside the measured closure.

use crate::facade::{BatchEdgeInput, GraphStore};
use canbench_rs::bench;
use gleaph_graph_kernel::entry::{EdgeInlineValueEncoding, EdgeInlineValueProfile, EdgeLabelId};
use ic_stable_lara::VertexId;
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
        0 => EdgeInlineValueEncoding::RawU8,
        1 => EdgeInlineValueEncoding::RawU8,
        2 => EdgeInlineValueEncoding::RawU16,
        4 => EdgeInlineValueEncoding::RawU32,
        8 => EdgeInlineValueEncoding::RawU64,
        _ => EdgeInlineValueEncoding::RawBytes,
    };
    crate::test_labels::install_test_edge_inline_value_profile(
        label,
        EdgeInlineValueProfile {
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
                inline_value_bytes: value.clone(),
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
                inline_value_bytes: value.clone(),
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
            inline_value_bytes: value.clone(),
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
            inline_value_bytes: value.clone(),
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
                .insert_directed_edge_with_inline_value_bytes(
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

#[bench(raw)]
fn bench_clean_slab_directed_128_width_0() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let label = label_id("BenchCleanSlabDirectedW0");
        install_width_profile(label, 0);
        let mut sources = Vec::with_capacity(128);
        let mut targets = Vec::with_capacity(128);
        for _ in 0..128 {
            sources.push(store.insert_vertex().expect("src"));
            targets.push(store.insert_vertex().expect("dst"));
        }
        for (i, &src) in sources.iter().enumerate() {
            let dst = targets[i];
            store.prepare_clean_slab_dir_buckets(src, dst, label, 0);
        }
        let input: Vec<BatchEdgeInput> = sources
            .iter()
            .zip(&targets)
            .map(|(&s, &t)| BatchEdgeInput {
                source_vertex_id: s,
                target_vertex_id: t,
                catalog_label: Some(label),
                directed: true,
                inline_value_bytes: vec![],
            })
            .collect();
        let _scope = canbench_rs::bench_scope("clean_slab_directed_128_w0");
        let result = store
            .try_insert_batch_edges_clean_slab(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
    })
}

#[bench(raw)]
fn bench_scalar_directed_128_width_0() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let label = label_id("BenchScalarDirectedW0");
        install_width_profile(label, 0);
        let mut sources = Vec::with_capacity(128);
        let mut targets = Vec::with_capacity(128);
        for _ in 0..128 {
            sources.push(store.insert_vertex().expect("src"));
            targets.push(store.insert_vertex().expect("dst"));
        }
        let input: Vec<BatchEdgeInput> = sources
            .iter()
            .zip(&targets)
            .map(|(&s, &t)| BatchEdgeInput {
                source_vertex_id: s,
                target_vertex_id: t,
                catalog_label: Some(label),
                directed: true,
                inline_value_bytes: vec![],
            })
            .collect();
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
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let label = label_id("BenchCleanSlabDirectedW8");
        install_width_profile(label, 8);
        let payload = vec![0u8; 8];
        let mut sources = Vec::with_capacity(128);
        let mut targets = Vec::with_capacity(128);
        for _ in 0..128 {
            sources.push(store.insert_vertex().expect("src"));
            targets.push(store.insert_vertex().expect("dst"));
        }
        for (i, &src) in sources.iter().enumerate() {
            let dst = targets[i];
            store.prepare_clean_slab_dir_buckets(src, dst, label, 8);
        }
        let input: Vec<BatchEdgeInput> = sources
            .iter()
            .zip(&targets)
            .map(|(&s, &t)| BatchEdgeInput {
                source_vertex_id: s,
                target_vertex_id: t,
                catalog_label: Some(label),
                directed: true,
                inline_value_bytes: payload.clone(),
            })
            .collect();
        let _scope = canbench_rs::bench_scope("clean_slab_directed_128_w8");
        let result = store
            .try_insert_batch_edges_clean_slab(&input)
            .expect("batch");
        assert!(result.total_edge_slots().is_some());
    })
}

#[bench(raw)]
fn bench_scalar_directed_128_width_8() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let label = label_id("BenchScalarDirectedW8");
        install_width_profile(label, 8);
        let payload = vec![0u8; 8];
        let mut sources = Vec::with_capacity(128);
        let mut targets = Vec::with_capacity(128);
        for _ in 0..128 {
            sources.push(store.insert_vertex().expect("src"));
            targets.push(store.insert_vertex().expect("dst"));
        }
        let input: Vec<BatchEdgeInput> = sources
            .iter()
            .zip(&targets)
            .map(|(&s, &t)| BatchEdgeInput {
                source_vertex_id: s,
                target_vertex_id: t,
                catalog_label: Some(label),
                directed: true,
                inline_value_bytes: payload.clone(),
            })
            .collect();
        let _scope = canbench_rs::bench_scope("scalar_directed_128_w8");
        for edge in &input {
            store
                .insert_directed_edge_with_inline_value_bytes(
                    edge.source_vertex_id,
                    edge.target_vertex_id,
                    Some(label),
                    &payload,
                )
                .expect("scalar insert");
        }
    })
}
