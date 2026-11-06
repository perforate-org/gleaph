//! Shared fixtures for labeled graph tests.

use std::ops::ControlFlow;

pub use super::error::LabeledOperationError;
pub use super::iter::LabeledEdgeInlinePropertyBatchScratch;
pub use super::{LabeledLaraGraph, OutEdgeOrder};
pub use crate::labeled::{MAX_VERTEX_LABEL_BUCKETS, record::LabeledVertexFieldError};
use crate::labeled::{bucket_label_key::BucketLabelKey, record::LabeledVertex};
pub use crate::lara::operation_error::LaraOperationError;
pub use crate::traits::CsrVertex;
use crate::{
    VertexId,
    test_support::vector_memory,
    traits::{CsrEdge, CsrEdgeTombstone},
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TestEdge {
    pub target: u32,
}

impl CsrEdge for TestEdge {
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

impl CsrEdgeTombstone for TestEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlagTombstoneEdge {
    raw: u32,
}

impl FlagTombstoneEdge {
    const TOMBSTONE_BIT: u32 = 1 << 31;

    pub fn live(target: u32) -> Self {
        Self { raw: target }
    }
}

impl CsrEdge for FlagTombstoneEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            raw: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
        }
    }

    fn write_to(&self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.raw.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.raw & !Self::TOMBSTONE_BIT)
    }

    fn with_neighbor_vid(&self, vid: VertexId) -> Self {
        Self {
            raw: (self.raw & Self::TOMBSTONE_BIT) | u32::from(vid),
        }
    }

    fn is_deleted_slot(&self) -> bool {
        self.raw & Self::TOMBSTONE_BIT != 0
    }
}

impl CsrEdgeTombstone for FlagTombstoneEdge {
    fn tombstone_edge() -> Self {
        Self {
            raw: Self::TOMBSTONE_BIT,
        }
    }

    fn is_tombstone_edge(&self) -> bool {
        self.raw & Self::TOMBSTONE_BIT != 0
    }
}

pub fn mem() -> crate::VectorMemory {
    vector_memory()
}

pub fn test_graph_with_default(
    default_label: BucketLabelKey,
) -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
    test_graph_with_segment_size(32, default_label)
}

/// The same fixture with a chosen segment size.
///
/// The segment size is the only knob that changes how much leaf space a vertex gets: one leaf block
/// holds `segment_size * quota(segment_size)` slots and serves `segment_size` vertices, so
/// `InitialCapacities` cannot widen it. Production uses 16 (`DEFAULT_SEGMENT_SIZE`), where a vertex is
/// initially reserved one slot and grows by PMA relocation; the tests keep 32.
pub fn test_graph_with_segment_size(
    segment_size: u32,
    default_label: BucketLabelKey,
) -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
    // Most graph-unit fixtures exercise generic geometry and retain an explicit
    // segment32 layout. Production construction uses the segment16 default; the
    // segment-size-specific contract tests below exercise that policy directly.
    let graph = LabeledLaraGraph::new_with_segment_size(
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        crate::labeled::InitialCapacities::uniform(256),
        default_label,
        segment_size,
    )
    .unwrap();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    graph
}

pub fn test_graph() -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
    test_graph_with_default(BucketLabelKey::directed_from_index(1))
}

/// Builds the same fixture graph over another edge record type, for tests that
/// pin a width-dependent layout (tiny capacity, tree slot stride).
pub fn test_graph_with_edge<E: crate::traits::CsrEdge>() -> LabeledLaraGraph<E, crate::VectorMemory>
{
    let graph = LabeledLaraGraph::new_with_segment_size(
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        crate::labeled::InitialCapacities::uniform(256),
        BucketLabelKey::directed_from_index(1),
        32,
    )
    .unwrap();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    graph
}

/// Promotes an existing tiny bucket to the slab (test fixture helper).
///
/// ADR 0096 §3b: four inline edges stay inline at K=4, so fixtures that need a
/// slab-backed bucket with an exact span promote explicitly instead of relying
/// on the n-th insert to promote (the pre-K=4 idiom). The promotion transcribes
/// the inline edges into a span of exactly `degree` slots.
///
/// Panics if the bucket is missing or is not tiny.
pub(crate) fn promote_bucket_to_slab<E: CsrEdgeTombstone, M: ic_stable_structures::Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label: BucketLabelKey,
) -> u64 {
    let vertex = graph.vertices().get(vid);
    let (slot, bucket) = match graph.find_bucket(vid, &vertex, label).expect("find bucket") {
        crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
        crate::labeled::graph::BucketSearch::Missing { .. } => {
            panic!("promote_bucket_to_slab: bucket missing")
        }
    };
    assert!(
        bucket.is_tiny_mode(),
        "promote_bucket_to_slab: bucket is already promoted"
    );
    graph
        .promote_tiny_to_slab(vid, slot, &bucket)
        .expect("promote tiny bucket to slab");
    slot
}

