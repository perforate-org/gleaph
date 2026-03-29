//! Fixed-width byte-format adapters between low-level runtime state and stable memory.

use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;

use crate::stable::Memory;
use gleaph_graph_kernel::NodeId;

use super::edge::{EdgeEntry, EdgeMeta};
use super::manager::RegionManager;
use super::overflow::{LogOffset, OverflowEntry};
use super::region::{RegionKind, RegionManagerLayout, RegionRef};
use super::runtime::{ForwardSurfaceRuntime, ReverseSurfaceRuntime, SurfaceRuntime};
use super::surface::{ForwardSurface, ReverseSurface, SurfaceLayout};
use super::vertex::{EdgeIndex, VertexEntry, VertexLabelIndexEntry, VertexLabelRange};

const SERIALIZED_VERTEX_ENTRY_LEN: usize = 16;
const SERIALIZED_EDGE_ENTRY_LEN: usize = 8;
const SERIALIZED_OVERFLOW_ENTRY_LEN: usize = 20;
const SERIALIZED_LABEL_INDEX_HEADER_LEN: usize = 8;
const SERIALIZED_LABEL_INDEX_ENTRY_LEN: usize = 8;
const SERIALIZED_LABEL_RANGE_LEN: usize = 12;

/// Reads raw bytes for a logical region.
///
/// This is the first stable-memory-facing seam for the rewrite. The source may
/// come from real stable memory later, but the low-level hydration logic only
/// requires read access to region payload bytes.
pub trait RegionByteSource {
    fn region_bytes(&self, region: RegionRef) -> Option<&[u8]>;
}

/// Minimal in-memory region source used for tests and early adapters.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InMemoryRegionByteSource {
    regions: BTreeMap<RegionKind, Vec<u8>>,
}

impl InMemoryRegionByteSource {
    /// Creates an empty in-memory region source.
    pub fn new() -> Self {
        Self::default()
    }

    /// Stores one region payload by logical region kind.
    pub fn insert(&mut self, kind: RegionKind, bytes: Vec<u8>) {
        self.regions.insert(kind, bytes);
    }
}

impl RegionByteSource for InMemoryRegionByteSource {
    fn region_bytes(&self, region: RegionRef) -> Option<&[u8]> {
        self.regions.get(&region.region_kind()).map(Vec::as_slice)
    }
}

/// Region-byte source that snapshots extent-backed region payloads out of
/// stable memory using region-manager metadata.
///
/// This is still an adapter, not a live mapped view. It eagerly materializes
/// region bytes into memory so the existing decode/hydration pipeline can stay
/// slice-based.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StableMemoryRegionByteSource {
    regions: BTreeMap<RegionKind, Vec<u8>>,
}

impl StableMemoryRegionByteSource {
    /// Materializes extent-backed region payloads from stable memory using manager metadata.
    pub fn from_region_manager(
        manager: &RegionManager,
        memory: &impl Memory,
    ) -> Result<Self, HydrationError> {
        let mut regions = BTreeMap::new();
        for entry in manager.layout.directory.iter() {
            let region = entry.region;
            if region.storage_kind() != super::region::RegionStorageKind::Extent {
                continue;
            }
            let kind = region.region_kind();
            let extent = manager
                .region_extent(kind)
                .ok_or(HydrationError::MissingExtentRegion(kind))?;
            let logical_len = usize::try_from(region.logical_len_bytes)
                .map_err(|_| HydrationError::RegionTooLarge(kind, region.logical_len_bytes))?;
            if logical_len > usize::try_from(extent.len_bytes).unwrap_or(usize::MAX) {
                return Err(HydrationError::LogicalLengthExceedsExtent {
                    kind,
                    logical_len_bytes: region.logical_len_bytes,
                    extent_len_bytes: extent.len_bytes,
                });
            }
            let mut bytes = vec![0_u8; logical_len];
            if logical_len > 0 {
                memory.read(extent.addr.0, &mut bytes);
            }
            regions.insert(kind, bytes);
        }
        Ok(Self { regions })
    }
}

impl RegionByteSource for StableMemoryRegionByteSource {
    fn region_bytes(&self, region: RegionRef) -> Option<&[u8]> {
        self.regions.get(&region.region_kind()).map(Vec::as_slice)
    }
}

/// Write-time failures while serializing low-level runtime state back into
/// stable-memory-backed regions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum WritebackError {
    MissingRegionDefinition(RegionKind),
    MissingExtentRegion(RegionKind),
    LengthMismatch {
        kind: RegionKind,
        expected: u64,
        actual: u64,
    },
    LogicalLengthExceedsExtent {
        kind: RegionKind,
        logical_len_bytes: u64,
        extent_len_bytes: u64,
    },
    MemoryGrowFailed {
        current_pages: u64,
        delta_pages: u64,
    },
}

impl fmt::Display for WritebackError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WritebackError::MissingRegionDefinition(kind) => {
                write!(f, "missing region definition for {:?}", kind)
            }
            WritebackError::MissingExtentRegion(kind) => {
                write!(f, "missing extent metadata for region {:?}", kind)
            }
            WritebackError::LengthMismatch {
                kind,
                expected,
                actual,
            } => write!(
                f,
                "region {:?} length mismatch: expected {} bytes, got {} bytes",
                kind, expected, actual
            ),
            WritebackError::LogicalLengthExceedsExtent {
                kind,
                logical_len_bytes,
                extent_len_bytes,
            } => write!(
                f,
                "region {:?} logical length {} exceeds extent length {}",
                kind, logical_len_bytes, extent_len_bytes
            ),
            WritebackError::MemoryGrowFailed {
                current_pages,
                delta_pages,
            } => write!(
                f,
                "failed to grow memory from {} pages by {} pages",
                current_pages, delta_pages
            ),
        }
    }
}

impl Error for WritebackError {}

/// Decode-time failures while hydrating low-level runtime state from bytes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HydrationError {
    MissingRegionDefinition(RegionKind),
    MissingRegion(RegionKind),
    MissingExtentRegion(RegionKind),
    RegionTooLarge(RegionKind, u64),
    LogicalLengthExceedsExtent {
        kind: RegionKind,
        logical_len_bytes: u64,
        extent_len_bytes: u64,
    },
    InvalidLength {
        kind: RegionKind,
        expected_multiple: usize,
        actual: usize,
    },
}

