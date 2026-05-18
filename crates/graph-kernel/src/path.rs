//! Canonical Gleaph path element identifiers.
//!
//! `gleaph-gql` treats path element IDs as opaque bytes. This module defines
//! the graph-kernel interpretation of those bytes for Gleaph runtimes.

use crate::entry::EdgeSlotIndex;
use ic_stable_lara::VertexId;
use std::fmt;

pub const VERTEX_PATH_ID_BYTES: usize = 12;
pub const EDGE_PATH_ID_BYTES: usize = 16;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphPathVertexId {
    pub shard_id: u64,
    pub vertex_id: VertexId,
}

impl GraphPathVertexId {
    #[inline]
    pub const fn new(shard_id: u64, vertex_id: VertexId) -> Self {
        Self {
            shard_id,
            vertex_id,
        }
    }

    #[inline]
    pub fn to_bytes(self) -> [u8; VERTEX_PATH_ID_BYTES] {
        let mut out = [0; VERTEX_PATH_ID_BYTES];
        out[0..8].copy_from_slice(&self.shard_id.to_le_bytes());
        out[8..12].copy_from_slice(&self.vertex_id.to_le_bytes());
        out
    }

    #[inline]
    pub fn from_bytes(bytes: [u8; VERTEX_PATH_ID_BYTES]) -> Self {
        let mut shard_id = [0; 8];
        shard_id.copy_from_slice(&bytes[0..8]);
        let mut vertex_id = [0; 4];
        vertex_id.copy_from_slice(&bytes[8..12]);
        Self {
            shard_id: u64::from_le_bytes(shard_id),
            vertex_id: VertexId::from(u32::from_le_bytes(vertex_id)),
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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GraphPathEdgeId {
    pub shard_id: u64,
    pub owner_vertex_id: VertexId,
    /// Physical slot index wrapper for the bound edge at query time.
    ///
    /// This is an engine-local handle component, not a stable logical edge id across compaction.
    pub edge_slot_index: EdgeSlotIndex,
}

impl GraphPathEdgeId {
    #[inline]
    pub const fn new(
        shard_id: u64,
        owner_vertex_id: VertexId,
        edge_slot_index: EdgeSlotIndex,
    ) -> Self {
        Self {
            shard_id,
            owner_vertex_id,
            edge_slot_index,
        }
    }

    #[inline]
    pub fn to_bytes(self) -> [u8; EDGE_PATH_ID_BYTES] {
        let mut out = [0; EDGE_PATH_ID_BYTES];
        out[0..8].copy_from_slice(&self.shard_id.to_le_bytes());
        out[8..12].copy_from_slice(&self.owner_vertex_id.to_le_bytes());
        out[12..16].copy_from_slice(&self.edge_slot_index.to_le_bytes());
        out
    }

    #[inline]
    pub fn from_bytes(bytes: [u8; EDGE_PATH_ID_BYTES]) -> Self {
        let mut shard_id = [0; 8];
        shard_id.copy_from_slice(&bytes[0..8]);
        let mut owner_vertex_id = [0; 4];
        owner_vertex_id.copy_from_slice(&bytes[8..12]);
        let mut edge_slot_index = [0; 4];
        edge_slot_index.copy_from_slice(&bytes[12..16]);
        Self {
            shard_id: u64::from_le_bytes(shard_id),
            owner_vertex_id: VertexId::from(u32::from_le_bytes(owner_vertex_id)),
            edge_slot_index: EdgeSlotIndex::from_le_bytes(edge_slot_index),
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
        let id = GraphPathVertexId::new(42, VertexId::from(7));
        assert_eq!(GraphPathVertexId::from_bytes(id.to_bytes()), id);
        assert_eq!(GraphPathVertexId::try_from_slice(&id.to_bytes()), Ok(id));
    }

    #[test]
    fn edge_path_id_roundtrips() {
        let id = GraphPathEdgeId::new(42, VertexId::from(7), EdgeSlotIndex::from_raw(9));
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
