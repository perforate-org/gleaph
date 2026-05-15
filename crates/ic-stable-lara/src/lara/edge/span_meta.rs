//! Stable LARA segment span metadata.
//!
//! This store is placement metadata for update/maintenance work. Clean query
//! scans must not read it.

use crate::{GrowFailed, read_u64, safe_write, types::Address, write_u64};
use ic_stable_structures::Memory;
use std::{cell::Cell, fmt, marker::PhantomData};

/// Magic bytes that identify LARA segment span metadata.
pub const MAGIC: [u8; 3] = *b"LSP";
const LAYOUT_VERSION: u8 = 1;
const DATA_OFFSET: u64 = 32;
const LEN_OFFSET: u64 = 4;
const STRIDE_OFFSET: u64 = 12;
const ENTRY_SIZE: u64 = 8;

#[derive(Debug)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u64,
    stride: u32,
}

/// Placement metadata for one leaf segment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentSpanMeta {
    /// Physical edge-slab start slot assigned to this segment, or the internal
    /// unassigned sentinel until the segment obtains a contiguous slab reservation.
    pub physical_start: u64,
}

/// Sentinel stored in [`SegmentSpanMeta::physical_start`] before a segment slab is pinned.
///
/// Actual slab spans may legally start at slot `0`, so **`0` is never used** as “unassigned”.
pub(crate) const SPAN_PHYSICAL_UNASSIGNED: u64 = u64::MAX;

impl Default for SegmentSpanMeta {
    fn default() -> Self {
        Self {
            physical_start: SPAN_PHYSICAL_UNASSIGNED,
        }
    }
}

/// Errors returned when reopening segment span metadata.
#[derive(PartialEq, Eq, Debug)]
pub enum InitError {
    /// The memory header does not contain the LARA span metadata magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The stored layout version is not supported by this crate version.
    IncompatibleVersion(u8),
    /// The persisted row width does not match [`SegmentSpanMeta`].
    StrideMismatch {
        /// Expected row width.
        expected: u32,
        /// Row width read from stable memory.
        actual: u32,
    },
    /// The store could not allocate its metadata.
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(fmt, "bad segment span magic {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(version) => {
                write!(fmt, "unsupported segment span layout version {version}")
            }
            Self::StrideMismatch { expected, actual } => write!(
                fmt,
                "segment span stride mismatch: expected {expected}, got {actual}"
            ),
            Self::OutOfMemory => write!(fmt, "failed to allocate segment span metadata"),
        }
    }
}

impl std::error::Error for InitError {}

/// Stable vector of per-segment physical span starts.
#[derive(Clone, Debug)]
pub struct SegmentSpanMetaStore<M: Memory> {
    memory: M,
    /// Mirrors the persisted row count in the stable layout header; [`Self::len`]/`get` consult this first.
    header_len_mirror: Cell<u64>,
    _marker: PhantomData<SegmentSpanMeta>,
}

