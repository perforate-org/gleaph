//! Distributed graph federation identifiers and placement types.

mod expand;
mod migration;
mod router_error;

pub use expand::{
    FederatedExpandNeighbor, FederatedIncomingExpandArgs, FederatedOutgoingExpandArgs,
};
pub use migration::{
    ExportedEdgeTarget, ExportedOutEdge, ExportedProperty, ExportedVertex,
};
pub use router_error::RouterError;

use candid::{CandidType, Decode, Encode, Principal};
use ic_stable_lara::VertexId;
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Stable logical vertex identity (globally unique, never changes on migration).
pub type LogicalVertexId = u64;

/// Graph shard partition id.
pub type ShardId = u32;

/// Dense vertex index within a single graph shard (`VertexId` in LARA).
pub type LocalVertexId = u32;

/// Router `commit_vertex_placement` argument (graph shard → router).
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct CommitVertexPlacementArgs {
    pub logical_vertex_id: LogicalVertexId,
    pub local_vertex_id: LocalVertexId,
}

/// Source shard begins migrating a vertex to another shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct BeginVertexMigrationArgs {
    pub logical_vertex_id: LogicalVertexId,
    pub destination_shard_id: ShardId,
}

/// Destination shard completes migration after importing vertex data.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct FinishVertexMigrationArgs {
    pub logical_vertex_id: LogicalVertexId,
    pub destination_local_vertex_id: LocalVertexId,
}

/// Authoritative graph shard drops router placement after deleting the vertex locally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ReleaseLogicalVertexArgs {
    pub logical_vertex_id: LogicalVertexId,
}

/// Standalone-shard identity mapping: local dense id equals logical id on one process.
#[inline]
pub fn standalone_logical_vertex_id(local: VertexId) -> LogicalVertexId {
    u64::from(u32::from_le_bytes(local.to_le_bytes()))
}

/// Stable key for reverse placement lookup (`shard_id`, `local_vertex_id`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PhysicalPlacementKey {
    pub shard_id: ShardId,
    pub local_vertex_id: LocalVertexId,
}

impl PhysicalPlacementKey {
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
        Self::new(u32::from_le_bytes(shard), u32::from_le_bytes(local))
    }
}

impl Storable for PhysicalPlacementKey {
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
    Migrating {
        epoch: u64,
        source: PhysicalVertexLocation,
        destination_shard_id: ShardId,
    },
}

/// Shard registration record returned by the router (`resolve_shard`).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ShardRegistryEntry {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub index_canister: Principal,
    pub logical_graph_name: String,
    pub registered_at_ns: u64,
}

impl Storable for ShardRegistryEntry {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ShardRegistryEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ShardRegistryEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), ShardRegistryEntry).expect("decode ShardRegistryEntry")
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
