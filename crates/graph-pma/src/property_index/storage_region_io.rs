use std::collections::BTreeMap;

use crate::low_level::{RegionKind, RegionManager, RegionStorageKind, WASM_PAGE_SIZE};
use crate::stable::Memory;

use crate::property_index::{
    PropertyIndexAllocatorHeader, PropertyIndexError, PropertyIndexNodeId, PropertyIndexNodeRecord,
    PropertyIndexNodeStore, PropertyIndexRegionHeader, PropertyIndexSnapshot,
    PropertyIndexStorageImage,
};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct PropertyIndexPagedAreaMetadata {
    pub(super) allocator: PropertyIndexAllocatorHeader,
    pub(super) page_count: usize,
}

pub fn read_property_index_region_header_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexRegionHeader, PropertyIndexError> {
    let bytes = read_property_index_region_slice(
        manager,
        memory,
        0,
        PropertyIndexRegionHeader::ENCODED_LEN,
    )?;
    PropertyIndexRegionHeader::decode(&bytes)
}

pub fn read_property_index_snapshot_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexSnapshot, PropertyIndexError> {
    let bytes = read_property_index_region_bytes(manager, memory)?;
    PropertyIndexSnapshot::decode(&bytes)
}

pub fn write_property_index_snapshot_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    snapshot: &PropertyIndexSnapshot,
) -> Result<(), PropertyIndexError> {
    let encoded = snapshot.encode()?;
    write_property_index_region_bytes(manager, memory, &encoded)
}

pub fn read_property_index_snapshot_section_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexSnapshot, PropertyIndexError> {
    let header = read_property_index_region_header_from_stable_memory(manager, memory)?;
    let bytes = read_property_index_region_slice(
        manager,
        memory,
        PropertyIndexRegionHeader::ENCODED_LEN,
        header.snapshot_len as usize,
    )?;
    PropertyIndexSnapshot::decode(&bytes)
}

pub fn read_node_property_index_paged_area_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexNodeStore, PropertyIndexError> {
    let header = read_property_index_region_header_from_stable_memory(manager, memory)?;
    let offset = PropertyIndexRegionHeader::ENCODED_LEN
        .checked_add(header.snapshot_len as usize)
        .ok_or(PropertyIndexError::LengthOverflow)?;
    let bytes =
        read_property_index_region_slice(manager, memory, offset, header.node_store_len as usize)?;
    PropertyIndexNodeStore::decode_paged_area(&bytes)
}

pub fn read_edge_property_index_paged_area_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexNodeStore, PropertyIndexError> {
    let header = read_property_index_region_header_from_stable_memory(manager, memory)?;
    let offset = PropertyIndexRegionHeader::ENCODED_LEN
        .checked_add(header.snapshot_len as usize)
        .and_then(|value| value.checked_add(header.node_store_len as usize))
        .ok_or(PropertyIndexError::LengthOverflow)?;
    let bytes =
        read_property_index_region_slice(manager, memory, offset, header.edge_store_len as usize)?;
    PropertyIndexNodeStore::decode_paged_area(&bytes)
}

pub fn read_node_property_index_node_record_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_id: PropertyIndexNodeId,
) -> Result<PropertyIndexNodeRecord, PropertyIndexError> {
    read_property_index_node_record_from_stable_memory(manager, memory, true, node_id)
}

pub fn read_edge_property_index_node_record_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_id: PropertyIndexNodeId,
) -> Result<PropertyIndexNodeRecord, PropertyIndexError> {
    read_property_index_node_record_from_stable_memory(manager, memory, false, node_id)
}

pub fn read_property_index_storage_image_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<PropertyIndexStorageImage, PropertyIndexError> {
    let bytes = read_property_index_region_bytes(manager, memory)?;
    PropertyIndexStorageImage::decode(&bytes)
}

pub fn write_property_index_storage_image_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    image: &PropertyIndexStorageImage,
) -> Result<(), PropertyIndexError> {
    let encoded = image.encode()?;
    write_property_index_region_bytes(manager, memory, &encoded)
}

