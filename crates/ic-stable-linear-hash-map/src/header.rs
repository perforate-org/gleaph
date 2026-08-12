use crate::memory::{read_u32, read_u64, write_u32, write_u64};
use ic_stable_structures::Memory;
use std::fmt;

pub const HEADER_SIZE: u64 = 64;
pub const CONTROL_BYTES: u64 = 64;
pub const MAGIC: [u8; 3] = *b"LHM";
pub const LAYOUT_VERSION: u8 = 1;

const VERSION_OFFSET: u64 = 3;
const KEY_SIZE_OFFSET: u64 = 4;
const VALUE_SIZE_OFFSET: u64 = 8;
const BUCKET_SIZE_OFFSET: u64 = 12;
const CONTROL_OFFSET_OFFSET: u64 = 16;
const CONTROL_BYTES_OFFSET: u64 = 24;
const JOURNAL_OFFSET_OFFSET: u64 = 32;
const BUCKETS_OFFSET_OFFSET: u64 = 40;
const VALUE_SLAB_OFFSET_OFFSET: u64 = 48;
const BUCKET_PAGE_STRIDE_OFFSET: u64 = 56;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub key_size: u32,
    pub value_size: u32,
    pub bucket_size: u32,
    pub control_offset: u64,
    pub control_bytes: u64,
    pub journal_offset: u64,
    pub buckets_offset: u64,
    pub value_slab_offset: u64,
    pub bucket_page_stride: u64,
}

impl Header {
    pub fn journal_bytes(self) -> Option<u64> {
        8u64.checked_add(u64::from(self.key_size))?
            .checked_add(u64::from(self.value_size))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlRegion {
    pub len: u64,
    pub level: u8,
    pub split_cursor: u64,
    pub physical_buckets: u64,
    pub hash_seed: u64,
    pub split_state: u8,
    pub split_work_cursor: u64,
    pub journal_state: u8,
    pub mutation_epoch: u64,
    pub hash_encoding_id: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InitError {
    BadMagic { actual: [u8; 3] },
    IncompatibleVersion(u8),
    IncompatibleElementType,
    IncompatibleHashEncoding,
    InvalidLayout,
    RecoveryRequired,
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => write!(f, "bad magic {actual:?}, expected {MAGIC:?}"),
            Self::IncompatibleVersion(version) => {
                write!(f, "unsupported linear hash map layout version {version}")
            }
            Self::IncompatibleElementType => write!(f, "incompatible fixed-size key/value type"),
            Self::IncompatibleHashEncoding => {
                write!(f, "incompatible stable hash key encoding")
            }
            Self::InvalidLayout => write!(f, "invalid linear hash map layout"),
            Self::RecoveryRequired => {
                write!(
                    f,
                    "non-idle split, journal, or mutation state requires recovery"
                )
            }
            Self::OutOfMemory => write!(f, "failed to allocate linear hash map memory"),
        }
    }
}

impl std::error::Error for InitError {}

pub(crate) fn read<M: Memory>(memory: &M) -> Result<Header, InitError> {
    let mut magic = [0; 3];
    memory.read(0, &mut magic);
    if magic != MAGIC {
        return Err(InitError::BadMagic { actual: magic });
    }
    let mut version = [0];
    memory.read(VERSION_OFFSET, &mut version);
    if version[0] != LAYOUT_VERSION {
        return Err(InitError::IncompatibleVersion(version[0]));
    }
    Ok(Header {
        key_size: read_u32(memory, KEY_SIZE_OFFSET),
        value_size: read_u32(memory, VALUE_SIZE_OFFSET),
        bucket_size: read_u32(memory, BUCKET_SIZE_OFFSET),
        control_offset: read_u64(memory, CONTROL_OFFSET_OFFSET),
        control_bytes: read_u64(memory, CONTROL_BYTES_OFFSET),
        journal_offset: read_u64(memory, JOURNAL_OFFSET_OFFSET),
        buckets_offset: read_u64(memory, BUCKETS_OFFSET_OFFSET),
        value_slab_offset: read_u64(memory, VALUE_SLAB_OFFSET_OFFSET),
        bucket_page_stride: read_u64(memory, BUCKET_PAGE_STRIDE_OFFSET),
    })
}

pub(crate) fn write<M: Memory>(memory: &M, header: Header) {
    memory.write(0, &MAGIC);
    memory.write(VERSION_OFFSET, &[LAYOUT_VERSION]);
    write_u32(memory, KEY_SIZE_OFFSET, header.key_size);
    write_u32(memory, VALUE_SIZE_OFFSET, header.value_size);
    write_u32(memory, BUCKET_SIZE_OFFSET, header.bucket_size);
    write_u64(memory, CONTROL_OFFSET_OFFSET, header.control_offset);
    write_u64(memory, CONTROL_BYTES_OFFSET, header.control_bytes);
    write_u64(memory, JOURNAL_OFFSET_OFFSET, header.journal_offset);
    write_u64(memory, BUCKETS_OFFSET_OFFSET, header.buckets_offset);
    write_u64(memory, VALUE_SLAB_OFFSET_OFFSET, header.value_slab_offset);
    write_u64(memory, BUCKET_PAGE_STRIDE_OFFSET, header.bucket_page_stride);
}
