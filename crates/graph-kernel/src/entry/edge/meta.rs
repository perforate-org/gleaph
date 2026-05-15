//! Compact metadata for edge placement flags (optional; labels live in Lara buckets).
//!
//! Layout:
//!
//! ```text
//! 15        14      13..0
//! +---------+-------+--------------+
//! |UNDIRECTED|REMOTE|  reserved    |
//! +---------+-------+--------------+
//! ```

use crate::entry::label::EDGE_LABEL_UNDIRECTED_BIT;

const REMOTE_BIT: u16 = 1 << 14;

/// Hot metadata packed into 16 bits (used at API boundaries when needed).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct EdgeMeta(u16);

impl EdgeMeta {
    #[inline]
    pub const fn new(remote: bool, undirected: bool) -> Self {
        let mut v = 0u16;
        if remote {
            v |= REMOTE_BIT;
        }
        if undirected {
            v |= EDGE_LABEL_UNDIRECTED_BIT;
        }
        Self(v)
    }

    #[inline]
    pub const fn raw(self) -> u16 {
        self.0
    }

    #[inline]
    pub const fn from_raw(word: u16) -> Self {
        Self(word)
    }

    #[inline]
    pub const fn to_le_bytes(self) -> [u8; 2] {
        self.0.to_le_bytes()
    }

    #[inline]
    pub const fn from_le_bytes(bytes: [u8; 2]) -> Self {
        Self(u16::from_le_bytes(bytes))
    }

    #[inline]
    pub const fn is_undirected(self) -> bool {
        self.0 & EDGE_LABEL_UNDIRECTED_BIT != 0
    }

    #[inline]
    pub const fn with_undirected(self, undirected: bool) -> Self {
        if undirected {
            Self(self.0 | EDGE_LABEL_UNDIRECTED_BIT)
        } else {
            Self(self.0 & !EDGE_LABEL_UNDIRECTED_BIT)
        }
    }

    #[inline]
    pub const fn is_remote(self) -> bool {
        self.0 & REMOTE_BIT != 0
    }

    #[inline]
    pub const fn with_remote(self, remote: bool) -> Self {
        if remote {
            Self(self.0 | REMOTE_BIT)
        } else {
            Self(self.0 & !REMOTE_BIT)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn packs_remote_and_undirected() {
        let meta = EdgeMeta::new(true, true);
        assert!(meta.is_remote());
        assert!(meta.is_undirected());
    }
}
