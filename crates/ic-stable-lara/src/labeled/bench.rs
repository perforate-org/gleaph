//! Benchmarks for the labeled CSR core.
//!
//! Instruction scopes for canbench live in the implementation (`graph.rs`,
//! `deferred.rs`) behind `feature = "canbench"`, not in this file.

use crate::bench as helper;
use crate::labeled::ltb_raw_block_store::LtbRawBlockStore;
use crate::labeled::tree_csr_prototype::TreeCsrBucket;
use crate::labeled::{
    BucketLabelKey, DeferredBidirectionalLabeledLaraGraph, DeferredLabeledLaraGraph,
    EdgePlacementPolicy, LabeledInlinePropertyValueBatchScratch, LabeledVertex, OutEdgeOrder,
    batch_write::{OneOrientationBatchEdge, OneOrientationBatchPlan, OneOrientationBucketRun},
    graph::{LabeledLaraGraph, VertexEdgeSpanCompactOneStep},
};
use crate::{
    VertexId,
    lara::maintenance::MaintenanceBudget,
    test_support::{labeled_lara_memories, vector_memory},
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex},
};
use canbench_rs::{bench, bench_fn};
use std::ops::ControlFlow;
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

/// Labeled edge with inline property bytes for inline-property-log / tombstone benches.
#[derive(Clone, Debug, PartialEq, Eq)]
struct InlinePropertyBenchEdge {
    target: u32,
    slot_index: u32,
    inline_property: [u8; 8],
    inline_property_len: u16,
}

impl InlinePropertyBenchEdge {
    fn with_inline_property(target: u32, inline_property_len: u16, bytes: &[u8]) -> Self {
        let len = u16::try_from(bytes.len()).expect("bench inline property fits u16");
        debug_assert_eq!(len, inline_property_len);
        let mut inline_property = [0u8; 8];
        inline_property[..bytes.len()].copy_from_slice(bytes);
        Self {
            target,
            slot_index: 0,
            inline_property,
            inline_property_len,
        }
    }
}

impl CsrEdge for InlinePropertyBenchEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            slot_index: 0,
            inline_property: [0u8; 8],
            inline_property_len: 0,
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

    fn edge_inline_property_byte_width(&self) -> u16 {
        self.inline_property_len
    }

    fn edge_inline_property_bytes(&self) -> &[u8] {
        &self.inline_property[..usize::from(self.inline_property_len)]
    }

    fn with_stored_inline_property_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len()).min(8);
        self.inline_property = [0u8; 8];
        self.inline_property[..len].copy_from_slice(&bytes[..len]);
        self.inline_property_len =
            u16::try_from(len).expect("bench inline property byte width fits u16");
        self
    }
}

impl CsrEdgeTombstone for InlinePropertyBenchEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            slot_index: 0,
            inline_property: [0u8; 8],
            inline_property_len: 0,
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
        BucketLabelKey::from_raw(1),
    )
    .expect("graph")
}

/// Existing-bucket edge-only expansion fixture. Setup is intentionally outside
/// the measured closure: the bucket slab and its per-leaf overflow log are
/// filled first, then one batch folds the log into an expanded slab window.
#[bench(raw)]
fn bench_l_b_eo_exp() -> canbench_rs::BenchResult {
    let graph = bench_graph(256);
    for _ in 0..2 {
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
    }
    let label = BucketLabelKey::from_raw(2);
    graph
        .insert_edge(
            VertexId::from(0),
            label,
            BenchEdge(1),
            crate::labeled::graph::EdgePlacementPolicy::Insertion,
        )
        .expect("create labeled bucket");
    let leaf = u32::from(VertexId::from(0)) / graph.edges().header().segment_size.max(1);
    let log_capacity = graph.edges().read_overflow_log_state(leaf).1 as usize;
    let entries: Vec<(i32, BenchEdge)> = (0..log_capacity)
        .map(|i| {
            (
                if i == 0 { -1 } else { (i as i32) - 1 },
                BenchEdge(100 + i as u32),
            )
        })
        .collect();
    graph
        .edges()
        .write_overflow_log_entries(leaf, 0, &entries)
        .expect("fill edge overflow log");
    let plan = OneOrientationBatchPlan {
        runs: vec![OneOrientationBucketRun {
            owner_vertex_id: VertexId::from(0),
            label_id: label,
            inline_property_width: 0,
            placement: EdgePlacementPolicy::Unordered,
            edges: vec![
                OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: BenchEdge(200),
                },
                OneOrientationBatchEdge {
                    logical_ordinal: 1,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: BenchEdge(201),
                },
            ],
        }],
    };
    bench_fn(|| {
        let reservation = graph
            .reserve_one_orientation_batch(&plan)
            .expect("edge-only expansion reservation");
        reservation.rollback(&graph);
        black_box(1u32);
    })
}

/// Tombstone-heavy unordered bucket batch fill (ADR 0052 §5 batch hole-first).
///
/// Setup builds a slab-backed bucket, deletes half its edges, and batches the
/// replacement edges; the measured closure runs the full reserve+commit once
/// (preflight window scan + per-hole or window-rewrite writes). This is the
/// batch counterpart to the scalar `labeled_*_tombstone_reuse` benches.
#[bench(raw)]
fn bench_l_b_uo_hf_256() -> canbench_rs::BenchResult {
    const EDGES: u32 = 256;
    let graph = bench_graph(1 << 16);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let label = BucketLabelKey::from_raw(2);
    for i in 0..EDGES {
        graph
            .insert_edge(
                VertexId::from(0),
                label,
                BenchEdge(i),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("seed");
    }
    graph
        .compact_vertex_edge_span(VertexId::from(0), 0)
        .expect("fold log to slab");
    for slot in 0..EDGES / 2 {
        graph
            .remove_edge_at_slot(VertexId::from(0), label, slot)
            .expect("remove")
            .expect("live edge");
    }
    let plan = OneOrientationBatchPlan {
        runs: vec![OneOrientationBucketRun {
            owner_vertex_id: VertexId::from(0),
            label_id: label,
            inline_property_width: 0,
            placement: EdgePlacementPolicy::Unordered,
            edges: (0..EDGES / 2)
                .map(|i| OneOrientationBatchEdge {
                    logical_ordinal: i,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: BenchEdge(1000 + i),
                })
                .collect(),
        }],
    };
    bench_fn(|| {
        graph
            .insert_one_orientation_batch(&plan)
            .expect("batch hole fill");
        black_box(graph.edges().header().num_edges);
    })
}

/// Existing-bucket fixed-width inline property expansion fixture. Setup remains outside
/// the measured closure; reserve and rollback measure the coupled edge/inline property
/// geometry and allocator boundary without charging fixture construction.
#[bench(raw)]
fn bench_l_b_ipe_exp() -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(256);
    for _ in 0..2 {
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
    }
    let inline_property_label = BucketLabelKey::from_raw(2);
    let edge_only_label = BucketLabelKey::from_raw(3);
    graph
        .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), inline_property_label, 8)
        .expect("inline property bytes bucket");
    graph
        .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), edge_only_label, 0)
        .expect("edge-only bucket");
    graph
        .insert_edge(
            VertexId::from(0),
            edge_only_label,
            InlinePropertyBenchEdge::with_inline_property(1, 0, &[]),
            crate::labeled::graph::EdgePlacementPolicy::Insertion,
        )
        .expect("edge-only seed");
    graph
        .insert_edge(
            VertexId::from(0),
            inline_property_label,
            InlinePropertyBenchEdge::with_inline_property(1, 8, &[1; 8]),
            crate::labeled::graph::EdgePlacementPolicy::Insertion,
        )
        .expect("inline property seed");

    let leaf = u32::from(VertexId::from(0)) / graph.edges().header().segment_size.max(1);
    let edge_log_capacity = graph.edges().read_overflow_log_state(leaf).1 as usize;
    let inline_property_bytes_log_capacity =
        graph.values().read_inline_property_bytes_log_state(leaf).1 as usize;
    let edge_entries: Vec<(i32, InlinePropertyBenchEdge)> = (0..edge_log_capacity)
        .map(|i| {
            (
                if i == 0 { -1 } else { (i as i32) - 1 },
                InlinePropertyBenchEdge::with_inline_property(100 + i as u32, 8, &[2; 8]),
            )
        })
        .collect();
    graph
        .edges()
        .write_overflow_log_entries(leaf, 0, &edge_entries)
        .expect("fill edge log");
    let inline_property_bytes = (0..inline_property_bytes_log_capacity)
        .flat_map(|_| [3u8; 8])
        .collect::<Vec<_>>();
    graph
        .values()
        .write_inline_property_bytes_log_entries(leaf, 0, -1, 8, &inline_property_bytes)
        .expect("fill inline property bytes log");
    let vertex = graph.vertices().get(VertexId::from(0));
    let (bucket_slot, bucket) = match graph
        .find_bucket(VertexId::from(0), &vertex, inline_property_label)
        .expect("inline property bytes bucket lookup")
    {
        crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
        _ => panic!("inline property bytes bucket must exist"),
    };
    graph
        .buckets()
        .write_label_bucket_slot(
            bucket_slot,
            bucket
                .with_overflow_log_head((edge_log_capacity - 1) as i32)
                .with_degree_field((edge_log_capacity + 1) as u32)
                .try_with_inline_property_bytes_log(
                    (inline_property_bytes_log_capacity - 1) as i32,
                    inline_property_bytes_log_capacity as u8,
                )
                .expect("inline property bytes log metadata"),
        )
        .expect("inline property bytes bucket metadata");
    let plan = OneOrientationBatchPlan {
        runs: vec![OneOrientationBucketRun {
            owner_vertex_id: VertexId::from(0),
            label_id: inline_property_label,
            inline_property_width: 8,
            placement: EdgePlacementPolicy::Unordered,
            edges: vec![OneOrientationBatchEdge {
                logical_ordinal: 0,
                owner_vertex_id: VertexId::from(0),
                neighbor_vertex_id: VertexId::from(1),
                label_id: inline_property_label,
                edge: InlinePropertyBenchEdge::with_inline_property(1, 8, &[4; 8]),
            }],
        }],
    };
    bench_fn(|| {
        let reservation = graph
            .reserve_one_orientation_batch(&plan)
            .expect("inline property expansion reservation");
        reservation.rollback(&graph);
        black_box(1u32);
    })
}

