use crate::memory::{
    MemoryReader, MemoryWriter, grow_memory_to_at_least_bytes, read_u64, read_u64_words_into,
    safe_write, write_u64, write_zero_bytes,
};
use core::cell::{Cell, RefCell};
use core::fmt;
use ic_stable_structures::Memory;
use roaring::RoaringTreemap;

const MAGIC: [u8; 3] = *b"RSB";
const VERSION: u8 = 1;
const HEADER_SIZE: u64 = 64;
const JOURNAL_RECORD_SIZE: u64 = 8;
const DEFAULT_JOURNAL_CAP: u64 = 4096;
const JOURNAL_REPLAY_CHUNK_WORDS: usize = 128;
const JOURNAL_KIND_SHIFT: u32 = 62;
const JOURNAL_VALUE_SHIFT: u32 = 61;

const MAGIC_OFFSET: u64 = 0;
const VERSION_OFFSET: u64 = 3;
const LEN_OFFSET: u64 = 4;
const JOURNAL_CAP_OFFSET: u64 = 12;
const SNAPSHOT_LEN_OFFSET: u64 = 20;

#[derive(Clone, Debug)]
struct HeapState {
    len_bits: u64,
    bitmap: RoaringTreemap,
}

impl HeapState {
    fn new() -> Self {
        Self {
            len_bits: 0,
            bitmap: RoaringTreemap::new(),
        }
    }
}

/// Error returned when a stable roaring bitmap cannot be opened from memory.
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
            Self::InvalidLayout => write!(f, "invalid stable roaring bitmap layout"),
            Self::OutOfMemory => write!(f, "failed to allocate memory for stable roaring bitmap"),
        }
    }
}

impl std::error::Error for InitError {}

/// Start offset of the journal region in stable memory.
fn journal_offset() -> u64 {
    HEADER_SIZE
}

/// Start offset of the serialized roaring snapshot in stable memory.
fn data_offset(journal_cap: u64) -> u64 {
    HEADER_SIZE + journal_cap.saturating_mul(JOURNAL_RECORD_SIZE)
}

fn read_header<M: Memory>(memory: &M) -> ([u8; 3], u8, u64, u64, u64) {
    let mut magic = [0u8; 3];
    let mut version = [0u8; 1];
    memory.read(MAGIC_OFFSET, &mut magic);
    memory.read(VERSION_OFFSET, &mut version);
    let len_bits = read_u64(memory, LEN_OFFSET);
    let journal_cap = read_u64(memory, JOURNAL_CAP_OFFSET);
    let snapshot_len_bytes = read_u64(memory, SNAPSHOT_LEN_OFFSET);
    (magic, version[0], len_bits, journal_cap, snapshot_len_bytes)
}

fn write_header<M: Memory>(
    memory: &M,
    len_bits: u64,
    journal_cap: u64,
    snapshot_len_bytes: u64,
) -> Result<(), crate::GrowFailed> {
    safe_write(memory, MAGIC_OFFSET, &MAGIC)?;
    safe_write(memory, VERSION_OFFSET, &[VERSION])?;
    write_u64(memory, LEN_OFFSET, len_bits)?;
    write_u64(memory, JOURNAL_CAP_OFFSET, journal_cap)?;
    write_u64(memory, SNAPSHOT_LEN_OFFSET, snapshot_len_bytes)?;
    Ok(())
}

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
            "bitmap length exceeds supported 2 ^ 61 - 1 payload"
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
            0 if self.0 == 0 => JournalTag::Empty,
            1 => JournalTag::SetLen,
            2 => JournalTag::SetBit,
            _ => return Err(InitError::InvalidLayout),
        };
        let value = ((self.0 >> JOURNAL_VALUE_SHIFT) & 1) != 0;
        let payload = self.0 & crate::JOURNAL_PAYLOAD_MAX;
        Ok((tag, value, payload))
    }
}

/// Stable roaring bitmap with a heap mirror and a durable journal.
///
/// # Storage model
///
/// The type keeps the read path in a heap-backed `RoaringTreemap` and mirrors updates into stable
/// memory via a compact journal. Once the journal is full, the current heap state is checkpointed
/// into the stable snapshot and the journal is cleared.
///
/// # Operational notes
///
/// - The type is intended for single-writer use.
/// - `contains` always reads the heap mirror.
/// - `set`, `clear`, `ensure_len`, and `truncate` are durable-first and may trigger checkpointing.
pub struct RoaringBitMap<M: Memory> {
    memory: M,
    state: RefCell<HeapState>,
    journal_len: Cell<u64>,
    journal_cap: u64,
}

