//! Allocator-level address types and packed low-level references.

use candid::CandidType;
use gleaph_graph_kernel::NodeId;
use serde::{Deserialize, Serialize};

/// Physical byte address inside the stable-memory address space.
///
/// This is allocator-level metadata. Adjacency code should prefer surface-local
/// indexes such as [`EdgeIndex`](crate::low_level::EdgeIndex) over raw
/// addresses.
#[repr(transparent)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    CandidType,
)]
pub struct StableAddr(pub u64);

/// Packed 40-bit reference to a vertex-table ordinal.
///
/// This is a low-level adjacency reference, not a semantic graph-node id.
#[repr(transparent)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default, Serialize, Deserialize,
    CandidType,
)]
pub struct VertexRef(u32);

impl VertexRef {
    pub const MAX: u64 = u32::MAX as u64;

    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }

    #[inline]
    pub const fn to_u32(self) -> u32 {
        self.0
    }

    #[inline]
    pub const fn to_u64(self) -> u64 {
        self.0 as u64
    }

    #[inline]
    pub const fn as_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }

    #[inline]
    pub const fn to_be_bytes(self) -> [u8; 4] {
        self.0.to_be_bytes()
    }

    #[inline]
    pub const fn from_be_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_be_bytes(bytes))
    }
}

impl TryFrom<u64> for VertexRef {
    type Error = &'static str;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        if value > Self::MAX {
            return Err("vertex ref exceeds 32-bit packed layout");
        }
        Ok(Self(value as u32))
    }
}

impl From<VertexRef> for u64 {
    fn from(value: VertexRef) -> Self {
        value.to_u64()
    }
}

impl From<u8> for VertexRef {
    fn from(value: u8) -> Self {
        Self::try_from(value as u64).expect("u8 always fits in VertexRef")
    }
}

impl From<u16> for VertexRef {
    fn from(value: u16) -> Self {
        Self::try_from(value as u64).expect("u16 always fits in VertexRef")
    }
}

impl From<u32> for VertexRef {
    fn from(value: u32) -> Self {
        Self::try_from(value as u64).expect("u32 always fits in VertexRef")
    }
}

impl From<NodeId> for VertexRef {
    fn from(value: NodeId) -> Self {
        Self::new(value.to_u32())
    }
}

impl From<VertexRef> for NodeId {
    fn from(value: VertexRef) -> Self {
        NodeId::new(value.to_u32())
    }
}

/// Packed base-neighborhood locator.
///
/// Layout:
/// - high 24 bits: segment id
/// - low 40 bits: start slot within the segment
#[repr(transparent)]
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Default,
    Serialize,
    Deserialize,
    CandidType,
)]
pub struct EdgeRef(u64);

impl EdgeRef {
    pub const SEGMENT_ID_BITS: u32 = 24;
    pub const START_SLOT_BITS: u32 = 40;
    pub const MAX_SEGMENT_ID: u32 = (1 << Self::SEGMENT_ID_BITS) - 1;
    pub const MAX_START_SLOT: u64 = (1u64 << Self::START_SLOT_BITS) - 1;

    #[inline]
    pub const fn from_raw(raw: u64) -> Self {
        Self(raw)
    }

    #[inline]
    pub fn new(segment_id: u32, start_slot: u64) -> Self {
        assert!(
            segment_id <= Self::MAX_SEGMENT_ID,
            "segment id exceeds 24-bit edge-ref layout"
        );
        assert!(
            start_slot <= Self::MAX_START_SLOT,
            "start slot exceeds 40-bit edge-ref layout"
        );
        Self(((segment_id as u64) << Self::START_SLOT_BITS) | start_slot)
    }

    #[inline]
    pub const fn raw(self) -> u64 {
        self.0
    }

    #[inline]
    pub const fn segment_id(self) -> u32 {
        (self.0 >> Self::START_SLOT_BITS) as u32
    }

    #[inline]
    pub const fn start_slot(self) -> u64 {
        self.0 & Self::MAX_START_SLOT
    }

    #[inline]
    pub fn with_start_slot(self, start_slot: u64) -> Self {
        Self::new(self.segment_id(), start_slot)
    }

    #[inline]
    pub fn with_segment_id(self, segment_id: u32) -> Self {
        Self::new(segment_id, self.start_slot())
    }
}

const _: () = assert!(core::mem::size_of::<VertexRef>() == 4);
const _: () = assert!(core::mem::size_of::<EdgeRef>() == 8);

#[cfg(test)]
mod tests {
    use super::{EdgeRef, VertexRef};
    use gleaph_graph_kernel::NodeId;

    #[test]
    fn vertex_ref_roundtrips_32_bit_payload() {
        let raw = 0xabcd_1234_u64;
        let vertex = VertexRef::try_from(raw).expect("32-bit value should fit");

        assert_eq!(u64::from(vertex), raw);
        assert_eq!(vertex.as_bytes(), [0xab, 0xcd, 0x12, 0x34]);
    }

    #[test]
    fn vertex_ref_converts_from_kernel_node_id_losslessly() {
        let node = NodeId::try_from(77_u64).expect("node id fits");
        let vertex = VertexRef::from(node);

        assert_eq!(NodeId::from(vertex), node);
        assert_eq!(u64::from(vertex), 77);
    }

    #[test]
    fn edge_ref_packs_segment_and_start_slot() {
        let edge = EdgeRef::new(0x0000_abcd, 0x0012_3456_789a);

        assert_eq!(edge.segment_id(), 0x0000_abcd);
        assert_eq!(edge.start_slot(), 0x0012_3456_789a);
        assert_eq!(edge.raw(), 0x00ab_cd12_3456_789a);
    }
}
