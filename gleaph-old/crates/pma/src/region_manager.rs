//! Unified stable-memory region manager for Gleaph non-PMA regions.
//!
//! Both `pma.rs` (PMA engine) and `gleaph-graph::state` (canister layer) share the
//! same on-disk format for the "reserved metadata" area in [`gleaph_types::StableHeader`].
//! This module provides the single source of truth for:
//!
//! - Binary layout constants and struct definitions
//! - [`ReservedPersistMeta`] – overlay snapshot location and canister config
//! - [`ReservedRegionsMeta`] – non-PMA region allocation table
//! - I/O helpers: [`read_metas`], [`write_metas`]
//! - [`relocate_non_pma_regions`] – shift all non-PMA spans when PMA grows
//! - [`validate_layout`] – check that spans do not overlap
//! - [`RegionManager`] – allocate, relocate, validate via a memory handle

use gleaph_types::{GleaphError, UsageQuota};

use crate::{
    abp_tree::{ABP_STORE_HEADER_LEN, AbpStoreHeader},
    layout,
    memory::Memory,
};

// ── Layout constants ──────────────────────────────────────────────────────────

/// Magic number for graph persist metadata (`GMET`).
pub const GRAPH_META_MAGIC: u32 = 0x474D_4554;
pub const GRAPH_META_VERSION: u16 = 3;

/// Magic number for graph regions metadata (`GREG`).
pub const GRAPH_REGIONS_META_MAGIC: u32 = 0x4752_4547;
pub const GRAPH_REGIONS_META_VERSION: u16 = 1;

/// Byte length occupied by [`ReservedPersistMeta`] in `StableHeader._reserved`.
pub const GRAPH_META_RESERVED_LEN: usize = 44;
/// Byte offset where [`ReservedRegionsMeta`] starts within `StableHeader._reserved`.
pub const GRAPH_REGIONS_META_OFFSET: usize = 48;
/// Byte length occupied by [`ReservedRegionsMeta`].
///
pub const GRAPH_REGIONS_META_LEN: usize = 96;

// ── ReservedPersistMeta ───────────────────────────────────────────────────────

/// Persist metadata stored at offset 0 of `StableHeader._reserved`.
///
/// Records the canister configuration and the byte range of the overlay snapshot blob.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReservedPersistMeta {
    pub magic: u32,
    pub version: u16,
    pub _pad: u16,
    pub max_vertices: u32,
    pub overlay_offset: u64,
    pub overlay_len: u32,
    pub overlay_alloc_len: u32,
    pub quota_max_vertices: u64,
    pub quota_max_edges: u64,
}

impl ReservedPersistMeta {
    /// Creates a new metadata record stamped with the current version.
    pub fn new(
        max_vertices: u32,
        overlay_offset: u64,
        overlay_len: u32,
        overlay_alloc_len: u32,
    ) -> Self {
        Self::new_with_quota(
            max_vertices,
            overlay_offset,
            overlay_len,
            overlay_alloc_len,
            UsageQuota::default(),
        )
    }

    pub fn new_with_quota(
        max_vertices: u32,
        overlay_offset: u64,
        overlay_len: u32,
        overlay_alloc_len: u32,
        quota: UsageQuota,
    ) -> Self {
        Self {
            magic: GRAPH_META_MAGIC,
            version: GRAPH_META_VERSION,
            _pad: 0,
            max_vertices,
            overlay_offset,
            overlay_len,
            // Treat zero alloc_len as "same as len" (backward compat with older on-disk data).
            overlay_alloc_len: if overlay_alloc_len == 0 {
                overlay_len
            } else {
                overlay_alloc_len
            },
            quota_max_vertices: quota.max_vertices,
            quota_max_edges: quota.max_edges,
        }
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < GRAPH_META_RESERVED_LEN {
            return None;
        }
        let magic = u32::from_le_bytes(bytes[0..4].try_into().ok()?);
        let version = u16::from_le_bytes(bytes[4..6].try_into().ok()?);
        let _pad = u16::from_le_bytes(bytes[6..8].try_into().ok()?);
        let max_vertices = u32::from_le_bytes(bytes[8..12].try_into().ok()?);
        let overlay_offset = u64::from_le_bytes(bytes[12..20].try_into().ok()?);
        let overlay_len = u32::from_le_bytes(bytes[20..24].try_into().ok()?);
        let overlay_alloc_len = u32::from_le_bytes(bytes[24..28].try_into().ok()?);
        let quota_max_vertices = u64::from_le_bytes(bytes[28..36].try_into().ok()?);
        let quota_max_edges = u64::from_le_bytes(bytes[36..44].try_into().ok()?);
        let overlay_alloc_len = if overlay_alloc_len == 0 {
            overlay_len
        } else {
            overlay_alloc_len
        };
        Some(Self {
            magic,
            version,
            _pad,
            max_vertices,
            overlay_offset,
            overlay_len,
            overlay_alloc_len,
            quota_max_vertices,
            quota_max_edges,
        })
    }