/// Builds a tiny-mode bucket with the given inline targets for R2b tests
/// (ADR 0096). Birth-tiny flips separately; tests construct explicitly so the
/// suite stays green with zero production behavior change meanwhile.
///
/// The bucket is created through the normal slab birth path (quota side
/// effects included and harmless here), then converted in place. Anchor keeps
/// the slab-placed span start; R2b arms never read spans for tiny buckets.
/// Returns the bucket descriptor slot.
pub(crate) fn force_tiny_bucket<E: CsrEdgeTombstone, M: ic_stable_structures::Memory>(
    graph: &LabeledLaraGraph<E, M>,
    vid: VertexId,
    label: BucketLabelKey,
    targets: &[u32],
) -> u64 {
    use crate::labeled::record::LabelBucket;
    assert!(
        targets.len() <= LabelBucket::TINY_MAX_DEGREE as usize,
        "force_tiny_bucket: at most TINY_MAX_DEGREE targets"
    );
    let vertex = graph.vertices().get(vid);
    let (slot, bucket) = graph
        .find_or_create_bucket(vid, &vertex, label)
        .expect("create bucket");
    let mut bucket = bucket.try_enable_tiny_mode(4).expect("enable tiny");
    for (i, target) in targets.iter().enumerate() {
        bucket = bucket.with_tiny_target(i as u32, *target);
    }
    // K=4 birth shape: every target within the used width is written (holes are
    // layout-native tombstones, never zero), and the width itself is the tiny
    // bound. Slots outside the used width stay untouched and are never read.
    let degree = targets.len() as u32;
    let bucket = bucket
        .with_degree_field(degree)
        .with_tiny_used_width(degree);
    graph
        .buckets()
        .write_label_bucket_slot(slot, bucket)
        .expect("publish tiny bucket");
    // Census honesty (same principle as production write boundaries):
    // helper-created edges are live, so the global live-edge census must count
    // them. Leaf `actual` stays untouched (tiny edges occupy no leaf slots).
    let header = graph.edges().header();
    graph
        .edges()
        .set_num_edges(header.num_edges + targets.len() as u64);
    slot
}

pub fn flag_tombstone_graph() -> LabeledLaraGraph<FlagTombstoneEdge, crate::VectorMemory> {
    LabeledLaraGraph::new_with_segment_size(
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        crate::labeled::InitialCapacities::uniform(256),
        BucketLabelKey::directed_from_index(1),
        32,
    )
    .unwrap()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InlinePropertyTestEdge {
    pub target: u32,
    pub slot_index: u32,
    pub value: Vec<u8>,
    pub inline_property_len: u16,
}

impl InlinePropertyTestEdge {
    pub fn with_bytes(target: u32, bytes: &[u8]) -> Self {
        let len = u16::try_from(bytes.len()).expect("test value fits u16");
        let mut value = vec![0u8; bytes.len()];
        value.copy_from_slice(bytes);
        Self {
            target,
            slot_index: 0,
            value,
            inline_property_len: len,
        }
    }

    pub fn with_i32(target: u32, inline: i32) -> Self {
        Self::with_bytes(target, &inline.to_le_bytes())
    }
}

impl CsrEdge for InlinePropertyTestEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            slot_index: 0,
            value: Vec::new(),
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
        &self.value[..usize::from(self.inline_property_len)]
    }

    fn with_stored_inline_property_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len());
        self.value = bytes[..len].to_vec();
        self.inline_property_len = u16::try_from(len).expect("test value width fits u16");
        self
    }
}

impl CsrEdgeTombstone for InlinePropertyTestEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            slot_index: 0,
            value: Vec::new(),
            inline_property_len: 0,
        }
    }
}

pub fn inline_property_test_graph() -> LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory>
{
    inline_property_test_graph_with_capacity(256)
}

