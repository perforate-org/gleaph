//! The shard-local vertex id.
//!
//! Gleaph's own type (ADR 0136, Decision 11): the Graph shard's store maps it one to one onto its
//! node id inside `facade::topology`, so nothing else names the store to speak of a vertex.

use std::fmt;

/// A shard-local vertex: a dense index in creation order, never reused (ADR 0136, Decision 2).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct VertexId(u32);

impl VertexId {
    /// The id from its little-endian bytes.
    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }

    /// The id's little-endian bytes.
    #[inline]
    pub const fn to_le_bytes(&self) -> [u8; 4] {
        self.0.to_le_bytes()
    }
}

impl From<u32> for VertexId {
    #[inline]
    fn from(raw: u32) -> Self {
        Self(raw)
    }
}

impl From<VertexId> for u32 {
    #[inline]
    fn from(id: VertexId) -> Self {
        id.0
    }
}

impl From<VertexId> for u64 {
    #[inline]
    fn from(id: VertexId) -> Self {
        u64::from(id.0)
    }
}

impl fmt::Display for VertexId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}
