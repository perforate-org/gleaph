//! Benchmarks for the labeled CSR core.
//!
//! Instruction scopes for canbench live in the implementation (`graph.rs`,
//! `deferred.rs`) behind `feature = "canbench"`, not in this file.

use crate::bench as helper;
use crate::labeled::hub_tree_prototype::{HubBucketTree, HubTargetTree};
use crate::labeled::{
    BucketLabelKey, DeferredBidirectionalLabeledLaraGraph, DeferredLabeledLaraGraph,
    LabeledPayloadValueBatchScratch, LabeledVertex, OutEdgeOrder,
    graph::{LabeledLaraGraph, VertexEdgeSpanCompactOneStep},
};
use crate::{
    VertexId,
    lara::maintenance::MaintenanceBudget,
    test_support::{labeled_lara_memories, vector_memory},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use canbench_rs::{bench, bench_fn};
use std::{cell::Cell, hint::black_box};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct BenchEdge(u32);

impl CsrEdge for BenchEdge {
    const BYTES: usize = 10;

    fn read_from(bytes: &[u8]) -> Self {
        Self(u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.0.to_le_bytes());
        bytes[4..10].fill(0);
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.0)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self(u32::from(vid))
    }
}

impl CsrEdgeTombstone for BenchEdge {
    fn tombstone_edge() -> Self {
        Self(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
    }
}

/// Labeled edge with inline payload bytes for payload-log / tombstone benches.
#[derive(Clone, Debug, PartialEq, Eq)]
struct PayloadBenchEdge {
    target: u32,
    slot_index: u32,
    payload: [u8; 8],
    inline_value_len: u16,
}

impl PayloadBenchEdge {
    fn with_payload(target: u32, inline_value_len: u16, bytes: &[u8]) -> Self {
        let len = u16::try_from(bytes.len()).expect("bench payload fits u16");
        debug_assert_eq!(len, inline_value_len);
        let mut payload = [0u8; 8];
        payload[..bytes.len()].copy_from_slice(bytes);
        Self {
            target,
            slot_index: 0,
            payload,
            inline_value_len,
        }
    }
}

impl CsrEdge for PayloadBenchEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            slot_index: 0,
            payload: [0u8; 8],
            inline_value_len: 0,
        }
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.target)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self {
            target: u32::from(vid),
            ..self.clone()
        }
    }

    fn with_slot_index(self, slot_index: u32) -> Self {
        Self { slot_index, ..self }
    }

    fn edge_slot_index_raw(&self) -> u32 {
        self.slot_index
    }

    fn edge_inline_value_byte_width(&self) -> u16 {
        self.inline_value_len
    }

    fn edge_inline_value_bytes(&self) -> &[u8] {
        &self.payload[..usize::from(self.inline_value_len)]
    }

    fn with_stored_inline_value_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len()).min(8);
        self.payload = [0u8; 8];
        self.payload[..len].copy_from_slice(&bytes[..len]);
        self.inline_value_len = u16::try_from(len).expect("bench payload width fits u16");
        self
    }
}

impl CsrEdgeTombstone for PayloadBenchEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            slot_index: 0,
            payload: [0u8; 8],
            inline_value_len: 0,
        }
    }
}

fn bench_graph(elem_capacity: u64) -> LabeledLaraGraph<BenchEdge, crate::VectorMemory> {
    let (
        vertices,
        buckets,
        bucket_free_spans,
        bucket_free_span_by_start,
        edge_counts,
        edges,
        edge_log,
        edge_span_meta,
        edge_free_spans,
        edge_free_span_by_start,
        inline_value_slab,
        value_free_spans,
        value_free_span_by_start,
        payload_log,
        value_blob,
    ) = labeled_lara_memories();
    LabeledLaraGraph::new(
        vertices,
        buckets,
        bucket_free_spans,
        bucket_free_span_by_start,
        edge_counts,
        edges,
        edge_log,
        edge_span_meta,
        edge_free_spans,
        edge_free_span_by_start,
        inline_value_slab,
        value_free_spans,
        value_free_span_by_start,
        payload_log,
        value_blob,
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        BucketLabelKey::from_raw(1),
    )
    .expect("graph")
}

fn payload_bench_graph(
    elem_capacity: u64,
) -> LabeledLaraGraph<PayloadBenchEdge, crate::VectorMemory> {
    let (
        vertices,
        buckets,
        bucket_free_spans,
        bucket_free_span_by_start,
        edge_counts,
        edges,
        edge_log,
        edge_span_meta,
        edge_free_spans,
        edge_free_span_by_start,
        inline_value_slab,
        value_free_spans,
        value_free_span_by_start,
        payload_log,
        value_blob,
    ) = labeled_lara_memories();
    LabeledLaraGraph::new(
        vertices,
        buckets,
        bucket_free_spans,
        bucket_free_span_by_start,
        edge_counts,
        edges,
        edge_log,
        edge_span_meta,
        edge_free_spans,
        edge_free_span_by_start,
        inline_value_slab,
        value_free_spans,
        value_free_span_by_start,
        payload_log,
        value_blob,
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        BucketLabelKey::from_raw(1),
    )
    .expect("graph")
}

fn deferred_bench_graph(
    elem_capacity: u64,
) -> DeferredLabeledLaraGraph<BenchEdge, crate::VectorMemory> {
    let inner = bench_graph(elem_capacity);
    DeferredLabeledLaraGraph::new(inner, vector_memory()).expect("deferred labeled graph")
}

/// Mirrors `CACHE_PREFIX_COUNT` in `crates/graph` shortest-path converging-hub benches:
/// one hub vertex with many parallel same-label edges, then repeated label-filtered scans.
const CONVERGING_HUB_PREFIX_EDGES: u32 = 48;
const CONVERGING_HUB_OUT_EDGES: u32 = 24;
const CONVERGING_HUB_EXPAND_CALLS: u32 = 51;

/// Same-label overflow hub used by ADR 0016 payload-log and tombstone benches.
const OVERFLOW_LOG_HUB_EDGES: u32 = 48;
const PAYLOAD_LOG_INLINE_WIDTH: u16 = 8;

fn seed_overflow_payload_hub(
    graph: &LabeledLaraGraph<PayloadBenchEdge, crate::VectorMemory>,
    edge_count: u32,
    payload_width: u16,
) -> (VertexId, BucketLabelKey) {
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_value_byte_width(vid, label, payload_width)
        .expect("payload width");
    for target in 1..=edge_count {
        let mut payload = [0u8; 8];
        let width = usize::from(payload_width);
        payload[..width].copy_from_slice(&(target as u64).to_le_bytes()[..width]);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                PayloadBenchEdge::with_payload(target, payload_width, &payload),
            )
            .expect("insert");
    }
    (vid, label)
}

fn compact_vertex_edge_span_until_overflow_or_done<E: CsrEdge + CsrEdgeTombstone>(
    graph: &LabeledLaraGraph<E, crate::VectorMemory>,
    vid: VertexId,
) {
    let mut resume = 0u32;
    loop {
        match graph
            .compact_vertex_edge_span_one_step(vid, resume)
            .expect("compact step")
        {
            VertexEdgeSpanCompactOneStep::EdgeMoved(_) => {}
            VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => resume = next,
            VertexEdgeSpanCompactOneStep::OverflowRewrite(_) => resume = 0,
            VertexEdgeSpanCompactOneStep::Finished => break,
        }
    }
}