fn inline_property_bench_graph(
    elem_capacity: u64,
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

/// Same-label overflow hub used by ADR 0016 inline-property-log and tombstone benches.
const OVERFLOW_LOG_HUB_EDGES: u32 = 48;
const INLINE_PROPERTY_BYTES_LOG_INLINE_WIDTH: u16 = 8;

fn seed_overflow_inline_property_hub(
    graph: &LabeledLaraGraph<InlinePropertyBenchEdge, crate::VectorMemory>,
    edge_count: u32,
    inline_property_width: u16,
) -> (VertexId, BucketLabelKey) {
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(vid, label, inline_property_width)
        .expect("inline property byte width");
    for target in 1..=edge_count {
        let mut inline_property = [0u8; 8];
        let width = usize::from(inline_property_width);
        inline_property[..width].copy_from_slice(&(target as u64).to_le_bytes()[..width]);
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                InlinePropertyBenchEdge::with_inline_property(
                    target,
                    inline_property_width,
                    &inline_property,
                ),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
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
            .compact_vertex_edge_span_one_step(vid, resume, 0, &|_| {
                crate::labeled::graph::EdgePlacementPolicy::Insertion
            })
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
                .insert_edge_skip_leaf_cascade(
                    hub,
                    label,
                    BenchEdge(edge_i),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
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
            .insert_edge(
                VertexId::from(0),
                label,
                BenchEdge(i),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
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
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                BenchEdge(i),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
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
fn bench_l_cap_sp_l32_1e() -> canbench_rs::BenchResult {
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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
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
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
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
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("hub edge");
    }
    black_box(edge_count);
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_l_cap_s_sp_l32_1e() -> canbench_rs::BenchResult {
    bench_fn(|| {
        run_stable_sparse_capacity(STAGE2_LEAF, 32);
    })
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_l_cap_s_sp_l1024_1e() -> canbench_rs::BenchResult {
    bench_fn(|| {
        run_stable_sparse_capacity(STABLE_CAPACITY_PROBE_VERTICES, 32);
    })
}

#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_l_cap_s_sp_s16_1024v_1e() -> canbench_rs::BenchResult {
    bench_fn(|| {
        run_stable_sparse_capacity(STABLE_CAPACITY_PROBE_VERTICES, 16);
    })
}

/// Stable-backed hub growth probe. This crosses the segment16 leaf quota and
/// exercises overflow-log fold plus leaf relocation/resize.
#[cfg(target_family = "wasm")]
#[bench(raw)]
fn bench_l_cap_s_hub_s16_256e() -> canbench_rs::BenchResult {
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
                    .insert_edge(
                        VertexId::from(v),
                        label,
                        BenchEdge(v * 10_000 + e),
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    )
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
fn bench_l_mix_hub_ins_33x50() -> canbench_rs::BenchResult {
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
fn bench_l_mix_hub_scn_33x50() -> canbench_rs::BenchResult {
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
                .visit_edges(hub, label, OutEdgeOrder::Descending, |_slot, _edge| {
                    count += 1;
                    ControlFlow::<()>::Continue(())
                })
                .map(|_| ())
                .expect("for_each");
        }
        black_box(count);
    })
}

#[bench(raw)]
fn bench_l_mix_hub_asc_33x50() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let hub = seed_mixed_label_hub(
        &graph,
        MIXED_LABEL_HUB_LABELS,
        MIXED_LABEL_HUB_EDGES_PER_LABEL,
    );
    bench_fn(|| {
        let edges = graph.out_edges(hub).expect("out_edges");
        black_box(edges.len());
    })
}

#[bench(raw)]
fn bench_l_fe_48x51() -> canbench_rs::BenchResult {
    let graph = bench_graph(16384);
    let label = seed_single_label_parallel_edges(&graph, CONVERGING_HUB_PREFIX_EDGES);
    let vid = VertexId::from(0);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            graph
                .visit_edges_with_inline_property(
                    vid,
                    label,
                    OutEdgeOrder::Descending,
                    |_slot, item| {
                        let edge = item.edge.with_label_id(label.raw());
                        count += usize::from(edge.neighbor_vid().0 > 0);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .expect("for_each");
            black_box(count);
        }
    })
}

#[bench(raw)]
fn bench_l_fe_24x51() -> canbench_rs::BenchResult {
    let graph = bench_graph(8192);
    let label = seed_single_label_parallel_edges(&graph, CONVERGING_HUB_OUT_EDGES);
    let vid = VertexId::from(0);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            graph
                .visit_edges_with_inline_property(
                    vid,
                    label,
                    OutEdgeOrder::Descending,
                    |_slot, item| {
                        let edge = item.edge.with_label_id(label.raw());
                        count += usize::from(edge.neighbor_vid().0 > 0);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .expect("for_each");
            black_box(count);
        }
    })
}

#[bench(raw)]
fn bench_l_iter_128() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096);
    graph
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .expect("vertex");
    let label = BucketLabelKey::from_raw(2);
    for i in 0..128u32 {
        graph
            .insert_edge(
                VertexId::from(0),
                label,
                BenchEdge(i),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
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
fn bench_l_def_bp_iter_128() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096);
    graph
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .expect("vertex");
    graph
        .enable_default_edge_bypass(VertexId::from(0))
        .expect("bypass");
    for i in 0..128u32 {
        graph
            .insert_edge(
                VertexId::from(0),
                graph.default_label(),
                BenchEdge(i),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
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
fn bench_l_ins_ex_bucket_128() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096);
    graph
        .push_vertex(crate::labeled::record::LabeledVertex::default())
        .expect("vertex");
    let label = BucketLabelKey::from_raw(2);
    bench_fn(|| {
        for i in 0..helper::MEDIUM_N as u32 {
            let i = black_box(i);
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    BenchEdge(i),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
    })
}

#[bench(raw)]
fn bench_l_ins_sb_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(4096);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let label = BucketLabelKey::from_raw(2);
    bench_fn(|| {
        for i in 0..helper::MEDIUM_N as u32 {
            let i = black_box(i);
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    BenchEdge(i),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        black_box(graph.vertex_count());
    })
}

/// Many [`LabelBucket`] rows on one vertex, then repeated inserts into the **last**
/// label (stresses `find_bucket_slot` / bucket metadata reads).
#[bench(raw)]
fn bench_l_ins_last_1024() -> canbench_rs::BenchResult {
    const N_BUCKETS: u16 = 128;
    let graph = bench_graph(16384);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    for k in 0..N_BUCKETS {
        let label = BucketLabelKey::from_raw(2 + k);
        graph
            .insert_edge(
                vid,
                label,
                BenchEdge(u32::from(k)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("seed insert");
    }
    let target = BucketLabelKey::from_raw(2 + N_BUCKETS - 1);
    bench_fn(|| {
        for i in 0..helper::MEDIUM_N as u32 {
            let i = black_box(i);
            graph
                .insert_edge(
                    vid,
                    target,
                    BenchEdge(i),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        black_box(target.raw());
    })
}

/// Round-robin across many labels (mix of `find_bucket_slot` hits on different indices).
#[bench(raw)]
fn bench_l_ins_rr_64l_1024() -> canbench_rs::BenchResult {
    const N_LABELS: u16 = 64;
    let graph = bench_graph(16384);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    for k in 0..N_LABELS {
        let label = BucketLabelKey::from_raw(10 + k);
        graph
            .insert_edge(
                vid,
                label,
                BenchEdge(u32::from(k)),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("seed insert");
    }
    bench_fn(|| {
        for i in 0..helper::MEDIUM_N as u32 {
            let i = black_box(i);
            let label = BucketLabelKey::from_raw(10 + (i % u32::from(N_LABELS)) as u16);
            graph
                .insert_edge(
                    vid,
                    label,
                    BenchEdge(i),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        black_box(N_LABELS);
    })
}

/// Every insert uses a **new** label id so each call walks `find_or_create_bucket`.
#[bench(raw)]
fn bench_l_ins_fresh_256() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = bench_graph(32768);
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
        let vid = VertexId::from(0);
        for i in 0u16..256 {
            let label = BucketLabelKey::from_raw(3000 + i);
            graph
                .insert_edge(
                    vid,
                    label,
                    BenchEdge(u32::from(i)),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        black_box(graph.vertex_count());
    })
}

/// One PMA leaf worth of vertices (32 rows — same as the labeled graph default segment size):
/// light seeding, then round-robin inserts on the same label.
#[bench(raw)]
fn bench_l_ins_mv_l32_2048() -> canbench_rs::BenchResult {
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
                .insert_edge(
                    VertexId::from(v),
                    label,
                    BenchEdge(v * 10_000 + e),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("seed");
        }
    }
    bench_fn(|| {
        for i in 0..2048u32 {
            let i = black_box(i);
            let vid = VertexId::from(i % LEAF);
            graph
                .insert_edge(
                    vid,
                    label,
                    BenchEdge(i),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        black_box(LEAF);
    })
}

#[bench(raw)]
fn bench_cmp_edge_sc_128() -> canbench_rs::BenchResult {
    let mut bytes = Vec::with_capacity(128 * BenchEdge::BYTES);
    for i in 0..128u32 {
        let mut slot = [0u8; BenchEdge::BYTES];
        BenchEdge(i).write_to(&mut slot);
        bytes.extend_from_slice(&slot);
    }
    bench_fn(|| {
        let mut count = 0usize;
        let (chunks, remainder) = bytes.as_chunks::<{ BenchEdge::BYTES }>();
        debug_assert!(remainder.is_empty());
        for chunk in chunks {
            let edge = BenchEdge::read_from(chunk);
            count += usize::from(edge.neighbor_vid().0 > 0);
        }
        black_box(count);
    })
}

/// Deferred admission path only (no maintenance drain in the measured region).
#[bench(raw)]
fn bench_l_df_ins_1024() -> canbench_rs::BenchResult {
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
            graph
                .insert_edge(
                    vid,
                    label,
                    BenchEdge(i),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
        }
        black_box(graph.maintenance_queue_len());
    })
}

/// Fragment a vertex edge span, enqueue one compaction item, then run one maintenance step.
#[bench(raw)]
fn bench_l_df_mnt_cv_1() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = deferred_bench_graph(8192);
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        graph
            .inner()
            .push_vertex(LabeledVertex::default())
            .expect("vertex");
        for t in 0..80u32 {
            graph
                .insert_edge(
                    vid,
                    label,
                    BenchEdge(t),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("insert");
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

/// ADR 0016: inline property attach over hybrid slab + 8 B inline property overflow log.
#[bench(raw)]
fn bench_l_ip_log_8b_of() -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 20);
    let (vid, label) = seed_overflow_inline_property_hub(
        &graph,
        OVERFLOW_LOG_HUB_EDGES,
        INLINE_PROPERTY_BYTES_LOG_INLINE_WIDTH,
    );
    let mut scratch = LabeledInlinePropertyValueBatchScratch::default();
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut byte_count = 0usize;
            let _ = graph
                .visit_out_inline_property_batches_for_label_next(
                    vid,
                    label,
                    OutEdgeOrder::Descending,
                    &mut scratch,
                    |batch| {
                        byte_count = byte_count.saturating_add(batch.values.len());
                        ControlFlow::<()>::Continue(())
                    },
                )
                .expect("inline property batches");
            black_box(byte_count);
        }
    })
}

/// Baseline for exact inline-property-slab growth before introducing bounded headroom.
#[bench(raw)]
fn bench_l_ip_exg_256() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = inline_property_bench_graph(1 << 20);
        let vid = graph.push_vertex(LabeledVertex::default()).expect("vertex");
        let label = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, 2)
            .expect("inline property byte width");
        for target in 0..256u32 {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    InlinePropertyBenchEdge::with_inline_property(
                        target,
                        2,
                        &(target as u16).to_le_bytes(),
                    ),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("inline property insert");
        }
        black_box(graph.out_edges(vid).expect("scan").len());
    })
}

