//! PIDX stable-memory I/O: v3 layout is a small header plus one `StableBTreeMap` backing blob.

use std::cell::RefCell;

use crate::low_level::{
    read_region_logical_slice, write_region_logical_slice, RegionKind, RegionManager,
    RegionStorageKind, WASM_PAGE_SIZE,
};
use ic_stable_structures::Memory;

use super::super::pidx_v3_layout::{
    PIDX_V3_HEADER_LEN, PIDX_V3_MAGIC, PropertyIndexRegionHeaderV3,
};
use super::super::{PropertyIndexError, PropertyIndexSnapshot, PropertyIndexStorageImage};

pub(crate) fn read_property_index_region_slice(
    manager: &RegionManager,
    memory: &impl Memory,
    offset: usize,
    len: usize,
) -> Result<Vec<u8>, PropertyIndexError> {
    read_region_logical_slice(manager, memory, RegionKind::PropertyIndex, offset, len)
        .map_err(Into::into)
}

/// Writes `bytes` at logical `offset` inside the PIDX region, extending layout/capacity as needed.
pub(crate) fn write_property_index_region_logical_slice(
    manager: &mut RegionManager,
    memory: &impl Memory,
    offset: usize,
    bytes: &[u8],
) -> Result<(), PropertyIndexError> {
    write_region_logical_slice(manager, memory, RegionKind::PropertyIndex, offset, bytes)
        .map_err(Into::into)
}

fn ensure_memory_covers(
    memory: &impl Memory,
    last_byte_exclusive: u64,
) -> Result<(), PropertyIndexError> {
    let current_pages = memory.size();
    let current_bytes = current_pages
        .checked_mul(WASM_PAGE_SIZE)
        .ok_or(PropertyIndexError::LengthOverflow)?;
    if current_bytes >= last_byte_exclusive {
        return Ok(());
    }
    let missing_bytes = last_byte_exclusive - current_bytes;
    let delta_pages = missing_bytes.div_ceil(WASM_PAGE_SIZE);
    if memory.grow(delta_pages) == -1 {
        return Err(PropertyIndexError::LengthOverflow);
    }
    Ok(())
}

pub fn read_property_index_region_magic(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<Option<[u8; 4]>, PropertyIndexError> {
    let region = manager.layout.region(RegionKind::PropertyIndex).ok_or(
        PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
    )?;
    if region.logical_len_bytes < 4 {
        return Ok(None);
    }
    let bytes = read_property_index_region_slice(manager, memory, 0, 4)?;
    Ok(Some(<[u8; 4]>::try_from(bytes.as_slice()).map_err(
        |_| PropertyIndexError::RecordTooShort(bytes.len()),
    )?))
}

pub fn read_property_index_region_header_from_stable_memory(
    _manager: &RegionManager,
    _memory: &impl Memory,
) -> Result<(), PropertyIndexError> {
    Err(PropertyIndexError::UnsupportedVersion(0))
}

pub fn read_property_index_snapshot_from_stable_memory(
    _manager: &RegionManager,
    _memory: &impl Memory,
) -> Result<PropertyIndexSnapshot, PropertyIndexError> {
    Err(PropertyIndexError::UnsupportedVersion(0))
}

pub fn read_property_index_snapshot_section_from_stable_memory(
    _manager: &RegionManager,
    _memory: &impl Memory,
) -> Result<PropertyIndexSnapshot, PropertyIndexError> {
    Err(PropertyIndexError::UnsupportedVersion(0))
}

pub fn read_property_index_storage_image_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexStorageImage, PropertyIndexError> {
    let bytes = read_property_index_region_bytes(manager, memory)?;
    PropertyIndexStorageImage::decode(&bytes)
}

pub fn write_property_index_snapshot_to_stable_memory(
    _manager: &mut RegionManager,
    _memory: &impl Memory,
    _snapshot: &PropertyIndexSnapshot,
) -> Result<(), PropertyIndexError> {
    Err(PropertyIndexError::UnsupportedVersion(0))
}

pub fn write_property_index_storage_image_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    image: &PropertyIndexStorageImage,
) -> Result<(), PropertyIndexError> {
    let encoded = image.encode()?;
    write_property_index_region_bytes(manager, memory, &encoded)
}

