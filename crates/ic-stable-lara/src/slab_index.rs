//! Shared 36-bit CSR slab slot indices (edge slab and label-bucket slab).

/// Width of a global slab slot index (`base_slot_start`, `edge_start`, …).
pub const SLOT_INDEX_BITS: u32 = 36;
/// All-ones mask for a valid slot index.
pub const SLOT_INDEX_MASK: u64 = (1u64 << SLOT_INDEX_BITS) - 1;
/// Maximum exclusive end of a slot range (last valid index is [`SLOT_INDEX_MASK`]).
pub const MAX_SLOT_EXCLUSIVE_END: u64 = SLOT_INDEX_MASK + 1;

/// Width of metadata packed above a slot index in a labeled vertex locator word.
pub const META28_BITS: u32 = 28;
/// All-ones mask for metadata in the low 28 bits of the metadata region.
pub const META28_MASK: u64 = (1u64 << META28_BITS) - 1;

/// Overflow-log head sentinel in an 8-bit wire field (`0..=169` valid).
pub const OVERFLOW_LOG_NONE: u8 = 0xFF;

/// Lower 36 bits of a packed locator or bucket word.
#[inline]
pub fn decode_slot_index(word: u64) -> u64 {
    word & SLOT_INDEX_MASK
}

/// Upper 28 bits of a labeled vertex locator word.
#[inline]
pub fn decode_meta28(word: u64) -> u32 {
    ((word >> SLOT_INDEX_BITS) & META28_MASK) as u32
}

/// Packs a vertex slot index and 28-bit metadata word.
#[inline]
pub fn encode_locator_word(slot: u64, meta28: u32) -> u64 {
    try_encode_locator_word(slot, meta28).expect("locator slot/metadata out of range")
}

/// Fallible [`encode_locator_word`].
#[inline]
pub fn try_encode_locator_word(slot: u64, meta28: u32) -> Option<u64> {
    if !slot_index_fits(slot) || u64::from(meta28) > META28_MASK {
        return None;
    }
    Some(slot | (u64::from(meta28) << SLOT_INDEX_BITS))
}

/// Replaces only the slot index bits in a packed word.
#[inline]
pub fn replace_slot_index(word: u64, slot: u64) -> u64 {
    try_replace_slot_index(word, slot).expect("slot index must fit in 36 bits")
}

/// Fallible [`replace_slot_index`].
#[inline]
pub fn try_replace_slot_index(word: u64, slot: u64) -> Option<u64> {
    if !slot_index_fits(slot) {
        return None;
    }
    Some((word & !SLOT_INDEX_MASK) | slot)
}

/// Returns `true` when `slot` fits in the 36-bit index space.
#[inline]
pub fn slot_index_fits(slot: u64) -> bool {
    slot <= SLOT_INDEX_MASK
}

/// Returns `true` when `end` is a valid exclusive end (`end <= MAX_SLOT_EXCLUSIVE_END`).
#[inline]
pub fn slot_exclusive_end_fits(end: u64) -> bool {
    end <= MAX_SLOT_EXCLUSIVE_END
}

/// Adds two slot indices and rejects sums above [`SLOT_INDEX_MASK`].
#[inline]
pub fn checked_add_slot_index(lhs: u64, rhs: u64) -> Option<u64> {
    lhs.checked_add(rhs).filter(|sum| slot_index_fits(*sum))
}

/// Adds to an exclusive end and rejects sums above [`MAX_SLOT_EXCLUSIVE_END`].
#[inline]
pub fn checked_add_slot_exclusive_end(lhs: u64, rhs: u64) -> Option<u64> {
    lhs.checked_add(rhs)
        .filter(|sum| slot_exclusive_end_fits(*sum))
}

/// Returns `Ok(())` when `elem_capacity` fits the 36-bit slab index space.
#[inline]
pub fn validate_elem_capacity(elem_capacity: u64) -> Result<(), ()> {
    if slot_exclusive_end_fits(elem_capacity) {
        Ok(())
    } else {
        Err(())
    }
}

/// [`validate_elem_capacity`] mapped to [`crate::GrowFailed`].
///
/// Uses `delta: 0` to distinguish validation failure from a real memory grow attempt.
#[inline]
pub fn validate_elem_capacity_grow_failed(
    elem_capacity: u64,
    current_size: u64,
) -> Result<(), crate::GrowFailed> {
    validate_elem_capacity(elem_capacity).map_err(|()| crate::GrowFailed {
        current_size,
        delta: 0,
    })
}

