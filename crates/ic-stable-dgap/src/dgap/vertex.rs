//! Stable DGAP vertex column.
//!
//! Each row mirrors DGAP's `vertex_element`: base slab index, degree, and
//! per-segment log head (`-1` when the whole neighborhood is on the slab).

use crate::{GrowFailed, read_u64, safe_write, traits::CsrVertex, types::Address, write_u64};
use ic_stable_structures::{Memory, Storable, storable::Bound};
use std::{borrow::Cow, fmt};

pub const MAGIC: [u8; 3] = *b"DVX";
const LAYOUT_VERSION: u8 = 1;
const DATA_OFFSET: u64 = 64;
const LEN_OFFSET: u64 = 4;
const STRIDE_OFFSET: u64 = 12;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    BadMagic { actual: [u8; 3] },
    IncompatibleVersion(u8),
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
    stride: u32,
    _marker: std::marker::PhantomData<V>,
}

impl<V: CsrVertex, M: Memory> VertexStore<V, M> {
    pub fn new(memory: M) -> Result<Self, GrowFailed> {
        let stride = fixed_stride::<V>().expect("DGAP vertices must be fixed-width");
        safe_write(&memory, 0, &MAGIC)?;
        memory.write(3, &[LAYOUT_VERSION]);
        write_u64(&memory, Address::from(LEN_OFFSET), 0);
        crate::write_u32(&memory, Address::from(STRIDE_OFFSET), stride);
        Ok(Self {
            memory,
            stride,
            _marker: std::marker::PhantomData,
        })
    }

    pub fn init(memory: M) -> Result<Self, InitError> {
        let stride = fixed_stride::<V>().ok_or(InitError::VariableWidthVertex)?;
        if memory.size() == 0 {
            return Self::new(memory).map_err(|_| InitError::OutOfMemory);
        }
        let mut magic = [0u8; 3];
        memory.read(0, &mut magic);
        if magic != MAGIC {
            return Err(InitError::BadMagic { actual: magic });
        }
        let mut version = [0u8; 1];
        memory.read(3, &mut version);
        if version[0] != LAYOUT_VERSION {
            return Err(InitError::IncompatibleVersion(version[0]));
        }
        Ok(Self {
            memory,
            stride,
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
        let mut buf = vec![0u8; self.stride as usize];
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
        DATA_OFFSET + u64::from(self.stride) * index
    }
}

fn fixed_stride<V: Storable>() -> Option<u32> {
    match V::BOUND {
        Bound::Bounded {
            max_size,
            is_fixed_size: true,
        } => Some(max_size),
        _ => None,
    }
}
