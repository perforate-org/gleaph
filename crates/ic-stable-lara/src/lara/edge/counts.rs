//! The stable vector of segment counts.
//!
//! # V1 layout
//!
//! ```text
//! ---------------------------------------- <- Address 0
//! Magic "LSC"             ↕ 3 bytes
//! ----------------------------------------
//! Layout version          ↕ 1 byte
//! ----------------------------------------
//! Number of segments = L  ↕ 8 bytes
//! ----------------------------------------
//! Reserved space          ↕ 20 bytes
//! ---------------------------------------- <- Address 32
//! C_0                     ↕ 16 or 24 bytes
//! ----------------------------------------
//! C_1                     ↕ 16 or 24 bytes
//! ----------------------------------------
//! ...
//! ----------------------------------------
//! C_(L-1)                 ↕ 16 or 24 bytes
//! ----------------------------------------
//! Unallocated space
//! ```

use crate::{GrowFailed, read_u64, safe_write, traits::CsrEdge, types::Address, write, write_u64};
use ic_stable_structures::Memory;
use std::{cell::Cell, convert::TryInto, fmt, marker::PhantomData, num::NonZero};

/// Magic bytes that identify a LARA segment-count memory.
pub const MAGIC: [u8; 3] = *b"LSC";

const LAYOUT_VERSION: u8 = 1;
/// The offset where the user data begins.
const DATA_OFFSET: u64 = 32;
/// The offset where the vector length resides.
const LEN_OFFSET: u64 = 4;

/// PMA segment-count row width (`actual` + `total`, little-endian `i64` each).
pub const ENTRY_BYTES: u64 = 16;

#[derive(Debug)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u64,
}

/// Errors returned when reopening a persisted segment-count store.
#[derive(PartialEq, Eq, Debug)]
pub enum InitError {
    /// The memory already contains another data structure.
    /// Use [SegmentEdgeCountsStore::new] to overwrite it.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The current version of this store does not support the version of the
    /// memory layout.
    IncompatibleVersion(u8),
    /// Failed to allocate memory for the vector.
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(f, "bad magic number {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(version) => write!(
                f,
                "unsupported layout version {version}; supported version numbers are 1..={LAYOUT_VERSION}"
            ),
            Self::OutOfMemory => write!(f, "failed to allocate memory for vector metadata"),
        }
    }
}

impl std::error::Error for InitError {}

/// Packed PMA counts for one segment-tree node (leaf = vertex block, internal = sum of children).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentEdgeCounts {
    /// Number of live edge records in this segment-tree node.
    pub actual: i64,
    /// Number of occupied physical slab slots in this segment-tree node.
    pub total: i64,
}

impl SegmentEdgeCounts {
    #[inline]
    fn as_le_bytes(&self) -> [u8; 16] {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.actual.to_le_bytes());
        b[8..16].copy_from_slice(&self.total.to_le_bytes());
        b
    }

    #[inline]
    fn unpack_le(bs: &[u8; 16]) -> Self {
        Self {
            actual: i64::from_le_bytes(bs[0..8].try_into().unwrap()),
            total: i64::from_le_bytes(bs[8..16].try_into().unwrap()),
        }
    }
}

/// Returns `actual / total` for PMA segment density, or `0.0` when `total <= 0`.
#[inline]
pub(crate) fn segment_span_density(counts: SegmentEdgeCounts) -> f64 {
    if counts.total <= 0 {
        0.0
    } else {
        counts.actual as f64 / counts.total as f64
    }
}

/// Stable vector storing PMA counts for leaves and internal segment-tree nodes.
#[derive(Clone, Debug)]
pub struct SegmentEdgeCountsStore<E: CsrEdge, M: Memory> {
    memory: M,
    /// Mirrors the persisted row count in the stable layout header; hot paths consult this instead of stable reads.
    header_len_mirror: Cell<u64>,
    _marker: PhantomData<E>,
}