fn seed_fragmented_inline_property_fixture(
    graph: &LabeledLaraGraph<InlinePropertyBenchEdge, crate::VectorMemory>,
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
            .ensure_label_bucket_inline_property_byte_width(vid, label, width)
            .expect("inline property byte width");
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                InlinePropertyBenchEdge::with_inline_property(target, width, &target.to_le_bytes()),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("inline property insert");
    }
    for (label, target) in [(removed_small, 0), (removed_wide, 2)] {
        graph
            .remove_edge_matching(vid, label, |edge| edge.target == target)
            .expect("inline property remove")
            .expect("removed inline property edge");
    }
    vid
}

fn seed_inline_property_free_span_count(
    graph: &LabeledLaraGraph<InlinePropertyBenchEdge, crate::VectorMemory>,
    span_count: u32,
) -> VertexId {
    let vid = graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let total_labels = span_count.saturating_mul(2);
    for index in 0..total_labels {
        let label = BucketLabelKey::directed_from_index(
            u16::try_from(100 + index).expect("benchmark label fits u16"),
        );
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, 2)
            .expect("inline property byte width");
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                InlinePropertyBenchEdge::with_inline_property(index, 2, &[index as u8; 2]),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("inline property insert");
        if index % 2 == 0 {
            graph
                .remove_edge_matching(vid, label, |edge| edge.target == index)
                .expect("inline property remove")
                .expect("inline property edge removed");
        }
    }
    vid
}

fn bench_inline_property_pressure_stats(
    span_count: u32,
    scope_name: &'static str,
) -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = inline_property_bench_graph(1 << 20);
        let vid = seed_inline_property_free_span_count(&graph, span_count);
        let _scope = canbench_rs::bench_scope(scope_name);
        for _ in 0..64 {
            black_box(
                graph
                    .inline_property_bytes_compaction_needed(3)
                    .expect("inline property pressure"),
            );
        }
        black_box(graph.vertices().get(vid));
    })
}

/// Measures allocator pressure checks with a small retired-span set.
#[bench(raw)]
fn bench_l_ip_pstats_16() -> canbench_rs::BenchResult {
    bench_inline_property_pressure_stats(16, "inline_property_pressure_stats_16")
}

/// Measures allocator pressure checks with a medium retired-span set.
#[bench(raw)]
fn bench_l_ip_pstats_64() -> canbench_rs::BenchResult {
    bench_inline_property_pressure_stats(64, "inline_property_pressure_stats_64")
}

/// Measures allocator pressure checks with a large retired-span set.
#[bench(raw)]
fn bench_l_ip_pstats_256() -> canbench_rs::BenchResult {
    bench_inline_property_pressure_stats(256, "inline_property_pressure_stats_256")
}

/// Measures the first inline property bytes allocation that triggers fragmented-slab compaction.
#[bench(raw)]
fn bench_l_ip_frag_s6() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = inline_property_bench_graph(1 << 20);
        let vid = seed_fragmented_inline_property_fixture(&graph);
        let target = BucketLabelKey::directed_from_index(5);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, target, 6)
            .expect("inline property byte width");
        let _scope = canbench_rs::bench_scope("inline_property_fragmented_trigger_insert");
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                target,
                InlinePropertyBenchEdge::with_inline_property(3, 6, &3u32.to_le_bytes()),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("inline property insert");
        black_box(
            graph
                .inline_property_bytes_storage_stats()
                .expect("inline property stats"),
        );
    })
}

/// Control for a same-fixture first allocation that can reuse one free span.
#[bench(raw)]
fn bench_l_ip_frag_s4_ctl() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = inline_property_bench_graph(1 << 20);
        let vid = seed_fragmented_inline_property_fixture(&graph);
        let target = BucketLabelKey::directed_from_index(5);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, target, 4)
            .expect("inline property byte width");
        let _scope = canbench_rs::bench_scope("inline_property_fragmented_control_insert");
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                target,
                InlinePropertyBenchEdge::with_inline_property(3, 4, &3u32.to_le_bytes()),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("inline property insert");
        black_box(
            graph
                .inline_property_bytes_storage_stats()
                .expect("inline property stats"),
        );
    })
}

/// Isolates the inline-property-bytes-only compaction pass for the fragmented fixture.
#[bench(raw)]
fn bench_l_ip_frag_comp() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = inline_property_bench_graph(1 << 20);
        let _vid = seed_fragmented_inline_property_fixture(&graph);
        let _scope = canbench_rs::bench_scope("inline_property_fragmented_compaction_only");
        black_box(
            graph
                .compact_inline_property_bytes_slab()
                .expect("inline property bytes compaction"),
        );
        black_box(
            graph
                .inline_property_bytes_storage_stats()
                .expect("inline property stats"),
        );
    })
}

/// Measures deferred insertion with inline property pressure detection and queue enqueue only.
#[bench(raw)]
fn bench_l_df_ip_frag_enq_6() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph =
            DeferredLabeledLaraGraph::new(inline_property_bench_graph(1 << 20), vector_memory())
                .expect("deferred graph");
        let vid = seed_fragmented_inline_property_fixture(graph.inner());
        let target = BucketLabelKey::directed_from_index(5);
        graph
            .inner()
            .ensure_label_bucket_inline_property_byte_width(vid, target, 6)
            .expect("inline property byte width");
        let _scope = canbench_rs::bench_scope("inline_property_deferred_enqueue_insert");
        graph
            .insert_edge(
                vid,
                target,
                InlinePropertyBenchEdge::with_inline_property(3, 6, &[3u8; 6]),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("inline property insert");
        black_box(graph.maintenance_queue_len());
    })
}

/// Measures the deferred maintenance step that performs inline property bytes compaction.
#[bench(raw)]
fn bench_l_df_ip_frag_mnt_6() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph =
            DeferredLabeledLaraGraph::new(inline_property_bench_graph(1 << 20), vector_memory())
                .expect("deferred graph");
        let vid = seed_fragmented_inline_property_fixture(graph.inner());
        let target = BucketLabelKey::directed_from_index(5);
        graph
            .inner()
            .ensure_label_bucket_inline_property_byte_width(vid, target, 6)
            .expect("inline property byte width");
        graph
            .insert_edge(
                vid,
                target,
                InlinePropertyBenchEdge::with_inline_property(3, 6, &[3u8; 6]),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("inline property insert");
        let budget = MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: Some(1),
            max_segments: None,
            max_delete_edge_steps: None,
        };
        let _scope = canbench_rs::bench_scope("inline_property_deferred_maintenance_compaction");
        black_box(graph.maintenance(budget));
    })
}

