//! ADR 0050 Phase 2 dedicated benchmarks for the `traverse_next` read surface.
//!
//! These benchmarks measure the consolidated logical read API in isolation.
//! Each benchmark sets up its fixture outside the measured closure and then
//! exercises one canonical read primitive. Results are intended to be compared
//! with the corresponding legacy `traverse` benchmarks in `labeled/bench.rs`
//! under identical build settings.

use crate::labeled::{
    BucketLabelKey, DeferredBidirectionalLabeledLaraGraph, LabeledInlinePropertyValueBatchScratch,
    LabeledVertex, MateStorageMemories, OutEdgeOrder,
    graph::LabeledLaraGraph,
    graph::traverse_next::{BucketEntryPosition, LabeledTraversalRequest},
};
use crate::traverse::{Traversal, TraversalWindow};
use crate::{
    VertexId,
    test_support::labeled_lara_memories,
    traits::{CsrEdge, CsrEdgeTombstone},
};
use canbench_rs::{bench, bench_fn};
use std::hint::black_box;

const DENSE_DEGREE: u32 = 256;
const HYBRID_DEGREE: u32 = 256;
const BYPASS_DEGREE: u32 = 64;
const SELECTED_SLOT_COUNT: usize = 32;
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
struct PayloadBenchEdge {
    target: u32,
    payload: [u8; 8],
}

impl PayloadBenchEdge {
    fn new(target: u32, value: u64) -> Self {
        Self {
            target,
            payload: value.to_le_bytes(),
        }
    }
}

impl CsrEdge for PayloadBenchEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            payload: [0u8; 8],
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
            payload: self.payload,
        }
    }

    fn edge_inline_property_byte_width(&self) -> u16 {
        INLINE_VALUE_WIDTH
    }

    fn edge_inline_property_bytes(&self) -> &[u8] {
        &self.payload
    }

    fn with_stored_inline_property_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len()).min(8);
        self.payload = [0u8; 8];
        self.payload[..len].copy_from_slice(&bytes[..len]);
        self
    }
}

impl CsrEdgeTombstone for PayloadBenchEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            payload: [0u8; 8],
        }
    }
}

fn payload_bench_graph(
    elem_capacity: u64,
    default_label: BucketLabelKey,
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
        inline_property_bytes_slab,
        value_free_spans,
        value_free_span_by_start,
        inline_property_bytes_log,
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
        inline_property_bytes_slab,
        value_free_spans,
        value_free_span_by_start,
        inline_property_bytes_log,
        value_blob,
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
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        default_label,
    )
    .expect("graph")
}

fn reverse_bench_graph<E>() -> DeferredBidirectionalLabeledLaraGraph<E, crate::VectorMemory>
where
    E: CsrEdge + CsrEdgeTombstone,
{
    let (fv, fb, fbf, fbfs, fec, fe, fel, fsm, fefs, fefbs, fvs, fpfs, fpfbs, fpl, fpb) =
        labeled_lara_memories();
    let (rv, rb, rbf, rbfs, rec, re, rel, rsm, refs, refbs, rvs, rpfs, rpfbs, rpl, rpb) =
        labeled_lara_memories();
    let memories = MateStorageMemories::new(
        crate::VectorMemory::default(),
        crate::VectorMemory::default(),
        crate::VectorMemory::default(),
        crate::VectorMemory::default(),
    );
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
        memories,
        crate::VectorMemory::default(),
        crate::VectorMemory::default(),
        crate::labeled::InitialCapacities::uniform(1 << 16),
        BucketLabelKey::from_raw(1),
    )
    .expect("bidirectional benchmark graph")
}

/// Reverse/incoming dense scan through the bidirectional adapter.
#[bench(raw)]
fn bench_traverse_next_visit_in_edges_dense() -> canbench_rs::BenchResult {
    let graph = reverse_bench_graph::<BenchEdge>();
    let src = VertexId::from(0);
    let dst = VertexId::from(1);
    graph.push_vertex().unwrap();
    graph.push_vertex().unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph
            .insert_directed_edge(src, dst, label, BenchEdge(i + 10), BenchEdge(0))
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

/// Reverse/incoming hybrid scan with inline-property payload batches.
#[bench(raw)]
fn bench_traverse_next_visit_in_edges_hybrid_inline_property() -> canbench_rs::BenchResult {
    let graph = reverse_bench_graph::<PayloadBenchEdge>();
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
                PayloadBenchEdge::new(dst.into(), u64::from(i)),
                PayloadBenchEdge::new(src.into(), u64::from(i)),
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
fn bench_traverse_next_visit_edges_dense_asc() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph.insert_edge(src, label, BenchEdge(i + 10)).unwrap();
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
fn bench_traverse_next_visit_edges_sparse() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph.insert_edge(src, label, BenchEdge(i + 10)).unwrap();
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
fn bench_traverse_next_visit_edges_window() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph.insert_edge(src, label, BenchEdge(i + 10)).unwrap();
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

/// Descending dense scan, matching the default hot-path order.
#[bench(raw)]
fn bench_traverse_next_visit_edges_dense_desc() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph.insert_edge(src, label, BenchEdge(i + 10)).unwrap();
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
fn bench_traverse_next_visit_edges_bypass() -> canbench_rs::BenchResult {
    let default_label = BucketLabelKey::from_raw(1);
    let graph = bench_graph(4096, default_label);
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    for i in 0..BYPASS_DEGREE {
        graph
            .insert_edge(src, default_label, BenchEdge(i + 10))
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
fn bench_traverse_next_visit_edges_early_break() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    for i in 0..DENSE_DEGREE {
        graph.insert_edge(src, label, BenchEdge(i + 10)).unwrap();
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
fn bench_traverse_next_visit_edges_hybrid() -> canbench_rs::BenchResult {
    let graph = payload_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(src, label, PayloadBenchEdge::new(i + 10, u64::from(i)))
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
fn bench_traverse_next_visit_edges_at_with_replay() -> canbench_rs::BenchResult {
    let graph = payload_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(src, label, PayloadBenchEdge::new(i + 10, u64::from(i)))
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
    let selected: Vec<BucketEntryPosition> = (0..SELECTED_SLOT_COUNT as u32)
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

/// Streaming visitor that attaches exact inline-property bytes per edge.
#[bench(raw)]
fn bench_traverse_next_visit_edges_with_inline_property() -> canbench_rs::BenchResult {
    let graph = payload_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(src, label, PayloadBenchEdge::new(i + 10, u64::from(i)))
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

/// Legacy traversal baseline on the identical 256-edge hybrid payload fixture.
///
/// This is intentionally kept beside the candidate benchmark so both closures share the same
/// graph type, edge count, label, inline width, and build conditions.
#[bench(raw)]
fn bench_traverse_next_legacy_visit_edges_with_inline_property_same_fixture()
-> canbench_rs::BenchResult {
    let graph = payload_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(src, label, PayloadBenchEdge::new(i + 10, u64::from(i)))
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
fn bench_traverse_next_property_first_then_selected() -> canbench_rs::BenchResult {
    let graph = payload_bench_graph(1 << 16, BucketLabelKey::from_raw(1));
    let src = graph.push_vertex(LabeledVertex::default()).unwrap();
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(src, label, INLINE_VALUE_WIDTH)
        .unwrap();
    for i in 0..HYBRID_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(src, label, PayloadBenchEdge::new(i + 10, u64::from(i)))
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