impl fmt::Display for HydrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HydrationError::MissingRegionDefinition(kind) => {
                write!(f, "missing region definition for {:?}", kind)
            }
            HydrationError::MissingExtentRegion(kind) => {
                write!(f, "missing extent metadata for region {:?}", kind)
            }
            HydrationError::RegionTooLarge(kind, logical_len_bytes) => write!(
                f,
                "region {:?} is too large to materialize on this platform: {} bytes",
                kind, logical_len_bytes
            ),
            HydrationError::LogicalLengthExceedsExtent {
                kind,
                logical_len_bytes,
                extent_len_bytes,
            } => write!(
                f,
                "region {:?} logical length {} exceeds extent length {}",
                kind, logical_len_bytes, extent_len_bytes
            ),
            HydrationError::MissingRegion(kind) => {
                write!(f, "missing bytes for region {:?}", kind)
            }
            HydrationError::InvalidLength {
                kind,
                expected_multiple,
                actual,
            } => write!(
                f,
                "region {:?} has invalid byte length {}; expected a multiple of {}",
                kind, actual, expected_multiple
            ),
        }
    }
}

impl Error for HydrationError {}

/// Pair of read-side runtimes for the forward and reverse adjacency surfaces.
///
/// This is the first adapter that lifts hydration from "one surface layout" to
/// "the graph's adjacency pair" as defined by the region-manager layout.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HydratedSurfaceRuntimes {
    pub forward: ForwardSurfaceRuntime,
    pub reverse: ReverseSurfaceRuntime,
}

impl HydratedSurfaceRuntimes {
    /// Bundles forward and reverse surface runtimes into one adjacency pair.
    pub const fn new(forward: ForwardSurfaceRuntime, reverse: ReverseSurfaceRuntime) -> Self {
        Self { forward, reverse }
    }
}

/// Reconstructs a typed forward surface from region-manager metadata.
pub fn forward_surface_from_layout(
    layout: &RegionManagerLayout,
) -> Result<ForwardSurface, HydrationError> {
    Ok(ForwardSurface::new(super::edge::SurfaceRegions::new(
        layout.region(RegionKind::ForwardVertexTable).ok_or(
            HydrationError::MissingRegionDefinition(RegionKind::ForwardVertexTable),
        )?,
        layout.region(RegionKind::ForwardEdgeEntries).ok_or(
            HydrationError::MissingRegionDefinition(RegionKind::ForwardEdgeEntries),
        )?,
        layout.region(RegionKind::ForwardLabelIndex).ok_or(
            HydrationError::MissingRegionDefinition(RegionKind::ForwardLabelIndex),
        )?,
        layout.region(RegionKind::ForwardSegmentLog).ok_or(
            HydrationError::MissingRegionDefinition(RegionKind::ForwardSegmentLog),
        )?,
    )))
}

/// Reconstructs a typed reverse surface from region-manager metadata.
pub fn reverse_surface_from_layout(
    layout: &RegionManagerLayout,
) -> Result<ReverseSurface, HydrationError> {
    Ok(ReverseSurface::new(super::edge::SurfaceRegions::new(
        layout.region(RegionKind::ReverseVertexTable).ok_or(
            HydrationError::MissingRegionDefinition(RegionKind::ReverseVertexTable),
        )?,
        layout.region(RegionKind::ReverseEdgeEntries).ok_or(
            HydrationError::MissingRegionDefinition(RegionKind::ReverseEdgeEntries),
        )?,
        layout.region(RegionKind::ReverseLabelIndex).ok_or(
            HydrationError::MissingRegionDefinition(RegionKind::ReverseLabelIndex),
        )?,
        layout.region(RegionKind::ReverseSegmentLog).ok_or(
            HydrationError::MissingRegionDefinition(RegionKind::ReverseSegmentLog),
        )?,
    )))
}

/// Hydrates both directional surfaces from a layout and a region-byte source.
pub fn hydrate_surface_runtimes_from_layout(
    layout: &RegionManagerLayout,
    source: &impl RegionByteSource,
) -> Result<HydratedSurfaceRuntimes, HydrationError> {
    let forward_surface = forward_surface_from_layout(layout)?;
    let reverse_surface = reverse_surface_from_layout(layout)?;
    Ok(HydratedSurfaceRuntimes::new(
        hydrate_forward_surface_runtime(forward_surface, source)?,
        hydrate_reverse_surface_runtime(reverse_surface, source)?,
    ))
}

/// Hydrates both directional surfaces directly from stable memory.
pub fn hydrate_surface_runtimes_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
) -> Result<HydratedSurfaceRuntimes, HydrationError> {
    let source = StableMemoryRegionByteSource::from_region_manager(manager, memory)?;
    hydrate_surface_runtimes_from_layout(&manager.layout, &source)
}

/// Writes both directional surface runtimes back to stable memory.
pub fn write_surface_runtimes_to_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    runtimes: &HydratedSurfaceRuntimes,
) -> Result<(), WritebackError> {
    write_forward_surface_runtime_to_stable_memory(manager, memory, &runtimes.forward)?;
    write_reverse_surface_runtime_to_stable_memory(manager, memory, &runtimes.reverse)?;
    Ok(())
}

/// Writes only the dirty regions of both directional surface runtimes.
pub fn write_dirty_surface_runtimes_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    runtimes: &mut HydratedSurfaceRuntimes,
) -> Result<(), WritebackError> {
    write_dirty_forward_surface_runtime_to_stable_memory(manager, memory, &mut runtimes.forward)?;
    write_dirty_reverse_surface_runtime_to_stable_memory(manager, memory, &mut runtimes.reverse)?;
    Ok(())
}

/// Writes the entire forward surface runtime back to stable memory.
pub fn write_forward_surface_runtime_to_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    runtime: &ForwardSurfaceRuntime,
) -> Result<(), WritebackError> {
    write_surface_runtime_to_stable_memory(manager, memory, &runtime.0)
}

/// Writes only dirty forward-surface regions back to stable memory.
pub fn write_dirty_forward_surface_runtime_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    runtime: &mut ForwardSurfaceRuntime,
) -> Result<(), WritebackError> {
    write_dirty_surface_runtime_to_stable_memory(manager, memory, &mut runtime.0)
}

/// Writes the entire reverse surface runtime back to stable memory.
pub fn write_reverse_surface_runtime_to_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    runtime: &ReverseSurfaceRuntime,
) -> Result<(), WritebackError> {
    write_surface_runtime_to_stable_memory(manager, memory, &runtime.0)
}