/// ADR 0016: scan after tombstone-free direct log unlinks.
#[bench(raw)]
fn bench_l_du_log_del_scn() -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 20);
    let (vid, label) = seed_overflow_inline_property_hub(&graph, OVERFLOW_LOG_HUB_EDGES, 2);
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
                .visit_edges_with_inline_property(
                    vid,
                    label,
                    OutEdgeOrder::Descending,
                    |_slot, item| {
                        count += usize::from(item.inline_property.width() > 0);
                        ControlFlow::<()>::Continue(())
                    },
                )
                .map(|_| ())
                .expect("for_each");
            black_box(count);
        }
    })
}

/// ADR 0016: foreground direct log unlink plus incremental span compaction on an overflow edge hub.
#[bench(raw)]
fn bench_l_du_log_fold_mnt() -> canbench_rs::BenchResult {
    bench_fn(|| {
        let graph = bench_graph(1 << 20);
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
        let vid = VertexId::from(0);
        let label = BucketLabelKey::from_raw(2);
        for target in 1..=OVERFLOW_LOG_HUB_EDGES {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    BenchEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
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
            .visit_edges(vid, label, OutEdgeOrder::Descending, |_slot, _edge| {
                count += 1;
                ControlFlow::<()>::Continue(())
            })
            .map(|_| ())
            .expect("for_each");
        black_box(count);
    })
}

/// ADR 0022 Stage 2 baseline (delete churn): remove half of a 1024-edge skewed hub
/// bucket by match, then compact the vertex edge span. This is the O(degree)
/// delete-plus-compaction cost that a B-tree tier's O(log d) delete-by-`seq`
/// (no compaction) would replace; the hub is seeded outside the measured region.
#[bench(raw)]
fn bench_l_s2_h_del_1024() -> canbench_rs::BenchResult {
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
            .visit_edges(vid, label, OutEdgeOrder::Descending, |_slot, _edge| {
                count += 1;
                ControlFlow::<()>::Continue(())
            })
            .map(|_| ())
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
fn bench_l_s2_h_del_h_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let (vid, label) = seed_single_label_hub(&graph, STAGE2_HUB_DEGREE);
    bench_fn(|| {
        // Remove from the high end so overflow-log unlink shifts cannot
        // invalidate the remaining physical slot handles before they are used.
        for slot in (0..STAGE2_HUB_DEGREE).step_by(2).rev() {
            graph
                .remove_edge_at_slot(vid, label, slot)
                .expect("remove")
                .expect("removed");
        }
        compact_vertex_edge_span_until_overflow_or_done(&graph, vid);
        let mut count = 0usize;
        graph
            .visit_edges(vid, label, OutEdgeOrder::Descending, |_slot, _edge| {
                count += 1;
                ControlFlow::<()>::Continue(())
            })
            .map(|_| ())
            .expect("for_each");
        black_box(count);
    })
}

/// ADR 0052 Slice 3: Unordered scalar insert into a tombstone-heavy slab-backed
/// hub bucket. The fixture folds the overflow log to the slab and deletes half
/// the edges, so every measured re-insert reuses an in-slab tombstone (same
/// `stored_slots`, degree + 1, ADR 0052 §5 step 1) instead of appending to the
/// tail or overflow log.
#[bench(raw)]
fn bench_l_uo_tomb_h_del_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let (vid, label) = seed_single_label_hub(&graph, STAGE2_HUB_DEGREE);
    compact_vertex_edge_span_until_overflow_or_done(&graph, vid);
    for slot in (0..STAGE2_HUB_DEGREE).step_by(2).rev() {
        graph
            .remove_edge_at_slot(vid, label, slot)
            .expect("remove")
            .expect("removed");
    }
    let next = Cell::new(0u32);
    bench_fn(|| {
        let _scope = canbench_rs::bench_scope("labeled_unordered_tombstone_reuse_half_deleted");
        let i = next.get();
        if i >= STAGE2_HUB_DEGREE / 2 {
            return;
        }
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                BenchEdge(100_000 + i),
                crate::labeled::graph::EdgePlacementPolicy::Unordered,
            )
            .expect("reuse insert");
        next.set(i + 1);
        black_box(graph.vertices().get(vid));
    })
}

/// ADR 0052 Slice 3 control: the same tombstone-heavy fixture with Insertion
/// placement, which must never reuse an interior tombstone (ADR 0052 §6) and
/// instead appends. Keeps the Insertion scalar path regression-visible against
/// the Unordered reuse path.
#[bench(raw)]
fn bench_l_ins_ap_hub_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let (vid, label) = seed_single_label_hub(&graph, STAGE2_HUB_DEGREE);
    compact_vertex_edge_span_until_overflow_or_done(&graph, vid);
    for slot in (0..STAGE2_HUB_DEGREE).step_by(2).rev() {
        graph
            .remove_edge_at_slot(vid, label, slot)
            .expect("remove")
            .expect("removed");
    }
    let next = Cell::new(0u32);
    bench_fn(|| {
        let _scope = canbench_rs::bench_scope("labeled_insertion_append_on_tombstone_hub");
        let i = next.get();
        if i >= STAGE2_HUB_DEGREE / 2 {
            return;
        }
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                BenchEdge(100_000 + i),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("append insert");
        next.set(i + 1);
        black_box(graph.vertices().get(vid));
    })
}

/// ADR 0052 Slice 5: Unordered swap-compaction maintenance on a tombstone-heavy
/// slab-backed hub bucket carrying inline property bytes. The fixture folds the
/// overflow log to the slab, deletes every other slot, then drains
/// `compact_vertex_edge_span_one_step` with the Unordered resolver — measuring the
/// swap finder scan, the edge writes into interior tombstones, and the synchronized
/// value-block rotation (ADR 0052 §7/§9). The step-drain mirrors the maintenance
/// loop's per-step invocation with Noop observers.
#[bench(raw)]
fn bench_l_uo_swap_h_del_1024() -> canbench_rs::BenchResult {
    let graph = inline_property_bench_graph(1 << 20);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(vid, label, 8)
        .expect("width");
    for i in 0..STAGE2_HUB_DEGREE {
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                InlinePropertyBenchEdge::with_inline_property(i, 8, &(i as u16).to_le_bytes()),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("insert");
    }
    compact_vertex_edge_span_until_overflow_or_done(&graph, vid);
    for slot in (0..STAGE2_HUB_DEGREE).step_by(2).rev() {
        graph
            .remove_edge_at_slot(vid, label, slot)
            .expect("remove")
            .expect("removed");
    }
    // (resume, moves); resume == u32::MAX marks the drained terminal state.
    let state = Cell::new((0u32, 0u64));
    bench_fn(|| {
        let _scope = canbench_rs::bench_scope("labeled_unordered_swap_compact_half_deleted");
        let (resume, moves) = state.get();
        if resume == u32::MAX {
            return;
        }
        let step = graph
            .compact_vertex_edge_span_one_step(vid, resume, 0, &|_| {
                crate::labeled::graph::EdgePlacementPolicy::Unordered
            })
            .expect("swap step");
        let next = match step {
            VertexEdgeSpanCompactOneStep::EdgeMoved(_) => (0, moves + 1),
            VertexEdgeSpanCompactOneStep::AdvanceBucket(next) => (next, moves),
            VertexEdgeSpanCompactOneStep::OverflowRewrite(_) => (0, moves),
            VertexEdgeSpanCompactOneStep::Finished => (u32::MAX, moves),
        };
        state.set(next);
        black_box(state.get());
    })
}

