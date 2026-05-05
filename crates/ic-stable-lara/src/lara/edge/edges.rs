//! Stable LARA CSR edge slab (`edges_`) plus graph-wide edge metadata.
//!
//! # V1 layout
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! Magic "LEG"                           ↕ 3 bytes
//! --------------------------------------------------
//! Layout version                        ↕ 1 byte
//! --------------------------------------------------
//! Element capacity                      ↕ 8 bytes
//! --------------------------------------------------
//! Number of leaf segments               ↕ 4 bytes
//! --------------------------------------------------
//! Segment size in vertices              ↕ 4 bytes
//! --------------------------------------------------
//! PMA tree height                       ↕ 4 bytes
//! --------------------------------------------------
//! Number of logical edges               ↕ 8 bytes
//! --------------------------------------------------
//! Edge slot stride                      ↕ 4 bytes
//! --------------------------------------------------
//! Slab occupied tail                    ↕ 8 bytes
//! --------------------------------------------------
//! Reserved                              ↕ 20 bytes
//! -------------------------------------------------- <- Address 64
//! E_0                                   ↕ E::BYTES bytes
//! --------------------------------------------------
//! E_1                                   ↕ E::BYTES bytes
//! --------------------------------------------------
//! ...
//! --------------------------------------------------
//! E_(elem_capacity-1)                   ↕ E::BYTES bytes
//! --------------------------------------------------
//! Unallocated space
//! ```

use crate::{
    GrowFailed, read_u32, read_u64, safe_write, traits::CsrEdge, types::Address, write_u32,
    write_u64,
};
use ic_stable_structures::Memory;
use std::{fmt, marker::PhantomData};

pub const MAGIC: [u8; 3] = *b"LEG";
pub const LAYOUT_VERSION: u8 = 1;
pub const HEADER_SIZE: u64 = 64;

const ELEM_CAPACITY_OFFSET: u64 = 4;
const SEGMENT_COUNT_OFFSET: u64 = 12;
const SEGMENT_SIZE_OFFSET: u64 = 16;
const TREE_HEIGHT_OFFSET: u64 = 20;
const NUM_EDGES_OFFSET: u64 = 24;
const EDGE_STRIDE_OFFSET: u64 = 32;
const SLAB_OCCUPIED_TAIL_OFFSET: u64 = 36;
const RESERVED_OFFSET: u64 = 44;
const RESERVED_SIZE: usize = 20;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    pub magic: [u8; 3],
    pub version: u8,
    pub elem_capacity: u64,
    pub segment_count: u32,
    pub segment_size: u32,
    pub tree_height: u32,
    pub num_edges: u64,
    pub stride: u32,
    pub slab_occupied_tail: u64,
}

impl HeaderV1 {
    pub fn new(elem_capacity: u64, segment_count: u32, segment_size: u32, stride: u32) -> Self {
        Self {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            elem_capacity,
            segment_count,
            segment_size,
            tree_height: floor_log2(segment_count.max(1)),
            num_edges: 0,
            stride,
            slab_occupied_tail: 0,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    BadMagic { actual: [u8; 3] },
    IncompatibleVersion(u8),
    InvalidLayout,
    OutOfMemory,
    StrideMismatch { expected: u32, actual: u32 },
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => write!(f, "bad edge magic {actual:?}, expected {MAGIC:?}"),
            Self::IncompatibleVersion(v) => write!(f, "unsupported edge layout version {v}"),
            Self::InvalidLayout => write!(f, "invalid edge slab layout"),
            Self::OutOfMemory => write!(f, "failed to allocate edge slab metadata"),
            Self::StrideMismatch { expected, actual } => {
                write!(f, "edge stride mismatch: expected {expected}, got {actual}")
            }
        }
    }
}

impl std::error::Error for InitError {}

#[derive(Clone, Debug)]
pub struct EdgeSlabStore<E: CsrEdge, M: Memory> {
    memory: M,
    _marker: PhantomData<E>,
}

impl<E: CsrEdge, M: Memory> EdgeSlabStore<E, M> {
    pub fn new(memory: M, header: HeaderV1) -> Result<Self, GrowFailed> {
        let store = Self {
            memory,
            _marker: PhantomData,
        };
        store.grow_for_header(&header)?;
        store.write_header(&header);
        Ok(store)
    }

