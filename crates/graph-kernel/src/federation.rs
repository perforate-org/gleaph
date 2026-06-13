//! Distributed graph federation identifiers and placement types.

mod backfill_shard_state;
mod edge_posting_backfill;
mod encoded;
mod expand;
mod global_edge_id;
mod peer_sync;
mod posting_backfill;
mod router_error;
mod shard_id;

pub use backfill_shard_state::BackfillShardState;
pub use edge_posting_backfill::{EdgePostingBackfillArgs, EdgePostingBackfillResult};
pub use encoded::{
    ENCODED_EDGE_ID_BYTES, ENCODED_VERTEX_ID_BYTES, ElementIdEncodingKey, EncodedEdgeId,
    EncodedVertexId, decode_global_edge_id, decode_global_vertex_id, encode_global_edge_id,
    encode_global_vertex_id,
};
pub use expand::{
    FederatedExpandArgs, FederatedExpandDirection, FederatedExpandNeighbor,
    MAX_FEDERATED_EXPAND_PAYLOAD_BYTE_WIDTH,
};
pub use global_edge_id::GlobalEdgeId;
pub use peer_sync::{AddGraphPeerArgs, BootstrapGraphPeersArgs, RemoveGraphPeerArgs};
pub use posting_backfill::{PostingBackfillArgs, PostingBackfillResult};
pub use router_error::RouterError;
pub use shard_id::ShardId;

use crate::entry::GraphId;

use candid::{CandidType, Decode, Encode, Principal};
use ic_stable_lara::VertexId;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Dense vertex index within a single graph shard (`VertexId` in LARA).
pub type LocalVertexId = u32;

/// Router `commit_vertex_placement` argument (graph shard → router).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct CommitVertexPlacementArgs {
    pub local_vertex_id: LocalVertexId,
}

/// Authoritative graph shard drops router placement after deleting the vertex locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ReleaseVertexPlacementArgs {
    pub local_vertex_id: LocalVertexId,
}

/// Canonical global vertex key (`shard_id`, `local_vertex_id`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct GlobalVertexId {
    pub shard_id: ShardId,
    pub local_vertex_id: LocalVertexId,
}

/// Deprecated alias retained for mechanical migration in router stable maps.
pub type PhysicalPlacementKey = GlobalVertexId;

impl GlobalVertexId {
    #[inline]
    pub const fn new(shard_id: ShardId, local_vertex_id: LocalVertexId) -> Self {
        Self {
            shard_id,
            local_vertex_id,
        }
    }

    #[inline]
    pub const fn from_posting_hit(shard_id: ShardId, vertex_id: u32) -> Self {
        Self::new(shard_id, vertex_id)
    }

    #[inline]
    pub fn to_le_bytes(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0..4].copy_from_slice(&self.shard_id.to_le_bytes());
        out[4..8].copy_from_slice(&self.local_vertex_id.to_le_bytes());
        out
    }

    #[inline]
    pub fn from_le_bytes(bytes: [u8; 8]) -> Self {
        let mut shard = [0; 4];
        let mut local = [0; 4];
        shard.copy_from_slice(&bytes[0..4]);
        local.copy_from_slice(&bytes[4..8]);
        Self::new(ShardId::from_le_bytes(shard), u32::from_le_bytes(local))
    }
}

impl Storable for GlobalVertexId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_le_bytes()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_le_bytes())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes.as_ref());
        Self::from_le_bytes(raw)
    }
}

/// Current physical storage location of a vertex.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct PhysicalVertexLocation {
    pub shard_id: ShardId,
    pub local_vertex_id: LocalVertexId,
}

impl PhysicalVertexLocation {
    #[inline]
    pub const fn new(shard_id: ShardId, local_vertex_id: LocalVertexId) -> Self {
        Self {
            shard_id,
            local_vertex_id,
        }
    }

    #[inline]
    pub fn local_vertex(self) -> VertexId {
        VertexId::from(self.local_vertex_id)
    }
}

/// Authoritative vertex placement state owned by the router.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum VertexPlacement {
    Active(PhysicalVertexLocation),
}

/// Shard registration record returned by the router (`resolve_shard`).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ShardRegistryEntry {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub index_canister: Principal,
    pub graph_id: GraphId,
    pub registered_at_ns: u64,
}

/// Stable-memory wire envelope for [`ShardRegistryEntry`].
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
enum ShardRegistryStableRecord {
    V1(ShardRegistryEntry),
}

impl Storable for ShardRegistryEntry {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&ShardRegistryStableRecord::V1(self.clone()))
                .expect("encode ShardRegistryEntry"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&ShardRegistryStableRecord::V1(self)).expect("encode ShardRegistryEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), ShardRegistryStableRecord).expect("decode ShardRegistryEntry")
        {
            ShardRegistryStableRecord::V1(v1) => v1,
        }
    }

    const BOUND: Bound = Bound::Unbounded;
}

impl Storable for VertexPlacement {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode VertexPlacement"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode VertexPlacement")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), VertexPlacement).expect("decode VertexPlacement")
    }

    const BOUND: Bound = Bound::Unbounded;
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};
    use ic_stable_structures::Storable;

    #[test]
    fn global_vertex_id_le_bytes_roundtrip() {
        let key = GlobalVertexId::new(ShardId::new(0), 42);
        assert_eq!(key, GlobalVertexId::from_le_bytes(key.to_le_bytes()));
        assert_eq!(key, GlobalVertexId::from_posting_hit(ShardId::new(0), 42));
    }

    #[test]
    fn global_vertex_id_storable_roundtrip() {
        let key = GlobalVertexId::new(ShardId::new(1), 99);
        let bytes = key.to_bytes();
        assert_eq!(key, GlobalVertexId::from_bytes(bytes));
    }

    #[test]
    fn vertex_placement_storable_and_candid_roundtrip() {
        let placement = VertexPlacement::Active(PhysicalVertexLocation::new(ShardId::new(1), 10));
        let storable = placement.to_bytes();
        assert_eq!(placement, VertexPlacement::from_bytes(storable));
        let encoded = Encode!(&placement).expect("encode");
        let decoded: VertexPlacement = Decode!(&encoded, VertexPlacement).expect("decode");
        assert_eq!(placement, decoded);
    }

    #[test]
    fn shard_registry_entry_storable_roundtrip() {
        let entry = ShardRegistryEntry {
            shard_id: ShardId::new(1),
            graph_canister: Principal::anonymous(),
            index_canister: Principal::management_canister(),
            graph_id: GraphId::from_raw(1),
            registered_at_ns: 123,
        };
        let bytes = entry.to_bytes();
        assert_eq!(entry, ShardRegistryEntry::from_bytes(bytes));
    }

    #[test]
    fn placement_args_candid_roundtrip() {
        let commit = CommitVertexPlacementArgs {
            local_vertex_id: 42,
        };
        let release = ReleaseVertexPlacementArgs {
            local_vertex_id: 42,
        };
        for value in [Encode!(&commit).unwrap(), Encode!(&release).unwrap()] {
            assert!(!value.is_empty());
        }
    }
}
