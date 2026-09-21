//! LabelBucket packed-word helpers.
//!
//! Re-exports shared [`crate::slab_index`] bounds/encode routines and adds
//! [`LabelBucket`]-specific field packing in the high bits of the wire word.

pub use crate::slab_index::*;

use crate::labeled::bucket_label_key::BucketLabelKey;

const BUCKET_LABEL_SHIFT: u32 = 36;
const BUCKET_LABEL_MASK: u64 = 0xFFFF << BUCKET_LABEL_SHIFT;
/// Bit 52 of the bucket word: tiny-mode flag (1 = descriptor-resident inline
/// targets, 0 = slab/tree interpretation). ADR 0096 §1. Tiny∧tree is invalid
/// (rejected by validation).
///
/// Moved from bit 60 by ADR 0097 §3, which retired the 8-bit edge overflow-log
/// head: the edge side has no log from the moment the spill store occupies its
/// region, so there is nothing to preserve.
pub const BUCKET_TINY_MODE_BIT: u64 = 1u64 << 52;
/// Bit 53 of the bucket word: tree-mode flag (1 = tree mode / LTB-backed,
/// 0 = slab mode). See Plan 0318 §Step 1 and ADR 0088 §1. Moved from bit 63 by
/// ADR 0097 §3.
pub const BUCKET_TREE_MODE_BIT: u64 = 1u64 << 53;
/// Word bits 54..64: **reserve for the edge spill run id's low 10 bits**, which
/// ADR 0097 §3 adds in the next slice. Named rather than left as unexplained
/// slack, and it cannot raise the edge arena's bound: that bound is set by the
/// unlabeled `Vertex` tail28, which shares the arena (`design/storage/lara.md`).
pub const BUCKET_WORD_RESERVE_BITS: u32 = 10;
/// Shift of [`BUCKET_WORD_RESERVE_BITS`] in the packed word.
pub const BUCKET_WORD_RESERVE_SHIFT: u32 = 54;
/// Mask of [`BUCKET_WORD_RESERVE_BITS`] in the packed word.
pub const BUCKET_WORD_RESERVE_MASK: u64 =
    ((1u64 << BUCKET_WORD_RESERVE_BITS) - 1) << BUCKET_WORD_RESERVE_SHIFT;

/// Packs a [`super::record::LabelBucket`] wire word.
#[inline]
pub fn encode_bucket_word(edge_start: u64, bucket_label_key: BucketLabelKey) -> u64 {
    try_encode_bucket_word(edge_start, bucket_label_key)
        .expect("label bucket packed word out of range")
}

/// Fallible [`encode_bucket_word`].
#[inline]
pub fn try_encode_bucket_word(edge_start: u64, bucket_label_key: BucketLabelKey) -> Option<u64> {
    if !slot_index_fits(edge_start) {
        return None;
    }
    Some(edge_start | (u64::from(bucket_label_key.raw()) << BUCKET_LABEL_SHIFT))
}

/// Replaces only the label-key field in a packed bucket word.
#[inline]
pub fn replace_bucket_label_key(word: u64, bucket_label_key: BucketLabelKey) -> u64 {
    (word & !BUCKET_LABEL_MASK) | (u64::from(bucket_label_key.raw()) << BUCKET_LABEL_SHIFT)
}

/// Label key field from a packed bucket word.
#[inline]
pub fn decode_bucket_label_key(word: u64) -> BucketLabelKey {
    BucketLabelKey::from_raw(((word >> BUCKET_LABEL_SHIFT) & 0xFFFF) as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bucket_word_round_trips_fields() {
        let word = encode_bucket_word(0x0F_FFFF_FFFF, BucketLabelKey::from_raw(0xA5A5));
        assert_eq!(decode_slot_index(word), 0x0F_FFFF_FFFF);
        assert_eq!(decode_bucket_label_key(word).raw(), 0xA5A5);
    }

    #[test]
    fn the_word_tail_is_the_mode_flags_and_the_named_reserve() {
        // ADR 0097 §3 retired the 8-bit edge log head: bits 52..64 are now the tiny flag (52), the
        // tree flag (53) and the named reserve (54..64), which the next slice turns into the edge
        // spill run id's low 10 bits. Nothing else lives there, and the flags must not overlap it.
        let base = encode_bucket_word(0x0F_FFFF_FFFF, BucketLabelKey::from_raw(0xA5A5));
        let word = base | BUCKET_TINY_MODE_BIT | BUCKET_TREE_MODE_BIT;
        assert_ne!(word & BUCKET_TINY_MODE_BIT, 0);
        assert_ne!(word & BUCKET_TREE_MODE_BIT, 0);
        assert_eq!(word & BUCKET_WORD_RESERVE_MASK, 0);
        assert_eq!(decode_slot_index(word), 0x0F_FFFF_FFFF);
        assert_eq!(decode_bucket_label_key(word).raw(), 0xA5A5);
        // The reserve is the top 10 bits, and the flags sit immediately below it.
        assert_eq!(BUCKET_WORD_RESERVE_SHIFT, 54);
        assert_eq!(BUCKET_TINY_MODE_BIT, 1 << 52);
        assert_eq!(BUCKET_TREE_MODE_BIT, 1 << 53);
    }

    #[test]
    fn replace_bucket_label_key_preserves_other_bits() {
        let word = encode_bucket_word(100, BucketLabelKey::from_raw(1));
        let relabeled = replace_bucket_label_key(word, BucketLabelKey::from_raw(0xBEEF));
        assert_eq!(decode_bucket_label_key(relabeled).raw(), 0xBEEF);
        assert_eq!(decode_slot_index(relabeled), 100);
    }
}