impl<E: CsrEdge, M: Memory> SegmentEdgeCountsStore<E, M> {
    /// Creates a fresh empty counts store.
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        let header = HeaderV1 {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            len: 0,
        };
        Self::write_header(&header, &memory)?;
        Ok(Self {
            memory,
            header_len_mirror: Cell::new(header.len),
            _marker: PhantomData,
        })
    }

    /// Initializes a vector in the specified memory.
    ///
    /// Complexity: O(1)
    ///
    /// PRECONDITION: the memory is either empty or contains a valid
    /// stable vector.
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

        Ok(Self {
            memory,
            header_len_mirror: Cell::new(header.len),
            _marker: PhantomData,
        })
    }

    /// Write the layout header to the memory.
    fn write_header(header: &HeaderV1, memory: &M) -> Result<(), GrowFailed> {
        safe_write(memory, 0, &header.magic)?;
        memory.write(3, &[header.version; 1]);
        write_u64(memory, Address::from(4), header.len);
        Ok(())
    }

    /// Returns the underlying memory instance.
    pub fn into_memory(self) -> M {
        self.memory
    }

    /// Returns true if the vector is empty.
    ///
    /// Complexity: O(1)
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the number of items in the vector.
    ///
    /// Complexity: O(1)
    #[inline]
    pub fn len(&self) -> u64 {
        self.header_len_mirror.get()
    }

    /// Returns the persisted byte width of one count row (always [`ENTRY_BYTES`]).
    #[inline]
    pub fn entry_size() -> u64 {
        ENTRY_BYTES
    }

    #[inline]
    fn entry_offset(index: u64) -> u64 {
        DATA_OFFSET + Self::entry_size() * index
    }

    /// Reads `index` without checking logical length (caller must ensure the slot exists).
    #[inline]
    fn read_entry(memory: &M, index: u64) -> SegmentEdgeCounts {
        let mut buf = [0u8; 16];
        memory.read(Self::entry_offset(index), &mut buf);
        SegmentEdgeCounts::unpack_le(&buf)
    }

    /// Returns the counts at `index`.
    ///
    /// Complexity: one 16-byte stable-memory read.
    ///
    /// PRECONDITION: index < self.len()
    pub fn get(&self, index: u64) -> SegmentEdgeCounts {
        assert!(index < self.len());
        Self::read_entry(&self.memory, index)
    }

    /// Iterator over all entries in index order (length is fixed when [`Self::iter`] is called).
    pub fn iter(&self) -> Iter<'_, E, M> {
        let len = self.len();
        Iter {
            memory: &self.memory,
            front: 0,
            back: len,
            _marker: PhantomData,
        }
    }

    /// Sets the item at the specified index to the specified value.
    ///
    /// Complexity: O(16) bytes written.
    ///
    /// PRECONDITION: index < self.len()
    pub fn set(&self, index: u64, item: &SegmentEdgeCounts) {
        assert!(index < self.len());
        let bytes = item.as_le_bytes();
        write(&self.memory, Self::entry_offset(index), &bytes);
    }

    /// Appends `item` after all existing entries, growing stable memory if necessary.
    ///
    /// Complexity: one stable-memory write of one entry's footprint plus updating length (`O(1)` logical updates).
    pub fn push(&self, item: SegmentEdgeCounts) -> Result<(), GrowFailed> {
        let len = self.len();
        let new_len = len
            .checked_add(1)
            .expect("segment counts vector length overflow");
        let bytes = item.as_le_bytes();
        safe_write(&self.memory, Self::entry_offset(len), &bytes)?;
        self.set_len(new_len);
        Ok(())
    }

    /// Removes and returns the last entry, or `None` if the vector is empty.
    ///
    /// Complexity: one read plus updating length. Does not shrink reserved stable memory.
    pub fn pop(&self) -> Option<SegmentEdgeCounts> {
        let len = self.len();
        if len == 0 {
            return None;
        }
        let last = len - 1;
        let item = Self::read_entry(&self.memory, last);
        self.set_len(last);
        Some(item)
    }

    /// Sets the vector's length.
    fn set_len(&self, new_len: u64) {
        write_u64(&self.memory, Address::from(LEN_OFFSET), new_len);
        self.header_len_mirror.set(new_len);
    }

    /// Reads the header from the specified memory.
    ///
    /// PRECONDITION: memory.size() > 0
    fn read_header(memory: &M) -> HeaderV1 {
        debug_assert!(memory.size() > 0);

        let mut magic = [0u8; 3];
        let mut version = [0u8; 1];
        memory.read(0, &mut magic);
        memory.read(3, &mut version);
        let len = read_u64(memory, Address::from(LEN_OFFSET));

        HeaderV1 {
            magic,
            version: version[0],
            len,
        }
    }
}

/// Double-ended iterator over [`SegmentEdgeCounts`] in index order (`front` … `back` exclusive).
pub struct Iter<'a, E: CsrEdge, M: Memory> {
    memory: &'a M,
    /// Next index for [`Iterator::next`].
    front: u64,
    /// One past the last index for [`DoubleEndedIterator::next_back`].
    back: u64,
    _marker: PhantomData<E>,
}

impl<'a, E: CsrEdge, M: Memory> Iterator for Iter<'a, E, M> {
    type Item = SegmentEdgeCounts;

    #[inline]
    fn advance_by(&mut self, n: usize) -> Result<(), NonZero<usize>> {
        if n == 0 {
            return Ok(());
        }
        let remaining_u64 = self.back.saturating_sub(self.front);
        let remaining = usize::try_from(remaining_u64).unwrap_or(usize::MAX);
        if n >= remaining {
            self.front = self.back;
            return match n - remaining {
                0 => Ok(()),
                left => Err(NonZero::new(left).expect("left > 0")),
            };
        }
        self.front = self
            .front
            .checked_add(n as u64)
            .expect("front + n fits within back");
        Ok(())
    }