pub fn inline_property_test_graph_with_capacity(
    elem_capacity: u64,
) -> LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory> {
    LabeledLaraGraph::new_with_segment_size(
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        BucketLabelKey::directed_from_index(1),
        32,
    )
    .unwrap()
}

/// Segment-16 inline-property fixture with vertex 0 already pushed. The
/// segment-16 initial bucket quota is nonzero, so the bucket is slab-backed and
/// Unordered tombstone reuse with ordinal-aligned bytes is observable.
pub fn inline_property_test_graph_segment16()
-> LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory> {
    let graph = LabeledLaraGraph::new_with_segment_size(
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        crate::labeled::InitialCapacities::uniform(256),
        BucketLabelKey::directed_from_index(1),
        16,
    )
    .unwrap();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    graph
}

/// Injective payload independent of the physical edge ordinal.
pub(crate) fn retained_compaction_value(target: u32, width: u16) -> Vec<u8> {
    let mut bytes = vec![0xa5; usize::from(width)];
    bytes[..8].copy_from_slice(&(u64::from(target) + 1).to_le_bytes());
    bytes
}

pub(crate) fn retained_compaction_rows(
    graph: &LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory>,
    vid: VertexId,
    label: BucketLabelKey,
    order: OutEdgeOrder,
) -> Vec<(u32, u32, Vec<u8>)> {
    let mut rows = Vec::new();
    assert_eq!(
        graph
            .visit_edges_with_inline_property(vid, label, order, |slot, item| {
                rows.push((
                    slot.raw(),
                    item.edge.target,
                    item.inline_property.bytes().to_vec(),
                ));
                ControlFlow::<()>::Continue(())
            })
            .unwrap(),
        ControlFlow::Continue(())
    );
    rows
}