/// Writes only dirty reverse-surface regions back to stable memory.
pub fn write_dirty_reverse_surface_runtime_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    runtime: &mut ReverseSurfaceRuntime,
) -> Result<(), WritebackError> {
    write_dirty_surface_runtime_to_stable_memory(manager, memory, &mut runtime.0)
}

/// Writes all regions of one surface runtime back to stable memory.
pub fn write_surface_runtime_to_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    runtime: &SurfaceRuntime,
) -> Result<(), WritebackError> {
    let layout = runtime.layout;
    write_region_bytes_to_stable_memory(
        manager,
        memory,
        layout.vertex_table_region(),
        &encode_vertex_entries(&runtime.vertices),
    )?;
    write_region_bytes_to_stable_memory(
        manager,
        memory,
        layout.edge_entries_region(),
        &encode_edge_entries(&runtime.base_entries),
    )?;
    write_region_bytes_to_stable_memory(
        manager,
        memory,
        layout.label_index_region(),
        &encode_label_index_region(&runtime.label_index_entries, &runtime.label_ranges),
    )?;
    write_region_bytes_to_stable_memory(
        manager,
        memory,
        layout.segment_log_region(),
        &encode_overflow_entries(&runtime.overflow_entries),
    )?;
    Ok(())
}

/// Writes only dirty regions of one surface runtime back to stable memory.
pub fn write_dirty_surface_runtime_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    runtime: &mut SurfaceRuntime,
) -> Result<(), WritebackError> {
    let layout = runtime.layout;
    let dirty = runtime.dirty_regions();

    if dirty.vertex_table {
        let bytes = encode_vertex_entries(&runtime.vertices);
        let kind = layout.vertex_table_region().region_kind();
        let _ = manager.set_region_logical_len(kind, bytes.len() as u64);
        let region = manager
            .layout
            .region(kind)
            .ok_or(WritebackError::MissingRegionDefinition(kind))?;
        write_region_bytes_to_stable_memory(manager, memory, region, &bytes)?;
    }
    if dirty.edge_entries {
        let bytes = encode_edge_entries(&runtime.base_entries);
        let kind = layout.edge_entries_region().region_kind();
        let _ = manager.set_region_logical_len(kind, bytes.len() as u64);
        let region = manager
            .layout
            .region(kind)
            .ok_or(WritebackError::MissingRegionDefinition(kind))?;
        write_region_bytes_to_stable_memory(manager, memory, region, &bytes)?;
    }
    if dirty.label_index {
        let bytes = encode_label_index_region(&runtime.label_index_entries, &runtime.label_ranges);
        let kind = layout.label_index_region().region_kind();
        let _ = manager.set_region_logical_len(kind, bytes.len() as u64);
        let region = manager
            .layout
            .region(kind)
            .ok_or(WritebackError::MissingRegionDefinition(kind))?;
        write_region_bytes_to_stable_memory(manager, memory, region, &bytes)?;
    }
    if dirty.segment_log {
        let bytes = encode_overflow_entries(&runtime.overflow_entries);
        let kind = layout.segment_log_region().region_kind();
        let _ = manager.set_region_logical_len(kind, bytes.len() as u64);
        let region = manager
            .layout
            .region(kind)
            .ok_or(WritebackError::MissingRegionDefinition(kind))?;
        write_region_bytes_to_stable_memory(manager, memory, region, &bytes)?;
    }

    runtime.clear_dirty_regions();
    Ok(())
}

/// Serializes vertex-table entries into the fixed-width low-level format.
pub fn encode_vertex_entries(entries: &[VertexEntry]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(entries.len() * SERIALIZED_VERTEX_ENTRY_LEN);
    for entry in entries {
        bytes.extend_from_slice(&entry.edge_index.raw.to_le_bytes());
        bytes.extend_from_slice(&entry.degree.to_le_bytes());
        bytes.extend_from_slice(&entry.log_offset.to_le_bytes());
    }
    bytes
}

/// Serializes hot base-edge entries into the fixed-width low-level format.
pub fn encode_edge_entries(entries: &[EdgeEntry]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(entries.len() * SERIALIZED_EDGE_ENTRY_LEN);
    for entry in entries {
        bytes.extend_from_slice(&entry.target.as_bytes());
        bytes.extend_from_slice(&entry.meta.raw().to_le_bytes());
    }
    bytes
}

/// Serializes overflow-log entries into the fixed-width low-level format.
pub fn encode_overflow_entries(entries: &[OverflowEntry]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(entries.len() * SERIALIZED_OVERFLOW_ENTRY_LEN);
    for entry in entries {
        bytes.extend_from_slice(&entry.edge_id.to_le_bytes());
        bytes.extend_from_slice(&entry.entry.target.as_bytes());
        bytes.extend_from_slice(&entry.entry.meta.raw().to_le_bytes());
        bytes.extend_from_slice(&entry.next.raw.to_le_bytes());
    }
    bytes
}

/// Serializes the surface-local label sidecar into its fixed-width region format.
pub fn encode_label_index_region(
    index_entries: &[VertexLabelIndexEntry],
    ranges: &[VertexLabelRange],
) -> Vec<u8> {
    if index_entries.is_empty() && ranges.is_empty() {
        return Vec::new();
    }
    let mut bytes = Vec::with_capacity(
        SERIALIZED_LABEL_INDEX_HEADER_LEN
            + index_entries.len() * SERIALIZED_LABEL_INDEX_ENTRY_LEN
            + ranges.len() * SERIALIZED_LABEL_RANGE_LEN,
    );
    bytes.extend_from_slice(&(index_entries.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(ranges.len() as u32).to_le_bytes());
    for entry in index_entries {
        bytes.extend_from_slice(&entry.start.to_le_bytes());
        bytes.extend_from_slice(&entry.len.to_le_bytes());
    }
    for range in ranges {
        bytes.extend_from_slice(&range.label_id.to_le_bytes());
        bytes.extend_from_slice(&0_u16.to_le_bytes());
        bytes.extend_from_slice(&range.start.to_le_bytes());
        bytes.extend_from_slice(&range.len.to_le_bytes());
    }
    bytes
}

fn write_region_bytes_to_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    region: RegionRef,
    bytes: &[u8],
) -> Result<(), WritebackError> {
    let kind = region.region_kind();
    let expected = region.logical_len_bytes;
    let actual = bytes.len() as u64;
    if expected != actual {
        return Err(WritebackError::LengthMismatch {
            kind,
            expected,
            actual,
        });
    }
    let extent = manager
        .region_extent(kind)
        .ok_or(WritebackError::MissingExtentRegion(kind))?;
    if actual > extent.len_bytes {
        return Err(WritebackError::LogicalLengthExceedsExtent {
            kind,
            logical_len_bytes: actual,
            extent_len_bytes: extent.len_bytes,
        });
    }
    ensure_memory_covers(memory, extent.addr.0 + actual)?;
    if !bytes.is_empty() {
        memory.write(extent.addr.0, bytes);
    }
    Ok(())
}

