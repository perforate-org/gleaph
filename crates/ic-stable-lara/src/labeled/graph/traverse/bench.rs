//! ADR 0050 Phase 2 dedicated benchmarks for the `traverse` read surface.
//!
//! These benchmarks measure the consolidated logical read API in isolation.
//! Each benchmark sets up its fixture outside the measured closure and then
//! exercises one canonical read primitive. Results are intended to be compared
//! with the corresponding legacy `traverse` benchmarks in `labeled/bench.rs`
//! under identical build settings.

use crate::labeled::{
    BucketLabelKey, DeferredBidirectionalLabeledLaraGraph, LabeledInlinePropertyValueBatchScratch,
    LabeledVertex, OutEdgeOrder,
    graph::LabeledLaraGraph,
    graph::traverse::{BucketEntryPosition, LabeledTraversalRequest},
};
use crate::traverse::{Traversal, TraversalWindow};
use crate::{
    VertexId,
    test_support::labeled_lara_memories,
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use canbench_rs::{bench, bench_fn};
use std::hint::black_box;

const DENSE_DEGREE: u32 = 256;
const HYBRID_DEGREE: u32 = 256;
const BYPASS_DEGREE: u32 = 64;
const DIRECT_SELECTED_SLOT_COUNT: usize = 2;
const INLINE_VALUE_WIDTH: u16 = 8;

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

#[derive(Clone, Debug, PartialEq, Eq)]
struct InlinePropertyBenchEdge {
    target: u32,
    inline_property: [u8; 8],
}

impl InlinePropertyBenchEdge {
    fn new(target: u32, value: u64) -> Self {
        Self {
            target,
            inline_property: value.to_le_bytes(),
        }
    }
}

impl CsrEdge for InlinePropertyBenchEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            inline_property: [0u8; 8],
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
            inline_property: self.inline_property,
        }
    }

    fn edge_inline_property_byte_width(&self) -> u16 {
        INLINE_VALUE_WIDTH
    }

    fn edge_inline_property_bytes(&self) -> &[u8] {
        &self.inline_property
    }

    fn with_stored_inline_property_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len()).min(8);
        self.inline_property = [0u8; 8];
        self.inline_property[..len].copy_from_slice(&bytes[..len]);
        self
    }
}

impl CsrEdgeTombstone for InlinePropertyBenchEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            inline_property: [0u8; 8],
        }
    }
}

const OFFSET_LIVE_ROWS: u32 = 1_024;
const OFFSET_WINDOW_OFFSET: u32 = 960;
const OFFSET_WINDOW_LIMIT: u32 = 32;
const OFFSET_EDGE_CAPACITY: u64 = 16_384;
const OFFSET_TARGET_BASE: u32 = 10_000;
const OFFSET_EXTENTS: [u32; 4] = [1_024, 2_048, 4_096, 8_192];

struct OffsetFixture {
    graph: LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
    truth: Vec<(BucketEntryPosition, u32)>,
}

fn offset_target(slot: u32) -> u32 {
    OFFSET_TARGET_BASE + slot
}

fn offset_bucket(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
) -> crate::labeled::LabelBucket {
    let vertex = graph.vertices().get(src);
    assert!(!vertex.is_default_edge_labeled());
    assert_eq!(vertex.degree(), 1, "fixture must contain one label bucket");
    let bucket = graph
        .buckets()
        .read_label_bucket_slot(vertex.base_slot_start())
        .expect("fixture label bucket");
    assert_eq!(bucket.bucket_label_key(), label);
    bucket
}

fn collect_offset_rows(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
    order: OutEdgeOrder,
) -> Vec<(BucketEntryPosition, u32)> {
    let request = LabeledTraversalRequest {
        owner: src,
        label,
        order,
    };
    let mut rows = Vec::new();
    let result = Traversal::visit_edges(graph, &request, |slot, edge| {
        rows.push((slot, edge.0));
        std::ops::ControlFlow::<()>::Continue(())
    })
    .expect("canonical offset traversal");
    assert!(result.is_continue());
    rows
}

fn collect_offset_window_rows(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    request: &LabeledTraversalRequest,
    window: TraversalWindow,
) -> (Vec<(BucketEntryPosition, u32)>, std::ops::ControlFlow<()>) {
    let mut rows = Vec::new();
    let result = Traversal::visit_edges_window(graph, request, window, |slot, edge| {
        rows.push((slot, edge.0));
        std::ops::ControlFlow::<()>::Continue(())
    })
    .expect("canonical offset window traversal");
    (rows, result)
}

