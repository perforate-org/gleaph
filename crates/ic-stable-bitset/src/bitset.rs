//! Heap-mirrored stable bitset implementation.
//!
//! The implementation is split into three pieces:
//!
//! - a stable header with layout metadata,
//! - a packed `u64` snapshot of the bitset words,
//! - an append-only journal of packed `u64` records for pending `set` and `truncate` updates.
//!
//! Each packed journal record reserves payload space for `2 ^ 61` distinct values, so the largest
//! bit index or logical length representable by the journal is `2 ^ 61 - 1`
//! (`crate::JOURNAL_PAYLOAD_MAX`).
//!
//! Reads always use the heap mirror so membership checks stay cheap. Writes are durable-first:
//! they append a packed journal record, then update the heap mirror, and finally checkpoint the
//! heap back into stable memory once the journal is full.

use crate::memory::{
    BULK_WORDS, GrowFailed, grow_memory_to_at_least_bytes, read_u64, read_u64_words_into,
    read_u64_words_vec, write, write_u64, write_u64_words_into, write_zero_words,
};
use core::cell::{Cell, RefCell};
use core::fmt;
use ic_stable_structures::Memory;

const MAGIC: [u8; 3] = *b"SBS";
const VERSION: u8 = 1;
const HEADER_SIZE: u64 = 64;
const JOURNAL_RECORD_SIZE: u64 = 8;
const DEFAULT_JOURNAL_CAP: u64 = 4096;
const JOURNAL_KIND_SHIFT: u32 = 62;
const JOURNAL_VALUE_SHIFT: u32 = 61;
const JOURNAL_REPLAY_CHUNK_WORDS: usize = 128;

const MAGIC_OFFSET: u64 = 0;
const VERSION_OFFSET: u64 = 3;
const LEN_OFFSET: u64 = 4;
const WORD_CAP_OFFSET: u64 = 12;
const JOURNAL_CAP_OFFSET: u64 = 20;

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
                "unsupported layout version {version}; supported version numbers are 1..={VERSION}"
            ),
            Self::InvalidLayout => write!(f, "invalid stable bitset layout"),
            Self::OutOfMemory => write!(f, "failed to allocate memory for stable bitset"),
        }
    }
}

impl std::error::Error for InitError {}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JournalTag {
    Empty = 0,
    SetLen = 1,
    SetBit = 2,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JournalRecord(u64);

impl JournalRecord {
    fn set_len(len: u64) -> Self {
        assert!(
            len <= crate::JOURNAL_PAYLOAD_MAX,
            "bitset length exceeds supported 2 ^ 61 - 1 payload"
        );
        Self::pack(JournalTag::SetLen, false, len)
    }

    fn set_bit(index: u64, value: bool) -> Self {
        assert!(
            index <= crate::JOURNAL_PAYLOAD_MAX,
            "bit index exceeds supported 2 ^ 61 - 1 payload"
        );
        Self::pack(JournalTag::SetBit, value, index)
    }

    fn pack(tag: JournalTag, value: bool, payload: u64) -> Self {
        debug_assert!(payload <= crate::JOURNAL_PAYLOAD_MAX);
        let raw = ((tag as u64) << JOURNAL_KIND_SHIFT)
            | (u64::from(value) << JOURNAL_VALUE_SHIFT)
            | (payload & crate::JOURNAL_PAYLOAD_MAX);
        Self(raw)
    }

    fn unpack(self) -> Result<(JournalTag, bool, u64), InitError> {
        let raw_tag = (self.0 >> JOURNAL_KIND_SHIFT) & 0b11;
        let tag = match raw_tag {
            0 => JournalTag::Empty,
            1 => JournalTag::SetLen,
            2 => JournalTag::SetBit,
            _ => return Err(InitError::InvalidLayout),
        };
        let value = ((self.0 >> JOURNAL_VALUE_SHIFT) & 1) != 0;
        let payload = self.0 & crate::JOURNAL_PAYLOAD_MAX;
        Ok((tag, value, payload))
    }