/// A valid, tightly backed prefix/run pair whose non-forcing fold must decline.
/// Label 3 also needs bounded slab moves; vertex 1 is an untouched neighboring owner.
pub(crate) fn retained_compaction_fixture(
    width: u16,
) -> LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory> {
    use super::{EdgePlacementPolicy, compact::bucket_content_slots};
    let graph = inline_property_test_graph();
    let vid = graph.push_vertex(LabeledVertex::default()).unwrap();
    for (raw, targets) in [(2, 0..8), (3, 9000..9004)] {
        let label = BucketLabelKey::from_raw(raw);
        graph
            .ensure_label_bucket_inline_property_byte_width(vid, label, width)
            .unwrap();
        for target in targets {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    InlinePropertyTestEdge::with_bytes(
                        target,
                        &retained_compaction_value(target, width),
                    ),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
    }
    // Establish dense prefixes before constructing holes or external identity keys.
    graph.vertex_layout(vid).compact_span().unwrap();
    let label = BucketLabelKey::from_raw(2);
    for target in 8..16 {
        graph
            .insert_edge_skip_leaf_cascade(
                vid,
                label,
                InlinePropertyTestEdge::with_bytes(
                    target,
                    &retained_compaction_value(target, width),
                ),
                EdgePlacementPolicy::Insertion,
            )
            .unwrap();
    }
    let peer = graph.push_vertex(LabeledVertex::default()).unwrap();
    graph
        .ensure_label_bucket_inline_property_byte_width(peer, label, width)
        .unwrap();
    graph
        .insert_edge_skip_leaf_cascade(
            peer,
            label,
            InlinePropertyTestEdge::with_bytes(19000, &retained_compaction_value(19000, width)),
            EdgePlacementPolicy::Insertion,
        )
        .unwrap();
    for slot in [0, 3, 9] {
        assert_eq!(
            graph
                .remove_edge_at_slot(vid, label, slot)
                .unwrap()
                .unwrap()
                .target,
            slot
        );
    }
    assert_eq!(
        graph
            .remove_edge_at_slot(vid, BucketLabelKey::from_raw(3), 0)
            .unwrap()
            .unwrap()
            .target,
        9000
    );
    graph
        .insert_edge_skip_leaf_cascade(
            vid,
            label,
            InlinePropertyTestEdge::with_bytes(16, &retained_compaction_value(16, width)),
            EdgePlacementPolicy::Insertion,
        )
        .unwrap();
    let vertex = graph.vertices().get(vid);
    let buckets = graph.read_vertex_label_buckets(&vertex).unwrap();
    let base = buckets[0].edge_start();
    let physical: u32 = buckets.iter().map(bucket_content_slots).sum();
    // Existing non-folding physical publisher; never a fictitious allocation/extent.
    graph
        .vertex_layout(vid)
        .rewrite_span(None, 0, false, Some((base, physical)))
        .unwrap();
    let buckets = graph
        .read_vertex_label_buckets(&graph.vertices().get(vid))
        .unwrap();
    let source = buckets[0];
    assert_eq!(source.stored_slots(), 8);
    assert_ne!(source.edge_spill_run(), crate::traits::NO_SPILL_RUN);
    assert_eq!(
        graph
            .edges()
            .spill_used_len(graph.edges().spill_mint(source.edge_spill_run()).unwrap()),
        9
    );
    assert_ne!(source.values_spill_run(), crate::traits::NO_SPILL_RUN);
    // ADR 0099: the value prefix is the edge prefix, so this is `stored_slots` and not a live-rank
    // count of 13.
    assert_eq!(
        source.inline_property_bytes_slab_slots(),
        source.stored_slots()
    );
    // ADR 0099: one value block per spilled row, so the value run mirrors the edge run's length
    // rather than holding a single block from before the rewrite.
    assert_eq!(
        graph
            .values
            .spill_used_len(graph.values.spill_mint(source.values_spill_run()).unwrap()),
        graph
            .edges()
            .spill_used_len(graph.edges().spill_mint(source.edge_spill_run()).unwrap()),
    );
    assert_eq!(buckets[1].edge_spill_run(), crate::traits::NO_SPILL_RUN);
    assert!(matches!(
        graph.compact_label_bucket_overflow_to_slab(vid, 0, source),
        Err(LabeledOperationError::Store(
            LaraOperationError::CollectAllocationOverflow
        ))
    ));
    assert_eq!(
        graph
            .read_vertex_label_buckets(&graph.vertices().get(vid))
            .unwrap(),
        buckets
    );
    assert_eq!(
        retained_compaction_rows(&graph, vid, label, OutEdgeOrder::Ascending),
        (0..17)
            .filter(|i| ![0, 3, 9].contains(i))
            .map(|i| (i, i, retained_compaction_value(i, width)))
            .collect::<Vec<_>>()
    );
    assert_eq!(graph.edges().header().num_edges, 18);
    crate::labeled::invariants::assert_labeled_layout_invariants(
        graph.vertices(),
        graph.buckets(),
        graph.edges(),
    );
    graph
}

/// A promoted tree sibling after a dense slab label. Promotion transcribes the
/// source holes at their high-water ordinals into real LTB-backed tree rows.
pub(crate) fn tree_compaction_fixture(
    width: u16,
) -> LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory> {
    use super::{EdgePlacementPolicy, bucket_content_slots};
    let graph = inline_property_test_graph();
    let vid = graph.push_vertex(LabeledVertex::default()).unwrap();
    let slab = BucketLabelKey::from_raw(2);
    let tree = BucketLabelKey::from_raw(3);
    let value = |target| {
        if width == 0 {
            Vec::new()
        } else {
            retained_compaction_value(target, width)
        }
    };
    for (label, targets) in [
        (slab, 20_000..20_016),
        (tree, 1000..1000 + super::T_PROMOTE),
    ] {
        if width > 0 {
            graph
                .ensure_label_bucket_inline_property_byte_width(vid, label, width)
                .unwrap();
        }
        for target in targets {
            graph
                .insert_edge_skip_leaf_cascade(
                    vid,
                    label,
                    InlinePropertyTestEdge::with_bytes(target, &value(target)),
                    EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }
        if label == slab {
            // Establish the slab prefix before introducing any tree descriptor.
            graph.vertex_layout(vid).compact_span().unwrap();
        }
    }
    for slot in [0, 3, 511, super::T_PROMOTE - 1] {
        assert_eq!(
            graph
                .remove_edge_at_slot(vid, tree, slot)
                .unwrap()
                .unwrap()
                .target,
            1000 + slot
        );
    }
    super::promote::promote_bucket_to_tree_mode(&graph, vid, tree).unwrap();
    let peer = graph.push_vertex(LabeledVertex::default()).unwrap();
    if width > 0 {
        graph
            .ensure_label_bucket_inline_property_byte_width(peer, slab, width)
            .unwrap();
    }
    graph
        .insert_edge_skip_leaf_cascade(
            peer,
            slab,
            InlinePropertyTestEdge::with_bytes(30_000, &value(30_000)),
            EdgePlacementPolicy::Insertion,
        )
        .unwrap();
    let buckets = graph
        .read_vertex_label_buckets(&graph.vertices().get(vid))
        .unwrap();
    let base = buckets[0].edge_start();
    let physical = buckets.iter().map(bucket_content_slots).sum();
    // No fabricated descriptor: reuse the non-folding publisher and its backing checks.
    graph
        .vertex_layout(vid)
        .rewrite_span(None, 0, false, Some((base, physical)))
        .unwrap();
    let buckets = graph
        .read_vertex_label_buckets(&graph.vertices().get(vid))
        .unwrap();
    assert_eq!(buckets.len(), 2);
    assert!(!buckets[0].is_tree_mode() && !buckets[0].is_tiny_mode());
    assert_eq!((buckets[0].degree(), buckets[0].stored_slots()), (16, 16));
    assert_eq!(buckets[0].edge_spill_run(), crate::traits::NO_SPILL_RUN);
    assert!(buckets[1].is_tree_mode());
    assert_eq!(
        (buckets[1].degree(), buckets[1].stored_slots()),
        (super::T_PROMOTE - 4, super::T_PROMOTE)
    );
    assert_eq!(buckets[1].edge_start(), buckets[0].edge_start() + 16);
    assert_eq!(
        graph.edges().header().num_edges,
        u64::from(super::T_PROMOTE + 13)
    );
    crate::labeled::invariants::assert_labeled_layout_invariants(
        graph.vertices(),
        graph.buckets(),
        graph.edges(),
    );
    graph
}

/// Dense multi-label hub fixture shared by span-release and hub regressions.
pub fn build_mixed_label_hub(
    labels: u16,
    edges_per_label: u32,
) -> (
    LabeledLaraGraph<TestEdge, crate::VectorMemory>,
    VertexId,
    VertexId,
) {
    let graph = LabeledLaraGraph::new(
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        mem(),
        crate::labeled::InitialCapacities::uniform(1 << 20),
        BucketLabelKey::from_raw(1),
    )
    .unwrap();
    let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
    let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
    for label_idx in 0..labels {
        let label = BucketLabelKey::from_raw(10_000 + label_idx);
        for edge_i in 0..edges_per_label {
            graph
                .insert_edge_skip_leaf_cascade(
                    hub,
                    label,
                    TestEdge {
                        target: u32::from(dst),
                    },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|e| panic!("label_idx={label_idx} edge_i={edge_i}: {e:?}"));
        }
    }
    (graph, hub, dst)
}

/// Per-label neighbor targets materialized via checked label iteration.
pub fn materialized_labeled_edges(
    graph: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
    vid: VertexId,
) -> Vec<(BucketLabelKey, Vec<u32>)> {
    let vertex = graph.vertices().get(vid);
    let base = vertex.base_slot_start();
    let mut out = Vec::new();
    for offset in 0..vertex.degree() {
        let slot = base.saturating_add(u64::from(offset));
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        let label = bucket.bucket_label_key();
        let targets = graph
            .iter_edges_for_label(vid, label)
            .unwrap()
            .into_iter()
            .map(|edge| edge.target)
            .collect();
        out.push((label, targets));
    }
    out
}

/// Active edge free-span count in the LARA edge allocator.
pub fn count_free_spans(graph: &LabeledLaraGraph<TestEdge, crate::VectorMemory>) -> usize {
    graph.edges().free_span_store().spans().len()
}

/// Fifteen fresh stable memories in [`LabeledLaraGraph`] constructor order.
///
/// Kept as owned handles so a graph can be built into them, dropped, and then
/// re-opened from the same bytes — the storage-engine analogue of a canister
/// upgrade boundary.
pub fn labeled_memories() -> [crate::VectorMemory; 16] {
    std::array::from_fn(|_| mem())
}

/// Builds a fresh [`LabeledLaraGraph`] over `mems` (clones the handles so the
/// caller retains them for a later [`reopen_labeled_graph`]).
pub fn open_labeled_graph(
    mems: &[crate::VectorMemory; 16],
    elem_capacity: u64,
    default_label: BucketLabelKey,
) -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
    LabeledLaraGraph::new(
        mems[0].clone(),
        mems[1].clone(),
        mems[2].clone(),
        mems[3].clone(),
        mems[4].clone(),
        mems[5].clone(),
        mems[6].clone(),
        mems[7].clone(),
        mems[8].clone(),
        mems[9].clone(),
        mems[10].clone(),
        mems[11].clone(),
        mems[12].clone(),
        mems[13].clone(),
        mems[14].clone(),
        mems[15].clone(),
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        default_label,
    )
    .unwrap()
}

/// Re-opens a [`LabeledLaraGraph`] from already-populated `mems`, modelling the
/// implicit stable-memory reattach a canister performs on `post_upgrade`.
pub fn reopen_labeled_graph(
    mems: &[crate::VectorMemory; 16],
    elem_capacity: u64,
    default_label: BucketLabelKey,
) -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
    LabeledLaraGraph::init(
        mems[0].clone(),
        mems[1].clone(),
        mems[2].clone(),
        mems[3].clone(),
        mems[4].clone(),
        mems[5].clone(),
        mems[6].clone(),
        mems[7].clone(),
        mems[8].clone(),
        mems[9].clone(),
        mems[10].clone(),
        mems[11].clone(),
        mems[12].clone(),
        mems[13].clone(),
        mems[14].clone(),
        mems[15].clone(),
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        default_label,
    )
    .unwrap()
}