fn ensure_memory_covers(
    memory: &impl Memory,
    last_byte_exclusive: u64,
) -> Result<(), WritebackError> {
    let current_pages = memory.size();
    let current_bytes = current_pages
        .checked_mul(65_536)
        .expect("address space overflow");
    if current_bytes >= last_byte_exclusive {
        return Ok(());
    }

    let missing_bytes = last_byte_exclusive - current_bytes;
    let delta_pages = missing_bytes.div_ceil(65_536);
    if memory.grow(delta_pages) == -1 {
        return Err(WritebackError::MemoryGrowFailed {
            current_pages,
            delta_pages,
        });
    }
    Ok(())
}

/// Hydrates one directional surface runtime from decoded region payloads.
pub fn hydrate_surface_runtime(
    layout: SurfaceLayout,
    source: &impl RegionByteSource,
) -> Result<SurfaceRuntime, HydrationError> {
    let vertex_bytes =
        source
            .region_bytes(layout.vertex_table_region())
            .ok_or(HydrationError::MissingRegion(
                layout.vertex_table_region().region_kind(),
            ))?;
    let base_bytes =
        source
            .region_bytes(layout.edge_entries_region())
            .ok_or(HydrationError::MissingRegion(
                layout.edge_entries_region().region_kind(),
            ))?;
    let overflow_bytes =
        source
            .region_bytes(layout.segment_log_region())
            .ok_or(HydrationError::MissingRegion(
                layout.segment_log_region().region_kind(),
            ))?;
    let label_index_bytes =
        source
            .region_bytes(layout.label_index_region())
            .ok_or(HydrationError::MissingRegion(
                layout.label_index_region().region_kind(),
            ))?;
    let (label_index_entries, label_ranges) =
        decode_label_index_region(layout.label_index_region().region_kind(), label_index_bytes)?;

    Ok(SurfaceRuntime::new(
        layout,
        decode_vertex_entries(layout.vertex_table_region().region_kind(), vertex_bytes)?,
        decode_edge_entries(layout.edge_entries_region().region_kind(), base_bytes)?,
        decode_overflow_entries(layout.segment_log_region().region_kind(), overflow_bytes)?,
        label_index_entries,
        label_ranges,
    ))
}

/// Hydrates a typed forward-surface runtime.
pub fn hydrate_forward_surface_runtime(
    surface: ForwardSurface,
    source: &impl RegionByteSource,
) -> Result<ForwardSurfaceRuntime, HydrationError> {
    Ok(ForwardSurfaceRuntime(hydrate_surface_runtime(
        surface.layout(),
        source,
    )?))
}

/// Hydrates a typed reverse-surface runtime.
pub fn hydrate_reverse_surface_runtime(
    surface: ReverseSurface,
    source: &impl RegionByteSource,
) -> Result<ReverseSurfaceRuntime, HydrationError> {
    Ok(ReverseSurfaceRuntime(hydrate_surface_runtime(
        surface.layout(),
        source,
    )?))
}

/// Decodes vertex-table entries from the fixed-width low-level format.
pub fn decode_vertex_entries(
    kind: RegionKind,
    bytes: &[u8],
) -> Result<Vec<VertexEntry>, HydrationError> {
    if bytes.len() % SERIALIZED_VERTEX_ENTRY_LEN != 0 {
        return Err(HydrationError::InvalidLength {
            kind,
            expected_multiple: SERIALIZED_VERTEX_ENTRY_LEN,
            actual: bytes.len(),
        });
    }

    let mut entries = Vec::with_capacity(bytes.len() / SERIALIZED_VERTEX_ENTRY_LEN);
    for chunk in bytes.chunks_exact(SERIALIZED_VERTEX_ENTRY_LEN) {
        let edge_index = u64::from_le_bytes(chunk[0..8].try_into().expect("fixed slice"));
        let degree = u32::from_le_bytes(chunk[8..12].try_into().expect("fixed slice"));
        let log_offset = i32::from_le_bytes(chunk[12..16].try_into().expect("fixed slice"));
        entries.push(VertexEntry::new(
            EdgeIndex::new(edge_index),
            degree,
            log_offset,
        ));
    }
    Ok(entries)
}

/// Decodes hot base-edge entries from the fixed-width low-level format.
pub fn decode_edge_entries(
    kind: RegionKind,
    bytes: &[u8],
) -> Result<Vec<EdgeEntry>, HydrationError> {
    if bytes.len() % SERIALIZED_EDGE_ENTRY_LEN != 0 {
        return Err(HydrationError::InvalidLength {
            kind,
            expected_multiple: SERIALIZED_EDGE_ENTRY_LEN,
            actual: bytes.len(),
        });
    }

    let mut entries = Vec::with_capacity(bytes.len() / SERIALIZED_EDGE_ENTRY_LEN);
    for chunk in bytes.chunks_exact(SERIALIZED_EDGE_ENTRY_LEN) {
        let target = NodeId::new(chunk[0..6].try_into().expect("fixed slice"));
        let meta = u16::from_le_bytes(chunk[6..8].try_into().expect("fixed slice"));
        entries.push(EdgeEntry::new(target, EdgeMeta::from_raw(meta)));
    }
    Ok(entries)
}

