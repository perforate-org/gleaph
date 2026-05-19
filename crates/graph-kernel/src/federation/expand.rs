//! Wire types for cross-shard expand (graph canister query API).

use candid::CandidType;
use serde::{Deserialize, Serialize};

use super::{LocalVertexId, LogicalVertexId, ShardId};

/// Incoming-neighbor probe for a logical vertex on one graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct FederatedIncomingExpandArgs {
    pub target_logical_vertex_id: LogicalVertexId,
    /// When set, only edges with this LARA `Edge.label_id` are returned.
    pub label_id_raw: Option<u16>,
}

/// One incoming half-edge visible on the responding shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct FederatedExpandNeighbor {
    pub shard_id: ShardId,
    pub neighbor_logical_vertex_id: LogicalVertexId,
    pub neighbor_local_vertex_id: LocalVertexId,
    /// Local id of `target_logical_vertex_id` when authoritative on this shard; else `0`.
    pub target_local_vertex_id: LocalVertexId,
    pub label_id_raw: u16,
    pub slot_index: u32,
    pub inline_value: u16,
}
