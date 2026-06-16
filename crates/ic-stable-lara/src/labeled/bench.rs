//! Benchmarks for the labeled CSR core.
//!
//! Instruction scopes for canbench live in the implementation (`graph.rs`,
//! `deferred.rs`) behind `feature = "canbench"`, not in this file.

use crate::bench as helper;
use crate::labeled::{
    BucketLabelKey, DeferredLabeledLaraGraph, LabeledPayloadValueBatchScratch, LabeledVertex,
    OutEdgeOrder,
    graph::{LabeledLaraGraph, VertexEdgeSpanCompactOneStep},
};
use crate::{
    VertexId,
    lara::maintenance::MaintenanceBudget,
    test_support::{labeled_lara_memories, vector_memory},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use canbench_rs::{bench, bench_fn};
use std::hint::black_box;

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
    payload_len: u16,
}

impl PayloadBenchEdge {
    fn with_payload(target: u32, payload_len: u16, bytes: &[u8]) -> Self {
        let len = u16::try_from(bytes.len()).expect("bench payload fits u16");
        debug_assert_eq!(len, payload_len);
        let mut payload = [0u8; 8];
        payload[..bytes.len()].copy_from_slice(bytes);
        Self {
            target,
            slot_index: 0,
            payload,
            payload_len,
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
            payload_len: 0,
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

    fn edge_payload_byte_width(&self) -> u16 {
        self.payload_len
    }

    fn edge_payload_bytes(&self) -> &[u8] {
        &self.payload[..usize::from(self.payload_len)]
    }

    fn with_stored_payload_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len()).min(8);
        self.payload = [0u8; 8];
        self.payload[..len].copy_from_slice(&bytes[..len]);
        self.payload_len = u16::try_from(len).expect("bench payload width fits u16");
        self
    }
}

impl CsrEdgeTombstone for PayloadBenchEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            slot_index: 0,
            payload: [0u8; 8],
            payload_len: 0,
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
        payload_slab,
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
        payload_slab,
        value_free_spans,
        value_free_span_by_start,
        payload_log,
        value_blob,
        elem_capacity,
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
        payload_slab,
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
        payload_slab,
        value_free_spans,
        value_free_span_by_start,
        payload_log,
        value_blob,
        elem_capacity,
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
        .ensure_label_bucket_payload_byte_width(vid, label, payload_width)
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
            VertexEdgeSpanCompactOneStep::OverflowRewrite(_)
            | VertexEdgeSpanCompactOneStep::Finished => break,
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
                .visit_out_payload_value_batches_for_label(
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

/// ADR 0016: scan after log-backed tombstone deletes without chain rewiring.
#[bench(raw)]
fn bench_labeled_tombstone_log_delete_then_scan() -> canbench_rs::BenchResult {
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
                    count += usize::from(edge.payload_len > 0);
                })
                .expect("for_each");
            black_box(count);
        }
    })
}

/// ADR 0016: foreground log deletes plus incremental span compaction on an overflow edge hub.
#[bench(raw)]
fn bench_labeled_tombstone_log_rewrite_maintenance() -> canbench_rs::BenchResult {
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