/// Writes only the PIDX v3 fixed header so it matches the live btree payload length in stable memory.
pub fn sync_property_index_pidx_v3_header_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    btree_payload_len: u64,
) -> Result<(), PropertyIndexError> {
    let header = PropertyIndexRegionHeaderV3 { btree_payload_len };
    write_property_index_region_logical_slice(manager, memory, 0, &header.encode())?;
    let total = (PIDX_V3_HEADER_LEN as u64)
        .checked_add(btree_payload_len)
        .ok_or(PropertyIndexError::LengthOverflow)?;
    manager
        .set_region_logical_len(RegionKind::PropertyIndex, total)
        .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
            RegionKind::PropertyIndex,
        ))?;
    Ok(())
}

pub fn read_pidx_v3_header_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<Option<PropertyIndexRegionHeaderV3>, PropertyIndexError> {
    match read_property_index_region_magic(manager, memory)? {
        Some(m) if m == PIDX_V3_MAGIC => {
            let hdr = read_property_index_region_slice(manager, memory, 0, PIDX_V3_HEADER_LEN)?;
            Ok(Some(PropertyIndexRegionHeaderV3::decode(&hdr)?))
        }
        _ => Ok(None),
    }
}

/// Byte length the btree subregion must expose so ic-stable-structures' Wasm-page `Memory::size()`
/// matches the logical region length (virtual capacity is always a multiple of [`WASM_PAGE_SIZE`]).
#[inline]
pub(crate) fn btree_payload_virtual_len_bytes(raw: u64) -> u64 {
    raw.div_ceil(WASM_PAGE_SIZE).saturating_mul(WASM_PAGE_SIZE)
}

/// Expands the PIDX region / pads the btree tail when an older image stored an unpadded payload
/// length in the v3 header.
pub fn ensure_pidx_v3_btree_subregion_for_hydrate(
    manager: &mut RegionManager,
    memory: &impl Memory,
    header: &PropertyIndexRegionHeaderV3,
) -> Result<u64, PropertyIndexError> {
    let raw = header.btree_payload_len;
    let virt = btree_payload_virtual_len_bytes(raw);
    let base = PIDX_V3_HEADER_LEN as u64;
    let min_logical = base.saturating_add(virt);
    let cur_logical = manager
        .layout
        .region(RegionKind::PropertyIndex)
        .map(|r| r.logical_len_bytes)
        .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
            RegionKind::PropertyIndex,
        ))?;
    if cur_logical < min_logical {
        manager
            .set_region_logical_len(RegionKind::PropertyIndex, min_logical)
            .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
                RegionKind::PropertyIndex,
            ))?;
    }
    if virt > raw {
        let gap_offset = usize::try_from(base.saturating_add(raw))
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        let gap_len = usize::try_from(virt.saturating_sub(raw))
            .map_err(|_| PropertyIndexError::LengthOverflow)?;
        if gap_len > 0 {
            let zeros = vec![0u8; gap_len];
            write_property_index_region_logical_slice(manager, memory, gap_offset, &zeros)?;
        }
    }
    Ok(virt)
}

/// Persists the PIDX v3 header after btree mutations; btree bytes are already written in-place.
pub fn write_property_index_stable_equality_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    btree_payload_len: &RefCell<u64>,
    must_flush: bool,
) -> Result<(), PropertyIndexError> {
    if !must_flush {
        crate::bench_profile::record_stat("pidx_flush_skipped_both_clean", 1);
        return Ok(());
    }
    let len = *btree_payload_len.borrow();
    crate::canbench_scope::scope("pma_pidx_write_region");
    sync_property_index_pidx_v3_header_to_stable_memory(manager, memory, len)?;
    Ok(())
}