/// Exercises common labeled scan iterators and batch readers for one hub vertex.
pub fn exercise_labeled_hub_scan_paths(
    graph: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
    hub: VertexId,
) {
    use crate::labeled::BucketDirectedness;

    let vertex = graph.vertices().get(hub);
    let _ = graph.out_edges(hub).unwrap();
    let _: Vec<_> = graph
        .desc_out_edges_iter(hub)
        .unwrap()
        .collect::<Result<_, LabeledOperationError>>()
        .unwrap();
    let _ = graph
        .iter_out_edges_by_directedness(hub, BucketDirectedness::Directed, OutEdgeOrder::Ascending)
        .unwrap();
    let _ = graph
        .iter_out_edges_undirected_only(hub, OutEdgeOrder::Descending)
        .unwrap();

    let mut inline_property_scratch = LabeledEdgeInlinePropertyBatchScratch::default();
    for offset in 0..vertex.degree() {
        let slot = vertex.base_slot_start().saturating_add(u64::from(offset));
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        let label = bucket.bucket_label_key();
        let _ = graph.iter_edges_for_label(hub, label).unwrap();
        let _ = graph
            .visit_edges(hub, label, OutEdgeOrder::Descending, |_slot, _edge| {
                ControlFlow::<()>::Continue(())
            })
            .unwrap();
        graph
            .visit_out_edge_inline_property_batches_for_label_next(
                hub,
                label,
                OutEdgeOrder::Ascending,
                &mut inline_property_scratch,
                |_| ControlFlow::<()>::Continue(()),
            )
            .map(|_| ())
            .unwrap();
        graph
            .visit_out_edge_inline_property_batches_for_label_next(
                hub,
                label,
                OutEdgeOrder::Descending,
                &mut inline_property_scratch,
                |_| ControlFlow::<()>::Continue(()),
            )
            .map(|_| ())
            .unwrap();
    }
}

