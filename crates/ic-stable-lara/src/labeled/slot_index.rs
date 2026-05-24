//! LabelBucket packed-word helpers.
//!
//! Re-exports shared [`crate::slab_index`] bounds/encode routines and adds
//! [`LabelBucket`]-specific field packing in the high bits of the wire word.

pub use crate::slab_index::*;

use crate::labeled::bucket_label_key::BucketLabelKey;

const BUCKET_LABEL_SHIFT: u32 = 36;
const BUCKET_LOG_SHIFT: u32 = 52;
const BUCKET_VALUE_WIDTH_SHIFT: u32 = 60;
const BUCKET_TOP_BIT_SHIFT: u32 = 63;
const BUCKET_LABEL_MASK: u64 = 0xFFFF << BUCKET_LABEL_SHIFT;
const BUCKET_LOG_MASK: u64 = 0xFF << BUCKET_LOG_SHIFT;
const BUCKET_VALUE_WIDTH_MASK: u64 = 0x7 << BUCKET_VALUE_WIDTH_SHIFT;
const BUCKET_TOP_BIT_MASK: u64 = 1 << BUCKET_TOP_BIT_SHIFT;

/// Physical edge-value width codes stored in a [`super::record::LabelBucket`] word.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum ValueWidthCode {
    /// No edge values for this label bucket.
    Zero = 0,
    /// 1 byte per edge slot.
    W1 = 1,
    /// 2 bytes per edge slot.
    W2 = 2,
    /// 4 bytes per edge slot.
    W4 = 3,
    /// 8 bytes per edge slot.
    W8 = 4,
    /// 16 bytes per edge slot.
    W16 = 5,
    /// 32 bytes per edge slot.
    W32 = 6,
    /// 64 bytes per edge slot.
    W64 = 7,
}

impl ValueWidthCode {
    /// All valid codes.
    pub const VALID: [Self; 8] = [
        Self::Zero,
        Self::W1,
        Self::W2,
        Self::W4,
        Self::W8,
        Self::W16,
        Self::W32,
        Self::W64,
    ];

    /// Byte width for a valid code (`0` for [`Self::Zero`]).
    #[inline]
    pub const fn byte_width(self) -> u8 {
        match self {
            Self::Zero => 0,
            Self::W1 => 1,
            Self::W2 => 2,
            Self::W4 => 4,
            Self::W8 => 8,
            Self::W16 => 16,
            Self::W32 => 32,
            Self::W64 => 64,
        }
    }

    /// Encodes a physical byte width; returns `None` for unsupported sizes.
    #[inline]
    pub const fn from_byte_width(width: u8) -> Option<Self> {
        match width {
            0 => Some(Self::Zero),
            1 => Some(Self::W1),
            2 => Some(Self::W2),
            4 => Some(Self::W4),
            8 => Some(Self::W8),
            16 => Some(Self::W16),
            32 => Some(Self::W32),
            64 => Some(Self::W64),
            _ => None,
        }
    }
}

/// Packs a [`super::record::LabelBucket`] wire word.
#[inline]
pub fn encode_bucket_word(
    edge_start: u64,
    bucket_label_key: BucketLabelKey,
    overflow_log_head: i32,
    value_width_code: ValueWidthCode,
) -> u64 {
    try_encode_bucket_word(
        edge_start,
        bucket_label_key,
        overflow_log_head,
        value_width_code,
    )
    .expect("label bucket packed word out of range")
}

/// Fallible [`encode_bucket_word`].
#[inline]
pub fn try_encode_bucket_word(
    edge_start: u64,
    bucket_label_key: BucketLabelKey,
    overflow_log_head: i32,
    value_width_code: ValueWidthCode,
) -> Option<u64> {
    if !slot_index_fits(edge_start) {
        return None;
    }
    let head = try_encode_overflow_log_byte(overflow_log_head)?;
    let code = value_width_code as u64;
    if code > 7 {
        return None;
    }
    Some(
        edge_start
            | (u64::from(bucket_label_key.raw()) << BUCKET_LABEL_SHIFT)
            | (u64::from(head) << BUCKET_LOG_SHIFT)
            | (code << BUCKET_VALUE_WIDTH_SHIFT),
    )
}