/// Decodes overflow-log entries from the fixed-width low-level format.
pub fn decode_overflow_entries(
    kind: RegionKind,
    bytes: &[u8],
) -> Result<Vec<OverflowEntry>, HydrationError> {
    if bytes.len() % SERIALIZED_OVERFLOW_ENTRY_LEN != 0 {
        return Err(HydrationError::InvalidLength {
            kind,
            expected_multiple: SERIALIZED_OVERFLOW_ENTRY_LEN,
            actual: bytes.len(),
        });
    }

    let mut entries = Vec::with_capacity(bytes.len() / SERIALIZED_OVERFLOW_ENTRY_LEN);
    for chunk in bytes.chunks_exact(SERIALIZED_OVERFLOW_ENTRY_LEN) {
        let edge_id = u64::from_le_bytes(chunk[0..8].try_into().expect("fixed slice"));
        let target = NodeId::new(chunk[8..14].try_into().expect("fixed slice"));
        let meta = u16::from_le_bytes(chunk[14..16].try_into().expect("fixed slice"));
        let next = i32::from_le_bytes(chunk[16..20].try_into().expect("fixed slice"));
        entries.push(OverflowEntry::new(
            edge_id,
            EdgeEntry::new(target, EdgeMeta::from_raw(meta)),
            LogOffset::new(next),
        ));
    }
    Ok(entries)
}

/// Decodes the surface-local label sidecar from its fixed-width region format.
pub fn decode_label_index_region(
    kind: RegionKind,
    bytes: &[u8],
) -> Result<(Vec<VertexLabelIndexEntry>, Vec<VertexLabelRange>), HydrationError> {
    if bytes.is_empty() {
        return Ok((Vec::new(), Vec::new()));
    }
    if bytes.len() < SERIALIZED_LABEL_INDEX_HEADER_LEN {
        return Err(HydrationError::InvalidLength {
            kind,
            expected_multiple: SERIALIZED_LABEL_INDEX_HEADER_LEN,
            actual: bytes.len(),
        });
    }

    let index_count = u32::from_le_bytes(bytes[0..4].try_into().expect("fixed slice")) as usize;
    let range_count = u32::from_le_bytes(bytes[4..8].try_into().expect("fixed slice")) as usize;
    let expected_len = SERIALIZED_LABEL_INDEX_HEADER_LEN
        + index_count * SERIALIZED_LABEL_INDEX_ENTRY_LEN
        + range_count * SERIALIZED_LABEL_RANGE_LEN;
    if bytes.len() != expected_len {
        return Err(HydrationError::InvalidLength {
            kind,
            expected_multiple: expected_len,
            actual: bytes.len(),
        });
    }

    let mut offset = SERIALIZED_LABEL_INDEX_HEADER_LEN;
    let mut index_entries = Vec::with_capacity(index_count);
    for _ in 0..index_count {
        let start = u32::from_le_bytes(bytes[offset..offset + 4].try_into().expect("fixed slice"));
        let len = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("fixed slice"),
        );
        index_entries.push(VertexLabelIndexEntry { start, len });
        offset += SERIALIZED_LABEL_INDEX_ENTRY_LEN;
    }

    let mut ranges = Vec::with_capacity(range_count);
    for _ in 0..range_count {
        let label_id =
            u16::from_le_bytes(bytes[offset..offset + 2].try_into().expect("fixed slice"));
        let start = u32::from_le_bytes(
            bytes[offset + 4..offset + 8]
                .try_into()
                .expect("fixed slice"),
        );
        let len = u32::from_le_bytes(
            bytes[offset + 8..offset + 12]
                .try_into()
                .expect("fixed slice"),
        );
        ranges.push(VertexLabelRange {
            label_id,
            start,
            len,
        });
        offset += SERIALIZED_LABEL_RANGE_LEN;
    }

    Ok((index_entries, ranges))
}

#[cfg(test)]
mod tests {
    use super::{
        decode_edge_entries, decode_label_index_region, decode_overflow_entries,
        decode_vertex_entries, encode_edge_entries, encode_label_index_region,
        encode_overflow_entries, encode_vertex_entries, forward_surface_from_layout,
        hydrate_forward_surface_runtime, hydrate_reverse_surface_runtime, hydrate_surface_runtime,
        hydrate_surface_runtimes_from_layout, hydrate_surface_runtimes_from_stable_memory,
        reverse_surface_from_layout, write_dirty_surface_runtime_to_stable_memory,
        write_forward_surface_runtime_to_stable_memory, write_surface_runtime_to_stable_memory,
        write_surface_runtimes_to_stable_memory, HydrationError, InMemoryRegionByteSource,
        RegionByteSource, StableMemoryRegionByteSource, WritebackError,
    };
    use crate::low_level::{
        BucketSizeInPages, EdgeEntry, EdgeIndex, EdgeMeta, ExtentChain, ExtentId, ForwardSurface,
        LogOffset, OverflowEntry, RegionKind, RegionManager, RegionManagerLayout, RegionRef,
        RegionStorageKind, ReverseSurface, SurfaceRegions, VertexEntry, VertexLabelIndexEntry,
        VertexLabelRange, WasmPages, EMPTY_LOG_OFFSET,
    };
    use crate::stable::{Memory, VecMemory};
    use gleaph_graph_kernel::NodeId;
    use std::cell::RefCell;

