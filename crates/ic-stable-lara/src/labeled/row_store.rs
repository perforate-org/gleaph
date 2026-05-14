//! Generic fixed-width row column used by labeled CSR stores.

use crate::{GrowFailed, VertexId, safe_write, types::Address, write_u32};
use ic_stable_structures::{Memory, Storable};
use std::{cell::Cell, fmt, marker::PhantomData};

const DATA_OFFSET: u64 = 64;
const LEN_OFFSET: u64 = 4;
const STRIDE_OFFSET: u64 = 8;

#[derive(Clone, Copy, Debug)]
struct HeaderV1 {
    magic: [u8; 3],
    version: u8,
    len: u32,
    stride: u32,
}

/// Errors returned when reopening a labeled row column.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum InitError {
    /// The memory header does not contain the expected magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The stored layout version is not supported.
    IncompatibleVersion(u8),
    /// The persisted row width does not match the row type `R`.
    StrideMismatch {
        /// Expected row width.
        expected: u32,
        /// Row width read from stable memory.
        actual: u32,
    },
    /// The row type does not use a fixed-width [`Storable`] encoding.
    VariableWidthRow,
    /// The store could not allocate its header.
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => write!(f, "bad row magic {actual:?}"),
            Self::IncompatibleVersion(v) => write!(f, "unsupported row layout version {v}"),
            Self::StrideMismatch { expected, actual } => {
                write!(f, "row stride mismatch: expected {expected}, got {actual}")
            }
            Self::VariableWidthRow => {
                write!(f, "labeled rows must use fixed-width Storable encoding")
            }
            Self::OutOfMemory => write!(f, "failed to allocate row metadata"),
        }
    }
}

impl std::error::Error for InitError {}

/// Stable column of fixed-width labeled CSR rows.
#[derive(Clone, Debug)]
pub struct RowStore<R, M>
where
    R: Storable,
    M: Memory,
{
    memory: M,
    header: Cell<HeaderV1>,
    _marker: PhantomData<R>,
}

impl<R, M> RowStore<R, M>
where
    R: Storable,
    M: Memory,
{
    fn row_stride() -> Result<u32, InitError> {
        match R::BOUND {
            ic_stable_structures::storable::Bound::Bounded {
                max_size,
                is_fixed_size: true,
            } => Ok(max_size),
            _ => Err(InitError::VariableWidthRow),
        }
    }

    fn write_header(&self, header: &HeaderV1) -> Result<(), GrowFailed> {
        safe_write(&self.memory, 0, &header.magic)?;
        safe_write(&self.memory, 3, &[header.version])?;
        write_u32(&self.memory, Address::from(LEN_OFFSET), header.len);
        write_u32(&self.memory, Address::from(STRIDE_OFFSET), header.stride);
        self.header.set(*header);
        Ok(())
    }

    fn read_header(memory: &M, magic: [u8; 3]) -> Result<HeaderV1, InitError> {
        let mut actual = [0u8; 3];
        memory.read(0, &mut actual);
        if actual != magic {
            return Err(InitError::BadMagic { actual });
        }
        let mut version = [0u8; 1];
        memory.read(3, &mut version);
        if version[0] != 1 {
            return Err(InitError::IncompatibleVersion(version[0]));
        }
        let mut len = [0u8; 4];
        memory.read(LEN_OFFSET, &mut len);
        let mut stride = [0u8; 4];
        memory.read(STRIDE_OFFSET, &mut stride);
        Ok(HeaderV1 {
            magic,
            version: version[0],
            len: u32::from_le_bytes(len),
            stride: u32::from_le_bytes(stride),
        })
    }

    /// Creates a fresh row column with the supplied magic bytes.
    pub fn new(memory: M, magic: [u8; 3]) -> Result<Self, GrowFailed> {
        let stride = Self::row_stride().map_err(|_| GrowFailed {
            current_size: 0,
            delta: 0,
        })?;
        let store = Self {
            memory,
            header: Cell::new(HeaderV1 {
                magic,
                version: 1,
                len: 0,
                stride,
            }),
            _marker: PhantomData,
        };
        store.write_header(&HeaderV1 {
            magic,
            version: 1,
            len: 0,
            stride,
        })?;
        Ok(store)
    }

    /// Reopens an existing row column from stable memory.
    pub fn init(memory: M, magic: [u8; 3]) -> Result<Self, InitError> {
        if memory.size() == 0 {
            return Self::new(memory, magic).map_err(|_| InitError::OutOfMemory);
        }
        let header = Self::read_header(&memory, magic)?;
        let expected = Self::row_stride()?;
        if header.stride != expected {
            return Err(InitError::StrideMismatch {
                expected,
                actual: header.stride,
            });
        }
        Ok(Self {
            memory,
            header: Cell::new(header),
            _marker: PhantomData,
        })
    }

    /// Returns the number of rows in the column.
    pub fn len(&self) -> u32 {
        self.header.get().len
    }

    fn row_offset(&self, index: u32) -> u64 {
        DATA_OFFSET + u64::from(index) * u64::from(self.header.get().stride)
    }

    fn grow_for_index(&self, index: u32) -> Result<(), GrowFailed> {
        let stride = u64::from(self.header.get().stride);
        let end = self
            .row_offset(index)
            .saturating_add(stride)
            .saturating_sub(1);
        let page = 65_536u64;
        let needed_pages = end
            .checked_add(page)
            .expect("address overflow")
            .div_ceil(page);
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

    /// Reads one row.
    pub fn get(&self, id: VertexId) -> R {
        let index = u32::from(id);
        let stride = self.header.get().stride as usize;
        let mut bytes = vec![0u8; stride];
        self.memory.read(self.row_offset(index), &mut bytes);
        R::from_bytes(bytes.into())
    }

    /// Writes one row.
    pub fn set(&self, id: VertexId, item: &R) {
        let index = u32::from(id);
        self.grow_for_index(index).expect("row grow failed");
        let bytes = item.to_bytes();
        let offset = self.row_offset(index);
        safe_write(&self.memory, offset, &bytes).expect("row write failed");
        if index >= self.header.get().len {
            let mut header = self.header.get();
            header.len = index + 1;
            self.write_header(&header).expect("row header write failed");
        }
    }

    /// Appends one row and returns its global index.
    pub fn push(&self, item: R) -> Result<VertexId, GrowFailed> {
        let id = VertexId::from(self.len());
        self.grow_for_index(u32::from(id))?;
        let bytes = item.to_bytes();
        let offset = self.row_offset(u32::from(id));
        safe_write(&self.memory, offset, &bytes)?;
        let mut header = self.header.get();
        header.len = header.len.saturating_add(1);
        self.write_header(&header)?;
        Ok(id)
    }
}