/// Writes node/edge paged index stores under an empty logical snapshot (compact flush shape).
///
/// When one side has [`PropertyIndexNodeStore::pidx_side_must_flush`] cleared, the stable bytes for
/// that paged area are read back and reused so only the dirty side is [`encode_paged_area`] encoded.
pub fn write_property_index_paged_stores_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    branching_factor: u16,
    node_store: &mut PropertyIndexNodeStore,
    edge_store: &mut PropertyIndexNodeStore,
) -> Result<(), PropertyIndexError> {
    if !node_store.pidx_side_must_flush && !edge_store.pidx_side_must_flush {
        crate::bench_profile::record_stat("pidx_flush_skipped_both_clean", 1);
        return Ok(());
    }

    let snapshot_bytes = PropertyIndexSnapshot::empty(branching_factor).encode()?;
    let header_existing = read_property_index_region_header_from_stable_memory(manager, memory).ok();

    let mut node_from_encode = true;
    let node_bytes = if node_store.pidx_side_must_flush {
        let old_node_slice: Option<Vec<u8>> = if let Some(ref h) = header_existing {
            if h.node_store_len == 0 {
                None
            } else {
                let off = PropertyIndexRegionHeader::ENCODED_LEN
                    .checked_add(h.snapshot_len as usize)
                    .ok_or(PropertyIndexError::LengthOverflow)?;
                read_property_index_region_slice(manager, memory, off, h.node_store_len as usize).ok()
            }
        } else {
            None
        };
        if let Some(ref old) = old_node_slice {
            if let Ok(Some(patched)) =
                PropertyIndexNodeStore::try_encode_paged_area_incremental(node_store, old)
            {
                crate::bench_profile::record_stat("pidx_node_incremental_paged_flush", 1);
                patched
            } else if let Ok(Some(patched)) =
                PropertyIndexNodeStore::try_encode_paged_area_zero_overflow_tail_extend(
                    node_store, old,
                )
            {
                crate::bench_profile::record_stat("pidx_node_tail_extend_paged_flush", 1);
                patched
            } else {
                node_store.encode_paged_area()?
            }
        } else {
            node_store.encode_paged_area()?
        }
    } else if let Some(ref h) = header_existing {
        if h.node_store_len == 0 {
            node_store.encode_paged_area()?
        } else {
            let off = PropertyIndexRegionHeader::ENCODED_LEN
                .checked_add(h.snapshot_len as usize)
                .ok_or(PropertyIndexError::LengthOverflow)?;
            match read_property_index_region_slice(manager, memory, off, h.node_store_len as usize) {
                Ok(b) => {
                    node_from_encode = false;
                    b
                }
                Err(_) => node_store.encode_paged_area()?,
            }
        }
    } else {
        node_store.encode_paged_area()?
    };

    let mut edge_from_encode = true;
    let edge_bytes = if edge_store.pidx_side_must_flush {
        let old_edge_slice: Option<Vec<u8>> = if let Some(ref h) = header_existing {
            if h.edge_store_len == 0 {
                None
            } else {
                let off = PropertyIndexRegionHeader::ENCODED_LEN
                    .checked_add(h.snapshot_len as usize)
                    .and_then(|v| v.checked_add(h.node_store_len as usize))
                    .ok_or(PropertyIndexError::LengthOverflow)?;
                read_property_index_region_slice(manager, memory, off, h.edge_store_len as usize).ok()
            }
        } else {
            None
        };
        if let Some(ref old) = old_edge_slice {
            if let Ok(Some(patched)) =
                PropertyIndexNodeStore::try_encode_paged_area_incremental(edge_store, old)
            {
                crate::bench_profile::record_stat("pidx_edge_incremental_paged_flush", 1);
                patched
            } else if let Ok(Some(patched)) =
                PropertyIndexNodeStore::try_encode_paged_area_zero_overflow_tail_extend(
                    edge_store, old,
                )
            {
                crate::bench_profile::record_stat("pidx_edge_tail_extend_paged_flush", 1);
                patched
            } else {
                edge_store.encode_paged_area()?
            }
        } else {
            edge_store.encode_paged_area()?
        }
    } else if let Some(ref h) = header_existing {
        if h.edge_store_len == 0 {
            edge_store.encode_paged_area()?
        } else {
            let off = PropertyIndexRegionHeader::ENCODED_LEN
                .checked_add(h.snapshot_len as usize)
                .and_then(|v| v.checked_add(h.node_store_len as usize))
                .ok_or(PropertyIndexError::LengthOverflow)?;
            match read_property_index_region_slice(manager, memory, off, h.edge_store_len as usize) {
                Ok(b) => {
                    edge_from_encode = false;
                    b
                }
                Err(_) => edge_store.encode_paged_area()?,
            }
        }
    } else {
        edge_store.encode_paged_area()?
    };

    if !node_from_encode {
        crate::bench_profile::record_stat(
            "pidx_node_section_reused_bytes",
            node_bytes.len() as u64,
        );
    }
    if !edge_from_encode {
        crate::bench_profile::record_stat(
            "pidx_edge_section_reused_bytes",
            edge_bytes.len() as u64,
        );
    }

    let header = PropertyIndexRegionHeader {
        version: PropertyIndexStorageImage::VERSION,
        reserved: [0; 3],
        snapshot_len: u32::try_from(snapshot_bytes.len())
            .map_err(|_| PropertyIndexError::LengthOverflow)?,
        node_store_len: u32::try_from(node_bytes.len()).map_err(|_| PropertyIndexError::LengthOverflow)?,
        edge_store_len: u32::try_from(edge_bytes.len()).map_err(|_| PropertyIndexError::LengthOverflow)?,
    };

    let mut encoded = Vec::new();
    encoded.extend_from_slice(&header.encode());
    encoded.extend_from_slice(&snapshot_bytes);
    encoded.extend_from_slice(&node_bytes);
    encoded.extend_from_slice(&edge_bytes);

    write_property_index_region_bytes(manager, memory, &encoded)?;
    node_store.pidx_side_must_flush = false;
    edge_store.pidx_side_must_flush = false;
    Ok(())
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

pub(super) fn read_property_index_region_bytes(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<Vec<u8>, PropertyIndexError> {
    let region = manager.layout.region(RegionKind::PropertyIndex).ok_or(
        PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
    )?;
    let logical_len = usize::try_from(region.logical_len_bytes)
        .map_err(|_| PropertyIndexError::LengthOverflow)?;

    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let extent = manager.region_extent(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            let mut bytes = vec![0u8; logical_len];
            if logical_len > 0 {
                memory.read(extent.addr.0, &mut bytes);
            }
            Ok(bytes)
        }
        RegionStorageKind::BucketChain => {
            let chain = manager.bucket_chain(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| PropertyIndexError::LengthOverflow)?;
            let mut bytes = vec![0u8; logical_len];
            let mut offset = 0usize;
            let mut cursor = chain.head;
            while !cursor.is_null() && offset < logical_len {
                let header = manager.bucket_header(cursor).ok_or(
                    PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
                )?;
                let len = bucket_size.min(logical_len - offset);
                memory.read(header.addr.0, &mut bytes[offset..offset + len]);
                offset += len;
                cursor = header.next;
            }
            if offset < logical_len {
                return Err(PropertyIndexError::TruncatedBucketChain {
                    kind: RegionKind::PropertyIndex,
                    logical_len,
                    read: offset,
                });
            }
            Ok(bytes)
        }
    }
}

pub(super) fn read_property_index_region_slice(
    manager: &RegionManager,
    memory: &impl Memory,
    offset: usize,
    len: usize,
) -> Result<Vec<u8>, PropertyIndexError> {
    let region = manager.layout.region(RegionKind::PropertyIndex).ok_or(
        PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
    )?;
    let logical_len = usize::try_from(region.logical_len_bytes)
        .map_err(|_| PropertyIndexError::LengthOverflow)?;
    let end = offset
        .checked_add(len)
        .ok_or(PropertyIndexError::LengthOverflow)?;
    if end > logical_len {
        return Err(PropertyIndexError::RecordLengthMismatch {
            expected: end,
            actual: logical_len,
        });
    }

    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let extent = manager.region_extent(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            let mut bytes = vec![0u8; len];
            if len > 0 {
                memory.read(
                    extent
                        .addr
                        .0
                        .checked_add(offset as u64)
                        .ok_or(PropertyIndexError::LengthOverflow)?,
                    &mut bytes,
                );
            }
            Ok(bytes)
        }
        RegionStorageKind::BucketChain => {
            let chain = manager.bucket_chain(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| PropertyIndexError::LengthOverflow)?;
            let mut bytes = vec![0u8; len];
            let mut remaining_skip = offset;
            let mut output_offset = 0usize;
            let mut cursor = chain.head;

            while !cursor.is_null() && output_offset < len {
                let header = manager.bucket_header(cursor).ok_or(
                    PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
                )?;
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
                    .ok_or(PropertyIndexError::LengthOverflow)?;
                memory.read(start_addr, &mut bytes[output_offset..output_offset + take]);
                output_offset += take;
                remaining_skip = 0;
                cursor = header.next;
            }

            if output_offset < len {
                return Err(PropertyIndexError::TruncatedBucketChain {
                    kind: RegionKind::PropertyIndex,
                    logical_len: len,
                    read: output_offset,
                });
            }
            Ok(bytes)
        }
    }
}

