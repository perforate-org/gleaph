//! Per-segment overflow log for edge inline property bytes.
//!
//! Each entry stores `prev` (4 bytes) and an 8-byte inline cell. Liveness is encoded in the paired
//! edge overflow log tombstone contract at the same `(leaf_segment, entry_idx)`.

use crate::{
    GrowFailed,
    lara::reserved::{region_is_zero, write_zeroes},
    read_i32, read_u32, safe_write,
    types::Address,
    write_i32, write_u32,
};
use ic_stable_structures::Memory;
use std::{cell::Cell, fmt};

use super::cell::INLINE_PROPERTY_BYTES_LOG_CELL_BYTES;

/// Magic bytes that identify a LARA inline property bytes overflow-log memory.
pub const MAGIC: [u8; 3] = *b"LIL";
/// Current overflow-log layout version.
pub const LAYOUT_VERSION: u8 = 1;
const HEADER_SIZE: u64 = 32;
const INLINE_LOG_ENTRY_BYTES: usize = 16;
/// Declared reserved header bytes between the stride field and [`HEADER_SIZE`].
const HEADER_RESERVED_OFFSET: u64 = 16;
const HEADER_RESERVED_SIZE: usize = 16;
/// Inline property bytes log cell bytes per overflow log entry.
/// Default per-segment overflow-log capacity (matches edge log).
pub const DEFAULT_MAX_LOG_ENTRIES: u32 = 170;

/// Persisted V1 inline property bytes overflow-log header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    /// Header magic bytes.
    pub magic: [u8; 3],
    /// Inline property bytes log layout version.
    pub version: u8,
    /// Number of edge segments represented by the log.
    pub segment_count: u32,
    /// Maximum log entries per segment.
    pub max_log_entries: u32,
    /// Bytes reserved for each log entry.
    pub stride: u32,
}

impl HeaderV1 {
    /// Creates a inline-property-bytes-log header for `segment_count` segments.
    pub fn new(segment_count: u32) -> Self {
        Self {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            segment_count,
            max_log_entries: DEFAULT_MAX_LOG_ENTRIES,
            stride: INLINE_PROPERTY_BYTES_LOG_ENTRY_STRIDE as u32,
        }
    }
}