    pub fn encode(self) -> [u8; GRAPH_META_RESERVED_LEN] {
        let mut out = [0u8; GRAPH_META_RESERVED_LEN];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..6].copy_from_slice(&self.version.to_le_bytes());
        out[6..8].copy_from_slice(&self._pad.to_le_bytes());
        out[8..12].copy_from_slice(&self.max_vertices.to_le_bytes());
        out[12..20].copy_from_slice(&self.overlay_offset.to_le_bytes());
        out[20..24].copy_from_slice(&self.overlay_len.to_le_bytes());
        out[24..28].copy_from_slice(&self.overlay_alloc_len.to_le_bytes());
        out[28..36].copy_from_slice(&self.quota_max_vertices.to_le_bytes());
        out[36..44].copy_from_slice(&self.quota_max_edges.to_le_bytes());
        out
    }

    pub fn is_valid(&self) -> bool {
        self.magic == GRAPH_META_MAGIC && self.version == GRAPH_META_VERSION
    }

    pub fn quota(&self) -> UsageQuota {
        UsageQuota {
            max_vertices: self.quota_max_vertices,
            max_edges: self.quota_max_edges,
        }
    }

    /// Returns `(overlay_offset, effective_alloc_len)` as a span tuple, or `None` if empty.
    pub fn overlay_span(&self) -> Option<(u64, u64)> {
        let len = u64::from(self.overlay_alloc_len.max(self.overlay_len));
        if len == 0 {
            None
        } else {
            Some((self.overlay_offset, len))
        }
    }
}

// ── ReservedRegionsMeta ───────────────────────────────────────────────────────

/// Region allocation table stored at [`GRAPH_REGIONS_META_OFFSET`] in `StableHeader._reserved`.
///
/// Tracks stable-memory spans of non-PMA regions: property store, secondary index.
/// `non_pma_base` is the byte address where non-PMA data begins; PMA must not grow past it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReservedRegionsMeta {
    pub magic: u32,
    pub version: u16,
    pub _pad: u16,
    pub property_store_offset: u64,
    pub property_store_len: u64,
    pub secondary_index_offset: u64,
    pub secondary_index_len: u64,
    pub non_pma_base: u64,
    /// Byte offset of the vertex tombstone bitset region (0 = not yet allocated).
    pub vertex_tombstone_offset: u64,
    /// Byte length allocated for the vertex tombstone bitset region (0 = not yet allocated).
    pub vertex_tombstone_len: u64,
    /// Byte offset of the `VertexMetaTable` B+ tree region (0 = not yet allocated).
    pub vertex_meta_offset: u64,
    /// Byte length allocated for the `VertexMetaTable` region (0 = not yet allocated).
    pub vertex_meta_len: u64,
    /// Byte offset of the admin/config catalog blob region (0 = not yet allocated).
    pub config_catalog_offset: u64,
    /// Byte length allocated for the admin/config catalog blob region (0 = not yet allocated).
    pub config_catalog_len: u64,
}

impl Default for ReservedRegionsMeta {
    fn default() -> Self {
        Self {
            magic: GRAPH_REGIONS_META_MAGIC,
            version: GRAPH_REGIONS_META_VERSION,
            _pad: 0,
            property_store_offset: 0,
            property_store_len: 0,
            secondary_index_offset: 0,
            secondary_index_len: 0,
            non_pma_base: 0,
            vertex_tombstone_offset: 0,
            vertex_tombstone_len: 0,
            vertex_meta_offset: 0,
            vertex_meta_len: 0,
            config_catalog_offset: 0,
            config_catalog_len: 0,
        }
    }
}

impl ReservedRegionsMeta {
    /// Alias for `Default::default()` — creates a valid, empty regions record.
    pub fn new_valid() -> Self {
        Self::default()
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        if bytes.len() < 44 {
            return None;
        }
        Some(Self {
            magic: u32::from_le_bytes(bytes[0..4].try_into().ok()?),
            version: u16::from_le_bytes(bytes[4..6].try_into().ok()?),
            _pad: u16::from_le_bytes(bytes[6..8].try_into().ok()?),
            property_store_offset: u64::from_le_bytes(bytes[8..16].try_into().ok()?),
            property_store_len: u64::from_le_bytes(bytes[16..24].try_into().ok()?),
            secondary_index_offset: u64::from_le_bytes(bytes[24..32].try_into().ok()?),
            secondary_index_len: u64::from_le_bytes(bytes[32..40].try_into().ok()?),
            non_pma_base: if bytes.len() >= 48 {
                u64::from_le_bytes(bytes[40..48].try_into().ok()?)
            } else {
                0
            },
            vertex_tombstone_offset: if bytes.len() >= 56 {
                u64::from_le_bytes(bytes[48..56].try_into().ok()?)
            } else {
                0
            },
            vertex_tombstone_len: if bytes.len() >= 64 {
                u64::from_le_bytes(bytes[56..64].try_into().ok()?)
            } else {
                0
            },
            vertex_meta_offset: if bytes.len() >= 72 {
                u64::from_le_bytes(bytes[64..72].try_into().ok()?)
            } else {
                0
            },
            vertex_meta_len: if bytes.len() >= 80 {
                u64::from_le_bytes(bytes[72..80].try_into().ok()?)
            } else {
                0
            },
            config_catalog_offset: if bytes.len() >= 88 {
                u64::from_le_bytes(bytes[80..88].try_into().ok()?)
            } else {
                0
            },
            config_catalog_len: if bytes.len() >= 96 {
                u64::from_le_bytes(bytes[88..96].try_into().ok()?)
            } else {
                0
            },
        })
    }

