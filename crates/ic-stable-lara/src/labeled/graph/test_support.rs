//! Shared fixtures for labeled graph tests.

pub use super::error::LabeledOperationError;
pub use super::iter::LabeledEdgeInlineValueBatchScratch;
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
        crate::labeled::InitialCapacities::uniform(256),
        default_label,
        32,
    )
    .unwrap();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    graph
}

pub fn test_graph() -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
    test_graph_with_default(BucketLabelKey::directed_from_index(1))
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
        crate::labeled::InitialCapacities::uniform(256),
        BucketLabelKey::directed_from_index(1),
        32,
    )
    .unwrap()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadTestEdge {
    pub target: u32,
    pub slot_index: u32,
    pub value: Vec<u8>,
    pub inline_value_len: u16,
}

impl PayloadTestEdge {
    pub fn with_bytes(target: u32, bytes: &[u8]) -> Self {
        let len = u16::try_from(bytes.len()).expect("test value fits u16");
        let mut value = vec![0u8; bytes.len()];
        value.copy_from_slice(bytes);
        Self {
            target,
            slot_index: 0,
            value,
            inline_value_len: len,
        }
    }

    pub fn with_i32(target: u32, inline: i32) -> Self {
        Self::with_bytes(target, &inline.to_le_bytes())
    }
}

impl CsrEdge for PayloadTestEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            slot_index: 0,
            value: Vec::new(),
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
        &self.value[..usize::from(self.inline_value_len)]
    }

    fn with_stored_inline_value_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len());
        self.value = bytes[..len].to_vec();
        self.inline_value_len = u16::try_from(len).expect("test value width fits u16");
        self
    }
}

impl CsrEdgeTombstone for PayloadTestEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            slot_index: 0,
            value: Vec::new(),
            inline_value_len: 0,
        }
    }
}

pub fn inline_value_test_graph() -> LabeledLaraGraph<PayloadTestEdge, crate::VectorMemory> {
    inline_value_test_graph_with_capacity(256)
}

pub fn inline_value_test_graph_with_capacity(
    elem_capacity: u64,
) -> LabeledLaraGraph<PayloadTestEdge, crate::VectorMemory> {
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
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        BucketLabelKey::directed_from_index(1),
        32,
    )
    .unwrap()
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
pub fn labeled_memories() -> [crate::VectorMemory; 15] {
    std::array::from_fn(|_| mem())
}

/// Builds a fresh [`LabeledLaraGraph`] over `mems` (clones the handles so the
/// caller retains them for a later [`reopen_labeled_graph`]).
pub fn open_labeled_graph(
    mems: &[crate::VectorMemory; 15],
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
        crate::labeled::InitialCapacities::uniform(elem_capacity),
        default_label,
    )
    .unwrap()
}

/// Re-opens a [`LabeledLaraGraph`] from already-populated `mems`, modelling the
/// implicit stable-memory reattach a canister performs on `post_upgrade`.
pub fn reopen_labeled_graph(
    mems: &[crate::VectorMemory; 15],
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
    let _ = graph.asc_out_edges(hub).unwrap();
    let _ = graph.out_edges(hub).unwrap();
    let _: Vec<_> = graph
        .asc_out_edges_iter(hub)
        .unwrap()
        .collect::<Result<_, LabeledOperationError>>()
        .unwrap();
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

    let mut payload_scratch = LabeledEdgeInlineValueBatchScratch::default();
    for offset in 0..vertex.degree() {
        let slot = vertex.base_slot_start().saturating_add(u64::from(offset));
        let bucket = graph.buckets().read_label_bucket_slot(slot).unwrap();
        let label = bucket.bucket_label_key();
        let _ = graph.iter_edges_for_label(hub, label).unwrap();
        graph
            .for_each_edges_for_label_unchecked(hub, label, |_| ())
            .unwrap();
        graph
            .visit_out_edge_inline_value_batches_for_label(
                hub,
                label,
                OutEdgeOrder::Ascending,
                &mut payload_scratch,
                |_| (),
            )
            .unwrap();
        graph
            .visit_out_edge_inline_value_batches_for_label(
                hub,
                label,
                OutEdgeOrder::Descending,
                &mut payload_scratch,
                |_| (),
            )
            .unwrap();
    }
}
