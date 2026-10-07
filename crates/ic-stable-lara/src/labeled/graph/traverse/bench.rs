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
// Plan 0336: shared with the sibling `bench_workload` module (bench-scope only).
pub(super) struct BenchEdge(pub(super) u32);

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
struct InlinePropertyBenchEdge<const W: usize = 8> {
    target: u32,
    inline_property: [u8; W],
}

impl<const W: usize> InlinePropertyBenchEdge<W> {
    fn new(target: u32, value: u64) -> Self {
        Self {
            target,
            inline_property: std::array::from_fn(|i| value.to_le_bytes()[i % 8]),
        }
    }
}

impl<const W: usize> CsrEdge for InlinePropertyBenchEdge<W> {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            inline_property: [0u8; W],
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
        W as u16
    }

    fn edge_inline_property_bytes(&self) -> &[u8] {
        &self.inline_property
    }

    fn with_stored_inline_property_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len()).min(W);
        self.inline_property = [0u8; W];
        self.inline_property[..len].copy_from_slice(&bytes[..len]);
        self
    }
}

impl<const W: usize> CsrEdgeTombstone for InlinePropertyBenchEdge<W> {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            inline_property: [0u8; W],
        }
    }
}

const OFFSET_LIVE_ROWS: u32 = 1_024;
pub(super) const OFFSET_WINDOW_OFFSET: u32 = 960;
pub(super) const OFFSET_WINDOW_LIMIT: u32 = 32;
const OFFSET_EDGE_CAPACITY: u64 = 16_384;
const OFFSET_TARGET_BASE: u32 = 10_000;
const OFFSET_EXTENTS: [u32; 4] = [1_024, 2_048, 4_096, 8_192];

// Plan 0336: bench-scope constants shared with `bench_workload`.
pub(super) const OFFSET_WORKLOAD_EXTENT: u32 = 8_192;
pub(super) const OFFSET_WORKLOAD_STRIDE: u32 = 8;

struct OffsetFixture {
    graph: LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
    truth: Vec<(BucketEntryPosition, u32)>,
}

fn offset_target(slot: u32) -> u32 {
    OFFSET_TARGET_BASE + slot
}

// Plan 0336: bench-scope helpers shared with `bench_workload`.
pub(super) fn workload_target(slot: u32) -> u32 {
    offset_target(slot)
}

