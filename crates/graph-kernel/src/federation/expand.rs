//! Wire types for cross-shard expand (graph canister query API).

use candid::CandidType;
use serde::{Deserialize, Serialize};

use super::{LocalVertexId, LogicalVertexId, ShardId};
use crate::entry::EdgeValuePayload;

/// Direction of a federated expand probe on a graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum FederatedExpandDirection {
    /// Reverse expand: predecessors visible on this shard (`in_edges` or forward-to-remote scan).
    Incoming,
    /// Forward expand: successors on the authoritative shard (`out_edges` only).
    Outgoing,
    /// Undirected expand: incident undirected edges on the authoritative shard plus cross-shard scans
    /// to a remote ref when the probe vertex is not local.
    Undirected,
}

/// Neighbor enumeration for one logical vertex on one graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct FederatedExpandArgs {
    pub logical_vertex_id: LogicalVertexId,
    pub direction: FederatedExpandDirection,
    /// When set, only edges with this LARA `Edge.label_id` are returned.
    pub label_id_raw: Option<u16>,
}

/// One half-edge visible on the responding shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct FederatedExpandNeighbor {
    pub shard_id: ShardId,
    pub neighbor_logical_vertex_id: LogicalVertexId,
    pub neighbor_local_vertex_id: LocalVertexId,
    /// Local id of the probe vertex when authoritative on this shard; else `0`.
    pub anchor_local_vertex_id: LocalVertexId,
    pub label_id_raw: u16,
    pub slot_index: u32,
    /// Little-endian u16 view when [`Self::value_len`] is `2`.
    pub inline_value: u16,
    pub value_len: u8,
    pub value_bytes: [u8; 8],
}

impl FederatedExpandNeighbor {
    #[inline]
    pub fn value_payload(self) -> EdgeValuePayload {
        EdgeValuePayload {
            bytes: self.value_bytes,
            len: self.value_len,
        }
    }

    #[inline]
    pub fn from_value_payload(
        mut self,
        value: EdgeValuePayload,
    ) -> Self {
        self.value_bytes = value.bytes;
        self.value_len = value.len;
        self.inline_value = value.inline_u16();
        self
    }
}
