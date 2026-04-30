//! The stable vector of segment counts.
//!
//! # V1 layout
//!
//! ```text
//! ---------------------------------------- <- Address 0
//! Magic "DSC"             ↕ 3 bytes
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
//! C_(L-1)                 ↕ 24 bytes
//! ----------------------------------------
//! Unallocated space
//! ```

use crate::{
    GrowFailed, read_u64, safe_write,
    traits::{CsrEdge, CsrEdgeTombstone},
    types::Address,
    write, write_u64,
};
use ic_stable_structures::Memory;
use std::{convert::TryInto, fmt, marker::PhantomData};

pub const MAGIC: [u8; 3] = *b"DSC";

const LAYOUT_VERSION: u8 = 1;
/// The offset where the user data begins.
const DATA_OFFSET: u64 = 32;
/// The offset where the vector length resides.
const LEN_OFFSET: u64 = 4;

#[derive(Debug)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u64,
}

#[derive(PartialEq, Eq, Debug)]
pub enum InitError {
    /// The memory already contains another data structure.
    /// Use [SegmentEdgeCountsStore::new] to overwrite it.
    BadMagic { actual: [u8; 3] },
    /// The current version of [Vec] does not support the version of the
    /// memory layout.
    IncompatibleVersion(u8),
    /// Failed to allocate memory for the vector.
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, fmt: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(fmt, "bad magic number {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(version) => write!(
                fmt,
                "unsupported layout version {version}; supported version numbers are 1..={LAYOUT_VERSION}"
            ),
            Self::OutOfMemory => write!(fmt, "failed to allocate memory for vector metadata"),
        }
    }
}

impl std::error::Error for InitError {}

/// Packed PMA counts for one segment-tree node (leaf = vertex block, internal = sum of children).
///
/// `tombstone` is persisted only when `E: CsrEdgeTombstone`. For non-tombstone
/// edge types the counts store uses a 16-byte stride and reads this field back
/// as `0`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SegmentEdgeCounts {
    pub actual: i64,
    pub total: i64,
    pub tombstone: i64,
}

/// Bytes per PMA tree node in the segment counts store.
pub trait EdgePmaCountsStride {
    fn pma_counts_stride_bytes() -> u64;
}

impl<E: CsrEdge> EdgePmaCountsStride for E {
    default fn pma_counts_stride_bytes() -> u64 {
        16
    }
}

impl<E: CsrEdge + CsrEdgeTombstone> EdgePmaCountsStride for E {
    fn pma_counts_stride_bytes() -> u64 {
        24
    }
}

impl SegmentEdgeCounts {
    #[inline]
    fn as_le_bytes(&self, stride: u64) -> [u8; 24] {
        debug_assert!(matches!(stride, 16 | 24));
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&self.actual.to_le_bytes());
        b[8..16].copy_from_slice(&self.total.to_le_bytes());
        if stride >= 24 {
            b[16..24].copy_from_slice(&self.tombstone.to_le_bytes());
        }
        b
    }

    #[inline]
    fn unpack_le(bs: &[u8; 24], stride: u64) -> Self {
        debug_assert!(matches!(stride, 16 | 24));
        Self {
            actual: i64::from_le_bytes(bs[0..8].try_into().unwrap()),
            total: i64::from_le_bytes(bs[8..16].try_into().unwrap()),
            tombstone: if stride >= 24 {
                i64::from_le_bytes(bs[16..24].try_into().unwrap())
            } else {
                0
            },
        }
    }
}

#[derive(Clone, Debug)]
pub struct SegmentEdgeCountsStore<E: CsrEdge, M: Memory> {
    memory: M,
    _marker: PhantomData<E>,
}

impl<E: CsrEdge + EdgePmaCountsStride, M: Memory> SegmentEdgeCountsStore<E, M> {
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        let header = HeaderV1 {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            len: 0,
        };
        Self::write_header(&header, &memory)?;
        Ok(Self {
            memory,
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
    pub fn len(&self) -> u64 {
        read_u64(&self.memory, Address::from(LEN_OFFSET))
    }

    #[inline]
    pub fn entry_size() -> u64 {
        E::pma_counts_stride_bytes()
    }

    #[inline]
    fn entry_offset(index: u64) -> u64 {
        DATA_OFFSET + Self::entry_size() * index
    }

    /// Reads `index` without checking logical length (caller must ensure the slot exists).
    #[inline]
    fn read_entry(memory: &M, index: u64) -> SegmentEdgeCounts {
        let mut buf = [0u8; 24];
        let stride = Self::entry_size();
        memory.read(Self::entry_offset(index), &mut buf[..stride as usize]);
        SegmentEdgeCounts::unpack_le(&buf, stride)
    }

    /// Returns the counts at `index`.
    ///
    /// Complexity: one 16- or 24-byte stable-memory read.
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
    /// Complexity: O(max_size(T))
    ///
    /// PRECONDITION: index < self.len()
    pub fn set(&self, index: u64, item: &SegmentEdgeCounts) {
        assert!(index < self.len());
        let stride = Self::entry_size();
        let bytes = item.as_le_bytes(stride);
        write(
            &self.memory,
            Self::entry_offset(index),
            &bytes[..stride as usize],
        );
    }

    /// Appends `item` after all existing entries, growing stable memory if necessary.
    ///
    /// Complexity: one [`safe_write`] of one entry's footprint plus updating length (`O(1)` logical updates).
    pub fn push(&self, item: SegmentEdgeCounts) -> Result<(), GrowFailed> {
        let len = self.len();
        let new_len = len
            .checked_add(1)
            .expect("segment counts vector length overflow");
        let stride = Self::entry_size();
        let bytes = item.as_le_bytes(stride);
        safe_write(
            &self.memory,
            Self::entry_offset(len),
            &bytes[..stride as usize],
        )?;
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

impl<'a, E: CsrEdge + EdgePmaCountsStride, M: Memory> Iterator for Iter<'a, E, M> {
    type Item = SegmentEdgeCounts;

    #[inline]
    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        let skip = n as u64;
        let remaining = self.back.saturating_sub(self.front);
        if skip >= remaining {
            self.front = self.back;
            return None;
        }
        self.front += skip;
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

impl<'a, E: CsrEdge + EdgePmaCountsStride, M: Memory> DoubleEndedIterator for Iter<'a, E, M> {
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

impl<'a, E: CsrEdge + EdgePmaCountsStride, M: Memory> ExactSizeIterator for Iter<'a, E, M> {}

impl<'a, E: CsrEdge + EdgePmaCountsStride, M: Memory> std::iter::FusedIterator for Iter<'a, E, M> {}
