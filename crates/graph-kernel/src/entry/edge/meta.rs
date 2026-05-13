//! Compact metadata stored in the last two bytes of a fixed-width [`super::Edge`] record.
//!
//! [`EdgeMeta`] packs the inline edge label id and two placement flags into one `u16`.
//!
//! Layout (little-endian on wire, **host-endian `u16` semantics**):
//!
//! ```text
//! 15        14      13..0
//! +---------+-------+--------------+
//! |UNDIRECTED|REMOTE|INLINE_LABEL |
//! |  1 bit   | 1 bit |  14 bits    |
//! +---------+-------+--------------+
//! ```
//!
//! - `INLINE_LABEL` (`bits 0..=13`): [`InlineEdgeLabelId`] numeric value, or `0` when no label.
//! - `REMOTE` (`bit 14`): target lives outside the local partition.
//! - `UNDIRECTED` (`bit 15`): logical edge is undirected.

use crate::entry::label::{INLINE_EDGE_LABEL_MAX, InlineEdgeLabelId};

const INLINE_MASK: u16 = INLINE_EDGE_LABEL_MAX;
const REMOTE_BIT: u16 = 1 << 14;
const UNDIRECTED_BIT: u16 = 1 << 15;

/// Hot metadata packed into 16 bits for one edge record.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct EdgeMeta(u16);

impl EdgeMeta {
    /// Packs an optional inline label with remote/undirected flags.
    #[inline]
    pub const fn new(remote: bool, undirected: bool, label: Option<InlineEdgeLabelId>) -> Self {
        let mut v = match label {
            Some(l) => l.raw() & INLINE_MASK,
            None => 0,
        };
        if remote {
            v |= REMOTE_BIT;
        }
        if undirected {
            v |= UNDIRECTED_BIT;
        }
        Self(v)
    }

    /// Raw label bits in `0..=0x3FFF` (zero means no inline label).
    #[inline]
    pub const fn inline_label_bits(self) -> u16 {
        self.0 & INLINE_MASK
    }

    /// Typed inline label when the low 14 bits are non-zero.
    #[inline]
    pub fn inline_edge_label_id(self) -> Option<InlineEdgeLabelId> {
        InlineEdgeLabelId::try_from_raw(self.inline_label_bits())
    }

    #[inline]
    pub const fn with_inline_edge_label_id(self, label: Option<InlineEdgeLabelId>) -> Self {
        let flags = self.0 & !INLINE_MASK;
        let id_bits = match label {
            Some(l) => l.raw() & INLINE_MASK,
            None => 0,
        };
        Self(flags | id_bits)
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
        self.0 & UNDIRECTED_BIT != 0
    }

    #[inline]
    pub const fn with_undirected(self, undirected: bool) -> Self {
        if undirected {
            Self(self.0 | UNDIRECTED_BIT)
        } else {
            Self(self.0 & !UNDIRECTED_BIT)
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
    use crate::entry::label::InlineEdgeLabelId;

    #[test]
    fn new_packs_label_and_flags() {
        let label = InlineEdgeLabelId::try_from_raw(0x2A).unwrap();
        let meta = EdgeMeta::new(true, true, Some(label));
        assert_eq!(meta.inline_label_bits(), 0x2A);
        assert!(meta.is_remote());
        assert!(meta.is_undirected());
        assert_eq!(meta.inline_edge_label_id(), Some(label));
    }

    #[test]
    fn from_le_bytes_round_trips_two_byte_form() {
        // `0x8034`: label 0x34, REMOTE clear, UNDIRECTED set (bit 15 only; `0xC0` would also set REMOTE).
        let meta = EdgeMeta::from_le_bytes([0x34, 0x80]);
        assert_eq!(meta.inline_label_bits(), 0x34);
        assert!(!meta.is_remote());
        assert!(meta.is_undirected());
        assert_eq!(meta.to_le_bytes(), [0x34, 0x80]);
    }

    #[test]
    fn toggles_only_target_flag() {
        let label = InlineEdgeLabelId::try_from_raw(5).unwrap();
        let base = EdgeMeta::new(false, false, Some(label));
        let u = base.with_undirected(true);
        assert!(u.is_undirected());
        assert!(!u.is_remote());
        assert_eq!(u.inline_label_bits(), 5);

        let r = u.with_remote(true);
        assert!(r.is_remote());
        assert!(r.is_undirected());

        let cleared = r.with_undirected(false).with_remote(false);
        assert_eq!(cleared.raw(), base.raw());
    }

    #[test]
    fn default_is_zeroed() {
        let m = EdgeMeta::default();
        assert_eq!(m.raw(), 0);
        assert!(!m.is_remote());
        assert!(!m.is_undirected());
        assert_eq!(m.inline_edge_label_id(), None);
    }
}
