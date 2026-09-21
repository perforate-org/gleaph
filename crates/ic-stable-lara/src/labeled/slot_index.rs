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
/// Word bits 54..64: the **low 10 bits of the edge spill run id** (ADR 0097 §3).
/// The id's other 17 bits and the whole values id live in the bytes the row
/// gained at 29..35, packed by `record.rs`. These bits cannot raise the edge
/// arena's bound: that bound is set by the unlabeled `Vertex` tail28, which
/// shares the arena (`design/storage/lara.md`).
pub const BUCKET_EDGE_RUN_LOW_BITS: u32 = 10;
/// Shift of [`BUCKET_EDGE_RUN_LOW_BITS`] in the packed word.
pub const BUCKET_EDGE_RUN_LOW_SHIFT: u32 = 54;
const BUCKET_EDGE_RUN_LOW_MASK: u64 =
    ((1u64 << BUCKET_EDGE_RUN_LOW_BITS) - 1) << BUCKET_EDGE_RUN_LOW_SHIFT;

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

/// Low bits of the edge spill run id from a packed bucket word.
#[inline]
pub fn decode_bucket_word_edge_run_low(word: u64) -> u32 {
    ((word & BUCKET_EDGE_RUN_LOW_MASK) >> BUCKET_EDGE_RUN_LOW_SHIFT) as u32
}

/// Clears the edge spill run id's low bits in a packed bucket word.
///
/// The wire word carries them (bits 54..64) so a decoded row must not keep them: the run id lives
/// in the row's own field, and `LabelBucket::write_to` re-injects them. Keeping both in sync would
/// duplicate the id in two places (ADR 0097 §3 keeps the descriptor field the single owner).
#[inline]
pub fn clear_bucket_word_edge_run_low(word: u64) -> u64 {
    word & !BUCKET_EDGE_RUN_LOW_MASK
}

/// Replaces only the edge spill run id's low bits in a packed bucket word.
#[inline]
pub fn replace_bucket_word_edge_run_low(word: u64, low: u32) -> u64 {
    debug_assert!(low < (1 << BUCKET_EDGE_RUN_LOW_BITS));
    (word & !BUCKET_EDGE_RUN_LOW_MASK) | (u64::from(low) << BUCKET_EDGE_RUN_LOW_SHIFT)
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
    fn the_word_tail_is_the_mode_flags_and_the_edge_id_low_bits() {
        // ADR 0097 §3 retired the 8-bit edge log head: bits 52..64 are now the tiny flag (52), the
        // tree flag (53) and the edge spill run id's low 10 bits (54..64). Nothing else lives there,
        // and the flags must not overlap the id.
        let base = encode_bucket_word(0x0F_FFFF_FFFF, BucketLabelKey::from_raw(0xA5A5));
        let word = replace_bucket_word_edge_run_low(
            base | BUCKET_TINY_MODE_BIT | BUCKET_TREE_MODE_BIT,
            0x3FF,
        );
        assert_eq!(decode_bucket_word_edge_run_low(word), 0x3FF);
        assert_ne!(word & BUCKET_TINY_MODE_BIT, 0);
        assert_ne!(word & BUCKET_TREE_MODE_BIT, 0);
        assert_eq!(decode_slot_index(word), 0x0F_FFFF_FFFF);
        assert_eq!(decode_bucket_label_key(word).raw(), 0xA5A5);
        // The flags and the id do not overlap: setting one leaves the others alone.
        let only_tiny = base | BUCKET_TINY_MODE_BIT;
        assert_eq!(decode_bucket_word_edge_run_low(only_tiny), 0);
        assert_eq!(only_tiny & BUCKET_TREE_MODE_BIT, 0);
        assert_eq!(BUCKET_EDGE_RUN_LOW_SHIFT, 54);
    }

    #[test]
    fn replace_bucket_label_key_preserves_other_bits() {
        let word = encode_bucket_word(100, BucketLabelKey::from_raw(1));
        let relabeled = replace_bucket_label_key(word, BucketLabelKey::from_raw(0xBEEF));
        assert_eq!(decode_bucket_label_key(relabeled).raw(), 0xBEEF);
        assert_eq!(decode_slot_index(relabeled), 100);
        let with_id = replace_bucket_word_edge_run_low(relabeled, 42);
        assert_eq!(decode_bucket_word_edge_run_low(with_id), 42);
        assert_eq!(decode_bucket_label_key(with_id).raw(), 0xBEEF);
    }
}
