//! Stable-memory encode/decode for the property index (PIDX) region.
//!
//! ## Next-phase I/O: diff writes vs layout (`pma_pidx_write_region`)
//!
//! [`write_property_index_paged_stores_to_stable_memory`] always builds one contiguous byte sequence
//! (header + empty snapshot + node paged area + edge paged area) and passes it to
//! [`write_property_index_region_bytes`]. For [`RegionStorageKind::Extent`], that helper already
//! performs **byte-run diffing** when logical length is unchanged (`memory.read` of the old image,
//! then one or more `memory.write` spans for changed runs — see `pidx_region_diff_write_bytes` stats).
//! When logical length changes, it may **full-write** the region or append a tail / clear a shrink.
//! **Bucket-chain** storage still writes full fixed-size bucket payloads per touched bucket.
//!
//! Design options (compare before changing on-disk layout):
//!
//! - **A — Page / dirty-extent diff (preferred first):** Track dirty page ranges (or coalesced byte
//!   spans) inside [`PropertyIndexNodeStore`] / flush pipeline, and emit **minimal `memory.write`s**
//!   for those spans *without* requiring a new region format. Builds on the existing incremental
//!   encode paths (`try_encode_paged_area_incremental`, tail extend) and the extent diff writer.
//!   *Risk:* API surface for “what’s dirty”; must stay correct vs header length fields and reuse paths
//!   when only one side flushes.
//! - **B — Split PIDX into sub-regions:** e.g. separate stable extents for node vs edge paged areas
//!   or hot/cold splits so frequent updates touch a smaller mapping. *Merit:* isolates churn.
//!   *Risk:* **layout / migration**; all `PropertyIndexStorageImage` hydrate paths and tooling must
//!   agree on the new map.
//! - **C — Snapshot vs paged area cadence:** Keep a single region but reduce how often the snapshot
//!   section is rewritten or move rarely updated metadata elsewhere. *Merit:* lower header churn.
//!   *Risk:* may be marginal if snapshot is already compact; still need correctness for crash
//!   recovery expectations.
//!
//! **Decision:** prioritize **A** (dirty page / span tracking + smaller writes into the existing
//! extent layout) as the best return for implementation cost and backward compatibility; treat **B**
//! only if A plateaus or product needs isolate hot keys; use **C** as an incremental add-on after
//! measuring snapshot cost in profiles.

use std::collections::{BTreeMap, BTreeSet};

use crate::low_level::{RegionKind, RegionManager, RegionStorageKind, WASM_PAGE_SIZE};
use crate::stable::Memory;