    pub fn encode(self) -> [u8; GRAPH_REGIONS_META_LEN] {
        let mut out = [0u8; GRAPH_REGIONS_META_LEN];
        out[0..4].copy_from_slice(&self.magic.to_le_bytes());
        out[4..6].copy_from_slice(&self.version.to_le_bytes());
        out[6..8].copy_from_slice(&self._pad.to_le_bytes());
        out[8..16].copy_from_slice(&self.property_store_offset.to_le_bytes());
        out[16..24].copy_from_slice(&self.property_store_len.to_le_bytes());
        out[24..32].copy_from_slice(&self.secondary_index_offset.to_le_bytes());
        out[32..40].copy_from_slice(&self.secondary_index_len.to_le_bytes());
        out[40..48].copy_from_slice(&self.non_pma_base.to_le_bytes());
        out[48..56].copy_from_slice(&self.vertex_tombstone_offset.to_le_bytes());
        out[56..64].copy_from_slice(&self.vertex_tombstone_len.to_le_bytes());
        out[64..72].copy_from_slice(&self.vertex_meta_offset.to_le_bytes());
        out[72..80].copy_from_slice(&self.vertex_meta_len.to_le_bytes());
        out[80..88].copy_from_slice(&self.config_catalog_offset.to_le_bytes());
        out[88..96].copy_from_slice(&self.config_catalog_len.to_le_bytes());
        out
    }

    pub fn is_valid(&self) -> bool {
        self.magic == GRAPH_REGIONS_META_MAGIC && self.version == GRAPH_REGIONS_META_VERSION
    }

    /// Sets `non_pma_base` to the minimum occupied offset when it has not been recorded yet.
    ///
    /// Pass `overlay: Some((offset, len))` to include the overlay snapshot in the inference;
    /// pass `None` to consider only property store and secondary index.
    pub fn infer_non_pma_base_if_missing(&mut self, overlay: Option<(u64, u64)>) {
        if self.non_pma_base != 0 {
            return;
        }
        let candidates = [
            overlay,
            if self.property_store_len > 0 {
                Some((self.property_store_offset, self.property_store_len))
            } else {
                None
            },
            if self.secondary_index_len > 0 {
                Some((self.secondary_index_offset, self.secondary_index_len))
            } else {
                None
            },
            if self.vertex_tombstone_len > 0 {
                Some((self.vertex_tombstone_offset, self.vertex_tombstone_len))
            } else {
                None
            },
            if self.vertex_meta_len > 0 {
                Some((self.vertex_meta_offset, self.vertex_meta_len))
            } else {
                None
            },
            if self.config_catalog_len > 0 {
                Some((self.config_catalog_offset, self.config_catalog_len))
            } else {
                None
            },
        ];
        self.non_pma_base = candidates
            .into_iter()
            .flatten()
            .filter(|(_, len)| *len > 0)
            .map(|(off, _)| off)
            .min()
            .unwrap_or(0);
    }
}

// ── I/O helpers ───────────────────────────────────────────────────────────────

/// Reads both metadata records from stable memory.
///
/// Returns `(None, None)` when the header magic is not present (fresh or invalid memory).
pub fn read_metas<M: Memory>(
    mem: &M,
) -> (Option<ReservedPersistMeta>, Option<ReservedRegionsMeta>) {
    use gleaph_types::STABLE_MAGIC;
    let header = layout::read_header(mem);
    if header.magic != STABLE_MAGIC {
        return (None, None);
    }
    let persist = ReservedPersistMeta::decode(&header._reserved[..GRAPH_META_RESERVED_LEN])
        .filter(ReservedPersistMeta::is_valid);
    let regions_end = GRAPH_REGIONS_META_OFFSET + GRAPH_REGIONS_META_LEN;
    let regions =
        ReservedRegionsMeta::decode(&header._reserved[GRAPH_REGIONS_META_OFFSET..regions_end])
            .filter(ReservedRegionsMeta::is_valid);
    (persist, regions)
}