/// Errors returned when reopening a persisted inline property bytes overflow log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The inline-property-bytes-log header had unexpected magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The inline-property-bytes-log layout version is not supported.
    IncompatibleVersion(u8),
    /// A declared header reserved byte is nonzero (foreign or corrupt layout).
    ReservedRegionNonZero,
    /// The inline-property-bytes-log memory could not be allocated or was empty on reopen.
    OutOfMemory,
    /// The persisted entry stride does not match this implementation.
    StrideMismatch {
        /// Expected entry stride.
        expected: u32,
        /// Entry stride read from stable memory.
        actual: u32,
    },
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(
                    f,
                    "bad inline property bytes log magic {actual:?}, expected {MAGIC:?}"
                )
            }
            Self::IncompatibleVersion(v) => write!(
                f,
                "unsupported inline property bytes log layout version {v}"
            ),
            Self::OutOfMemory => write!(f, "failed to allocate inline property bytes log metadata"),
            Self::ReservedRegionNonZero => {
                write!(
                    f,
                    "inline property bytes log header reserved region must be zero"
                )
            }
            Self::StrideMismatch { expected, actual } => {
                write!(
                    f,
                    "inline property bytes log entry stride mismatch: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Stable per-segment overflow log for values that did not fit on the byte slab.
#[derive(Clone, Debug)]
pub struct InlinePropertyBytesLogStore<M: Memory> {
    memory: M,
    header_mirror: Cell<HeaderV1>,
}

impl<M: Memory> InlinePropertyBytesLogStore<M> {
    /// Creates a new inline property bytes overflow log with `header`.
    pub fn new(memory: M, header: HeaderV1) -> Result<Self, GrowFailed> {
        let store = Self {
            memory,
            header_mirror: Cell::new(header),
        };
        store.grow_for_header(&header)?;
        store.write_header(&header)?;
        Ok(store)
    }

    /// Reopens an existing inline property bytes overflow log.
    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Err(InitError::OutOfMemory);
        }
        let header = Self::read_header_from_memory(&memory);
        let store = Self {
            memory,
            header_mirror: Cell::new(header),
        };
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        let expected = INLINE_PROPERTY_BYTES_LOG_ENTRY_STRIDE as u32;
        if header.stride != expected {
            return Err(InitError::StrideMismatch {
                expected,
                actual: header.stride,
            });
        }
        // Reserved-region guard: nonzero reserved bytes mean a foreign or corrupt
        // layout; fail closed with the dedicated variant. The structurally unsound
        // backing-size guard below keeps its OutOfMemory mapping.
        if !region_is_zero(&store.memory, HEADER_RESERVED_OFFSET, HEADER_RESERVED_SIZE) {
            return Err(InitError::ReservedRegionNonZero);
        }
        // Backing-size guard (mirrors free_span::validate_header): reject memory smaller than
        // the layout the header declares so later per-segment offset reads cannot trap with an
        // opaque out-of-bounds error.
        let needed = required_bytes(&header);
        if store.memory.size().saturating_mul(crate::WASM_PAGE_SIZE) < needed {
            return Err(InitError::OutOfMemory);
        }
        Ok(store)
    }

    /// Returns the backing memory.
    pub fn into_memory(self) -> M {
        self.memory
    }

    #[inline]
    /// Returns the cached inline-property-bytes-log header.
    pub fn header(&self) -> HeaderV1 {
        self.header_mirror.get()
    }

    pub(crate) fn read_idx_with_header(&self, h: &HeaderV1, leaf_segment: u32) -> i32 {
        read_i32(&self.memory, Address::from(idx_offset(h, leaf_segment)))
    }

    pub(crate) fn write_idx_with_header(&self, h: &HeaderV1, leaf_segment: u32, idx: i32) {
        write_i32(
            &self.memory,
            Address::from(idx_offset(h, leaf_segment)),
            idx,
        );
    }

    pub(crate) fn write_idx_at_least(&self, leaf_segment: u32, min_idx: i32) {
        let h = self.header();
        let cur = self.read_idx_with_header(&h, leaf_segment);
        if min_idx > cur {
            self.write_idx_with_header(&h, leaf_segment, min_idx);
        }
    }

    pub(crate) fn read_entry_with_header(
        &self,
        h: &HeaderV1,
        leaf_segment: u32,
        entry_idx: u32,
        out: &mut [u8],
    ) -> i32 {
        debug_assert!(
            out.len() >= INLINE_PROPERTY_BYTES_LOG_CELL_BYTES,
            "inline property bytes log read buffer too small"
        );
        let off = entry_offset(h, leaf_segment, entry_idx);
        let prev = read_i32(&self.memory, Address::from(off));
        self.memory
            .read(off + 4, &mut out[..INLINE_PROPERTY_BYTES_LOG_CELL_BYTES]);
        prev
    }

    pub(crate) fn write_entry_with_header(
        &self,
        h: &HeaderV1,
        leaf_segment: u32,
        entry_idx: u32,
        prev: i32,
        inline_property_bytes: &[u8; INLINE_PROPERTY_BYTES_LOG_CELL_BYTES],
    ) -> Result<(), GrowFailed> {
        let off = entry_offset(h, leaf_segment, entry_idx);
        let entry_len = INLINE_PROPERTY_BYTES_LOG_ENTRY_STRIDE;
        let mut bytes = [0u8; INLINE_LOG_ENTRY_BYTES];
        bytes[0..4].copy_from_slice(&prev.to_le_bytes());
        bytes[4..4 + INLINE_PROPERTY_BYTES_LOG_CELL_BYTES].copy_from_slice(inline_property_bytes);
        safe_write(&self.memory, off, &bytes[..entry_len])
    }

    /// Clears the inline property bytes overflow-log entries for `leaf_segment`.
    pub fn release_segment(&self, leaf_segment: u32) -> Result<(), GrowFailed> {
        let h = self.header();
        let idx = self.read_idx_with_header(&h, leaf_segment);
        let stride = INLINE_PROPERTY_BYTES_LOG_ENTRY_STRIDE;
        let zeros = [0u8; INLINE_LOG_ENTRY_BYTES];
        for i in 0..idx.max(0) as u32 {
            safe_write(
                &self.memory,
                entry_offset(&h, leaf_segment, i),
                &zeros[..stride],
            )?;
        }
        self.write_idx_with_header(&h, leaf_segment, 0);
        Ok(())
    }

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

    fn write_header(&self, h: &HeaderV1) -> Result<(), GrowFailed> {
        safe_write(&self.memory, 0, &h.magic)?;
        self.memory.write(3, &[h.version]);
        write_u32(&self.memory, Address::from(4), h.segment_count);
        write_u32(&self.memory, Address::from(8), h.max_log_entries);
        write_u32(&self.memory, Address::from(12), h.stride);
        write_zeroes(&self.memory, HEADER_RESERVED_OFFSET, HEADER_RESERVED_SIZE)?;
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
        let need = required_bytes(h);
        if need == 0 {
            return Ok(());
        }
        safe_write(&self.memory, need - 1, &[0])
    }
}

