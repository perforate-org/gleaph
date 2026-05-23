//! Shared fixtures for labeled graph tests.

pub use super::LabeledLaraGraph;
pub use crate::labeled::{
    MAX_VERTEX_LABEL_BUCKETS, record::LabeledVertexFieldError, slot_index::ValueWidthCode,
};
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

    fn write_to(self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.target)
    }

    fn with_neighbor_vid(self, vid: VertexId) -> Self {
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

    fn write_to(self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.raw.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.raw & !Self::TOMBSTONE_BIT)
    }

    fn with_neighbor_vid(self, vid: VertexId) -> Self {
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
        256,
        BucketLabelKey::directed_from_index(1),
    )
    .unwrap()
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ValuedTestEdge {
    pub target: u32,
    pub slot_index: u32,
    pub value: [u8; 8],
    pub value_len: u8,
}

impl ValuedTestEdge {
    pub fn with_u16(target: u32, inline: u16) -> Self {
        let mut value = [0u8; 8];
        value[0..2].copy_from_slice(&inline.to_le_bytes());
        Self {
            target,
            slot_index: 0,
            value,
            value_len: 2,
        }
    }

    pub fn with_i32(target: u32, inline: i32) -> Self {
        let mut value = [0u8; 8];
        value[0..4].copy_from_slice(&inline.to_le_bytes());
        Self {
            target,
            slot_index: 0,
            value,
            value_len: 4,
        }
    }
}

impl CsrEdge for ValuedTestEdge {
    const BYTES: usize = 4;

    fn read_from(bytes: &[u8]) -> Self {
        Self {
            target: u32::from_le_bytes(bytes[0..4].try_into().unwrap()),
            slot_index: 0,
            value: [0u8; 8],
            value_len: 0,
        }
    }

    fn write_to(self, bytes: &mut [u8]) {
        bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
    }

    fn neighbor_vid(&self) -> VertexId {
        VertexId::from(self.target)
    }

    fn with_neighbor_vid(self, vid: VertexId) -> Self {
        Self {
            target: u32::from(vid),
            ..self
        }
    }

    fn with_slot_index(self, slot_index: u32) -> Self {
        Self { slot_index, ..self }
    }

    fn edge_slot_index_raw(&self) -> u32 {
        self.slot_index
    }

    fn edge_value_byte_width(&self) -> u8 {
        self.value_len
    }

    fn edge_value_bytes(&self) -> &[u8] {
        &self.value[..usize::from(self.value_len)]
    }

    fn with_stored_value_bytes(mut self, width: u8, bytes: &[u8]) -> Self {
        self.value = [0u8; 8];
        let len = usize::from(width).min(bytes.len()).min(8);
        self.value[..len].copy_from_slice(&bytes[..len]);
        self.value_len = width;
        self
    }
}

impl CsrEdgeTombstone for ValuedTestEdge {
    fn tombstone_edge() -> Self {
        Self {
            target: u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL),
            slot_index: 0,
            value: [0u8; 8],
            value_len: 0,
        }
    }
}

pub fn valued_test_graph() -> LabeledLaraGraph<ValuedTestEdge, crate::VectorMemory> {
    valued_test_graph_with_capacity(256)
}

pub fn valued_test_graph_with_capacity(
    elem_capacity: u64,
) -> LabeledLaraGraph<ValuedTestEdge, crate::VectorMemory> {
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
        elem_capacity,
        BucketLabelKey::directed_from_index(1),
    )
    .unwrap()
}
