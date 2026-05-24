//! Segment overflow-log head index (`0..170` valid, [`LogHead::NONE`] = no log).

use crate::lara::edge::DEFAULT_MAX_LOG_ENTRIES;

/// Head index into a per-segment overflow log, or [`Self::NONE`] when absent.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct LogHead(u8);

impl LogHead {
    /// Sentinel: no overflow log chain (`u8::MAX` on wire).
    pub const NONE: Self = Self(u8::MAX);

    /// Maximum valid entry index (`0..`[`DEFAULT_MAX_LOG_ENTRIES`]).
    pub const MAX_VALID_INDEX: u8 = (DEFAULT_MAX_LOG_ENTRIES - 1) as u8;

    /// Creates a log head from a valid entry index.
    #[inline]
    pub const fn from_index(index: u8) -> Option<Self> {
        if index < DEFAULT_MAX_LOG_ENTRIES as u8 {
            Some(Self(index))
        } else {
            None
        }
    }

    /// Converts legacy `i32` API (`-1` → [`Self::NONE`], `0..170` → valid).
    #[inline]
    pub const fn from_i32(head: i32) -> Option<Self> {
        if head < 0 {
            Some(Self::NONE)
        } else if head < DEFAULT_MAX_LOG_ENTRIES as i32 {
            Some(Self(head as u8))
        } else {
            None
        }
    }

    /// Legacy `i32` view (`-1` when [`Self::is_none`]).
    #[inline]
    pub fn to_i32(self) -> i32 {
        if self.is_none() { -1 } else { self.0 as i32 }
    }

    /// Raw wire byte.
    #[inline]
    pub const fn as_byte(self) -> u8 {
        self.0
    }

    /// `true` when this is [`Self::NONE`].
    #[inline]
    pub const fn is_none(self) -> bool {
        self.0 == u8::MAX
    }

    /// Entry index when not [`Self::NONE`].
    #[inline]
    pub const fn index(self) -> Option<u8> {
        if self.is_none() { None } else { Some(self.0) }
    }

    /// Decodes a wire byte (same encoding as [`crate::slab_index::decode_overflow_log_byte`]).
    #[inline]
    pub const fn from_byte(byte: u8) -> Self {
        if byte == u8::MAX {
            Self::NONE
        } else {
            Self(byte)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn none_is_max_u8() {
        assert_eq!(LogHead::NONE.as_byte(), u8::MAX);
        assert_eq!(LogHead::NONE.to_i32(), -1);
    }

    #[test]
    fn round_trip_i32() {
        assert_eq!(LogHead::from_i32(-1), Some(LogHead::NONE));
        assert_eq!(LogHead::from_i32(0), LogHead::from_index(0));
        assert_eq!(LogHead::from_i32(169), LogHead::from_index(169));
        assert!(LogHead::from_i32(170).is_none());
    }
}
