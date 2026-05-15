//! Stable LARA per-segment overflow log.
//!
//! The index array and entry pool live in this memory. Vertex rows keep only
//! the head index into their owning segment's chain.
//!
//! # V1 layout
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! Magic "LLG"                           ↕ 3 bytes
//! --------------------------------------------------
//! Layout version                        ↕ 1 byte
//! --------------------------------------------------
//! Number of leaf segments               ↕ 4 bytes
//! --------------------------------------------------
//! Max log entries per segment           ↕ 4 bytes
//! --------------------------------------------------
//! Log entry stride                      ↕ 4 bytes
//! --------------------------------------------------
//! Reserved                              ↕ 16 bytes
//! -------------------------------------------------- <- Address 32
//! Segment log block 0
//!   I_0                                 ↕ 4 bytes
//!   L_0_0                               ↕ 8 + E::BYTES bytes
//!   L_0_1                               ↕ 8 + E::BYTES bytes
//!   ...
//!   L_0_(max_log_entries-1)             ↕ 8 + E::BYTES bytes
//! --------------------------------------------------
//! Segment log block 1
//!   I_1                                 ↕ 4 bytes
//!   L_1_0                               ↕ 8 + E::BYTES bytes
//!   L_1_1                               ↕ 8 + E::BYTES bytes
//!   ...
//!   L_1_(max_log_entries-1)             ↕ 8 + E::BYTES bytes
//! --------------------------------------------------
//! ...
//! --------------------------------------------------
//! Segment log block (segment_count-1)
//!   I_(segment_count-1)                 ↕ 4 bytes
//!   L_(segment_count-1)_0               ↕ 8 + E::BYTES bytes
//!   ...
//!   L_(segment_count-1)_(max_log_entries-1)
//!                                       ↕ 8 + E::BYTES bytes
//! --------------------------------------------------
//! Unallocated space
//! ```
//!
//! Each log entry stores `prev_offset` (4 bytes), `src` (4 bytes), and the edge payload.

use crate::{
    GrowFailed, read_i32, read_u32, safe_write, traits::CsrEdge, types::Address, write_i32,
    write_u32,
};
use ic_stable_structures::Memory;
use std::{cell::Cell, fmt, marker::PhantomData};

/// Magic bytes that identify a LARA overflow-log memory.
pub const MAGIC: [u8; 3] = *b"LLG";
/// Current overflow-log layout version.
pub const LAYOUT_VERSION: u8 = 1;
const HEADER_SIZE: u64 = 32;
const INLINE_LOG_ENTRY_BYTES: usize = 128;

/// Default per-segment overflow-log capacity.
pub const DEFAULT_MAX_LOG_ENTRIES: u32 = 170;

/// Persisted V1 overflow-log header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    /// Magic bytes, always `LLG` for this layout.
    pub magic: [u8; 3],
    /// Layout version for this header.
    pub version: u8,
    /// Number of leaf segments with a log block.
    pub segment_count: u32,
    /// Maximum number of log entries in each segment block.
    pub max_log_entries: u32,
    /// Encoded byte width of one log entry.
    pub stride: u32,
}

impl HeaderV1 {
    /// Builds a fresh overflow-log header.
    pub fn new(segment_count: u32, edge_stride: u32) -> Self {
        Self {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            segment_count,
            max_log_entries: DEFAULT_MAX_LOG_ENTRIES,
            stride: 8 + edge_stride,
        }
    }
}