/// Mirrors `build_mixed_label_hub` in `graph/test_support.rs` for canbench fixtures.
fn seed_mixed_label_hub(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    labels: u16,
    edges_per_label: u32,
) -> VertexId {
    let hub = graph.push_vertex(LabeledVertex::default()).expect("hub");
    let dst = graph.push_vertex(LabeledVertex::default()).expect("dst");
    for label_idx in 0..labels {
        let label = BucketLabelKey::from_raw(10_000 + label_idx);
        for edge_i in 0..edges_per_label {
            graph
                .insert_edge_skip_leaf_cascade(hub, label, BenchEdge(edge_i))
                .expect("insert");
        }
    }
    black_box(dst);
    hub
}

fn seed_single_label_parallel_edges(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    edge_count: u32,
) -> BucketLabelKey {
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let label = BucketLabelKey::from_raw(2);
    for i in 0..edge_count {
        graph
            .insert_edge(VertexId::from(0), label, BenchEdge(i))
            .expect("insert");
    }
    label
}

/// ADR 0022 Stage 2 baseline: a single skewed `(vertex, label)` hub bucket grown
/// far past the per-vertex leaf quota so the per-leaf overflow log is active —
/// the status-quo shape a dedicated-span / B-tree tier would replace. Seeds via
/// `insert_edge_skip_leaf_cascade` so the cost being measured later (delete,
/// lookup, scan) is isolated from leaf-cascade maintenance during seeding.
fn seed_single_label_hub(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    edge_count: u32,
) -> (VertexId, BucketLabelKey) {
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    let label = BucketLabelKey::from_raw(2);
    for i in 0..edge_count {
        graph
            .insert_edge_skip_leaf_cascade(vid, label, BenchEdge(i))
            .expect("hub insert");
    }
    (vid, label)
}

/// Hub degree for the ADR 0022 Stage 2 baselines (single skewed bucket).
const STAGE2_HUB_DEGREE: u32 = 1024;

/// One PMA leaf worth of vertices (matches `DEFAULT_SEGMENT_SIZE`).
const STAGE2_LEAF: u32 = 32;
#[cfg(target_family = "wasm")]
const STABLE_CAPACITY_PROBE_VERTICES: u32 = 1024;
/// Target degree to grow the probed vertex to during the warrant benches.
const STAGE2_GROW_DEGREE: u32 = 256;

/// Capacity probe for the sparse case: one labeled edge for every vertex in one PMA leaf.
/// The measured stable-memory increase includes the first leaf pin, so it exposes the
/// `segment_size × vertex_edge_quota` tradeoff directly.
#[bench(raw)]
fn bench_labeled_capacity_sparse_leaf_32_vertices_1_edge() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = bench_graph(16);
        for _ in 0..STAGE2_LEAF {
            graph.push_vertex(LabeledVertex::default()).expect("vertex");
        }
        for vertex in 0..STAGE2_LEAF {
            graph
                .insert_edge(
                    VertexId::from(vertex),
                    BucketLabelKey::from_raw(2),
                    BenchEdge(vertex),
                )
                .expect("sparse edge");
        }
        black_box(STAGE2_LEAF);
    })
}

/// Stable-backed counterpart of the sparse-leaf capacity probe. `DefaultMemoryImpl`
/// maps to the canister stable memory on wasm, so the result includes the physical
/// page growth caused by the first labeled leaf pin.
#[cfg(target_family = "wasm")]
fn stable_bench_graph(segment_size: u32) -> LabeledLaraGraph<BenchEdge, helper::BenchMemory> {
    let mut memories = helper::BenchMemoryFactory::new();
    LabeledLaraGraph::new_with_segment_size(
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        memories.memory(),
        crate::labeled::InitialCapacities::uniform(16),
        BucketLabelKey::from_raw(1),
        segment_size,
    )
    .expect("stable-backed graph")
}

#[cfg(target_family = "wasm")]
fn run_stable_sparse_capacity(vertex_count: u32, segment_size: u32) {
    let graph = stable_bench_graph(segment_size);
    for vertex in 0..vertex_count {
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
        graph
            .insert_edge(
                VertexId::from(vertex),
                BucketLabelKey::from_raw(2),
                BenchEdge(vertex),
            )
            .expect("sparse edge");
    }
    black_box(vertex_count);
}

#[cfg(target_family = "wasm")]
fn run_stable_hub_growth_capacity(edge_count: u32, segment_size: u32) {
    let graph = stable_bench_graph(segment_size);
    graph
        .push_vertex(LabeledVertex::default())
        .expect("hub vertex");
    for edge in 0..edge_count {
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(2),
                BenchEdge(edge),
            )
            .expect("hub edge");
    }
    black_box(edge_count);
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_labeled_capacity_stable_sparse_leaf_32_vertices_1_edge() -> canbench_rs::BenchResult {
    bench_fn(|| {
        run_stable_sparse_capacity(STAGE2_LEAF, 32);
    })
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_labeled_capacity_stable_sparse_leaf_1024_vertices_1_edge() -> canbench_rs::BenchResult {
    bench_fn(|| {
        run_stable_sparse_capacity(STABLE_CAPACITY_PROBE_VERTICES, 32);
    })
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_labeled_capacity_stable_sparse_segment16_1024_vertices_1_edge() -> canbench_rs::BenchResult
{
    bench_fn(|| {
        run_stable_sparse_capacity(STABLE_CAPACITY_PROBE_VERTICES, 16);
    })
}

/// Stable-backed hub growth probe. This crosses the segment16 leaf quota and
/// exercises overflow-log fold plus leaf relocation/resize.
#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_labeled_capacity_stable_hub_segment16_256_edges() -> canbench_rs::BenchResult {
    bench_fn(|| {
        run_stable_hub_growth_capacity(256, 16);
    })
}

/// Seeds `STAGE2_LEAF` vertices in one leaf; when `saturate_mates` is set, fills
/// every leaf-mate (vids `1..STAGE2_LEAF`) to `STAGE2_LEAF_MATE_DEGREE` on the
/// shared label so the leaf physical block is near-full. Uses the full
/// `insert_edge` path so the measured growth exercises the real leaf
/// slide/relocate cascade. Returns the shared label.
fn seed_stage2_leaf(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    saturate_mates: bool,
) -> BucketLabelKey {
    for _ in 0..STAGE2_LEAF {
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
    }
    let label = BucketLabelKey::from_raw(2);
    if saturate_mates {
        // Keep the fixture at the same relative density when testing an experimental
        // quota. The default quota is 1, so this remains a small sparse-leaf fixture.
        let leaf_mate_degree = crate::labeled::graph::leaf_pin::labeled_leaf_vertex_edge_quota(
            graph.edges().header().segment_size,
        )
        .saturating_sub(4)
        .max(1);
        for v in 1..STAGE2_LEAF {
            for e in 0..leaf_mate_degree {
                graph
                    .insert_edge(VertexId::from(v), label, BenchEdge(v * 10_000 + e))
                    .expect("leaf-mate seed");
            }
        }
    }
    label
}

/// Phase 6 hub regression: 33 labels × 50 edges (span-release cliff shape).
const MIXED_LABEL_HUB_LABELS: u16 = 33;
const MIXED_LABEL_HUB_EDGES_PER_LABEL: u32 = 50;

#[bench(raw)]
fn bench_labeled_mixed_label_hub_insert_33x50() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = bench_graph(1 << 20);
        black_box(seed_mixed_label_hub(
            &graph,
            MIXED_LABEL_HUB_LABELS,
            MIXED_LABEL_HUB_EDGES_PER_LABEL,
        ));
    })
}

