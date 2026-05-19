//! LabelBucket packed-word helpers.
//!
//! Re-exports shared [`crate::slab_index`] bounds/encode routines and adds
//! [`LabelBucket`]-specific field packing in the high bits of the wire word.

pub use crate::slab_index::*;

use crate::labeled::bucket_label_key::BucketLabelKey;

const BUCKET_LABEL_SHIFT: u32 = 36;
const BUCKET_LOG_SHIFT: u32 = 52;
const BUCKET_RESERVED_SHIFT: u32 = 60;
const BUCKET_LABEL_MASK: u64 = 0xFFFF << BUCKET_LABEL_SHIFT;
const BUCKET_LOG_MASK: u64 = 0xFF << BUCKET_LOG_SHIFT;
const BUCKET_RESERVED_MASK: u64 = 0xF << BUCKET_RESERVED_SHIFT;

/// Packs a [`super::record::LabelBucket`] wire word.
#[inline]
pub fn encode_bucket_word(
    edge_start: u64,
    bucket_label_key: BucketLabelKey,
    overflow_log_head: i32,
    reserved: u8,
) -> u64 {
    try_encode_bucket_word(edge_start, bucket_label_key, overflow_log_head, reserved)
        .expect("label bucket packed word out of range")
}

/// Fallible [`encode_bucket_word`].
#[inline]
pub fn try_encode_bucket_word(
    edge_start: u64,
    bucket_label_key: BucketLabelKey,
    overflow_log_head: i32,
    reserved: u8,
) -> Option<u64> {
    if !slot_index_fits(edge_start) || reserved > 0x0F {
        return None;
    }
    let head = try_encode_overflow_log_byte(overflow_log_head)?;
    Some(
        edge_start
            | (u64::from(bucket_label_key.raw()) << BUCKET_LABEL_SHIFT)
            | (u64::from(head) << BUCKET_LOG_SHIFT)
            | (u64::from(reserved & 0x0F) << BUCKET_RESERVED_SHIFT),
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

/// Reserved top nibble of a packed bucket word.
#[inline]
pub fn bucket_reserved_nibble(word: u64) -> u8 {
    ((word >> BUCKET_RESERVED_SHIFT) & 0xF) as u8
}

/// Returns `true` when the bucket word reserved nibble is zero.
#[inline]
pub fn bucket_word_has_zero_reserved(word: u64) -> bool {
    word & BUCKET_RESERVED_MASK == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_word_round_trips_fields() {
        let word = encode_bucket_word(0x0F_FFFF_FFFF, BucketLabelKey::from_raw(0xA5A5), 169, 0);
        assert_eq!(decode_slot_index(word), 0x0F_FFFF_FFFF);
        assert_eq!(decode_bucket_label_key(word).raw(), 0xA5A5);
        assert_eq!(decode_bucket_overflow_log_head(word), 169);
        assert!(bucket_word_has_zero_reserved(word));
    }

    #[test]
    fn replace_bucket_fields_preserves_other_bits() {
        let word = encode_bucket_word(100, BucketLabelKey::from_raw(1), 9, 0);
        let relabeled = replace_bucket_label_key(word, BucketLabelKey::from_raw(0xBEEF));
        assert_eq!(decode_bucket_label_key(relabeled).raw(), 0xBEEF);
        assert_eq!(decode_slot_index(relabeled), 100);
        assert_eq!(decode_bucket_overflow_log_head(relabeled), 9);
        let relogged = replace_bucket_overflow_log_head(relabeled, 42).unwrap();
        assert_eq!(decode_bucket_overflow_log_head(relogged), 42);
        assert_eq!(decode_bucket_label_key(relogged).raw(), 0xBEEF);
    }
}