/// Unlabeled [`crate::lara::vertex::Vertex`] tail28: bit 0 tombstone; bits 1–27 encode `(log_head + 1)` (`0` = no log).
const VERTEX_TAIL_TOMBSTONE_BIT: u32 = 1;
const VERTEX_TAIL_LOG_SHIFT: u32 = 1;
/// Maximum `(log_head + 1)` encoding for unlabeled vertex tail28.
pub const VERTEX_TAIL_LOG_MASK: u32 = (1 << 27) - 1;

/// Packs overflow-log head and tombstone into 28 bits of a locator word.
#[inline]
pub fn pack_vertex_tail28(log_head: i32, tombstone: bool) -> u32 {
    let enc = if log_head < 0 {
        0u32
    } else {
        let e = (log_head as u32).wrapping_add(1);
        debug_assert!(e <= VERTEX_TAIL_LOG_MASK);
        e
    };
    let mut raw = enc << VERTEX_TAIL_LOG_SHIFT;
    if tombstone {
        raw |= VERTEX_TAIL_TOMBSTONE_BIT;
    }
    debug_assert!(u64::from(raw) <= META28_MASK);
    raw
}

/// Unpacks [`pack_vertex_tail28`].
#[inline]
pub fn unpack_vertex_tail28(raw: u32) -> (i32, bool) {
    let deleted = (raw & VERTEX_TAIL_TOMBSTONE_BIT) != 0;
    let enc = (raw >> VERTEX_TAIL_LOG_SHIFT) & VERTEX_TAIL_LOG_MASK;
    let log_head = if enc == 0 { -1 } else { (enc - 1) as i32 };
    (log_head, deleted)
}

/// Fallible [`pack_vertex_tail28`] (log head must fit in 27 payload bits).
#[inline]
pub fn try_pack_vertex_tail28(log_head: i32, tombstone: bool) -> Option<u32> {
    if log_head >= 0 {
        let e = (log_head as u32).wrapping_add(1);
        if e > VERTEX_TAIL_LOG_MASK {
            return None;
        }
    }
    Some(pack_vertex_tail28(log_head, tombstone))
}

/// Encodes an overflow-log head (`-1` → [`OVERFLOW_LOG_NONE`]).
#[inline]
pub fn encode_overflow_log_byte(head: i32) -> u8 {
    try_encode_overflow_log_byte(head).expect("overflow log head out of range")
}

/// Fallible [`encode_overflow_log_byte`].
#[inline]
pub fn try_encode_overflow_log_byte(head: i32) -> Option<u8> {
    if head < 0 {
        Some(OVERFLOW_LOG_NONE)
    } else if head < 170 {
        Some(head as u8)
    } else {
        None
    }
}

/// Decodes an overflow-log head byte.
#[inline]
pub fn decode_overflow_log_byte(byte: u8) -> i32 {
    if byte == OVERFLOW_LOG_NONE {
        -1
    } else {
        i32::from(byte)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locator_round_trips_slot_and_meta() {
        let slot = SLOT_INDEX_MASK;
        let meta = META28_MASK as u32;
        let word = encode_locator_word(slot, meta);
        assert_eq!(decode_slot_index(word), slot);
        assert_eq!(decode_meta28(word), meta);
    }

    #[test]
    fn try_replace_rejects_slot_above_36_bits() {
        let word = encode_locator_word(10, 0);
        assert_eq!(
            try_replace_slot_index(word, SLOT_INDEX_MASK),
            Some(SLOT_INDEX_MASK)
        );
        assert_eq!(try_replace_slot_index(word, SLOT_INDEX_MASK + 1), None);
    }

    #[test]
    fn validate_elem_capacity_rejects_above_index_space() {
        assert!(validate_elem_capacity(MAX_SLOT_EXCLUSIVE_END).is_ok());
        assert!(validate_elem_capacity(MAX_SLOT_EXCLUSIVE_END + 1).is_err());
    }

    #[test]
    fn exclusive_end_bound_is_one_past_max_index() {
        assert!(slot_exclusive_end_fits(MAX_SLOT_EXCLUSIVE_END));
        assert!(!slot_exclusive_end_fits(MAX_SLOT_EXCLUSIVE_END + 1));
        assert_eq!(
            checked_add_slot_exclusive_end(SLOT_INDEX_MASK, 1),
            Some(MAX_SLOT_EXCLUSIVE_END)
        );
    }
}