pub(super) fn read_property_index_node_record_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
    node_id: PropertyIndexNodeId,
) -> Result<PropertyIndexNodeRecord, PropertyIndexError> {
    if node_id.is_null() {
        return Err(PropertyIndexError::NullNodeId);
    }
    let header = read_property_index_region_header_from_stable_memory(manager, memory)?;
    let allocator = if node_side {
        read_node_property_index_paged_area_from_stable_memory(manager, memory)?.allocator
    } else {
        read_edge_property_index_paged_area_from_stable_memory(manager, memory)?.allocator
    };
    let helper = PropertyIndexNodeStore {
        allocator,
        free_node_ids: Vec::new(),
        nodes: BTreeMap::new(),
        pidx_side_must_flush: false,
    };
    let page_size = usize::try_from(helper.allocator.page_size_bytes)
        .map_err(|_| PropertyIndexError::LengthOverflow)?;

    let section_offset = if node_side {
        PropertyIndexRegionHeader::ENCODED_LEN
            .checked_add(header.snapshot_len as usize)
            .ok_or(PropertyIndexError::LengthOverflow)?
    } else {
        PropertyIndexRegionHeader::ENCODED_LEN
            .checked_add(header.snapshot_len as usize)
            .and_then(|value| value.checked_add(header.node_store_len as usize))
            .ok_or(PropertyIndexError::LengthOverflow)?
    };
    let paged_prefix = read_property_index_region_slice(
        manager,
        memory,
        section_offset,
        PropertyIndexNodeStore::PAGED_AREA_FIXED_HEADER_LEN,
    )?;
    if paged_prefix[..4] != PropertyIndexNodeStore::PAGED_AREA_MAGIC {
        return Err(PropertyIndexError::InvalidPagedAreaMagic(
            paged_prefix[..4].to_vec(),
        ));
    }
    let paged_version = paged_prefix[4];
    let allocator_start = 5;
    let allocator_end = allocator_start + PropertyIndexAllocatorHeader::ENCODED_LEN;
    let mut free_count = [0u8; 4];
    free_count.copy_from_slice(&paged_prefix[allocator_end..allocator_end + 4]);
    let free_count = u32::from_le_bytes(free_count) as usize;
    let pages_start = PropertyIndexNodeStore::paged_area_pages_offset(paged_version, free_count)?;
    let page_offset = helper.node_page_offset(node_id)? as usize;
    let initial_page = read_property_index_region_slice(
        manager,
        memory,
        section_offset
            .checked_add(pages_start)
            .ok_or(PropertyIndexError::LengthOverflow)?
            .checked_add(page_offset)
            .ok_or(PropertyIndexError::LengthOverflow)?,
        page_size,
    )?;
    if initial_page.iter().all(|byte| *byte == 0) {
        return Err(PropertyIndexError::MissingNodeSlot(node_id));
    }

    if paged_version == 1 {
        return helper.decode_node_page(&initial_page);
    }
    let mut pages = vec![initial_page];
    let mut next = [0u8; 8];
    next.copy_from_slice(&pages[0][9..17]);
    let mut next_index = u64::from_le_bytes(next);
    while next_index != 0 {
        let global_index =
            usize::try_from(next_index).map_err(|_| PropertyIndexError::LengthOverflow)?;
        let slot_offset = global_index
            .checked_mul(page_size)
            .ok_or(PropertyIndexError::LengthOverflow)?;
        let overflow_offset = section_offset
            .checked_add(pages_start)
            .and_then(|value| value.checked_add(slot_offset))
            .ok_or(PropertyIndexError::LengthOverflow)?;
        let overflow_page =
            read_property_index_region_slice(manager, memory, overflow_offset, page_size)?;
        let mut overflow_next = [0u8; 8];
        overflow_next.copy_from_slice(&overflow_page[5..13]);
        next_index = u64::from_le_bytes(overflow_next);
        pages.push(overflow_page);
    }
    helper.decode_node_pages(&pages)
}