pub(super) fn workload_dense_graph(
    extent: u32,
) -> (
    LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    VertexId,
    BucketLabelKey,
) {
    build_dense_offset_graph(extent)
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

/// Expected window rows under the tombstone-inclusive position contract:
/// the window covers request-order positions `[offset, offset + limit)`
/// over the bucket's full slot extent; ascending position == slot id,
/// descending position == `extent - 1 - slot`.
fn truth_position_window(
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
    truth: &[(BucketEntryPosition, u32)],
    order: OutEdgeOrder,
    offset: u32,
    limit: u32,
) -> Vec<(BucketEntryPosition, u32)> {
    let extent = offset_bucket(graph, src, label).stored_slots();
    let (lo, hi) = match order {
        OutEdgeOrder::Ascending => (u64::from(offset), u64::from(offset) + u64::from(limit)),
        OutEdgeOrder::Descending => (
            u64::from(extent).saturating_sub(u64::from(offset) + u64::from(limit)),
            u64::from(extent).saturating_sub(u64::from(offset)),
        ),
    };
    let mut rows: Vec<_> = truth
        .iter()
        .filter(|(position, _)| {
            let raw = u64::from(position.raw());
            raw >= lo && raw < hi
        })
        .copied()
        .collect();
    if order == OutEdgeOrder::Descending {
        rows.reverse();
    }
    rows
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
        // Plan 0327 polish: the window cuts the tombstone-inclusive
        // position space in the requested order, so the expected rows are
        // the truth rows whose positions fall inside the window
        // (descending maps positions to the top of the slot range).
        let expected = truth_position_window(
            graph,
            src,
            label,
            truth,
            order,
            OFFSET_WINDOW_OFFSET,
            OFFSET_WINDOW_LIMIT,
        );
        let (rows, result) = collect_offset_window_rows(
            graph,
            &request,
            TraversalWindow::new(OFFSET_WINDOW_OFFSET, Some(OFFSET_WINDOW_LIMIT)),
        );
        assert_eq!(result, std::ops::ControlFlow::Continue(()));
        assert_eq!(rows, expected);
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
            let expected = truth_position_window(graph, src, label, truth, order, offset as u32, 1);
            let (rows, result) = collect_offset_window_rows(
                graph,
                &request,
                TraversalWindow::new(offset as u32, Some(1)),
            );
            assert_eq!(result, std::ops::ControlFlow::Continue(()));
            assert_eq!(rows, expected);
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
    assert_eq!(bucket.stored_slots(), extent);
    assert!(bucket.edge_spill_run() == crate::traits::NO_SPILL_RUN);
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
    assert_eq!(bucket.stored_slots(), extent);
    assert!(bucket.edge_spill_run() == crate::traits::NO_SPILL_RUN);
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
    graph: &LabeledLaraGraph<BenchEdge, crate::VectorMemory>,
    src: VertexId,
    label: BucketLabelKey,
    ascending: &[(BucketEntryPosition, u32)],
    order: OutEdgeOrder,
    offset: u32,
) -> Vec<(BucketEntryPosition, u32)> {
    truth_position_window(
        graph,
        src,
        label,
        ascending,
        order,
        offset,
        OFFSET_WINDOW_LIMIT,
    )
}

fn assert_production_window_rows(
    fixture: &OffsetFixture,
    request: &LabeledTraversalRequest,
    window: TraversalWindow,
) {
    let expected = expected_offset_window_rows(
        &fixture.graph,
        fixture.src,
        fixture.label,
        &fixture.truth,
        request.order,
        window.offset,
    );
    let (rows, control) = collect_offset_window_rows(&fixture.graph, request, window);
    assert_eq!(control, std::ops::ControlFlow::Continue(()));
    assert_eq!(rows, expected);
    assert!(!rows.is_empty());
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
    E: CsrEdge + CsrEdgeTombstone + PartialEq,
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

/// Selected-slot topology read after an untimed hybrid value scan.
#[bench(raw)]
fn bench_t_v_at_after_values() -> canbench_rs::BenchResult {
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
            .visit_edges_at(
                src,
                label,
                &selected,
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

fn bounded_inline_bench(degree: u32, value_bytes: usize) -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..degree {
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                InlinePropertyBenchEdge::new(i + 10, u64::from(i)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    let crate::labeled::graph::BucketSearch::Found { bucket, .. } = graph
        .find_bucket(src, &graph.vertices().get(src), label)
        .unwrap()
    else {
        panic!("missing bucket")
    };
    assert!(!bucket.is_tree_mode());
    // ADR 0097 §5: a bucket's rows are its slab prefix plus its own spill run, so the prefix is
    // not required to cover `degree` any more. What must hold is that every row is reachable.
    let edge_run_rows = match graph.edges().spill_mint(bucket.edge_spill_run()) {
        Ok(run) => graph.edges().spill_used_len(run),
        Err(_) => 0,
    };
    assert_eq!(bucket.degree(), degree);
    assert_eq!(bucket.stored_slots_raw() + edge_run_rows, degree);
    // Setup-time check via the public batch API: collect all rows and
    // verify content once (the measured closure below re-runs the same call).
    let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
    let mut check_rows: Vec<(u32, u32)> = Vec::new();
    graph
        .visit_out_inline_property_batches_for_label(
            src,
            label,
            crate::labeled::graph::OutEdgeOrder::Ascending,
            &mut scratch,
            |batch| {
                assert_eq!(batch.byte_width, INLINE_VALUE_WIDTH);
                for (slot, value_lo) in batch.slot_indices.iter().zip(
                    batch
                        .values
                        .as_chunks::<8>()
                        .0
                        .iter()
                        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]])),
                ) {
                    check_rows.push((*slot, value_lo));
                }
            },
        )
        .expect("batch visit");
    // Batch yields (slot, value-bytes); edge targets live in the topology
    // domain (checked by scan tests). Batches are per-segment (slab chunks,
    // log suffix), so assert per-row identity after sorting, not global order:
    // slot s carries value u64 LE of s.
    assert_eq!(check_rows.len(), degree as usize, "complete inline read");
    check_rows.sort_unstable();
    for (i, (slot, value_lo)) in check_rows.into_iter().enumerate() {
        assert_eq!(slot, i as u32);
        assert_eq!(value_lo, i as u32);
    }
    // NOTE: the byte-cap variant (`value_bytes < needed`) previously asserted a
    // `ReadByteLimitExceeded` typed error from a bounded-collect API that was
    // removed before landing (uncommitted R2b matrix scaffolding; the bounded
    // API never existed on HEAD). The cap contract lives in unit tests; the
    // bench measures the complete read.
    let _ = value_bytes;
    bench_fn(|| {
        // Include result allocation and destruction, but not fixture setup/assertions.
        let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
        let mut n = 0u32;
        graph
            .visit_out_inline_property_batches_for_label(
                src,
                label,
                crate::labeled::graph::OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    n += batch.slot_indices.len() as u32;
                },
            )
            .expect("batch visit");
        black_box(n);
    })
}