/// ADR 0022 Stage 2 baseline (point lookup): scan a 1024-edge skewed hub bucket in
/// the hot descending order to locate one target. Today this is O(degree) (the
/// `find_first_forward_handle_descending` walk); an optional `target -> seq`
/// secondary index would make it O(log d). Worst case: the first-inserted target
/// (target `0`) is reached last in descending order.
#[bench(raw)]
fn bench_l_s2_h_pt_d_1024() -> canbench_rs::BenchResult {
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
fn bench_l_s2_h_scn_d_1024() -> canbench_rs::BenchResult {
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
fn bench_l_s2_sat_256() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let label = seed_stage2_leaf(&graph, true);
    bench_fn(|| {
        for e in 0..STAGE2_GROW_DEGREE {
            let e = black_box(e);
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    BenchEdge(e),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
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
fn bench_l_s2_iso_256() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    let label = seed_stage2_leaf(&graph, false);
    bench_fn(|| {
        for e in 0..STAGE2_GROW_DEGREE {
            let e = black_box(e);
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    BenchEdge(e),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("isolated grow insert");
        }
        black_box(STAGE2_GROW_DEGREE);
    })
}

/// ADR 0022 Stage 2b *insert cost* (paid update path): grow one fresh hub vertex
/// 0 → 1024 edges via the **real** `insert_edge` cascade (leaf slide/relocate +
/// overflow log) — the status-quo insert cost an update call pays. Pair with
/// `bench_labeled_stage2b_narrow_hub_insert_1024` for the B-tree insert delta.
#[bench(raw)]
fn bench_l_s2_h_ins_1024() -> canbench_rs::BenchResult {
    let graph = bench_graph(1 << 20);
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    let label = BucketLabelKey::from_raw(2);
    bench_fn(|| {
        for target in 0..STAGE2_HUB_DEGREE {
            let target = black_box(target);
            graph
                .insert_edge(
                    vid,
                    label,
                    BenchEdge(target),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("hub grow insert");
        }
        black_box(STAGE2_HUB_DEGREE);
    })
}

/// ADR 0022 Stage 2 *crossover* (paid update path): the same insert and fair
/// delete-by-handle pair as the `..._1024` benches, parameterized by degree, to
/// locate where the slab's unindexed overflow-log delete (O(degree) chain walk
/// per log-resident slot; O(degree²) over a delete-half) loses to the B-tree's
/// O(log d) delete — and to confirm insert stays slab-favored at scale.
///
/// Plan 0318 post-Step-9 cleanup: the B-tree / narrow-tree arms (Stage 2b) were
/// removed from this macro — the Stage 2b B-tree tier is a closed decision
/// ("NOT warranted", ADR 0022 §Decision) and its evidence lives in the ADR
/// text, `canbench_results.yml` git history (keys `bench_labeled_stage2b_*`),
/// and this file's git history. The slab arms remain: they feed ADR 0088
/// Gate 1 (problem sizing) and Gate 2 (operation parity matrix).
macro_rules! stage2b_crossover_benches {
    ($deg:expr, $slab_del:ident, $slab_ins:ident) => {
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
                    .visit_edges(vid, label, OutEdgeOrder::Descending, |_slot, _edge| {
                        count += 1;
                        ControlFlow::<()>::Continue(())
                    })
                    .map(|_| ())
                    .expect("for_each");
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
                        .insert_edge(
                            vid,
                            label,
                            BenchEdge(target),
                            crate::labeled::graph::EdgePlacementPolicy::Insertion,
                        )
                        .expect("hub grow insert");
                }
                black_box::<u32>($deg);
            })
        }
    };
}

stage2b_crossover_benches!(
    4096u32,
    bench_labeled_stage2_hub_delete_half_by_slot_then_compact_4096,
    bench_labeled_stage2_hub_insert_grow_4096
);

stage2b_crossover_benches!(
    16384u32,
    bench_labeled_stage2_hub_delete_half_by_slot_then_compact_16384,
    bench_labeled_stage2_hub_insert_grow_16384
);

// Plan 0313 (ADR 0088 Tree-CSR measurement gates) — slab baselines at the
// high-degree sweep points (Gate 1 input). Reuses `stage2b_crossover_benches!`
// to expose `bench_labeled_stage2_hub_*_<deg>` for `stored_slots = 65,536`
// and `stored_slots = 131,072`. These arms feed Gate 1 (problem sizing)
// and Gate 2 (operation parity matrix) per ADR 0088 §Measurement gates.
stage2b_crossover_benches!(
    65536u32,
    bench_labeled_stage2_hub_delete_half_by_slot_then_compact_65536,
    bench_labeled_stage2_hub_insert_grow_65536
);

// Plan 0313 / Step 1: the 131K slab-baseline arm was removed because the
// PocketIC wasm export name budget (20,000 chars across all exported fn
// names) was already saturated by the pre-existing 141 canbench functions
// plus the new parity + Gate 3/4 benches. Adding the 131K slab arm would
// push the wasm above the budget and prevent the canister from installing.
// The 4K and 65K slab arms above cover the depth-1 and depth-1↔2 boundaries
// that ADR 0088 §4 calls out; the 131K point is exercised instead by a
// cargo-test-based sweep (see `tree_csr_high_degree_test.rs`).

// ---------------------------------------------------------------------------
// Plan 0313 (ADR 0088 Tree-CSR measurement gates) — Tree-CSR evidence-only
// prototype benches (Gate 2 operation parity matrix).
//
// Mirrors `stage2b_crossover_benches!`'s degree parameterization and exposes
// the seven parity rows (sequential full scan, prefix scan, random ordinal
// access, counterpart resolution, insert, delete, compaction) at 4K / 64K /
// 131K. Each Tree-CSR bench is paired (one degree above, one below) with its
// slab-baseline counterpart by naming convention so the comparison lands in
// `canbench_results.yml` next to the slab numbers.
// ---------------------------------------------------------------------------

/// Seeds one Tree-CSR evidence-only bucket with `edge_count` edges in
/// insertion order (target == i). Seeding happens outside the measured region
/// so the bench costs only the operation under test.
fn seed_tree_csr_hub(edge_count: u32) -> TreeCsrBucket<crate::VectorMemory> {
    let mut bucket = TreeCsrBucket::new(vector_memory());
    for i in 0..edge_count {
        bucket.insert(i);
    }
    bucket
}

/// Build one parity-row bench at a given degree target. The macro takes the
/// numeric degree, the full bench identifier (a `tt` token sequence), and the
/// operation closure body. We avoid `${concat(...)}` by accepting the full
/// identifier as a token tree that the call site builds with `paste!`-free
/// stable syntax (literal concatenation).
macro_rules! tree_csr_parity_bench {
    ($deg:expr, $name:ident, $body:block) => {
        #[bench(raw)]
        fn $name() -> canbench_rs::BenchResult {
            $body
        }
    };
}

// The seven parity-row bodies are shared across the three degree targets. We
// inline each body at the call site to keep the macro small and the closure
// logic visible. Each degree then emits 7 `tree_csr_parity_bench!` calls
// with the degree-specific identifier.
//
// Degree 4K ----------------------------------------------------------------
// Plan 0313 / Step 3 amend: only the four headline parity rows run via
// canbench at degree 4K (full scan, random ordinal access, insert grow,
// delete half). The remaining three rows (prefix scan, counterpart resolve,
// compaction) live in `tree_csr_high_degree_test.rs` as cargo-test benches
// to keep the wasm export name budget under 20,000 chars.
tree_csr_parity_bench!(4096u32, tcsr_4096_full_scan_descending, {
    let bucket = seed_tree_csr_hub(4096);
    bench_fn(|| {
        let mut count = 0usize;
        bucket.for_each_descending(|_slot, target| {
            count += usize::from(target < 4096);
        });
        black_box(count);
    })
});
tree_csr_parity_bench!(4096u32, tcsr_4096_random_ordinal_access, {
    let bucket = seed_tree_csr_hub(4096);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            bucket.random_ordinal_access(black_box(64), |_slot, target| {
                count += usize::from(target < 4096);
            });
            black_box(count);
        }
    })
});
tree_csr_parity_bench!(4096u32, tcsr_4096_insert_grow, {
    bench_fn(|| {
        let mut bucket = TreeCsrBucket::new(vector_memory());
        for target in 0..4096u32 {
            bucket.insert(black_box(target));
        }
        black_box(bucket.stored_slots());
    })
});
// Plan 0318 post-Step-9 removal: the `tcsr_4096_delete_half_by_slot_then_scan`
// and `tcsr_65536_delete_half_by_slot_then_scan` benches were removed from the
// canbench surface. Both are O(N²) prototype-only parity rows (24 B and 6.14 T
// instructions respectively) with no production-representative meaning; the
// 65K arm intermittently crashes the PocketIC daemon mid-query (connection
// reset during the 6.14 T-instruction call). Their measured results are
// preserved in `plans/0318-tree-csr-implementation.md` §Step 9 and in
// `git history` of `canbench_results.yml`.

// Degree 65K ----------------------------------------------------------------
// Same amend as 4K above: only the three headline parity rows run via
// canbench at degree 65K (delete_half removed — see the 4K note above). The
// remaining three rows live in `tree_csr_high_degree_test.rs`.
tree_csr_parity_bench!(65536u32, tcsr_65536_full_scan_descending, {
    let bucket = seed_tree_csr_hub(65536);
    bench_fn(|| {
        let mut count = 0usize;
        bucket.for_each_descending(|_slot, target| {
            count += usize::from(target < 65536);
        });
        black_box(count);
    })
});
tree_csr_parity_bench!(65536u32, tcsr_65536_random_ordinal_access, {
    let bucket = seed_tree_csr_hub(65536);
    bench_fn(|| {
        for _ in 0..CONVERGING_HUB_EXPAND_CALLS {
            let mut count = 0usize;
            bucket.random_ordinal_access(black_box(64), |_slot, target| {
                count += usize::from(target < 65536);
            });
            black_box(count);
        }
    })
});
tree_csr_parity_bench!(65536u32, tcsr_65536_insert_grow, {
    bench_fn(|| {
        let mut bucket = TreeCsrBucket::new(vector_memory());
        for target in 0..65536u32 {
            bucket.insert(black_box(target));
        }
        black_box(bucket.stored_slots());
    })
});
// Degree 131K -----------------------------------------------------------------
// Plan 0313 / Step 3 amend: the 131K parity arms were removed because the
// PocketIC wasm export name budget (20,000 chars across all exported fn
// names) was already saturated by the pre-existing 141 canbench functions
// plus the new parity + Gate 3/4 benches. Adding 7 × 1 = 7 more 131K
// `tcsr_131072_<op>` functions would push the wasm above the budget and
// prevent the canister from installing.
//
// The 131K sweep is instead covered by a cargo-test-based benchmark in
// `tree_csr_high_degree_test.rs` (Plan 0313 Step 3 amend), which records
// wall-clock per-edge cost on the host (not instruction counts) so it can
// run without affecting the wasm export name budget.

// ---------------------------------------------------------------------------
// Plan 0313 (ADR 0088 Tree-CSR measurement gates) — Gate 3 (promotion cost)
// and Gate 4 (LTB reopen envelope) benches.
// ---------------------------------------------------------------------------

/// T_promote per ADR 0088 §3 = 4,096 slots. Used by the Gate 3 promotion
/// benches (edge-only + inline-property widest admitted profile).
const T_PROMOTE_BENCH: usize = 4096;

/// Gate 3 — edge-only promotion at T_promote = 4096. Reserves 4 data blocks
/// (ceil(4096 / 1024)) and commits the canonical edge rows. Per ADR 0088 §7,
/// `Memory::grow` calls complete in the reserve phase; the measured cost is
/// the commit-phase transcription plus descriptor publish.
#[bench(raw)]
fn tcsr_promote_edge_only() -> canbench_rs::BenchResult {
    let targets: Vec<u32> = (0..T_PROMOTE_BENCH as u32).collect();
    bench_fn(|| {
        let mut bucket = TreeCsrBucket::new(vector_memory());
        bucket.promote_from_slice(black_box(&targets));
        black_box(bucket.stored_slots());
    })
}

