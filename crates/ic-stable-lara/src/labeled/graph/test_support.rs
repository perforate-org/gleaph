//! Shared fixtures for labeled graph tests.

pub use super::LabeledLaraGraph;
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
        256,
        default_label,
    )
    .unwrap();
    graph.push_vertex(LabeledVertex::default()).unwrap();
    graph
}

pub fn test_graph() -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
    test_graph_with_default(BucketLabelKey::directed_from_index(1))
}

pub fn flag_tombstone_graph() -> LabeledLaraGraph<FlagTombstoneEdge, crate::VectorMemory> {
    LabeledLaraGraph::new(
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
        256,
        BucketLabelKey::directed_from_index(1),
    )
    .unwrap()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PayloadTestEdge {
    pub target: u32,
    pub slot_index: u32,
    pub value: Vec<u8>,
    pub payload_len: u16,
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
            payload_len: len,
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
        &self.value[..usize::from(self.payload_len)]
    }

    fn with_stored_payload_bytes(mut self, width: u16, bytes: &[u8]) -> Self {
        let len = usize::from(width).min(bytes.len());
        self.value = bytes[..len].to_vec();
        self.payload_len = u16::try_from(len).expect("test value width fits u16");
        self
    }
}

impl CsrEdgeTombstone for PayloadTestEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            slot_index: 0,
            value: Vec::new(),
            payload_len: 0,
        }
    }
}

pub fn payload_test_graph() -> LabeledLaraGraph<PayloadTestEdge, crate::VectorMemory> {
    payload_test_graph_with_capacity(256)
}

pub fn payload_test_graph_with_capacity(
    elem_capacity: u64,
) -> LabeledLaraGraph<PayloadTestEdge, crate::VectorMemory> {
    LabeledLaraGraph::new(
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
        elem_capacity,
        BucketLabelKey::directed_from_index(1),
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
        1 << 20,
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
