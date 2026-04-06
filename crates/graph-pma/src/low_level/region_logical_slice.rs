//! Logical byte-range I/O for any [`RegionKind`] backed by an extent or bucket chain.
//!
//! Used by [`super::virtual_region_memory::VirtualBucketMemory`], property-store helpers, and PIDX
//! region I/O so bucket-walk / extent addressing stays in one place.

use std::fmt;

use ic_stable_structures::Memory;

use super::manager::RegionManager;
use super::region::{RegionKind, RegionStorageKind, WASM_PAGE_SIZE};

/// Failure reading or writing a logical slice inside one region.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegionLogicalIoError {
    MissingRegion(RegionKind),
    LengthOverflow,
    RecordLengthMismatch {
        expected: usize,
        actual: usize,
    },
    RegionTooSmall {
        kind: RegionKind,
        required: u64,
        capacity: u64,
    },
    TruncatedBucketChain {
        kind: RegionKind,
        logical_len: usize,
        read: usize,
    },
}

impl fmt::Display for RegionLogicalIoError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingRegion(k) => write!(f, "missing region definition for {:?}", k),
            Self::LengthOverflow => write!(f, "region logical slice length overflow"),
            Self::RecordLengthMismatch { expected, actual } => write!(
                f,
                "region slice length mismatch: expected {expected} bytes, logical region has {actual}"
            ),
            Self::RegionTooSmall {
                kind,
                required,
                capacity,
            } => write!(
                f,
                "region {:?} too small: required {required} bytes, capacity {capacity} bytes",
                kind
            ),
            Self::TruncatedBucketChain {
                kind,
                logical_len,
                read,
            } => write!(
                f,
                "truncated bucket chain for {:?}: needed {logical_len} bytes, read {read}",
                kind
            ),
        }
    }
}

impl std::error::Error for RegionLogicalIoError {}

fn ensure_backing_covers(memory: &impl Memory, last_byte_exclusive: u64) -> Result<(), RegionLogicalIoError> {
    let current_pages = memory.size();
    let current_bytes = current_pages
        .checked_mul(WASM_PAGE_SIZE)
        .ok_or(RegionLogicalIoError::LengthOverflow)?;
    if current_bytes >= last_byte_exclusive {
        return Ok(());
    }
    let missing_bytes = last_byte_exclusive - current_bytes;
    let delta_pages = missing_bytes.div_ceil(WASM_PAGE_SIZE);
    if memory.grow(delta_pages) == -1 {
        return Err(RegionLogicalIoError::LengthOverflow);
    }
    Ok(())
}

/// Reads `len` bytes at logical `offset` within `kind` (extent or bucket chain).
pub fn read_region_logical_slice(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    offset: usize,
    len: usize,
) -> Result<Vec<u8>, RegionLogicalIoError> {
    let region = manager
        .layout
        .region(kind)
        .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
    let logical_len = usize::try_from(region.logical_len_bytes)
        .map_err(|_| RegionLogicalIoError::LengthOverflow)?;
    let end = offset
        .checked_add(len)
        .ok_or(RegionLogicalIoError::LengthOverflow)?;
    if end > logical_len {
        return Err(RegionLogicalIoError::RecordLengthMismatch {
            expected: end,
            actual: logical_len,
        });
    }

    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let extent = manager
                .region_extent(kind)
                .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
            let mut bytes = vec![0u8; len];
            if len > 0 {
                memory.read(
                    extent
                        .addr
                        .0
                        .checked_add(offset as u64)
                        .ok_or(RegionLogicalIoError::LengthOverflow)?,
                    &mut bytes,
                );
            }
            Ok(bytes)
        }
        RegionStorageKind::BucketChain => {
            let chain = manager
                .bucket_chain(kind)
                .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| RegionLogicalIoError::LengthOverflow)?;
            let mut bytes = vec![0u8; len];
            let mut remaining_skip = offset;
            let mut output_offset = 0usize;
            let mut cursor = chain.head;

            while !cursor.is_null() && output_offset < len {
                let header = manager
                    .bucket_header(cursor)
                    .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
                if remaining_skip >= bucket_size {
                    remaining_skip -= bucket_size;
                    cursor = header.next;
                    continue;
                }
                let available = bucket_size - remaining_skip;
                let take = available.min(len - output_offset);
                let start_addr = header
                    .addr
                    .0
                    .checked_add(remaining_skip as u64)
                    .ok_or(RegionLogicalIoError::LengthOverflow)?;
                memory.read(start_addr, &mut bytes[output_offset..output_offset + take]);
                output_offset += take;
                remaining_skip = 0;
                cursor = header.next;
            }

            if output_offset < len {
                return Err(RegionLogicalIoError::TruncatedBucketChain {
                    kind,
                    logical_len: len,
                    read: output_offset,
                });
            }
            Ok(bytes)
        }
    }
}