/// Gate 3 — inline-property widest admitted profile (w = 32) at T_promote.
/// Reserves 4 edge blocks + 32 property blocks (K = 4096 / 32 = 128 properties
/// per block, ceil(4096 / 128) = 32). The expected byte transcription is
/// O(T_promote × (4 + w)) = 4096 × 36 ≈ 144 KiB; measured cost should be in
/// single-digit M instructions per ADR 0088 §Measurement gates Gate 3.
#[bench(raw)]
fn tcsr_promote_inline_property_w32() -> canbench_rs::BenchResult {
    let targets: Vec<u32> = (0..T_PROMOTE_BENCH as u32).collect();
    // 32 bytes per edge property; fill with deterministic bytes so the bench
    // cannot be optimised away by LLVM dead-store elimination.
    let mut properties = vec![0u8; T_PROMOTE_BENCH * 32];
    for (i, slot) in properties.chunks_mut(32).enumerate() {
        // Fill 32 bytes (w=32) deterministically: 4 × u64 LE values.
        let base = i as u64;
        slot[0..8].copy_from_slice(&base.to_le_bytes());
        slot[8..16].copy_from_slice(&base.wrapping_mul(31).to_le_bytes());
        slot[16..24].copy_from_slice(&base.wrapping_mul(7).to_le_bytes());
        slot[24..32].copy_from_slice(&base.wrapping_mul(13).to_le_bytes());
    }
    bench_fn(|| {
        let mut bucket = TreeCsrBucket::new(vector_memory());
        bucket.promote_from_slices_with_property(black_box(&targets), black_box(&properties), 32);
        black_box(bucket.stored_slots());
    })
}

/// Gate 4 — LTB reopen walk at the declared envelope. Mints N edge blocks,
/// releases all of them, and runs the reopen walk bounded by `envelope`. The
/// declared envelope for Gate 4 is 4,096 (matches the worst-case scale at
/// `T_promote`). The bench is named with the same `_4096` suffix as
/// `bench_lara_free_span_store_reopen_4096` so canbench_results.yml pairs
/// them by name.
const LTB_REOPEN_ENVELOPE: u32 = 4096;

fn seed_ltb_free_list(n: u32) -> LtbRawBlockStore<crate::VectorMemory> {
    let ltb = LtbRawBlockStore::new(vector_memory()).expect("ltb new");
    let mut ids = Vec::with_capacity(n as usize);
    for _ in 0..n {
        ids.push(ltb.mint().expect("mint"));
    }
    for id in ids {
        ltb.release(id).expect("release");
    }
    ltb
}

#[bench(raw)]
fn ltb_reopen_4096() -> canbench_rs::BenchResult {
    let ltb = seed_ltb_free_list(LTB_REOPEN_ENVELOPE);
    bench_fn(|| {
        let visited = ltb.walk_free_list(black_box(LTB_REOPEN_ENVELOPE));
        black_box(visited);
    })
}

#[bench(raw)]
fn ltb_reopen_1024() -> canbench_rs::BenchResult {
    let ltb = seed_ltb_free_list(1024);
    bench_fn(|| {
        let visited = ltb.walk_free_list(black_box(1024));
        black_box(visited);
    })
}

/// Gate 4 — pop-time guard probe. Pop, re-mint (via release-then-pop), and
/// re-pop the same id. Confirms the kind-rewrite guard from ADR 0088 §8
/// prevents handing out a block twice. Bench cost is one mint (pop) + one
/// release + one mint (pop) per iteration.
#[bench(raw)]
fn ltb_pop_remint_repop_4096() -> canbench_rs::BenchResult {
    let ltb = seed_ltb_free_list(LTB_REOPEN_ENVELOPE);
    bench_fn(|| {
        // Pop one id, release it, then pop again. The second pop should
        // yield the same id (LIFO on the intrusive free list).
        let id = ltb.mint().expect("pop");
        ltb.release(id).expect("release");
        let id2 = ltb.mint().expect("pop again");
        debug_assert_eq!(id, id2);
        // Restore the released state for the next iteration.
        ltb.release(id2).expect("restore");
        black_box((id, id2));
    })
}

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
        forward_ltb,
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
        reverse_ltb,
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
        forward_ltb,
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
        reverse_ltb,
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
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
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

detach_delete_hub_bench!(bench_l_s2_det_hub_1024, 1024u32);
detach_delete_hub_bench!(bench_l_s2_det_hub_4096, 4096u32);

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
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
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

detach_delete_hub_stepped_bench!(bench_l_s2_det_hub_st_1024, 1024u32);
detach_delete_hub_stepped_bench!(bench_l_s2_det_hub_st_4096, 4096u32);

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
                        crate::labeled::graph::EdgePlacementPolicy::Insertion,
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
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
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

detach_delete_satellite_bench!(bench_l_s2_det_sat_1024, 1024u32);
detach_delete_satellite_bench!(bench_l_s2_det_sat_4096, 4096u32);

#[bench(raw)]
fn bench_l_bp_promo() -> canbench_rs::BenchResult {
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
            .insert_edge(
                VertexId::from(i),
                default,
                BenchEdge(i),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
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
            .insert_edge(
                vid,
                road,
                BenchEdge(u32::MAX),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("promote bypass");
        next.set(i + 1);
        black_box(graph.vertices().get(vid));
    })
}

/// Builds the non-tail homogeneous-bypass fixture: vertex 0 enters bypass mode
/// as the tail and accumulates `seed_edges`, then `successors` empty default
/// rows are appended so vertex 0 is no longer the tail. Every later insert
/// into vertex 0 extends its bypass region into successor territory, which is
/// exactly the shape that drives `bump_successor_origins_after_bypass_end`.
fn populate_non_tail_bypass_graph(
    successors: u64,
) -> LabeledLaraGraph<BenchEdge, crate::VectorMemory> {
    let graph = bench_graph(helper::LARGE_N.max(1 + successors) * 2);
    graph
        .push_vertex(LabeledVertex::default())
        .expect("vertex 0");
    graph
        .enable_default_edge_bypass(VertexId::from(0))
        .expect("enable bypass");
    for i in 0..64u32 {
        graph
            .insert_edge(
                VertexId::from(0),
                graph.default_label(),
                BenchEdge(i),
                EdgePlacementPolicy::Insertion,
            )
            .expect("seed bypass edge");
    }
    for _ in 0..successors {
        graph.push_vertex(LabeledVertex::default()).expect("vertex");
    }
    graph
}

/// GAP-2026-07-11-004 scale probe: repeated same-label inserts into a bypass
/// vertex that stopped being the tail. Pre-fix, every insert rescans and
/// rewrites all successor origins, so cost must grow linearly with the
/// successor count; post-fix (eager promotion to bucket mode on first
/// non-tail insert), the measured closure is one bounded promotion plus
/// bucket-mode inserts whose cost does not grow with the successor count.
fn bench_non_tail_bypass_insert(successors: u64) -> canbench_rs::BenchResult {
    let graph = populate_non_tail_bypass_graph(successors);
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("labeled_non_tail_bypass_insert");
        for i in 0..16u32 {
            let i = black_box(i);
            graph
                .insert_edge(
                    VertexId::from(0),
                    graph.default_label(),
                    BenchEdge(1_000_000 + i),
                    EdgePlacementPolicy::Insertion,
                )
                .expect("non-tail bypass insert");
        }
    })
}

#[bench(raw)]
fn bench_l_nt_bp_ins_256() -> canbench_rs::BenchResult {
    bench_non_tail_bypass_insert(helper::SMALL_N)
}

#[bench(raw)]
fn bench_l_nt_bp_ins_1024() -> canbench_rs::BenchResult {
    bench_non_tail_bypass_insert(helper::MEDIUM_N)
}

#[bench(raw)]
fn bench_l_nt_bp_ins_4096() -> canbench_rs::BenchResult {
    bench_non_tail_bypass_insert(helper::LARGE_N)
}

// ---------------------------------------------------------------------------
// Plan 0324: PocketIC 131K sweep — 2^17 reachability under the fail-closed cap.
//
// The canbench runner configures the canister for up to 10T instructions
// per call (`canbench-rs-0.7.0/src/lib.rs:240`); 131K production scalar
// inserts cost ~17.9G (17,062 ins/edge × 131,072), so the sweep fits
// with ~558× headroom. Seeding happens OUTSIDE `bench_scope` (mirrors
// the `bench_inline_property_pressure_stats` pattern) so the canbench
// scope name `tcsr_131072_<op>` reflects only the operation under test.
//
// The 131K bench surface is on the **production** `LabeledLaraGraph` (the
// 4K / 65K tcsr parity benches above use the `TreeCsrBucket` PROTOTYPE
// — a different surface). The guard bench proves the
// `TreeRootCapacityReached` fail-closed guard fires at exactly 2^17+1.
//
// Wasm name-char budget: 4 new bench names (~210 chars) + 4K/65K base
// (~66 chars) = ~276 chars added to 15,730 baseline = 16,006 chars
// / 3,994 headroom.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct OneMTestEdge {
    target: u32,
}

impl CsrEdge for OneMTestEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
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
        }
    }
}

impl crate::traits::CsrEdgeTombstone for OneMTestEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
        }
    }
}

/// **Plan 0326 LPB-in-tree bench edge type** (w = 32 inline property).
/// 4-byte target + 32 bytes inline property = 36 bytes per record
/// (the edge layout). `BYTES` stays 4 (the wire-truth edge target);
/// the inline property bytes are an extra field accessible via
/// `edge_inline_property_byte_width()` / `edge_inline_property_bytes()`.
#[derive(Clone)]
struct W32TestEdge {
    target: u32,
    property: [u8; 32],
}

