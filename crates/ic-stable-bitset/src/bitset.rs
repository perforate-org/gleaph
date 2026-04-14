//! Heap-mirrored stable bitset implementation.
//!
//! The implementation is split into three pieces:
//!
//! - a stable header with layout metadata,
//! - a packed `u64` snapshot of the bitset words,
//! - an append-only journal of **5-byte** packed records for pending `set`, `truncate`, and `remove`
//!   updates.
//!
//! Each journal record is 40 little-endian bits (see [`crate::JOURNAL_RECORD_RAW_MASK`]); the
//! API allows bit indices up to `u32::MAX` and exclusive logical length up to `u32::MAX + 1`
//! ([`crate::JOURNAL_LEN_MAX`]).
//!
//! Reads always use the heap mirror so membership checks stay cheap. Writes are durable-first:
//! they append a packed journal record, then update the heap mirror, and finally checkpoint the
//! heap back into stable memory once the journal is full.

use crate::memory::{
    GrowFailed, grow_memory_to_at_least_bytes, read_bytes, read_u64, read_u64_words_vec, write,
    write_5_bytes, write_u64, write_u64_words_direct, write_zero_bytes,
};
use core::cell::{Cell, Ref, RefCell};
use core::fmt;
use ic_stable_structures::Memory;

const MAGIC: [u8; 3] = *b"SBS";
const VERSION: u8 = 2;
const HEADER_SIZE: u64 = 64;
const JOURNAL_RECORD_SIZE: u64 = 5;

const MAGIC_OFFSET: u64 = 0;
const VERSION_OFFSET: u64 = 3;
const LEN_OFFSET: u64 = 4;
const WORD_CAP_OFFSET: u64 = 12;
/// Header field: must equal [`crate::JOURNAL_CAP_SLOTS`] as `u64` (fixed journal size on disk).
const JOURNAL_SLOTS_METADATA_OFFSET: u64 = 20;

#[derive(Clone, Debug)]
struct HeapState {
    len_bits: u64,
    word_cap: u64,
    words: Vec<u64>,
}

impl HeapState {
    fn new() -> Self {
        Self {
            len_bits: 0,
            word_cap: 0,
            words: Vec::new(),
        }
    }
}

/// Error returned when a stable bitset cannot be opened from memory.
#[derive(Debug, PartialEq, Eq)]
pub enum InitError {
    BadMagic { actual: [u8; 3], expected: [u8; 3] },
    IncompatibleVersion(u8),
    InvalidLayout,
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual, expected } => {
                write!(f, "bad magic number {actual:?}, expected {expected:?}")
            }
            Self::IncompatibleVersion(version) => write!(
                f,
                "unsupported layout version {version}; supported version number is {VERSION}"
            ),
            Self::InvalidLayout => write!(f, "invalid stable bitset layout"),
            Self::OutOfMemory => write!(f, "failed to allocate memory for stable bitset"),
        }
    }
}

impl std::error::Error for InitError {}

/// Returns the number of `u64` words needed to store `bits` bits.
fn words_for_bits(bits: u64) -> u64 {
    if bits == 0 { 0 } else { (bits - 1) / 64 + 1 }
}

/// Start offset of the journal region in stable memory.
fn journal_offset() -> u64 {
    HEADER_SIZE
}

fn journal_end_bytes() -> u64 {
    journal_offset().saturating_add((crate::JOURNAL_CAP_SLOTS as u64).saturating_mul(JOURNAL_RECORD_SIZE))
}

/// Start of the packed `u64` snapshot; always 8-byte aligned after zero padding.
fn snapshot_base() -> u64 {
    let end = journal_end_bytes();
    (end + 7) & !7
}

/// Converts a bit index into `(word_index, bit_index)`.
fn bit_offset(index: u64) -> (usize, u64) {
    ((index / 64) as usize, index & 63)
}

#[inline]
/// Clears any bits beyond the logical length.
fn clear_suffix(words: &mut [u64], len_bits: u64) {
    let full_words = words_for_bits(len_bits);
    let full_words_usize = full_words as usize;
    if full_words_usize < words.len() {
        words[full_words_usize..].fill(0);
    }
    if !len_bits.is_multiple_of(64) {
        let idx = full_words_usize.saturating_sub(1);
        if idx < words.len() {
            let keep = len_bits % 64;
            let mask = (1u64 << keep) - 1;
            words[idx] &= mask;
        }
    }
}