    #[inline]
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.advance_by(n).ok()?;
        self.next()
    }

    #[inline]
    fn next(&mut self) -> Option<Self::Item> {
        if self.front >= self.back {
            return None;
        }
        let item = SegmentEdgeCountsStore::<E, M>::read_entry(self.memory, self.front);
        self.front += 1;
        Some(item)
    }

    #[inline]
    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = self.back.saturating_sub(self.front);
        let n = usize::try_from(remaining).unwrap_or(usize::MAX);
        (n, Some(n))
    }

    #[inline]
    fn count(self) -> usize {
        let remaining = self.back.saturating_sub(self.front);
        usize::try_from(remaining).unwrap_or(usize::MAX)
    }
}

impl<'a, E: CsrEdge, M: Memory> DoubleEndedIterator for Iter<'a, E, M> {
    #[inline]
    fn nth_back(&mut self, n: usize) -> Option<Self::Item> {
        let skip = n as u64;
        let remaining = self.back.saturating_sub(self.front);
        if skip >= remaining {
            self.front = self.back;
            return None;
        }
        self.back -= skip;
        self.next_back()
    }

    #[inline]
    fn next_back(&mut self) -> Option<Self::Item> {
        if self.front >= self.back {
            return None;
        }
        self.back -= 1;
        Some(SegmentEdgeCountsStore::<E, M>::read_entry(
            self.memory,
            self.back,
        ))
    }
}

impl<'a, E: CsrEdge, M: Memory> ExactSizeIterator for Iter<'a, E, M> {}

impl<'a, E: CsrEdge, M: Memory> std::iter::FusedIterator for Iter<'a, E, M> {}

#[cfg(test)]
mod tests {
    use crate::VectorMemory;
    use crate::test_support::{TestEdge, vector_memory};

    #[test]
    fn segment_edge_counts_entry_size_is_fixed() {
        use crate::lara::edge::counts::{ENTRY_BYTES, SegmentEdgeCounts, SegmentEdgeCountsStore};

        let store = SegmentEdgeCountsStore::<TestEdge, _>::new(vector_memory()).unwrap();
        assert_eq!(
            SegmentEdgeCountsStore::<TestEdge, VectorMemory>::entry_size(),
            ENTRY_BYTES
        );
        let counts = SegmentEdgeCounts {
            actual: 1,
            total: 2,
        };
        store.push(counts).unwrap();
        assert_eq!(store.get(0), counts);
    }
}

#[cfg(feature = "canbench")]
mod bench {
    use std::hint::black_box;

    use canbench_rs::bench;

    use super::{SegmentEdgeCounts, SegmentEdgeCountsStore};
    use crate::{bench as helper, test_support::TestEdge};

    fn populate_plain(n: u64) -> SegmentEdgeCountsStore<TestEdge, helper::BenchMemory> {
        let mut memories = helper::BenchMemoryFactory::new();
        let store = SegmentEdgeCountsStore::new(memories.memory()).expect("counts store");
        for i in 0..n {
            store
                .push(SegmentEdgeCounts {
                    actual: i as i64,
                    total: (i * 2) as i64,
                })
                .expect("push counts");
        }
        store
    }

    fn bench_counts_push(scope: &'static str) -> canbench_rs::BenchResult {
        let mut memories = helper::BenchMemoryFactory::new();
        let store =
            SegmentEdgeCountsStore::<TestEdge, _>::new(memories.memory()).expect("counts store");
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope(scope);
            for i in 0..helper::MEDIUM_N {
                store
                    .push(SegmentEdgeCounts {
                        actual: black_box(i as i64),
                        total: black_box((i * 2) as i64),
                    })
                    .expect("push counts");
            }
        })
    }

    /// Measures appending segment count rows.
    #[bench(raw)]
    fn bench_lara_counts_push_plain_1024() -> canbench_rs::BenchResult {
        bench_counts_push("lara_counts_push_plain")
    }

    /// Measures mixed reads, writes, and iteration over segment counts. This
    /// protects the common recount path that reads leaves, rewrites ancestors,
    /// and scans count ranges.
    #[bench(raw)]
    fn bench_lara_counts_get_set_iter_1024() -> canbench_rs::BenchResult {
        let store = populate_plain(helper::MEDIUM_N);
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_counts_get_set_iter");
            let mut sum = 0i64;
            for i in 0..helper::MEDIUM_N {
                let i = black_box(i);
                let count = store.get(i);
                sum = sum.wrapping_add(count.actual);
                store.set(
                    i,
                    &SegmentEdgeCounts {
                        actual: count.actual + 1,
                        ..count
                    },
                );
            }
            for count in store.iter() {
                sum = sum.wrapping_add(count.total);
            }
            black_box(sum);
        })
    }
}