    fn to_bytes(self) -> [u8; JOURNAL_RECORD_SIZE as usize] {
        self.0.to_le_bytes()
    }

    fn from_bytes(bytes: &[u8]) -> Self {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes);
        Self(u64::from_le_bytes(raw))
    }
}

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

/// Returns the number of `u64` words needed to store `bits` bits.
fn words_for_bits(bits: u64) -> u64 {
    if bits == 0 { 0 } else { (bits - 1) / 64 + 1 }
}

/// Start offset of the journal region in stable memory.
fn journal_offset() -> u64 {
    HEADER_SIZE
}

/// Start offset of the packed bitset words in stable memory.
fn data_offset(journal_cap: u64) -> u64 {
    HEADER_SIZE + journal_cap.saturating_mul(JOURNAL_RECORD_SIZE)
}

/// Returns a mask for the selected bit position.
fn word_mask(bit: u64) -> u64 {
    1u64 << bit
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
    let journal_cap = read_u64(memory, JOURNAL_CAP_OFFSET);
    (magic, version[0], len_bits, word_cap, journal_cap)
}

/// Writes the stable header fields.
fn write_header<M: Memory>(
    memory: &M,
    len_bits: u64,
    word_cap: u64,
    journal_cap: u64,
) -> Result<(), GrowFailed> {
    write(memory, MAGIC_OFFSET, &MAGIC);
    write(memory, VERSION_OFFSET, &[VERSION]);
    write_u64(memory, LEN_OFFSET, len_bits);
    write_u64(memory, WORD_CAP_OFFSET, word_cap);
    write_u64(memory, JOURNAL_CAP_OFFSET, journal_cap);
    Ok(())
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
/// - `set` and `truncate` are durable-first and may trigger checkpointing.
pub struct BitSet<M: Memory> {
    memory: M,
    state: RefCell<HeapState>,
    journal_len: Cell<u64>,
    journal_cap: u64,
}

/// Stable-memory bitset backed by a heap mirror and journal.
pub type StableBitSet<M> = BitSet<M>;

impl<M: Memory> fmt::Debug for BitSet<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = self.state.borrow();
        f.debug_struct("BitSet")
            .field("len_bits", &st.len_bits)
            .field("word_cap", &st.word_cap)
            .field("journal_len", &self.journal_len.get())
            .field("journal_cap", &self.journal_cap)
            .finish()
    }
}