fn assert_offset_window_contract(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
    truth: &[(BucketEntryPosition, u32)],
) {
    for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
        let request = LabeledTraversalRequest {
            owner: src,
            label,
            order,
        };
        let expected = if order == OutEdgeOrder::Ascending {
            truth.to_vec()
        } else {
            truth.iter().copied().rev().collect()
        };
        let (rows, result) = collect_offset_window_rows(
            graph,
            &request,
            TraversalWindow::new(OFFSET_WINDOW_OFFSET, Some(OFFSET_WINDOW_LIMIT)),
        );
        assert_eq!(result, std::ops::ControlFlow::Continue(()));
        assert_eq!(rows, expected[960..992]);
        assert!(
            rows.iter()
                .all(|(_, target)| *target != u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
        );

        let mut calls = 0u32;
        let result = Traversal::visit_edges_window(
            graph,
            &request,
            TraversalWindow::new(0, Some(0)),
            |_slot, _edge| {
                calls += 1;
                std::ops::ControlFlow::<()>::Continue(())
            },
        )
        .expect("zero-limit window");
        assert_eq!(result, std::ops::ControlFlow::Continue(()));
        assert_eq!(calls, 0);

        for offset in [31usize, 32, 33] {
            let (rows, result) = collect_offset_window_rows(
                graph,
                &request,
                TraversalWindow::new(offset as u32, Some(1)),
            );
            assert_eq!(result, std::ops::ControlFlow::Continue(()));
            assert_eq!(rows, expected[offset..offset + 1]);
        }
    }
}