#[bench(raw)]
fn bench_labeled_mixed_label_hub_scan_33x50() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let hub = seed_mixed_label_hub(
        &graph,
        MIXED_LABEL_HUB_LABELS,
        MIXED_LABEL_HUB_EDGES_PER_LABEL,
    );
    let vertex = graph.vertices().get(hub);
    bench_fn(|| {
        let mut count = 0usize;
        for offset in 0..vertex.degree() {
            let slot = vertex.base_slot_start().saturating_add(u64::from(offset));
            let bucket = graph
                .buckets()
                .read_label_bucket_slot(slot)
                .expect("bucket");
            let label = bucket.bucket_label_key();
            graph
                .for_each_edges_for_label(hub, label, |_| count += 1)
                .expect("for_each");
        }
        black_box(count);
    })
}

#[bench(raw)]
fn bench_labeled_mixed_label_hub_asc_iter_33x50() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let hub = seed_mixed_label_hub(
        &graph,
        MIXED_LABEL_HUB_LABELS,
        MIXED_LABEL_HUB_EDGES_PER_LABEL,
    );
    bench_fn(|| {
        let edges = graph.asc_out_edges(hub).expect("asc");
        black_box(edges.len());
    })
}

#[bench(raw)]
fn bench_labeled_for_each_edges_for_label_48_x51() -> canbench_rs::BenchResult {
    let graph = bench_graph(16384);
    let label = seed_single_label_parallel_edges(&graph, CONVERGING_HUB_PREFIX_EDGES);
    let vid = VertexId::from(0);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            graph
                .for_each_edges_for_label(vid, label, |edge| {
                    count += usize::from(edge.neighbor_vid().0 > 0);
                })
                .expect("for_each");
            black_box(count);
        }
    })
}

#[bench(raw)]
fn bench_labeled_for_each_edges_for_label_24_x51() -> canbench_rs::BenchResult {
    let graph = bench_graph(8192);
    let label = seed_single_label_parallel_edges(&graph, CONVERGING_HUB_OUT_EDGES);
    let vid = VertexId::from(0);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            graph
                .for_each_edges_for_label(vid, label, |edge| {
                    count += usize::from(edge.neighbor_vid().0 > 0);
                })
                .expect("for_each");
            black_box(count);
        }
    })
}

#[bench(raw)]
fn bench_labeled_iter_edges_for_label_128() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096);
    graph
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .expect("vertex");
    let label = BucketLabelKey::from_raw(2);
    for i in 0..128u32 {
        graph
            .insert_edge(VertexId::from(0), label, BenchEdge(i))
            .expect("insert");
    }
    bench_fn(|| {
        let mut count = 0usize;
        for edge in graph
            .iter_edges_for_label(VertexId::from(0), label)
            .expect("iter")
        {
            count += usize::from(edge.neighbor_vid().0 > 0);
        }
        black_box(count);
    })
}

#[bench(raw)]
fn bench_labeled_default_bypass_iter_128() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096);
    graph
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .expect("vertex");
    graph
        .enable_default_edge_bypass(VertexId::from(0))
        .expect("bypass");
    for i in 0..128u32 {
        graph
            .insert_edge(VertexId::from(0), graph.default_label(), BenchEdge(i))
            .expect("insert");
    }
    bench_fn(|| {
        let mut count = 0usize;
        for edge in graph.out_edges(VertexId::from(0)).expect("iter") {
            count += usize::from(edge.neighbor_vid().0 > 0);
        }
        black_box(count);
    })
}

#[bench(raw)]
fn bench_labeled_insert_existing_bucket_128() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096);
    graph
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .expect("vertex");
    let label = BucketLabelKey::from_raw(2);
    bench_fn(|| {
        for i in 0..helper::MEDIUM_N as u32 {
            let i = black_box(i);
            graph
                .insert_edge(VertexId::from(0), label, BenchEdge(i))
                .expect("insert");
        }
    })
}

#[bench(raw)]
fn bench_labeled_insert_single_bucket_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let label = BucketLabelKey::from_raw(2);
    bench_fn(|| {
        for i in 0..helper::MEDIUM_N as u32 {
            let i = black_box(i);
            graph
                .insert_edge(VertexId::from(0), label, BenchEdge(i))
                .expect("insert");
        }
        black_box(graph.vertex_count());
    })
}

/// Many [`LabelBucket`] rows on one vertex, then repeated inserts into the **last**
/// label (stresses `find_bucket_slot` / bucket metadata reads).
#[bench(raw)]
fn bench_labeled_insert_last_of_many_buckets_1024() -> canbench_rs::BenchResult {
    const N_BUCKETS: u16 = 128;
    let graph = bench_graph(16384);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    for k in 0..N_BUCKETS {
        let label = BucketLabelKey::from_raw(2 + k);
        graph
            .insert_edge(vid, label, BenchEdge(u32::from(k)))
            .expect("seed insert");
    }
    let target = BucketLabelKey::from_raw(2 + N_BUCKETS - 1);
    bench_fn(|| {
        for i in 0..helper::MEDIUM_N as u32 {
            let i = black_box(i);
            graph
                .insert_edge(vid, target, BenchEdge(i))
                .expect("insert");
        }
        black_box(target.raw());
    })
}

/// Round-robin across many labels (mix of `find_bucket_slot` hits on different indices).
#[bench(raw)]
fn bench_labeled_insert_round_robin_64_labels_1024() -> canbench_rs::BenchResult {
    const N_LABELS: u16 = 64;
    let graph = bench_graph(16384);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    for k in 0..N_LABELS {
        let label = BucketLabelKey::from_raw(10 + k);
        graph
            .insert_edge(vid, label, BenchEdge(u32::from(k)))
            .expect("seed insert");
    }
    bench_fn(|| {
        for i in 0..helper::MEDIUM_N as u32 {
            let i = black_box(i);
            let label = BucketLabelKey::from_raw(10 + (i % u32::from(N_LABELS)) as u16);
            graph.insert_edge(vid, label, BenchEdge(i)).expect("insert");
        }
        black_box(N_LABELS);
    })
}

/// Every insert uses a **new** label id so each call walks `find_or_create_bucket`.
#[bench(raw)]
fn bench_labeled_insert_fresh_label_each_edge_256() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = bench_graph(32768);
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
        let vid = VertexId::from(0);
        for i in 0u16..256 {
            let label = BucketLabelKey::from_raw(3000 + i);
            graph
                .insert_edge(vid, label, BenchEdge(u32::from(i)))
                .expect("insert");
        }
        black_box(graph.vertex_count());
    })
}