/// Writes updated metadata records into the [`gleaph_types::StableHeader`] reserved area.
pub fn write_metas<M: Memory>(
    mem: &mut M,
    persist: Option<ReservedPersistMeta>,
    regions: Option<ReservedRegionsMeta>,
) -> Result<(), GleaphError> {
    use gleaph_types::STABLE_MAGIC;
    let mut header = layout::read_header(mem);
    if header.magic != STABLE_MAGIC {
        return Err(GleaphError::InvalidHeader);
    }
    if let Some(meta) = persist {
        header._reserved[..GRAPH_META_RESERVED_LEN].copy_from_slice(&meta.encode());
    }
    if let Some(meta) = regions {
        let end = GRAPH_REGIONS_META_OFFSET + GRAPH_REGIONS_META_LEN;
        header._reserved[GRAPH_REGIONS_META_OFFSET..end].copy_from_slice(&meta.encode());
    }
    layout::write_header(mem, &header);
    Ok(())
}

// ── Memory growth ─────────────────────────────────────────────────────────────

/// Grows backing memory to at least `required` bytes if it is currently smaller.
pub fn ensure_mem_size<M: Memory>(mem: &mut M, required: u64) -> Result<(), GleaphError> {
    let cur = mem.size_bytes();
    if required > cur {
        mem.grow(required - cur)
            .map_err(|e| GleaphError::Memory(e.to_string()))?;
    }
    Ok(())
}

/// Refreshes ABP-backed reserved-region lengths from live store headers.
///
/// Some reserved regions can grow independently of the PMA, so their recorded lengths
/// in [`ReservedRegionsMeta`] may lag behind the true allocated extent in stable memory.
/// This recomputes the allocated length for each ABP-backed region that has a readable
/// header at its current offset.
pub fn refresh_reserved_abp_region_lengths_from_headers<M: Memory>(
    mem: &M,
    regions: &mut ReservedRegionsMeta,
) -> Result<(), GleaphError> {
    fn allocated_extent<M: Memory>(mem: &M, offset: u64) -> Result<Option<u64>, GleaphError> {
        let Some(header) = AbpStoreHeader::read_from(mem, offset) else {
            return Ok(None);
        };
        let pages_bytes = u64::from(header.next_page_id)
            .checked_mul(u64::from(header.page_size))
            .ok_or_else(|| GleaphError::ExecutionError("ABP allocated extent overflow".into()))?;
        let total = ABP_STORE_HEADER_LEN
            .checked_add(pages_bytes)
            .ok_or_else(|| GleaphError::ExecutionError("ABP allocated extent overflow".into()))?;
        Ok(Some(total))
    }

    if regions.property_store_offset != 0
        && let Some(len) = allocated_extent(mem, regions.property_store_offset)?
    {
        regions.property_store_len = len;
    }
    if regions.secondary_index_offset != 0
        && let Some(len) = allocated_extent(mem, regions.secondary_index_offset)?
    {
        regions.secondary_index_len = len;
    }
    if regions.vertex_meta_offset != 0
        && let Some(len) = allocated_extent(mem, regions.vertex_meta_offset)?
    {
        regions.vertex_meta_len = len;
    }
    Ok(())
}

// ── Relocation ────────────────────────────────────────────────────────────────

