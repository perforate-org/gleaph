//! Segment overflow-log head index (`0..=169` valid, [`LogHead::NONE`] = no log).

/// Head index into a per-segment overflow log, or [`Self::NONE`] when absent.
#[repr(transparent)]
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct LogHead(u8);

impl LogHead {
    /// Sentinel: no overflow log chain (`u8::MAX` on wire).
    pub const NONE: Self = Self(u8::MAX);

    /// Maximum valid entry index. The retired per-leaf log's 170-entry bound is the widest a
    /// single byte could address under the old layout; the byte survives only until the
    /// descriptor's final 34-byte row removes it.
    pub const MAX_VALID_INDEX: u8 = 169;

    /// Creates a log head from a valid entry index (`0..=MAX_VALID_INDEX`).
    #[inline]
    pub const fn from_index(index: u8) -> Option<Self> {
        if index <= Self::MAX_VALID_INDEX {
            Some(Self(index))
        } else {
            None
        }
    }

    /// Converts legacy `i32` API (any negative → [`Self::NONE`], `0..=169` → valid).
    #[inline]
    pub const fn from_i32(head: i32) -> Option<Self> {
        if head < 0 {
            Some(Self::NONE)
        } else if head <= Self::MAX_VALID_INDEX as i32 {
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
    ///
    /// This deliberately does not validate the entry-index domain: any byte other than the sentinel
    /// becomes a head, and record validation owns the `0..=169` check at rehydrate time.
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
        for index in 0..=LogHead::MAX_VALID_INDEX {
            assert_eq!(
                LogHead::from_i32(index as i32),
                LogHead::from_index(index),
                "legacy and u8 constructors must agree at {index}"
            );
            assert!(LogHead::from_i32(index as i32).is_some());
        }
        assert!(LogHead::from_i32(170).is_none());
        assert!(LogHead::from_i32(-2).is_some_and(|head| head.is_none()));
    }

    #[test]
    fn from_index_accepts_max_valid_index() {
        assert_eq!(
            LogHead::from_index(LogHead::MAX_VALID_INDEX).map(LogHead::as_byte),
            Some(LogHead::MAX_VALID_INDEX)
        );
    }

    #[test]
    fn from_index_rejects_above_max() {
        assert!(LogHead::from_index(LogHead::MAX_VALID_INDEX + 1).is_none());
        assert!(LogHead::from_index(u8::MAX).is_none());
    }

    #[test]
    fn from_i32_accepts_max_valid_index() {
        assert_eq!(
            LogHead::from_i32(LogHead::MAX_VALID_INDEX as i32).map(LogHead::as_byte),
            Some(LogHead::MAX_VALID_INDEX)
        );
    }

    #[test]
    fn from_i32_rejects_above_max() {
        assert!(LogHead::from_i32(LogHead::MAX_VALID_INDEX as i32 + 1).is_none());
        assert!(LogHead::from_i32(i32::MAX).is_none());
    }

    #[test]
    fn negative_i32_maps_to_none() {
        for head in [-1, -2, -100, i32::MIN] {
            assert_eq!(LogHead::from_i32(head), Some(LogHead::NONE), "head {head}");
        }
    }
}