pub(super) fn read_property_index_paged_area_metadata_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    node_side: bool,
) -> Result<PropertyIndexPagedAreaMetadata, PropertyIndexError> {
    let header = read_property_index_region_header_from_stable_memory(manager, memory)?;
    let section_offset = if node_side {
        PropertyIndexRegionHeader::ENCODED_LEN
            .checked_add(header.snapshot_len as usize)
            .ok_or(PropertyIndexError::LengthOverflow)?
    } else {
        PropertyIndexRegionHeader::ENCODED_LEN
            .checked_add(header.snapshot_len as usize)
            .and_then(|value| value.checked_add(header.node_store_len as usize))
            .ok_or(PropertyIndexError::LengthOverflow)?
    };
    let paged_prefix = read_property_index_region_slice(
        manager,
        memory,
        section_offset,
        PropertyIndexNodeStore::PAGED_AREA_FIXED_HEADER_LEN,
    )?;
    if paged_prefix[..4] != PropertyIndexNodeStore::PAGED_AREA_MAGIC {
        return Err(PropertyIndexError::InvalidPagedAreaMagic(
            paged_prefix[..4].to_vec(),
        ));
    }
    let paged_version = paged_prefix[4];
    let allocator_start = 5;
    let allocator_end = allocator_start + PropertyIndexAllocatorHeader::ENCODED_LEN;
    let allocator =
        PropertyIndexAllocatorHeader::decode(&paged_prefix[allocator_start..allocator_end])?;
    let mut free_count = [0u8; 4];
    free_count.copy_from_slice(&paged_prefix[allocator_end..allocator_end + 4]);
    let free_count = u32::from_le_bytes(free_count) as usize;
    let mut page_count = [0u8; 8];
    page_count.copy_from_slice(&paged_prefix[allocator_end + 4..allocator_end + 12]);
    let page_count = u64::from_le_bytes(page_count) as usize;
    let _pages_start = PropertyIndexNodeStore::paged_area_pages_offset(paged_version, free_count)?;
    Ok(PropertyIndexPagedAreaMetadata {
        allocator,
        page_count,
    })
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

            if encoded.len() == old_logical && !encoded.is_empty() {
                let mut old = vec![0u8; encoded.len()];
                memory.read(extent.addr.0, &mut old);
                if old == encoded {
                    return Ok(());
                }
                let mut i = 0usize;
                while i < encoded.len() {
                    while i < encoded.len() && old[i] == encoded[i] {
                        i += 1;
                    }
                    if i >= encoded.len() {
                        break;
                    }
                    let run_start = i;
                    while i < encoded.len() && old[i] != encoded[i] {
                        i += 1;
                    }
                    let span = i - run_start;
                    crate::bench_profile::record_stat("pidx_region_diff_write_bytes", span as u64);
                    memory.write(
                        extent
                            .addr
                            .0
                            .checked_add(run_start as u64)
                            .ok_or(PropertyIndexError::LengthOverflow)?,
                        &encoded[run_start..i],
                    );
                }
                return Ok(());
            }

            if encoded.len() < old_logical {
                if !encoded.is_empty() {
                    memory.write(extent.addr.0, encoded);
                }
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
                }
                crate::bench_profile::record_stat(
                    "pidx_region_shrink_cleared_bytes",
                    clear_len as u64,
                );
                return Ok(());
            }

            if encoded.len() > old_logical && old_logical > 0 {
                let mut old_prefix = vec![0u8; old_logical];
                memory.read(extent.addr.0, &mut old_prefix);
                if encoded.len() >= old_logical && encoded[..old_logical] == old_prefix[..] {
                    let tail = &encoded[old_logical..];
                    memory.write(
                        extent
                            .addr
                            .0
                            .checked_add(old_logical as u64)
                            .ok_or(PropertyIndexError::LengthOverflow)?,
                        tail,
                    );
                    crate::bench_profile::record_stat("pidx_region_tail_write_bytes", tail.len() as u64);
                    return Ok(());
                }
            }

            if !encoded.is_empty() {
                memory.write(extent.addr.0, encoded);
            }
            crate::bench_profile::record_stat(
                "pidx_region_full_write_bytes",
                encoded.len() as u64,
            );
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