impl<M: Memory> fmt::Debug for RoaringBitMap<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = self.state.borrow();
        f.debug_struct("RoaringBitMap")
            .field("len_bits", &st.len_bits)
            .field("cardinality", &st.bitmap.len())
            .field("journal_len", &self.journal_len.get())
            .field("journal_cap", &self.journal_cap)
            .finish()
    }
}

impl<M: Memory> RoaringBitMap<M> {
    /// Creates a new empty bitmap using the provided stable memory.
    pub fn new(memory: M) -> Result<Self, crate::GrowFailed> {
        Self::new_with_journal_capacity(memory, DEFAULT_JOURNAL_CAP)
    }

    /// Creates a new empty bitmap with an explicit journal capacity.
    pub(crate) fn new_with_journal_capacity(
        memory: M,
        journal_cap: u64,
    ) -> Result<Self, crate::GrowFailed> {
        let journal_cap = journal_cap.max(1);
        write_header(&memory, 0, journal_cap, 0)?;
        grow_memory_to_at_least_bytes(&memory, data_offset(journal_cap))?;
        Ok(Self {
            memory,
            state: RefCell::new(HeapState::new()),
            journal_len: Cell::new(0),
            journal_cap,
        })
    }

    /// Reopens a bitmap from stable memory and replays any pending mutation records.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let (magic, version, len_bits, journal_cap, snapshot_len_bytes) = read_header(&memory);
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
        let need = data_offset(journal_cap)
            .checked_add(snapshot_len_bytes)
            .ok_or(InitError::InvalidLayout)?;
        let size_bytes = memory
            .size()
            .checked_mul(crate::memory::WASM_PAGE_SIZE)
            .expect("address overflow");
        if size_bytes < need {
            return Err(InitError::InvalidLayout);
        }

        let bitmap = if snapshot_len_bytes == 0 {
            RoaringTreemap::new()
        } else {
            let reader = MemoryReader::new(&memory, data_offset(journal_cap), snapshot_len_bytes);
            RoaringTreemap::deserialize_from(reader).map_err(|_| InitError::InvalidLayout)?
        };
        let mut state = HeapState { len_bits, bitmap };