/// Errors returned when reopening a persisted overflow log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The memory header does not contain the LARA log magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The stored layout version is not supported by this crate version.
    IncompatibleVersion(u8),
    /// The memory is empty or the log metadata could not be allocated.
    OutOfMemory,
    /// The persisted entry width does not match the edge type `E`.
    StrideMismatch {
        /// Expected log-entry width.
        expected: u32,
        /// Log-entry width read from stable memory.
        actual: u32,
    },
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => write!(f, "bad log magic {actual:?}, expected {MAGIC:?}"),
            Self::IncompatibleVersion(v) => write!(f, "unsupported log layout version {v}"),
            Self::OutOfMemory => write!(f, "failed to allocate log metadata"),
            Self::StrideMismatch { expected, actual } => {
                write!(
                    f,
                    "log entry stride mismatch: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Stable per-segment overflow log for edges that did not fit on the slab.
#[derive(Clone, Debug)]
pub struct LogStore<E: CsrEdge, M: Memory> {
    memory: M,
    /// Mirrors the persisted overflow-log header; hot paths consult this instead of
    /// rereading stable memory whenever [`LogStore::header`] layout fields are needed.
    header_mirror: Cell<HeaderV1>,
    _marker: PhantomData<E>,
}

impl<E: CsrEdge, M: Memory> LogStore<E, M> {
    /// Creates a fresh overflow log with `header`.
    pub fn new(memory: M, header: HeaderV1) -> Result<Self, GrowFailed> {
        let store = Self {
            memory,
            header_mirror: Cell::new(header),
            _marker: PhantomData,
        };
        store.grow_for_header(&header)?;
        store.write_header(&header)?;
        Ok(store)
    }

    /// Reopens an existing overflow log from stable memory.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Err(InitError::OutOfMemory);
        }
        let header = Self::read_header_from_memory(&memory);
        let store = Self {
            memory,
            header_mirror: Cell::new(header),
            _marker: PhantomData,
        };
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        let expected = log_entry_stride::<E>() as u32;
        if header.stride != expected {
            return Err(InitError::StrideMismatch {
                expected,
                actual: header.stride,
            });
        }
        Ok(store)
    }

    /// Consumes the store and returns its underlying memory.
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Returns the mirrored overflow-log header (kept in sync with stable storage on writes).
    #[inline]
    pub fn header(&self) -> HeaderV1 {
        self.header_mirror.get()
    }

    /// Reads the next-free entry index for a leaf segment.
    #[allow(dead_code)] // stable `LogStore` API; in-crate uses `read_idx_with_header`
    pub fn read_idx(&self, leaf_segment: u32) -> i32 {
        let h = self.header();
        self.read_idx_with_header(&h, leaf_segment)
    }

    pub(crate) fn read_idx_with_header(&self, h: &HeaderV1, leaf_segment: u32) -> i32 {
        read_i32(
            &self.memory,
            Address::from(idx_offset::<E>(h, leaf_segment)),
        )
    }

    /// Copies every allocated log entry slot for `leaf_segment` (`0..read_idx`) in
    /// one stable-memory read. `out` is cleared when the segment has no entries yet.
    ///
    /// Chain walks can index this buffer instead of [`Self::read_entry_with_header`]
    /// per hop; indices beyond the copied prefix fall back to per-entry reads.
    pub(crate) fn read_segment_entry_table_into(
        &self,
        h: &HeaderV1,
        leaf_segment: u32,
        out: &mut Vec<u8>,
    ) {
        let stride = h.stride as usize;
        if stride == 0 {
            out.clear();
            return;
        }
        let next_idx = self.read_idx_with_header(h, leaf_segment);
        if next_idx <= 0 {
            out.clear();
            return;
        }
        let capped = (next_idx as u32).min(h.max_log_entries) as usize;
        let Some(nbytes) = capped.checked_mul(stride) else {
            out.clear();
            return;
        };
        out.resize(nbytes, 0);
        let start = entry_offset::<E>(h, leaf_segment, 0);
        self.memory.read(start, out.as_mut_slice());
    }

    /// Writes the next-free entry index for a leaf segment.
    #[allow(dead_code)] // stable `LogStore` API; in-crate uses `write_idx_with_header`
    pub fn write_idx(&self, leaf_segment: u32, idx: i32) {
        let h = self.header();
        self.write_idx_with_header(&h, leaf_segment, idx);
    }

    pub(crate) fn write_idx_with_header(&self, h: &HeaderV1, leaf_segment: u32, idx: i32) {
        write_i32(
            &self.memory,
            Address::from(idx_offset::<E>(h, leaf_segment)),
            idx,
        );
    }

    /// Reads one log entry payload and returns `(previous_entry, source_vertex)`.
    #[allow(dead_code)] // stable `LogStore` API; in-crate uses `read_entry_with_header`
    pub fn read_entry(&self, leaf_segment: u32, entry_idx: u32, out: &mut [u8]) -> (i32, i32) {
        let h = self.header();
        self.read_entry_with_header(&h, leaf_segment, entry_idx, out)
    }

    pub(crate) fn read_entry_with_header(
        &self,
        h: &HeaderV1,
        leaf_segment: u32,
        entry_idx: u32,
        out: &mut [u8],
    ) -> (i32, i32) {
        let off = entry_offset::<E>(h, leaf_segment, entry_idx);
        let prev = read_i32(&self.memory, Address::from(off));
        let src = read_i32(&self.memory, Address::from(off + 4));
        self.memory.read(off + 8, out);
        (prev, src)
    }

    /// Writes one log entry in a leaf segment.
    #[allow(dead_code)] // stable `LogStore` API; in-crate uses `write_entry_with_header`
    pub fn write_entry(
        &self,
        leaf_segment: u32,
        entry_idx: u32,
        prev: i32,
        src: i32,
        payload: &[u8],
    ) -> Result<(), GrowFailed> {
        let h = self.header();
        self.write_entry_with_header(&h, leaf_segment, entry_idx, prev, src, payload)
    }

    pub(crate) fn write_entry_with_header(
        &self,
        h: &HeaderV1,
        leaf_segment: u32,
        entry_idx: u32,
        prev: i32,
        src: i32,
        payload: &[u8],
    ) -> Result<(), GrowFailed> {
        let off = entry_offset::<E>(h, leaf_segment, entry_idx);
        debug_assert_eq!(payload.len(), E::BYTES);
        let entry_len = log_entry_stride::<E>() as usize;
        if entry_len <= INLINE_LOG_ENTRY_BYTES {
            let mut bytes = [0u8; INLINE_LOG_ENTRY_BYTES];
            bytes[0..4].copy_from_slice(&prev.to_le_bytes());
            bytes[4..8].copy_from_slice(&src.to_le_bytes());
            bytes[8..8 + payload.len()].copy_from_slice(payload);
            safe_write(&self.memory, off, &bytes[..entry_len])
        } else {
            let mut bytes = vec![0u8; entry_len];
            bytes[0..4].copy_from_slice(&prev.to_le_bytes());
            bytes[4..8].copy_from_slice(&src.to_le_bytes());
            bytes[8..8 + payload.len()].copy_from_slice(payload);
            safe_write(&self.memory, off, &bytes)
        }
    }

    /// Clears all log entries and resets the segment index to zero.
    pub fn release_segment(&self, leaf_segment: u32) -> Result<(), GrowFailed> {
        let h = self.header();
        let idx = self.read_idx_with_header(&h, leaf_segment);
        let stride = log_entry_stride::<E>() as usize;
        if stride <= INLINE_LOG_ENTRY_BYTES {
            let zeros = [0u8; INLINE_LOG_ENTRY_BYTES];
            for i in 0..idx.max(0) as u32 {
                safe_write(
                    &self.memory,
                    entry_offset::<E>(&h, leaf_segment, i),
                    &zeros[..stride],
                )?;
            }
        } else {
            let zeros = vec![0u8; stride];
            for i in 0..idx.max(0) as u32 {
                safe_write(&self.memory, entry_offset::<E>(&h, leaf_segment, i), &zeros)?;
            }
        }
        self.write_idx_with_header(&h, leaf_segment, 0);
        Ok(())
    }

    fn write_header(&self, h: &HeaderV1) -> Result<(), GrowFailed> {
        safe_write(&self.memory, 0, &h.magic)?;
        self.memory.write(3, &[h.version]);
        write_u32(&self.memory, Address::from(4), h.segment_count);
        write_u32(&self.memory, Address::from(8), h.max_log_entries);
        write_u32(&self.memory, Address::from(12), h.stride);
        self.memory.write(16, &[0u8; 16]);
        self.header_mirror.set(*h);
        Ok(())
    }

    fn read_header_from_memory(memory: &M) -> HeaderV1 {
        let mut magic = [0u8; 3];
        let mut version = [0u8; 1];
        memory.read(0, &mut magic);
        memory.read(3, &mut version);
        HeaderV1 {
            magic,
            version: version[0],
            segment_count: read_u32(memory, Address::from(4)),
            max_log_entries: read_u32(memory, Address::from(8)),
            stride: read_u32(memory, Address::from(12)),
        }
    }

    fn grow_for_header(&self, h: &HeaderV1) -> Result<(), GrowFailed> {
        let need = required_bytes::<E>(h);
        if need == 0 {
            return Ok(());
        }
        safe_write(&self.memory, need - 1, &[0])
    }

    /// Extends the log layout so `segment_count` becomes `new_count`, resetting new segment indexes.
    pub(crate) fn grow_segment_count_to(&self, new_count: u32) -> Result<(), GrowFailed> {
        let mut h = self.header();
        let old = h.segment_count;
        if new_count <= old {
            return Ok(());
        }
        h.segment_count = new_count;
        self.grow_for_header(&h)?;
        self.write_header(&h)?;
        for leaf in old..new_count {
            self.write_idx_with_header(&h, leaf, 0);
        }
        Ok(())
    }
}