/// Complete eight-byte inline values from a small slab.
#[bench(raw)]
fn bounded_inline_64_full() -> canbench_rs::BenchResult {
    bounded_inline_bench(64, 64 * 8)
}

/// Complete eight-byte inline values from a promotion-sized slab.
///
/// "Promotion-sized" is `T_promote` rows, not a literal: the fixture asserts
/// slab mode, and a slab bucket cannot exceed the tuned threshold (this bench
/// trapped once the threshold moved to 1024).
#[bench(raw)]
fn bounded_inline_promote_full() -> canbench_rs::BenchResult {
    let degree = crate::labeled::graph::T_PROMOTE;
    bounded_inline_bench(degree, degree as usize * 8)
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
            .visit_edges_at(
                src,
                label,
                &selected,
                OutEdgeOrder::Ascending,
                |_slot, _edge| {
                    count += 1;
                    std::ops::ControlFlow::<()>::Continue(())
                },
            )
            .unwrap();
        black_box((count, selected.len()));
    })
}

/// Mutation fixtures must retain logical positions when deletion resolves live value ranks.
/// The target/value codec is shared with the fixed-width baseline edge above.
#[derive(Clone, Debug, PartialEq, Eq)]
struct RangeBenchEdge<const W: usize>(InlinePropertyBenchEdge<W>, u32);
impl<const W: usize> CsrEdge for RangeBenchEdge<W> {
    const BYTES: usize = InlinePropertyBenchEdge::<W>::BYTES;
    fn read_from(bytes: &[u8]) -> Self {
        Self(InlinePropertyBenchEdge::read_from(bytes), 0)
    }
    fn write_to(&self, bytes: &mut [u8]) {
        self.0.write_to(bytes);
    }
    fn neighbor_vid(&self) -> VertexId {
        self.0.neighbor_vid()
    }
    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self(self.0.with_neighbor_vid(vid), self.1)
    }
    fn with_slot_index(self, slot: u32) -> Self {
        Self(self.0, slot)
    }
    fn edge_slot_index_raw(&self) -> u32 {
        self.1
    }
    fn edge_inline_property_byte_width(&self) -> u16 {
        self.0.edge_inline_property_byte_width()
    }
    fn edge_inline_property_bytes(&self) -> &[u8] {
        self.0.edge_inline_property_bytes()
    }
    fn with_stored_inline_property_bytes(self, width: u16, bytes: &[u8]) -> Self {
        Self(
            self.0.with_stored_inline_property_bytes(width, bytes),
            self.1,
        )
    }
}
impl<const W: usize> CsrEdgeTombstone for RangeBenchEdge<W> {
    fn tombstone_edge() -> Self {
        Self(InlinePropertyBenchEdge::tombstone_edge(), 0)
    }
}

