//! Canonical global edge identity (query-time CSR handle).

use super::{LocalVertexId, ShardId};
use crate::entry::EdgeSlotIndex;
use ic_stable_lara::VertexId;

/// Physical edge handle at query time: `(shard_id, owner_local, edge_slot_index)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GlobalEdgeId {
    pub shard_id: ShardId,
    pub owner_vertex_id: LocalVertexId,
    pub edge_slot_index: EdgeSlotIndex,
}

impl GlobalEdgeId {
    #[inline]
    pub const fn new(
        shard_id: ShardId,
        owner_vertex_id: LocalVertexId,
        edge_slot_index: EdgeSlotIndex,
    ) -> Self {
        Self {
            shard_id,
            owner_vertex_id,
            edge_slot_index,
        }
    }

    #[inline]
    pub fn to_le_bytes(self) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0..4].copy_from_slice(&self.shard_id.to_le_bytes());
        out[4..8].copy_from_slice(&self.owner_vertex_id.to_le_bytes());
        out[8..12].copy_from_slice(&self.edge_slot_index.to_le_bytes());
        out
    }

    #[inline]
    pub fn from_le_bytes(bytes: [u8; 12]) -> Self {
        let mut shard = [0; 4];
        let mut owner = [0; 4];
        let mut slot = [0; 4];
        shard.copy_from_slice(&bytes[0..4]);
        owner.copy_from_slice(&bytes[4..8]);
        slot.copy_from_slice(&bytes[8..12]);
        Self::new(
            ShardId::from_le_bytes(shard),
            u32::from_le_bytes(owner),
            EdgeSlotIndex::from_le_bytes(slot),
        )
    }

    #[inline]
    pub fn owner_vertex(self) -> VertexId {
        VertexId::from(self.owner_vertex_id)
    }
}
