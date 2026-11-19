//! Canonical global edge identity (query-time CSR handle).

use super::{LocalVertexId, ShardId};
use crate::entry::EdgeLabelId;
use crate::entry::EdgeSlotIndex;
use ic_stable_lara::VertexId;

/// Number of little-endian bytes occupied by a [`GlobalEdgeId`].
///
/// Layout (LE):
///   `[0..4]`   `shard_id`
///   `[4..8]`   `owner_vertex_id`
///   `[8..12]`  `label_id` (widened from `u16` to `u32`; high 16 bits are zero)
///   `[12..16]` `edge_slot_index`
pub const GLOBAL_EDGE_ID_BYTES: usize = 16;

/// Physical edge handle at query time: `(shard_id, owner_local, label_id, edge_slot_index)`.
///
/// The label is part of the identity so two edges from the same source vertex under
/// different labels (which have independent per-bucket slot indices in LARA) cannot
/// collide on the wire. See [ADR 0090](../../../../design/adr/0090-edge-element-id-label-attribution.md).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GlobalEdgeId {
    pub shard_id: ShardId,
    pub owner_vertex_id: LocalVertexId,
    pub label_id: EdgeLabelId,
    pub edge_slot_index: EdgeSlotIndex,
}

impl GlobalEdgeId {
    #[inline]
    pub const fn new(
        shard_id: ShardId,
        owner_vertex_id: LocalVertexId,
        label_id: EdgeLabelId,
        edge_slot_index: EdgeSlotIndex,
    ) -> Self {
        Self {
            shard_id,
            owner_vertex_id,
            label_id,
            edge_slot_index,
        }
    }

    #[inline]
    pub fn to_le_bytes(self) -> [u8; GLOBAL_EDGE_ID_BYTES] {
        let mut out = [0u8; GLOBAL_EDGE_ID_BYTES];
        out[0..4].copy_from_slice(&self.shard_id.to_le_bytes());
        out[4..8].copy_from_slice(&self.owner_vertex_id.to_le_bytes());
        // Widen the `u16` label into a `u32` LE word; the high 16 bits stay zero so the
        // canonical encoding is one-to-one with the `u16` domain.
        out[8..12].copy_from_slice(&u32::from(self.label_id.raw()).to_le_bytes());
        out[12..16].copy_from_slice(&self.edge_slot_index.to_le_bytes());
        out
    }

    #[inline]
    pub fn from_le_bytes(bytes: [u8; GLOBAL_EDGE_ID_BYTES]) -> Self {
        let mut shard = [0; 4];
        let mut owner = [0; 4];
        let mut label = [0; 4];
        let mut slot = [0; 4];
        shard.copy_from_slice(&bytes[0..4]);
        owner.copy_from_slice(&bytes[4..8]);
        label.copy_from_slice(&bytes[8..12]);
        slot.copy_from_slice(&bytes[12..16]);
        // The high 16 bits of the `u32` label word are zero on the canonical form. Reject
        // (fail closed) instead of truncating: a nonzero high half would make two different
        // 16-byte identities decode to the same `GlobalEdgeId`, breaking the identity
        // uniqueness ADR 0090 relies on.
        let label_word = u32::from_le_bytes(label);
        assert!(
            label_word <= u32::from(u16::MAX),
            "global edge id label word has nonzero high 16 bits"
        );
        let label_raw = label_word as u16;
        Self::new(
            ShardId::from_le_bytes(shard),
            LocalVertexId::from_le_bytes(owner),
            EdgeLabelId::from_raw(label_raw),
            EdgeSlotIndex::from_le_bytes(slot),
        )
    }

    #[inline]
    pub fn owner_vertex(self) -> VertexId {
        VertexId::from(self.owner_vertex_id.raw())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let id = GlobalEdgeId::new(
            ShardId::new(1),
            9.into(),
            EdgeLabelId::from_raw(0x1234),
            EdgeSlotIndex::from_raw(3),
        );
        let bytes = id.to_le_bytes();
        assert_eq!(bytes.len(), GLOBAL_EDGE_ID_BYTES);
        assert_eq!(GlobalEdgeId::from_le_bytes(bytes), id);
    }

    #[test]
    fn nonzero_label_high_bits_fail_closed_instead_of_truncating() {
        let id = GlobalEdgeId::new(
            ShardId::new(0),
            0.into(),
            EdgeLabelId::from_raw(1),
            EdgeSlotIndex::from_raw(0),
        );
        let mut bytes = id.to_le_bytes();
        bytes[8..12].copy_from_slice(&0x1_0001u32.to_le_bytes());
        assert!(std::panic::catch_unwind(|| GlobalEdgeId::from_le_bytes(bytes)).is_err());
    }

    #[test]
    fn label_widening_is_lossless_for_catalog_range() {
        // Every catalog-allocatable `EdgeLabelId` (lower 15 bits) must encode to a unique
        // 16-byte word and round-trip back. This pins the widen/truncate contract.
        for raw in [0u16, 1, 0x7FFE, 0x7FFF] {
            let id = GlobalEdgeId::new(
                ShardId::new(0),
                0.into(),
                EdgeLabelId::from_raw(raw),
                EdgeSlotIndex::from_raw(0),
            );
            let bytes = id.to_le_bytes();
            assert_eq!(bytes[8..10], raw.to_le_bytes());
            assert_eq!(bytes[10..12], [0, 0]);
            assert_eq!(GlobalEdgeId::from_le_bytes(bytes), id);
        }
    }
}