/// One PMA leaf worth of vertices (32 rows — same as the labeled graph default segment size):
/// light seeding, then round-robin inserts on the same label.
#[bench(raw)]
fn bench_labeled_insert_multi_vertex_leaf32_2048() -> canbench_rs::BenchResult {
    const LEAF: u32 = 32;
    const SEED_PER_VERTEX: u32 = 8;
    let graph = bench_graph(65536);
    for _ in 0..LEAF {
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
    }
    let label = BucketLabelKey::from_raw(5);
    for v in 0..LEAF {
        for e in 0..SEED_PER_VERTEX {
            graph
                .insert_edge(VertexId::from(v), label, BenchEdge(v * 10_000 + e))
                .expect("seed");
        }
    }
    bench_fn(|| {
        for i in 0..2048u32 {
            let i = black_box(i);
            let vid = VertexId::from(i % LEAF);
            graph.insert_edge(vid, label, BenchEdge(i)).expect("insert");
        }
        black_box(LEAF);
    })
}

#[bench(raw)]
fn bench_compact_edge_decode_scan_128() -> canbench_rs::BenchResult {
    let mut bytes = Vec::with_capacity(128 * BenchEdge::BYTES);
    for i in 0..128u32 {
        let mut slot = [0u8; BenchEdge::BYTES];
        BenchEdge(i).write_to(&mut slot);
        bytes.extend_from_slice(&slot);
    }
    bench_fn(|| {
        let mut count = 0usize;
        for chunk in bytes.chunks_exact(BenchEdge::BYTES) {
            let edge = BenchEdge::read_from(chunk);
            count += usize::from(edge.neighbor_vid().0 > 0);
        }
        black_box(count);
    })
}

/// Deferred admission path only (no maintenance drain in the measured region).
#[bench(raw)]
fn bench_labeled_deferred_inserts_only_1024() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = deferred_bench_graph(8192);
        graph
            .inner()
            .push_vertex(LabeledVertex::default())
            .expect("vertex");
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        for i in 0..helper::MEDIUM_N as u32 {
            let i = black_box(i);
            graph.insert_edge(vid, label, BenchEdge(i)).expect("insert");
        }
        black_box(graph.maintenance_queue_len());
    })
}

/// Fragment a vertex edge span, enqueue one compaction item, then run one maintenance step.
#[bench(raw)]
fn bench_labeled_deferred_maintenance_compact_vertex_span_1() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = deferred_bench_graph(8192);
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        graph
            .inner()
            .push_vertex(LabeledVertex::default())
            .expect("vertex");
        for t in 0..80u32 {
            graph.insert_edge(vid, label, BenchEdge(t)).expect("insert");
        }
        for t in 0..72u32 {
            graph
                .remove_edge_matching(vid, label, |e| e.0 == t)
                .expect("remove");
        }
        graph
            .mark_compact_vertex_edge_span(vid, 0)
            .expect("mark compact");
        let report = graph.maintenance(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: None,
        });
        black_box(report.rebalanced_segments);
        black_box(graph.maintenance_queue_len());
    })
}

/// ADR 0016: payload attach over hybrid slab + 8 B inline payload overflow log.
#[bench(raw)]
fn bench_labeled_payload_log_scan_8b_inline_overflow() -> canbench_rs::BenchResult {
    let graph = payload_bench_graph(1 << 20);
    let (vid, label) =
        seed_overflow_payload_hub(&graph, OVERFLOW_LOG_HUB_EDGES, PAYLOAD_LOG_INLINE_WIDTH);
    let mut scratch = LabeledPayloadValueBatchScratch::default();
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut byte_count = 0usize;
            graph
                .visit_out_inline_value_batches_for_label(
                    vid,
                    label,
                    OutEdgeOrder::Descending,
                    &mut scratch,
                    |batch| {
                        byte_count = byte_count.saturating_add(batch.values.len());
                    },
                )
                .expect("payload batches");
            black_box(byte_count);
        }
    })
}

/// Baseline for exact payload-slab growth before introducing bounded headroom.
#[bench(raw)]
fn bench_labeled_payload_exact_growth_256() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = payload_bench_graph(1 << 20);
        let vid = graph.push_vertex(LabeledVertex::default()).expect("vertex");
        let label = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(vid, label, 2)
            .expect("payload width");
        for target in 0..256u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    PayloadBenchEdge::with_payload(target, 2, &(target as u16).to_le_bytes()),
                )
                .expect("payload insert");
        }
        black_box(graph.out_edges(vid).expect("scan").len());
    })
}

fn seed_fragmented_payload_fixture(
    graph: &LabeledLaraGraph<PayloadBenchEdge, crate::VectorMemory>,
) -> VertexId {
    let vid = graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let removed_small = BucketLabelKey::directed_from_index(2);
    let live_separator = BucketLabelKey::directed_from_index(3);
    let removed_wide = BucketLabelKey::directed_from_index(4);
    for (label, target, width) in [
        (removed_small, 0, 2),
        (live_separator, 1, 2),
        (removed_wide, 2, 4),
    ] {
        graph
            .ensure_label_bucket_inline_value_byte_width(vid, label, width)
            .expect("payload width");
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                PayloadBenchEdge::with_payload(target, width, &target.to_le_bytes()),
            )
            .expect("payload insert");
    }
    for (label, target) in [(removed_small, 0), (removed_wide, 2)] {
        graph
            .remove_edge_matching(vid, label, |edge| edge.target == target)
            .expect("payload remove")
            .expect("removed payload edge");
    }
    vid
}

/// Measures the first payload allocation that triggers fragmented-slab compaction.
#[bench(raw)]
fn bench_labeled_payload_fragmented_first_span_6() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = payload_bench_graph(1 << 20);
        let vid = seed_fragmented_payload_fixture(&graph);
        let target = BucketLabelKey::directed_from_index(5);
        graph
            .ensure_label_bucket_inline_value_byte_width(vid, target, 6)
            .expect("payload width");
        let _scope = canbench_rs::bench_scope("payload_fragmented_trigger_insert");
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                target,
                PayloadBenchEdge::with_payload(3, 6, &3u32.to_le_bytes()),
            )
            .expect("payload insert");
        black_box(graph.payload_storage_stats().expect("payload stats"));
    })
}

/// Control for a same-fixture first allocation that can reuse one free span.
#[bench(raw)]
fn bench_labeled_payload_fragmented_first_span_4_control() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = payload_bench_graph(1 << 20);
        let vid = seed_fragmented_payload_fixture(&graph);
        let target = BucketLabelKey::directed_from_index(5);
        graph
            .ensure_label_bucket_inline_value_byte_width(vid, target, 4)
            .expect("payload width");
        let _scope = canbench_rs::bench_scope("payload_fragmented_control_insert");
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                target,
                PayloadBenchEdge::with_payload(3, 4, &3u32.to_le_bytes()),
            )
            .expect("payload insert");
        black_box(graph.payload_storage_stats().expect("payload stats"));
    })
}

/// Isolates the payload-only compaction pass for the fragmented fixture.
#[bench(raw)]
fn bench_labeled_payload_fragmented_compaction_only() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = payload_bench_graph(1 << 20);
        let _vid = seed_fragmented_payload_fixture(&graph);
        let _scope = canbench_rs::bench_scope("payload_fragmented_compaction_only");
        black_box(graph.compact_payload_slab().expect("payload compaction"));
        black_box(graph.payload_storage_stats().expect("payload stats"));
    })
}