impl<M: Memory> SegmentSpanMetaStore<M> {
    /// Creates a fresh empty segment span metadata store.
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        let header = HeaderV1 {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            len: 0,
            stride: ENTRY_SIZE as u32,
        };
        Self::write_header(&header, &memory)?;
        Ok(Self {
            memory,
            header_len_mirror: Cell::new(header.len),
            _marker: PhantomData,
        })
    }

    /// Reopens an existing metadata store, or creates one if memory is empty.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let header = Self::read_header(&memory);
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        if header.stride != ENTRY_SIZE as u32 {
            return Err(InitError::StrideMismatch {
                expected: ENTRY_SIZE as u32,
                actual: header.stride,
            });
        }
        Ok(Self {
            memory,
            header_len_mirror: Cell::new(header.len),
            _marker: PhantomData,
        })
    }

    /// Consumes the store and returns the underlying memory.
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Returns the number of metadata rows.
    #[inline]
    pub fn len(&self) -> u64 {
        self.header_len_mirror.get()
    }

    /// Returns `true` when the store contains no rows.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Reads the metadata row at `index`.
    ///
    /// Panics if `index >= self.len()`.
    pub fn get(&self, index: u64) -> SegmentSpanMeta {
        assert!(index < self.len());
        SegmentSpanMeta {
            physical_start: read_u64(&self.memory, Address::from(Self::entry_offset(index))),
        }
    }

    /// Replaces the metadata row at `index`.
    ///
    /// Panics if `index >= self.len()`.
    pub fn set(&self, index: u64, item: &SegmentSpanMeta) {
        assert!(index < self.len());
        write_u64(
            &self.memory,
            Address::from(Self::entry_offset(index)),
            item.physical_start,
        );
    }

    /// Reads `physical_start` at `index`, stores `new_start`, and returns the previous value.
    ///
    /// Panics if `index >= self.len()`.
    ///
    /// Prefer [`update_physical_start`](Self::update_physical_start) when the new value is derived
    /// from the old one so the row is read only once.
    #[inline]
    pub fn replace_physical_start(&self, index: u64, new_start: u64) -> u64 {
        assert!(index < self.len());
        let addr = Address::from(Self::entry_offset(index));
        let old = read_u64(&self.memory, addr);
        write_u64(&self.memory, addr, new_start);
        old
    }

    /// Reads `physical_start` at `index`, replaces it with `f(old)`, and returns `old`.
    ///
    /// Panics if `index >= self.len()`.
    ///
    /// This performs one length check and one stable-memory read of the row; prefer it over
    /// [`get`](Self::get) followed by [`set`](Self::set) when updates depend on the prior value.
    #[inline]
    pub fn update_physical_start<F>(&self, index: u64, f: F) -> u64
    where
        F: FnOnce(u64) -> u64,
    {
        assert!(index < self.len());
        let addr = Address::from(Self::entry_offset(index));
        let old = read_u64(&self.memory, addr);
        write_u64(&self.memory, addr, f(old));
        old
    }

    /// Appends a metadata row and grows stable memory if necessary.
    pub fn push(&self, item: SegmentSpanMeta) -> Result<(), GrowFailed> {
        let len = self.len();
        let new_len = len
            .checked_add(1)
            .expect("segment span vector length overflow");
        safe_write(
            &self.memory,
            Self::entry_offset(len),
            &item.physical_start.to_le_bytes(),
        )?;
        self.set_len(new_len);
        Ok(())
    }

    fn set_len(&self, new_len: u64) {
        write_u64(&self.memory, Address::from(LEN_OFFSET), new_len);
        self.header_len_mirror.set(new_len);
    }

    #[inline]
    fn entry_offset(index: u64) -> u64 {
        DATA_OFFSET + ENTRY_SIZE * index
    }

    fn write_header(header: &HeaderV1, memory: &M) -> Result<(), GrowFailed> {
        safe_write(memory, 0, &header.magic)?;
        memory.write(3, &[header.version; 1]);
        write_u64(memory, Address::from(LEN_OFFSET), header.len);
        crate::write_u32(memory, Address::from(STRIDE_OFFSET), header.stride);
        Ok(())
    }

    fn read_header(memory: &M) -> HeaderV1 {
        let mut magic = [0u8; 3];
        let mut version = [0u8; 1];
        memory.read(0, &mut magic);
        memory.read(3, &mut version);
        HeaderV1 {
            magic,
            version: version[0],
            len: read_u64(memory, Address::from(LEN_OFFSET)),
            stride: crate::read_u32(memory, Address::from(STRIDE_OFFSET)),
        }
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::vector_memory;

    #[test]
    fn segment_span_meta_store_reopens_physical_starts() {
        let memory = vector_memory();
        let store = SegmentSpanMetaStore::new(memory.clone()).unwrap();
        store.push(SegmentSpanMeta { physical_start: 12 }).unwrap();
        store.push(SegmentSpanMeta { physical_start: 48 }).unwrap();

        let reopened = SegmentSpanMetaStore::init(memory).unwrap();
        assert_eq!(reopened.len(), 2);
        assert_eq!(reopened.get(0), SegmentSpanMeta { physical_start: 12 });
        assert_eq!(reopened.get(1), SegmentSpanMeta { physical_start: 48 });
    }

    #[test]
    fn replace_physical_start_returns_old_and_writes_new() {
        let memory = vector_memory();
        let store = SegmentSpanMetaStore::new(memory).unwrap();
        store
            .push(SegmentSpanMeta {
                physical_start: 100,
            })
            .unwrap();
        assert_eq!(store.replace_physical_start(0, 101), 100);
        assert_eq!(store.get(0).physical_start, 101);
    }

    #[test]
    fn update_physical_start_transforms_row_once() {
        let memory = vector_memory();
        let store = SegmentSpanMetaStore::new(memory).unwrap();
        store
            .push(SegmentSpanMeta {
                physical_start: 200,
            })
            .unwrap();
        assert_eq!(store.update_physical_start(0, |p| p.wrapping_add(7)), 200);
        assert_eq!(store.get(0).physical_start, 207);
    }
}

#[cfg(feature = "canbench")]
mod bench {
    use std::hint::black_box;

    use canbench_rs::bench;

    use super::{SegmentSpanMeta, SegmentSpanMetaStore};
    use crate::bench as helper;

    /// Measures segment span metadata append, read, update, and reopen. This
    /// protects the tiny placement-metadata store used by relocation while
    /// keeping query scans independent of it.
    #[bench(raw)]
    fn bench_lara_span_meta_push_get_set_reopen_1024() -> canbench_rs::BenchResult {
        let mut memories = helper::BenchMemoryFactory::new();
        let memory = memories.memory();
        let store = SegmentSpanMetaStore::new(memory.clone()).expect("span meta");
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_span_meta_push_get_set_reopen");
            for i in 0..helper::MEDIUM_N {
                store
                    .push(SegmentSpanMeta {
                        physical_start: black_box(i * 16),
                    })
                    .expect("push span meta");
            }
            let mut sum = 0u64;
            for i in 0..helper::MEDIUM_N {
                let i = black_box(i);
                let old = store.update_physical_start(i, |p| p.wrapping_add(1));
                sum ^= old;
            }
            let reopened = SegmentSpanMetaStore::init(memory.clone()).expect("reopen span meta");
            black_box(sum ^ reopened.get(helper::MEDIUM_N - 1).physical_start);
        })
    }
}