/// Shifts all non-PMA regions upward when the PMA grows to `new_pma_end`.
///
/// Non-PMA regions include: overlay snapshot (if `persist` is provided), property store,
/// secondary index. Spans are moved highest-first to avoid clobbering live data.
///
/// Returns the updated [`ReservedRegionsMeta`] (the caller is also responsible for any
/// updated `persist` fields — see `RegionManager::relocate` for a complete wrapper).
pub fn relocate_non_pma_regions<M: Memory>(
    mem: &mut M,
    new_pma_end: u64,
    persist: Option<ReservedPersistMeta>,
    mut regions: ReservedRegionsMeta,
) -> Result<(Option<ReservedPersistMeta>, ReservedRegionsMeta), GleaphError> {
    let overlay = persist.and_then(|p| p.overlay_span());
    regions.infer_non_pma_base_if_missing(overlay);

    if regions.non_pma_base == 0 {
        regions.non_pma_base = new_pma_end;
        write_metas(mem, persist, Some(regions))?;
        return Ok((persist, regions));
    }
    if new_pma_end <= regions.non_pma_base {
        write_metas(mem, persist, Some(regions))?;
        return Ok((persist, regions));
    }

    let shift = new_pma_end - regions.non_pma_base;

    // Gather all occupied spans for movement.
    let mut spans: Vec<(u64, u64)> = Vec::new();
    if let Some((off, len)) = overlay {
        spans.push((off, len));
    }
    if regions.property_store_len > 0 {
        spans.push((regions.property_store_offset, regions.property_store_len));
    }
    if regions.secondary_index_len > 0 {
        spans.push((regions.secondary_index_offset, regions.secondary_index_len));
    }
    if regions.vertex_tombstone_len > 0 {
        spans.push((
            regions.vertex_tombstone_offset,
            regions.vertex_tombstone_len,
        ));
    }
    if regions.vertex_meta_len > 0 {
        spans.push((regions.vertex_meta_offset, regions.vertex_meta_len));
    }
    if regions.config_catalog_len > 0 {
        spans.push((regions.config_catalog_offset, regions.config_catalog_len));
    }
    spans.sort_by(|a, b| b.0.cmp(&a.0)); // descending offset → safe overlap-free copy

    let mut new_persist = persist;
    for (offset, len) in spans {
        let dst_offset = offset
            .checked_add(shift)
            .ok_or_else(|| GleaphError::ExecutionError("region relocation overflow".into()))?;
        let dst_end = dst_offset
            .checked_add(len)
            .ok_or_else(|| GleaphError::ExecutionError("region relocation overflow".into()))?;
        ensure_mem_size(mem, dst_end)?;
        let mut buf = vec![0u8; len as usize];
        mem.read(offset, &mut buf);
        mem.write(dst_offset, &buf);

        // Update metadata pointers that matched this span.
        if let Some(ref mut p) = new_persist
            && let Some((ov_off, ov_len)) = p.overlay_span()
            && ov_off == offset
            && ov_len == len
        {
            p.overlay_offset = dst_offset;
        }
        if regions.property_store_offset == offset && regions.property_store_len == len {
            regions.property_store_offset = dst_offset;
        }
        if regions.secondary_index_offset == offset && regions.secondary_index_len == len {
            regions.secondary_index_offset = dst_offset;
        }
        if regions.vertex_tombstone_offset == offset && regions.vertex_tombstone_len == len {
            regions.vertex_tombstone_offset = dst_offset;
        }
        if regions.vertex_meta_offset == offset && regions.vertex_meta_len == len {
            regions.vertex_meta_offset = dst_offset;
        }
        if regions.config_catalog_offset == offset && regions.config_catalog_len == len {
            regions.config_catalog_offset = dst_offset;
        }
    }

    regions.non_pma_base = new_pma_end;
    write_metas(mem, new_persist, Some(regions))?;
    Ok((new_persist, regions))
}

// ── Validation ────────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug)]
struct Span {
    start: u64,
    len: u64,
}

impl Span {
    fn end(self) -> Option<u64> {
        self.start.checked_add(self.len)
    }
}

/// Validates that stable-memory regions do not overlap and respect `non_pma_base`.
///
/// - `pma_end`: total bytes used by the PMA layer (= `layout::total_memory_needed(...)`)
/// - `persist`: overlay snapshot location
/// - `regions`: non-PMA region allocation table
pub fn validate_layout(
    pma_end: u64,
    persist: &ReservedPersistMeta,
    regions: &ReservedRegionsMeta,
) -> Result<(), GleaphError> {
    let overlay_len = u64::from(persist.overlay_alloc_len.max(persist.overlay_len));
    let spans = [
        (
            "pma",
            Span {
                start: 0,
                len: pma_end,
            },
        ),
        (
            "overlay",
            Span {
                start: persist.overlay_offset,
                len: overlay_len,
            },
        ),
        (
            "property_store",
            Span {
                start: regions.property_store_offset,
                len: regions.property_store_len,
            },
        ),
        (
            "secondary_index",
            Span {
                start: regions.secondary_index_offset,
                len: regions.secondary_index_len,
            },
        ),
        (
            "vertex_tombstone",
            Span {
                start: regions.vertex_tombstone_offset,
                len: regions.vertex_tombstone_len,
            },
        ),
        (
            "vertex_meta",
            Span {
                start: regions.vertex_meta_offset,
                len: regions.vertex_meta_len,
            },
        ),
        (
            "config_catalog",
            Span {
                start: regions.config_catalog_offset,
                len: regions.config_catalog_len,
            },
        ),
    ];

    // Validate each span's arithmetic doesn't overflow.
    for (name, span) in spans {
        if span.len == 0 {
            continue;
        }
        if span.end().is_none() {
            return Err(GleaphError::ExecutionError(format!(
                "stable-memory region overflow for {name}: start={} len={}",
                span.start, span.len
            )));
        }
    }

    // Check pair-wise overlaps.
    for i in 0..spans.len() {
        for j in (i + 1)..spans.len() {
            let (a_name, a) = spans[i];
            let (b_name, b) = spans[j];
            if a.len == 0 || b.len == 0 {
                continue;
            }
            let a_end = a.end().unwrap();
            let b_end = b.end().unwrap();
            if a.start < b_end && b.start < a_end {
                return Err(GleaphError::ExecutionError(format!(
                    "stable-memory region collision: {a_name}[{}..{}) overlaps {b_name}[{}..{})",
                    a.start, a_end, b.start, b_end
                )));
            }
        }
    }

    // Verify non_pma_base constraint.
    let non_pma_base = regions.non_pma_base;
    if non_pma_base != 0 {
        if pma_end > non_pma_base {
            return Err(GleaphError::ExecutionError(format!(
                "pma_end exceeds non_pma_base: pma_end={} non_pma_base={}",
                pma_end, non_pma_base
            )));
        }
        for (name, span) in [
            (
                "property_store",
                Span {
                    start: regions.property_store_offset,
                    len: regions.property_store_len,
                },
            ),
            (
                "secondary_index",
                Span {
                    start: regions.secondary_index_offset,
                    len: regions.secondary_index_len,
                },
            ),
            (
                "vertex_tombstone",
                Span {
                    start: regions.vertex_tombstone_offset,
                    len: regions.vertex_tombstone_len,
                },
            ),
            (
                "vertex_meta",
                Span {
                    start: regions.vertex_meta_offset,
                    len: regions.vertex_meta_len,
                },
            ),
            (
                "config_catalog",
                Span {
                    start: regions.config_catalog_offset,
                    len: regions.config_catalog_len,
                },
            ),
        ] {
            if span.len > 0 && span.start < non_pma_base {
                return Err(GleaphError::ExecutionError(format!(
                    "{name} region starts below non_pma_base: start={} non_pma_base={}",
                    span.start, non_pma_base
                )));
            }
        }
    }
    Ok(())
}

