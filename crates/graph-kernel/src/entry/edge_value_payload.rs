//! In-memory edge value bytes shared across storage, federation wire, and query bindings.

use super::edge::MAX_EDGE_VALUE_BYTES;
use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Stored edge-value bytes (not part of the 4-byte labeled CSR row).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, CandidType, Serialize, Deserialize)]
pub struct EdgeValuePayload {
    pub bytes: [u8; MAX_EDGE_VALUE_BYTES],
    pub len: u8,
}

impl Default for EdgeValuePayload {
    fn default() -> Self {
        Self::EMPTY
    }
}

impl EdgeValuePayload {
    pub const EMPTY: Self = Self {
        bytes: [0u8; MAX_EDGE_VALUE_BYTES],
        len: 0,
    };

    #[inline]
    pub fn from_slice(bytes: &[u8]) -> Self {
        let len = bytes.len().min(MAX_EDGE_VALUE_BYTES) as u8;
        let mut out = Self::EMPTY;
        out.bytes[..usize::from(len)].copy_from_slice(&bytes[..usize::from(len)]);
        out.len = len;
        out
    }

    #[inline]
    pub fn from_inline_u16(inline: u16) -> Self {
        Self::from_slice(&inline.to_le_bytes())
    }

    #[inline]
    pub fn is_empty(self) -> bool {
        self.len == 0
    }

    /// Active value bytes as a slice.
    #[inline]
    pub fn as_slice(&self) -> &[u8] {
        &self.bytes[..usize::from(self.len.min(MAX_EDGE_VALUE_BYTES as u8))]
    }

    #[inline]
    pub fn bytes_slice(&self) -> &[u8] {
        self.as_slice()
    }

    /// Little-endian `u16` view when [`Self::len`] is exactly `2`.
    #[inline]
    pub fn inline_u16(self) -> u16 {
        if self.len == 2 {
            u16::from_le_bytes(self.bytes[0..2].try_into().unwrap())
        } else {
            0
        }
    }
}