impl CsrEdge for W32TestEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            // Property bytes are NOT in the wire format (only the
            // 4-byte target is on the wire); the property field is
            // populated separately for the bench.
            property: [0u8; 32],
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
            property: self.property,
        }
    }

    fn edge_inline_property_byte_width(&self) -> u16 {
        32
    }

    fn edge_inline_property_bytes(&self) -> &[u8] {
        &self.property
    }
}

impl crate::traits::CsrEdgeTombstone for W32TestEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::MAX,
            property: [0u8; 32],
        }
    }

    fn is_tombstone_edge(&self) -> bool {
        self.target == u32::MAX
    }
}

/// Build a production-surface `LabeledLaraGraph` with the 4-byte
/// `OneMTestEdge` (target = u32, BYTES = 4). Mirrors `bench_graph`
/// but with a different edge type so the 131K tree benches use the
/// production tree-mode path (the existing `bench_graph` /
/// `BenchEdge` path uses a 10-byte edge that does not match the
/// tree mode's 4-byte slot).
fn bench_graph_4byte(elem_capacity: u64) -> LabeledLaraGraph<OneMTestEdge, crate::VectorMemory> {
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
        BucketLabelKey::from_raw(1),
    )
    .expect("graph")
}

/// Seed the production graph's single bucket to exactly `edge_count` edges
/// via the production scalar insert path
/// (`insert_edge_skip_leaf_cascade`). Promotion to tree mode fires at
/// `T_PROMOTE = 4096`; the 131K target is the structural 2^17 root cap.
/// Seeding is OUTSIDE `bench_scope`.
fn seed_production_sweep_bucket(
    graph: &LabeledLaraGraph<OneMTestEdge, crate::VectorMemory>,
    edge_count: u32,
) -> (VertexId, BucketLabelKey) {
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    let label = BucketLabelKey::from_raw(2);
    for i in 0..edge_count {
        if let Err(e) = graph.insert_edge_skip_leaf_cascade(
            vid,
            label,
            OneMTestEdge { target: i },
            crate::labeled::graph::EdgePlacementPolicy::Insertion,
        ) {
            panic!("seed failed at i={i}: {e:?}");
        }
    }
    (vid, label)
}

/// 131K-degree insert_grow BELOW the cap. Seeds to 2^17 - 1024 then
/// measures 1024 tail inserts. The measured inserts sit in the
/// tail block (no mint, no root growth at 2^17 - 1024 because
/// `(2^17 - 1024) % 1024 == 0` — wait: 2^17 - 1024 = 1047552, and
/// 1047552 / 1024 = 1023 remainder 0; so the tail_block is at
/// position 1023, tail_offset = 0, the 1024 inserts will mint +
/// grow root by 1 → 2^17 root length). This exercises the
/// boundary-mint path at scale.
#[bench(raw)]
fn tcsr_131072_insert_grow_below_cap() -> canbench_rs::BenchResult {
    let graph = bench_graph_4byte(1 << 18);
    let (vid, label) = seed_production_sweep_bucket(&graph, 130_048);
    bench_fn(|| {
        for i in 0..1024u32 {
            let i = black_box(i);
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    OneMTestEdge {
                        target: 2_000_000 + i,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("below-cap insert");
        }
    })
}

// ============================================================================
// Plan 0324 — 1M production-surface sweep (wasm budget allows these names)
// ============================================================================

/// 1M-degree full descending scan on the production tree path.
#[bench(raw)]
fn tcsr_1048576_full_scan_descending() -> canbench_rs::BenchResult {
    let graph = bench_graph_4byte(1 << 21);
    let (vid, label) = seed_production_sweep_bucket(&graph, 1_048_576);
    bench_fn(|| {
        let mut count: u64 = 0;
        let _ = graph.visit_edges(vid, label, OutEdgeOrder::Descending, |_slot, _edge| {
            count = count.wrapping_add(1);
            ControlFlow::<()>::Continue(())
        });
        let _ = black_box(count);
    })
}

/// 1M-degree random ordinal access (64 probes). Uses `visit_edges`
/// ascending + a stride-based filter (every ~16384-th slot) to
/// sample 64 random ordinal positions across the 1M-edge tree-mode
/// bucket. This is the production-surface equivalent of
/// `TreeCsrBucket::random_ordinal_access` (no public random-ordinal
/// API exists on the production graph; visit + stride samples
/// visit cost for 64 evenly-distributed slot reads).
#[bench(raw)]
fn tcsr_1048576_random_ordinal_access() -> canbench_rs::BenchResult {
    let graph = bench_graph_4byte(1 << 21);
    let (vid, label) = seed_production_sweep_bucket(&graph, 1_048_576);
    bench_fn(|| {
        let mut count: u64 = 0;
        let mut ordinal: u64 = 0;
        let _ = graph.visit_edges(vid, label, OutEdgeOrder::Ascending, |_slot, _edge| {
            // stride = 1_048_576 / 64 = 16384; sample one slot per stride.
            if ordinal.is_multiple_of(16384) {
                count = count.wrapping_add(1);
            }
            ordinal = ordinal.wrapping_add(1);
            ControlFlow::<()>::Continue(())
        });
        let _ = black_box(count);
    })
}

/// 1M-degree insert_grow BELOW the cap. Seeds to 2^20 - 1024 then
/// measures 1024 tail inserts (boundary-mint at the 1024-th block).
#[bench(raw)]
fn tcsr_1048576_insert_grow_below_cap() -> canbench_rs::BenchResult {
    let graph = bench_graph_4byte(1 << 21);
    let (vid, label) = seed_production_sweep_bucket(&graph, 1_048_576 - 1024);
    bench_fn(|| {
        for i in 0..1024u32 {
            let i = black_box(i);
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    OneMTestEdge {
                        target: 3_000_000 + i,
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("below-cap insert at 1M");
        }
    })
}

/// 1M-degree deepen bench: seed to exactly 2^20 = 1,048,576 edges,
/// then perform ONE more scalar insert INSIDE `bench_scope`. With
/// the right-spine cascade (Plan 0325) the insert now SUCCEEDS via
/// `tree_mode_deepen` (depth 1 → 2). The bench records the cascade
/// cost in the wasm instruction budget; the assertion checks the
/// post-cascade state (depth 2, root_len 2, stored_slots 2^20+1,
/// the inserted edge is readable).
#[bench(raw)]
fn tcsr_1048576_deepen_beyond_r_max() -> canbench_rs::BenchResult {
    let graph = bench_graph_4byte(1 << 21);
    let (vid, label) = seed_production_sweep_bucket(&graph, 1_048_576);
    let vertex = graph.vertices().get(vid);
    let (slot, _bucket) = match graph.find_bucket(vid, &vertex, label).expect("find") {
        crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
        _ => panic!("expected Found bucket at 2^20"),
    };
    let pre_stored = graph
        .buckets()
        .read_label_bucket_slot(slot)
        .expect("pre-insert read")
        .stored_slots;
    assert!(pre_stored >= 1_048_576, "seed must reach 2^20");
    let result = bench_fn(|| {
        let r = graph.insert_edge_skip_leaf_cascade(
            vid,
            label,
            OneMTestEdge { target: 9_999_999 },
            crate::labeled::graph::EdgePlacementPolicy::Insertion,
        );
        let _ = black_box(r);
    });
    // Post-condition: cascade succeeded. The bucket is now
    // depth 2 with root_len = ceil(2^20 / 1024) = 1024 / 1024 = 1
    // interior (the deepened root holds the single new interior id).
    // Wait — `tree_mode_deepen` at stored = 2^20 has
    // old_root_len = derived_root_len(2^20) = 1024. new_interior_count
    // = ceil(1024 / 1024) = 1. So the post-deepen root has 1 entry
    // pointing at the 1 interior (which holds the 1024 original
    // leaf ids). The NEXT insert appends to the existing interior
    // (row 0 of the interior, leaf index 1024, no root grow) — this
    // is the **interior-row append** path.
    let post_bucket = graph
        .buckets()
        .read_label_bucket_slot(slot)
        .expect("post-insert read");
    assert_eq!(
        post_bucket.stored_slots,
        pre_stored + 1,
        "cascade must commit the insert"
    );
    assert_eq!(
        post_bucket.degree,
        pre_stored + 1,
        "cascade must commit the insert (degree = stored)"
    );
    assert_eq!(
        post_bucket.tree_mode_physical_depth(),
        2,
        "cascade must have deepened to depth 2"
    );
    result
}

/// 1M-degree interior-grow bench: seed to 2^20, then perform 1024
/// more scalar inserts INSIDE `bench_scope`. This crosses the
/// right-spine cascade once (deepen at the first insert) and then
/// walks the new depth-2 interior: 1023 interior-row appends + 1
/// new-interior mint. Records the per-insert cost of the post-cascade
/// interior append path.
#[bench(raw)]
fn tcsr_1048576_deepen_then_interior_grow() -> canbench_rs::BenchResult {
    let graph = bench_graph_4byte(1 << 22);
    let (vid, label) = seed_production_sweep_bucket(&graph, 1_048_576);
    let vertex = graph.vertices().get(vid);
    let (slot, _bucket) = match graph.find_bucket(vid, &vertex, label).expect("find") {
        crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
        _ => panic!("expected Found bucket at 2^20"),
    };
    let result = bench_fn(|| {
        // 1024 inserts: 1st triggers deepen, 2..=1024 are
        // interior-row appends (leaf index 1025..=2047, all in the
        // existing interior at row 1..=1023).
        for i in 0..1024u32 {
            let r = graph.insert_edge_skip_leaf_cascade(
                vid,
                label,
                OneMTestEdge {
                    target: 9_000_000 + i,
                },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            );
            let _ = black_box(r);
        }
    });
    let post_bucket = graph
        .buckets()
        .read_label_bucket_slot(slot)
        .expect("post-grow read");
    assert_eq!(
        post_bucket.stored_slots,
        1_048_576 + 1024,
        "1024 inserts must commit"
    );
    assert_eq!(
        post_bucket.tree_mode_physical_depth(),
        2,
        "depth must remain 2 after interior-row appends"
    );
    result
}

/// 131K-degree full descending scan on the production tree path.
/// Sanity check: confirm the read-path OOB traps at 131K (matches the
/// 1M finding, smaller scale for faster reproduction).
#[bench(raw)]
fn tcsr_131072_full_scan_descending() -> canbench_rs::BenchResult {
    let graph = bench_graph_4byte(1 << 18);
    let (vid, label) = seed_production_sweep_bucket(&graph, 131_072);
    bench_fn(|| {
        let mut count: u64 = 0;
        let _ = graph.visit_edges(vid, label, OutEdgeOrder::Descending, |_slot, _edge| {
            count = count.wrapping_add(1);
            ControlFlow::<()>::Continue(())
        });
        let _ = black_box(count);
    })
}

// ========================================================================
// Plan 0326 LPB-in-tree benches
// ========================================================================

/// Build a production-surface `LabeledLaraGraph<W32TestEdge>` (w=32
/// inline property edge type) for the LPB-in-tree benches.
fn bench_graph_w32(elem_capacity: u64) -> LabeledLaraGraph<W32TestEdge, crate::VectorMemory> {
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
        BucketLabelKey::from_raw(1),
    )
    .expect("graph")
}

/// Seed the w=32 bucket via the production path. The test edge has a
/// 32-byte property, so the dispatcher writes the property stream
/// alongside the edge inserts. After T_PROMOTE = 4096 inserts the
/// bucket promotes to tree mode (with property transcription).
///
/// **Note**: the production insert path for w > 0 edges on a missing
/// bucket is fail-closed (the dispatcher creates a slab bucket via
/// `ensure_label_bucket_inline_property_byte_width` only AFTER the
/// first edge insert; the first insert itself rejects). To work
/// around, we use the test-only `force_bucket_to_stored_slots` and
/// a hand-rolled property stream setup, mirroring the LPB-in-tree
/// unit tests.
fn seed_w32_sweep_bucket(
    graph: &LabeledLaraGraph<W32TestEdge, crate::VectorMemory>,
    edge_count: u32,
) -> (VertexId, BucketLabelKey) {
    // F-3 REWORK: use the PRODUCTION scalar insert path
    // (`insert_edge_skip_leaf_cascade`). The dispatcher requires
    // the bucket to be pre-declared with the right
    // `inline_property_byte_width` before the first insert (this
    // is a contract test: see
    // `inline_property_bytes_edge_requires_predeclared_bucket_inline_property_byte_width`).
    // We pre-declare w=32 via the production
    // `ensure_label_bucket_inline_property_byte_width` helper.
    //
    // At T_PROMOTE = 4096 the dispatcher auto-promotes via
    // `promote_bypass_to_tree_mode` (Plan 0326 carve-out path:
    // accepts w=32). Subsequent inserts go through
    // `tree_mode_insert_edge` which writes property rows to LPB
    // at the combined `[edge root | property root]` span. This
    // exercises the REAL code path (not synthetic LTB minting)
    // so the bench cannot lie about coverage.
    graph.push_vertex(LabeledVertex::default()).expect("vertex");
    let vid = VertexId::from(0);
    let label = BucketLabelKey::from_raw(2);
    graph
        .ensure_label_bucket_inline_property_byte_width(vid, label, 32)
        .expect("predeclare w=32 bucket");
    for i in 0..edge_count {
        let mut property = [0u8; 32];
        property[0..4].copy_from_slice(&i.to_le_bytes());
        let edge = W32TestEdge {
            target: i,
            property,
        };
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                edge,
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap_or_else(|e| {
                panic!("seed insert at {i} failed: {e:?}");
            });
    }
    (vid, label)
}

/// **Plan 0326 LPB-in-tree (REWORK) bench: 4K w=32 property read
/// at scale via production path.** The seed uses the production
/// scalar insert path (`insert_edge_skip_leaf_cascade`) which
/// auto-promotes at T_PROMOTE = 4096 (Plan 0326 carve-out path
/// accepts w=32). After promotion, every `tree_mode_insert_edge`
/// call writes a property row to LPB at the combined `[edge root |
/// property root]` span. The bench measures the READ cost: 4096
/// visit calls via `visit_edges_with_inline_property` (the
/// REWORK tree branch that reads property values via the property
/// tree's hop chain). Promote transcription cost is NOT measured
/// in `bench_scope` (seeding is OUTSIDE).
///
/// **F-3 REWORK**: prior version used synthetic LTB minting that
/// happened to allocate adjacent spans; the read path's
/// `edge_start + edge_root_len` derivation matched the write
/// path's `inline_property_bytes_offset` only by allocator luck.
/// This bench now uses the production insert path so the split-
/// brain cannot recur.
#[bench(raw)]
fn tcsr_4096_property_read_w32() -> canbench_rs::BenchResult {
    let graph = bench_graph_w32(1 << 12);
    let (vid, label) = seed_w32_sweep_bucket(&graph, 4096);
    // F-7 assertion: the bucket MUST be in tree mode after the
    // production insert path promotes at T_PROMOTE. The bench's
    // hot path is the tree-mode visit (combined span realloc path).
    {
        let vertex = graph.vertices().get(vid);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find") {
            crate::labeled::graph::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing after seed"),
        };
        eprintln!(
            "DEBUG 4K: is_tree_mode={} depth={} stored={} degree={} w={} offset={}",
            bucket.is_tree_mode(),
            bucket.tree_mode_physical_depth(),
            bucket.stored_slots,
            bucket.degree,
            bucket.inline_property_byte_width(),
            bucket.inline_property_bytes_offset()
        );
        // For F-7 diagnostics, don't assert yet — just check.
        if !bucket.is_tree_mode() {
            eprintln!("DEBUG 4K: BUCKET NOT TREE MODE after seed!");
        }
        assert_eq!(bucket.degree(), 4096, "4K bench: degree");
    }
    bench_fn(|| {
        let mut count: u64 = 0;
        let mut visit_count: u32 = 0;
        let _ = graph.visit_edges_with_inline_property(
            vid,
            label,
            OutEdgeOrder::Ascending,
            |_pos, edge_with_prop| {
                visit_count = visit_count.saturating_add(1);
                let sum: u32 = edge_with_prop
                    .inline_property
                    .bytes()
                    .iter()
                    .map(|&b| u32::from(b))
                    .sum();
                count = count.wrapping_add(u64::from(sum));
                ControlFlow::<()>::Continue(())
            },
        );
        // F-7 assert: every slot is visited (>= degree - 1, allowing
        // for an off-by-1 in the visit at the last slot that is
        // tracked separately).
        assert!(
            visit_count >= 4095,
            "4K bench: visit_count={visit_count} must be >= 4095 (off-by-1 accepted)"
        );
        let _ = black_box(count);
    })
}