    fn forward_surface() -> ForwardSurface {
        ForwardSurface::new(SurfaceRegions::new(
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardVertexTable,
                1,
                128,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardEdgeEntries,
                2,
                4096,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardLabelIndex,
                3,
                256,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ForwardSegmentLog,
                4,
                1024,
            ),
        ))
    }

    fn reverse_surface() -> ReverseSurface {
        ReverseSurface::new(SurfaceRegions::new(
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseVertexTable,
                11,
                128,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseEdgeEntries,
                12,
                4096,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseLabelIndex,
                13,
                256,
            ),
            RegionRef::new(
                RegionStorageKind::Extent,
                RegionKind::ReverseSegmentLog,
                14,
                1024,
            ),
        ))
    }

    fn region_manager_layout() -> RegionManagerLayout {
        let mut layout = RegionManagerLayout::with_bucket_size(BucketSizeInPages::DEFAULT);
        for region in [
            forward_surface().layout().vertex_table_region(),
            forward_surface().layout().edge_entries_region(),
            forward_surface().layout().label_index_region(),
            forward_surface().layout().segment_log_region(),
            reverse_surface().layout().vertex_table_region(),
            reverse_surface().layout().edge_entries_region(),
            reverse_surface().layout().label_index_region(),
            reverse_surface().layout().segment_log_region(),
        ] {
            layout.define_region(region);
        }
        layout
    }

    #[derive(Debug, Default)]
    struct TestStableMemory {
        bytes: RefCell<Vec<u8>>,
    }

    impl TestStableMemory {
        fn write(&self, offset: u64, data: &[u8]) {
            let start = usize::try_from(offset).expect("offset should fit usize");
            let end = start + data.len();
            let mut bytes = self.bytes.borrow_mut();
            if end > bytes.len() {
                bytes.resize(end, 0);
            }
            bytes[start..end].copy_from_slice(data);
        }
    }

    impl Memory for TestStableMemory {
        fn size(&self) -> u64 {
            let len = self.bytes.borrow().len() as u64;
            len.div_ceil(65_536)
        }

        fn grow(&self, pages: u64) -> i64 {
            let old = self.size();
            let new_len = (old + pages) * 65_536;
            self.bytes.borrow_mut().resize(new_len as usize, 0);
            old as i64
        }

        fn read(&self, offset: u64, buf: &mut [u8]) {
            let start = usize::try_from(offset).expect("offset should fit usize");
            let end = start + buf.len();
            buf.copy_from_slice(&self.bytes.borrow()[start..end]);
        }

        fn write(&self, offset: u64, src: &[u8]) {
            let start = usize::try_from(offset).expect("offset should fit usize");
            let end = start + src.len();
            let mut bytes = self.bytes.borrow_mut();
            if end > bytes.len() {
                bytes.resize(end, 0);
            }
            bytes[start..end].copy_from_slice(src);
        }
    }

    #[test]
    fn decode_vertex_entries_reads_fixed_width_format() {
        let bytes = encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(7), 3, -1)]);
        let decoded = decode_vertex_entries(RegionKind::ForwardVertexTable, &bytes)
            .expect("vertex entries should decode");
        assert_eq!(decoded, vec![VertexEntry::new(EdgeIndex::new(7), 3, -1)]);
    }

    #[test]
    fn decode_edge_entries_reads_fixed_width_format() {
        let bytes =
            encode_edge_entries(&[EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(4, false))]);
        let decoded = decode_edge_entries(RegionKind::ForwardEdgeEntries, &bytes)
            .expect("edge entries should decode");
        assert_eq!(decoded[0].meta.label_id(), 4);
        assert_eq!(u64::from(decoded[0].target), 9);
    }

    #[test]
    fn decode_overflow_entries_reads_fixed_width_format() {
        let bytes = encode_overflow_entries(&[OverflowEntry::new(
            42,
            EdgeEntry::new(NodeId::from(11u8), EdgeMeta::new(5, false)),
            LogOffset::new(-1),
        )]);
        let decoded = decode_overflow_entries(RegionKind::ForwardSegmentLog, &bytes)
            .expect("overflow entries should decode");
        assert_eq!(decoded[0].edge_id, 42);
        assert_eq!(u64::from(decoded[0].entry.target), 11);
    }

    #[test]
    fn label_index_region_round_trips_fixed_width_format() {
        let bytes = encode_label_index_region(
            &[VertexLabelIndexEntry { start: 0, len: 2 }],
            &[
                VertexLabelRange {
                    label_id: 3,
                    start: 0,
                    len: 1,
                },
                VertexLabelRange {
                    label_id: 4,
                    start: 1,
                    len: 1,
                },
            ],
        );
        let (index_entries, ranges) =
            decode_label_index_region(RegionKind::ForwardLabelIndex, &bytes)
                .expect("label index region should decode");

        assert_eq!(
            index_entries,
            vec![VertexLabelIndexEntry { start: 0, len: 2 }]
        );
        assert_eq!(ranges[0].label_id, 3);
        assert_eq!(ranges[1].label_id, 4);
    }

    #[test]
    fn hydration_rejects_invalid_region_length() {
        let err = decode_edge_entries(RegionKind::ForwardEdgeEntries, &[1, 2, 3])
            .expect_err("invalid bytes should fail");
        assert_eq!(
            err,
            HydrationError::InvalidLength {
                kind: RegionKind::ForwardEdgeEntries,
                expected_multiple: 8,
                actual: 3,
            }
        );
    }

    #[test]
    fn hydrate_surface_runtime_builds_runtime_from_region_source() {
        let surface = forward_surface();
        let mut source = InMemoryRegionByteSource::new();
        source.insert(
            RegionKind::ForwardVertexTable,
            encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(1), 2, 0)]),
        );
        source.insert(
            RegionKind::ForwardEdgeEntries,
            encode_edge_entries(&[
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(3u8), EdgeMeta::new(1, false)),
            ]),
        );
        source.insert(RegionKind::ForwardLabelIndex, Vec::new());
        source.insert(
            RegionKind::ForwardSegmentLog,
            encode_overflow_entries(&[OverflowEntry::new(
                41,
                EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(3, false)),
                LogOffset::new(-1),
            )]),
        );

        let runtime = hydrate_surface_runtime(surface.layout(), &source)
            .expect("surface runtime should hydrate");
        assert_eq!(runtime.vertices.len(), 1);
        assert_eq!(runtime.base_entries.len(), 3);
        assert_eq!(runtime.overflow_entries.len(), 1);
    }

    #[test]
    fn hydrate_forward_surface_runtime_wraps_surface_runtime() {
        let surface = forward_surface();
        let mut source = InMemoryRegionByteSource::new();
        source.insert(
            RegionKind::ForwardVertexTable,
            encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
        );
        source.insert(
            RegionKind::ForwardEdgeEntries,
            encode_edge_entries(&[EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(1, false))]),
        );
        source.insert(RegionKind::ForwardLabelIndex, Vec::new());
        source.insert(RegionKind::ForwardSegmentLog, Vec::new());

        let runtime = hydrate_forward_surface_runtime(surface, &source)
            .expect("forward surface should hydrate");
        assert_eq!(runtime.0.layout.kind as u8, surface.layout().kind as u8);
        assert_eq!(runtime.0.vertices.len(), 1);
    }

    #[test]
    fn hydrate_reverse_surface_runtime_wraps_surface_runtime() {
        let surface = reverse_surface();
        let mut source = InMemoryRegionByteSource::new();
        source.insert(
            RegionKind::ReverseVertexTable,
            encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
        );
        source.insert(
            RegionKind::ReverseEdgeEntries,
            encode_edge_entries(&[EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(2, false))]),
        );
        source.insert(RegionKind::ReverseLabelIndex, Vec::new());
        source.insert(RegionKind::ReverseSegmentLog, Vec::new());

        let runtime = hydrate_reverse_surface_runtime(surface, &source)
            .expect("reverse surface should hydrate");
        assert_eq!(runtime.0.layout.kind as u8, surface.layout().kind as u8);
        assert_eq!(runtime.0.base_entries.len(), 1);
    }

    #[test]
    fn forward_surface_from_layout_reconstructs_forward_regions() {
        let layout = region_manager_layout();
        let surface = forward_surface_from_layout(&layout).expect("forward surface should exist");

        assert_eq!(
            surface.layout().edge_entries_region().region_kind(),
            RegionKind::ForwardEdgeEntries
        );
        assert!(surface.layout().validate());
    }

    #[test]
    fn reverse_surface_from_layout_reconstructs_reverse_regions() {
        let layout = region_manager_layout();
        let surface = reverse_surface_from_layout(&layout).expect("reverse surface should exist");

        assert_eq!(
            surface.layout().segment_log_region().region_kind(),
            RegionKind::ReverseSegmentLog
        );
        assert!(surface.layout().validate());
    }

    #[test]
    fn hydrate_surface_runtimes_from_layout_hydrates_both_surfaces() {
        let layout = region_manager_layout();
        let mut source = InMemoryRegionByteSource::new();
        source.insert(
            RegionKind::ForwardVertexTable,
            encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
        );
        source.insert(
            RegionKind::ForwardEdgeEntries,
            encode_edge_entries(&[EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(1, false))]),
        );
        source.insert(RegionKind::ForwardLabelIndex, Vec::new());
        source.insert(RegionKind::ForwardSegmentLog, Vec::new());
        source.insert(
            RegionKind::ReverseVertexTable,
            encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
        );
        source.insert(
            RegionKind::ReverseEdgeEntries,
            encode_edge_entries(&[EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(2, false))]),
        );
        source.insert(RegionKind::ReverseLabelIndex, Vec::new());
        source.insert(RegionKind::ReverseSegmentLog, Vec::new());

        let runtimes = hydrate_surface_runtimes_from_layout(&layout, &source)
            .expect("both surfaces should hydrate");

        assert_eq!(runtimes.forward.0.base_entries.len(), 1);
        assert_eq!(runtimes.reverse.0.base_entries.len(), 1);
        assert_eq!(u64::from(runtimes.forward.0.base_entries[0].target), 1);
        assert_eq!(u64::from(runtimes.reverse.0.base_entries[0].target), 2);
    }

    #[test]
    fn stable_memory_region_byte_source_reads_extent_backed_region_payloads() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        let region = manager.define_extent_region(
            RegionKind::ForwardVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                16,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        let extent = manager
            .region_extent(RegionKind::ForwardVertexTable)
            .expect("extent metadata should exist");
        let expected = encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(7), 3, -1)]);
        let memory = TestStableMemory::default();
        memory.write(extent.addr.0, &expected);

        let source = StableMemoryRegionByteSource::from_region_manager(&manager, &memory)
            .expect("extent-backed source should materialize");

        assert_eq!(source.region_bytes(region), Some(expected.as_slice()));
    }

    #[test]
    fn hydrate_surface_runtimes_from_stable_memory_reads_both_surfaces() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                16,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                8,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardLabelIndex,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardSegmentLog,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                16,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                8,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseLabelIndex,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseSegmentLog,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );

        let memory = TestStableMemory::default();
        for (kind, bytes) in [
            (
                RegionKind::ForwardVertexTable,
                encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
            ),
            (
                RegionKind::ForwardEdgeEntries,
                encode_edge_entries(&[EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(1, false))]),
            ),
            (RegionKind::ForwardLabelIndex, Vec::new()),
            (RegionKind::ForwardSegmentLog, Vec::new()),
            (
                RegionKind::ReverseVertexTable,
                encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
            ),
            (
                RegionKind::ReverseEdgeEntries,
                encode_edge_entries(&[EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(2, false))]),
            ),
            (RegionKind::ReverseLabelIndex, Vec::new()),
            (RegionKind::ReverseSegmentLog, Vec::new()),
        ] {
            let extent = manager.region_extent(kind).expect("extent should exist");
            memory.write(extent.addr.0, &bytes);
        }

        let runtimes = hydrate_surface_runtimes_from_stable_memory(&manager, &memory)
            .expect("stable-memory hydration should succeed");

        assert_eq!(u64::from(runtimes.forward.0.base_entries[0].target), 1);
        assert_eq!(u64::from(runtimes.reverse.0.base_entries[0].target), 2);
    }

    #[test]
    fn hydrate_surface_runtimes_from_vector_memory_reads_both_surfaces() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                16,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                8,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardLabelIndex,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardSegmentLog,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                16,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                8,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseLabelIndex,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ReverseSegmentLog,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );

        let memory = VecMemory::default();
        assert_eq!(memory.grow(8), 0);
        assert_eq!(memory.size(), 8);

        for (kind, bytes) in [
            (
                RegionKind::ForwardVertexTable,
                encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
            ),
            (
                RegionKind::ForwardEdgeEntries,
                encode_edge_entries(&[EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(1, false))]),
            ),
            (RegionKind::ForwardLabelIndex, Vec::new()),
            (RegionKind::ForwardSegmentLog, Vec::new()),
            (
                RegionKind::ReverseVertexTable,
                encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
            ),
            (
                RegionKind::ReverseEdgeEntries,
                encode_edge_entries(&[EdgeEntry::new(NodeId::from(20u8), EdgeMeta::new(2, false))]),
            ),
            (RegionKind::ReverseLabelIndex, Vec::new()),
            (RegionKind::ReverseSegmentLog, Vec::new()),
        ] {
            let extent = manager.region_extent(kind).expect("extent should exist");
            memory.write(extent.addr.0, &bytes);
        }

        let runtimes = hydrate_surface_runtimes_from_stable_memory(&manager, &memory)
            .expect("vector memory hydration should succeed");

        assert_eq!(u64::from(runtimes.forward.0.base_entries[0].target), 10);
        assert_eq!(u64::from(runtimes.reverse.0.base_entries[0].target), 20);
    }

    #[test]
    fn write_surface_runtime_to_stable_memory_round_trips_through_hydration() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                16,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                16,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardLabelIndex,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardSegmentLog,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                20,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );

        let runtime = super::SurfaceRuntime::new(
            forward_surface_from_layout(&manager.layout)
                .expect("forward layout should exist")
                .layout(),
            vec![VertexEntry::new(EdgeIndex::new(0), 2, 0)],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(1, false)),
            ],
            vec![OverflowEntry::new(
                99,
                EdgeEntry::new(NodeId::from(9u8), EdgeMeta::new(3, false)),
                LogOffset::EMPTY,
            )],
            Vec::new(),
            Vec::new(),
        );
        let memory = VecMemory::default();

        write_surface_runtime_to_stable_memory(&manager, &memory, &runtime)
            .expect("writeback should succeed");
        let source = StableMemoryRegionByteSource::from_region_manager(&manager, &memory)
            .expect("byte source should materialize");
        let hydrated = hydrate_surface_runtime(
            forward_surface_from_layout(&manager.layout)
                .expect("forward layout should exist")
                .layout(),
            &source,
        )
        .expect("hydration should succeed");

        assert_eq!(hydrated.vertices, runtime.vertices);
        assert_eq!(hydrated.base_entries, runtime.base_entries);
        assert_eq!(hydrated.overflow_entries, runtime.overflow_entries);
    }

    #[test]
    fn write_dirty_surface_runtime_to_stable_memory_only_flushes_mutated_regions() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                16,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                16,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardLabelIndex,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                40,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardSegmentLog,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );

        let mut runtime = super::SurfaceRuntime::new(
            forward_surface_from_layout(&manager.layout)
                .expect("forward layout should exist")
                .layout(),
            vec![VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET)],
            vec![
                EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(4, false)),
            ],
            Vec::new(),
            vec![VertexLabelIndexEntry::new(0, 2)],
            vec![
                VertexLabelRange::new(3, 0, 1),
                VertexLabelRange::new(4, 1, 1),
            ],
        );
        let memory = VecMemory::default();

        write_surface_runtime_to_stable_memory(&manager, &memory, &runtime)
            .expect("initial writeback should succeed");

        runtime
            .replace_base_entry(
                0,
                1,
                EdgeEntry::new(NodeId::from(2u8), EdgeMeta::new(3, false)),
            )
            .expect("base update");
        runtime
            .refresh_label_sidecar_for_dirty_vertices()
            .expect("refresh sidecar");
        assert!(runtime.has_dirty_regions());

        write_dirty_surface_runtime_to_stable_memory(&mut manager, &memory, &mut runtime)
            .expect("dirty writeback should succeed");
        assert!(!runtime.has_dirty_regions());

        let source = StableMemoryRegionByteSource::from_region_manager(&manager, &memory)
            .expect("byte source should materialize");
        let hydrated = hydrate_surface_runtime(
            forward_surface_from_layout(&manager.layout)
                .expect("forward layout should exist")
                .layout(),
            &source,
        )
        .expect("hydration should succeed");

        assert_eq!(hydrated.base_entries[1].meta.label_id(), 3);
        assert_eq!(
            hydrated.label_index_entries,
            vec![VertexLabelIndexEntry::new(0, 1)]
        );
        assert_eq!(hydrated.label_ranges, vec![VertexLabelRange::new(3, 0, 2)]);
    }

    #[test]
    fn write_surface_runtimes_to_stable_memory_writes_both_surfaces() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 16_u64),
            (RegionKind::ForwardEdgeEntries, 8_u64),
            (RegionKind::ForwardLabelIndex, 0_u64),
            (RegionKind::ForwardSegmentLog, 0_u64),
            (RegionKind::ReverseVertexTable, 16_u64),
            (RegionKind::ReverseEdgeEntries, 8_u64),
            (RegionKind::ReverseLabelIndex, 0_u64),
            (RegionKind::ReverseSegmentLog, 0_u64),
        ] {
            manager.define_extent_region(
                kind,
                ExtentChain::new(
                    ExtentId::NULL,
                    ExtentId::NULL,
                    logical_len,
                    WasmPages::new(1),
                    WasmPages::new(if logical_len == 0 { 1 } else { 0 }),
                ),
            );
        }

        let runtimes = super::HydratedSurfaceRuntimes::new(
            super::ForwardSurfaceRuntime::new(
                forward_surface_from_layout(&manager.layout).expect("forward layout should exist"),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, -1)],
                vec![EdgeEntry::new(NodeId::from(10u8), EdgeMeta::new(1, false))],
                Vec::new(),
            ),
            super::ReverseSurfaceRuntime::new(
                reverse_surface_from_layout(&manager.layout).expect("reverse layout should exist"),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, -1)],
                vec![EdgeEntry::new(NodeId::from(20u8), EdgeMeta::new(2, false))],
                Vec::new(),
            ),
        );
        let memory = VecMemory::default();

        write_surface_runtimes_to_stable_memory(&manager, &memory, &runtimes)
            .expect("pair writeback should succeed");
        let hydrated = hydrate_surface_runtimes_from_stable_memory(&manager, &memory)
            .expect("hydration should succeed");

        assert_eq!(u64::from(hydrated.forward.0.base_entries[0].target), 10);
        assert_eq!(u64::from(hydrated.reverse.0.base_entries[0].target), 20);
    }

    #[test]
    fn writeback_rejects_region_length_mismatch() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                16,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                8,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardLabelIndex,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardSegmentLog,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                0,
                WasmPages::new(1),
                WasmPages::new(1),
            ),
        );

        let runtime = super::ForwardSurfaceRuntime::new(
            forward_surface_from_layout(&manager.layout).expect("forward layout should exist"),
            vec![
                VertexEntry::new(EdgeIndex::new(0), 2, -1),
                VertexEntry::new(EdgeIndex::new(2), 0, -1),
            ],
            vec![EdgeEntry::new(NodeId::from(1u8), EdgeMeta::new(1, false))],
            Vec::new(),
        );
        let memory = VecMemory::default();

        let err = write_forward_surface_runtime_to_stable_memory(&manager, &memory, &runtime)
            .expect_err("length mismatch should fail");
        assert_eq!(
            err,
            WritebackError::LengthMismatch {
                kind: RegionKind::ForwardVertexTable,
                expected: 16,
                actual: 32,
            }
        );
    }

    #[test]
    fn forward_surface_from_layout_requires_forward_regions() {
        let layout = RegionManagerLayout::with_bucket_size(BucketSizeInPages::DEFAULT);
        let err = forward_surface_from_layout(&layout).expect_err("missing layout should fail");

        assert_eq!(
            err,
            HydrationError::MissingRegionDefinition(RegionKind::ForwardVertexTable)
        );
    }
}