/// Same 256-slot fixture for value phase one, selected topology and selected hydration.
/// Scopes are separate operations, not additive components of one predicate query.
fn range_read_bench<const W: usize, M: ic_stable_structures::Memory>(
    memories: [M; 16],
    selection: std::ops::Range<u32>,
    stride: usize,
    holes: bool,
) -> canbench_rs::BenchResult {
    let [a, b, c, d, e, f, g, h, i, j, k, l, m, n, o, p] = memories;
    let graph = LabeledLaraGraph::<RangeBenchEdge<W>, M>::new(
        a,
        b,
        c,
        d,
        e,
        f,
        g,
        h,
        i,
        j,
        k,
        l,
        m,
        n,
        o,
        p,
        crate::labeled::InitialCapacities::uniform(4096),
        BucketLabelKey::from_raw(1),
    )
    .unwrap();
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    let add = |label, slot: u32| {
        graph
            .ensure_label_bucket_inline_property_byte_width(src, label, W as u16)
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                src,
                label,
                RangeBenchEdge(InlinePropertyBenchEdge::new(slot + 10, u64::from(slot)), 0),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    };
    for slot in 0..255 {
        if slot == 32 {
            add(BucketLabelKey::from_raw(3), 9000);
        }
        add(label, slot);
    }
    let dead: Vec<_> = if holes {
        (0..255).step_by(16).collect()
    } else {
        Vec::new()
    };
    for &slot in &dead {
        graph
            .remove_edge_at_slot(src, label, slot)
            .unwrap()
            .unwrap();
    }
    add(BucketLabelKey::from_raw(4), 9001);
    add(label, 255);
    let vertex = graph.vertices().get(src);
    let bucket = graph
        .buckets()
        .read_label_bucket_slot(graph.find_bucket_slot(&vertex, label).unwrap().unwrap())
        .unwrap();
    assert!(!bucket.is_tree_mode());
    assert_ne!(bucket.edge_spill_run(), crate::traits::NO_SPILL_RUN);
    assert_ne!(bucket.values_spill_run(), crate::traits::NO_SPILL_RUN);
    let selected: Vec<_> = selection
        .step_by(stride)
        .map(BucketEntryPosition::new)
        .collect();
    let expected: Vec<_> = selected
        .iter()
        .map(|s| s.raw())
        .filter(|s| !dead.contains(s))
        .collect();
    // Exact slot/target/value validation is outside the measured closure, in both orders.
    for order in [OutEdgeOrder::Ascending, OutEdgeOrder::Descending] {
        let mut seen = Vec::new();
        let _ = graph
            .visit_edges_at_with_inline_property(src, label, &selected, order, |slot, item| {
                assert_eq!(item.edge.0.target, slot.raw() + 10);
                assert_eq!(
                    item.inline_property.bytes(),
                    InlinePropertyBenchEdge::<W>::new(0, u64::from(slot.raw())).inline_property
                );
                seen.push(slot.raw());
                std::ops::ControlFlow::<()>::Continue(())
            })
            .unwrap();
        let mut want = expected.clone();
        if order == OutEdgeOrder::Descending {
            want.reverse();
        }
        assert_eq!(seen, want);
    }
    let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
    let mut values_seen = Vec::new();
    graph
        .visit_out_inline_property_batches_for_label(
            src,
            label,
            OutEdgeOrder::Descending,
            &mut scratch,
            |batch| {
                for (&slot, bytes) in batch
                    .slot_indices
                    .iter()
                    .zip(batch.values.as_chunks::<W>().0.iter())
                {
                    assert_eq!(
                        *bytes,
                        InlinePropertyBenchEdge::<W>::new(0, u64::from(slot)).inline_property
                    );
                    values_seen.push(slot);
                }
            },
        )
        .unwrap();
    assert_eq!(
        values_seen,
        (0..256)
            .rev()
            .filter(|s| !dead.contains(s))
            .collect::<Vec<_>>()
    );
    let mut topology_seen = Vec::new();
    let _ = graph
        .visit_edges_at(
            src,
            label,
            &selected,
            OutEdgeOrder::Ascending,
            |slot, edge| {
                assert_eq!(edge.0.target, slot.raw() + 10);
                topology_seen.push(slot.raw());
                std::ops::ControlFlow::<()>::Continue(())
            },
        )
        .unwrap();
    assert_eq!(topology_seen, expected);
    bench_fn(|| {
        {
            let _scope = canbench_rs::bench_scope("values");
            graph
                .visit_out_inline_property_batches_for_label(
                    src,
                    label,
                    OutEdgeOrder::Descending,
                    &mut scratch,
                    |batch| {
                        black_box((batch.slot_indices, batch.values));
                    },
                )
                .unwrap();
        }
        {
            let _scope = canbench_rs::bench_scope("selected");
            let _ = graph
                .visit_edges_at(
                    src,
                    label,
                    &selected,
                    OutEdgeOrder::Ascending,
                    |slot, edge| {
                        black_box((slot, edge));
                        std::ops::ControlFlow::<()>::Continue(())
                    },
                )
                .unwrap();
        }
        {
            let _scope = canbench_rs::bench_scope("hydrated");
            let _ = graph
                .visit_edges_at_with_inline_property(
                    src,
                    label,
                    &selected,
                    OutEdgeOrder::Descending,
                    |slot, item| {
                        black_box((slot, item.edge, item.inline_property.bytes()));
                        std::ops::ControlFlow::<()>::Continue(())
                    },
                )
                .unwrap();
        }
    })
}