/// Plan 0321 Step 1 test helper: forces the `(vid, label)` bucket
/// to tree mode by routing through the natural promotion path
/// (`force_bucket_to_stored_slots` + `fill_leg_slab_prefix` +
/// `promote_bucket_to_tree_mode`). Used by the tree-run
/// fail-closed guard tests in `batch_write_test` and any other
/// test that needs an already-tree bucket without paying the
/// 4096-insert promotion cost.
pub(crate) fn force_tree_mode_for_test(
    graph: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
    vid: crate::VertexId,
    label: BucketLabelKey,
) {
    use crate::labeled::graph::T_PROMOTE;
    use crate::labeled::graph::promote::{
        fill_leg_slab_prefix_pub, force_bucket_to_stored_slots_pub,
    };
    // Pre-populate the LEG slab prefix with deterministic data
    // and force stored_slots = T_PROMOTE, then promote.
    force_bucket_to_stored_slots_pub(graph, vid, label, T_PROMOTE);
    let vertex = graph.vertices().get(vid);
    let slot = match graph.find_bucket(vid, &vertex, label).expect("find") {
        crate::labeled::graph::BucketSearch::Found { slot, .. } => slot,
        crate::labeled::graph::BucketSearch::Missing { .. } => {
            panic!("bucket missing after force_bucket_to_stored_slots")
        }
    };
    let bucket = graph.buckets().read_label_bucket_slot(slot).expect("read");
    let edge_start = bucket.edge_start();
    fill_leg_slab_prefix_pub(graph, edge_start, T_PROMOTE);
    crate::labeled::graph::promote::promote_bucket_to_tree_mode_pub(graph, vid, label)
        .expect("promote");
}

