//! Per-segment overflow log for edge payload bytes (paired with edge overflow logs).

use crate::{GrowFailed, read_i32, read_u32, safe_write, types::Address, write_i32, write_u32};
use ic_stable_structures::Memory;
use std::{cell::Cell, fmt};

/// Magic bytes that identify a LARA payload overflow-log memory.
pub const MAGIC: [u8; 3] = *b"LVL";
/// Current overflow-log layout version.
pub const LAYOUT_VERSION: u8 = 2;
const HEADER_SIZE: u64 = 32;
const INLINE_LOG_ENTRY_BYTES: usize = 17;
/// Payload bytes per payload overflow log entry (`PayloadLogCell`).
pub const PAYLOAD_BYTES: usize = 9;

/// Default per-segment overflow-log capacity (matches edge log).
pub const DEFAULT_MAX_LOG_ENTRIES: u32 = 170;

/// Persisted V1 payload overflow-log header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    pub magic: [u8; 3],
    pub version: u8,
    pub segment_count: u32,
    pub max_log_entries: u32,
    pub stride: u32,
}

impl HeaderV1 {
    pub fn new(segment_count: u32) -> Self {
        Self {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            segment_count,
            max_log_entries: DEFAULT_MAX_LOG_ENTRIES,
            stride: PAYLOAD_LOG_ENTRY_STRIDE as u32,
        }
    }
}

/// Errors returned when reopening a persisted payload overflow log.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    BadMagic { actual: [u8; 3] },
    IncompatibleVersion(u8),
    OutOfMemory,
    StrideMismatch { expected: u32, actual: u32 },
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(f, "bad payload log magic {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(v) => write!(f, "unsupported payload log layout version {v}"),
            Self::OutOfMemory => write!(f, "failed to allocate payload log metadata"),
            Self::StrideMismatch { expected, actual } => {
                write!(
                    f,
                    "payload log entry stride mismatch: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Stable per-segment overflow log for values that did not fit on the byte slab.
#[derive(Clone, Debug)]
pub struct PayloadLogStore<M: Memory> {
    memory: M,
    header_mirror: Cell<HeaderV1>,
}

impl<M: Memory> PayloadLogStore<M> {
    pub fn new(memory: M, header: HeaderV1) -> Result<Self, GrowFailed> {
        let store = Self {
            memory,
            header_mirror: Cell::new(header),
        };
        store.grow_for_header(&header)?;
        store.write_header(&header)?;
        Ok(store)
    }

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
        let expected = PAYLOAD_LOG_ENTRY_STRIDE as u32;
        if header.stride != expected {
            return Err(InitError::StrideMismatch {
                expected,
                actual: header.stride,
            });
        }
        Ok(store)
    }

    pub fn into_memory(self) -> M {
        self.memory
    }

    #[inline]
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
    ) -> (i32, i32) {
        debug_assert!(
            out.len() >= PAYLOAD_BYTES,
            "payload log read buffer too small"
        );
        let off = entry_offset(h, leaf_segment, entry_idx);
        let prev = read_i32(&self.memory, Address::from(off));
        let src = read_i32(&self.memory, Address::from(off + 4));
        self.memory.read(off + 8, &mut out[..PAYLOAD_BYTES]);
        (prev, src)
    }

    pub(crate) fn write_entry_with_header(
        &self,
        h: &HeaderV1,
        leaf_segment: u32,
        entry_idx: u32,
        prev: i32,
        src: i32,
        payload: &[u8; PAYLOAD_BYTES],
    ) -> Result<(), GrowFailed> {
        let off = entry_offset(h, leaf_segment, entry_idx);
        let entry_len = PAYLOAD_LOG_ENTRY_STRIDE;
        let mut bytes = [0u8; INLINE_LOG_ENTRY_BYTES];
        bytes[0..4].copy_from_slice(&prev.to_le_bytes());
        bytes[4..8].copy_from_slice(&src.to_le_bytes());
        bytes[8..8 + PAYLOAD_BYTES].copy_from_slice(payload);
        safe_write(&self.memory, off, &bytes[..entry_len])
    }

    pub fn release_segment(&self, leaf_segment: u32) -> Result<(), GrowFailed> {
        let h = self.header();
        let idx = self.read_idx_with_header(&h, leaf_segment);
        let stride = PAYLOAD_LOG_ENTRY_STRIDE;
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
        let need = required_bytes(h);
        if need == 0 {
            return Ok(());
        }
        safe_write(&self.memory, need - 1, &[0])
    }
}

pub const PAYLOAD_LOG_ENTRY_STRIDE: usize = 8 + PAYLOAD_BYTES;

#[inline]
fn idx_offset(h: &HeaderV1, leaf_segment: u32) -> u64 {
    HEADER_SIZE + u64::from(leaf_segment) * segment_block_size(h)
}

#[inline]
fn segment_block_size(h: &HeaderV1) -> u64 {
    4 + u64::from(h.max_log_entries).saturating_mul(PAYLOAD_LOG_ENTRY_STRIDE as u64)
}

#[inline]
fn entry_offset(h: &HeaderV1, leaf_segment: u32, entry_idx: u32) -> u64 {
    idx_offset(h, leaf_segment)
        .saturating_add(4)
        .saturating_add(u64::from(entry_idx).saturating_mul(PAYLOAD_LOG_ENTRY_STRIDE as u64))
}

#[inline]
fn required_bytes(h: &HeaderV1) -> u64 {
    HEADER_SIZE.saturating_add(u64::from(h.segment_count).saturating_mul(segment_block_size(h)))
}