/// Reads the stable header fields.
fn read_header<M: Memory>(memory: &M) -> ([u8; 3], u8, u64, u64, u64) {
    let mut magic = [0u8; 3];
    let mut version = [0u8; 1];
    memory.read(MAGIC_OFFSET, &mut magic);
    memory.read(VERSION_OFFSET, &mut version);
    let len_bits = read_u64(memory, LEN_OFFSET);
    let word_cap = read_u64(memory, WORD_CAP_OFFSET);
    let journal_slots = read_u64(memory, JOURNAL_SLOTS_METADATA_OFFSET);
    (magic, version[0], len_bits, word_cap, journal_slots)
}

/// Writes the stable header fields.
fn write_header<M: Memory>(
    memory: &M,
    len_bits: u64,
    word_cap: u64,
) -> Result<(), GrowFailed> {
    write(memory, MAGIC_OFFSET, &MAGIC);
    write(memory, VERSION_OFFSET, &[VERSION]);
    write_u64(memory, LEN_OFFSET, len_bits);
    write_u64(memory, WORD_CAP_OFFSET, word_cap);
    write_u64(
        memory,
        JOURNAL_SLOTS_METADATA_OFFSET,
        crate::JOURNAL_CAP_SLOTS as u64,
    );
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JournalTag {
    Empty = 0,
    SetLen = 1,
    SetBit = 2,
    Remove = 3,
}

/// 5-byte journal record (40 bits LE). Same bit layout as `ic-stable-roaring`; `tag` includes
/// `Remove = 3`. Replay ends at five zero bytes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JournalRecord([u8; 5]);

impl JournalRecord {
    fn set_len(len: u64) -> Self {
        assert!(
            len <= crate::JOURNAL_LEN_MAX,
            "bitset length exceeds supported u32 index space"
        );
        let payload_lo = len as u32;
        let len_hi = ((len >> 32) & 1) as u32;
        Self::pack_fields(JournalTag::SetLen, false, payload_lo, len_hi)
    }

    fn set_bit(index: u32, value: bool) -> Self {
        Self::pack_fields(JournalTag::SetBit, value, index, 0)
    }

    fn remove(index: u32) -> Self {
        Self::pack_fields(JournalTag::Remove, false, index, 0)
    }

    fn pack_fields(tag: JournalTag, value: bool, payload_lo: u32, len_hi: u32) -> Self {
        let raw = (payload_lo as u64)
            | (((len_hi & 1) as u64) << 32)
            | (((value as u64) & 1) << 37)
            | (((tag as u64) & 3) << 38);
        Self::from_raw(raw)
    }

    fn from_raw(raw: u64) -> Self {
        let raw = raw & crate::JOURNAL_RECORD_RAW_MASK;
        let b = raw.to_le_bytes();
        Self([b[0], b[1], b[2], b[3], b[4]])
    }

    fn raw(&self) -> u64 {
        let mut w = [0u8; 8];
        w[..5].copy_from_slice(&self.0);
        u64::from_le_bytes(w) & crate::JOURNAL_RECORD_RAW_MASK
    }

    fn unpack(self) -> Result<(JournalTag, bool, u64), InitError> {
        let raw = self.raw();
        if raw == 0 {
            return Ok((JournalTag::Empty, false, 0));
        }
        let reserved = (raw >> 33) & 0xF;
        if reserved != 0 {
            return Err(InitError::InvalidLayout);
        }
        let tag_bits = (raw >> 38) & 3;
        let tag = match tag_bits {
            0 => return Err(InitError::InvalidLayout),
            1 => JournalTag::SetLen,
            2 => JournalTag::SetBit,
            3 => JournalTag::Remove,
            _ => return Err(InitError::InvalidLayout),
        };
        let value = ((raw >> 37) & 1) != 0;
        let len_hi = (raw >> 32) & 1;
        let payload_lo = raw as u32;
        let payload = match tag {
            JournalTag::SetLen => (len_hi << 32) | (payload_lo as u64),
            JournalTag::SetBit | JournalTag::Remove => {
                if len_hi != 0 {
                    return Err(InitError::InvalidLayout);
                }
                payload_lo as u64
            }
            JournalTag::Empty => unreachable!(),
        };
        Ok((tag, value, payload))
    }
}

/// Stable bitset with a heap mirror and a durable journal.
///
/// # Storage model
///
/// The type keeps the read path in a heap-backed `Vec<u64>` and mirrors updates into stable memory
/// via a small journal. Once the journal is full, the current heap state is checkpointed into the
/// stable snapshot and the journal is cleared.
///
/// # Operational notes
///
/// - The type is intended for single-writer use.
/// - `contains` always reads the heap mirror.
/// - `set`, `remove`, and `truncate` are durable-first and may trigger checkpointing.
pub struct Bitset<M: Memory> {
    memory: M,
    state: RefCell<HeapState>,
    journal_len: Cell<u64>,
}

/// Borrowed view for repeated membership checks against a [`Bitset`].
pub struct ContainsView<'a> {
    state: Ref<'a, HeapState>,
}