        let mut journal_len = 0u64;
        let mut remaining = journal_cap as usize;
        let mut offset = journal_offset();
        let mut journal_buf = [0u64; JOURNAL_REPLAY_CHUNK_WORDS];
        while remaining > 0 {
            let take = remaining.min(JOURNAL_REPLAY_CHUNK_WORDS);
            read_u64_words_into(&memory, offset, &mut journal_buf[..take]);
            for raw in &journal_buf[..take] {
                if *raw == 0 {
                    remaining = 0;
                    break;
                }
                let rec = JournalRecord(*raw);
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

    /// Returns `true` when the logical length is zero.
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
        st.bitmap.contains(index)
    }

    /// Ensures that the logical length is at least `min_len`.
    pub fn ensure_len(&self, min_len: u64) -> Result<(), crate::GrowFailed> {
        assert!(
            min_len <= crate::JOURNAL_PAYLOAD_MAX,
            "bitmap length exceeds supported 2 ^ 61 - 1 payload"
        );
        let current = self.len();
        if min_len <= current {
            return Ok(());
        }
        self.append_record(JournalRecord::set_len(min_len))?;
        {
            let mut st = self.state.borrow_mut();
            st.len_bits = min_len;
        }
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Sets or clears a bit.
    pub fn set(&self, index: u64, value: bool) -> Result<(), crate::GrowFailed> {
        assert!(
            index <= crate::JOURNAL_PAYLOAD_MAX,
            "bit index exceeds supported 2 ^ 61 - 1 payload"
        );
        if value {
            let need_len = index.saturating_add(1);
            if !self.contains(index) {
                self.append_record(JournalRecord::set_bit(index, true))?;
                {
                    let mut st = self.state.borrow_mut();
                    if need_len > st.len_bits {
                        st.len_bits = need_len;
                    }
                    st.bitmap.insert(index);
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
            st.bitmap.remove(index);
        }
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Inserts a bit by setting it to `true`.
    pub fn insert(&self, index: u64) -> Result<(), crate::GrowFailed> {
        self.set(index, true)
    }

    /// Clears a bit by setting it to `false`.
    pub fn clear(&self, index: u64) -> Result<(), crate::GrowFailed> {
        self.set(index, false)
    }

    /// Shrinks the logical length to `new_len`.
    pub fn truncate(&self, new_len: u64) -> Result<(), crate::GrowFailed> {
        assert!(
            new_len <= crate::JOURNAL_PAYLOAD_MAX,
            "bitmap length exceeds supported 2 ^ 61 - 1 payload"
        );
        if new_len >= self.len() {
            return Ok(());
        }
        self.append_record(JournalRecord::set_len(new_len))?;
        {
            let mut st = self.state.borrow_mut();
            st.len_bits = new_len;
            st.bitmap.remove_range(new_len..=u64::MAX);
        }
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Appends a packed mutation record to the journal.
    fn append_record(&self, record: JournalRecord) -> Result<(), crate::GrowFailed> {
        if self.journal_len.get() >= self.journal_cap {
            self.checkpoint()?;
        }
        let idx = self.journal_len.get();
        let base = journal_offset() + idx * JOURNAL_RECORD_SIZE;
        write_u64(&self.memory, base, record.0)?;
        self.journal_len.set(idx + 1);
        Ok(())
    }

    /// Checkpoints the heap mirror when the journal reaches capacity.
    fn maybe_checkpoint(&self) -> Result<(), crate::GrowFailed> {
        if self.journal_len.get() >= self.journal_cap {
            self.checkpoint()?;
        }
        Ok(())
    }

    /// Writes the heap mirror back into stable memory and clears the journal.
    fn checkpoint(&self) -> Result<(), crate::GrowFailed> {
        let (len_bits, snapshot_len_bytes) = {
            let st = self.state.borrow();
            (st.len_bits, st.bitmap.serialized_size() as u64)
        };
        let need_bytes = data_offset(self.journal_cap)
            .checked_add(snapshot_len_bytes)
            .expect("address overflow");
        grow_memory_to_at_least_bytes(&self.memory, need_bytes)?;

        {
            let st = self.state.borrow();
            let mut writer = MemoryWriter::new(&self.memory, data_offset(self.journal_cap));
            st.bitmap
                .serialize_into(&mut writer)
                .expect("serialize roaring snapshot");
        }

        write_header(&self.memory, len_bits, self.journal_cap, snapshot_len_bytes)?;
        write_zero_bytes(
            &self.memory,
            journal_offset(),
            self.journal_len.get() * JOURNAL_RECORD_SIZE,
        )?;
        self.journal_len.set(0);
        Ok(())
    }
}

fn apply_record(state: &mut HeapState, record: JournalRecord) -> Result<(), InitError> {
    let (tag, value, payload) = record.unpack()?;
    match tag {
        JournalTag::Empty => return Err(InitError::InvalidLayout),
        JournalTag::SetLen => {
            let new_len = payload;
            if new_len < state.len_bits {
                state.len_bits = new_len;
                state.bitmap.remove_range(new_len..=u64::MAX);
            } else {
                state.len_bits = new_len;
            }
        }
        JournalTag::SetBit => {
            if value {
                let need_len = payload.saturating_add(1);
                if need_len > state.len_bits {
                    state.len_bits = need_len;
                }
                state.bitmap.insert(payload);
            } else {
                state.bitmap.remove(payload);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::{Memory, vec_mem::VectorMemory};

    fn reopen(memory: VectorMemory) -> RoaringBitMap<VectorMemory> {
        RoaringBitMap::init(memory).unwrap()
    }

    #[test]
    fn fresh_create_and_reopen_roundtrip() {
        let mem = VectorMemory::default();
        let bs = RoaringBitMap::new_with_journal_capacity(mem, 8).unwrap();
        assert_eq!(bs.len(), 0);
        assert!(bs.is_empty());
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert_eq!(bs.len(), 0);
        assert!(bs.is_empty());
    }

    #[test]
    fn insert_clear_contains_roundtrip() {
        let mem = VectorMemory::default();
        let bs = RoaringBitMap::new_with_journal_capacity(mem, 8).unwrap();
        bs.insert(0).unwrap();
        bs.insert(3).unwrap();
        bs.insert(10).unwrap();
        bs.clear(3).unwrap();
        assert!(bs.contains(0));
        assert!(!bs.contains(3));
        assert!(bs.contains(10));
        assert_eq!(bs.len(), 11);
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert!(bs.contains(0));
        assert!(!bs.contains(3));
        assert!(bs.contains(10));
        assert_eq!(bs.len(), 11);
    }

    #[test]
    fn ensure_len_preserves_zero_suffix_across_reopen() {
        let mem = VectorMemory::default();
        let bs = RoaringBitMap::new_with_journal_capacity(mem, 8).unwrap();
        bs.ensure_len(16).unwrap();
        assert_eq!(bs.len(), 16);
        assert!(!bs.contains(0));
        assert!(!bs.contains(15));
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert_eq!(bs.len(), 16);
        assert!(!bs.contains(0));
        assert!(!bs.contains(15));
    }

    #[test]
    fn truncate_clears_suffix_across_reopen() {
        let mem = VectorMemory::default();
        let bs = RoaringBitMap::new_with_journal_capacity(mem, 8).unwrap();
        bs.insert(1).unwrap();
        bs.insert(70).unwrap();
        bs.insert(130).unwrap();
        bs.truncate(64).unwrap();
        assert_eq!(bs.len(), 64);
        assert!(bs.contains(1));
        assert!(!bs.contains(70));
        assert!(!bs.contains(130));
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert_eq!(bs.len(), 64);
        assert!(bs.contains(1));
        assert!(!bs.contains(70));
        assert!(!bs.contains(130));
    }

    #[test]
    fn checkpoint_after_full_journal_preserves_state() {
        let mem = VectorMemory::default();
        let bs = RoaringBitMap::new_with_journal_capacity(mem, 2).unwrap();
        bs.insert(0).unwrap();
        bs.insert(1).unwrap();
        bs.clear(1).unwrap();
        bs.insert(2).unwrap();
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
    fn mixed_operations_replay_deterministically() {
        let mem = VectorMemory::default();
        let bs = RoaringBitMap::new_with_journal_capacity(mem, 3).unwrap();
        bs.insert(0).unwrap();
        bs.insert(9).unwrap();
        bs.clear(0).unwrap();
        bs.ensure_len(20).unwrap();
        bs.insert(19).unwrap();
        bs.truncate(12).unwrap();
        assert_eq!(bs.len(), 12);
        assert!(!bs.contains(0));
        assert!(bs.contains(9));
        assert!(!bs.contains(19));
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert_eq!(bs.len(), 12);
        assert!(!bs.contains(0));
        assert!(bs.contains(9));
        assert!(!bs.contains(19));
    }

    #[test]
    fn sparse_high_index_inserts_roundtrip() {
        let mem = VectorMemory::default();
        let bs = RoaringBitMap::new_with_journal_capacity(mem, 8).unwrap();
        let a = 1u64 << 40;
        let b = (1u64 << 48) + 123;
        bs.insert(a).unwrap();
        bs.insert(b).unwrap();
        assert!(bs.contains(a));
        assert!(bs.contains(b));
        assert_eq!(bs.len(), b + 1);
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert!(bs.contains(a));
        assert!(bs.contains(b));
        assert_eq!(bs.len(), b + 1);
    }

    #[test]
    fn invalid_magic_version_layout_and_snapshot_are_rejected() {
        let mem = VectorMemory::default();
        let bs = RoaringBitMap::new_with_journal_capacity(mem, 1).unwrap();
        bs.insert(1).unwrap();
        let mem = bs.into_memory();
        mem.write(0, b"BAD");
        assert!(matches!(RoaringBitMap::init(mem), Err(InitError::BadMagic { .. })));

        let bs = RoaringBitMap::new_with_journal_capacity(VectorMemory::default(), 1).unwrap();
        let mem = bs.into_memory();
        mem.write(VERSION_OFFSET, &[VERSION.wrapping_add(1)]);
        assert!(matches!(
            RoaringBitMap::init(mem),
            Err(InitError::IncompatibleVersion(_))
        ));

        let bs = RoaringBitMap::new_with_journal_capacity(VectorMemory::default(), 1).unwrap();
        let mem = bs.into_memory();
        mem.write(JOURNAL_CAP_OFFSET, &0u64.to_le_bytes());
        assert!(matches!(RoaringBitMap::init(mem), Err(InitError::InvalidLayout)));

        let bs = RoaringBitMap::new_with_journal_capacity(VectorMemory::default(), 1).unwrap();
        bs.insert(1).unwrap();
        let mem = bs.into_memory();
        let snapshot_offset = data_offset(1);
        mem.write(snapshot_offset, &[0xff]);
        assert!(matches!(RoaringBitMap::init(mem), Err(InitError::InvalidLayout)));
    }
}
