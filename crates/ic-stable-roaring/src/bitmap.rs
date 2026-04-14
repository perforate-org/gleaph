use crate::memory::{
    MemoryReader, MemoryWriter, grow_memory_to_at_least_bytes, read_5_bytes, read_u64,
    safe_write, write_5_bytes, write_u64, write_zero_bytes,
};
use core::cell::{Cell, Ref, RefCell};
use core::fmt;
use ic_stable_structures::Memory;
use roaring::RoaringBitmap as RoaringHeap;

const MAGIC: [u8; 3] = *b"RSB";
const VERSION: u8 = 1;
const HEADER_SIZE: u64 = 64;
const JOURNAL_RECORD_SIZE: u64 = 5;

const MAGIC_OFFSET: u64 = 0;
const VERSION_OFFSET: u64 = 3;
const LEN_OFFSET: u64 = 4;
/// Header field: must equal [`crate::JOURNAL_CAP_SLOTS`] as `u64` (fixed journal size on disk).
const JOURNAL_SLOTS_METADATA_OFFSET: u64 = 12;
const SNAPSHOT_LEN_OFFSET: u64 = 20;

#[derive(Clone, Debug)]
struct HeapState {
    len_bits: u64,
    bitmap: RoaringHeap,
}

impl HeapState {
    fn new() -> Self {
        Self {
            len_bits: 0,
            bitmap: RoaringHeap::new(),
        }
    }
}