fn build_dense_offset_graph(
    extent: u32,
) -> (
    LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    VertexId,
    BucketLabelKey,
) {
    assert!(OFFSET_EXTENTS.contains(&extent));
    let graph = bench_graph(OFFSET_EDGE_CAPACITY, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for slot in 0..extent {
        graph
            .insert_edge(
                src,
                label,
                BenchEdge(offset_target(slot)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    graph.compact_vertex_edge_span(src, 0).unwrap();
    let bucket = offset_bucket(&graph, src, label);
    assert_eq!(bucket.degree(), extent);
    assert_eq!(bucket.stored_slots, extent);
    assert!(bucket.overflow_log_head() < 0);
    assert_eq!(graph.vertices().get(src).stored_slots, extent);
    (graph, src, label)
}

fn collect_offset_truth(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
    extent: u32,
    stride: u32,
) -> Vec<(BucketEntryPosition, u32)> {
    let bucket = offset_bucket(graph, src, label);
    assert_eq!(bucket.degree(), OFFSET_LIVE_ROWS);
    assert_eq!(bucket.stored_slots, extent);
    assert!(bucket.overflow_log_head() < 0);
    let mut tombstones = 0u32;
    for slot in 0..extent {
        let edge = graph
            .edges()
            .read_slot(bucket.edge_start() + u64::from(slot));
        if edge.is_tombstone_edge() || edge.is_deleted_slot() {
            tombstones += 1;
        } else {
            assert_eq!(edge.0, offset_target(slot));
        }
    }
    assert_eq!(tombstones, extent - OFFSET_LIVE_ROWS);
    assert_eq!(graph.vertices().get(src).stored_slots, extent);

    let ascending = collect_offset_rows(graph, src, label, OutEdgeOrder::Ascending);
    assert_eq!(ascending.len(), OFFSET_LIVE_ROWS as usize);
    assert!(
        ascending
            .iter()
            .enumerate()
            .all(|(ordinal, (slot, target))| {
                slot.raw() % stride == 0
                    && *target == offset_target(slot.raw())
                    && *target == offset_target(ordinal as u32 * stride)
            })
    );
    let descending = collect_offset_rows(graph, src, label, OutEdgeOrder::Descending);
    let expected_descending: Vec<_> = ascending.iter().copied().rev().collect();
    assert_eq!(descending, expected_descending);

    assert_offset_window_contract(graph, src, label, &ascending);
    ascending
}

fn build_offset_fixture(extent: u32, stride: u32) -> OffsetFixture {
    assert_eq!(extent / stride, OFFSET_LIVE_ROWS);
    let (graph, src, label) = build_dense_offset_graph(extent);
    for slot in 0..extent {
        if slot % stride != 0 {
            assert!(
                graph
                    .remove_edge_at_slot(src, label, slot)
                    .unwrap()
                    .is_some()
            );
        }
    }
    let truth = collect_offset_truth(&graph, src, label, extent, stride);
    OffsetFixture {
        graph,
        src,
        label,
        truth,
    }
}

fn visit_offset_window(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    request: &LabeledTraversalRequest,
    window: TraversalWindow,
) -> (std::ops::ControlFlow<()>, u32, u64) {
    let mut count = 0u32;
    let mut checksum = 0u64;
    let result = Traversal::visit_edges_window(graph, request, window, |slot, edge| {
        count += 1;
        checksum = checksum
            .wrapping_add(u64::from(slot.raw()).wrapping_mul(31))
            .wrapping_add(u64::from(edge.0));
        black_box((slot.raw(), edge.0));
        std::ops::ControlFlow::<()>::Continue(())
    })
    .expect("measured offset window traversal");
    (result, count, checksum)
}

fn expected_offset_window_rows(
    ascending: &[(BucketEntryPosition, u32)],
    order: OutEdgeOrder,
    offset: u32,
) -> Vec<(BucketEntryPosition, u32)> {
    let offset = offset as usize;
    let limit = OFFSET_WINDOW_LIMIT as usize;
    match order {
        OutEdgeOrder::Ascending => ascending[offset..offset + limit].to_vec(),
        OutEdgeOrder::Descending => ascending
            .iter()
            .rev()
            .skip(offset)
            .take(limit)
            .copied()
            .collect(),
    }
}

fn assert_production_window_rows(
    fixture: &OffsetFixture,
    request: &LabeledTraversalRequest,
    window: TraversalWindow,
) {
    let expected = expected_offset_window_rows(&fixture.truth, request.order, window.offset);
    let (rows, control) = collect_offset_window_rows(&fixture.graph, request, window);
    assert_eq!(control, std::ops::ControlFlow::Continue(()));
    assert_eq!(rows, expected);
    assert_eq!(rows.len(), OFFSET_WINDOW_LIMIT as usize);
    assert!(
        rows.iter()
            .all(|(_, target)| *target != u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
    );
}

fn offset_query_bench(
    extent: u32,
    stride: u32,
    offset: u32,
    order: OutEdgeOrder,
) -> canbench_rs::BenchResult {
    let fixture = build_offset_fixture(extent, stride);
    let request = LabeledTraversalRequest {
        owner: fixture.src,
        label: fixture.label,
        order,
    };
    let window = TraversalWindow::new(offset, Some(OFFSET_WINDOW_LIMIT));
    assert_production_window_rows(&fixture, &request, window);
    let result = bench_fn(|| {
        let (control, count, checksum) = visit_offset_window(&fixture.graph, &request, window);
        black_box((control.is_continue(), count, checksum));
    });
    assert_eq!(
        fixture.graph.vertices().get(fixture.src).stored_slots,
        extent
    );
    result
}

fn inline_property_bench_graph(
    elem_capacity: u64,
    default_label: BucketLabelKey,
) -> LabeledLaraGraph<InlinePropertyBenchEdge, crate::VectorMemory> {
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
        inline_property_bytes_slab,
        value_free_spans,
        value_free_span_by_start,
        inline_property_bytes_log,
        value_blob,
        ltb,
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
        inline_property_bytes_slab,
        value_free_spans,
        value_free_span_by_start,
        inline_property_bytes_log,
        value_blob,
        ltb,
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        default_label,
    )
    .expect("graph")
}

fn bench_graph(
    elem_capacity: u64,
    default_label: BucketLabelKey,
) -> LabeledLaraGraph<BenchEdge, crate::VectorMemory> {
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
        inline_property_bytes_slab,
        value_free_spans,
        value_free_span_by_start,
        inline_property_bytes_log,
        value_blob,
        ltb,
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
        inline_property_bytes_slab,
        value_free_spans,
        value_free_span_by_start,
        inline_property_bytes_log,
        value_blob,
        ltb,
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        default_label,
    )
    .expect("graph")
}

fn reverse_bench_graph<E>() -> DeferredBidirectionalLabeledLaraGraph<E, crate::VectorMemory>
where
    E: CsrEdge + CsrEdgeTombstone,
{
    let (
        fv,
        fb,
        fbf,
        fbfs,
        fec,
        fe,
        fel,
        fsm,
        fefs,
        fefbs,
        fvs,
        fpfs,
        fpfbs,
        fpl,
        fpb,
        forward_ltb,
    ) = labeled_lara_memories();
    let (
        rv,
        rb,
        rbf,
        rbfs,
        rec,
        re,
        rel,
        rsm,
        refs,
        refbs,
        rvs,
        rpfs,
        rpfbs,
        rpl,
        rpb,
        reverse_ltb,
    ) = labeled_lara_memories();
    DeferredBidirectionalLabeledLaraGraph::new(
        fv,
        fb,
        fbf,
        fbfs,
        fec,
        fe,
        fel,
        fsm,
        fefs,
        fefbs,
        fvs,
        fpfs,
        fpfbs,
        fpl,
        fpb,
        forward_ltb,
        rv,
        rb,
        rbf,
        rbfs,
        rec,
        re,
        rel,
        rsm,
        refs,
        refbs,
        rvs,
        rpfs,
        rpfbs,
        rpl,
        rpb,
        reverse_ltb,
        crate::VectorMemory::default(),
        crate::labeled::InitialCapacities::uniform(1 << 16),
        BucketLabelKey::from_raw(1),
    )
    .expect("bidirectional benchmark graph")
}

/// Reverse/incoming dense scan through the bidirectional adapter.
#[bench(raw)]
fn bench_t_v_in_dense() -> canbench_rs::BenchResult {
    let graph = reverse_bench_graph::<BenchEdge>();
    let src = VertexId::from(0);
    let dst = VertexId::from(1);
    graph.push_vertex().unwrap();
    graph.push_vertex().unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph
            .insert_directed_edge(
                src,
                dst,
                label,
                BenchEdge(i + 10),
                BenchEdge(0),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    bench_fn(|| {
        let mut count = 0u32;
        graph
            .for_each_in_edges_for_label_ordered(dst, label, OutEdgeOrder::Ascending, |_edge| {
                count += 1;
            })
            .unwrap();
        black_box(count);
    })
}

/// Reverse/incoming hybrid scan with inline-property inline property batches.
#[bench(raw)]
fn bench_t_v_in_hyb_ip() -> canbench_rs::BenchResult {
    let graph = reverse_bench_graph::<InlinePropertyBenchEdge>();
    let src = VertexId::from(0);
    let dst = VertexId::from(1);
    graph.push_vertex().unwrap();
    graph.push_vertex().unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_directed_edge_inline_property_width(src, dst, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        let value = u64::from(i).to_le_bytes();
        graph
            .insert_directed_edge(
                src,
                dst,
                label,
                InlinePropertyBenchEdge::new(dst.into(), u64::from(i)),
                InlinePropertyBenchEdge::new(src.into(), u64::from(i)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        black_box(value);
    }
    bench_fn(|| {
        let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
        let mut count = 0u32;
        graph
            .visit_in_inline_property_batches_for_label(
                dst,
                label,
                OutEdgeOrder::Descending,
                &mut scratch,
                |batch| count += batch.slot_indices.len() as u32,
            )
            .unwrap();
        black_box(count);
    })
}

/// Dense single-label bucket with all edges on the slab.
#[bench(raw)]
fn bench_t_v_dense_a() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph
            .insert_edge(
                src,
                label,
                BenchEdge(i + 10),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    bench_fn(|| {
        let mut count = 0u32;
        let _ = graph
            .visit_edges(src, label, OutEdgeOrder::Ascending, |_slot, _edge| {
                count += 1;
                std::ops::ControlFlow::<()>::Continue(())
            })
            .unwrap();
        black_box(count);
    })
}

/// Sparse scan with tombstones retained in the bucket logical extent.
#[bench(raw)]
fn bench_t_v_sparse() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph
            .insert_edge(
                src,
                label,
                BenchEdge(i + 10),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    graph.compact_vertex_edge_span(src, 0).unwrap();
    for slot in (1..DENSE_DEGREE).step_by(3) {
        graph.remove_edge_at_slot(src, label, slot).unwrap();
    }
    bench_fn(|| {
        let mut count = 0u32;
        let _ = graph
            .visit_edges(src, label, OutEdgeOrder::Ascending, |_slot, _edge| {
                count += 1;
                std::ops::ControlFlow::<()>::Continue(())
            })
            .unwrap();
        black_box(count);
    })
}

/// Windowed dense scan used by OFFSET/LIMIT pushdown candidates.
#[bench(raw)]
fn bench_t_v_window() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph
            .insert_edge(
                src,
                label,
                BenchEdge(i + 10),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    let request = LabeledTraversalRequest {
        owner: src,
        label,
        order: OutEdgeOrder::Ascending,
    };
    bench_fn(|| {
        let mut count = 0u32;
        let _ = Traversal::visit_edges_window(
            &graph,
            &request,
            TraversalWindow::new(64, Some(32)),
            |_slot, _edge| {
                count += 1;
                std::ops::ControlFlow::<()>::Continue(())
            },
        )
        .unwrap();
        black_box(count);
    })
}

/// Explicit descending dense scan (opt-in order, counterpart to the ascending default).
#[bench(raw)]
fn bench_t_v_dense_d() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph
            .insert_edge(
                src,
                label,
                BenchEdge(i + 10),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    bench_fn(|| {
        let mut count = 0u32;
        let _ = graph
            .visit_edges(src, label, OutEdgeOrder::Descending, |_slot, _edge| {
                count += 1;
                std::ops::ControlFlow::<()>::Continue(())
            })
            .unwrap();
        black_box(count);
    })
}

/// Default-label bypass scan with zero inline-property width.
#[bench(raw)]
fn bench_t_v_bypass() -> canbench_rs::BenchResult {
    let default_label = BucketLabelKey::from_raw(1);
    let graph = bench_graph(4096, default_label);
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    for i in 0..BYPASS_DEGREE {
        graph
            .insert_edge(
                src,
                default_label,
                BenchEdge(i + 10),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    bench_fn(|| {
        let mut count = 0u32;
        let _ = graph
            .visit_edges(
                src,
                default_label,
                OutEdgeOrder::Ascending,
                |_slot, _edge| {
                    count += 1;
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        black_box(count);
    })
}

/// Early termination after a fixed number of matching rows.
#[bench(raw)]
fn bench_t_v_ebreak() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph
            .insert_edge(
                src,
                label,
                BenchEdge(i + 10),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    bench_fn(|| {
        let result = graph
            .visit_edges(src, label, OutEdgeOrder::Ascending, |slot, _edge| {
                if slot.raw() == 16 {
                    std::ops::ControlFlow::Break(())
                } else {
                    std::ops::ControlFlow::Continue(())
                }
            })
            .unwrap();
        let _ = black_box(result);
    })
}

/// Hybrid bucket: slab prefix plus overflow-log suffix.
#[bench(raw)]
fn bench_t_v_hyb() -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyBenchEdge::new(i + 10, u64::from(i)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    bench_fn(|| {
        let mut count = 0u32;
        let _ = graph
            .visit_edges(src, label, OutEdgeOrder::Descending, |_slot, _edge| {
                count += 1;
                std::ops::ControlFlow::<()>::Continue(())
            })
            .unwrap();
        black_box(count);
    })
}

/// Selected-slot topology read reusing a cached hybrid overflow replay.
#[bench(raw)]
fn bench_t_v_at_replay() -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyBenchEdge::new(i + 10, u64::from(i)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
    graph
        .visit_out_inline_property_batches_for_label(
            src,
            label,
            OutEdgeOrder::Ascending,
            &mut scratch,
            |_batch| {},
        )
        .unwrap();
    let selected: Vec<BucketEntryPosition> = (0..DIRECT_SELECTED_SLOT_COUNT as u32)
        .map(BucketEntryPosition::new)
        .collect();
    bench_fn(|| {
        let mut count = 0u32;
        let _ = graph
            .visit_edges_at_with_replay(
                src,
                label,
                &selected,
                OutEdgeOrder::Ascending,
                Some(&scratch.hybrid_overflow_replay),
                |_slot, _edge| {
                    count += 1;
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        black_box(count);
    })
}

fn bench_selected_inline_property_case(
    selected: Vec<BucketEntryPosition>,
    order: OutEdgeOrder,
) -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyBenchEdge::new(i + 10, u64::from(i)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    bench_fn(|| {
        let mut count = 0u32;
        let _ = graph
            .visit_edges_at_with_inline_property::<()>(
                src,
                label,
                &selected,
                order,
                |_slot, item| {
                    count += u32::from(item.inline_property.width == INLINE_VALUE_WIDTH);
                    black_box(item.inline_property.bytes());
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        black_box(count);
    })
}

/// Selected prefix: canonical scan should stop immediately after the requested range.
#[bench(raw)]
fn bench_t_vip() -> canbench_rs::BenchResult {
    bench_selected_inline_property_case(
        (0..DIRECT_SELECTED_SLOT_COUNT as u32)
            .map(BucketEntryPosition::new)
            .collect(),
        OutEdgeOrder::Ascending,
    )
}

/// Selected suffix: direct reads should avoid scanning the prefix of the bucket.
#[bench(raw)]
fn bench_t_vip_tail() -> canbench_rs::BenchResult {
    bench_selected_inline_property_case(
        ((HYBRID_DEGREE - DIRECT_SELECTED_SLOT_COUNT as u32)..HYBRID_DEGREE)
            .map(BucketEntryPosition::new)
            .collect(),
        OutEdgeOrder::Ascending,
    )
}

/// Sparse selection: direct reads should avoid scanning the gaps between requested slots.
#[bench(raw)]
fn bench_t_vip_sparse() -> canbench_rs::BenchResult {
    bench_selected_inline_property_case(
        [0, HYBRID_DEGREE / 2, HYBRID_DEGREE - 1]
            .into_iter()
            .map(BucketEntryPosition::new)
            .collect(),
        OutEdgeOrder::Ascending,
    )
}

/// Descending selected suffix: canonical scan should stop after the requested range.
#[bench(raw)]
fn bench_t_vip_descending() -> canbench_rs::BenchResult {
    bench_selected_inline_property_case(
        ((HYBRID_DEGREE - DIRECT_SELECTED_SLOT_COUNT as u32)..HYBRID_DEGREE)
            .map(BucketEntryPosition::new)
            .collect(),
        OutEdgeOrder::Descending,
    )
}

/// Descending selected prefix: direct reads should avoid scanning the remaining bucket.
#[bench(raw)]
fn bench_t_vip_descending_tail() -> canbench_rs::BenchResult {
    bench_selected_inline_property_case(
        (0..DIRECT_SELECTED_SLOT_COUNT as u32)
            .map(BucketEntryPosition::new)
            .collect(),
        OutEdgeOrder::Descending,
    )
}

/// Reference cost of filtering the full inline-property visitor for the same selected prefix.
#[bench(raw)]
fn bench_t_vip_filtered_reference() -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyBenchEdge::new(i + 10, u64::from(i)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    let selected: Vec<BucketEntryPosition> = (0..DIRECT_SELECTED_SLOT_COUNT as u32)
        .map(BucketEntryPosition::new)
        .collect();
    bench_fn(|| {
        let mut selected_index = 0usize;
        let mut count = 0u32;
        let _ = graph
            .visit_edges_with_inline_property::<()>(
                src,
                label,
                OutEdgeOrder::Ascending,
                |slot, item| {
                    if selected_index < selected.len()
                        && selected[selected_index].raw() == slot.raw()
                    {
                        selected_index += 1;
                        count += u32::from(item.inline_property.width == INLINE_VALUE_WIDTH);
                        black_box(item.inline_property.bytes());
                    }
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        black_box((selected_index, count));
    })
}

/// Streaming visitor that attaches exact inline-property bytes per edge.
#[bench(raw)]
fn bench_t_v_w_ip() -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyBenchEdge::new(i + 10, u64::from(i)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    bench_fn(|| {
        let mut count = 0u32;
        let _ = graph
            .visit_edges_with_inline_property::<()>(
                src,
                label,
                OutEdgeOrder::Ascending,
                |_slot, item| {
                    count += u32::from(
                        item.inline_property.width == item.inline_property.bytes().len() as u16,
                    );
                    black_box(item.inline_property.bytes());
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        black_box(count);
    })
}

/// Legacy traversal baseline on the identical 256-edge hybrid inline property fixture.
///
/// This is intentionally kept beside the candidate benchmark so both closures share the same
/// graph type, edge count, label, inline width, and build conditions.
#[bench(raw)]
fn bench_t_lvp_same() -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyBenchEdge::new(i + 10, u64::from(i)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    bench_fn(|| {
        let mut count = 0u32;
        graph
            .for_each_edges_for_label_ordered(src, label, OutEdgeOrder::Ascending, |edge| {
                count += u32::from(edge.edge_inline_property_byte_width() == INLINE_VALUE_WIDTH);
                black_box(edge.edge_inline_property_bytes());
            })
            .unwrap();
        black_box(count);
    })
}

/// Property-first batch read, followed by selected-slot topology read.
#[bench(raw)]
fn bench_t_v_pf_then_sel() -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyBenchEdge::new(i + 10, u64::from(i)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    bench_fn(|| {
        let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
        let mut selected = Vec::new();
        graph
            .visit_out_inline_property_batches_for_label(
                src,
                label,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    selected.extend(
                        batch
                            .slot_indices
                            .iter()
                            .copied()
                            .map(BucketEntryPosition::new),
                    )
                },
            )
            .unwrap();
        let mut count = 0u32;
        let _ = graph
            .visit_edges_at_with_replay(
                src,
                label,
                &selected,
                OutEdgeOrder::Ascending,
                Some(&scratch.hybrid_overflow_replay),
                |_slot, _edge| {
                    count += 1;
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        black_box((count, selected.len()));
    })
}

/// Fixed-survivor OFFSET matrix: dense control at the first live ordinal.
#[bench(raw)]
fn bench_t_off_l1k_e1k_0_a() -> canbench_rs::BenchResult {
    offset_query_bench(1_024, 1, 0, OutEdgeOrder::Ascending)
}

/// Fixed-survivor OFFSET matrix: dense OFFSET query.
#[bench(raw)]
fn bench_t_off_l1k_e1k_960_a() -> canbench_rs::BenchResult {
    offset_query_bench(1_024, 1, OFFSET_WINDOW_OFFSET, OutEdgeOrder::Ascending)
}

/// Fixed-survivor OFFSET matrix: 50% tombstones at the first live ordinal.
#[bench(raw)]
fn bench_t_off_l1k_e2k_0_a() -> canbench_rs::BenchResult {
    offset_query_bench(2_048, 2, 0, OutEdgeOrder::Ascending)
}

/// Fixed-survivor OFFSET matrix: 50% tombstones at live ordinal 960.
#[bench(raw)]
fn bench_t_off_l1k_e2k_960_a() -> canbench_rs::BenchResult {
    offset_query_bench(2_048, 2, OFFSET_WINDOW_OFFSET, OutEdgeOrder::Ascending)
}

/// Fixed-survivor OFFSET matrix: 75% tombstones at the first live ordinal.
#[bench(raw)]
fn bench_t_off_l1k_e4k_0_a() -> canbench_rs::BenchResult {
    offset_query_bench(4_096, 4, 0, OutEdgeOrder::Ascending)
}

/// Fixed-survivor OFFSET matrix: 75% tombstones at live ordinal 960.
#[bench(raw)]
fn bench_t_off_l1k_e4k_960_a() -> canbench_rs::BenchResult {
    offset_query_bench(4_096, 4, OFFSET_WINDOW_OFFSET, OutEdgeOrder::Ascending)
}

/// Fixed-survivor OFFSET matrix: 87.5% tombstones at the first live ordinal.
#[bench(raw)]
fn bench_t_off_l1k_e8k_0_a() -> canbench_rs::BenchResult {
    offset_query_bench(8_192, 8, 0, OutEdgeOrder::Ascending)
}

/// Fixed-survivor OFFSET matrix: 87.5% tombstones at live ordinal 960.
#[bench(raw)]
fn bench_t_off_l1k_e8k_960_a() -> canbench_rs::BenchResult {
    offset_query_bench(8_192, 8, OFFSET_WINDOW_OFFSET, OutEdgeOrder::Ascending)
}

/// Descending dense control for the fixed-survivor OFFSET matrix.
#[bench(raw)]
fn bench_t_off_l1k_e1k_0_d() -> canbench_rs::BenchResult {
    offset_query_bench(1_024, 1, 0, OutEdgeOrder::Descending)
}

/// Descending 87.5% tombstone control for the fixed-survivor OFFSET matrix.
#[bench(raw)]
fn bench_t_off_l1k_e8k_0_d() -> canbench_rs::BenchResult {
    offset_query_bench(8_192, 8, 0, OutEdgeOrder::Descending)
}