pub const INLINE_PROPERTY_BYTES_LOG_ENTRY_STRIDE: usize = 4 + INLINE_PROPERTY_BYTES_LOG_CELL_BYTES;

#[inline]
fn idx_offset(h: &HeaderV1, leaf_segment: u32) -> u64 {
    HEADER_SIZE + u64::from(leaf_segment) * segment_block_size(h)
}

#[inline]
fn segment_block_size(h: &HeaderV1) -> u64 {
    4 + u64::from(h.max_log_entries).saturating_mul(INLINE_PROPERTY_BYTES_LOG_ENTRY_STRIDE as u64)
}

#[inline]
fn entry_offset(h: &HeaderV1, leaf_segment: u32, entry_idx: u32) -> u64 {
    idx_offset(h, leaf_segment)
        .saturating_add(4)
        .saturating_add(
            u64::from(entry_idx).saturating_mul(INLINE_PROPERTY_BYTES_LOG_ENTRY_STRIDE as u64),
        )
}

#[inline]
fn required_bytes(h: &HeaderV1) -> u64 {
    HEADER_SIZE.saturating_add(u64::from(h.segment_count).saturating_mul(segment_block_size(h)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::vector_memory;

    #[test]
    fn init_rejects_backing_smaller_than_declared_layout() {
        // Corrupt the declared segment_count so the layout the header describes far exceeds the
        // backing memory; init must fail fast instead of trapping on a later offset read.
        let store =
            InlinePropertyBytesLogStore::new(vector_memory(), HeaderV1::new(2)).expect("store");
        let mem = store.into_memory();
        crate::write_u32(&mem, Address::from(4), u32::MAX);
        assert!(matches!(
            InlinePropertyBytesLogStore::init(mem),
            Err(InitError::OutOfMemory)
        ));
    }

    #[test]
    fn log_header_reserved_region_is_zeroed_and_reopens() {
        let mem = vector_memory();
        let store =
            InlinePropertyBytesLogStore::new(mem.clone(), HeaderV1::new(2)).expect("seed log");
        store.write_idx_at_least(0, 5);
        drop(store);

        let mut reserved = [0u8; HEADER_RESERVED_SIZE];
        mem.read(HEADER_RESERVED_OFFSET, &mut reserved);
        assert!(reserved.iter().all(|&byte| byte == 0));

        let reopened = InlinePropertyBytesLogStore::init(mem).expect("reopen");
        assert_eq!(reopened.header().segment_count, 2);
        assert_eq!(reopened.read_idx_with_header(&reopened.header(), 0), 5);
    }

    #[test]
    fn log_init_rejects_nonzero_reserved_byte() {
        let store =
            InlinePropertyBytesLogStore::new(vector_memory(), HeaderV1::new(2)).expect("store");
        let mem = store.into_memory();
        crate::write_u32(&mem, Address::from(HEADER_RESERVED_OFFSET), 1);
        assert!(matches!(
            InlinePropertyBytesLogStore::init(mem),
            Err(InitError::ReservedRegionNonZero)
        ));
    }
}