/// Installs a hand-built tree-mode bucket by handing its child ids to the production writer.
///
/// A fixture knows two things: which blocks it minted for the edge tree and which for the value tree. Where
/// each root's addresses are stored, in what order the roots are written, and how the descriptor records the
/// value root's address are the production writer's business - so a layout change after ADR 0101 does not
/// reach the fixtures at all, and a fixture cannot spell out a layout the production code does not use.
///
/// `stored_slots`, `width` and the two depths describe the bucket; `edge_child_ids` are the edge tree's root
/// children (leaf ids at depth 1, interior ids above) and `value_child_ids` the value tree's.
#[allow(clippy::too_many_arguments)]
pub(crate) fn install_tree_bucket<E, M>(
    graph: &LabeledLaraGraph<E, M>,
    bucket_slot: u64,
    label: BucketLabelKey,
    stored_slots: u32,
    width: u16,
    physical_depth: u32,
    property_depth: u32,
    edge_child_ids: &[u32],
    value_child_ids: &[u32],
) -> crate::labeled::record::LabelBucket
where
    E: crate::traits::CsrEdgeTombstone,
    M: ic_stable_structures::Memory,
{
    // The span the bucket needs comes from the production geometry, not from a count this helper keeps: that is
    // what makes the helper follow a layout change instead of fighting it.
    let template = crate::labeled::record::LabelBucket::try_from_parts(
        label,
        0,
        stored_slots,
        stored_slots,
        width,
        0,
        0,
        -1,
        0,
    )
    .expect("try_from_parts")
    .with_tree_mode(true)
    .with_tree_mode_physical_depth(physical_depth)
    .with_tree_mode_property_depth(property_depth);
    let span_slots = super::compact::bucket_content_slots(&template);
    let edge_start = graph
        .edges()
        .allocate_span(u64::from(span_slots))
        .expect("allocate root span");
    let base = crate::labeled::record::LabelBucket::try_from_parts(
        label,
        edge_start,
        stored_slots,
        stored_slots,
        width,
        0,
        0,
        -1,
        0,
    )
    .expect("try_from_parts")
    .with_tree_mode(true)
    .with_tree_mode_physical_depth(physical_depth)
    .with_tree_mode_property_depth(property_depth);
    let mut edge_root_image: Vec<u8> = Vec::with_capacity(edge_child_ids.len() * E::BYTES);
    for id in edge_child_ids {
        edge_root_image.extend_from_slice(&id.to_le_bytes());
    }
    graph
        .tree_plane(base)
        .publish::<E>(
            bucket_slot,
            |b| b,
            edge_start,
            &edge_root_image,
            value_child_ids,
        )
        .expect("publish tree bucket")
}

pub(crate) use crate::test_support::CountMemory as CountingMemory;

/// Builds a fully counting-backed graph: all 16 memories share one
/// read/write counter pair. Returns the graph plus the two counters.
///
/// G6 scope: the counters observe EVERY stable read/write (descriptors,
/// vertices, slab, logs, LTB...). A tiny path asserting zero reads proves a
/// stronger claim than the ADR's per-store wording (no stable reads at all
/// outside... — see each test for the exact bound: descriptor + vertex reads
/// are structural and out of scope; the asserts pin edge-slab/log/ipb/span
/// silence via differential comparison against a slab control, not an
/// absolute zero).
pub fn counting_graph() -> (
    LabeledLaraGraph<TestEdge, CountingMemory>,
    std::rc::Rc<std::cell::Cell<usize>>,
    std::rc::Rc<std::cell::Cell<usize>>,
) {
    let (m0, reads, writes) = CountingMemory::counted();
    let m = || CountingMemory::sibling(&reads, &writes);
    let graph = LabeledLaraGraph::new(
        m0,
        m(),
        m(),
        m(),
        m(),
        m(),
        m(),
        m(),
        m(),
        m(),
        m(),
        m(),
        m(),
        m(),
        m(),
        m(),
        crate::labeled::InitialCapacities::uniform(256),
        BucketLabelKey::directed_from_index(1),
    )
    .unwrap();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    (graph, reads, writes)
}
