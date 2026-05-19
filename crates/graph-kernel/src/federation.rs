//! Distributed graph federation identifiers and placement types.

mod router_error;

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

/// Standalone-shard identity mapping: local dense id equals logical id on one process.
#[inline]
pub fn standalone_logical_vertex_id(local: VertexId) -> LogicalVertexId {
    u64::from(u32::from_le_bytes(local.to_le_bytes()))
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
    Migrating { epoch: u64 },
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