/// Writes `bytes` at logical `offset`, extending the region’s logical length as needed.
pub fn write_region_logical_slice(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    offset: usize,
    bytes: &[u8],
) -> Result<(), RegionLogicalIoError> {
    let end = offset
        .checked_add(bytes.len())
        .ok_or(RegionLogicalIoError::LengthOverflow)?;
    let prior_len = usize::try_from(
        manager
            .layout
            .region(kind)
            .ok_or(RegionLogicalIoError::MissingRegion(kind))?
            .logical_len_bytes,
    )
    .map_err(|_| RegionLogicalIoError::LengthOverflow)?;
    let new_len =
        u64::try_from(prior_len.max(end)).map_err(|_| RegionLogicalIoError::LengthOverflow)?;
    match manager
        .layout
        .region(kind)
        .ok_or(RegionLogicalIoError::MissingRegion(kind))?
        .storage_kind()
    {
        RegionStorageKind::Extent => {
            let extent = manager
                .region_extent(kind)
                .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
            ensure_backing_covers(memory, extent.addr.0.saturating_add(extent.len_bytes))?;
            if new_len > extent.len_bytes {
                return Err(RegionLogicalIoError::RegionTooSmall {
                    kind,
                    required: new_len,
                    capacity: extent.len_bytes,
                });
            }
            manager
                .set_region_logical_len(kind, new_len)
                .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
            if !bytes.is_empty() {
                memory.write(
                    extent
                        .addr
                        .0
                        .checked_add(offset as u64)
                        .ok_or(RegionLogicalIoError::LengthOverflow)?,
                    bytes,
                );
            }
        }
        RegionStorageKind::BucketChain => {
            manager
                .ensure_bucket_region_capacity(kind, new_len)
                .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
            let chain = manager
                .bucket_chain(kind)
                .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| RegionLogicalIoError::LengthOverflow)?;
            let last_byte_exclusive = manager
                .bucket_header(chain.tail)
                .map(|header| header.addr.0 + manager.bucket_size_bytes())
                .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
            ensure_backing_covers(memory, last_byte_exclusive)?;

            let mut remaining_skip = offset;
            let mut written = 0usize;
            let mut cursor = chain.head;
            while !cursor.is_null() && written < bytes.len() {
                let header = manager
                    .bucket_header(cursor)
                    .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
                if remaining_skip >= bucket_size {
                    remaining_skip -= bucket_size;
                    cursor = header.next;
                    continue;
                }
                let available = bucket_size - remaining_skip;
                let take = available.min(bytes.len() - written);
                memory.write(
                    header
                        .addr
                        .0
                        .checked_add(remaining_skip as u64)
                        .ok_or(RegionLogicalIoError::LengthOverflow)?,
                    &bytes[written..written + take],
                );
                written += take;
                remaining_skip = 0;
                cursor = header.next;
            }
            if written < bytes.len() {
                return Err(RegionLogicalIoError::TruncatedBucketChain {
                    kind,
                    logical_len: offset.saturating_add(bytes.len()),
                    read: offset.saturating_add(written),
                });
            }
            manager
                .set_region_logical_len(kind, new_len)
                .ok_or(RegionLogicalIoError::MissingRegion(kind))?;
        }
    }
    Ok(())
}