use crate::property_index::{
    PropertyIndexAllocatorHeader, PropertyIndexError, PropertyIndexNodeId, PropertyIndexNodeRecord,
    PropertyIndexNodeStore, PropertyIndexPagedAreaPagePatch, PropertyIndexRegionHeader,
    PropertyIndexSnapshot, PropertyIndexStorageImage,
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

/// When this returns `Ok(Some(_))`, flush may reuse `old` section bytes and skip incremental encode.
/// Empty dirty hints yield `Ok(None)` so callers always take the encode path; `upsert_*` / `remove_*`
/// only set `pidx_side_must_flush`, while summary-based property mutations supply hints via
/// `diff_against` and [`PropertyIndexNodeStore::note_dirty_node_ids`].
fn try_patch_first_paged_plan(
    store: &PropertyIndexNodeStore,
    old: &[u8],
) -> Result<Option<Vec<PropertyIndexPagedAreaPagePatch>>, PropertyIndexError> {
    if store.pidx_dirty_node_hints.is_empty() {
        return Ok(None);
    }
    let dirty: Vec<PropertyIndexNodeId> = store.pidx_dirty_node_hints.iter().copied().collect();
    store.try_build_paged_area_page_patches(old, &dirty)
}

/// Writes node/edge paged index stores under an empty logical snapshot (compact flush shape).
///
/// When one side has [`PropertyIndexNodeStore::pidx_side_must_flush`] cleared, the stable bytes for
/// that paged area are read back and reused so only the dirty side is [`encode_paged_area`] encoded.
///
/// ## Hot path / canbench (`pma_pidx_write_region`)
///
/// After incremental or full encode, this function **concatenates** header + snapshot + node paged
/// bytes + edge paged bytes into one `Vec` and calls [`write_property_index_region_bytes`]. On IC,
/// that serializes to a **single logical region write** over the current `PropertyIndex` extent, so
/// canbench sees most instructions under `pma_pidx_write_region` even when node/edge encode scopes
/// are small. Further wins likely require **reducing bytes touched** (page- or extent-level diff
/// writes, splitting the region, or avoiding full-vector assembly) rather than micro-optimizing
/// encode branches alone. See the module-level **“Next-phase I/O”** note for options **A/B/C** and
/// the agreed priority (**A:** dirty page / span tracking on the current layout).
pub fn write_property_index_paged_stores_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    branching_factor: u16,
    node_store: &mut PropertyIndexNodeStore,
    edge_store: &mut PropertyIndexNodeStore,
) -> Result<(), PropertyIndexError> {
    let node_dirty_hint_count = node_store.pidx_dirty_node_hints.len() as u64;
    let edge_dirty_hint_count = edge_store.pidx_dirty_node_hints.len() as u64;
    crate::bench_profile::record_stat("pidx_node_dirty_hint_count", node_dirty_hint_count);
    crate::bench_profile::record_stat("pidx_edge_dirty_hint_count", edge_dirty_hint_count);

    if !node_store.pidx_side_must_flush && !edge_store.pidx_side_must_flush {
        crate::bench_profile::record_stat("pidx_flush_skipped_both_clean", 1);
        return Ok(());
    }

    let snapshot_bytes = PropertyIndexSnapshot::empty(branching_factor).encode()?;
    let header_existing = read_property_index_region_header_from_stable_memory(manager, memory).ok();

    let mut node_precmp_patches: Option<Vec<PropertyIndexPagedAreaPagePatch>> = None;
    let mut edge_precmp_patches: Option<Vec<PropertyIndexPagedAreaPagePatch>> = None;

    let mut node_from_encode = true;
    let mut node_bytes = if node_store.pidx_side_must_flush {
        let old_node_slice: Option<Vec<u8>> = if let Some(ref h) = header_existing {
            if h.node_store_len == 0 {
                None
            } else {
                let off = PropertyIndexRegionHeader::ENCODED_LEN
                    .checked_add(h.snapshot_len as usize)
                    .ok_or(PropertyIndexError::LengthOverflow)?;
                {
                    let _read = crate::canbench_scope::scope("pma_pidx_read_node_old");
                    read_property_index_region_slice(manager, memory, off, h.node_store_len as usize).ok()
                }
            }
        } else {
            None
        };
        if let Some(old) = old_node_slice {
            {
                let _pf = crate::canbench_scope::scope("pma_pidx_node_patch_first_try");
                if let Some(patches) = try_patch_first_paged_plan(node_store, &old)? {
                    crate::bench_profile::record_stat("pidx_patch_first_skip_node_encode", 1);
                    node_precmp_patches = Some(patches);
                    old
                } else {
                    let inc = {
                        let _inc = crate::canbench_scope::scope("pma_pidx_node_inc");
                        PropertyIndexNodeStore::try_encode_paged_area_incremental(node_store, &old)
                    };
                    if let Ok(Some(patched)) = inc {
                        crate::bench_profile::record_stat("pidx_node_incremental_paged_flush", 1);
                        patched
                    } else {
                        let tail = {
                            let _tail = crate::canbench_scope::scope("pma_pidx_node_tail");
                            PropertyIndexNodeStore::try_encode_paged_area_zero_overflow_tail_extend(
                                node_store, &old,
                            )
                        };
                        if let Ok(Some(patched)) = tail {
                            crate::bench_profile::record_stat("pidx_node_tail_extend_paged_flush", 1);
                            patched
                        } else {
                            let _full = crate::canbench_scope::scope("pma_pidx_node_full");
                            node_store.encode_paged_area()?
                        }
                    }
                }
            }
        } else {
            let _full = crate::canbench_scope::scope("pma_pidx_node_full");
            node_store.encode_paged_area()?
        }
    } else if let Some(ref h) = header_existing {
        if h.node_store_len == 0 {
            let _full = crate::canbench_scope::scope("pma_pidx_node_full");
            node_store.encode_paged_area()?
        } else {
            let off = PropertyIndexRegionHeader::ENCODED_LEN
                .checked_add(h.snapshot_len as usize)
                .ok_or(PropertyIndexError::LengthOverflow)?;
            let read_res = {
                let _reuse = crate::canbench_scope::scope("pma_pidx_read_node_reuse");
                read_property_index_region_slice(manager, memory, off, h.node_store_len as usize)
            };
            match read_res {
                Ok(b) => {
                    node_from_encode = false;
                    b
                }
                Err(_) => {
                    let _full = crate::canbench_scope::scope("pma_pidx_node_full");
                    node_store.encode_paged_area()?
                }
            }
        }
    } else {
        let _full = crate::canbench_scope::scope("pma_pidx_node_full");
        node_store.encode_paged_area()?
    };

    let mut edge_from_encode = true;
    let mut edge_bytes = if edge_store.pidx_side_must_flush {
        let old_edge_slice: Option<Vec<u8>> = if let Some(ref h) = header_existing {
            if h.edge_store_len == 0 {
                None
            } else {
                let off = PropertyIndexRegionHeader::ENCODED_LEN
                    .checked_add(h.snapshot_len as usize)
                    .and_then(|v| v.checked_add(h.node_store_len as usize))
                    .ok_or(PropertyIndexError::LengthOverflow)?;
                {
                    let _read = crate::canbench_scope::scope("pma_pidx_read_edge_old");
                    read_property_index_region_slice(manager, memory, off, h.edge_store_len as usize).ok()
                }
            }
        } else {
            None
        };
        if let Some(old) = old_edge_slice {
            {
                let _pf = crate::canbench_scope::scope("pma_pidx_edge_patch_first_try");
                if let Some(patches) = try_patch_first_paged_plan(edge_store, &old)? {
                    crate::bench_profile::record_stat("pidx_patch_first_skip_edge_encode", 1);
                    edge_precmp_patches = Some(patches);
                    old
                } else {
                    let inc = {
                        let _inc = crate::canbench_scope::scope("pma_pidx_edge_inc");
                        PropertyIndexNodeStore::try_encode_paged_area_incremental(edge_store, &old)
                    };
                    if let Ok(Some(patched)) = inc {
                        crate::bench_profile::record_stat("pidx_edge_incremental_paged_flush", 1);
                        patched
                    } else {
                        let tail = {
                            let _tail = crate::canbench_scope::scope("pma_pidx_edge_tail");
                            PropertyIndexNodeStore::try_encode_paged_area_zero_overflow_tail_extend(
                                edge_store, &old,
                            )
                        };
                        if let Ok(Some(patched)) = tail {
                            crate::bench_profile::record_stat("pidx_edge_tail_extend_paged_flush", 1);
                            patched
                        } else {
                            let _full = crate::canbench_scope::scope("pma_pidx_edge_full");
                            edge_store.encode_paged_area()?
                        }
                    }
                }
            }
        } else {
            let _full = crate::canbench_scope::scope("pma_pidx_edge_full");
            edge_store.encode_paged_area()?
        }
    } else if let Some(ref h) = header_existing {
        if h.edge_store_len == 0 {
            let _full = crate::canbench_scope::scope("pma_pidx_edge_full");
            edge_store.encode_paged_area()?
        } else {
            let off = PropertyIndexRegionHeader::ENCODED_LEN
                .checked_add(h.snapshot_len as usize)
                .and_then(|v| v.checked_add(h.node_store_len as usize))
                .ok_or(PropertyIndexError::LengthOverflow)?;
            let read_res = {
                let _reuse = crate::canbench_scope::scope("pma_pidx_read_edge_reuse");
                read_property_index_region_slice(manager, memory, off, h.edge_store_len as usize)
            };
            match read_res {
                Ok(b) => {
                    edge_from_encode = false;
                    b
                }
                Err(_) => {
                    let _full = crate::canbench_scope::scope("pma_pidx_edge_full");
                    edge_store.encode_paged_area()?
                }
            }
        }
    } else {
        let _full = crate::canbench_scope::scope("pma_pidx_edge_full");
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

    let mut header = PropertyIndexRegionHeader {
        version: PropertyIndexStorageImage::VERSION,
        reserved: [0; 3],
        snapshot_len: u32::try_from(snapshot_bytes.len())
            .map_err(|_| PropertyIndexError::LengthOverflow)?,
        node_store_len: u32::try_from(node_bytes.len()).map_err(|_| PropertyIndexError::LengthOverflow)?,
        edge_store_len: u32::try_from(edge_bytes.len()).map_err(|_| PropertyIndexError::LengthOverflow)?,
    };

    if node_store.pidx_side_must_flush
        && edge_store.pidx_side_must_flush
        && node_precmp_patches.is_some() != edge_precmp_patches.is_some()
    {
        if let Some(ref h) = header_existing {
            let lens_match = h.snapshot_len == header.snapshot_len
                && h.node_store_len == header.node_store_len
                && h.edge_store_len == header.edge_store_len;
            if !lens_match {
                crate::bench_profile::record_stat("pidx_patch_first_mixed_fallback_full_encode", 1);
                node_precmp_patches = None;
                edge_precmp_patches = None;
                node_bytes = node_store.encode_paged_area()?;
                edge_bytes = edge_store.encode_paged_area()?;
                header = PropertyIndexRegionHeader {
                    version: PropertyIndexStorageImage::VERSION,
                    reserved: [0; 3],
                    snapshot_len: u32::try_from(snapshot_bytes.len())
                        .map_err(|_| PropertyIndexError::LengthOverflow)?,
                    node_store_len: u32::try_from(node_bytes.len())
                        .map_err(|_| PropertyIndexError::LengthOverflow)?,
                    edge_store_len: u32::try_from(edge_bytes.len())
                        .map_err(|_| PropertyIndexError::LengthOverflow)?,
                };
            }
        }
    }

    // Fast path (extent only): when section lengths are unchanged, patch only changed sections
    // instead of re-assembling and diff-writing the whole PIDX region image.
    if let Some(ref h) = header_existing {
        if h.snapshot_len == header.snapshot_len
            && h.node_store_len == header.node_store_len
            && h.edge_store_len == header.edge_store_len
        {
            let region = manager.layout.region(RegionKind::PropertyIndex).ok_or(
                PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
            )?;
            if region.storage_kind() == RegionStorageKind::Extent {
                let extent = manager.region_extent(RegionKind::PropertyIndex).ok_or(
                    PropertyIndexError::MissingPropertyIndexRegion(RegionKind::PropertyIndex),
                )?;
                let total_len = PropertyIndexRegionHeader::ENCODED_LEN
                    .checked_add(snapshot_bytes.len())
                    .and_then(|v| v.checked_add(node_bytes.len()))
                    .and_then(|v| v.checked_add(edge_bytes.len()))
                    .ok_or(PropertyIndexError::LengthOverflow)?;
                let capacity = usize::try_from(extent.len_bytes)
                    .map_err(|_| PropertyIndexError::LengthOverflow)?;
                if total_len <= capacity {
                    ensure_memory_covers(memory, extent.addr.0 + extent.len_bytes)?;
                    manager
                        .set_region_logical_len(RegionKind::PropertyIndex, total_len as u64)
                        .ok_or(PropertyIndexError::MissingPropertyIndexRegion(
                            RegionKind::PropertyIndex,
                        ))?;

                    // Header/snapshot are stable under this condition; patch only dirty sides.
                    let node_offset = PropertyIndexRegionHeader::ENCODED_LEN
                        .checked_add(snapshot_bytes.len())
                        .ok_or(PropertyIndexError::LengthOverflow)?;
                    let edge_offset = node_offset
                        .checked_add(node_bytes.len())
                        .ok_or(PropertyIndexError::LengthOverflow)?;

                    let mut patched_any = false;
                    if node_from_encode {
                        let mut node_page_patched = false;
                        if let Some(patches) = node_precmp_patches.take() {
                            let _ = node_store.take_dirty_node_hints();
                            if !patches.is_empty() {
                                let (page_size, pages_start) =
                                    paged_area_page_layout(&node_bytes)?;
                                for patch in &patches {
                                    let slot_off = patch
                                        .slot_index
                                        .checked_mul(page_size)
                                        .ok_or(PropertyIndexError::LengthOverflow)?;
                                    let abs = extent
                                        .addr
                                        .0
                                        .checked_add(node_offset as u64)
                                        .and_then(|v| v.checked_add(pages_start as u64))
                                        .and_then(|v| v.checked_add(slot_off as u64))
                                        .ok_or(PropertyIndexError::LengthOverflow)?;
                                    memory.write(abs, &patch.bytes);
                                }
                                crate::bench_profile::record_stat(
                                    "pidx_region_page_patch_pages",
                                    patches.len() as u64,
                                );
                                crate::bench_profile::record_stat(
                                    "pidx_region_page_patch_bytes",
                                    patches
                                        .iter()
                                        .map(|p| p.bytes.len() as u64)
                                        .sum::<u64>(),
                                );
                                patched_any = true;
                            }
                        } else {
                            let node_dirty_ids = node_store.take_dirty_node_hints();
                            if !node_dirty_ids.is_empty() {
                                if let Some(old_node) = old_node_slice_from_header(
                                    manager,
                                    memory,
                                    h,
                                )? {
                                    if let Some(patches) = node_store
                                        .try_build_paged_area_page_patches(&old_node, &node_dirty_ids)?
                                    {
                                        if !patches.is_empty() {
                                            let (page_size, pages_start) =
                                                paged_area_page_layout(&node_bytes)?;
                                            for patch in &patches {
                                                let slot_off = patch
                                                    .slot_index
                                                    .checked_mul(page_size)
                                                    .ok_or(PropertyIndexError::LengthOverflow)?;
                                                let abs = extent
                                                    .addr
                                                    .0
                                                    .checked_add(node_offset as u64)
                                                    .and_then(|v| v.checked_add(pages_start as u64))
                                                    .and_then(|v| v.checked_add(slot_off as u64))
                                                    .ok_or(PropertyIndexError::LengthOverflow)?;
                                                memory.write(abs, &patch.bytes);
                                            }
                                            crate::bench_profile::record_stat(
                                                "pidx_region_page_patch_pages",
                                                patches.len() as u64,
                                            );
                                            crate::bench_profile::record_stat(
                                                "pidx_region_page_patch_bytes",
                                                patches
                                                    .iter()
                                                    .map(|p| p.bytes.len() as u64)
                                                    .sum::<u64>(),
                                            );
                                            node_page_patched = true;
                                            patched_any = true;
                                        }
                                    } else {
                                        crate::bench_profile::record_stat(
                                            "pidx_region_page_patch_fallback",
                                            1,
                                        );
                                    }
                                }
                            }
                            if !node_page_patched {
                                patched_any = true;
                                memory.write(
                                    extent
                                        .addr
                                        .0
                                        .checked_add(node_offset as u64)
                                        .ok_or(PropertyIndexError::LengthOverflow)?,
                                    &node_bytes,
                                );
                                crate::bench_profile::record_stat(
                                    "pidx_region_section_write_node_bytes",
                                    node_bytes.len() as u64,
                                );
                            }
                        }
                    }
                    if edge_from_encode {
                        let mut edge_page_patched = false;
                        if let Some(patches) = edge_precmp_patches.take() {
                            let _ = edge_store.take_dirty_node_hints();
                            if !patches.is_empty() {
                                let (page_size, pages_start) =
                                    paged_area_page_layout(&edge_bytes)?;
                                for patch in &patches {
                                    let slot_off = patch
                                        .slot_index
                                        .checked_mul(page_size)
                                        .ok_or(PropertyIndexError::LengthOverflow)?;
                                    let abs = extent
                                        .addr
                                        .0
                                        .checked_add(edge_offset as u64)
                                        .and_then(|v| v.checked_add(pages_start as u64))
                                        .and_then(|v| v.checked_add(slot_off as u64))
                                        .ok_or(PropertyIndexError::LengthOverflow)?;
                                    memory.write(abs, &patch.bytes);
                                }
                                crate::bench_profile::record_stat(
                                    "pidx_region_page_patch_pages",
                                    patches.len() as u64,
                                );
                                crate::bench_profile::record_stat(
                                    "pidx_region_page_patch_bytes",
                                    patches
                                        .iter()
                                        .map(|p| p.bytes.len() as u64)
                                        .sum::<u64>(),
                                );
                                patched_any = true;
                            }
                        } else {
                            let edge_dirty_ids = edge_store.take_dirty_node_hints();
                            if !edge_dirty_ids.is_empty() {
                                if let Some(old_edge) = old_edge_slice_from_header(
                                    manager,
                                    memory,
                                    h,
                                )? {
                                    if let Some(patches) = edge_store
                                        .try_build_paged_area_page_patches(&old_edge, &edge_dirty_ids)?
                                    {
                                        if !patches.is_empty() {
                                            let (page_size, pages_start) =
                                                paged_area_page_layout(&edge_bytes)?;
                                            for patch in &patches {
                                                let slot_off = patch
                                                    .slot_index
                                                    .checked_mul(page_size)
                                                    .ok_or(PropertyIndexError::LengthOverflow)?;
                                                let abs = extent
                                                    .addr
                                                    .0
                                                    .checked_add(edge_offset as u64)
                                                    .and_then(|v| v.checked_add(pages_start as u64))
                                                    .and_then(|v| v.checked_add(slot_off as u64))
                                                    .ok_or(PropertyIndexError::LengthOverflow)?;
                                                memory.write(abs, &patch.bytes);
                                            }
                                            crate::bench_profile::record_stat(
                                                "pidx_region_page_patch_pages",
                                                patches.len() as u64,
                                            );
                                            crate::bench_profile::record_stat(
                                                "pidx_region_page_patch_bytes",
                                                patches
                                                    .iter()
                                                    .map(|p| p.bytes.len() as u64)
                                                    .sum::<u64>(),
                                            );
                                            edge_page_patched = true;
                                            patched_any = true;
                                        }
                                    } else {
                                        crate::bench_profile::record_stat(
                                            "pidx_region_page_patch_fallback",
                                            1,
                                        );
                                    }
                                }
                            }
                            if !edge_page_patched {
                                patched_any = true;
                                memory.write(
                                    extent
                                        .addr
                                        .0
                                        .checked_add(edge_offset as u64)
                                        .ok_or(PropertyIndexError::LengthOverflow)?,
                                    &edge_bytes,
                                );
                                crate::bench_profile::record_stat(
                                    "pidx_region_section_write_edge_bytes",
                                    edge_bytes.len() as u64,
                                );
                            }
                        }
                    }
                    if patched_any {
                        crate::bench_profile::record_stat("pidx_region_section_write_fast_path", 1);
                    }
                    node_store.pidx_side_must_flush = false;
                    edge_store.pidx_side_must_flush = false;
                    node_store.clear_dirty_node_hints();
                    edge_store.clear_dirty_node_hints();
                    return Ok(());
                }
            }
        }
    }

    let mut encoded = Vec::new();
    encoded.extend_from_slice(&header.encode());
    encoded.extend_from_slice(&snapshot_bytes);
    encoded.extend_from_slice(&node_bytes);
    encoded.extend_from_slice(&edge_bytes);

    {
        let _write = crate::canbench_scope::scope("pma_pidx_write_region");
        write_property_index_region_bytes(manager, memory, &encoded)?;
    }
    node_store.pidx_side_must_flush = false;
    edge_store.pidx_side_must_flush = false;
    node_store.clear_dirty_node_hints();
    edge_store.clear_dirty_node_hints();
    Ok(())
}

fn paged_area_page_layout(section_bytes: &[u8]) -> Result<(usize, usize), PropertyIndexError> {
    if section_bytes.len() < PropertyIndexNodeStore::PAGED_AREA_FIXED_HEADER_LEN {
        return Err(PropertyIndexError::RecordTooShort(section_bytes.len()));
    }
    if section_bytes[..4] != PropertyIndexNodeStore::PAGED_AREA_MAGIC {
        return Err(PropertyIndexError::InvalidPagedAreaMagic(
            section_bytes[..4].to_vec(),
        ));
    }
    let version = section_bytes[4];
    let allocator_start = 5;
    let allocator_end = allocator_start + PropertyIndexAllocatorHeader::ENCODED_LEN;
    let allocator = PropertyIndexAllocatorHeader::decode(&section_bytes[allocator_start..allocator_end])?;
    let mut free_count = [0u8; 4];
    free_count.copy_from_slice(&section_bytes[allocator_end..allocator_end + 4]);
    let free_count = u32::from_le_bytes(free_count) as usize;
    let page_size = usize::try_from(allocator.page_size_bytes)
        .map_err(|_| PropertyIndexError::LengthOverflow)?;
    let pages_start = PropertyIndexNodeStore::paged_area_pages_offset(version, free_count)?;
    Ok((page_size, pages_start))
}

fn old_node_slice_from_header(
    manager: &RegionManager,
    memory: &impl Memory,
    h: &PropertyIndexRegionHeader,
) -> Result<Option<Vec<u8>>, PropertyIndexError> {
    if h.node_store_len == 0 {
        return Ok(None);
    }
    let off = PropertyIndexRegionHeader::ENCODED_LEN
        .checked_add(h.snapshot_len as usize)
        .ok_or(PropertyIndexError::LengthOverflow)?;
    Ok(Some(read_property_index_region_slice(
        manager,
        memory,
        off,
        h.node_store_len as usize,
    )?))
}

fn old_edge_slice_from_header(
    manager: &RegionManager,
    memory: &impl Memory,
    h: &PropertyIndexRegionHeader,
) -> Result<Option<Vec<u8>>, PropertyIndexError> {
    if h.edge_store_len == 0 {
        return Ok(None);
    }
    let off = PropertyIndexRegionHeader::ENCODED_LEN
        .checked_add(h.snapshot_len as usize)
        .and_then(|v| v.checked_add(h.node_store_len as usize))
        .ok_or(PropertyIndexError::LengthOverflow)?;
    Ok(Some(read_property_index_region_slice(
        manager,
        memory,
        off,
        h.edge_store_len as usize,
    )?))
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
        pidx_dirty_node_hints: BTreeSet::new(),
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
            // Always diff the common prefix, even when logical length changes.
            // This avoids full rewrites when only small ranges changed.
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
                    crate::bench_profile::record_stat("pidx_region_tail_write_bytes", tail.len() as u64);
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
            } else if encoded.is_empty() && old_logical == 0 {
                // no-op
            } else {
                crate::bench_profile::record_stat(
                    "pidx_region_full_write_bytes",
                    0,
                );
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
    use super::{
        paged_area_page_layout, read_property_index_region_bytes,
        write_property_index_paged_stores_to_stable_memory, write_property_index_region_bytes,
    };
    use crate::low_level::{BucketSizeInPages, ExtentChain, ExtentId, RegionKind, RegionManager, WasmPages};
    use crate::property_index::{PropertyIndex, PropertyIndexEntry, PropertyIndexKey, PropertyIndexNodeStore};
    use crate::stable::Memory;
    use crate::VecMemory;
    use gleaph_graph_kernel::NodeId;

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
        if capacity > 0 {
            memory.write(extent.addr.0, &vec![0u8; capacity]);
        }
        (manager, memory, extent.addr.0, capacity)
    }

    fn reference_full_write_with_clear(
        memory: &VecMemory,
        base_addr: u64,
        old_logical_len: usize,
        encoded: &[u8],
    ) {
        if !encoded.is_empty() {
            memory.write(base_addr, encoded);
        }
        if old_logical_len > encoded.len() {
            let clear_len = old_logical_len - encoded.len();
            if clear_len > 0 {
                memory.write(base_addr + encoded.len() as u64, &vec![0u8; clear_len]);
            }
        }
    }

    fn read_extent_bytes(memory: &VecMemory, base_addr: u64, len: usize) -> Vec<u8> {
        let mut out = vec![0u8; len];
        if len > 0 {
            memory.read(base_addr, &mut out);
        }
        out
    }

    #[test]
    fn extent_write_matches_reference_on_same_length_diff() {
        let old = (0..128u8).collect::<Vec<_>>();
        let mut new = old.clone();
        for i in [3usize, 4, 5, 80, 81, 120] {
            new[i] ^= 0x7F;
        }

        let (mut manager_a, memory_a, base_a, capacity) = setup_property_index_extent(old.len(), 2);
        let (mut manager_b, memory_b, base_b, _) = setup_property_index_extent(old.len(), 2);
        memory_a.write(base_a, &old);
        memory_b.write(base_b, &old);

        write_property_index_region_bytes(&mut manager_a, &memory_a, &new).expect("optimized write");
        manager_b
            .set_region_logical_len(RegionKind::PropertyIndex, new.len() as u64)
            .expect("set logical len");
        reference_full_write_with_clear(&memory_b, base_b, old.len(), &new);

        let a = read_extent_bytes(&memory_a, base_a, capacity);
        let b = read_extent_bytes(&memory_b, base_b, capacity);
        assert_eq!(a, b);
    }

    #[test]
    fn extent_write_matches_reference_on_growth_with_prefix_changes() {
        let old = (0..96u8).collect::<Vec<_>>();
        let mut new = old.clone();
        for i in [0usize, 31, 63, 95] {
            new[i] = new[i].wrapping_add(11);
        }
        new.extend((0..24u8).map(|v| v.wrapping_mul(3)));

        let (mut manager_a, memory_a, base_a, capacity) = setup_property_index_extent(old.len(), 2);
        let (mut manager_b, memory_b, base_b, _) = setup_property_index_extent(old.len(), 2);
        memory_a.write(base_a, &old);
        memory_b.write(base_b, &old);

        write_property_index_region_bytes(&mut manager_a, &memory_a, &new).expect("optimized write");
        manager_b
            .set_region_logical_len(RegionKind::PropertyIndex, new.len() as u64)
            .expect("set logical len");
        reference_full_write_with_clear(&memory_b, base_b, old.len(), &new);

        let a = read_extent_bytes(&memory_a, base_a, capacity);
        let b = read_extent_bytes(&memory_b, base_b, capacity);
        assert_eq!(a, b);
    }

    #[test]
    fn extent_write_matches_reference_on_shrink_with_prefix_changes() {
        let old = (0..160u8).collect::<Vec<_>>();
        let mut new = old[..88].to_vec();
        for i in [2usize, 40, 70, 87] {
            new[i] = new[i].wrapping_sub(9);
        }

        let (mut manager_a, memory_a, base_a, capacity) = setup_property_index_extent(old.len(), 2);
        let (mut manager_b, memory_b, base_b, _) = setup_property_index_extent(old.len(), 2);
        memory_a.write(base_a, &old);
        memory_b.write(base_b, &old);

        write_property_index_region_bytes(&mut manager_a, &memory_a, &new).expect("optimized write");
        manager_b
            .set_region_logical_len(RegionKind::PropertyIndex, new.len() as u64)
            .expect("set logical len");
        reference_full_write_with_clear(&memory_b, base_b, old.len(), &new);

        let a = read_extent_bytes(&memory_a, base_a, capacity);
        let b = read_extent_bytes(&memory_b, base_b, capacity);
        assert_eq!(a, b);
    }

    #[test]
    fn page_patch_equivalent_to_full_encode_single_node_change() {
        let page_size = 256u32;
        let mut old_index = PropertyIndex::new(64);
        old_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "name", b"alice".to_vec()),
            PropertyIndexEntry { payload: b"v1".to_vec() },
        );
        old_index.insert(
            PropertyIndexKey::node(NodeId::from(2u8), "name", b"bob".to_vec()),
            PropertyIndexEntry { payload: b"v1".to_vec() },
        );

        let old_store =
            PropertyIndexNodeStore::try_from_index(&old_index, page_size).expect("old store");
        let old_bytes = old_store.encode_paged_area().expect("old paged");

        let mut new_index = old_index.clone();
        new_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "name", b"alice".to_vec()),
            PropertyIndexEntry { payload: b"v2".to_vec() },
        );
        let new_store =
            PropertyIndexNodeStore::try_from_index(&new_index, page_size).expect("new store");
        let new_bytes = new_store.encode_paged_area().expect("new paged");

        let delta = new_store.diff_against(&old_store);
        let patches = new_store
            .try_build_paged_area_page_patches(&old_bytes, &delta.touched_node_ids)
            .expect("patch plan")
            .expect("patchable");

        let mut patched = old_bytes.clone();
        let (slot_size, pages_start) = paged_area_page_layout(&patched).expect("layout");
        for patch in patches {
            let off = pages_start + patch.slot_index * slot_size;
            patched[off..off + slot_size].copy_from_slice(&patch.bytes);
        }
        assert_eq!(patched, new_bytes);
    }

    #[test]
    fn page_patch_falls_back_with_overflow_pages() {
        let page_size = 128u32;
        let mut old_index = PropertyIndex::new(64);
        old_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "blob", vec![7u8; 700]),
            PropertyIndexEntry { payload: vec![9u8; 700] },
        );

        let old_store =
            PropertyIndexNodeStore::try_from_index(&old_index, page_size).expect("old store");
        let old_bytes = old_store.encode_paged_area().expect("old paged");

        let mut new_index = old_index.clone();
        new_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "blob", vec![8u8; 700]),
            PropertyIndexEntry { payload: vec![10u8; 700] },
        );
        let new_store =
            PropertyIndexNodeStore::try_from_index(&new_index, page_size).expect("new store");
        let new_bytes = new_store.encode_paged_area().expect("new paged");

        let delta = new_store.diff_against(&old_store);
        let patches = new_store
            .try_build_paged_area_page_patches(&old_bytes, &delta.touched_node_ids)
            .expect("patch plan");
        assert!(patches.is_none(), "overflow-heavy change should fallback");
        assert_ne!(old_bytes, new_bytes, "setup must include an actual change");
    }

    #[test]
    fn patch_first_flush_matches_full_region_image() {
        use crate::property_index::{
            PropertyIndexRegionHeader, PropertyIndexSnapshot, PropertyIndexStorageImage,
        };

        let page_size = 256u32;
        let branching = 64u16;
        let mut old_index = PropertyIndex::new(64);
        old_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "name", b"alice".to_vec()),
            PropertyIndexEntry { payload: b"v1".to_vec() },
        );
        old_index.insert(
            PropertyIndexKey::node(NodeId::from(2u8), "name", b"bob".to_vec()),
            PropertyIndexEntry { payload: b"v1".to_vec() },
        );

        let empty_index = PropertyIndex::new(64);
        let old_node_store =
            PropertyIndexNodeStore::try_from_index(&old_index, page_size).expect("old node store");
        let edge_store_template =
            PropertyIndexNodeStore::try_from_index(&empty_index, page_size).expect("edge store");
        let old_node_bytes = old_node_store.encode_paged_area().expect("old node paged");
        let edge_bytes = edge_store_template
            .encode_paged_area()
            .expect("edge paged");

        let snapshot_bytes = PropertyIndexSnapshot::empty(branching)
            .encode()
            .expect("snap");
        let header_old = PropertyIndexRegionHeader {
            version: PropertyIndexStorageImage::VERSION,
            reserved: [0; 3],
            snapshot_len: u32::try_from(snapshot_bytes.len()).unwrap(),
            node_store_len: u32::try_from(old_node_bytes.len()).unwrap(),
            edge_store_len: u32::try_from(edge_bytes.len()).unwrap(),
        };
        let mut initial_region = Vec::new();
        initial_region.extend_from_slice(&header_old.encode());
        initial_region.extend_from_slice(&snapshot_bytes);
        initial_region.extend_from_slice(&old_node_bytes);
        initial_region.extend_from_slice(&edge_bytes);

        let mut new_index = old_index.clone();
        new_index.insert(
            PropertyIndexKey::node(NodeId::from(1u8), "name", b"alice".to_vec()),
            PropertyIndexEntry { payload: b"v2".to_vec() },
        );
        let new_node_store =
            PropertyIndexNodeStore::try_from_index(&new_index, page_size).expect("new node store");
        let new_node_bytes = new_node_store.encode_paged_area().expect("new node paged");
        let header_new = PropertyIndexRegionHeader {
            version: PropertyIndexStorageImage::VERSION,
            reserved: [0; 3],
            snapshot_len: u32::try_from(snapshot_bytes.len()).unwrap(),
            node_store_len: u32::try_from(new_node_bytes.len()).unwrap(),
            edge_store_len: u32::try_from(edge_bytes.len()).unwrap(),
        };
        let mut reference_region = Vec::new();
        reference_region.extend_from_slice(&header_new.encode());
        reference_region.extend_from_slice(&snapshot_bytes);
        reference_region.extend_from_slice(&new_node_bytes);
        reference_region.extend_from_slice(&edge_bytes);

        let (mut manager, memory, base, _) =
            setup_property_index_extent(initial_region.len(), 64);
        memory.write(base, &initial_region);
        manager
            .set_region_logical_len(RegionKind::PropertyIndex, initial_region.len() as u64)
            .expect("logical len");

        let delta = new_node_store.diff_against(&old_node_store);
        let mut node_store_flush = new_node_store.clone();
        node_store_flush.pidx_side_must_flush = true;
        node_store_flush.note_dirty_node_ids(delta.touched_node_ids.iter().copied());

        let mut edge_store_flush = edge_store_template.clone();
        edge_store_flush.pidx_side_must_flush = false;

        write_property_index_paged_stores_to_stable_memory(
            &mut manager,
            &memory,
            branching,
            &mut node_store_flush,
            &mut edge_store_flush,
        )
        .expect("flush");

        let region = manager
            .layout
            .region(RegionKind::PropertyIndex)
            .expect("property index region");
        let logical_len = usize::try_from(region.logical_len_bytes).unwrap();
        assert_eq!(logical_len, reference_region.len());

        let got = read_property_index_region_bytes(&manager, &memory).expect("read region");
        assert_eq!(got, reference_region);
    }
}