    pub fn init(memory: M) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Err(InitError::InvalidLayout);
        }
        let store = Self {
            memory,
            _marker: PhantomData,
        };
        let header = store.read_header()?;
        if header.magic != MAGIC {
            return Err(InitError::BadMagic {
                actual: header.magic,
            });
        }
        if header.version != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(header.version));
        }
        if header.stride as usize != E::BYTES {
            return Err(InitError::StrideMismatch {
                expected: E::BYTES as u32,
                actual: header.stride,
            });
        }
        Ok(store)
    }

    pub fn into_memory(self) -> M {
        self.memory
    }

    pub fn header(&self) -> Result<HeaderV1, InitError> {
        self.read_header()
    }

    pub fn write_header(&self, h: &HeaderV1) {
        self.memory.write(0, &h.magic);
        self.memory.write(3, &[h.version]);
        write_u64(
            &self.memory,
            Address::from(ELEM_CAPACITY_OFFSET),
            h.elem_capacity,
        );
        write_u32(
            &self.memory,
            Address::from(SEGMENT_COUNT_OFFSET),
            h.segment_count,
        );
        write_u32(
            &self.memory,
            Address::from(SEGMENT_SIZE_OFFSET),
            h.segment_size,
        );
        write_u32(
            &self.memory,
            Address::from(TREE_HEIGHT_OFFSET),
            h.tree_height,
        );
        write_u64(&self.memory, Address::from(NUM_EDGES_OFFSET), h.num_edges);
        write_u32(&self.memory, Address::from(EDGE_STRIDE_OFFSET), h.stride);
        write_u64(
            &self.memory,
            Address::from(SLAB_OCCUPIED_TAIL_OFFSET),
            h.slab_occupied_tail,
        );
        self.memory.write(RESERVED_OFFSET, &[0u8; RESERVED_SIZE]);
    }

    pub fn set_num_edges(&self, n: u64) {
        write_u64(&self.memory, Address::from(NUM_EDGES_OFFSET), n);
    }
    pub fn set_elem_capacity(&self, n: u64) -> Result<(), GrowFailed> {
        let mut h = self.header().map_err(|_| GrowFailed {
            current_size: self.memory.size(),
            delta: 0,
        })?;
        h.elem_capacity = n;
        self.grow_for_header(&h)?;
        write_u64(&self.memory, Address::from(ELEM_CAPACITY_OFFSET), n);
        Ok(())
    }
    pub fn set_slab_occupied_tail(&self, n: u64) {
        write_u64(&self.memory, Address::from(SLAB_OCCUPIED_TAIL_OFFSET), n);
    }

    pub fn read_slot(&self, slot: u64, out: &mut [u8]) {
        self.memory.read(slot_offset::<E>(slot), out);
    }

    pub fn write_slot(&self, slot: u64, bytes: &[u8]) -> Result<(), GrowFailed> {
        debug_assert_eq!(bytes.len(), E::BYTES);
        safe_write(&self.memory, slot_offset::<E>(slot), bytes)
    }

    fn read_header(&self) -> Result<HeaderV1, InitError> {
        let mut magic = [0u8; 3];
        self.memory.read(0, &mut magic);
        if magic != MAGIC {
            return Err(InitError::BadMagic { actual: magic });
        }
        let mut version = [0u8; 1];
        self.memory.read(3, &mut version);
        if version[0] != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(version[0]));
        }
        Ok(HeaderV1 {
            magic,
            version: version[0],
            elem_capacity: read_u64(&self.memory, Address::from(ELEM_CAPACITY_OFFSET)),
            segment_count: read_u32(&self.memory, Address::from(SEGMENT_COUNT_OFFSET)),
            segment_size: read_u32(&self.memory, Address::from(SEGMENT_SIZE_OFFSET)),
            tree_height: read_u32(&self.memory, Address::from(TREE_HEIGHT_OFFSET)),
            num_edges: read_u64(&self.memory, Address::from(NUM_EDGES_OFFSET)),
            stride: read_u32(&self.memory, Address::from(EDGE_STRIDE_OFFSET)),
            slab_occupied_tail: read_u64(&self.memory, Address::from(SLAB_OCCUPIED_TAIL_OFFSET)),
        })
    }

    fn grow_for_header(&self, h: &HeaderV1) -> Result<(), GrowFailed> {
        let need = HEADER_SIZE + h.elem_capacity.saturating_mul(E::BYTES as u64);
        if need == 0 {
            return Ok(());
        }
        safe_write(&self.memory, need - 1, &[0])
    }
}

#[inline]
pub fn slot_offset<E: CsrEdge>(slot: u64) -> u64 {
    HEADER_SIZE + slot.saturating_mul(E::BYTES as u64)
}

#[inline]
fn floor_log2(x: u32) -> u32 {
    31 - x.leading_zeros()
}
