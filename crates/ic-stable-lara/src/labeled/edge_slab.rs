//! Log-free edge slab used by the labeled CSR graph.

use crate::{
    GrowFailed, read_u32, read_u64, safe_write, traits::CsrEdge, types::Address, write_u32,
    write_u64,
};
use ic_stable_structures::Memory;
use std::{cell::Cell, fmt, marker::PhantomData};

pub const MAGIC: [u8; 3] = *b"LGE";
const LAYOUT_VERSION: u8 = 1;
const HEADER_SIZE: u64 = 64;
const ELEM_CAPACITY_OFFSET: u64 = 4;
const NUM_EDGES_OFFSET: u64 = 12;
const EDGE_STRIDE_OFFSET: u64 = 20;
const SLAB_OCCUPIED_TAIL_OFFSET: u64 = 24;

/// Persisted header for the labeled edge slab.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    pub magic: [u8; 3],
    pub version: u8,
    pub elem_capacity: u64,
    pub num_edges: u64,
    pub stride: u32,
    pub slab_occupied_tail: u64,
}

impl HeaderV1 {
    pub fn new(elem_capacity: u64, stride: u32) -> Self {
        Self {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            elem_capacity,
            num_edges: 0,
            stride,
            slab_occupied_tail: 0,
        }
    }
}

/// Errors returned when reopening a labeled edge slab.
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
            Self::BadMagic { actual } => write!(f, "bad labeled edge magic {actual:?}"),
            Self::IncompatibleVersion(v) => write!(f, "unsupported labeled edge version {v}"),
            Self::InvalidLayout => write!(f, "invalid labeled edge slab layout"),
            Self::OutOfMemory => write!(f, "failed to allocate labeled edge slab metadata"),
            Self::StrideMismatch { expected, actual } => {
                write!(
                    f,
                    "labeled edge stride mismatch: expected {expected}, got {actual}"
                )
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Stable storage for compact edge slots without overflow logs.
#[derive(Clone, Debug)]
pub struct EdgeSlabStore<E: CsrEdge, M: Memory> {
    memory: M,
    header: Cell<HeaderV1>,
    _marker: PhantomData<E>,
}

impl<E: CsrEdge, M: Memory> EdgeSlabStore<E, M> {
    pub fn new(memory: M, elem_capacity: u64) -> Result<Self, GrowFailed> {
        let header = HeaderV1::new(elem_capacity, E::BYTES as u32);
        let store = Self {
            memory,
            header: Cell::new(header),
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
            header: Cell::new(HeaderV1::new(0, 0)),
            _marker: PhantomData,
        };
        let header = store.read_header()?;
        if header.stride != E::BYTES as u32 {
            return Err(InitError::StrideMismatch {
                expected: E::BYTES as u32,
                actual: header.stride,
            });
        }
        store.header.set(header);
        Ok(store)
    }

    pub fn header(&self) -> HeaderV1 {
        self.header.get()
    }

    fn write_header(&self, header: &HeaderV1) {
        safe_write(&self.memory, 0, &header.magic).expect("edge header write failed");
        safe_write(&self.memory, 3, &[header.version]).expect("edge header write failed");
        write_u64(
            &self.memory,
            Address::from(ELEM_CAPACITY_OFFSET),
            header.elem_capacity,
        );
        write_u64(
            &self.memory,
            Address::from(NUM_EDGES_OFFSET),
            header.num_edges,
        );
        write_u32(
            &self.memory,
            Address::from(EDGE_STRIDE_OFFSET),
            header.stride,
        );
        write_u64(
            &self.memory,
            Address::from(SLAB_OCCUPIED_TAIL_OFFSET),
            header.slab_occupied_tail,
        );
        self.header.set(*header);
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
            num_edges: read_u64(&self.memory, Address::from(NUM_EDGES_OFFSET)),
            stride: read_u32(&self.memory, Address::from(EDGE_STRIDE_OFFSET)),
            slab_occupied_tail: read_u64(&self.memory, Address::from(SLAB_OCCUPIED_TAIL_OFFSET)),
        })
    }

    fn grow_for_header(&self, header: &HeaderV1) -> Result<(), GrowFailed> {
        let bytes = HEADER_SIZE.saturating_add(
            header
                .elem_capacity
                .saturating_mul(u64::from(header.stride)),
        );
        let page = 65_536u64;
        let needed_pages = bytes.saturating_add(page - 1) / page;
        while self.memory.size() < needed_pages {
            if self.memory.grow(1) == -1 {
                return Err(GrowFailed {
                    current_size: self.memory.size(),
                    delta: 1,
                });
            }
        }
        Ok(())
    }

    fn slot_offset(&self, slot: u64) -> u64 {
        HEADER_SIZE + slot.saturating_mul(E::BYTES as u64)
    }

    pub fn read_slot(&self, slot: u64) -> E {
        let mut bytes = [0u8; 64];
        let stride = E::BYTES;
        self.memory
            .read(self.slot_offset(slot), &mut bytes[..stride]);
        E::read_from(&bytes[..stride])
    }

    pub fn write_slot(&self, slot: u64, edge: E) -> Result<(), GrowFailed> {
        if slot >= self.header().elem_capacity {
            return Err(GrowFailed {
                current_size: self.memory.size(),
                delta: 0,
            });
        }
        let mut bytes = [0u8; 64];
        edge.write_to(&mut bytes[..E::BYTES]);
        safe_write(&self.memory, self.slot_offset(slot), &bytes[..E::BYTES])?;
        let mut header = self.header();
        let tail = slot.saturating_add(1);
        if tail > header.slab_occupied_tail {
            header.slab_occupied_tail = tail;
        }
        header.num_edges = header.num_edges.saturating_add(1);
        self.write_header(&header);
        Ok(())
    }

    pub fn allocate_slot(&self) -> Result<u64, GrowFailed> {
        let header = self.header();
        if header.slab_occupied_tail >= header.elem_capacity {
            return Err(GrowFailed {
                current_size: self.memory.size(),
                delta: 0,
            });
        }
        Ok(header.slab_occupied_tail)
    }

    pub fn grow_capacity(&self, new_capacity: u64) -> Result<(), GrowFailed> {
        let mut header = self.header();
        if new_capacity <= header.elem_capacity {
            return Ok(());
        }
        header.elem_capacity = new_capacity;
        self.grow_for_header(&header)?;
        self.write_header(&header);
        Ok(())
    }
}
