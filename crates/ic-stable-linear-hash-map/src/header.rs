use ic_stable_structures::Memory;
use std::fmt;

pub const HEADER_SIZE: u64 = 128;
pub const CONTROL_BYTES: u64 = 64;
pub(crate) const CONTROL_OFFSET: u64 = HEADER_SIZE;
pub(crate) const BUCKETS_OFFSET: u64 = HEADER_SIZE + CONTROL_BYTES;
pub const MAGIC: [u8; 3] = *b"LHM";
pub const LAYOUT_VERSION: u8 = 1;

const KEY_SIZE_OFFSET: usize = 4;
const VALUE_SIZE_OFFSET: usize = 8;
const KEY_STORAGE_SCHEMA_ID_OFFSET: usize = 16;
const KEY_ROUTING_SCHEMA_ID_OFFSET: usize = 32;
const VALUE_STORAGE_SCHEMA_ID_OFFSET: usize = 48;
const HASH_SEED_OFFSET: usize = 64;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
    pub key_size: u32,
    pub value_size: u32,
    pub key_storage_schema_id: [u8; 16],
    pub key_routing_schema_id: [u8; 16],
    pub value_storage_schema_id: [u8; 16],
    pub hash_seed: u64,
    pub(crate) bucket_size: u32,
    pub(crate) control_offset: u64,
    pub(crate) control_bytes: u64,
    pub(crate) buckets_offset: u64,
    pub(crate) value_slab_offset: u64,
    pub(crate) bucket_page_stride: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ControlRegion {
    pub len: u64,
    pub physical_buckets: u64,
    pub mutation_epoch: u64,
    pub incarnation: u64,
    pub backward_relocation_generation: u64,
    pub(crate) level: u8,
    pub(crate) split_cursor: u64,
    pub(crate) hash_seed: u64,
}

#[derive(Debug, PartialEq, Eq)]
pub enum InitError {
    NonEmptyMemory,
    BadMagic { actual: [u8; 3] },
    IncompatibleVersion(u8),
    IncompatibleElementType,
    IncompatibleKeyStorageSchema,
    IncompatibleKeyRoutingSchema,
    IncompatibleValueStorageSchema,
    InvalidLayout,
    RecoveryRequired,
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NonEmptyMemory => write!(f, "strict create requires zero-sized memory"),
            Self::BadMagic { actual } => write!(f, "bad magic {actual:?}, expected {MAGIC:?}"),
            Self::IncompatibleVersion(version) => {
                write!(f, "unsupported linear hash map layout version {version}")
            }
            Self::IncompatibleElementType => write!(f, "incompatible fixed-size key/value type"),
            Self::IncompatibleKeyStorageSchema => {
                write!(f, "incompatible stable key storage schema")
            }
            Self::IncompatibleKeyRoutingSchema => {
                write!(f, "incompatible stable key routing schema")
            }
            Self::IncompatibleValueStorageSchema => {
                write!(f, "incompatible stable value storage schema")
            }
            Self::InvalidLayout => write!(f, "invalid linear hash map layout"),
            Self::RecoveryRequired => write!(f, "odd mutation epoch requires recovery"),
            Self::OutOfMemory => write!(f, "failed to allocate linear hash map memory"),
        }
    }
}

impl std::error::Error for InitError {}

pub(crate) fn read<M: Memory>(memory: &M) -> Result<Header, InitError> {
    let mut bytes = [0; HEADER_SIZE as usize];
    memory.read(0, &mut bytes);

    let actual = [bytes[0], bytes[1], bytes[2]];
    if actual != MAGIC {
        return Err(InitError::BadMagic { actual });
    }
    if bytes[3] != LAYOUT_VERSION {
        return Err(InitError::IncompatibleVersion(bytes[3]));
    }
    if bytes[12..16].iter().any(|byte| *byte != 0) || bytes[72..].iter().any(|byte| *byte != 0) {
        return Err(InitError::InvalidLayout);
    }

    let key_size = u32_at(&bytes, KEY_SIZE_OFFSET);
    let value_size = u32_at(&bytes, VALUE_SIZE_OFFSET);
    let value_slab_offset = 8u64
        .checked_add(
            8u64.checked_mul(u64::from(key_size))
                .ok_or(InitError::InvalidLayout)?,
        )
        .ok_or(InitError::InvalidLayout)?;
    let bucket_page_stride = value_slab_offset
        .checked_add(
            8u64.checked_mul(u64::from(value_size))
                .ok_or(InitError::InvalidLayout)?,
        )
        .ok_or(InitError::InvalidLayout)?;
    Ok(Header {
        key_size,
        value_size,
        key_storage_schema_id: id_at(&bytes, KEY_STORAGE_SCHEMA_ID_OFFSET),
        key_routing_schema_id: id_at(&bytes, KEY_ROUTING_SCHEMA_ID_OFFSET),
        value_storage_schema_id: id_at(&bytes, VALUE_STORAGE_SCHEMA_ID_OFFSET),
        hash_seed: u64_at(&bytes, HASH_SEED_OFFSET),
        bucket_size: 8,
        control_offset: CONTROL_OFFSET,
        control_bytes: CONTROL_BYTES,
        buckets_offset: BUCKETS_OFFSET,
        value_slab_offset,
        bucket_page_stride,
    })
}

pub(crate) fn write<M: Memory>(memory: &M, header: Header) {
    let mut bytes = [0; HEADER_SIZE as usize];
    bytes[3] = LAYOUT_VERSION;
    bytes[KEY_SIZE_OFFSET..KEY_SIZE_OFFSET + 4].copy_from_slice(&header.key_size.to_le_bytes());
    bytes[VALUE_SIZE_OFFSET..VALUE_SIZE_OFFSET + 4]
        .copy_from_slice(&header.value_size.to_le_bytes());
    bytes[KEY_STORAGE_SCHEMA_ID_OFFSET..KEY_STORAGE_SCHEMA_ID_OFFSET + 16]
        .copy_from_slice(&header.key_storage_schema_id);
    bytes[KEY_ROUTING_SCHEMA_ID_OFFSET..KEY_ROUTING_SCHEMA_ID_OFFSET + 16]
        .copy_from_slice(&header.key_routing_schema_id);
    bytes[VALUE_STORAGE_SCHEMA_ID_OFFSET..VALUE_STORAGE_SCHEMA_ID_OFFSET + 16]
        .copy_from_slice(&header.value_storage_schema_id);
    bytes[HASH_SEED_OFFSET..HASH_SEED_OFFSET + 8].copy_from_slice(&header.hash_seed.to_le_bytes());

    memory.write(3, &bytes[3..]);
    memory.write(0, &MAGIC);
}

fn u32_at(bytes: &[u8; HEADER_SIZE as usize], offset: usize) -> u32 {
    u32::from_le_bytes(
        bytes[offset..offset + 4]
            .try_into()
            .expect("fixed header field"),
    )
}

fn u64_at(bytes: &[u8; HEADER_SIZE as usize], offset: usize) -> u64 {
    u64::from_le_bytes(
        bytes[offset..offset + 8]
            .try_into()
            .expect("fixed header field"),
    )
}

fn id_at(bytes: &[u8; HEADER_SIZE as usize], offset: usize) -> [u8; 16] {
    bytes[offset..offset + 16]
        .try_into()
        .expect("fixed header schema identifier")
}
