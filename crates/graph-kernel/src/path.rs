//! Canonical Gleaph path element identifiers.
//!
//! `gleaph-gql` treats path element IDs as opaque bytes. This module defines
//! the graph-kernel interpretation of those bytes for Gleaph runtimes.

use crate::entry::EdgeSlotIndex;
use crate::federation::{
    ENCODED_EDGE_ID_BYTES, ENCODED_VERTEX_ID_BYTES, ElementIdEncodingKey, EncodedEdgeId,
    EncodedVertexId, GlobalEdgeId, GlobalVertexId, ShardId, decode_global_edge_id,
    decode_global_vertex_id, encode_global_edge_id, encode_global_vertex_id,
};
use ic_stable_lara::VertexId;
use std::fmt;

pub const VERTEX_PATH_ID_BYTES: usize = ENCODED_VERTEX_ID_BYTES;
pub const EDGE_PATH_ID_BYTES: usize = ENCODED_EDGE_ID_BYTES;

/// Encoded global vertex identity exposed in paths and `ELEMENT_ID` for vertices.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphPathVertexId {
    pub encoded: EncodedVertexId,
}

impl GraphPathVertexId {
    #[inline]
    pub const fn from_encoded(encoded: EncodedVertexId) -> Self {
        Self { encoded }
    }

    #[inline]
    pub fn from_global(key: &ElementIdEncodingKey, id: GlobalVertexId) -> Self {
        Self {
            encoded: encode_global_vertex_id(key, id),
        }
    }

    #[inline]
    pub fn to_bytes(self) -> [u8; VERTEX_PATH_ID_BYTES] {
        self.encoded.0
    }

    #[inline]
    pub fn from_bytes(bytes: [u8; VERTEX_PATH_ID_BYTES]) -> Self {
        Self {
            encoded: EncodedVertexId(bytes),
        }
    }

    #[inline]
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, GraphPathIdError> {
        let bytes: [u8; VERTEX_PATH_ID_BYTES] =
            bytes
                .try_into()
                .map_err(|_| GraphPathIdError::InvalidVertexLength {
                    actual: bytes.len(),
                })?;
        Ok(Self::from_bytes(bytes))
    }

    #[inline]
    pub fn decode_global(self, key: &ElementIdEncodingKey) -> GlobalVertexId {
        decode_global_vertex_id(key, self.encoded)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphPathEdgeId {
    pub encoded: EncodedEdgeId,
}

impl GraphPathEdgeId {
    #[inline]
    pub const fn from_encoded(encoded: EncodedEdgeId) -> Self {
        Self { encoded }
    }

    #[inline]
    pub fn from_global(key: &ElementIdEncodingKey, id: GlobalEdgeId) -> Self {
        Self {
            encoded: encode_global_edge_id(key, id),
        }
    }

    #[inline]
    pub fn new(
        key: &ElementIdEncodingKey,
        shard_id: ShardId,
        owner_vertex_id: VertexId,
        edge_slot_index: EdgeSlotIndex,
    ) -> Self {
        Self::from_global(
            key,
            GlobalEdgeId::new(
                shard_id,
                u32::from_le_bytes(owner_vertex_id.to_le_bytes()),
                edge_slot_index,
            ),
        )
    }

    #[inline]
    pub fn to_bytes(self) -> [u8; EDGE_PATH_ID_BYTES] {
        self.encoded.0
    }

    #[inline]
    pub fn from_bytes(bytes: [u8; EDGE_PATH_ID_BYTES]) -> Self {
        Self {
            encoded: EncodedEdgeId(bytes),
        }
    }

    #[inline]
    pub fn try_from_slice(bytes: &[u8]) -> Result<Self, GraphPathIdError> {
        let bytes: [u8; EDGE_PATH_ID_BYTES] =
            bytes
                .try_into()
                .map_err(|_| GraphPathIdError::InvalidEdgeLength {
                    actual: bytes.len(),
                })?;
        Ok(Self::from_bytes(bytes))
    }

    #[inline]
    pub fn decode_global(self, key: &ElementIdEncodingKey) -> GlobalEdgeId {
        decode_global_edge_id(key, self.encoded)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GraphPathIdError {
    InvalidVertexLength { actual: usize },
    InvalidEdgeLength { actual: usize },
}

impl fmt::Display for GraphPathIdError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidVertexLength { actual } => write!(
                f,
                "invalid graph path vertex id length: expected {VERTEX_PATH_ID_BYTES}, got {actual}"
            ),
            Self::InvalidEdgeLength { actual } => write!(
                f,
                "invalid graph path edge id length: expected {EDGE_PATH_ID_BYTES}, got {actual}"
            ),
        }
    }
}

impl std::error::Error for GraphPathIdError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vertex_path_id_roundtrips() {
        let key = ElementIdEncodingKey::standalone();
        let id = GraphPathVertexId::from_global(&key, GlobalVertexId::new(ShardId::new(0), 42));
        assert_eq!(GraphPathVertexId::from_bytes(id.to_bytes()), id);
        assert_eq!(GraphPathVertexId::try_from_slice(&id.to_bytes()), Ok(id));
    }

    #[test]
    fn edge_path_id_roundtrips() {
        let key = ElementIdEncodingKey::standalone();
        let id = GraphPathEdgeId::new(
            &key,
            ShardId::new(0),
            VertexId::from(7),
            EdgeSlotIndex::from_raw(9),
        );
        assert_eq!(GraphPathEdgeId::from_bytes(id.to_bytes()), id);
        assert_eq!(GraphPathEdgeId::try_from_slice(&id.to_bytes()), Ok(id));
    }

    #[test]
    fn path_id_length_errors_are_specific() {
        assert_eq!(
            GraphPathVertexId::try_from_slice(&[1, 2, 3]),
            Err(GraphPathIdError::InvalidVertexLength { actual: 3 })
        );
        assert_eq!(
            GraphPathEdgeId::try_from_slice(&[1, 2, 3]),
            Err(GraphPathIdError::InvalidEdgeLength { actual: 3 })
        );
    }
}