/// Measures deferred insertion with payload pressure detection and queue enqueue only.
#[bench(raw)]
fn bench_labeled_deferred_payload_fragmented_enqueue_6() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = DeferredLabeledLaraGraph::new(payload_bench_graph(1 << 20), vector_memory())
            .expect("deferred graph");
        let vid = seed_fragmented_payload_fixture(graph.inner());
        let target = BucketLabelKey::directed_from_index(5);
        graph
            .inner()
            .ensure_label_bucket_inline_value_byte_width(vid, target, 6)
            .expect("payload width");
        let _scope = canbench_rs::bench_scope("payload_deferred_enqueue_insert");
        graph
            .insert_edge(vid, target, PayloadBenchEdge::with_payload(3, 6, &[3u8; 6]))
            .expect("payload insert");
        black_box(graph.maintenance_queue_len());
    })
}

/// Measures the deferred maintenance step that performs payload compaction.
#[bench(raw)]
fn bench_labeled_deferred_payload_fragmented_maintenance_6() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = DeferredLabeledLaraGraph::new(payload_bench_graph(1 << 20), vector_memory())
            .expect("deferred graph");
        let vid = seed_fragmented_payload_fixture(graph.inner());
        let target = BucketLabelKey::directed_from_index(5);
        graph
            .inner()
            .ensure_label_bucket_inline_value_byte_width(vid, target, 6)
            .expect("payload width");
        graph
            .insert_edge(vid, target, PayloadBenchEdge::with_payload(3, 6, &[3u8; 6]))
            .expect("payload insert");
        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: None,
        };
        let _scope = canbench_rs::bench_scope("payload_deferred_maintenance_compaction");
        black_box(graph.maintenance(budget));
    })
}

/// ADR 0016: scan after tombstone-free direct log unlinks.
#[bench(raw)]
fn bench_labeled_direct_unlink_log_delete_then_scan() -> canbench_rs::BenchResult {
    let graph = payload_bench_graph(1 << 20);
    let (vid, label) = seed_overflow_payload_hub(&graph, OVERFLOW_LOG_HUB_EDGES, 2);
    for target in (1..=OVERFLOW_LOG_HUB_EDGES).step_by(2) {
        graph
            .remove_edge_matching(vid, label, |edge| edge.target == target)
            .expect("remove")
            .expect("removed");
    }
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            graph
                .for_each_edges_for_label(vid, label, |edge| {
                    count += usize::from(edge.inline_value_len > 0);
                })
                .expect("for_each");
            black_box(count);
        }
    })
}

/// ADR 0016: foreground direct log unlink plus incremental span compaction on an overflow edge hub.
#[bench(raw)]
fn bench_labeled_direct_unlink_log_fold_maintenance() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = bench_graph(1 << 20);
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        for target in 1..=OVERFLOW_LOG_HUB_EDGES {
            graph
                .insert_edge_skip_leaf_cascade(vid, label, BenchEdge(target))
                .expect("insert");
        }
        for target in 1..=OVERFLOW_LOG_HUB_EDGES / 2 {
            graph
                .remove_edge_matching(vid, label, |edge| edge.0 == target)
                .expect("remove")
                .expect("removed");
        }
        compact_vertex_edge_span_until_overflow_or_done(&graph, vid);
        let mut count = 0usize;
        graph
            .for_each_edges_for_label(vid, label, |_| count += 1)
            .expect("for_each");
        black_box(count);
    })
}

/// ADR 0022 Stage 2 baseline (delete churn): remove half of a 1024-edge skewed hub
/// bucket by match, then compact the vertex edge span. This is the O(degree)
/// delete-plus-compaction cost that a B-tree tier's O(log d) delete-by-`seq`
/// (no compaction) would replace; the hub is seeded outside the measured region.
#[bench(raw)]
fn bench_labeled_stage2_hub_delete_half_then_compact_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let (vid, label) = seed_single_label_hub(&graph, STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for target in (0..STAGE2_HUB_DEGREE).step_by(2) {
            graph
                .remove_edge_matching(vid, label, |edge| edge.0 == target)
                .expect("remove")
                .expect("removed");
        }
        compact_vertex_edge_span_until_overflow_or_done(&graph, vid);
        let mut count = 0usize;
        graph
            .for_each_edges_for_label(vid, label, |_| count += 1)
            .expect("for_each");
        black_box(count);
    })
}

/// ADR 0022 Stage 2 baseline (fair delete-by-handle): remove half of a 1024-edge
/// skewed hub bucket via `remove_edge_at_slot` — the **real** delete-by-handle path
/// (O(1) slab tombstone for prefix slots, O(chain) for overflow-log slots), with no
/// O(degree) find-scan — then compact and scan survivors. This is the honest
/// status-quo delete cost to weigh against the B-tree `..._delete_half_1024`;
/// `..._delete_half_then_compact_1024` overstates it by also paying a find-scan.
#[bench(raw)]
fn bench_labeled_stage2_hub_delete_half_by_slot_then_compact_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let (vid, label) = seed_single_label_hub(&graph, STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for slot in (0..STAGE2_HUB_DEGREE).step_by(2) {
            graph
                .remove_edge_at_slot(vid, label, slot)
                .expect("remove")
                .expect("removed");
        }
        compact_vertex_edge_span_until_overflow_or_done(&graph, vid);
        let mut count = 0usize;
        graph
            .for_each_edges_for_label(vid, label, |_| count += 1)
            .expect("for_each");
        black_box(count);
    })
}

/// ADR 0022 Stage 2 baseline (point lookup): scan a 1024-edge skewed hub bucket in
/// the hot descending order to locate one target. Today this is O(degree) (the
/// `find_first_forward_handle_descending` walk); an optional `target -> seq`
/// secondary index would make it O(log d). Worst case: the first-inserted target
/// (target `0`) is reached last in descending order.
#[bench(raw)]
fn bench_labeled_stage2_hub_point_lookup_descending_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let (vid, label) = seed_single_label_hub(&graph, STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let needle = black_box(0u32);
            let mut hits = 0usize;
            graph
                .for_each_edges_for_label_ordered(vid, label, OutEdgeOrder::Descending, |edge| {
                    hits += usize::from(edge.0 == needle);
                })
                .expect("descending lookup");
            black_box(hits);
        }
    })
}

/// ADR 0022 Stage 2 baseline (scan locality): full descending scan of a 1024-edge
/// skewed hub bucket — the contiguous-scan cost the span/B-tree tiers must not
/// regress relative to the shared-leaf slab.
#[bench(raw)]
fn bench_labeled_stage2_hub_scan_descending_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let (vid, label) = seed_single_label_hub(&graph, STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            graph
                .for_each_edges_for_label_ordered(vid, label, OutEdgeOrder::Descending, |edge| {
                    count += usize::from(edge.0 < STAGE2_HUB_DEGREE);
                })
                .expect("descending scan");
            black_box(count);
        }
    })
}