impl<M: Memory> BitSet<M> {
    /// Creates a new empty bitset using the provided stable memory.
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        Self::new_with_journal_capacity(memory, DEFAULT_JOURNAL_CAP)
    }

    /// Creates a new empty bitset with an explicit journal capacity.
    pub(crate) fn new_with_journal_capacity(
        memory: M,
        journal_cap: u64,
    ) -> Result<Self, GrowFailed> {
        let journal_cap = journal_cap.max(1);
        write_header(&memory, 0, 0, journal_cap)?;
        grow_memory_to_at_least_bytes(&memory, data_offset(journal_cap))?;
        Ok(Self {
            memory,
            state: RefCell::new(HeapState::new()),
            journal_len: Cell::new(0),
            journal_cap,
        })
    }

    /// Reopens a bitset from stable memory and replays any pending journal records.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let (magic, version, len_bits, word_cap, journal_cap) = read_header(&memory);
        if magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: magic,
                expected: MAGIC,
            });
        }
        if version != VERSION {
            return Err(InitError::IncompatibleVersion(version));
        }
        if journal_cap == 0 {
            return Err(InitError::InvalidLayout);
        }
        let need = data_offset(journal_cap).saturating_add(word_cap.saturating_mul(8));
        let size_bytes = memory
            .size()
            .checked_mul(crate::memory::WASM_PAGE_SIZE)
            .expect("address overflow");
        if size_bytes < need {
            return Err(InitError::InvalidLayout);
        }
        let words = read_u64_words_vec(&memory, data_offset(journal_cap), word_cap);
        let mut state = HeapState {
            len_bits,
            word_cap,
            words,
        };
        clear_suffix(&mut state.words, state.len_bits);

        let mut journal_len = 0u64;
        let mut remaining = journal_cap as usize;
        let mut offset = journal_offset();
        let mut journal_buf = [0u64; JOURNAL_REPLAY_CHUNK_WORDS];
        let mut journal_scratch = vec![0u8; JOURNAL_REPLAY_CHUNK_WORDS * 8];
        while remaining > 0 {
            let take = remaining.min(JOURNAL_REPLAY_CHUNK_WORDS);
            read_u64_words_into(
                &memory,
                offset,
                &mut journal_buf[..take],
                &mut journal_scratch,
            );
            for raw in &journal_buf[..take] {
                let raw = *raw;
                if raw == 0 {
                    remaining = 0;
                    break;
                }
                let rec = JournalRecord::from_bytes(&raw.to_le_bytes());
                apply_record(&mut state, rec)?;
                journal_len += 1;
            }
            if remaining == 0 {
                break;
            }
            offset += (take as u64) * 8;
            remaining -= take;
        }
        Ok(Self {
            memory,
            state: RefCell::new(state),
            journal_len: Cell::new(journal_len),
            journal_cap,
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
    pub fn contains(&self, index: u64) -> bool {
        let st = self.state.borrow();
        if index >= st.len_bits {
            return false;
        }
        let word = (index >> 6) as usize;
        let bit_mask = 1u64 << (index & 63);
        unsafe { (*st.words.get_unchecked(word) & bit_mask) != 0 }
    }

    /// Ensures that the logical length is at least `min_len`.
    pub fn ensure_len(&self, min_len: u64) -> Result<(), GrowFailed> {
        assert!(
            min_len <= crate::JOURNAL_PAYLOAD_MAX,
            "bitset length exceeds supported 2 ^ 61 - 1 payload"
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
    pub fn set(&self, index: u64, value: bool) -> Result<(), GrowFailed> {
        assert!(
            index <= crate::JOURNAL_PAYLOAD_MAX,
            "bit index exceeds supported 2 ^ 61 - 1 payload"
        );
        if value {
            let need_len = index.saturating_add(1);
            if need_len > self.len() {
                self.ensure_word_capacity(words_for_bits(need_len))?;
            }
            if !self.contains(index) {
                self.append_record(JournalRecord::set_bit(index, true))?;
                {
                    let mut st = self.state.borrow_mut();
                    if index >= st.len_bits {
                        st.len_bits = need_len;
                    }
                    set_heap_bit(&mut st.words, index, true);
                }
                self.maybe_checkpoint()?;
            }
            return Ok(());
        }
        if !self.contains(index) {
            return Ok(());
        }
        self.append_record(JournalRecord::set_bit(index, false))?;
        {
            let mut st = self.state.borrow_mut();
            if index < st.len_bits {
                set_heap_bit(&mut st.words, index, false);
            }
        }
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Inserts a bit by setting it to `true`.
    pub fn insert(&self, index: u64) -> Result<(), GrowFailed> {
        self.set(index, true)
    }

    /// Removes a bit by setting it to `false`.
    pub fn remove(&self, index: u64) -> Result<(), GrowFailed> {
        self.set(index, false)
    }

    /// Shrinks the logical length to `new_len`.
    pub fn truncate(&self, new_len: u64) -> Result<(), GrowFailed> {
        assert!(
            new_len <= crate::JOURNAL_PAYLOAD_MAX,
            "bitset length exceeds supported 2 ^ 61 - 1 payload"
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
        let need_bytes = data_offset(self.journal_cap) + new_cap.saturating_mul(8);
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

    /// Appends a record to the journal.
    fn append_record(&self, record: JournalRecord) -> Result<(), GrowFailed> {
        if self.journal_len.get() >= self.journal_cap {
            self.checkpoint()?;
        }
        let idx = self.journal_len.get();
        let base = journal_offset() + idx * JOURNAL_RECORD_SIZE;
        write_u64(&self.memory, base, u64::from_le_bytes(record.to_bytes()));
        self.journal_len.set(idx + 1);
        Ok(())
    }

    /// Checkpoints the heap mirror when the journal reaches capacity.
    fn maybe_checkpoint(&self) -> Result<(), GrowFailed> {
        if self.journal_len.get() >= self.journal_cap {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Writes the heap mirror back into stable memory and clears the journal.
    fn checkpoint(&self) -> Result<(), GrowFailed> {
        let st = self.state.borrow();
        let word_offset = data_offset(self.journal_cap);
        let mut scratch = vec![0u8; BULK_WORDS * 8];
        write_u64_words_into(&self.memory, word_offset, &st.words, &mut scratch);
        write_u64(&self.memory, LEN_OFFSET, st.len_bits);
        write_u64(&self.memory, WORD_CAP_OFFSET, st.word_cap);
        write_zero_words(&self.memory, journal_offset(), self.journal_len.get());
        self.journal_len.set(0);
        Ok(())
    }
}

fn set_heap_bit(words: &mut [u64], index: u64, value: bool) {
    let word = (index >> 6) as usize;
    let bit = index & 63;
    if let Some(slot) = words.get_mut(word) {
        let mask = word_mask(bit);
        if value {
            *slot |= mask;
        } else {
            *slot &= !mask;
        }
    }
}

fn apply_record(state: &mut HeapState, record: JournalRecord) -> Result<(), InitError> {
    let (tag, value, payload) = record.unpack()?;
    match tag {
        JournalTag::SetLen => {
            let new_len = payload;
            if new_len < state.len_bits {
                state.len_bits = new_len;
                clear_suffix(&mut state.words, state.len_bits);
            } else {
                state.len_bits = new_len;
            }
        }
        JournalTag::SetBit => {
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
        JournalTag::Empty => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::vec_mem::VectorMemory;

    fn reopen(memory: VectorMemory) -> BitSet<VectorMemory> {
        BitSet::init(memory).unwrap()
    }

    #[test]
    fn insert_and_remove_roundtrip() {
        let mem = VectorMemory::default();
        let bs = BitSet::new_with_journal_capacity(mem, 8).unwrap();
        bs.insert(3).unwrap();
        bs.insert(65).unwrap();
        assert!(bs.contains(3));
        assert!(bs.contains(65));
        bs.remove(3).unwrap();
        assert!(!bs.contains(3));
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert!(!bs.contains(3));
        assert!(bs.contains(65));
    }

    #[test]
    fn truncate_replays_after_reopen() {
        let mem = VectorMemory::default();
        let bs = BitSet::new_with_journal_capacity(mem, 8).unwrap();
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
    fn checkpoint_clears_journal() {
        let mem = VectorMemory::default();
        let bs = BitSet::new_with_journal_capacity(mem, 2).unwrap();
        bs.insert(0).unwrap();
        bs.insert(1).unwrap();
        bs.insert(2).unwrap();
        assert!(bs.contains(2));
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert!(bs.contains(0));
        assert!(bs.contains(1));
        assert!(bs.contains(2));
    }

    #[test]
    fn packed_journal_uses_61_bit_payload() {
        let rec = JournalRecord::set_bit(crate::JOURNAL_PAYLOAD_MAX, true);
        let roundtrip = JournalRecord::from_bytes(&rec.to_bytes());
        let (tag, value, payload) = roundtrip.unpack().unwrap();
        assert_eq!(tag, JournalTag::SetBit);
        assert!(value);
        assert_eq!(payload, crate::JOURNAL_PAYLOAD_MAX);
    }
}
