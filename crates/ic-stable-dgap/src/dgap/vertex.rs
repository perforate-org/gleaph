//! Stable DGAP vertex column.
//!
//! Each row mirrors DGAP's `vertex_element`: base slab index, degree, and
//! per-segment log head (`-1` when the whole neighborhood is on the slab).
//!
//! # V1 layout
//!
//! ```text
//! -------------------------------------------------- <- Address 0
//! Magic "DVX"                           ↕ 3 bytes
//! --------------------------------------------------
//! Layout version                        ↕ 1 byte
//! --------------------------------------------------
//! Number of vertices                    ↕ 8 bytes
//! --------------------------------------------------
//! Vertex row stride                     ↕ 4 bytes
//! --------------------------------------------------
//! Reserved                              ↕ 48 bytes
//! -------------------------------------------------- <- Address 64
//! V_0                                   ↕ V::BYTES bytes
//! --------------------------------------------------
//! V_1                                   ↕ V::BYTES bytes
//! --------------------------------------------------
//! ...
//! --------------------------------------------------
//! V_(len-1)                             ↕ V::BYTES bytes
//! --------------------------------------------------
//! Unallocated space
//! ```

use crate::{GrowFailed, read_u64, safe_write, traits::CsrVertex, types::Address, write_u64};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use std::{borrow::Cow, fmt};

pub const MAGIC: [u8; 3] = *b"DVX";
const LAYOUT_VERSION: u8 = 1;
const DATA_OFFSET: u64 = 64;
const LEN_OFFSET: u64 = 4;
const STRIDE_OFFSET: u64 = 12;

#[derive(Debug)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u64,
    stride: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    BadMagic { actual: [u8; 3] },
    IncompatibleVersion(u8),
    StrideMismatch { expected: u32, actual: u32 },
    VariableWidthVertex,
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(f, "bad vertex magic {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(v) => write!(f, "unsupported vertex layout version {v}"),
            Self::StrideMismatch { expected, actual } => {
                write!(
                    f,
                    "vertex stride mismatch: expected {expected}, got {actual}"
                )
            }
            Self::VariableWidthVertex => {
                write!(f, "DGAP vertices must use fixed-width Storable encoding")
            }
            Self::OutOfMemory => write!(f, "failed to allocate vertex metadata"),
        }
    }
}

impl std::error::Error for InitError {}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Vertex {
    pub base_slot_start: u64,
    pub degree: u32,
    pub log_head: i32,
}

impl CsrVertex for Vertex {
    const BYTES: usize = 16;

    fn base_slot_start(&self) -> u64 {
        self.base_slot_start
    }
    fn degree(&self) -> u32 {
        self.degree
    }
    fn with_base_slot_start(mut self, start: u64) -> Self {
        self.base_slot_start = start;
        self
    }
    fn with_degree(mut self, degree: u32) -> Self {
        self.degree = degree;
        self
    }
    fn log_head(self) -> i32 {
        self.log_head
    }
    fn with_log_head(mut self, idx: i32) -> Self {
        self.log_head = idx;
        self
    }
}

impl Storable for Vertex {
    const BOUND: Bound = Bound::Bounded {
        max_size: 16,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut b = [0u8; 16];
        b[0..8].copy_from_slice(&self.base_slot_start.to_le_bytes());
        b[8..12].copy_from_slice(&self.degree.to_le_bytes());
        b[12..16].copy_from_slice(&self.log_head.to_le_bytes());
        Cow::Owned(b.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<[u8]>) -> Self {
        let b = bytes.as_ref();
        let mut u = [0u8; 8];
        let mut d = [0u8; 4];
        let mut l = [0u8; 4];
        u.copy_from_slice(&b[0..8]);
        d.copy_from_slice(&b[8..12]);
        l.copy_from_slice(&b[12..16]);
        Self {
            base_slot_start: u64::from_le_bytes(u),
            degree: u32::from_le_bytes(d),
            log_head: i32::from_le_bytes(l),
        }
    }
}

#[derive(Clone, Debug)]
pub struct VertexStore<V: CsrVertex, M: Memory> {
    memory: M,
    _marker: std::marker::PhantomData<V>,
}

impl<V: CsrVertex, M: Memory> VertexStore<V, M> {
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        verify_vertex_width::<V>().expect("DGAP vertices must be fixed-width");
        let header = HeaderV1 {
            magic: MAGIC,
            version: LAYOUT_VERSION,
            len: 0,
            stride: V::BYTES as u32,
        };
        Self::write_header(&header, &memory)?;
        Ok(Self {
            memory,
            _marker: std::marker::PhantomData,
        })
    }

    pub fn init(memory: M) -> Result<Self, InitError> {
        verify_vertex_width::<V>()?;
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
        let expected_stride = V::BYTES as u32;
        if header.stride != expected_stride {
            return Err(InitError::StrideMismatch {
                expected: expected_stride,
                actual: header.stride,
            });
        }
        Ok(Self {
            memory,
            _marker: std::marker::PhantomData,
        })
    }

    pub fn len(&self) -> u64 {
        read_u64(&self.memory, Address::from(LEN_OFFSET))
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    pub fn into_memory(self) -> M {
        self.memory
    }

    pub fn get(&self, index: u64) -> V {
        assert!(index < self.len());
        let mut buf = vec![0u8; V::BYTES];
        self.memory.read(self.entry_offset(index), &mut buf);
        V::from_bytes(Cow::Owned(buf))
    }

    pub fn set(&self, index: u64, item: &V) {
        assert!(index < self.len());
        crate::write(
            &self.memory,
            self.entry_offset(index),
            &item.to_bytes_checked(),
        );
    }

    pub fn push(&self, item: V) -> Result<(), GrowFailed> {
        let len = self.len();
        safe_write(
            &self.memory,
            self.entry_offset(len),
            &item.to_bytes_checked(),
        )?;
        write_u64(&self.memory, Address::from(LEN_OFFSET), len + 1);
        Ok(())
    }

    fn entry_offset(&self, index: u64) -> u64 {
        DATA_OFFSET + V::BYTES as u64 * index
    }

    fn write_header(header: &HeaderV1, memory: &M) -> Result<(), GrowFailed> {
        safe_write(memory, 0, &header.magic)?;
        memory.write(3, &[header.version]);
        write_u64(memory, Address::from(LEN_OFFSET), header.len);
        crate::write_u32(memory, Address::from(STRIDE_OFFSET), header.stride);
        Ok(())
    }

    fn read_header(memory: &M) -> HeaderV1 {
        debug_assert!(memory.size() > 0);

        let mut magic = [0u8; 3];
        let mut version = [0u8; 1];
        memory.read(0, &mut magic);
        memory.read(3, &mut version);
        let len = read_u64(memory, Address::from(LEN_OFFSET));
        let stride = crate::read_u32(memory, Address::from(STRIDE_OFFSET));

        HeaderV1 {
            magic,
            version: version[0],
            len,
            stride,
        }
    }
}

fn verify_vertex_width<V: CsrVertex>() -> Result<(), InitError> {
    match V::BOUND {
        Bound::Bounded {
            max_size,
            is_fixed_size: true,
        } if max_size as usize == V::BYTES => Ok(()),
        _ => Err(InitError::VariableWidthVertex),
    }
}