// ── RegionManager ─────────────────────────────────────────────────────────────

/// Identifies a known non-PMA storage region type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionKind {
    PropertyStore,
    SecondaryIndex,
    VertexTombstone,
    VertexMeta,
    ConfigCatalog,
}

/// Wraps a [`Memory`] backend to provide allocate/relocate/validate for non-PMA regions.
pub struct RegionManager<M: Memory>(pub M);

impl<M: Memory> RegionManager<M> {
    pub fn new(mem: M) -> Self {
        Self(mem)
    }

    pub fn memory(&self) -> &M {
        &self.0
    }

    pub fn memory_mut(&mut self) -> &mut M {
        &mut self.0
    }

    pub fn into_memory(self) -> M {
        self.0
    }

    /// Reads both metadata records from stable memory.
    pub fn read(&self) -> (Option<ReservedPersistMeta>, Option<ReservedRegionsMeta>) {
        read_metas(&self.0)
    }

    /// Writes updated metadata records to stable memory.
    pub fn write(
        &mut self,
        persist: Option<ReservedPersistMeta>,
        regions: Option<ReservedRegionsMeta>,
    ) -> Result<(), GleaphError> {
        write_metas(&mut self.0, persist, regions)
    }

    /// Relocates all non-PMA regions when the PMA grows to `new_pma_end`.
    ///
    /// Returns the (optionally updated) persist meta and updated regions meta.
    pub fn relocate(
        &mut self,
        new_pma_end: u64,
    ) -> Result<(Option<ReservedPersistMeta>, ReservedRegionsMeta), GleaphError> {
        let (persist, regions_opt) = read_metas(&self.0);
        let regions = regions_opt.unwrap_or_default();
        relocate_non_pma_regions(&mut self.0, new_pma_end, persist, regions)
    }