/// Replaces only the label-key field in a packed bucket word.
#[inline]
pub fn replace_bucket_label_key(word: u64, bucket_label_key: BucketLabelKey) -> u64 {
    (word & !BUCKET_LABEL_MASK) | (u64::from(bucket_label_key.raw()) << BUCKET_LABEL_SHIFT)
}

/// Replaces only the overflow-log head byte in a packed bucket word.
#[inline]
pub fn replace_bucket_overflow_log_head(word: u64, overflow_log_head: i32) -> Option<u64> {
    let head = try_encode_overflow_log_byte(overflow_log_head)?;
    Some((word & !BUCKET_LOG_MASK) | (u64::from(head) << BUCKET_LOG_SHIFT))
}

/// Replaces only the value-width code in a packed bucket word.
#[inline]
pub fn replace_bucket_value_width_code(word: u64, value_width_code: ValueWidthCode) -> u64 {
    let code = value_width_code as u64;
    debug_assert!(code <= 7);
    (word & !BUCKET_VALUE_WIDTH_MASK) | (code << BUCKET_VALUE_WIDTH_SHIFT)
}

/// Label key field from a packed bucket word.
#[inline]
pub fn decode_bucket_label_key(word: u64) -> BucketLabelKey {
    BucketLabelKey::from_raw(((word >> BUCKET_LABEL_SHIFT) & 0xFFFF) as u16)
}

/// Overflow log head from a packed bucket word.
#[inline]
pub fn decode_bucket_overflow_log_head(word: u64) -> i32 {
    decode_overflow_log_byte(((word >> BUCKET_LOG_SHIFT) & 0xFF) as u8)
}

/// Value-width code from a packed bucket word.
#[inline]
pub fn decode_bucket_value_width_code(word: u64) -> ValueWidthCode {
    match bucket_value_width_code_raw(word) {
        0 => ValueWidthCode::Zero,
        1 => ValueWidthCode::W1,
        2 => ValueWidthCode::W2,
        3 => ValueWidthCode::W4,
        4 => ValueWidthCode::W8,
        5 => ValueWidthCode::W16,
        6 => ValueWidthCode::W32,
        7 => ValueWidthCode::W64,
        _ => ValueWidthCode::Zero,
    }
}

/// Raw 3-bit value-width code from a packed bucket word.
#[inline]
pub fn bucket_value_width_code_raw(word: u64) -> u8 {
    ((word >> BUCKET_VALUE_WIDTH_SHIFT) & 0x7) as u8
}

/// Returns `true` when the bucket word top bit (bit 63) is zero.
#[inline]
pub fn bucket_word_has_zero_reserved(word: u64) -> bool {
    word & BUCKET_TOP_BIT_MASK == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_word_round_trips_fields() {
        let word = encode_bucket_word(
            0x0F_FFFF_FFFF,
            BucketLabelKey::from_raw(0xA5A5),
            169,
            ValueWidthCode::W2,
        );
        assert_eq!(decode_slot_index(word), 0x0F_FFFF_FFFF);
        assert_eq!(decode_bucket_label_key(word).raw(), 0xA5A5);
        assert_eq!(decode_bucket_overflow_log_head(word), 169);
        assert_eq!(decode_bucket_value_width_code(word), ValueWidthCode::W2);
        assert!(bucket_word_has_zero_reserved(word));
    }

    #[test]
    fn replace_bucket_fields_preserves_other_bits() {
        let word = encode_bucket_word(100, BucketLabelKey::from_raw(1), 9, ValueWidthCode::W4);
        let relabeled = replace_bucket_label_key(word, BucketLabelKey::from_raw(0xBEEF));
        assert_eq!(decode_bucket_label_key(relabeled).raw(), 0xBEEF);
        assert_eq!(decode_slot_index(relabeled), 100);
        assert_eq!(decode_bucket_overflow_log_head(relabeled), 9);
        assert_eq!(
            decode_bucket_value_width_code(relabeled),
            ValueWidthCode::W4
        );
        let relogged = replace_bucket_overflow_log_head(relabeled, 42).unwrap();
        assert_eq!(decode_bucket_overflow_log_head(relogged), 42);
        assert_eq!(decode_bucket_label_key(relogged).raw(), 0xBEEF);
    }

    #[test]
    fn value_width_code_round_trips_byte_width() {
        for code in ValueWidthCode::VALID {
            assert_eq!(
                ValueWidthCode::from_byte_width(code.byte_width()),
                Some(code)
            );
        }
    }
}
