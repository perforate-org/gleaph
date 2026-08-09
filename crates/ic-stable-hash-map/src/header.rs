//! V1 header layout for the stable hash map, following the `ic-stable-structures` 64-byte header
//! prefix convention (3-byte magic + 1-byte version + metadata, data from byte 64).

use crate::memory::{read_u32, read_u64, write_u32, write_u64};
use ic_stable_structures::Memory;

/// Magic bytes for the stable hash map.
pub const MAGIC: [u8; 3] = *b"SHM";
/// Current layout version.
pub const LAYOUT_VERSION: u8 = 1;
/// Data (the table) starts at byte 64, matching the `ic-stable-structures` header prefix.
pub const DATA_OFFSET: u64 = 64;

pub(crate) const VERSION_OFFSET: u64 = 3;
pub(crate) const LEN_OFFSET: u64 = 4;
pub(crate) const CAP_OFFSET: u64 = 12;
pub(crate) const KEY_SIZE_OFFSET: u64 = 20;
pub(crate) const VALUE_SIZE_OFFSET: u64 = 24;

/// Failure opening existing memory with [`crate::StableHashMap::init`].
#[derive(PartialEq, Eq, Debug)]
pub enum InitError {
    /// First three bytes are not magic `SHM`. Use [`crate::StableHashMap::new`] to overwrite.
    BadMagic { actual: [u8; 3] },
    /// Persisted layout version is not supported by this crate.
    IncompatibleVersion(u8),
    /// `K`/`V`'s [`Storable`](ic_stable_structures::Storable) sizes do not match the header.
    IncompatibleElementType,
    /// `len`, `capacity`, or allocated memory size are inconsistent.
    InvalidLayout,
    /// Empty memory and [`crate::StableHashMap::new`] failed (e.g. could not grow for the header).
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
            Self::InvalidLayout => write!(f, "invalid hash map layout"),
            Self::OutOfMemory => write!(f, "failed to allocate memory for the hash map"),
        }
    }
}

impl std::error::Error for InitError {}

/// The persisted header fields of a stable hash map.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub len: u64,
    pub capacity: u64,
    pub key_size: u32,
    pub value_size: u32,
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
        capacity: read_u64(m, CAP_OFFSET),
        key_size: read_u32(m, KEY_SIZE_OFFSET),
        value_size: read_u32(m, VALUE_SIZE_OFFSET),
    })
}

/// Writes a fresh header (len 0) with the given capacity and element sizes.
pub(crate) fn write_header<M: Memory>(m: &M, capacity: u64, key_size: u32, value_size: u32) {
    m.write(0, &MAGIC);
    write_u32(m, VERSION_OFFSET, LAYOUT_VERSION as u32);
    write_u64(m, LEN_OFFSET, 0);
    write_u64(m, CAP_OFFSET, capacity);
    write_u32(m, KEY_SIZE_OFFSET, key_size);
    write_u32(m, VALUE_SIZE_OFFSET, value_size);
}