    /// Ensures a region of at least `min_len` bytes is allocated for `kind`.
    ///
    /// - `pma_end`: current PMA end byte (lower bound for new allocations)
    /// - `overlay_end`: end of overlay snapshot region (additional lower-bound constraint)
    ///
    /// Returns the start offset of the (existing or newly allocated) region.
    pub fn allocate(
        &mut self,
        pma_end: u64,
        overlay_end: u64,
        kind: RegionKind,
        min_len: u64,
    ) -> Result<u64, GleaphError> {
        let (persist, regions_opt) = read_metas(&self.0);
        let mut regions = regions_opt.unwrap_or_default();

        let overlay = persist.and_then(|p| p.overlay_span());
        regions.infer_non_pma_base_if_missing(overlay);
        if regions.non_pma_base == 0 {
            regions.non_pma_base = pma_end;
        }
        if regions.non_pma_base < pma_end {
            return Err(GleaphError::ExecutionError(format!(
                "cannot allocate non-PMA region below PMA end: non_pma_base={} pma_end={}",
                regions.non_pma_base, pma_end
            )));
        }

        // Find next free offset after all existing non-PMA data.
        let mut next_free = regions.non_pma_base.max(overlay_end);
        if regions.property_store_len > 0 {
            next_free = next_free.max(
                regions
                    .property_store_offset
                    .checked_add(regions.property_store_len)
                    .ok_or_else(|| {
                        GleaphError::ExecutionError("property region overflow".into())
                    })?,
            );
        }
        if regions.secondary_index_len > 0 {
            next_free = next_free.max(
                regions
                    .secondary_index_offset
                    .checked_add(regions.secondary_index_len)
                    .ok_or_else(|| {
                        GleaphError::ExecutionError("secondary region overflow".into())
                    })?,
            );
        }
        if regions.vertex_tombstone_len > 0 {
            next_free = next_free.max(
                regions
                    .vertex_tombstone_offset
                    .checked_add(regions.vertex_tombstone_len)
                    .ok_or_else(|| {
                        GleaphError::ExecutionError("vertex_tombstone region overflow".into())
                    })?,
            );
        }
        if regions.vertex_meta_len > 0 {
            next_free = next_free.max(
                regions
                    .vertex_meta_offset
                    .checked_add(regions.vertex_meta_len)
                    .ok_or_else(|| {
                        GleaphError::ExecutionError("vertex_meta region overflow".into())
                    })?,
            );
        }
        if regions.config_catalog_len > 0 {
            next_free = next_free.max(
                regions
                    .config_catalog_offset
                    .checked_add(regions.config_catalog_len)
                    .ok_or_else(|| {
                        GleaphError::ExecutionError("config_catalog region overflow".into())
                    })?,
            );
        }

        match kind {
            RegionKind::PropertyStore => {
                if regions.property_store_len == 0 {
                    regions.property_store_offset = next_free;
                }
                regions.property_store_len = regions.property_store_len.max(min_len);
            }
            RegionKind::SecondaryIndex => {
                if regions.secondary_index_len == 0 {
                    regions.secondary_index_offset = next_free;
                }
                regions.secondary_index_len = regions.secondary_index_len.max(min_len);
            }
            RegionKind::VertexTombstone => {
                if regions.vertex_tombstone_len == 0 {
                    regions.vertex_tombstone_offset = next_free;
                }
                regions.vertex_tombstone_len = regions.vertex_tombstone_len.max(min_len);
            }
            RegionKind::VertexMeta => {
                if regions.vertex_meta_len == 0 {
                    regions.vertex_meta_offset = next_free;
                }
                regions.vertex_meta_len = regions.vertex_meta_len.max(min_len);
            }
            RegionKind::ConfigCatalog => {
                if regions.config_catalog_len == 0 {
                    regions.config_catalog_offset = next_free;
                }
                regions.config_catalog_len = regions.config_catalog_len.max(min_len);
            }
        }

        // Grow memory if needed.
        let max_end = [
            regions
                .property_store_offset
                .saturating_add(regions.property_store_len),
            regions
                .secondary_index_offset
                .saturating_add(regions.secondary_index_len),
            regions
                .vertex_tombstone_offset
                .saturating_add(regions.vertex_tombstone_len),
            regions
                .vertex_meta_offset
                .saturating_add(regions.vertex_meta_len),
            regions
                .config_catalog_offset
                .saturating_add(regions.config_catalog_len),
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        ensure_mem_size(&mut self.0, max_end)?;
        write_metas(&mut self.0, None, Some(regions))?;

        Ok(match kind {
            RegionKind::PropertyStore => regions.property_store_offset,
            RegionKind::SecondaryIndex => regions.secondary_index_offset,
            RegionKind::VertexTombstone => regions.vertex_tombstone_offset,
            RegionKind::VertexMeta => regions.vertex_meta_offset,
            RegionKind::ConfigCatalog => regions.config_catalog_offset,
        })
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{VecMemory, pma::PmaGraph};

    /// Creates a fresh graph with a valid header, returns (graph, pma_end).
    fn make_graph_with_header() -> (PmaGraph<VecMemory>, u64) {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).expect("graph");
        g.write_header().expect("header");
        let pma_end = crate::layout::total_memory_needed(
            g.num_vertices,
            g.elem_capacity,
            u64::from(g.segment_count),
        );
        (g, pma_end)
    }

    #[test]
    fn region_allocate_and_free_property_then_secondary() {
        let (mut g, pma_end) = make_graph_with_header();
        let mut rm = RegionManager::new(g.mem.clone());

        // Allocate property store.
        let prop_off = rm
            .allocate(pma_end, 0, RegionKind::PropertyStore, 512)
            .expect("alloc prop");
        assert!(prop_off >= pma_end, "property store must be beyond PMA");

        // Retrieve updated memory.
        g.mem = rm.into_memory();
        let mut rm2 = RegionManager::new(g.mem.clone());

        // Allocate secondary index after the property store.
        let sec_off = rm2
            .allocate(pma_end, 0, RegionKind::SecondaryIndex, 256)
            .expect("alloc secondary");
        assert!(
            sec_off >= prop_off + 512,
            "secondary index must not overlap property store"
        );

        // Verify metadata persisted correctly.
        let (_, regions_opt) = rm2.read();
        let regions = regions_opt.expect("regions written");
        assert_eq!(regions.property_store_offset, prop_off);
        assert_eq!(regions.property_store_len, 512);
        assert_eq!(regions.secondary_index_offset, sec_off);
        assert_eq!(regions.secondary_index_len, 256);
    }

    #[test]
    fn region_relocate_on_pma_growth_moves_bytes_and_updates_metadata() {
        let (mut g, pma_end) = make_graph_with_header();

        // Write known byte patterns to the future region locations.
        let prop_off = pma_end + 1024;
        let sec_off = pma_end + 4096;
        let grow_to = sec_off + 32;
        g.mem.grow(grow_to - g.mem.size_bytes()).expect("grow");
        g.mem.write(prop_off, b"HELLO_PROP_STORE");
        g.mem.write(sec_off, b"HELLO_SEC_IDX");

        // Record regions metadata manually.
        let regions = ReservedRegionsMeta {
            property_store_offset: prop_off,
            property_store_len: 16,
            secondary_index_offset: sec_off,
            secondary_index_len: 13,
            non_pma_base: pma_end + 512,
            ..Default::default()
        };
        write_metas(&mut g.mem, None, Some(regions)).expect("write regions");

        // Simulate PMA growth past non_pma_base.
        let new_pma_end = regions.non_pma_base + 2048;
        let mut rm = RegionManager::new(g.mem);
        let (_, moved) = rm.relocate(new_pma_end).expect("relocate");

        assert_eq!(moved.non_pma_base, new_pma_end);
        assert_eq!(moved.property_store_offset, prop_off + 2048);
        assert_eq!(moved.secondary_index_offset, sec_off + 2048);

        // Verify bytes were physically copied to new locations.
        let mut prop_buf = [0u8; 16];
        rm.memory().read(moved.property_store_offset, &mut prop_buf);
        assert_eq!(&prop_buf, b"HELLO_PROP_STORE");

        let mut sec_buf = [0u8; 13];
        rm.memory().read(moved.secondary_index_offset, &mut sec_buf);
        assert_eq!(&sec_buf, b"HELLO_SEC_IDX");
    }

    #[test]
    fn region_validate_rejects_overlap() {
        let (_, pma_end) = make_graph_with_header();

        let persist = ReservedPersistMeta::new(8, pma_end + 100, 200, 200);
        // property_store overlaps the overlay region.
        let regions = ReservedRegionsMeta {
            property_store_offset: pma_end + 150,
            property_store_len: 300,
            ..Default::default()
        };
        let err = validate_layout(pma_end, &persist, &regions).expect_err("overlap must fail");
        assert!(matches!(err, GleaphError::ExecutionError(_)), "{err:?}");
    }

    #[test]
    fn region_validate_rejects_pma_non_pma_overlap() {
        let (_, pma_end) = make_graph_with_header();

        let persist = ReservedPersistMeta::new(8, 0, 0, 0);
        let regions = ReservedRegionsMeta {
            property_store_offset: pma_end.saturating_sub(64),
            property_store_len: 256,
            non_pma_base: pma_end + 1024,
            ..Default::default()
        };
        let err = validate_layout(pma_end, &persist, &regions)
            .expect_err("PMA/non-PMA overlap must fail");
        assert!(matches!(err, GleaphError::ExecutionError(_)), "{err:?}");
    }

    #[test]
    fn region_meta_round_trip_encode_decode() {
        let persist = ReservedPersistMeta::new(16, 65536, 1024, 2048);
        let encoded = persist.encode();
        let decoded = ReservedPersistMeta::decode(&encoded).expect("decode persist");
        assert_eq!(persist, decoded);

        let regions = ReservedRegionsMeta {
            property_store_offset: 1 << 20,
            property_store_len: 1 << 16,
            secondary_index_offset: (1 << 20) + (1 << 16),
            secondary_index_len: 1 << 15,
            non_pma_base: 1 << 20,
            ..Default::default()
        };
        let encoded = regions.encode();
        let decoded = ReservedRegionsMeta::decode(&encoded).expect("decode regions");
        assert_eq!(regions, decoded);
    }

    #[test]
    fn infer_non_pma_base_uses_minimum_offset_across_all_regions() {
        let mut regions = ReservedRegionsMeta {
            property_store_offset: 20_000,
            property_store_len: 500,
            secondary_index_offset: 30_000,
            secondary_index_len: 500,
            non_pma_base: 0,
            ..Default::default()
        };
        // Overlay at 10_000 should win as the minimum.
        regions.infer_non_pma_base_if_missing(Some((10_000, 100)));
        assert_eq!(regions.non_pma_base, 10_000);
    }

    #[test]
    fn infer_non_pma_base_without_overlay_uses_region_offsets() {
        let mut regions = ReservedRegionsMeta {
            property_store_offset: 10_000,
            property_store_len: 500,
            secondary_index_offset: 20_000,
            secondary_index_len: 500,
            non_pma_base: 0,
            ..Default::default()
        };
        regions.infer_non_pma_base_if_missing(None);
        assert_eq!(regions.non_pma_base, 10_000);
    }
}