/// ADR 0022 Stage 2 *warrant* (dedicated-span tier): grow one vertex to 256 edges
/// inside a **saturated** PMA leaf (31 leaf-mates near the per-vertex quota), so
/// growth repeatedly fights the shared leaf block via slide/relocate. Pair with
/// `..._isolated_vertex_grow_one_256`; the delta estimates the cost a per-bucket
/// dedicated span (which isolates the hot vertex) would recover — the evidence
/// that gates whether the dedicated-span tier is warranted.
#[bench(raw)]
fn bench_labeled_stage2_saturated_leaf_grow_one_256() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let label = seed_stage2_leaf(&graph, true);
    bench_fn(|| {
        for e in 0..STAGE2_GROW_DEGREE {
            let e = black_box(e);
            graph
                .insert_edge(VertexId::from(0), label, BenchEdge(e))
                .expect("saturated grow insert");
        }
        black_box(STAGE2_GROW_DEGREE);
    })
}

/// ADR 0022 Stage 2 *warrant* (dedicated-span tier): grow one vertex to 256 edges
/// with its leaf-mates **empty**, i.e. the vertex effectively owns its leaf block
/// — the lower bound a dedicated span approximates. Compare against
/// `..._saturated_leaf_grow_one_256` for the 2a benefit estimate.
#[bench(raw)]
fn bench_labeled_stage2_isolated_vertex_grow_one_256() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let label = seed_stage2_leaf(&graph, false);
    bench_fn(|| {
        for e in 0..STAGE2_GROW_DEGREE {
            let e = black_box(e);
            graph
                .insert_edge(VertexId::from(0), label, BenchEdge(e))
                .expect("isolated grow insert");
        }
        black_box(STAGE2_GROW_DEGREE);
    })
}

/// Vertex / label the Stage 2b prototype tree uses (single hot bucket; values
/// are arbitrary since the tree holds exactly one `(vertex, label)` prefix).
const STAGE2B_VERTEX: u32 = 0;
const STAGE2B_LABEL: u16 = 2;

/// Seeds the ADR 0022 Stage 2b prototype B-tree with one hot bucket of
/// `edge_count` edges in insertion order; `seq == target == i`. Seeding happens
/// outside the measured region, mirroring `seed_single_label_hub`.
fn seed_stage2b_hub_tree(edge_count: u32) -> HubBucketTree {
    let mut tree = HubBucketTree::new(vector_memory());
    for i in 0..edge_count {
        let seq = tree.insert(STAGE2B_VERTEX, STAGE2B_LABEL, i);
        debug_assert_eq!(seq, i);
    }
    tree
}

/// ADR 0022 Stage 2b prototype (delete churn) — B-tree counterpart of
/// `bench_labeled_stage2_hub_delete_half_then_compact_1024`. Removes the same
/// half (even seqs) by `seq` — the realistic delete-by-handle path — at O(log d)
/// each, with no tombstone and no compaction, then iterates the survivors. The
/// delta against the slab baseline is the Stage 2b delete win.
#[bench(raw)]
fn bench_labeled_stage2b_btree_hub_delete_half_1024() -> canbench_rs::BenchResult {
    let mut tree = seed_stage2b_hub_tree(STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for seq in (0..STAGE2_HUB_DEGREE).step_by(2) {
            let removed = tree.remove_by_seq(STAGE2B_VERTEX, STAGE2B_LABEL, seq);
            debug_assert!(removed);
        }
        let mut count = 0usize;
        tree.for_each_ascending(STAGE2B_VERTEX, STAGE2B_LABEL, |_seq, _target| count += 1);
        black_box(count);
    })
}

/// ADR 0022 Stage 2b prototype (point lookup) — B-tree counterpart of
/// `bench_labeled_stage2_hub_point_lookup_descending_1024`. Without a
/// `target -> seq` index this is still O(degree) (descending value scan); it
/// quantifies the *no-index* B-tree lookup so the index's marginal value is
/// visible. Worst case: target `0` is reached last in descending order.
#[bench(raw)]
fn bench_labeled_stage2b_btree_hub_point_lookup_descending_1024() -> canbench_rs::BenchResult {
    let tree = seed_stage2b_hub_tree(STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let needle = black_box(0u32);
            let found = tree.find_seq_by_target(STAGE2B_VERTEX, STAGE2B_LABEL, needle);
            black_box(found);
        }
    })
}

/// ADR 0022 Stage 2b prototype (scan locality) — B-tree counterpart of
/// `bench_labeled_stage2_hub_scan_descending_1024`. Full descending range scan
/// via the map's `DoubleEndedIterator`; the delta against the slab baseline is
/// the scan-locality cost the B-tree tier must not regress.
#[bench(raw)]
fn bench_labeled_stage2b_btree_hub_scan_descending_1024() -> canbench_rs::BenchResult {
    let tree = seed_stage2b_hub_tree(STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            tree.for_each_descending(STAGE2B_VERTEX, STAGE2B_LABEL, |_seq, target| {
                count += usize::from(target < STAGE2_HUB_DEGREE);
            });
            black_box(count);
        }
    })
}

/// ADR 0022 Stage 2b *experiment* (value-size sensitivity): full descending scan
/// of the 10-byte-value tree reading **only the key** (value never deserialized).
/// Compared with `..._scan_descending_1024` this isolates the traversal + key
/// floor from value-deser cost — i.e. whether shrinking/splitting the value can
/// help scans at all, or whether B-tree traversal dominates.
#[bench(raw)]
fn bench_labeled_stage2b_btree_hub_scan_descending_keyonly_1024() -> canbench_rs::BenchResult {
    let tree = seed_stage2b_hub_tree(STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            tree.for_each_descending_key_only(STAGE2B_VERTEX, STAGE2B_LABEL, |seq| {
                count += usize::from(seq < STAGE2_HUB_DEGREE);
            });
            black_box(count);
        }
    })
}

/// Seeds the production-faithful narrow tree (4-byte `target` value) with one hot
/// bucket of `edge_count` edges; `seq == target == i`.
fn seed_stage2b_narrow_tree(edge_count: u32) -> HubTargetTree {
    let mut tree = HubTargetTree::new(vector_memory());
    for i in 0..edge_count {
        let seq = tree.insert(STAGE2B_VERTEX, STAGE2B_LABEL, i);
        debug_assert_eq!(seq, i);
    }
    tree
}

/// ADR 0022 Stage 2b *experiment* (narrow value): delete-half-by-`seq` + survivor
/// scan on the 4-byte-value tree (matches `Edge::BYTES`). Compare against
/// `..._btree_hub_delete_half_1024` (10-byte value) for delete value-sensitivity.
#[bench(raw)]
fn bench_labeled_stage2b_narrow_hub_delete_half_1024() -> canbench_rs::BenchResult {
    let mut tree = seed_stage2b_narrow_tree(STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for seq in (0..STAGE2_HUB_DEGREE).step_by(2) {
            let removed = tree.remove_by_seq(STAGE2B_VERTEX, STAGE2B_LABEL, seq);
            debug_assert!(removed);
        }
        let mut count = 0usize;
        tree.for_each_descending(STAGE2B_VERTEX, STAGE2B_LABEL, |_seq, _target| count += 1);
        black_box(count);
    })
}