#[inline]
fn idx_offset<E: CsrEdge>(h: &HeaderV1, leaf_segment: u32) -> u64 {
    HEADER_SIZE + u64::from(leaf_segment) * segment_block_size::<E>(h)
}

#[inline]
fn segment_block_size<E: CsrEdge>(h: &HeaderV1) -> u64 {
    4 + u64::from(h.max_log_entries).saturating_mul(log_entry_stride::<E>())
}

#[inline]
fn entry_offset<E: CsrEdge>(h: &HeaderV1, leaf_segment: u32, entry_idx: u32) -> u64 {
    idx_offset::<E>(h, leaf_segment)
        .saturating_add(4)
        .saturating_add(u64::from(entry_idx).saturating_mul(log_entry_stride::<E>()))
}

#[inline]
fn required_bytes<E: CsrEdge>(h: &HeaderV1) -> u64 {
    HEADER_SIZE
        .saturating_add(u64::from(h.segment_count).saturating_mul(segment_block_size::<E>(h)))
}

/// Returns the byte width of one log entry for edge type `E`.
#[inline]
pub fn log_entry_stride<E: CsrEdge>() -> u64 {
    8 + E::BYTES as u64
}

#[cfg(feature = "canbench")]
mod bench {
    use std::hint::black_box;

    use canbench_rs::bench;

