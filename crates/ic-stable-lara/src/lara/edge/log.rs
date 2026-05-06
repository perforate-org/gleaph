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
use std::{fmt, marker::PhantomData};

pub const MAGIC: [u8; 3] = *b"LLG";
pub const LAYOUT_VERSION: u8 = 1;
const HEADER_SIZE: u64 = 32;
const INLINE_LOG_ENTRY_BYTES: usize = 128;

pub const DEFAULT_MAX_LOG_ENTRIES: u32 = 170;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    pub magic: [u8; 3],
    pub version: u8,
    pub segment_count: u32,
    pub max_log_entries: u32,
    pub stride: u32,
}

impl HeaderV1 {
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

#[derive(Clone, Debug)]
pub struct LogStore<E: CsrEdge, M: Memory> {
    memory: M,
    _marker: PhantomData<E>,
}

impl<E: CsrEdge, M: Memory> LogStore<E, M> {
    pub fn new(memory: M, header: HeaderV1) -> Result<Self, GrowFailed> {
        let store = Self {
            memory,
            _marker: PhantomData,
        };
        store.grow_for_header(&header)?;
        store.write_header(&header)?;
        Ok(store)
    }

    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Err(InitError::OutOfMemory);
        }
        let store = Self {
            memory,
            _marker: PhantomData,
        };
        let header = store.read_header();
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

    pub fn into_memory(self) -> M {
        self.memory
    }

    pub fn header(&self) -> HeaderV1 {
        self.read_header()
    }

    pub fn read_idx(&self, leaf_segment: u32) -> i32 {
        let h = self.header();
        read_i32(
            &self.memory,
            Address::from(idx_offset::<E>(&h, leaf_segment)),
        )
    }

    pub fn write_idx(&self, leaf_segment: u32, idx: i32) {
        let h = self.header();
        write_i32(
            &self.memory,
            Address::from(idx_offset::<E>(&h, leaf_segment)),
            idx,
        );
    }

    pub fn read_entry(&self, leaf_segment: u32, entry_idx: u32, out: &mut [u8]) -> (i32, i32) {
        let h = self.header();
        let off = entry_offset::<E>(&h, leaf_segment, entry_idx);
        let prev = read_i32(&self.memory, Address::from(off));
        let src = read_i32(&self.memory, Address::from(off + 4));
        self.memory.read(off + 8, out);
        (prev, src)
    }

    pub fn write_entry(
        &self,
        leaf_segment: u32,
        entry_idx: u32,
        prev: i32,
        src: i32,
        payload: &[u8],
    ) -> Result<(), GrowFailed> {
        let h = self.header();
        let off = entry_offset::<E>(&h, leaf_segment, entry_idx);
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

    pub fn release_segment(&self, leaf_segment: u32) -> Result<(), GrowFailed> {
        let h = self.header();
        let idx = self.read_idx(leaf_segment);
        for i in 0..idx.max(0) as u32 {
            safe_write(
                &self.memory,
                entry_offset::<E>(&h, leaf_segment, i),
                &vec![0u8; log_entry_stride::<E>() as usize],
            )?;
        }
        self.write_idx(leaf_segment, 0);
        Ok(())
    }

    fn write_header(&self, h: &HeaderV1) -> Result<(), GrowFailed> {
        safe_write(&self.memory, 0, &h.magic)?;
        self.memory.write(3, &[h.version]);
        write_u32(&self.memory, Address::from(4), h.segment_count);
        write_u32(&self.memory, Address::from(8), h.max_log_entries);
        write_u32(&self.memory, Address::from(12), h.stride);
        self.memory.write(16, &[0u8; 16]);
        Ok(())
    }

    fn read_header(&self) -> HeaderV1 {
        let mut magic = [0u8; 3];
        let mut version = [0u8; 1];
        self.memory.read(0, &mut magic);
        self.memory.read(3, &mut version);
        HeaderV1 {
            magic,
            version: version[0],
            segment_count: read_u32(&self.memory, Address::from(4)),
            max_log_entries: read_u32(&self.memory, Address::from(8)),
            stride: read_u32(&self.memory, Address::from(12)),
        }
    }

    fn grow_for_header(&self, h: &HeaderV1) -> Result<(), GrowFailed> {
        let need = required_bytes::<E>(h);
        if need == 0 {
            return Ok(());
        }
        safe_write(&self.memory, need - 1, &[0])
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

#[inline]
pub fn log_entry_stride<E: CsrEdge>() -> u64 {
    8 + E::BYTES as u64
}

#[cfg(feature = "canbench")]
mod bench {
    use std::hint::black_box;

    use canbench_rs::bench;

    use super::{HeaderV1, LogStore};
    use crate::{
        bench as helper,
        test_support::{TestEdge, vector_memory},
        traits::CsrEdge,
    };

    /// Measures per-segment log entry writes, reads, index updates, and one
    /// segment release. This is the storage-layer baseline for overflow edges
    /// before graph maintenance folds them back into the slab.
    #[bench(raw)]
    fn bench_lara_edge_log_write_read_release_1024() -> canbench_rs::BenchResult {
        let store =
            LogStore::<TestEdge, _>::new(vector_memory(), HeaderV1::new(16, 4)).expect("log store");
        canbench_rs::bench_fn(|| {
            let _scope = canbench_rs::bench_scope("lara_edge_log_write_read_release");
            let mut payload = [0u8; TestEdge::BYTES];
            for i in 0..helper::MEDIUM_N {
                let segment = (i % 16) as u32;
                let entry = (i / 16) as u32;
                helper::test_edge(i).write_to(&mut payload);
                store
                    .write_entry(segment, entry, entry as i32 - 1, i as i32, &payload)
                    .expect("write log entry");
                store.write_idx(segment, entry as i32 + 1);
            }
            let mut sum = 0i32;
            for i in 0..helper::MEDIUM_N {
                let segment = (i % 16) as u32;
                let entry = (i / 16) as u32;
                let (prev, src) = store.read_entry(segment, entry, &mut payload);
                sum ^= prev ^ src ^ TestEdge::read_from(&payload).0 as i32;
            }
            store.release_segment(0).expect("release segment");
            black_box(sum);
        })
    }
}