fn stable_range_memories()
-> [ic_stable_structures::memory_manager::VirtualMemory<ic_stable_structures::DefaultMemoryImpl>;
    16] {
    use ic_stable_structures::memory_manager::{MemoryId, MemoryManager};
    let manager = MemoryManager::init(ic_stable_structures::DefaultMemoryImpl::default());
    std::array::from_fn(|i| manager.get(MemoryId::new(i as u8)))
}

macro_rules! range_case {
    ($name:ident, $width:literal, $memories:expr, $range:expr, $stride:literal, $holes:literal) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            range_read_bench::<$width, _>($memories, $range, $stride, $holes)
        }
    };
}
range_case!(
    bench_t_range_v_all8,
    8,
    std::array::from_fn(|_| crate::test_support::vector_memory()),
    0..256,
    1,
    false
);
range_case!(
    bench_t_range_v_point8,
    8,
    std::array::from_fn(|_| crate::test_support::vector_memory()),
    255..256,
    1,
    false
);
range_case!(
    bench_t_range_v_contig8,
    8,
    std::array::from_fn(|_| crate::test_support::vector_memory()),
    112..128,
    1,
    false
);
range_case!(
    bench_t_range_v_scattered8,
    8,
    std::array::from_fn(|_| crate::test_support::vector_memory()),
    0..256,
    16,
    false
);
range_case!(
    bench_t_range_v_holes8,
    8,
    std::array::from_fn(|_| crate::test_support::vector_memory()),
    0..256,
    15,
    true
);
range_case!(
    bench_t_range_v_all33,
    33,
    std::array::from_fn(|_| crate::test_support::vector_memory()),
    0..256,
    1,
    false
);
range_case!(
    bench_t_range_v_holes33,
    33,
    std::array::from_fn(|_| crate::test_support::vector_memory()),
    0..256,
    15,
    true
);
range_case!(
    bench_t_range_s_all8,
    8,
    stable_range_memories(),
    0..256,
    1,
    false
);
range_case!(
    bench_t_range_s_point8,
    8,
    stable_range_memories(),
    255..256,
    1,
    false
);
range_case!(
    bench_t_range_s_contig8,
    8,
    stable_range_memories(),
    112..128,
    1,
    false
);
range_case!(
    bench_t_range_s_scattered8,
    8,
    stable_range_memories(),
    0..256,
    16,
    false
);
range_case!(
    bench_t_range_s_holes8,
    8,
    stable_range_memories(),
    0..256,
    15,
    true
);
range_case!(
    bench_t_range_s_all33,
    33,
    stable_range_memories(),
    0..256,
    1,
    false
);
range_case!(
    bench_t_range_s_holes33,
    33,
    stable_range_memories(),
    0..256,
    15,
    true
);

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