    use super::{HeaderV1, LogStore};
    use crate::{bench as helper, test_support::TestEdge, traits::CsrEdge};

    /// Measures per-segment log entry writes, reads, index updates, and one
    /// segment release. This is the storage-layer baseline for overflow edges
    /// before graph maintenance folds them back into the slab.
    #[bench(raw)]
    fn bench_lara_edge_log_write_read_release_1024() -> canbench_rs::BenchResult {
        let mut memories = helper::BenchMemoryFactory::new();
        let store = LogStore::<TestEdge, _>::new(memories.memory(), HeaderV1::new(16, 4))
            .expect("log store");
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_edge_log_write_read_release");
            let mut payload = [0u8; TestEdge::BYTES];
            for i in 0..helper::MEDIUM_N {
                let i = black_box(i);
                let segment = (i % 16) as u32;
                let entry = (i / 16) as u32;
                helper::test_edge(i).write_to(&mut payload);
                store
                    .write_entry(
                        black_box(segment),
                        black_box(entry),
                        entry as i32 - 1,
                        i as i32,
                        &payload,
                    )
                    .expect("write log entry");
                store.write_idx(black_box(segment), entry as i32 + 1);
            }
            let mut sum = 0i32;
            for i in 0..helper::MEDIUM_N {
                let i = black_box(i);
                let segment = (i % 16) as u32;
                let entry = (i / 16) as u32;
                let (prev, src) =
                    store.read_entry(black_box(segment), black_box(entry), &mut payload);
                sum ^= prev ^ src ^ TestEdge::read_from(&payload).0 as i32;
            }
            store.release_segment(0).expect("release segment");
            black_box(sum);
        })
    }
}
