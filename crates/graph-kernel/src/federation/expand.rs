//! Wire types for cross-shard expand (graph canister query API).

use candid::CandidType;
use serde::{Deserialize, Serialize};

use super::{LocalVertexId, LogicalVertexId, ShardId};
use crate::entry::EdgeValuePayload;

/// Maximum edge-value bytes carried by one federated expand hit.
pub const MAX_FEDERATED_EXPAND_VALUE_BYTE_WIDTH: u16 = 4096;

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

/// Rejects oversize federated expand value payloads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FederatedExpandValueError {
    ValueBytesTooLong { len: usize, max: u16 },
}

impl std::fmt::Display for FederatedExpandValueError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ValueBytesTooLong { len, max } => {
                write!(
                    f,
                    "federated expand value_bytes length {len} exceeds max {max}"
                )
            }
        }
    }
}

impl std::error::Error for FederatedExpandValueError {}

/// One half-edge visible on the responding shard.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct FederatedExpandNeighbor {
    pub shard_id: ShardId,
    pub neighbor_logical_vertex_id: LogicalVertexId,
    pub neighbor_local_vertex_id: LocalVertexId,
    /// Local id of the probe vertex when authoritative on this shard; else `0`.
    pub anchor_local_vertex_id: LocalVertexId,
    pub label_id_raw: u16,
    pub slot_index: u32,
    /// Little-endian u16 view when value length is `2`.
    pub inline_value: u16,
    pub value_bytes: Vec<u8>,
}

impl FederatedExpandNeighbor {
    #[inline]
    pub fn value_payload(&self) -> EdgeValuePayload {
        EdgeValuePayload::from_slice(&self.value_bytes)
    }

    #[inline]
    pub fn from_value_payload(mut self, value: EdgeValuePayload) -> Self {
        self.inline_value = value.inline_u16();
        self.value_bytes = value.as_slice().to_vec();
        self
    }

    #[inline]
    pub fn value_len(&self) -> usize {
        self.value_bytes.len()
    }

    /// Bounds-checks [`Self::value_bytes`] before returning neighbors on the wire.
    pub fn validate_wire(&self) -> Result<(), FederatedExpandValueError> {
        let len = self.value_bytes.len();
        if len > usize::from(MAX_FEDERATED_EXPAND_VALUE_BYTE_WIDTH) {
            return Err(FederatedExpandValueError::ValueBytesTooLong {
                len,
                max: MAX_FEDERATED_EXPAND_VALUE_BYTE_WIDTH,
            });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[test]
    fn federated_expand_neighbor_value_payload_roundtrip() {
        let neighbor = FederatedExpandNeighbor {
            shard_id: 1,
            neighbor_logical_vertex_id: 2,
            neighbor_local_vertex_id: 3,
            anchor_local_vertex_id: 4,
            label_id_raw: 5,
            slot_index: 6,
            inline_value: 0,
            value_bytes: vec![9, 8],
        };
        let payload = neighbor.value_payload();
        let restored = FederatedExpandNeighbor {
            shard_id: 1,
            neighbor_logical_vertex_id: 2,
            neighbor_local_vertex_id: 3,
            anchor_local_vertex_id: 4,
            label_id_raw: 5,
            slot_index: 6,
            inline_value: 0,
            value_bytes: Vec::new(),
        }
        .from_value_payload(payload);
        assert_eq!(restored.value_bytes, vec![9, 8]);
    }

    #[test]
    fn federated_expand_args_candid_roundtrip() {
        let args = FederatedExpandArgs {
            logical_vertex_id: 99,
            direction: FederatedExpandDirection::Undirected,
            label_id_raw: Some(7),
        };
        let bytes = Encode!(&args).expect("encode");
        let decoded: FederatedExpandArgs = Decode!(&bytes, FederatedExpandArgs).expect("decode");
        assert_eq!(args, decoded);
    }

    #[test]
    fn edge_value_payload_inline_u16_maps_neighbor_fields() {
        let payload = EdgeValuePayload::from_slice(&0x1234u16.to_le_bytes());
        let neighbor = FederatedExpandNeighbor {
            shard_id: 0,
            neighbor_logical_vertex_id: 0,
            neighbor_local_vertex_id: 0,
            anchor_local_vertex_id: 0,
            label_id_raw: 0,
            slot_index: 0,
            inline_value: 0,
            value_bytes: Vec::new(),
        }
        .from_value_payload(payload);
        assert_eq!(neighbor.inline_value, 0x1234);
    }

    #[test]
    fn value_bytes_reject_over_max_width() {
        let oversized = vec![0u8; usize::from(MAX_FEDERATED_EXPAND_VALUE_BYTE_WIDTH) + 1];
        let neighbor = FederatedExpandNeighbor {
            shard_id: 0,
            neighbor_logical_vertex_id: 0,
            neighbor_local_vertex_id: 0,
            anchor_local_vertex_id: 0,
            label_id_raw: 0,
            slot_index: 0,
            inline_value: 0,
            value_bytes: oversized,
        };
        assert!(matches!(
            neighbor.validate_wire(),
            Err(FederatedExpandValueError::ValueBytesTooLong { .. })
        ));
    }

    #[test]
    fn federated_expand_neighbor_candid_roundtrip_validates() {
        let neighbor = FederatedExpandNeighbor {
            shard_id: 1,
            neighbor_logical_vertex_id: 2,
            neighbor_local_vertex_id: 3,
            anchor_local_vertex_id: 4,
            label_id_raw: 5,
            slot_index: 6,
            inline_value: 7,
            value_bytes: vec![1, 2, 3],
        };
        neighbor.validate_wire().expect("valid payload");
        let bytes = Encode!(&neighbor).expect("encode");
        let decoded: FederatedExpandNeighbor =
            Decode!(&bytes, FederatedExpandNeighbor).expect("decode");
        decoded.validate_wire().expect("decoded payload valid");
        assert_eq!(decoded.value_bytes, neighbor.value_bytes);
    }
}
