//! Wire types for cross-shard expand (graph canister query API).

use candid::CandidType;
use serde::{Deserialize, Serialize};
use std::fmt;

use super::{GlobalVertexId, LocalVertexId, ShardId};
use crate::entry::EdgePayload;

/// Maximum edge-payload bytes carried by one federated expand hit.
pub const MAX_FEDERATED_EXPAND_PAYLOAD_BYTE_WIDTH: u16 = 4096;

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

/// Neighbor enumeration for one global vertex on one graph shard.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct FederatedExpandArgs {
    pub vertex_id: GlobalVertexId,
    pub direction: FederatedExpandDirection,
    /// When set, only edges with this LARA `Edge.label_id` are returned.
    pub label_id_raw: Option<u16>,
}

/// Rejects oversize federated expand value payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FederatedExpandPayloadError {
    PayloadBytesTooLong { len: usize, max: u16 },
}

impl std::fmt::Display for FederatedExpandPayloadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::PayloadBytesTooLong { len, max } => {
                write!(
                    f,
                    "federated expand payload_bytes length {len} exceeds max {max}"
                )
            }
        }
    }
}

impl std::error::Error for FederatedExpandPayloadError {}

/// One half-edge visible on the responding shard.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct FederatedExpandNeighbor {
    pub shard_id: ShardId,
    pub neighbor_vertex_id: GlobalVertexId,
    pub neighbor_local_vertex_id: LocalVertexId,
    /// Local id of the probe vertex when authoritative on this shard; else `0`.
    pub anchor_local_vertex_id: LocalVertexId,
    pub label_id_raw: u16,
    pub slot_index: u32,
    pub payload_bytes: Vec<u8>,
}

impl FederatedExpandNeighbor {
    #[inline]
    pub fn payload(&self) -> EdgePayload {
        EdgePayload::from_slice(&self.payload_bytes)
    }

    #[inline]
    pub fn from_payload(mut self, value: EdgePayload) -> Self {
        self.payload_bytes = value.as_slice().to_vec();
        self
    }

    #[inline]
    pub fn payload_len(&self) -> usize {
        self.payload_bytes.len()
    }

    /// Bounds-checks [`Self::payload_bytes`] before returning neighbors on the wire.
    pub fn validate_wire(&self) -> Result<(), FederatedExpandPayloadError> {
        let len = self.payload_bytes.len();
        if len > usize::from(MAX_FEDERATED_EXPAND_PAYLOAD_BYTE_WIDTH) {
            return Err(FederatedExpandPayloadError::PayloadBytesTooLong {
                len,
                max: MAX_FEDERATED_EXPAND_PAYLOAD_BYTE_WIDTH,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::federation::ShardId;
    use candid::{Decode, Encode};

    #[test]
    fn federated_expand_neighbor_payload_roundtrip() {
        let neighbor = FederatedExpandNeighbor {
            shard_id: ShardId::new(1),
            neighbor_vertex_id: GlobalVertexId::new(ShardId::new(1), 2),
            neighbor_local_vertex_id: 3,
            anchor_local_vertex_id: 4,
            label_id_raw: 5,
            slot_index: 6,
            payload_bytes: vec![9, 8],
        };
        let payload = neighbor.payload();
        let restored = FederatedExpandNeighbor {
            shard_id: ShardId::new(1),
            neighbor_vertex_id: GlobalVertexId::new(ShardId::new(1), 2),
            neighbor_local_vertex_id: 3,
            anchor_local_vertex_id: 4,
            label_id_raw: 5,
            slot_index: 6,
            payload_bytes: Vec::new(),
        }
        .from_payload(payload);
        assert_eq!(restored.payload_bytes, vec![9, 8]);
    }

    #[test]
    fn federated_expand_args_candid_roundtrip() {
        let args = FederatedExpandArgs {
            vertex_id: GlobalVertexId::new(ShardId::new(0), 99),
            direction: FederatedExpandDirection::Undirected,
            label_id_raw: Some(7),
        };
        let bytes = Encode!(&args).expect("encode");
        let decoded: FederatedExpandArgs = Decode!(&bytes, FederatedExpandArgs).expect("decode");
        assert_eq!(args, decoded);
    }

    #[test]
    fn payload_bytes_reject_over_max_width() {
        let oversized = vec![0u8; usize::from(MAX_FEDERATED_EXPAND_PAYLOAD_BYTE_WIDTH) + 1];
        let neighbor = FederatedExpandNeighbor {
            shard_id: ShardId::new(0),
            neighbor_vertex_id: GlobalVertexId::new(ShardId::new(0), 0),
            neighbor_local_vertex_id: 0,
            anchor_local_vertex_id: 0,
            label_id_raw: 0,
            slot_index: 0,
            payload_bytes: oversized,
        };
        assert!(matches!(
            neighbor.validate_wire(),
            Err(FederatedExpandPayloadError::PayloadBytesTooLong { .. })
        ));
    }

    #[test]
    fn federated_expand_neighbor_candid_roundtrip_validates() {
        let neighbor = FederatedExpandNeighbor {
            shard_id: ShardId::new(1),
            neighbor_vertex_id: GlobalVertexId::new(ShardId::new(1), 2),
            neighbor_local_vertex_id: 3,
            anchor_local_vertex_id: 4,
            label_id_raw: 5,
            slot_index: 6,
            payload_bytes: vec![1, 2, 3],
        };
        neighbor.validate_wire().expect("valid payload");
        let bytes = Encode!(&neighbor).expect("encode");
        let decoded: FederatedExpandNeighbor =
            Decode!(&bytes, FederatedExpandNeighbor).expect("decode");
        decoded.validate_wire().expect("decoded payload valid");
        assert_eq!(decoded.payload_bytes, neighbor.payload_bytes);
    }
}