impl ContainsView<'_> {
    /// Tests whether the selected bit is set while reusing a previously borrowed heap mirror.
    #[inline]
    pub fn contains(&self, index: u32) -> bool {
        let index = u64::from(index);
        if index >= self.state.len_bits {
            return false;
        }
        let word = (index >> 6) as usize;
        let bit_mask = 1u64 << (index & 63);
        unsafe { (*self.state.words.get_unchecked(word) & bit_mask) != 0 }
    }
}

impl<M: Memory> fmt::Debug for Bitset<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = self.state.borrow();
        f.debug_struct("Bitset")
            .field("len_bits", &st.len_bits)
            .field("word_cap", &st.word_cap)
            .field("journal_len", &self.journal_len.get())
            .field("journal_cap_slots", &crate::JOURNAL_CAP_SLOTS)
            .finish()
    }
}

impl<M: Memory> Bitset<M> {
    /// Creates a new empty bitset using the provided stable memory.
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        write_header(&memory, 0, 0)?;
        let snap = snapshot_base();
        grow_memory_to_at_least_bytes(&memory, snap)?;
        let journal_end = journal_end_bytes();
        if journal_end < snap {
            write_zero_bytes(&memory, journal_end, snap - journal_end)?;
        }
        Ok(Self {
            memory,
            state: RefCell::new(HeapState::new()),
            journal_len: Cell::new(0),
        })
    }

    /// Reopens a bitset from stable memory and replays any pending mutation records.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let (magic, version, len_bits, word_cap, journal_slots) = read_header(&memory);
        if magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: magic,
                expected: MAGIC,
            });
        }
        if version != VERSION {
            return Err(InitError::IncompatibleVersion(version));
        }
        if journal_slots != crate::JOURNAL_CAP_SLOTS as u64 {
            return Err(InitError::InvalidLayout);
        }
        let need = snapshot_base().saturating_add(word_cap.saturating_mul(8));
        let size_bytes = memory
            .size()
            .checked_mul(crate::memory::WASM_PAGE_SIZE)
            .expect("address overflow");
        if size_bytes < need {
            return Err(InitError::InvalidLayout);
        }
        if len_bits > crate::JOURNAL_LEN_MAX {
            return Err(InitError::InvalidLayout);
        }
        let words = read_u64_words_vec(&memory, snapshot_base(), word_cap);
        let mut state = HeapState {
            len_bits,
            word_cap,
            words,
        };
        clear_suffix(&mut state.words, state.len_bits);

        let mut journal_len = 0u64;
        let mut chunk_buf = [0u8; crate::JOURNAL_READ_CHUNK_BYTES];
        let n_chunks = crate::JOURNAL_REGION_BYTES / crate::JOURNAL_READ_CHUNK_BYTES;
        'replay: for chunk_idx in 0..n_chunks {
            let off = journal_offset() + (chunk_idx * crate::JOURNAL_READ_CHUNK_BYTES) as u64;
            read_bytes(&memory, off, &mut chunk_buf);
            for slot in chunk_buf.chunks_exact(JOURNAL_RECORD_SIZE as usize) {
                let slot: [u8; 5] = slot.try_into().expect("chunks_exact by 5");
                if slot == [0u8; 5] {
                    break 'replay;
                }
                apply_record(&mut state, JournalRecord(slot))?;
                journal_len += 1;
            }
        }
        Ok(Self {
            memory,
            state: RefCell::new(state),
            journal_len: Cell::new(journal_len),
        })
    }

    /// Returns the underlying stable memory handle.
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Returns the logical bit length.
    pub fn len(&self) -> u64 {
        self.state.borrow().len_bits
    }

    /// Returns `true` when the bitset is empty.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Tests whether the selected bit is set.
    #[inline]
    pub fn contains(&self, index: u32) -> bool {
        let index = u64::from(index);
        let st = self.state.borrow();
        if index >= st.len_bits {
            return false;
        }
        let word = (index >> 6) as usize;
        let bit_mask = 1u64 << (index & 63);
        unsafe { (*st.words.get_unchecked(word) & bit_mask) != 0 }
    }

    /// Returns a borrowed view that can be reused for repeated membership checks during a scan.
    #[inline]
    pub fn contains_view(&self) -> ContainsView<'_> {
        ContainsView {
            state: self.state.borrow(),
        }
    }

    /// Ensures that the logical length is at least `min_len`.
    pub fn ensure_len(&self, min_len: u64) -> Result<(), GrowFailed> {
        assert!(
            min_len <= crate::JOURNAL_LEN_MAX,
            "bitset length exceeds supported u32 index space"
        );
        let current = self.len();
        if min_len <= current {
            return Ok(());
        }
        self.ensure_word_capacity(words_for_bits(min_len))?;
        self.append_record(JournalRecord::set_len(min_len))?;
        {
            let mut st = self.state.borrow_mut();
            st.len_bits = min_len;
            let len = st.len_bits;
            clear_suffix(&mut st.words, len);
        }
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Sets or clears a bit.
    pub fn set(&self, index: u32, value: bool) -> Result<(), GrowFailed> {
        let i = u64::from(index);
        let need_len = i.saturating_add(1);
        assert!(
            need_len <= crate::JOURNAL_LEN_MAX,
            "bit index exceeds supported u32 index space"
        );
        if value {
            if need_len > self.len() {
                self.ensure_word_capacity(words_for_bits(need_len))?;
            }
            if !self.contains(index) {
                self.append_record(JournalRecord::set_bit(index, true))?;
                {
                    let mut st = self.state.borrow_mut();
                    if i >= st.len_bits {
                        st.len_bits = need_len;
                    }
                    set_heap_bit(&mut st.words, i, true);
                }
            }
            self.maybe_checkpoint()?;
            return Ok(());
        }

        if !self.contains(index) {
            return Ok(());
        }
        self.append_record(JournalRecord::set_bit(index, false))?;
        {
            let mut st = self.state.borrow_mut();
            if i < st.len_bits {
                set_heap_bit(&mut st.words, i, false);
            }
        }
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Inserts a bit by setting it to `true`.
    pub fn insert(&self, index: u32) -> Result<(), GrowFailed> {
        self.set(index, true)
    }

    /// Clears a bit by setting it to `false` without shifting later bits.
    pub fn clear(&self, index: u32) -> Result<(), GrowFailed> {
        self.set(index, false)
    }

    /// Removes a bit and shifts all later bits left by one position.
    pub fn remove(&self, index: u32) -> Result<(), GrowFailed> {
        let i = u64::from(index);
        assert!(
            i < self.len(),
            "remove index out of bounds: index={i} len={}",
            self.len()
        );
        self.append_record(JournalRecord::remove(index))?;
        {
            let mut st = self.state.borrow_mut();
            let len_bits = st.len_bits;
            let new_len = remove_heap_bit(&mut st.words, len_bits, i);
            st.len_bits = new_len;
        }
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Shrinks the logical length to `new_len`.
    pub fn truncate(&self, new_len: u64) -> Result<(), GrowFailed> {
        assert!(
            new_len <= crate::JOURNAL_LEN_MAX,
            "bitset length exceeds supported u32 index space"
        );
        if new_len >= self.len() {
            return Ok(());
        }
        self.append_record(JournalRecord::set_len(new_len))?;
        {
            let mut st = self.state.borrow_mut();
            st.len_bits = new_len;
            let len = st.len_bits;
            clear_suffix(&mut st.words, len);
        }
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Grows the stable snapshot capacity if needed.
    fn ensure_word_capacity(&self, need_words: u64) -> Result<(), GrowFailed> {
        let mut st = self.state.borrow_mut();
        if need_words <= st.word_cap {
            return Ok(());
        }
        let new_cap = need_words.max(st.word_cap.max(1).saturating_mul(2));
        let need_bytes = snapshot_base().saturating_add(new_cap.saturating_mul(8));
        grow_memory_to_at_least_bytes(&self.memory, need_bytes)?;
        let old_cap = st.word_cap as usize;
        let new_cap_usize = new_cap as usize;
        if st.words.len() < new_cap_usize {
            st.words.resize(new_cap_usize, 0);
        }
        st.word_cap = new_cap;
        write_u64(&self.memory, WORD_CAP_OFFSET, st.word_cap);
        if st.words.len() < old_cap {
            st.words.resize(old_cap, 0);
        }
        Ok(())
    }

    /// Appends a packed mutation record to the journal.
    fn append_record(&self, record: JournalRecord) -> Result<(), GrowFailed> {
        if self.journal_len.get() >= crate::JOURNAL_CAP_SLOTS as u64 {
            self.checkpoint()?;
        }
        let idx = self.journal_len.get();
        let base = journal_offset() + idx * JOURNAL_RECORD_SIZE;
        write_5_bytes(&self.memory, base, &record.0)?;
        self.journal_len.set(idx + 1);
        Ok(())
    }

    /// Checkpoints the heap mirror when the journal reaches capacity.
    fn maybe_checkpoint(&self) -> Result<(), GrowFailed> {
        if self.journal_len.get() >= crate::JOURNAL_CAP_SLOTS as u64 {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Writes the heap mirror back into stable memory and clears the journal.
    fn checkpoint(&self) -> Result<(), GrowFailed> {
        let (len_bits, word_cap) = {
            let st = self.state.borrow();
            let word_offset = snapshot_base();
            write_u64_words_direct(&self.memory, word_offset, &st.words);
            (st.len_bits, st.word_cap)
        };
        write_u64(&self.memory, LEN_OFFSET, len_bits);
        write_u64(&self.memory, WORD_CAP_OFFSET, word_cap);
        write_zero_bytes(
            &self.memory,
            journal_offset(),
            self.journal_len.get() * JOURNAL_RECORD_SIZE,
        )?;
        self.journal_len.set(0);
        Ok(())
    }
}

#[cfg(not(all(target_arch = "wasm32", target_feature = "simd128")))]
fn remove_heap_bit(words: &mut [u64], len_bits: u64, index: u64) -> u64 {
    remove_heap_bit_scalar(words, len_bits, index)
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
fn remove_heap_bit(words: &mut [u64], len_bits: u64, index: u64) -> u64 {
    unsafe { remove_heap_bit_simd(words, len_bits, index) }
}

fn remove_heap_bit_scalar(words: &mut [u64], len_bits: u64, index: u64) -> u64 {
    let new_len = len_bits - 1;
    debug_assert!(index < len_bits);

    if new_len == 0 {
        words.fill(0);
        return 0;
    }

    let start_word = (index >> 6) as usize;
    let start_bit = (index & 63) as u32;
    let last_word = (words_for_bits(new_len) - 1) as usize;

    if start_word <= last_word {
        let prefix_mask = if start_bit == 0 {
            0
        } else {
            (1u64 << start_bit) - 1
        };

        for word_idx in start_word..=last_word {
            let old = words[word_idx];
            let carry = words.get(word_idx + 1).copied().unwrap_or(0) & 1;

            let next_word = if word_idx == start_word && start_bit > 0 {
                (old & prefix_mask) | ((old >> 1) & !prefix_mask) | (carry << 63)
            } else {
                (old >> 1) | (carry << 63)
            };
            words[word_idx] = next_word;
        }
    }

    clear_suffix(words, new_len);
    new_len
}

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
// Empirically tuned for the current remove benchmarks: keep very short suffixes scalar,
// and route the larger head/mid cases through SIMD.
const SIMD_REMOVE_MIN_WORDS: usize = 8;

#[cfg(all(target_arch = "wasm32", target_feature = "simd128"))]
#[target_feature(enable = "simd128")]
unsafe fn remove_heap_bit_simd(words: &mut [u64], len_bits: u64, index: u64) -> u64 {
    use core::arch::wasm32::{
        i64x2_shl, i64x2_shr, u64x2_replace_lane, u64x2_splat, v128_load, v128_or, v128_store,
    };

    let new_len = len_bits - 1;
    debug_assert!(index < len_bits);

    if new_len == 0 {
        words.fill(0);
        return 0;
    }

    let start_word = (index >> 6) as usize;
    let start_bit = (index & 63) as u32;
    let last_word = (words_for_bits(new_len) - 1) as usize;
    let remaining_words = last_word.saturating_sub(start_word).saturating_add(1);

    if remaining_words < SIMD_REMOVE_MIN_WORDS {
        return remove_heap_bit_scalar(words, len_bits, index);
    }

    let mut cursor = start_word;
    if start_bit > 0 {
        let old = words[cursor];
        let carry = words.get(cursor + 1).copied().unwrap_or(0) & 1;
        let prefix_mask = (1u64 << start_bit) - 1;
        words[cursor] = (old & prefix_mask) | ((old >> 1) & !prefix_mask) | (carry << 63);
        cursor += 1;
    }

    while cursor + 1 <= last_word {
        let words_left = last_word - cursor + 1;
        if words_left < SIMD_REMOVE_MIN_WORDS {
            break;
        }

        let curr = unsafe { v128_load(words.as_ptr().add(cursor) as *const _) };
        let carry0 = words.get(cursor + 1).copied().unwrap_or(0) & 1;
        let carry1 = words.get(cursor + 2).copied().unwrap_or(0) & 1;

        let mut carry = u64x2_splat(0);
        carry = u64x2_replace_lane::<0>(carry, carry0);
        carry = u64x2_replace_lane::<1>(carry, carry1);

        let shifted = i64x2_shr(curr, 1);
        let out = v128_or(shifted, i64x2_shl(carry, 63));
        unsafe { v128_store(words.as_mut_ptr().add(cursor) as *mut _, out) };
        cursor += 2;
    }

    while cursor <= last_word {
        let old = words[cursor];
        let carry = words.get(cursor + 1).copied().unwrap_or(0) & 1;
        words[cursor] = (old >> 1) | (carry << 63);
        cursor += 1;
    }

    clear_suffix(words, new_len);
    new_len
}

fn apply_remove_record(state: &mut HeapState, index: u64) -> Result<(), InitError> {
    if index >= state.len_bits {
        return Err(InitError::InvalidLayout);
    }
    let len_bits = state.len_bits;
    state.len_bits = remove_heap_bit(&mut state.words, len_bits, index);
    Ok(())
}

fn apply_record(state: &mut HeapState, record: JournalRecord) -> Result<(), InitError> {
    let (tag, value, payload) = record.unpack()?;
    match tag {
        JournalTag::Empty => {}
        JournalTag::SetLen => {
            let new_len = payload;
            if new_len > crate::JOURNAL_LEN_MAX {
                return Err(InitError::InvalidLayout);
            }
            if new_len < state.len_bits {
                state.len_bits = new_len;
                clear_suffix(&mut state.words, state.len_bits);
            } else {
                state.len_bits = new_len;
            }
        }
        JournalTag::SetBit => {
            if payload > u32::MAX as u64 {
                return Err(InitError::InvalidLayout);
            }
            let index = payload;
            if value {
                if index >= state.len_bits {
                    state.len_bits = index.saturating_add(1);
                }
                let need_words = words_for_bits(state.len_bits);
                if need_words > state.word_cap {
                    return Err(InitError::InvalidLayout);
                }
                set_heap_bit(&mut state.words, index, true);
            } else if index < state.len_bits {
                set_heap_bit(&mut state.words, index, false);
            }
        }
        JournalTag::Remove => {
            if payload > u32::MAX as u64 {
                return Err(InitError::InvalidLayout);
            }
            apply_remove_record(state, payload)?;
        }
    }
    Ok(())
}

fn set_heap_bit(words: &mut [u64], index: u64, value: bool) {
    let (word, bit) = bit_offset(index);
    if let Some(slot) = words.get_mut(word) {
        let mask = 1u64 << bit;
        if value {
            *slot |= mask;
        } else {
            *slot &= !mask;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::vec_mem::VectorMemory;

    fn reopen(memory: VectorMemory) -> Bitset<VectorMemory> {
        Bitset::init(memory).unwrap()
    }

    #[test]
    fn insert_and_remove_roundtrip() {
        let mem = VectorMemory::default();
        let bs = Bitset::new(mem).unwrap();
        bs.insert(0).unwrap();
        bs.insert(1).unwrap();
        bs.insert(2).unwrap();
        bs.insert(3).unwrap();
        assert!(bs.contains(0));
        assert!(bs.contains(1));
        assert!(bs.contains(2));
        assert!(bs.contains(3));
        bs.remove(1).unwrap();
        assert!(bs.contains(0));
        assert!(bs.contains(1));
        assert!(bs.contains(2));
        assert_eq!(bs.len(), 3);
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert!(bs.contains(0));
        assert!(bs.contains(1));
        assert!(bs.contains(2));
        assert_eq!(bs.len(), 3);
    }

    #[test]
    fn remove_shifts_across_word_boundary() {
        let mem = VectorMemory::default();
        let bs = Bitset::new(mem).unwrap();
        for index in 0..80 {
            if matches!(index, 60..=67) {
                bs.insert(index as u32).unwrap();
            }
        }
        bs.remove(63).unwrap();
        assert!(bs.contains(60));
        assert!(bs.contains(61));
        assert!(bs.contains(62));
        assert!(bs.contains(63));
        assert!(bs.contains(64));
        assert!(bs.contains(65));
        assert!(bs.contains(66));
        assert!(!bs.contains(67));
        assert_eq!(bs.len(), 67);
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert!(bs.contains(60));
        assert!(bs.contains(61));
        assert!(bs.contains(62));
        assert!(bs.contains(63));
        assert!(bs.contains(64));
        assert!(bs.contains(65));
        assert!(bs.contains(66));
        assert!(!bs.contains(67));
        assert_eq!(bs.len(), 67);
    }

    #[test]
    fn clear_only_unsets_bit() {
        let mem = VectorMemory::default();
        let bs = Bitset::new(mem).unwrap();
        bs.insert(0).unwrap();
        bs.insert(1).unwrap();
        bs.insert(2).unwrap();
        bs.clear(1).unwrap();
        assert!(bs.contains(0));
        assert!(!bs.contains(1));
        assert!(bs.contains(2));
        assert_eq!(bs.len(), 3);
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert!(bs.contains(0));
        assert!(!bs.contains(1));
        assert!(bs.contains(2));
        assert_eq!(bs.len(), 3);
    }

    #[test]
    fn truncate_replays_after_reopen() {
        let mem = VectorMemory::default();
        let bs = Bitset::new(mem).unwrap();
        bs.insert(1).unwrap();
        bs.insert(70).unwrap();
        bs.truncate(4).unwrap();
        assert!(!bs.contains(70));
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert!(bs.contains(1));
        assert!(!bs.contains(70));
        assert_eq!(bs.len(), 4);
    }

    #[test]
    fn checkpoint_roundtrip_after_full_journal() {
        let mem = VectorMemory::default();
        let bs = Bitset::new(mem).unwrap();
        for i in 0..crate::JOURNAL_CAP_SLOTS {
            bs.insert(i as u32).unwrap();
        }
        bs.insert(crate::JOURNAL_CAP_SLOTS as u32).unwrap();
        assert_eq!(bs.len(), (crate::JOURNAL_CAP_SLOTS + 1) as u64);
        assert!(bs.contains(crate::JOURNAL_CAP_SLOTS as u32));
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert_eq!(bs.len(), (crate::JOURNAL_CAP_SLOTS + 1) as u64);
        assert!(bs.contains(crate::JOURNAL_CAP_SLOTS as u32));
        assert!(bs.contains(0));
    }

    #[test]
    fn ensure_len_replays_after_reopen() {
        let mem = VectorMemory::default();
        let bs = Bitset::new(mem).unwrap();
        bs.ensure_len(4).unwrap();
        assert_eq!(bs.len(), 4);
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert_eq!(bs.len(), 4);
        assert!(!bs.contains(0));
        assert!(!bs.contains(3));
    }

    #[test]
    fn mixed_remove_and_set_replays_after_reopen() {
        let mem = VectorMemory::default();
        let bs = Bitset::new(mem).unwrap();
        bs.insert(0).unwrap();
        bs.insert(1).unwrap();
        bs.insert(3).unwrap();
        bs.clear(1).unwrap();
        bs.ensure_len(5).unwrap();
        bs.insert(4).unwrap();
        bs.remove(0).unwrap();
        bs.truncate(3).unwrap();
        assert_eq!(bs.len(), 3);
        assert!(!bs.contains(0));
        assert!(!bs.contains(1));
        assert!(bs.contains(2));
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert_eq!(bs.len(), 3);
        assert!(!bs.contains(0));
        assert!(!bs.contains(1));
        assert!(bs.contains(2));
    }
}
