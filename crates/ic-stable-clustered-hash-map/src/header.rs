//! V1 header layout for the stable clustered hash map with a 128-byte metadata prefix.

use crate::memory::{read_u8, read_u32, read_u64, write_u8, write_u32, write_u64};
use ic_stable_structures::Memory;

/// Magic bytes for the stable clustered hash map.
pub const MAGIC: [u8; 3] = *b"CHM";
/// Current layout version.
pub const LAYOUT_VERSION: u8 = 1;
/// Current V1 metadata boundary. Entries begin immediately after this prefix.
pub const HEADER_SIZE: u64 = 128;
pub const DATA_OFFSET: u64 = HEADER_SIZE;
/// Start of the V1 metadata extension.
const HEADER_EXTENSION_OFFSET: u64 = 64;
const HEADER_EXTENSION_SIZE: usize = (HEADER_SIZE - HEADER_EXTENSION_OFFSET) as usize;

pub(crate) const VERSION_OFFSET: u64 = 3;
pub(crate) const LEN_OFFSET: u64 = 4;
pub(crate) const LOG2_BUCKETS_OFFSET: u64 = 12;
pub(crate) const KEY_SIZE_OFFSET: u64 = 13;
pub(crate) const VALUE_SIZE_OFFSET: u64 = 17;
pub(crate) const REMAP_END_OFFSET: u64 = 21;
pub(crate) const CAPACITY_OFFSET: u64 = 29;
pub(crate) const RESIZE_TARGET_CAPACITY_OFFSET: u64 = 37;
pub(crate) const RESIZE_CURSOR_OFFSET: u64 = 45;
pub(crate) const RESIZE_TARGET_LOG2_OFFSET: u64 = 53;
pub(crate) const RESIZE_STATE_OFFSET: u64 = 54;
pub(crate) const RESIZE_REMAP_START_OFFSET: u64 = 55;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub(crate) enum ResizeState {
    Settled = 0,
    Clearing = 1,
    Publishing = 2,
}

impl ResizeState {
    pub(crate) fn from_u8(value: u8) -> Option<Self> {
        match value {
            0 => Some(Self::Settled),
            1 => Some(Self::Clearing),
            2 => Some(Self::Publishing),
            _ => None,
        }
    }
}

/// Failure opening existing memory with [`crate::StableClusteredHashMap::init`].
#[derive(PartialEq, Eq, Debug)]
pub enum InitError {
    /// First three bytes are not magic `CHM`. Use [`crate::StableClusteredHashMap::new`] to overwrite.
    BadMagic { actual: [u8; 3] },
    /// Persisted layout version is not supported by this crate.
    IncompatibleVersion(u8),
    /// `K`/`V`'s [`Storable`](ic_stable_structures::Storable) sizes do not match the header.
    IncompatibleElementType,
    /// `len`, `log2_buckets`, or allocated memory size are inconsistent.
    InvalidLayout,
    /// Empty memory and [`crate::StableClusteredHashMap::new`] failed (e.g. could not grow for the header).
    OutOfMemory,
}

impl std::fmt::Display for InitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(f, "bad magic number {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(version) => write!(
                f,
                "unsupported layout version {version}; supported version numbers are 1..={LAYOUT_VERSION}"
            ),
            Self::IncompatibleElementType => write!(
                f,
                "the fixed sizes of the key/value types do not match the persisted header"
            ),
            Self::InvalidLayout => write!(f, "invalid clustered hash map layout"),
            Self::OutOfMemory => write!(f, "failed to allocate memory for the clustered hash map"),
        }
    }
}

impl std::error::Error for InitError {}

/// The persisted header fields of a stable clustered hash map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub len: u64,
    pub log2_buckets: u8,
    pub key_size: u32,
    pub value_size: u32,
    /// Boundary of the in-place incremental resize's mixed range; `u64::MAX` = no resize in progress.
    pub remap_end: u64,
    /// Logical number of allocated entry slots. This is the capacity source of truth.
    pub capacity: u64,
    pub resize_target_capacity: u64,
    pub resize_cursor: u64,
    pub resize_target_log2: u8,
    pub resize_state: ResizeState,
    pub resize_remap_start: u64,
}

/// Reads and validates the header at the start of `memory`.
pub(crate) fn read_header<M: Memory>(m: &M) -> Result<Header, InitError> {
    if m.size() == 0 {
        return Err(InitError::BadMagic { actual: [0u8; 3] });
    }
    let mut magic = [0u8; 3];
    m.read(0, &mut magic);
    if magic != MAGIC {
        return Err(InitError::BadMagic { actual: magic });
    }
    let version = read_u32(m, VERSION_OFFSET) as u8;
    if version != LAYOUT_VERSION {
        return Err(InitError::IncompatibleVersion(version));
    }
    Ok(Header {
        len: read_u64(m, LEN_OFFSET),
        log2_buckets: read_u8(m, LOG2_BUCKETS_OFFSET),
        key_size: read_u32(m, KEY_SIZE_OFFSET),
        value_size: read_u32(m, VALUE_SIZE_OFFSET),
        remap_end: read_u64(m, REMAP_END_OFFSET),
        capacity: read_u64(m, CAPACITY_OFFSET),
        resize_target_capacity: read_u64(m, RESIZE_TARGET_CAPACITY_OFFSET),
        resize_cursor: read_u64(m, RESIZE_CURSOR_OFFSET),
        resize_target_log2: read_u8(m, RESIZE_TARGET_LOG2_OFFSET),
        resize_state: ResizeState::from_u8(read_u8(m, RESIZE_STATE_OFFSET))
            .ok_or(InitError::InvalidLayout)?,
        resize_remap_start: read_u64(m, RESIZE_REMAP_START_OFFSET),
    })
}

/// Writes a fresh header (len 0) with the given table geometry and element sizes.
pub(crate) fn write_header<M: Memory>(
    m: &M,
    log2_buckets: u8,
    key_size: u32,
    value_size: u32,
    capacity: u64,
) {
    m.write(0, &MAGIC);
    write_u32(m, VERSION_OFFSET, LAYOUT_VERSION as u32);
    write_u64(m, LEN_OFFSET, 0);
    write_u8(m, LOG2_BUCKETS_OFFSET, log2_buckets);
    write_u32(m, KEY_SIZE_OFFSET, key_size);
    write_u32(m, VALUE_SIZE_OFFSET, value_size);
    write_u64(m, REMAP_END_OFFSET, u64::MAX);
    write_u64(m, CAPACITY_OFFSET, capacity);
    write_u64(m, RESIZE_TARGET_CAPACITY_OFFSET, 0);
    write_u64(m, RESIZE_CURSOR_OFFSET, 0);
    write_u8(m, RESIZE_TARGET_LOG2_OFFSET, 0);
    write_u8(m, RESIZE_STATE_OFFSET, ResizeState::Settled as u8);
    write_u64(m, RESIZE_REMAP_START_OFFSET, 0);
    m.write(HEADER_EXTENSION_OFFSET, &[0; HEADER_EXTENSION_SIZE]);
}