/// ADR 0022 Stage 2b *experiment* (narrow value): point lookup by target on the
/// 4-byte-value tree. Compare against `..._btree_hub_point_lookup_descending_1024`.
#[bench(raw)]
fn bench_labeled_stage2b_narrow_hub_point_lookup_descending_1024() -> canbench_rs::BenchResult {
    let tree = seed_stage2b_narrow_tree(STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let needle = black_box(0u32);
            let found = tree.find_seq_by_target(STAGE2B_VERTEX, STAGE2B_LABEL, needle);
            black_box(found);
        }
    })
}

/// ADR 0022 Stage 2b *experiment* (narrow value): full descending scan on the
/// 4-byte-value tree. Compare against `..._btree_hub_scan_descending_1024`
/// (10-byte value) to read off the value-size effect on scan locality.
#[bench(raw)]
fn bench_labeled_stage2b_narrow_hub_scan_descending_1024() -> canbench_rs::BenchResult {
    let tree = seed_stage2b_narrow_tree(STAGE2_HUB_DEGREE);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            tree.for_each_descending(STAGE2B_VERTEX, STAGE2B_LABEL, |_seq, target| {
                count += usize::from(target < STAGE2_HUB_DEGREE);
            });
            black_box(count);
        }
    })
}

/// ADR 0022 Stage 2b *insert cost* (paid update path): grow one fresh hub vertex
/// 0 → 1024 edges via the **real** `insert_edge` cascade (leaf slide/relocate +
/// overflow log) — the status-quo insert cost an update call pays. Pair with
/// `bench_labeled_stage2b_narrow_hub_insert_1024` for the B-tree insert delta.
#[bench(raw)]
fn bench_labeled_stage2_hub_insert_grow_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    let label = BucketLabelKey::from_raw(2);
    bench_fn(|| {
        for target in 0..STAGE2_HUB_DEGREE {
            let target = black_box(target);
            graph
                .insert_edge(vid, label, BenchEdge(target))
                .expect("hub grow insert");
        }
        black_box(STAGE2_HUB_DEGREE);
    })
}

/// ADR 0022 Stage 2b *insert cost* (paid update path): append 1024 edges into a
/// fresh production-faithful B-tree (4-byte value), each an O(log d) insert with
/// no leaf cascade. Delta vs `bench_labeled_stage2_hub_insert_grow_1024` is the
/// B-tree insert win/loss on the update (cost-bearing) path.
#[bench(raw)]
fn bench_labeled_stage2b_narrow_hub_insert_1024() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let mut tree = HubTargetTree::new(vector_memory());
        for target in 0..STAGE2_HUB_DEGREE {
            let seq = tree.insert(STAGE2B_VERTEX, STAGE2B_LABEL, black_box(target));
            black_box(seq);
        }
    })
}

/// ADR 0022 Stage 2b *crossover* (paid update path): the same insert and fair
/// delete-by-handle pair as the `..._1024` benches, parameterized by degree, to
/// locate where the slab's unindexed overflow-log delete (O(degree) chain walk
/// per log-resident slot; O(degree²) over a delete-half) loses to the B-tree's
/// O(log d) delete — and to confirm insert stays slab-favored at scale.
macro_rules! stage2b_crossover_benches {
    ($deg:expr, $slab_del:ident, $bt_del:ident, $slab_ins:ident, $bt_ins:ident) => {
        #[bench(raw)]
        fn $slab_del() -> canbench_rs::BenchResult {
            let graph = bench_graph(1 << 20);
            let (vid, label) = seed_single_label_hub(&graph, $deg);
            bench_fn(|| {
                for slot in (0..$deg).step_by(2) {
                    graph
                        .remove_edge_at_slot(vid, label, slot)
                        .expect("remove")
                        .expect("removed");
                }
                compact_vertex_edge_span_until_overflow_or_done(&graph, vid);
                let mut count = 0usize;
                graph
                    .for_each_edges_for_label(vid, label, |_| count += 1)
                    .expect("for_each");
                black_box(count);
            })
        }

        #[bench(raw)]
        fn $bt_del() -> canbench_rs::BenchResult {
            let mut tree = seed_stage2b_narrow_tree($deg);
            bench_fn(|| {
                for seq in (0..$deg).step_by(2) {
                    let removed = tree.remove_by_seq(STAGE2B_VERTEX, STAGE2B_LABEL, seq);
                    debug_assert!(removed);
                }
                let mut count = 0usize;
                tree.for_each_descending(STAGE2B_VERTEX, STAGE2B_LABEL, |_seq, _target| count += 1);
                black_box(count);
            })
        }

        #[bench(raw)]
        fn $slab_ins() -> canbench_rs::BenchResult {
            let graph = bench_graph(1 << 20);
            graph.push_vertex(LabeledVertex::default()).expect("vertex");
            let vid = VertexId::from(0);
            let label = BucketLabelKey::from_raw(2);
            bench_fn(|| {
                for target in 0..$deg {
                    let target = black_box(target);
                    graph
                        .insert_edge(vid, label, BenchEdge(target))
                        .expect("hub grow insert");
                }
                black_box::<u32>($deg);
            })
        }

        #[bench(raw)]
        fn $bt_ins() -> canbench_rs::BenchResult {
            bench_fn(|| {
                let mut tree = HubTargetTree::new(vector_memory());
                for target in 0..$deg {
                    let seq = tree.insert(STAGE2B_VERTEX, STAGE2B_LABEL, black_box(target));
                    black_box(seq);
                }
            })
        }
    };
}

stage2b_crossover_benches!(
    4096u32,
    bench_labeled_stage2_hub_delete_half_by_slot_then_compact_4096,
    bench_labeled_stage2b_narrow_hub_delete_half_4096,
    bench_labeled_stage2_hub_insert_grow_4096,
    bench_labeled_stage2b_narrow_hub_insert_4096
);

stage2b_crossover_benches!(
    16384u32,
    bench_labeled_stage2_hub_delete_half_by_slot_then_compact_16384,
    bench_labeled_stage2b_narrow_hub_delete_half_16384,
    bench_labeled_stage2_hub_insert_grow_16384,
    bench_labeled_stage2b_narrow_hub_insert_16384
);

/// Builds an empty bidirectional (forward + reverse store) deferred labeled graph
/// for `BenchEdge`, mirroring the `valued_bidirectional_graph` test fixture. Used
/// by the real DETACH DELETE benches, which need both orientations.
fn bidirectional_bench_graph()
-> DeferredBidirectionalLabeledLaraGraph<BenchEdge, crate::VectorMemory> {
    let (
        fv,
        fb,
        fbfs,
        fbfsbs,
        fec,
        fe,
        fel,
        fesm,
        fefs,
        fefsbs,
        fvs,
        fvffs,
        fvffsbs,
        fvlog,
        fvblobs,
    ) = labeled_lara_memories();
    let (
        rv,
        rb,
        rbfs,
        rbfsbs,
        rec,
        re,
        rel,
        resm,
        refs,
        refsbs,
        rvs,
        rvffs,
        rvffsbs,
        rvlog,
        rvblobs,
    ) = labeled_lara_memories();
    DeferredBidirectionalLabeledLaraGraph::new(
        fv,
        fb,
        fbfs,
        fbfsbs,
        fec,
        fe,
        fel,
        fesm,
        fefs,
        fefsbs,
        fvs,
        fvffs,
        fvffsbs,
        fvlog,
        fvblobs,
        rv,
        rb,
        rbfs,
        rbfsbs,
        rec,
        re,
        rel,
        resm,
        refs,
        refsbs,
        rvs,
        rvffs,
        rvffsbs,
        rvlog,
        rvblobs,
        vector_memory(),
        vector_memory(),
        crate::labeled::InitialCapacities::uniform(1 << 20),
        BucketLabelKey::UNLABELED_DIRECTED,
    )
    .expect("bidirectional bench graph")
}