/// Clears all set bits with index `>= start_exclusive` (indices are `u32`).
fn remove_suffix_bits(bitmap: &mut RoaringHeap, start_exclusive: u64) {
    if start_exclusive > u32::MAX as u64 {
        return;
    }
    bitmap.remove_range(start_exclusive as u32..=u32::MAX);
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

/// Byte offset just past the last journal slot: `64 + JOURNAL_CAP_SLOTS * 5` (not necessarily 8-aligned).
fn journal_end_bytes() -> u64 {
    journal_offset().saturating_add((crate::JOURNAL_CAP_SLOTS as u64).saturating_mul(JOURNAL_RECORD_SIZE))
}

/// Start of the serialized Roaring snapshot; always 8-byte aligned after zero padding.
fn snapshot_base() -> u64 {
    let end = journal_end_bytes();
    (end + 7) & !7
}

fn read_header<M: Memory>(memory: &M) -> ([u8; 3], u8, u64, u64, u64) {
    let mut magic = [0u8; 3];
    let mut version = [0u8; 1];
    memory.read(MAGIC_OFFSET, &mut magic);
    memory.read(VERSION_OFFSET, &mut version);
    let len_bits = read_u64(memory, LEN_OFFSET);
    let journal_slots = read_u64(memory, JOURNAL_SLOTS_METADATA_OFFSET);
    let snapshot_len_bytes = read_u64(memory, SNAPSHOT_LEN_OFFSET);
    (magic, version[0], len_bits, snapshot_len_bytes, journal_slots)
}

fn write_header<M: Memory>(
    memory: &M,
    len_bits: u64,
    snapshot_len_bytes: u64,
) -> Result<(), crate::GrowFailed> {
    safe_write(memory, MAGIC_OFFSET, &MAGIC)?;
    safe_write(memory, VERSION_OFFSET, &[VERSION])?;
    write_u64(memory, LEN_OFFSET, len_bits)?;
    write_u64(
        memory,
        JOURNAL_SLOTS_METADATA_OFFSET,
        crate::JOURNAL_CAP_SLOTS as u64,
    )?;
    write_u64(memory, SNAPSHOT_LEN_OFFSET, snapshot_len_bytes)?;
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum JournalTag {
    Empty = 0,
    SetLen = 1,
    SetBit = 2,
}

/// One journal record is **5 bytes** (40 bits, little-endian). Layout (LSB → MSB within the 40 bits):
///
/// | bits     | field        | meaning |
/// |----------|--------------|---------|
/// | 0..32    | `payload_lo` | `SetLen`: low 32 bits of `len_bits`. `SetBit`: bit index. |
/// | 32       | `len_hi`     | `SetLen`: MSB of the 33-bit length. `SetBit`: must be 0. |
/// | 33..37   | `reserved`   | must be 0 |
/// | 37       | `value`      | `SetBit`: set vs clear |
/// | 38..40   | `tag`        | 1 = SetLen, 2 = SetBit |
///
/// `SetLen` length: `len_bits = ((len_hi as u64) << 32) | (payload_lo as u64)` (33 contiguous bits).
///
/// Replay ends at the first record whose **five bytes are all zero** (unused tail).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct JournalRecord([u8; 5]);

impl JournalRecord {
    fn set_len(len: u64) -> Self {
        assert!(
            len <= crate::JOURNAL_LEN_MAX,
            "bitmap length exceeds supported u32 index space"
        );
        let payload_lo = len as u32;
        let len_hi = ((len >> 32) & 1) as u32;
        Self::pack_fields(JournalTag::SetLen, false, payload_lo, len_hi)
    }

    fn set_bit(index: u32, value: bool) -> Self {
        Self::pack_fields(JournalTag::SetBit, value, index, 0)
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
            1 => JournalTag::SetLen,
            2 => JournalTag::SetBit,
            _ => return Err(InitError::InvalidLayout),
        };
        let value = ((raw >> 37) & 1) != 0;
        let len_hi = (raw >> 32) & 1;
        let payload_lo = raw as u32;
        let payload = match tag {
            JournalTag::SetLen => (len_hi << 32) | (payload_lo as u64),
            JournalTag::SetBit => {
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

/// Stable roaring bitmap with a heap mirror and a durable journal.
///
/// # Storage model
///
/// The type keeps the read path in a heap-backed [`RoaringHeap`] and mirrors updates into stable
/// memory via a **5-byte-per-slot** journal; the serialized snapshot begins at an 8-byte aligned
/// offset after the journal and optional zero padding. Once the journal is full, the current heap state is
/// checkpointed into the stable snapshot and the journal is cleared.
///
/// # Operational notes
///
/// - The type is intended for single-writer use.
/// - `contains` always reads the heap mirror.
/// - `set`, `clear`, `ensure_len`, and `truncate` are durable-first and may trigger checkpointing.
pub struct RoaringBitmap<M: Memory> {
    memory: M,
    state: RefCell<HeapState>,
    journal_len: Cell<u64>,
}

/// Borrowed view for repeated membership checks against a [`RoaringBitmap`].
pub struct ContainsView<'a> {
    state: Ref<'a, HeapState>,
}

impl ContainsView<'_> {
    /// Tests whether the selected bit is set while reusing a previously borrowed heap mirror.
    #[inline]
    pub fn contains(&self, index: u32) -> bool {
        if u64::from(index) >= self.state.len_bits {
            return false;
        }
        self.state.bitmap.contains(index)
    }
}

impl<M: Memory> fmt::Debug for RoaringBitmap<M> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let st = self.state.borrow();
        f.debug_struct("RoaringBitmap")
            .field("len_bits", &st.len_bits)
            .field("cardinality", &st.bitmap.len())
            .field("journal_len", &self.journal_len.get())
            .field("journal_cap_slots", &crate::JOURNAL_CAP_SLOTS)
            .finish()
    }
}

impl<M: Memory> RoaringBitmap<M> {
    /// Creates a new empty bitmap using the provided stable memory.
    pub fn new(memory: M) -> Result<Self, crate::GrowFailed> {
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

    /// Reopens a bitmap from stable memory and replays any pending mutation records.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let (magic, version, len_bits, snapshot_len_bytes, journal_slots) = read_header(&memory);
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
        let need = snapshot_base()
            .checked_add(snapshot_len_bytes)
            .ok_or(InitError::InvalidLayout)?;
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

        let bitmap = if snapshot_len_bytes == 0 {
            RoaringHeap::new()
        } else {
            let reader = MemoryReader::new(&memory, snapshot_base(), snapshot_len_bytes);
            RoaringHeap::deserialize_from(reader).map_err(|_| InitError::InvalidLayout)?
        };
        let mut state = HeapState { len_bits, bitmap };

        let mut journal_len = 0u64;
        let mut slot = [0u8; 5];
        for i in 0..crate::JOURNAL_CAP_SLOTS {
            let base = journal_offset() + (i as u64) * JOURNAL_RECORD_SIZE;
            read_5_bytes(&memory, base, &mut slot);
            if slot == [0u8; 5] {
                break;
            }
            apply_record(&mut state, JournalRecord(slot))?;
            journal_len += 1;
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

    /// Returns `true` when the logical length is zero.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Tests whether the selected bit is set.
    #[inline]
    pub fn contains(&self, index: u32) -> bool {
        let st = self.state.borrow();
        if u64::from(index) >= st.len_bits {
            return false;
        }
        st.bitmap.contains(index)
    }

    /// Returns a borrowed view that can be reused for repeated membership checks during a scan.
    #[inline]
    pub fn contains_view(&self) -> ContainsView<'_> {
        ContainsView {
            state: self.state.borrow(),
        }
    }

    /// Ensures that the logical length is at least `min_len`.
    pub fn ensure_len(&self, min_len: u64) -> Result<(), crate::GrowFailed> {
        assert!(
            min_len <= crate::JOURNAL_LEN_MAX,
            "bitmap length exceeds supported u32 index space"
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
    pub fn set(&self, index: u32, value: bool) -> Result<(), crate::GrowFailed> {
        let need_len = u64::from(index).saturating_add(1);
        assert!(
            need_len <= crate::JOURNAL_LEN_MAX,
            "bit index exceeds supported u32 index space"
        );
        if value {
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
    pub fn insert(&self, index: u32) -> Result<(), crate::GrowFailed> {
        self.set(index, true)
    }

    /// Clears a bit by setting it to `false`.
    pub fn clear(&self, index: u32) -> Result<(), crate::GrowFailed> {
        self.set(index, false)
    }

    /// Shrinks the logical length to `new_len`.
    pub fn truncate(&self, new_len: u64) -> Result<(), crate::GrowFailed> {
        assert!(
            new_len <= crate::JOURNAL_LEN_MAX,
            "bitmap length exceeds supported u32 index space"
        );
        if new_len >= self.len() {
            return Ok(());
        }
        self.append_record(JournalRecord::set_len(new_len))?;
        {
            let mut st = self.state.borrow_mut();
            st.len_bits = new_len;
            remove_suffix_bits(&mut st.bitmap, new_len);
        }
        self.maybe_checkpoint()?;
        Ok(())
    }

    /// Appends a packed mutation record to the journal.
    fn append_record(&self, record: JournalRecord) -> Result<(), crate::GrowFailed> {
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
    fn maybe_checkpoint(&self) -> Result<(), crate::GrowFailed> {
        if self.journal_len.get() >= crate::JOURNAL_CAP_SLOTS as u64 {
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
        let need_bytes = snapshot_base()
            .checked_add(snapshot_len_bytes)
            .expect("address overflow");
        grow_memory_to_at_least_bytes(&self.memory, need_bytes)?;

        {
            let st = self.state.borrow();
            let mut writer = MemoryWriter::new(&self.memory, snapshot_base());
            st.bitmap
                .serialize_into(&mut writer)
                .expect("serialize roaring snapshot");
        }

        write_header(&self.memory, len_bits, snapshot_len_bytes)?;
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
            if new_len > crate::JOURNAL_LEN_MAX {
                return Err(InitError::InvalidLayout);
            }
            if new_len < state.len_bits {
                state.len_bits = new_len;
                remove_suffix_bits(&mut state.bitmap, new_len);
            } else {
                state.len_bits = new_len;
            }
        }
        JournalTag::SetBit => {
            if payload > u32::MAX as u64 {
                return Err(InitError::InvalidLayout);
            }
            let index = payload as u32;
            if value {
                let need_len = u64::from(index).saturating_add(1);
                if need_len > crate::JOURNAL_LEN_MAX {
                    return Err(InitError::InvalidLayout);
                }
                if need_len > state.len_bits {
                    state.len_bits = need_len;
                }
                state.bitmap.insert(index);
            } else {
                state.bitmap.remove(index);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::{Memory, vec_mem::VectorMemory};

    fn reopen(memory: VectorMemory) -> RoaringBitmap<VectorMemory> {
        RoaringBitmap::init(memory).unwrap()
    }

    #[test]
    fn fresh_create_and_reopen_roundtrip() {
        let mem = VectorMemory::default();
        let bs = RoaringBitmap::new(mem).unwrap();
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
        let bs = RoaringBitmap::new(mem).unwrap();
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
        let bs = RoaringBitmap::new(mem).unwrap();
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
        let bs = RoaringBitmap::new(mem).unwrap();
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
        let bs = RoaringBitmap::new(mem).unwrap();
        for i in 0..crate::JOURNAL_CAP_SLOTS {
            bs.insert(i as u32).unwrap();
        }
        bs.clear(1).unwrap();
        bs.insert(crate::JOURNAL_CAP_SLOTS as u32).unwrap();
        assert!(bs.contains(0));
        assert!(!bs.contains(1));
        assert!(bs.contains(crate::JOURNAL_CAP_SLOTS as u32));
        assert_eq!(bs.len(), (crate::JOURNAL_CAP_SLOTS + 1) as u64);
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert!(bs.contains(0));
        assert!(!bs.contains(1));
        assert!(bs.contains(crate::JOURNAL_CAP_SLOTS as u32));
        assert_eq!(bs.len(), (crate::JOURNAL_CAP_SLOTS + 1) as u64);
    }

    #[test]
    fn mixed_operations_replay_deterministically() {
        let mem = VectorMemory::default();
        let bs = RoaringBitmap::new(mem).unwrap();
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
    fn sparse_high_u32_index_inserts_roundtrip() {
        let mem = VectorMemory::default();
        let bs = RoaringBitmap::new(mem).unwrap();
        let a = (1u32 << 31) + 123;
        let b = u32::MAX;
        bs.insert(a).unwrap();
        bs.insert(b).unwrap();
        assert!(bs.contains(a));
        assert!(bs.contains(b));
        assert_eq!(bs.len(), u32::MAX as u64 + 1);
        let mem = bs.into_memory();
        let bs = reopen(mem);
        assert!(bs.contains(a));
        assert!(bs.contains(b));
        assert_eq!(bs.len(), u32::MAX as u64 + 1);
    }

    #[test]
    fn invalid_magic_version_layout_and_snapshot_are_rejected() {
        let mem = VectorMemory::default();
        let bs = RoaringBitmap::new(mem).unwrap();
        bs.insert(1).unwrap();
        let mem = bs.into_memory();
        mem.write(0, b"BAD");
        assert!(matches!(
            RoaringBitmap::init(mem),
            Err(InitError::BadMagic { .. })
        ));

        let bs = RoaringBitmap::new(VectorMemory::default()).unwrap();
        let mem = bs.into_memory();
        mem.write(VERSION_OFFSET, &[VERSION.wrapping_add(1)]);
        assert!(matches!(
            RoaringBitmap::init(mem),
            Err(InitError::IncompatibleVersion(_))
        ));

        let bs = RoaringBitmap::new(VectorMemory::default()).unwrap();
        let mem = bs.into_memory();
        mem.write(
            JOURNAL_SLOTS_METADATA_OFFSET,
            &123u64.to_le_bytes(),
        );
        assert!(matches!(
            RoaringBitmap::init(mem),
            Err(InitError::InvalidLayout)
        ));

        let bs = RoaringBitmap::new(VectorMemory::default()).unwrap();
        let mem = bs.into_memory();
        mem.write(
            SNAPSHOT_LEN_OFFSET,
            &10_000_000u64.to_le_bytes(),
        );
        assert!(matches!(
            RoaringBitmap::init(mem),
            Err(InitError::InvalidLayout)
        ));
    }
}