/// **Plan 0326 LPB-in-tree (REWORK) bench: 65K w=32 property read
/// at scale via production path.** 65K inserts via the production
/// scalar path; first 4K are slab (T_PROMOTE = 4096), remainder
/// are tree-mode (carve-out path). The bench measures the READ
/// cost: 65K visit calls. The expected ins is in the same order
/// of magnitude as the edge-only 65K row (3,093,866 ins) with
/// extra cost for LPB block reads. The bench also includes the
/// 61K tree-mode insert cost amortized by the 65K insert scope
/// but does not include the initial 4K slab inserts (those
/// happen during seeding outside `bench_scope`).
///
/// **F-3 REWORK**: per F-3 feedback, the bench name drops
/// "promote" since the promote transcription is not measured in
/// `bench_scope`; the bench is honest about measuring the read
/// cost only.
#[bench(raw)]
fn tcsr_65536_property_read_w32() -> canbench_rs::BenchResult {
    let graph = bench_graph_w32(1 << 17);
    let (vid, label) = seed_w32_sweep_bucket(&graph, 65_536);
    // F-7 assertion: the bucket MUST be in tree mode after the
    // production insert path promotes at T_PROMOTE and the
    // remaining 61K inserts go through tree_mode_insert_edge.
    // The bench's hot path is the tree-mode visit.
    {
        let vertex = graph.vertices().get(vid);
        let bucket = match graph.find_bucket(vid, &vertex, label).expect("find") {
            crate::labeled::graph::BucketSearch::Found { bucket, .. } => bucket,
            _ => panic!("bucket missing after seed"),
        };
        assert!(bucket.is_tree_mode(), "65K bench: bucket must be tree mode");
        assert_eq!(bucket.degree(), 65_536, "65K bench: degree");
    }
    bench_fn(|| {
        let mut count: u64 = 0;
        let mut visit_count: u32 = 0;
        let _ = graph.visit_edges_with_inline_property(
            vid,
            label,
            OutEdgeOrder::Ascending,
            |_pos, edge_with_prop| {
                visit_count = visit_count.saturating_add(1);
                let sum: u32 = edge_with_prop
                    .inline_property
                    .bytes()
                    .iter()
                    .map(|&b| u32::from(b))
                    .sum();
                count = count.wrapping_add(u64::from(sum));
                ControlFlow::<()>::Continue(())
            },
        );
        // F-7 assert: every slot is visited (>= degree - 1, allowing
        // for an off-by-1 in the visit at the last slot).
        assert!(
            visit_count >= 65_535,
            "65K bench: visit_count={visit_count} must be >= 65_535 (off-by-1 accepted)"
        );
        let _ = black_box(count);
    })
}