/// ADR 0022 Stage 2b *real DETACH DELETE* (paid update path): delete a degree-D
/// hub from the bidirectional graph via `delete_vertex_deferred` — the true
/// vertex-delete cost the single-bucket benches omit. It removes both
/// orientations (forward + reverse stores) and, for every incident edge, locates
/// and removes the mirror at the neighbour by target predicate scan
/// (`remove_edge_matching`). Neighbours have degree 1, so the cost is dominated by
/// re-scanning the hub's own shrinking adjacency: O(D²) on the slab. This is the
/// operation a B-tree hub tier would have to beat — and it requires the same
/// target-based mirror lookups, the B-tree's weakest axis.
macro_rules! detach_delete_hub_bench {
    ($name:ident, $deg:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let graph = bidirectional_bench_graph();
            let hub = graph.push_vertex().expect("hub");
            let label = BucketLabelKey::directed_from_index(2);
            for _ in 0..$deg {
                let neighbor = graph.push_vertex().expect("neighbor");
                graph
                    .insert_directed_edge(
                        hub,
                        neighbor,
                        label,
                        BenchEdge(u32::from(neighbor)),
                        BenchEdge(u32::from(hub)),
                    )
                    .expect("insert directed");
            }
            bench_fn(|| {
                let removed = graph.delete_vertex_deferred(hub).expect("detach delete");
                black_box(removed);
            })
        }
    };
}

detach_delete_hub_bench!(bench_labeled_stage2_detach_delete_hub_1024, 1024u32);
detach_delete_hub_bench!(bench_labeled_stage2_detach_delete_hub_4096, 4096u32);

/// Same DETACH DELETE as above, but through the **resumable/stepped** path
/// (`enqueue_vertex_delete` + maintenance drain) that the production
/// `detach_delete_vertex` uses. Validates that the stepped step removes incident
/// edges in O(degree) (front-packed top-slot removal after a one-time compaction)
/// rather than the prior O(degree^2) per-step `asc_out_edges` + predicate re-scan.
macro_rules! detach_delete_hub_stepped_bench {
    ($name:ident, $deg:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let graph = bidirectional_bench_graph();
            let hub = graph.push_vertex().expect("hub");
            let label = BucketLabelKey::directed_from_index(2);
            for _ in 0..$deg {
                let neighbor = graph.push_vertex().expect("neighbor");
                graph
                    .insert_directed_edge(
                        hub,
                        neighbor,
                        label,
                        BenchEdge(u32::from(neighbor)),
                        BenchEdge(u32::from(hub)),
                    )
                    .expect("insert directed");
            }
            let budget = MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            };
            graph.maintenance(budget).expect("settle inserts");
            bench_fn(|| {
                graph.enqueue_vertex_delete(hub).expect("enqueue");
                while graph.maintenance_queue_len() > 0 {
                    let report = graph.maintenance(budget).expect("drain");
                    black_box(report.work.processed_delete_edge_steps);
                }
            })
        }
    };
}

detach_delete_hub_stepped_bench!(bench_labeled_stage2_detach_delete_hub_stepped_1024, 1024u32);
detach_delete_hub_stepped_bench!(bench_labeled_stage2_detach_delete_hub_stepped_4096, 4096u32);

/// ADR 0022 *dual* DETACH DELETE: delete a **small** satellite vertex that points
/// into a high-in-degree hub. Draining the satellite is O(1), but removing its
/// single mirror in the hub's reverse row uses a `remove_edge_matching` predicate
/// scan over the hub's whole in-adjacency — O(hub in-degree). The satellite's edge
/// is inserted last, so the scan walks the full row (worst case). This measures the
/// cost a source-keyed reverse-store locator / mirror index would remove. Compare
/// 1024 vs 4096 to see whether the single-vertex delete scales with the *neighbour's*
/// degree (the dual of the hub-delete quadratic).
macro_rules! detach_delete_satellite_bench {
    ($name:ident, $hub_in_deg:expr) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            let graph = bidirectional_bench_graph();
            let hub = graph.push_vertex().expect("hub");
            let label = BucketLabelKey::directed_from_index(2);
            for _ in 0..$hub_in_deg {
                let source = graph.push_vertex().expect("source");
                graph
                    .insert_directed_edge(
                        source,
                        hub,
                        label,
                        BenchEdge(u32::from(hub)),
                        BenchEdge(u32::from(source)),
                    )
                    .expect("source -> hub");
            }
            // Satellite inserted last: its mirror sits at the tail of the hub's
            // reverse in-adjacency, so the predicate scan walks the entire row.
            let satellite = graph.push_vertex().expect("satellite");
            graph
                .insert_directed_edge(
                    satellite,
                    hub,
                    label,
                    BenchEdge(u32::from(hub)),
                    BenchEdge(u32::from(satellite)),
                )
                .expect("satellite -> hub");
            let budget = MaintenanceBudget {
                max_instructions: 0,
                reserve_instructions: 0,
                checkpoint_every: 1,
                max_work_items: None,
                max_segments: None,
                max_delete_edge_steps: None,
            };
            graph.maintenance(budget).expect("settle inserts");
            bench_fn(|| {
                let removed = graph
                    .delete_vertex_deferred(satellite)
                    .expect("detach delete satellite");
                black_box(removed);
            })
        }
    };
}

detach_delete_satellite_bench!(
    bench_labeled_stage2_detach_delete_satellite_of_hub_1024,
    1024u32
);
detach_delete_satellite_bench!(
    bench_labeled_stage2_detach_delete_satellite_of_hub_4096,
    4096u32
);

#[bench(raw)]
fn bench_labeled_bypass_promotion() -> canbench_rs::BenchResult {
    const N: u32 = 512;
    let default = BucketLabelKey::directed_from_index(1);
    let graph = LabeledLaraGraph::<BenchEdge, _>::new(
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        vector_memory(),
        crate::labeled::InitialCapacities::uniform(256),
        default,
    )
    .expect("labeled graph");

    for i in 0..N {
        graph
            .push_vertex(LabeledVertex::default())
            .expect("push vertex");
        graph
            .insert_edge(VertexId::from(i), default, BenchEdge(i))
            .expect("default edge");
    }

    let next = Cell::new(0u32);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("labeled_bypass_promotion");
        let i = next.get();
        if i >= N {
            return;
        }
        let vid = VertexId::from(i);
        let road = BucketLabelKey::directed_from_index((i as u16).wrapping_add(1000));
        graph
            .insert_edge(vid, road, BenchEdge(u32::MAX))
            .expect("promote bypass");
        next.set(i + 1);
        black_box(graph.vertices().get(vid));
    })
}