pub fn read_property_index_region_bytes(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<Vec<u8>, PropertyIndexError> {
    let region = manager.layout.region(RegionKind::PropertyIndex).ok_or(
        PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
    )?;
    let logical_len = usize::try_from(region.logical_len_bytes)
        .map_err(|_| PropertyIndexError::LengthOverflow)?;
    read_region_logical_slice(manager, memory, RegionKind::PropertyIndex, 0, logical_len)
        .map_err(Into::into)
}

fn write_property_index_region_bytes(
    manager: &mut RegionManager,
    memory: &impl Memory,
    encoded: &[u8],
) -> Result<(), PropertyIndexError> {
    let region = manager.layout.region(RegionKind::PropertyIndex).ok_or(
        PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
    )?;

    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let extent = manager.region_extent(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            let capacity = usize::try_from(extent.len_bytes)
                .map_err(|_| PropertyIndexError::LengthOverflow)?;
            let old_logical = usize::try_from(region.logical_len_bytes)
                .map_err(|_| PropertyIndexError::LengthOverflow)?;
            if encoded.len() > capacity {
                return Err(PropertyIndexError::RegionTooSmall {
                    kind: RegionKind::PropertyIndex,
                    required: encoded.len() as u64,
                    capacity: extent.len_bytes,
                });
            }
            ensure_memory_covers(memory, extent.addr.0 + extent.len_bytes)?;
            manager
                .set_region_logical_len(RegionKind::PropertyIndex, encoded.len() as u64)
                .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
                    RegionKind::PropertyIndex,
                ))?;
            let common_len = old_logical.min(encoded.len());
            if common_len > 0 {
                let mut old_prefix = vec![0u8; common_len];
                memory.read(extent.addr.0, &mut old_prefix);
                if old_prefix != encoded[..common_len] {
                    let mut i = 0usize;
                    while i < common_len {
                        while i < common_len && old_prefix[i] == encoded[i] {
                            i += 1;
                        }
                        if i >= common_len {
                            break;
                        }
                        let run_start = i;
                        while i < common_len && old_prefix[i] != encoded[i] {
                            i += 1;
                        }
                        let span = i - run_start;
                        crate::bench_profile::record_stat(
                            "pidx_region_diff_write_bytes",
                            span as u64,
                        );
                        memory.write(
                            extent
                                .addr
                                .0
                                .checked_add(run_start as u64)
                                .ok_or(PropertyIndexError::LengthOverflow)?,
                            &encoded[run_start..i],
                        );
                    }
                }
            }

            if encoded.len() > old_logical {
                let tail = &encoded[old_logical..];
                if !tail.is_empty() {
                    memory.write(
                        extent
                            .addr
                            .0
                            .checked_add(old_logical as u64)
                            .ok_or(PropertyIndexError::LengthOverflow)?,
                        tail,
                    );
                    crate::bench_profile::record_stat(
                        "pidx_region_tail_write_bytes",
                        tail.len() as u64,
                    );
                }
            } else if encoded.len() < old_logical {
                let clear_len = old_logical - encoded.len();
                if clear_len > 0 {
                    const ZMAX: usize = 4096;
                    let zero_chunk = [0u8; ZMAX];
                    let mut remaining = clear_len;
                    let mut pos = extent
                        .addr
                        .0
                        .checked_add(encoded.len() as u64)
                        .ok_or(PropertyIndexError::LengthOverflow)?;
                    while remaining > 0 {
                        let take = remaining.min(ZMAX);
                        memory.write(pos, &zero_chunk[..take]);
                        pos = pos
                            .checked_add(take as u64)
                            .ok_or(PropertyIndexError::LengthOverflow)?;
                        remaining -= take;
                    }
                    crate::bench_profile::record_stat(
                        "pidx_region_shrink_cleared_bytes",
                        clear_len as u64,
                    );
                }
            }
            Ok(())
        }
        RegionStorageKind::BucketChain => {
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| PropertyIndexError::LengthOverflow)?;
            let chain = manager
                .ensure_bucket_region_capacity(RegionKind::PropertyIndex, encoded.len() as u64)
                .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
                    RegionKind::PropertyIndex,
                ))?;
            let required_buckets = encoded.len().max(1).div_ceil(bucket_size);
            let last_byte_exclusive = manager
                .bucket_header(chain.tail)
                .map(|header| header.addr.0 + manager.bucket_size_bytes())
                .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
                    RegionKind::PropertyIndex,
                ))?;
            ensure_memory_covers(memory, last_byte_exclusive)?;

            let mut cursor = chain.head;
            let mut offset = 0usize;
            let mut written = 0usize;
            while !cursor.is_null() && written < required_buckets {
                let header = manager.bucket_header(cursor).ok_or(
                    PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
                )?;
                let remaining = encoded.len().saturating_sub(offset);
                let len = bucket_size.min(remaining);
                let mut padded = vec![0u8; bucket_size];
                if len > 0 {
                    padded[..len].copy_from_slice(&encoded[offset..offset + len]);
                    offset += len;
                }
                memory.write(header.addr.0, &padded);
                written += 1;
                cursor = header.next;
            }
            manager
                .set_region_logical_len(RegionKind::PropertyIndex, encoded.len() as u64)
                .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
                    RegionKind::PropertyIndex,
                ))?;
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{read_property_index_region_bytes, write_property_index_region_bytes};
    use crate::VecMemory;
    use crate::low_level::{
        BucketSizeInPages, ExtentChain, ExtentId, RegionKind, RegionManager, WASM_PAGE_SIZE,
        WasmPages,
    };
    use crate::property_index::PIDX_V3_MAGIC;
    use crate::property_index::property_equality::{
        decode_pidx_v3_region, empty_property_equality_map,
    };
    use ic_stable_structures::Memory;

    fn setup_property_index_extent(
        old_logical_len: usize,
        extent_pages: u64,
    ) -> (RegionManager, VecMemory, u64, usize) {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::PropertyIndex,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                old_logical_len as u64,
                WasmPages::new(extent_pages),
                WasmPages::new(0),
            ),
        );
        manager
            .set_region_logical_len(RegionKind::PropertyIndex, old_logical_len as u64)
            .expect("set logical len");

        let memory = VecMemory::default();
        let extent = manager
            .region_extent(RegionKind::PropertyIndex)
            .expect("region extent");
        let capacity = usize::try_from(extent.len_bytes).expect("capacity usize");
        let end_byte = extent
            .addr
            .0
            .checked_add(extent.len_bytes)
            .expect("extent end");
        let pages_needed = end_byte.div_ceil(WASM_PAGE_SIZE);
        while memory.size() < pages_needed {
            assert!(memory.grow(1) >= 0, "grow vector backing for tests");
        }
        if capacity > 0 {
            memory.write(extent.addr.0, &vec![0u8; capacity]);
        }
        (manager, memory, extent.addr.0, capacity)
    }

    #[test]
    fn v3_round_trip_empty_map_through_region_write() {
        let map = empty_property_equality_map();
        let enc =
            crate::property_index::property_equality::encode_pidx_v3_region(&map).expect("encode");
        let (mut manager, memory, _base, _) = setup_property_index_extent(enc.len(), 4);
        write_property_index_region_bytes(&mut manager, &memory, &enc).expect("write");
        let got = read_property_index_region_bytes(&manager, &memory).expect("read");
        assert_eq!(got, enc);
        assert!(got.starts_with(&PIDX_V3_MAGIC));
        let _loaded = decode_pidx_v3_region(&got).expect("btree");
    }
}
