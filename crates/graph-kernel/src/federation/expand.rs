//! Wire types for cross-shard expand (graph canister query API).

use candid::CandidType;
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use super::{LocalVertexId, LogicalVertexId, ShardId};
use crate::entry::{EdgeValuePayload, MAX_EDGE_VALUE_BYTES};

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
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType)]
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
    pub value_bytes: [u8; MAX_EDGE_VALUE_BYTES],
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
    pub fn from_value_payload(mut self, value: EdgeValuePayload) -> Self {
        self.value_bytes = value.bytes;
        self.value_len = value.len;
        self.inline_value = value.inline_u16();
        self
    }
}

impl Serialize for FederatedExpandNeighbor {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut s = serializer.serialize_struct("FederatedExpandNeighbor", 9)?;
        s.serialize_field("shard_id", &self.shard_id)?;
        s.serialize_field(
            "neighbor_logical_vertex_id",
            &self.neighbor_logical_vertex_id,
        )?;
        s.serialize_field("neighbor_local_vertex_id", &self.neighbor_local_vertex_id)?;
        s.serialize_field("anchor_local_vertex_id", &self.anchor_local_vertex_id)?;
        s.serialize_field("label_id_raw", &self.label_id_raw)?;
        s.serialize_field("slot_index", &self.slot_index)?;
        s.serialize_field("inline_value", &self.inline_value)?;
        s.serialize_field("value_len", &self.value_len)?;
        s.serialize_field(
            "value_bytes",
            &self.value_bytes[..usize::from(self.value_len)],
        )?;
        s.end()
    }
}

impl<'de> Deserialize<'de> for FederatedExpandNeighbor {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Wire {
            shard_id: ShardId,
            neighbor_logical_vertex_id: LogicalVertexId,
            neighbor_local_vertex_id: LocalVertexId,
            anchor_local_vertex_id: LocalVertexId,
            label_id_raw: u16,
            slot_index: u32,
            inline_value: u16,
            value_len: u8,
            value_bytes: Vec<u8>,
        }
        let wire = Wire::deserialize(deserializer)?;
        if usize::from(wire.value_len) != wire.value_bytes.len() {
            return Err(serde::de::Error::custom(
                "federated expand value_len does not match value_bytes length",
            ));
        }
        if wire.value_bytes.len() > MAX_EDGE_VALUE_BYTES {
            return Err(serde::de::Error::custom(format!(
                "federated expand value_bytes length {} exceeds max {}",
                wire.value_bytes.len(),
                MAX_EDGE_VALUE_BYTES
            )));
        }
        let mut value_bytes = [0u8; MAX_EDGE_VALUE_BYTES];
        value_bytes[..wire.value_bytes.len()].copy_from_slice(&wire.value_bytes);
        Ok(Self {
            shard_id: wire.shard_id,
            neighbor_logical_vertex_id: wire.neighbor_logical_vertex_id,
            neighbor_local_vertex_id: wire.neighbor_local_vertex_id,
            anchor_local_vertex_id: wire.anchor_local_vertex_id,
            label_id_raw: wire.label_id_raw,
            slot_index: wire.slot_index,
            inline_value: wire.inline_value,
            value_len: wire.value_len,
            value_bytes,
        })
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
            value_len: 2,
            value_bytes: {
                let mut b = [0u8; MAX_EDGE_VALUE_BYTES];
                b[0] = 9;
                b[1] = 8;
                b
            },
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
            value_len: 0,
            value_bytes: [0u8; MAX_EDGE_VALUE_BYTES],
        }
        .from_value_payload(payload);
        assert_eq!(restored.value_len, 2);
        assert_eq!(restored.value_bytes[0], 9);
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
            value_len: 0,
            value_bytes: [0u8; MAX_EDGE_VALUE_BYTES],
        }
        .from_value_payload(payload);
        assert_eq!(neighbor.inline_value, 0x1234);
    }
}
