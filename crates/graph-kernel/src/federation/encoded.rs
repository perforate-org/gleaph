//! Bijective encoded element ids for client wire (`ELEMENT_ID`, path elements).

use super::{GlobalEdgeId, GlobalVertexId};

pub const ENCODED_VERTEX_ID_BYTES: usize = 8;
pub const ENCODED_EDGE_ID_BYTES: usize = 12;

/// Per-graph encoding key (router stable config at graph registration).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElementIdEncodingKey(pub [u8; 16]);

impl ElementIdEncodingKey {
    /// Fixed key for **host-only** unit tests that do not run through router graph registration.
    ///
    /// Production graphs use per-graph keys from `ROUTER_GRAPH_RUNTIME_CONFIG` (ADR 0019).
    #[inline]
    pub const fn host_test_fixture() -> Self {
        Self(*b"gleaph-std-key!!")
    }

    /// Alias for [`Self::host_test_fixture`]. Prefer `host_test_fixture` in new tests.
    #[inline]
    pub const fn standalone() -> Self {
        Self::host_test_fixture()
    }
}

/// Opaque 8-byte vertex id on the client wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EncodedVertexId(pub [u8; ENCODED_VERTEX_ID_BYTES]);

/// Opaque 12-byte edge id on the client wire.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EncodedEdgeId(pub [u8; ENCODED_EDGE_ID_BYTES]);

#[inline]
fn feistel_round(right: u32, key: u64, round: u32) -> u32 {
    right
        .wrapping_mul(key as u32)
        .wrapping_add(round)
        .wrapping_add((key >> 32) as u32)
}

#[inline]
fn feistel64_encrypt(block: u64, key: u64, rounds: u32) -> u64 {
    let mut left = (block >> 32) as u32;
    let mut right = (block & 0xFFFF_FFFF) as u32;
    for round in 0..rounds {
        let new_right = left ^ feistel_round(right, key, round);
        left = right;
        right = new_right;
    }
    u64::from(left) << 32 | u64::from(right)
}

#[inline]
fn feistel64_decrypt(block: u64, key: u64, rounds: u32) -> u64 {
    let mut left = (block >> 32) as u32;
    let mut right = (block & 0xFFFF_FFFF) as u32;
    for round in (0..rounds).rev() {
        let prev_right = left;
        let prev_left = right ^ feistel_round(left, key, round);
        left = prev_left;
        right = prev_right;
    }
    u64::from(left) << 32 | u64::from(right)
}

#[inline]
fn key_u64(key: &ElementIdEncodingKey) -> u64 {
    u64::from_le_bytes(key.0[0..8].try_into().expect("8 bytes"))
}

#[inline]
fn key_u32_tail(key: &ElementIdEncodingKey) -> u32 {
    u32::from_le_bytes(key.0[8..12].try_into().expect("4 bytes"))
}

pub fn encode_global_vertex_id(key: &ElementIdEncodingKey, id: GlobalVertexId) -> EncodedVertexId {
    let canonical = u64::from_le_bytes(id.to_le_bytes());
    let encoded = feistel64_encrypt(canonical, key_u64(key), 4);
    EncodedVertexId(encoded.to_le_bytes())
}

pub fn decode_global_vertex_id(
    key: &ElementIdEncodingKey,
    encoded: EncodedVertexId,
) -> GlobalVertexId {
    let block = u64::from_le_bytes(encoded.0);
    let canonical = feistel64_decrypt(block, key_u64(key), 4);
    GlobalVertexId::from_le_bytes(canonical.to_le_bytes())
}

pub fn encode_global_edge_id(key: &ElementIdEncodingKey, id: GlobalEdgeId) -> EncodedEdgeId {
    let mut bytes = id.to_le_bytes();
    let head = u64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes"));
    let tail = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
    let encoded_head = feistel64_encrypt(head, key_u64(key), 4);
    let encoded_tail = tail ^ key_u32_tail(key) ^ (encoded_head as u32);
    bytes[0..8].copy_from_slice(&encoded_head.to_le_bytes());
    bytes[8..12].copy_from_slice(&encoded_tail.to_le_bytes());
    EncodedEdgeId(bytes)
}

pub fn decode_global_edge_id(key: &ElementIdEncodingKey, encoded: EncodedEdgeId) -> GlobalEdgeId {
    let mut bytes = encoded.0;
    let head = u64::from_le_bytes(bytes[0..8].try_into().expect("8 bytes"));
    let tail = u32::from_le_bytes(bytes[8..12].try_into().expect("4 bytes"));
    let canonical_head = feistel64_decrypt(head, key_u64(key), 4);
    let canonical_tail = tail ^ key_u32_tail(key) ^ (head as u32);
    bytes[0..8].copy_from_slice(&canonical_head.to_le_bytes());
    bytes[8..12].copy_from_slice(&canonical_tail.to_le_bytes());
    GlobalEdgeId::from_le_bytes(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::entry::EdgeSlotIndex;
    use crate::federation::ShardId;

    #[test]
    fn vertex_encode_decode_roundtrip() {
        let key = ElementIdEncodingKey::host_test_fixture();
        let id = GlobalVertexId::new(ShardId::new(0), 42);
        let enc = encode_global_vertex_id(&key, id);
        assert_eq!(decode_global_vertex_id(&key, enc), id);
    }

    #[test]
    fn edge_encode_decode_roundtrip() {
        let key = ElementIdEncodingKey::host_test_fixture();
        let id = GlobalEdgeId::new(ShardId::new(1), 9, EdgeSlotIndex::from_raw(3));
        let enc = encode_global_edge_id(&key, id);
        assert_eq!(decode_global_edge_id(&key, enc), id);
    }

    #[test]
    fn encoded_vertex_differs_from_canonical() {
        let key = ElementIdEncodingKey::host_test_fixture();
        let id = GlobalVertexId::new(ShardId::new(0), 1);
        let enc = encode_global_vertex_id(&key, id);
        assert_ne!(enc.0, id.to_le_bytes());
    }
}
