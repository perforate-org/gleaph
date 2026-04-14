//! Fixed-width byte-format adapters between low-level runtime state and stable memory.

use std::cell::{Cell, RefCell};
use std::collections::BTreeMap;
use std::error::Error;
use std::fmt;
use std::mem::size_of;

use ic_stable_structures::Memory;

use super::edge::{EdgeEntry, EdgeMeta};
use super::graph::{GraphInsertPolicy, SurfaceVertexWindowReserveHint};
use super::ids::{EdgeRef, VertexRef};
use super::manager::RegionManager;
use super::overflow::{LogOffset, OverflowEntry};
use super::region::{RegionKind, RegionManagerLayout, RegionRef, RegionStorageKind};
use super::runtime::{
    ForwardSurfaceRuntime, ReverseSurfaceRuntime, SurfaceBaseStorage, SurfaceRuntime,
    SurfaceVertexWindowSummary, summarize_vertex_window_entries,
};
use super::surface::{ForwardSurface, ReverseSurface, SurfaceLayout};
use super::vertex::{EdgeIndex, VertexEntry, VertexLabelIndexEntry, VertexLabelRange};

const SERIALIZED_VERTEX_ENTRY_LEN: usize = 16;
const SERIALIZED_EDGE_ENTRY_LEN: usize = 8;
const SERIALIZED_OVERFLOW_ENTRY_LEN: usize = 20;
const SERIALIZED_LABEL_INDEX_HEADER_LEN: usize = 8;
const SERIALIZED_LABEL_INDEX_ENTRY_LEN: usize = 8;
const SERIALIZED_LABEL_RANGE_LEN: usize = 12;

thread_local! {
    /// Reused across dirty surface flushes on this thread to avoid reallocating encode buffers
    /// (hot for repeated `append_empty_vertex` + writeback workloads).
    static SURFACE_DIRTY_FLUSH_SCRATCH: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

/// Reads raw bytes for a logical region.
///
/// Stable-memory-facing hydration seam. The source may
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

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct VertexBucketCacheEntry {
    virtual_bucket_start: usize,
    virtual_bucket_len: usize,
    real_bucket_addr: u64,
}

/// Stable-memory reader for one vertex table with a tiny last-bucket cache.
///
/// This is useful when callers need repeated logical-ordinal reads without
/// hydrating the entire vertex table into memory.
pub struct StableVertexTableReader<'a, M: Memory> {
    manager: &'a RegionManager,
    memory: &'a M,
    kind: RegionKind,
    cache: Cell<Option<VertexBucketCacheEntry>>,
}

impl<'a, M: Memory> StableVertexTableReader<'a, M> {
    /// Creates a direct stable-memory reader for one vertex-table region.
    pub fn new(manager: &'a RegionManager, memory: &'a M, kind: RegionKind) -> Self {
        Self {
            manager,
            memory,
            kind,
            cache: Cell::new(None),
        }
    }

    /// Reads one logical vertex entry by ordinal, reusing the last bucket mapping when possible.
    pub fn read_vertex_entry(&self, ordinal: usize) -> Result<Option<VertexEntry>, HydrationError> {
        let region = self
            .manager
            .layout
            .region(self.kind)
            .ok_or(HydrationError::MissingRegionDefinition(self.kind))?;
        let byte_offset = ordinal.checked_mul(SERIALIZED_VERTEX_ENTRY_LEN).ok_or(
            HydrationError::RegionTooLarge(self.kind, region.logical_len_bytes),
        )?;
        let logical_len = usize::try_from(region.logical_len_bytes)
            .map_err(|_| HydrationError::RegionTooLarge(self.kind, region.logical_len_bytes))?;
        let end = byte_offset + SERIALIZED_VERTEX_ENTRY_LEN;
        if end > logical_len {
            return Ok(None);
        }

        let mut bytes = [0u8; SERIALIZED_VERTEX_ENTRY_LEN];
        match region.storage_kind() {
            RegionStorageKind::Extent => {
                let extent = self
                    .manager
                    .region_extent(self.kind)
                    .ok_or(HydrationError::MissingExtentRegion(self.kind))?;
                let start = extent.addr.0.checked_add(byte_offset as u64).ok_or(
                    HydrationError::RegionTooLarge(self.kind, region.logical_len_bytes),
                )?;
                self.memory.read(start, &mut bytes);
            }
            RegionStorageKind::BucketChain => {
                let bucket_size =
                    usize::try_from(self.manager.bucket_size_bytes()).map_err(|_| {
                        HydrationError::RegionTooLarge(self.kind, region.logical_len_bytes)
                    })?;
                let virtual_bucket_start = (byte_offset / bucket_size) * bucket_size;
                let in_bucket_offset = byte_offset - virtual_bucket_start;

                let cached = self.cache.get().filter(|entry| {
                    entry.virtual_bucket_start == virtual_bucket_start
                        && in_bucket_offset + SERIALIZED_VERTEX_ENTRY_LEN
                            <= entry.virtual_bucket_len
                });

                if let Some(entry) = cached {
                    self.memory
                        .read(entry.real_bucket_addr + in_bucket_offset as u64, &mut bytes);
                } else {
                    let bucket_idx = byte_offset / bucket_size;
                    let chain = self
                        .manager
                        .bucket_chain(self.kind)
                        .ok_or(HydrationError::MissingBucketChain(self.kind))?;
                    let mut cursor = chain.head;
                    for _ in 0..bucket_idx {
                        let header = self
                            .manager
                            .bucket_header(cursor)
                            .ok_or(HydrationError::MissingBucketChain(self.kind))?;
                        cursor = header.next;
                    }
                    let header = self
                        .manager
                        .bucket_header(cursor)
                        .ok_or(HydrationError::MissingBucketChain(self.kind))?;
                    self.cache.set(Some(VertexBucketCacheEntry {
                        virtual_bucket_start,
                        virtual_bucket_len: bucket_size,
                        real_bucket_addr: header.addr.0,
                    }));
                    self.memory
                        .read(header.addr.0 + in_bucket_offset as u64, &mut bytes);
                }
            }
        }

        let mut decoded = decode_vertex_entries(self.kind, &bytes)?;
        Ok(decoded.pop())
    }

    /// Reads up to `count` logical vertex entries starting at `start_ordinal`.
    ///
    /// Stops early when the requested range reaches the logical end of the
    /// vertex table.
    pub fn read_vertex_entries(
        &self,
        start_ordinal: usize,
        count: usize,
    ) -> Result<Vec<VertexEntry>, HydrationError> {
        let mut entries = Vec::with_capacity(count);
        for ordinal in start_ordinal..start_ordinal.saturating_add(count) {
            let Some(entry) = self.read_vertex_entry(ordinal)? else {
                break;
            };
            entries.push(entry);
        }
        Ok(entries)
    }
}

impl StableMemoryRegionByteSource {
    /// Materializes region payloads from stable memory using manager metadata.
    pub fn from_region_manager(
        manager: &RegionManager,
        memory: &impl Memory,
    ) -> Result<Self, HydrationError> {
        let mut regions = BTreeMap::new();
        for entry in manager.layout.directory.iter() {
            let region = entry.region;
            let kind = region.region_kind();
            let bytes = read_region_bytes_from_stable_memory(manager, memory, region)?;
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
    MissingBucketChain(RegionKind),
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
    RegionTooLarge(RegionKind, u64),
    TruncatedBucketChain {
        kind: RegionKind,
        logical_len_bytes: u64,
        written: u64,
    },
    MemoryGrowFailed {
        current_pages: u64,
        delta_pages: u64,
    },
    /// Failed to write the tail [`super::pma_stable_root`] footer.
    PmaStableRoot(String),
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
            WritebackError::MissingBucketChain(kind) => {
                write!(f, "missing bucket-chain metadata for region {:?}", kind)
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
            WritebackError::RegionTooLarge(kind, logical_len_bytes) => write!(
                f,
                "region {:?} is too large to materialize on this platform: {} bytes",
                kind, logical_len_bytes
            ),
            WritebackError::TruncatedBucketChain {
                kind,
                logical_len_bytes,
                written,
            } => write!(
                f,
                "region {:?} bucket chain ended early while writing: expected {} bytes, wrote {} bytes",
                kind, logical_len_bytes, written
            ),
            WritebackError::MemoryGrowFailed {
                current_pages,
                delta_pages,
            } => write!(
                f,
                "failed to grow memory from {} pages by {} pages",
                current_pages, delta_pages
            ),
            WritebackError::PmaStableRoot(msg) => write!(f, "PMA stable root write: {msg}"),
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
    MissingBucketChain(RegionKind),
    RegionTooLarge(RegionKind, u64),
    LogicalLengthExceedsExtent {
        kind: RegionKind,
        logical_len_bytes: u64,
        extent_len_bytes: u64,
    },
    TruncatedBucketChain {
        kind: RegionKind,
        logical_len_bytes: u64,
        read: u64,
    },
    InvalidLength {
        kind: RegionKind,
        expected_multiple: usize,
        actual: usize,
    },
    UnsupportedFormatVersion {
        kind: RegionKind,
        expected: u32,
        actual: u32,
    },
    ChecksumMismatch {
        kind: RegionKind,
        expected: u64,
        actual: u64,
    },
    /// Serialized bytes are present but do not start with the expected MGQ1 header (or are too short).
    InvalidMaintenanceQueueHeader(RegionKind),
    /// [`ShardCanisterDirectory`](crate::low_level::ShardCanisterDirectory) bytes failed to decode.
    InvalidShardCanisterDirectory {
        kind: RegionKind,
        reason: &'static str,
    },
    /// A live edge marks [`EdgeMeta::is_shard_canister`] with a slot at or beyond the directory length.
    ShardCanisterSlotOutOfRange {
        slot: u16,
        directory_len: usize,
    },
    /// Tail [`super::pma_stable_root`] footer could not be decoded or conflicts with graph data.
    PmaStableRoot(String),
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
            HydrationError::MissingBucketChain(kind) => {
                write!(f, "missing bucket-chain metadata for region {:?}", kind)
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
            HydrationError::TruncatedBucketChain {
                kind,
                logical_len_bytes,
                read,
            } => write!(
                f,
                "region {:?} bucket chain ended early while reading: expected {} bytes, read {} bytes",
                kind, logical_len_bytes, read
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
            HydrationError::UnsupportedFormatVersion {
                kind,
                expected,
                actual,
            } => write!(
                f,
                "region {:?} has unsupported format version {}; expected {}",
                kind, actual, expected
            ),
            HydrationError::ChecksumMismatch {
                kind,
                expected,
                actual,
            } => write!(
                f,
                "region {:?} checksum mismatch: expected {}, actual {}",
                kind, expected, actual
            ),
            HydrationError::InvalidMaintenanceQueueHeader(kind) => write!(
                f,
                "region {:?} maintenance queue bytes are not a valid MGQ1 v1 header payload",
                kind
            ),
            HydrationError::InvalidShardCanisterDirectory { kind, reason } => write!(
                f,
                "region {:?} shard canister directory bytes are invalid: {}",
                kind, reason
            ),
            HydrationError::ShardCanisterSlotOutOfRange {
                slot,
                directory_len,
            } => write!(
                f,
                "shard canister edge slot {} is out of range for directory length {}",
                slot, directory_len
            ),
            HydrationError::PmaStableRoot(msg) => write!(f, "PMA stable root: {msg}"),
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

fn sync_surface_runtime_segment_capacities(
    surface: &mut SurfaceRuntime,
    manager: &RegionManager,
    kind: RegionKind,
) -> Option<()> {
    let segment_zero = manager.edge_segment(kind, 0)?;
    surface.sync_base_segment_slot_capacity_from_manager(
        segment_zero.segment_id,
        segment_zero.slot_capacity,
    );
    for header in manager.edge_segment_directory(kind)?.iter().copied() {
        surface
            .sync_base_segment_slot_capacity_from_manager(header.segment_id, header.slot_capacity);
    }
    Some(())
}

fn sync_hydrated_surface_runtimes_segment_capacities(
    runtimes: &mut HydratedSurfaceRuntimes,
    manager: &RegionManager,
) -> Option<()> {
    sync_surface_runtime_segment_capacities(
        &mut runtimes.forward.0,
        manager,
        RegionKind::ForwardEdgeEntries,
    )?;
    sync_surface_runtime_segment_capacities(
        &mut runtimes.reverse.0,
        manager,
        RegionKind::ReverseEdgeEntries,
    )?;
    Some(())
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
    let mut runtimes = hydrate_surface_runtimes_from_layout(&manager.layout, &source)?;
    runtimes.forward.0.base_entries =
        hydrate_edge_storage_from_stable_memory(manager, memory, RegionKind::ForwardEdgeEntries)?;
    runtimes.reverse.0.base_entries =
        hydrate_edge_storage_from_stable_memory(manager, memory, RegionKind::ReverseEdgeEntries)?;
    let _ = sync_hydrated_surface_runtimes_segment_capacities(&mut runtimes, manager);
    Ok(runtimes)
}

/// Reads one vertex-table entry directly from stable memory by logical ordinal.
///
/// This keeps `ordinal` as the logical contract while hiding whether the
/// underlying vertex table is extent-backed or bucket-chain-backed.
pub fn read_vertex_entry_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    ordinal: usize,
) -> Result<Option<VertexEntry>, HydrationError> {
    StableVertexTableReader::new(manager, memory, kind).read_vertex_entry(ordinal)
}

/// Reads one vertex-table entry directly from stable memory by packed vertex ref.
pub fn read_vertex_entry_by_ref_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    vertex_ref: VertexRef,
) -> Result<Option<VertexEntry>, HydrationError> {
    let ordinal = usize::try_from(u64::from(vertex_ref))
        .map_err(|_| HydrationError::RegionTooLarge(kind, u64::from(vertex_ref)))?;
    read_vertex_entry_from_stable_memory(manager, memory, kind, ordinal)
}

/// Reads up to `count` vertex-table entries directly from stable memory.
pub fn read_vertex_entries_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    start_ordinal: usize,
    count: usize,
) -> Result<Vec<VertexEntry>, HydrationError> {
    StableVertexTableReader::new(manager, memory, kind).read_vertex_entries(start_ordinal, count)
}

/// Reads the reserved base-span length for one logical vertex ordinal directly
/// from stable memory.
pub fn read_vertex_reserved_span_len_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    ordinal: usize,
) -> Result<Option<u64>, HydrationError> {
    let current = match read_vertex_entry_from_stable_memory(manager, memory, kind, ordinal)? {
        Some(entry) => entry,
        None => return Ok(None),
    };
    let next = read_vertex_entry_from_stable_memory(manager, memory, kind, ordinal + 1)?;
    let edge_region_kind = match kind {
        RegionKind::ForwardVertexTable => RegionKind::ForwardEdgeEntries,
        RegionKind::ReverseVertexTable => RegionKind::ReverseEdgeEntries,
        _ => return Ok(None),
    };
    let Some(slot_capacity) = manager.edge_ref_slot_capacity(edge_region_kind, current.edge_ref())
    else {
        return Ok(None);
    };
    Ok(current.reserved_span_len(next, slot_capacity))
}

fn edge_region_kind_for_vertex_table(kind: RegionKind) -> Option<RegionKind> {
    match kind {
        RegionKind::ForwardVertexTable => Some(RegionKind::ForwardEdgeEntries),
        RegionKind::ReverseVertexTable => Some(RegionKind::ReverseEdgeEntries),
        _ => None,
    }
}

/// Reads a contiguous slice of hot edge entries directly from stable memory by edge ref.
pub fn read_edge_entries_by_ref_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    edge_ref: super::ids::EdgeRef,
    count: usize,
) -> Result<Vec<EdgeEntry>, HydrationError> {
    let Some((segment, extent)) = manager.resolve_edge_ref(kind, edge_ref) else {
        return Ok(Vec::new());
    };
    let start_slot = usize::try_from(edge_ref.start_slot())
        .map_err(|_| HydrationError::RegionTooLarge(kind, edge_ref.start_slot()))?;
    let slot_capacity = usize::try_from(segment.slot_capacity)
        .map_err(|_| HydrationError::RegionTooLarge(kind, segment.slot_capacity))?;
    let end_slot = start_slot.saturating_add(count);
    if end_slot > slot_capacity {
        return Ok(Vec::new());
    }

    let start_byte = start_slot
        .checked_mul(SERIALIZED_EDGE_ENTRY_LEN)
        .ok_or(HydrationError::RegionTooLarge(kind, extent.len_bytes))?;
    let byte_len = count
        .checked_mul(SERIALIZED_EDGE_ENTRY_LEN)
        .ok_or(HydrationError::RegionTooLarge(kind, extent.len_bytes))?;
    let end_byte = start_byte
        .checked_add(byte_len)
        .ok_or(HydrationError::RegionTooLarge(kind, extent.len_bytes))?;
    if end_byte > usize::try_from(extent.len_bytes).unwrap_or(usize::MAX) {
        return Err(HydrationError::LogicalLengthExceedsExtent {
            kind,
            logical_len_bytes: end_byte as u64,
            extent_len_bytes: extent.len_bytes,
        });
    }

    let mut bytes = vec![0u8; byte_len];
    if byte_len > 0 {
        memory.read(extent.addr.0 + start_byte as u64, &mut bytes);
    }
    decode_edge_entries(kind, &bytes)
}

/// Reads live base entries for one vertex ordinal directly from stable memory.
pub fn read_vertex_base_entries_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    ordinal: usize,
) -> Result<Option<Vec<EdgeEntry>>, HydrationError> {
    let Some(vertex) = read_vertex_entry_from_stable_memory(manager, memory, kind, ordinal)? else {
        return Ok(None);
    };
    let edge_kind = match edge_region_kind_for_vertex_table(kind) {
        Some(kind) => kind,
        None => return Ok(None),
    };
    let count = usize::try_from(vertex.degree)
        .map_err(|_| HydrationError::RegionTooLarge(edge_kind, u64::from(vertex.degree)))?;
    Ok(Some(read_edge_entries_by_ref_from_stable_memory(
        manager,
        memory,
        edge_kind,
        vertex.edge_ref(),
        count,
    )?))
}

/// Resolves the packed [`EdgeRef`] for one live base entry of a vertex ordinal.
pub fn read_vertex_base_edge_ref_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    ordinal: usize,
    logical_index: usize,
) -> Result<Option<super::ids::EdgeRef>, HydrationError> {
    let Some(vertex) = read_vertex_entry_from_stable_memory(manager, memory, kind, ordinal)? else {
        return Ok(None);
    };
    let degree = usize::try_from(vertex.degree)
        .map_err(|_| HydrationError::RegionTooLarge(kind, u64::from(vertex.degree)))?;
    if logical_index >= degree {
        return Ok(None);
    }
    let start_slot = match vertex
        .edge_ref()
        .start_slot()
        .checked_add(logical_index as u64)
    {
        Some(start_slot) => start_slot,
        None => return Ok(None),
    };
    Ok(Some(vertex.edge_ref().with_start_slot(start_slot)))
}

/// Reads one live base entry for a vertex ordinal and logical base index directly from stable memory.
pub fn read_vertex_base_entry_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    ordinal: usize,
    logical_index: usize,
) -> Result<Option<EdgeEntry>, HydrationError> {
    let Some(edge_ref) = read_vertex_base_edge_ref_from_stable_memory(
        manager,
        memory,
        kind,
        ordinal,
        logical_index,
    )?
    else {
        return Ok(None);
    };
    let edge_kind = match edge_region_kind_for_vertex_table(kind) {
        Some(kind) => kind,
        None => return Ok(None),
    };
    let mut entries =
        read_edge_entries_by_ref_from_stable_memory(manager, memory, edge_kind, edge_ref, 1)?;
    Ok(entries.pop())
}

/// Reads the full reserved base span for one vertex ordinal directly from stable memory.
pub fn read_vertex_reserved_base_entries_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    ordinal: usize,
) -> Result<Option<Vec<EdgeEntry>>, HydrationError> {
    let Some(vertex) = read_vertex_entry_from_stable_memory(manager, memory, kind, ordinal)? else {
        return Ok(None);
    };
    let Some(span_len) =
        read_vertex_reserved_span_len_from_stable_memory(manager, memory, kind, ordinal)?
    else {
        return Ok(None);
    };
    let edge_kind = match edge_region_kind_for_vertex_table(kind) {
        Some(kind) => kind,
        None => return Ok(None),
    };
    let count = usize::try_from(span_len)
        .map_err(|_| HydrationError::RegionTooLarge(edge_kind, span_len))?;
    Ok(Some(read_edge_entries_by_ref_from_stable_memory(
        manager,
        memory,
        edge_kind,
        vertex.edge_ref(),
        count,
    )?))
}

/// Summarizes one logical vertex-table window directly from stable memory.
pub fn summarize_vertex_window_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    start_ordinal: usize,
    count: usize,
) -> Result<Option<SurfaceVertexWindowSummary>, HydrationError> {
    let entries =
        read_vertex_entries_from_stable_memory(manager, memory, kind, start_ordinal, count)?;
    Ok(summarize_vertex_window_entries(start_ordinal, &entries))
}

/// Estimates a lower-bound reserve hint for one vertex-table window directly from stable memory.
pub fn estimate_vertex_window_reserve_hint_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    start_ordinal: usize,
    count: usize,
    insert_policy_and_anchor_live_degree_after_rebalance: (GraphInsertPolicy, u32),
    incoming_live_entries: u32,
) -> Result<Option<SurfaceVertexWindowReserveHint>, HydrationError> {
    let (insert_policy, anchor_live_degree_after_rebalance) =
        insert_policy_and_anchor_live_degree_after_rebalance;
    let Some(summary) =
        summarize_vertex_window_from_stable_memory(manager, memory, kind, start_ordinal, count)?
    else {
        return Ok(None);
    };
    Ok(insert_policy.estimate_vertex_window_reserve_hint(
        summary,
        anchor_live_degree_after_rebalance,
        incoming_live_entries,
    ))
}

/// Writes both directional surface runtimes back to stable memory.
pub fn write_surface_runtimes_to_stable_memory(
    manager: &mut RegionManager,
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
    manager: &mut RegionManager,
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
    manager: &mut RegionManager,
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
///
/// Each region's recorded [`RegionRef::logical_len_bytes`] must already equal the
/// serialized payload length; otherwise [`WritebackError::LengthMismatch`] is
/// returned. Use [`RegionManager::set_region_logical_len`] /
/// [`RegionManager::ensure_bucket_region_capacity`] (or a dirty flush after
/// in-memory updates) when the layout must track a new serialized size.
pub fn write_surface_runtime_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    runtime: &SurfaceRuntime,
) -> Result<(), WritebackError> {
    let surface_layout = runtime.layout;
    let vertex_bytes = encode_vertex_entries(&runtime.vertices);
    let label_index_bytes =
        encode_label_index_region(&runtime.label_index_entries, &runtime.label_ranges);
    let overflow_bytes = encode_overflow_entries(&runtime.overflow_entries);
    let vertex_region = manager
        .layout
        .region(surface_layout.vertex_table_region().region_kind())
        .ok_or(WritebackError::MissingRegionDefinition(
            surface_layout.vertex_table_region().region_kind(),
        ))?;
    let label_region = manager
        .layout
        .region(surface_layout.label_index_region().region_kind())
        .ok_or(WritebackError::MissingRegionDefinition(
            surface_layout.label_index_region().region_kind(),
        ))?;
    let overflow_region = manager
        .layout
        .region(surface_layout.segment_log_region().region_kind())
        .ok_or(WritebackError::MissingRegionDefinition(
            surface_layout.segment_log_region().region_kind(),
        ))?;
    write_region_bytes_to_stable_memory(manager, memory, vertex_region, &vertex_bytes)?;
    write_edge_storage_to_stable_memory(
        manager,
        memory,
        surface_layout.edge_entries_region().region_kind(),
        &runtime.base_entries,
        true,
    )?;
    write_region_bytes_to_stable_memory(manager, memory, label_region, &label_index_bytes)?;
    write_region_bytes_to_stable_memory(manager, memory, overflow_region, &overflow_bytes)?;
    Ok(())
}

/// Writes only dirty regions of one surface runtime back to stable memory.
///
/// For each dirty region, updates recorded logical length to the serialized length
/// before writing (so e.g. compacted label sidecars can shrink the logical payload).
pub fn write_dirty_surface_runtime_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    runtime: &mut SurfaceRuntime,
) -> Result<(), WritebackError> {
    let layout = runtime.layout;
    let dirty = runtime.dirty_regions();

    SURFACE_DIRTY_FLUSH_SCRATCH.with(|scratch| -> Result<(), WritebackError> {
        let mut buf = scratch.borrow_mut();

        if dirty.vertex_table {
            let kind = layout.vertex_table_region().region_kind();
            let region_before = manager
                .layout
                .region(kind)
                .ok_or(WritebackError::MissingRegionDefinition(kind))?;
            let el = SERIALIZED_VERTEX_ENTRY_LEN as u64;
            let new_len_u64 = u64::try_from(runtime.vertices.len())
                .map_err(|_| WritebackError::RegionTooLarge(kind, u64::MAX))?
                .checked_mul(el)
                .ok_or(WritebackError::RegionTooLarge(kind, u64::MAX))?;

            let suffix_start = dirty
                .vertex_table_suffix_start
                .and_then(|s| usize::try_from(s).ok())
                .filter(|&s| s < runtime.vertices.len());

            let try_tail = suffix_start.is_some();

            let mut wrote_vertex_tail = false;
            if try_tail {
                let s = suffix_start.expect("suffix_start is_some when try_tail");
                let old_log = region_before.logical_len_bytes;
                let expected_prefix = (s as u64)
                    .checked_mul(el)
                    .ok_or(WritebackError::RegionTooLarge(kind, u64::MAX))?;
                if old_log == expected_prefix {
                    encode_vertex_entries_suffix_into(&runtime.vertices, s, &mut buf);
                    let _ = manager.set_region_logical_len(kind, new_len_u64);
                    match region_before.storage_kind() {
                        RegionStorageKind::Extent => {
                            let extent = manager
                                .region_extent(kind)
                                .ok_or(WritebackError::MissingExtentRegion(kind))?;
                            ensure_memory_covers(
                                memory,
                                extent
                                    .addr
                                    .0
                                    .checked_add(new_len_u64)
                                    .ok_or(WritebackError::RegionTooLarge(kind, new_len_u64))?,
                            )?;
                            if !buf.is_empty() {
                                memory.write(
                                    extent
                                        .addr
                                        .0
                                        .checked_add(expected_prefix)
                                        .ok_or(WritebackError::RegionTooLarge(kind, u64::MAX))?,
                                    &buf,
                                );
                            }
                        }
                        RegionStorageKind::BucketChain => {
                            write_bucket_chain_region_suffix_to_stable_memory(
                                manager,
                                memory,
                                kind,
                                expected_prefix as usize,
                                &buf,
                                new_len_u64,
                            )?;
                        }
                    }
                    wrote_vertex_tail = true;
                }
            }
            if !wrote_vertex_tail {
                encode_vertex_entries_into(&runtime.vertices, &mut buf);
                let _ = manager.set_region_logical_len(kind, buf.len() as u64);
                let region = manager
                    .layout
                    .region(kind)
                    .ok_or(WritebackError::MissingRegionDefinition(kind))?;
                write_region_bytes_to_stable_memory(manager, memory, region, &buf)?;
            }
        }
        if dirty.edge_entries {
            let kind = layout.edge_entries_region().region_kind();
            write_edge_storage_to_stable_memory(
                manager,
                memory,
                kind,
                &runtime.base_entries,
                true,
            )?;
        }
        if dirty.label_index {
            let kind = layout.label_index_region().region_kind();
            let mut wrote_label_incremental = false;
            if let Some(append_from) = dirty.label_index_append_from
                && write_label_index_region_incremental_extent(
                    manager,
                    memory,
                    kind,
                    append_from,
                    &runtime.label_index_entries,
                    &runtime.label_ranges,
                )?
            {
                wrote_label_incremental = true;
            }
            if !wrote_label_incremental {
                encode_label_index_region_into(
                    &runtime.label_index_entries,
                    &runtime.label_ranges,
                    &mut buf,
                );
                let _ = manager.set_region_logical_len(kind, buf.len() as u64);
                let region = manager
                    .layout
                    .region(kind)
                    .ok_or(WritebackError::MissingRegionDefinition(kind))?;
                write_region_bytes_to_stable_memory(manager, memory, region, &buf)?;
            }
        }
        if dirty.segment_log {
            encode_overflow_entries_into(&runtime.overflow_entries, &mut buf);
            let kind = layout.segment_log_region().region_kind();
            let _ = manager.set_region_logical_len(kind, buf.len() as u64);
            let region = manager
                .layout
                .region(kind)
                .ok_or(WritebackError::MissingRegionDefinition(kind))?;
            write_region_bytes_to_stable_memory(manager, memory, region, &buf)?;
        }

        Ok(())
    })?;

    runtime.clear_dirty_regions();
    Ok(())
}

/// Serializes vertex-table entries into the fixed-width low-level format.
pub fn encode_vertex_entries(entries: &[VertexEntry]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(entries.len() * SERIALIZED_VERTEX_ENTRY_LEN);
    encode_vertex_entries_into(entries, &mut bytes);
    bytes
}

/// Encodes vertex-table entries into `out` (clears `out` first).
///
/// On little-endian targets, [`VertexEntry`] is `repr(C)` with wire-compatible layout
/// (`SERIALIZED_VERTEX_ENTRY_LEN` == `size_of::<VertexEntry>()`), so this uses one memcpy.
pub fn encode_vertex_entries_into(entries: &[VertexEntry], out: &mut Vec<u8>) {
    out.clear();
    let nbytes = entries.len().saturating_mul(SERIALIZED_VERTEX_ENTRY_LEN);
    if nbytes == 0 {
        return;
    }
    debug_assert_eq!(size_of::<VertexEntry>(), SERIALIZED_VERTEX_ENTRY_LEN);
    #[cfg(target_endian = "little")]
    {
        out.reserve(nbytes);
        unsafe {
            let src = core::slice::from_raw_parts(entries.as_ptr().cast::<u8>(), nbytes);
            out.extend_from_slice(src);
        }
    }
    #[cfg(not(target_endian = "little"))]
    {
        out.reserve(nbytes);
        for entry in entries {
            out.extend_from_slice(&entry.edge_index.raw.to_le_bytes());
            out.extend_from_slice(&entry.degree.to_le_bytes());
            out.extend_from_slice(&entry.log_offset.to_le_bytes());
        }
    }
}

/// Encodes `entries[start..]` into `out` (clears `out` first).
pub fn encode_vertex_entries_suffix_into(entries: &[VertexEntry], start: usize, out: &mut Vec<u8>) {
    encode_vertex_entries_into(entries.get(start..).unwrap_or(&[]), out);
}

fn encode_label_ranges_into(ranges: &[VertexLabelRange], out: &mut Vec<u8>) {
    out.clear();
    out.reserve(ranges.len().saturating_mul(SERIALIZED_LABEL_RANGE_LEN));
    for range in ranges {
        out.extend_from_slice(&range.label_id.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&range.start.to_le_bytes());
        out.extend_from_slice(&range.len.to_le_bytes());
    }
}

fn write_label_index_region_incremental_extent(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    append_from: u32,
    index_entries: &[VertexLabelIndexEntry],
    ranges: &[VertexLabelRange],
) -> Result<bool, WritebackError> {
    let el = SERIALIZED_LABEL_INDEX_ENTRY_LEN as u64;
    let rl = SERIALIZED_LABEL_RANGE_LEN as u64;
    let region = manager
        .layout
        .region(kind)
        .ok_or(WritebackError::MissingRegionDefinition(kind))?;
    if region.storage_kind() != RegionStorageKind::Extent {
        return Ok(false);
    }
    let extent = manager
        .region_extent(kind)
        .ok_or(WritebackError::MissingExtentRegion(kind))?;
    let old_logical = region.logical_len_bytes;
    if old_logical < SERIALIZED_LABEL_INDEX_HEADER_LEN as u64 {
        return Ok(false);
    }
    let mut hdr = [0u8; SERIALIZED_LABEL_INDEX_HEADER_LEN];
    memory.read(extent.addr.0, &mut hdr);
    let old_idx = u32::from_le_bytes(hdr[0..4].try_into().expect("header index count")) as u64;
    let old_range_cnt =
        u32::from_le_bytes(hdr[4..8].try_into().expect("header range count")) as u64;
    let new_idx = u64::try_from(index_entries.len())
        .map_err(|_| WritebackError::RegionTooLarge(kind, u64::MAX))?;
    let new_range_cnt =
        u64::try_from(ranges.len()).map_err(|_| WritebackError::RegionTooLarge(kind, u64::MAX))?;
    if u64::from(append_from) != old_idx
        || old_range_cnt != new_range_cnt
        || (append_from as usize) > index_entries.len()
    {
        return Ok(false);
    }
    let header_len = SERIALIZED_LABEL_INDEX_HEADER_LEN as u64;
    let old_entries_bytes = old_idx
        .checked_mul(el)
        .ok_or(WritebackError::RegionTooLarge(kind, old_logical))?;
    let old_ranges_bytes = old_range_cnt
        .checked_mul(rl)
        .ok_or(WritebackError::RegionTooLarge(kind, old_logical))?;
    let old_total = header_len
        .checked_add(old_entries_bytes)
        .and_then(|v| v.checked_add(old_ranges_bytes))
        .ok_or(WritebackError::RegionTooLarge(kind, old_logical))?;
    if old_total != old_logical {
        return Ok(false);
    }
    let new_entries_bytes = new_idx
        .checked_mul(el)
        .ok_or(WritebackError::RegionTooLarge(kind, new_idx))?;
    let new_ranges_bytes = new_range_cnt
        .checked_mul(rl)
        .ok_or(WritebackError::RegionTooLarge(kind, new_range_cnt))?;
    let new_total = header_len
        .checked_add(new_entries_bytes)
        .and_then(|v| v.checked_add(new_ranges_bytes))
        .ok_or(WritebackError::RegionTooLarge(kind, u64::MAX))?;
    let _ = manager.set_region_logical_len(kind, new_total);
    let idx_u32 = u32::try_from(index_entries.len())
        .map_err(|_| WritebackError::RegionTooLarge(kind, u64::MAX))?;
    let range_u32 =
        u32::try_from(ranges.len()).map_err(|_| WritebackError::RegionTooLarge(kind, u64::MAX))?;
    let mut new_hdr = [0u8; SERIALIZED_LABEL_INDEX_HEADER_LEN];
    new_hdr[0..4].copy_from_slice(&idx_u32.to_le_bytes());
    new_hdr[4..8].copy_from_slice(&range_u32.to_le_bytes());
    ensure_memory_covers(
        memory,
        extent
            .addr
            .0
            .checked_add(new_total)
            .ok_or(WritebackError::RegionTooLarge(kind, new_total))?,
    )?;
    memory.write(extent.addr.0, &new_hdr);
    let mut buf = Vec::with_capacity(
        index_entries
            .len()
            .saturating_sub(append_from as usize)
            .saturating_mul(SERIALIZED_LABEL_INDEX_ENTRY_LEN),
    );
    for entry in index_entries.iter().skip(append_from as usize) {
        buf.extend_from_slice(&entry.start.to_le_bytes());
        buf.extend_from_slice(&entry.len.to_le_bytes());
    }
    let entry_off = header_len
        .checked_add(
            u64::from(append_from)
                .checked_mul(el)
                .ok_or(WritebackError::RegionTooLarge(kind, u64::MAX))?,
        )
        .ok_or(WritebackError::RegionTooLarge(kind, u64::MAX))?;
    if !buf.is_empty() {
        memory.write(
            extent
                .addr
                .0
                .checked_add(entry_off)
                .ok_or(WritebackError::RegionTooLarge(kind, u64::MAX))?,
            &buf,
        );
    }
    encode_label_ranges_into(ranges, &mut buf);
    let range_off = header_len
        .checked_add(
            new_idx
                .checked_mul(el)
                .ok_or(WritebackError::RegionTooLarge(kind, u64::MAX))?,
        )
        .ok_or(WritebackError::RegionTooLarge(kind, u64::MAX))?;
    if !buf.is_empty() {
        memory.write(
            extent
                .addr
                .0
                .checked_add(range_off)
                .ok_or(WritebackError::RegionTooLarge(kind, u64::MAX))?,
            &buf,
        );
    }
    Ok(true)
}

/// Serializes hot base-edge entries into the fixed-width low-level format.
pub fn encode_edge_entries(entries: &[EdgeEntry]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(entries.len() * SERIALIZED_EDGE_ENTRY_LEN);
    for entry in entries {
        bytes.extend_from_slice(&entry.target.as_bytes());
        bytes.extend_from_slice(&entry.meta.to_le_bytes());
    }
    bytes
}

/// Serializes overflow-log entries into the fixed-width low-level format.
pub fn encode_overflow_entries(entries: &[OverflowEntry]) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(entries.len() * SERIALIZED_OVERFLOW_ENTRY_LEN);
    encode_overflow_entries_into(entries, &mut bytes);
    bytes
}

/// Encodes overflow-log entries into `out` (clears `out` first).
pub fn encode_overflow_entries_into(entries: &[OverflowEntry], out: &mut Vec<u8>) {
    out.clear();
    let nbytes = entries.len().saturating_mul(SERIALIZED_OVERFLOW_ENTRY_LEN);
    out.reserve(nbytes);
    for entry in entries {
        out.extend_from_slice(&entry.edge_id.to_le_bytes());
        out.extend_from_slice(&entry.entry.target.as_bytes());
        out.extend_from_slice(&entry.entry.meta.to_le_bytes());
        out.extend_from_slice(&entry.next.raw.to_le_bytes());
    }
}

/// Serializes the surface-local label sidecar into its fixed-width region format.
pub fn encode_label_index_region(
    index_entries: &[VertexLabelIndexEntry],
    ranges: &[VertexLabelRange],
) -> Vec<u8> {
    let mut bytes = Vec::new();
    encode_label_index_region_into(index_entries, ranges, &mut bytes);
    bytes
}

/// Encodes label sidecar bytes into `out` (clears `out` first).
pub fn encode_label_index_region_into(
    index_entries: &[VertexLabelIndexEntry],
    ranges: &[VertexLabelRange],
    out: &mut Vec<u8>,
) {
    out.clear();
    if index_entries.is_empty() && ranges.is_empty() {
        return;
    }
    let cap = SERIALIZED_LABEL_INDEX_HEADER_LEN
        + index_entries.len() * SERIALIZED_LABEL_INDEX_ENTRY_LEN
        + ranges.len() * SERIALIZED_LABEL_RANGE_LEN;
    out.reserve(cap);
    out.extend_from_slice(&(index_entries.len() as u32).to_le_bytes());
    out.extend_from_slice(&(ranges.len() as u32).to_le_bytes());
    for entry in index_entries {
        out.extend_from_slice(&entry.start.to_le_bytes());
        out.extend_from_slice(&entry.len.to_le_bytes());
    }
    for range in ranges {
        out.extend_from_slice(&range.label_id.to_le_bytes());
        out.extend_from_slice(&0_u16.to_le_bytes());
        out.extend_from_slice(&range.start.to_le_bytes());
        out.extend_from_slice(&range.len.to_le_bytes());
    }
}

fn write_region_bytes_to_stable_memory(
    manager: &mut RegionManager,
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
    let region = match region.storage_kind() {
        RegionStorageKind::Extent => region,
        RegionStorageKind::BucketChain => {
            manager
                .ensure_bucket_region_capacity(kind, actual)
                .ok_or(WritebackError::MissingBucketChain(kind))?;
            manager
                .layout
                .region(kind)
                .ok_or(WritebackError::MissingRegionDefinition(kind))?
        }
    };
    match region.storage_kind() {
        RegionStorageKind::Extent => {
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
        }
        RegionStorageKind::BucketChain => {
            write_bucket_chain_region_bytes_to_stable_memory(manager, memory, region, bytes)?
        }
    }
    Ok(())
}

fn read_region_bytes_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    region: RegionRef,
) -> Result<Vec<u8>, HydrationError> {
    let kind = region.region_kind();
    let logical_len = usize::try_from(region.logical_len_bytes)
        .map_err(|_| HydrationError::RegionTooLarge(kind, region.logical_len_bytes))?;

    match region.storage_kind() {
        RegionStorageKind::Extent => {
            let extent = manager
                .region_extent(kind)
                .ok_or(HydrationError::MissingExtentRegion(kind))?;
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
            Ok(bytes)
        }
        RegionStorageKind::BucketChain => {
            let chain = manager
                .bucket_chain(kind)
                .ok_or(HydrationError::MissingBucketChain(kind))?;
            let bucket_size = usize::try_from(manager.bucket_size_bytes())
                .map_err(|_| HydrationError::RegionTooLarge(kind, region.logical_len_bytes))?;
            let mut bytes = vec![0_u8; logical_len];
            let mut offset = 0usize;
            let mut cursor = chain.head;

            while !cursor.is_null() && offset < logical_len {
                let header = manager
                    .bucket_header(cursor)
                    .ok_or(HydrationError::MissingBucketChain(kind))?;
                let len = bucket_size.min(logical_len - offset);
                memory.read(header.addr.0, &mut bytes[offset..offset + len]);
                offset += len;
                cursor = header.next;
            }

            if offset < logical_len {
                return Err(HydrationError::TruncatedBucketChain {
                    kind,
                    logical_len_bytes: region.logical_len_bytes,
                    read: offset as u64,
                });
            }
            Ok(bytes)
        }
    }
}

fn hydrate_edge_storage_from_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
) -> Result<SurfaceBaseStorage, HydrationError> {
    let base_region = manager
        .layout
        .region(kind)
        .ok_or(HydrationError::MissingRegionDefinition(kind))?;
    let mut segments = BTreeMap::new();
    segments.insert(
        0,
        decode_edge_entries(
            kind,
            &read_region_bytes_from_stable_memory(manager, memory, base_region)?,
        )?,
    );

    if let Some(directory) = manager.edge_segment_directory(kind) {
        for header in directory.iter().copied() {
            let Some((_, extent)) = manager.resolve_edge_ref(
                kind,
                EdgeRef::from_raw((header.segment_id as u64) << EdgeRef::START_SLOT_BITS),
            ) else {
                continue;
            };
            let byte_len = usize::try_from(header.slot_capacity)
                .ok()
                .and_then(|slots| slots.checked_mul(SERIALIZED_EDGE_ENTRY_LEN))
                .ok_or(HydrationError::RegionTooLarge(kind, header.slot_capacity))?;
            if byte_len > usize::try_from(extent.len_bytes).unwrap_or(usize::MAX) {
                return Err(HydrationError::LogicalLengthExceedsExtent {
                    kind,
                    logical_len_bytes: byte_len as u64,
                    extent_len_bytes: extent.len_bytes,
                });
            }
            let mut bytes = vec![0u8; byte_len];
            if byte_len > 0 {
                memory.read(extent.addr.0, &mut bytes);
            }
            segments.insert(header.segment_id, decode_edge_entries(kind, &bytes)?);
        }
    }

    let mut slot_capacities = BTreeMap::new();
    slot_capacities.insert(
        0,
        base_region.logical_len_bytes / SERIALIZED_EDGE_ENTRY_LEN as u64,
    );
    if let Some(directory) = manager.edge_segment_directory(kind) {
        for header in directory.iter().copied() {
            slot_capacities.insert(header.segment_id, header.slot_capacity);
        }
    }
    Ok(SurfaceBaseStorage::from_segmented_with_slot_capacities(
        segments,
        slot_capacities,
    ))
}

fn write_edge_storage_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    storage: &SurfaceBaseStorage,
    update_region_len: bool,
) -> Result<(), WritebackError> {
    let _p = crate::bench_profile::PhaseGuard::new("hydration_write_edge_storage");
    storage.foreach_segment_entry_slices(|segment_id, entries| {
        let bytes = encode_edge_entries(entries);
        if segment_id == 0 {
            if update_region_len {
                let _ = manager.set_region_logical_len(kind, bytes.len() as u64);
            }
            let region = manager
                .layout
                .region(kind)
                .ok_or(WritebackError::MissingRegionDefinition(kind))?;
            write_region_bytes_to_stable_memory(manager, memory, region, &bytes)?;
            return Ok(());
        }

        let (header, extent) = manager
            .resolve_edge_ref(
                kind,
                EdgeRef::from_raw((segment_id as u64) << EdgeRef::START_SLOT_BITS),
            )
            .ok_or(WritebackError::MissingExtentRegion(kind))?;
        let capacity_bytes = header
            .slot_capacity
            .checked_mul(SERIALIZED_EDGE_ENTRY_LEN as u64)
            .ok_or(WritebackError::RegionTooLarge(kind, header.slot_capacity))?;
        let actual = bytes.len() as u64;
        if actual > capacity_bytes || actual > extent.len_bytes {
            return Err(WritebackError::LogicalLengthExceedsExtent {
                kind,
                logical_len_bytes: actual,
                extent_len_bytes: extent.len_bytes.min(capacity_bytes),
            });
        }
        ensure_memory_covers(memory, extent.addr.0 + actual)?;
        if actual > 0 {
            memory.write(extent.addr.0, &bytes);
        }
        Ok(())
    })
}

fn write_bucket_chain_region_bytes_to_stable_memory(
    manager: &RegionManager,
    memory: &impl Memory,
    region: RegionRef,
    bytes: &[u8],
) -> Result<(), WritebackError> {
    let kind = region.region_kind();
    let chain = manager
        .bucket_chain(kind)
        .ok_or(WritebackError::MissingBucketChain(kind))?;
    let bucket_size = usize::try_from(manager.bucket_size_bytes())
        .map_err(|_| WritebackError::RegionTooLarge(kind, region.logical_len_bytes))?;
    let required_buckets = bytes.len().max(1).div_ceil(bucket_size);
    let last_byte_exclusive = manager
        .bucket_header(chain.tail)
        .map(|header| header.addr.0 + manager.bucket_size_bytes())
        .ok_or(WritebackError::MissingBucketChain(kind))?;
    ensure_memory_covers(memory, last_byte_exclusive)?;

    let mut cursor = chain.head;
    let mut offset = 0usize;
    let mut written_buckets = 0usize;
    while !cursor.is_null() && written_buckets < required_buckets {
        let header = manager
            .bucket_header(cursor)
            .ok_or(WritebackError::MissingBucketChain(kind))?;
        let remaining = bytes.len().saturating_sub(offset);
        let len = bucket_size.min(remaining);
        let mut padded = vec![0u8; bucket_size];
        if len > 0 {
            padded[..len].copy_from_slice(&bytes[offset..offset + len]);
        }
        memory.write(header.addr.0, &padded);
        offset += len;
        written_buckets += 1;
        cursor = header.next;
    }

    if written_buckets < required_buckets {
        return Err(WritebackError::TruncatedBucketChain {
            kind,
            logical_len_bytes: region.logical_len_bytes,
            written: offset as u64,
        });
    }

    Ok(())
}

fn write_bucket_chain_region_suffix_to_stable_memory(
    manager: &mut RegionManager,
    memory: &impl Memory,
    kind: RegionKind,
    start_offset: usize,
    bytes: &[u8],
    new_logical_len: u64,
) -> Result<(), WritebackError> {
    manager
        .ensure_bucket_region_capacity(kind, new_logical_len)
        .ok_or(WritebackError::MissingBucketChain(kind))?;
    let chain = manager
        .bucket_chain(kind)
        .ok_or(WritebackError::MissingBucketChain(kind))?;
    let bucket_size = usize::try_from(manager.bucket_size_bytes())
        .map_err(|_| WritebackError::RegionTooLarge(kind, new_logical_len))?;
    let last_byte_exclusive = manager
        .bucket_header(chain.tail)
        .map(|header| header.addr.0 + manager.bucket_size_bytes())
        .ok_or(WritebackError::MissingBucketChain(kind))?;
    ensure_memory_covers(memory, last_byte_exclusive)?;

    let mut remaining_skip = start_offset;
    let mut written = 0usize;
    let mut cursor = chain.head;

    while !cursor.is_null() && written < bytes.len() {
        let header = manager
            .bucket_header(cursor)
            .ok_or(WritebackError::MissingBucketChain(kind))?;
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
                .ok_or(WritebackError::RegionTooLarge(kind, new_logical_len))?,
            &bytes[written..written + take],
        );
        written += take;
        remaining_skip = 0;
        cursor = header.next;
    }

    if written < bytes.len() {
        return Err(WritebackError::TruncatedBucketChain {
            kind,
            logical_len_bytes: new_logical_len,
            written: (start_offset + written) as u64,
        });
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

    let base_entries = decode_edge_entries(layout.edge_entries_region().region_kind(), base_bytes)?;
    let base_storage = SurfaceBaseStorage::from_segmented(BTreeMap::from([(0, base_entries)]));

    Ok(SurfaceRuntime::from_decoded_regions(
        layout,
        decode_vertex_entries(layout.vertex_table_region().region_kind(), vertex_bytes)?,
        base_storage,
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
    if !bytes.len().is_multiple_of(SERIALIZED_VERTEX_ENTRY_LEN) {
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
///
/// Wire layout per entry: **4 bytes** big-endian [`VertexRef`] + **4 bytes** little-endian
/// [`EdgeMeta`](super::edge::EdgeMeta). This supersedes the older 6+2 byte row shape;
/// persisted images using the legacy layout are not supported.
pub fn decode_edge_entries(
    kind: RegionKind,
    bytes: &[u8],
) -> Result<Vec<EdgeEntry>, HydrationError> {
    if !bytes.len().is_multiple_of(SERIALIZED_EDGE_ENTRY_LEN) {
        return Err(HydrationError::InvalidLength {
            kind,
            expected_multiple: SERIALIZED_EDGE_ENTRY_LEN,
            actual: bytes.len(),
        });
    }

    let mut entries = Vec::with_capacity(bytes.len() / SERIALIZED_EDGE_ENTRY_LEN);
    for chunk in bytes.chunks_exact(SERIALIZED_EDGE_ENTRY_LEN) {
        let target = VertexRef::from_be_bytes(chunk[0..4].try_into().expect("fixed slice"));
        let meta = EdgeMeta::from_le_bytes(chunk[4..8].try_into().expect("fixed slice"));
        entries.push(EdgeEntry::new(target, meta));
    }
    Ok(entries)
}

/// Decodes overflow-log entries from the fixed-width low-level format.
pub fn decode_overflow_entries(
    kind: RegionKind,
    bytes: &[u8],
) -> Result<Vec<OverflowEntry>, HydrationError> {
    if !bytes.len().is_multiple_of(SERIALIZED_OVERFLOW_ENTRY_LEN) {
        return Err(HydrationError::InvalidLength {
            kind,
            expected_multiple: SERIALIZED_OVERFLOW_ENTRY_LEN,
            actual: bytes.len(),
        });
    }

    let mut entries = Vec::with_capacity(bytes.len() / SERIALIZED_OVERFLOW_ENTRY_LEN);
    for chunk in bytes.chunks_exact(SERIALIZED_OVERFLOW_ENTRY_LEN) {
        let edge_id = u64::from_le_bytes(chunk[0..8].try_into().expect("fixed slice"));
        let target = VertexRef::from_be_bytes(chunk[8..12].try_into().expect("fixed slice"));
        let meta = EdgeMeta::from_le_bytes(chunk[12..16].try_into().expect("fixed slice"));
        let next = i32::from_le_bytes(chunk[16..20].try_into().expect("fixed slice"));
        entries.push(OverflowEntry::new(
            edge_id,
            EdgeEntry::new(target, meta),
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

    // Some write paths can clear the header to (0, 0) without shrinking `logical_len_bytes` or
    // rewriting the trailing index-entry blob. Accept a tail of whole index-entry chunks as
    // orphaned entries with an empty range table so hydration still succeeds.
    if index_count == 0 && range_count == 0 && bytes.len() > SERIALIZED_LABEL_INDEX_HEADER_LEN {
        let tail = &bytes[SERIALIZED_LABEL_INDEX_HEADER_LEN..];
        if tail.len().is_multiple_of(SERIALIZED_LABEL_INDEX_ENTRY_LEN) {
            let mut index_entries =
                Vec::with_capacity(tail.len() / SERIALIZED_LABEL_INDEX_ENTRY_LEN);
            for chunk in tail.chunks_exact(SERIALIZED_LABEL_INDEX_ENTRY_LEN) {
                let start = u32::from_le_bytes(chunk[0..4].try_into().expect("fixed slice"));
                let len = u32::from_le_bytes(chunk[4..8].try_into().expect("fixed slice"));
                index_entries.push(VertexLabelIndexEntry::new(start, len));
            }
            return Ok((index_entries, Vec::new()));
        }
    }

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
        HydrationError, InMemoryRegionByteSource, RegionByteSource, StableMemoryRegionByteSource,
        StableVertexTableReader, WritebackError, decode_edge_entries, decode_label_index_region,
        decode_overflow_entries, decode_vertex_entries, encode_edge_entries,
        encode_label_index_region, encode_overflow_entries, encode_vertex_entries,
        estimate_vertex_window_reserve_hint_from_stable_memory, forward_surface_from_layout,
        hydrate_forward_surface_runtime, hydrate_reverse_surface_runtime, hydrate_surface_runtime,
        hydrate_surface_runtimes_from_layout, hydrate_surface_runtimes_from_stable_memory,
        read_edge_entries_by_ref_from_stable_memory, read_vertex_base_edge_ref_from_stable_memory,
        read_vertex_base_entries_from_stable_memory, read_vertex_base_entry_from_stable_memory,
        read_vertex_entry_from_stable_memory, read_vertex_reserved_base_entries_from_stable_memory,
        read_vertex_reserved_span_len_from_stable_memory, reverse_surface_from_layout,
        summarize_vertex_window_from_stable_memory, write_dirty_surface_runtime_to_stable_memory,
        write_forward_surface_runtime_to_stable_memory, write_surface_runtime_to_stable_memory,
        write_surface_runtimes_to_stable_memory,
    };
    use crate::VecMemory;
    use crate::low_level::{
        BucketChain, BucketId, BucketSizeInPages, EMPTY_LOG_OFFSET, EdgeEntry, EdgeIndex, EdgeMeta,
        EdgeRef, EdgeSegmentHeader, EdgeSegmentState, ExtentChain, ExtentId, ForwardSurface,
        GraphInsertPolicy, LogOffset, OverflowEntry, RegionKind, RegionManager,
        RegionManagerLayout, RegionRef, RegionStorageKind, ReverseSurface, SurfaceRegions,
        VertexEntry, VertexLabelIndexEntry, VertexLabelRange, VertexRef, WasmPages,
    };
    use ic_stable_structures::Memory;
    use std::cell::RefCell;

    fn assert_same_base_edge_payload(
        left: &super::SurfaceBaseStorage,
        right: &super::SurfaceBaseStorage,
    ) {
        assert_eq!(left.len(), right.len(), "base entry counts differ");
        assert!(
            super::SurfaceBaseStorage::iter(left)
                .zip(super::SurfaceBaseStorage::iter(right))
                .all(|(a, b)| a == b),
            "base edge payloads differ"
        );
    }

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
        let bytes = encode_edge_entries(&[EdgeEntry::new(
            VertexRef::from(9u8),
            EdgeMeta::new(4, false),
        )]);
        let decoded = decode_edge_entries(RegionKind::ForwardEdgeEntries, &bytes)
            .expect("edge entries should decode");
        assert_eq!(decoded[0].meta.local_id(), Some(4));
        assert_eq!(u64::from(decoded[0].target), 9);
    }

    #[test]
    fn decode_overflow_entries_reads_fixed_width_format() {
        let bytes = encode_overflow_entries(&[OverflowEntry::new(
            42,
            EdgeEntry::new(VertexRef::from(11u8), EdgeMeta::new(5, false)),
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(1, false)),
            ]),
        );
        source.insert(RegionKind::ForwardLabelIndex, Vec::new());
        source.insert(
            RegionKind::ForwardSegmentLog,
            encode_overflow_entries(&[OverflowEntry::new(
                41,
                EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(3, false)),
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
            encode_edge_entries(&[EdgeEntry::new(
                VertexRef::from(1u8),
                EdgeMeta::new(1, false),
            )]),
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
            encode_edge_entries(&[EdgeEntry::new(
                VertexRef::from(2u8),
                EdgeMeta::new(2, false),
            )]),
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
            encode_edge_entries(&[EdgeEntry::new(
                VertexRef::from(1u8),
                EdgeMeta::new(1, false),
            )]),
        );
        source.insert(RegionKind::ForwardLabelIndex, Vec::new());
        source.insert(RegionKind::ForwardSegmentLog, Vec::new());
        source.insert(
            RegionKind::ReverseVertexTable,
            encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
        );
        source.insert(
            RegionKind::ReverseEdgeEntries,
            encode_edge_entries(&[EdgeEntry::new(
                VertexRef::from(2u8),
                EdgeMeta::new(2, false),
            )]),
        );
        source.insert(RegionKind::ReverseLabelIndex, Vec::new());
        source.insert(RegionKind::ReverseSegmentLog, Vec::new());

        let runtimes = hydrate_surface_runtimes_from_layout(&layout, &source)
            .expect("both surfaces should hydrate");

        assert_eq!(runtimes.forward.0.base_entries.len(), 1);
        assert_eq!(runtimes.reverse.0.base_entries.len(), 1);
        assert_eq!(
            u64::from(
                runtimes
                    .forward
                    .0
                    .base_entries
                    .get(0)
                    .copied()
                    .expect("forward base entry 0")
                    .target
            ),
            1
        );
        assert_eq!(
            u64::from(
                runtimes
                    .reverse
                    .0
                    .base_entries
                    .get(0)
                    .copied()
                    .expect("reverse base entry 0")
                    .target
            ),
            2
        );
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
    fn stable_memory_region_byte_source_reads_bucket_backed_region_payloads() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        let region = manager.define_bucket_region(
            RegionKind::ForwardVertexTable,
            BucketChain::new(BucketId::NULL, BucketId::NULL, 0),
        );
        manager
            .ensure_bucket_region_capacity(RegionKind::ForwardVertexTable, 16)
            .expect("bucket capacity should allocate");
        let expected = encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(7), 3, -1)]);
        let bucket = manager
            .bucket_chain(RegionKind::ForwardVertexTable)
            .expect("bucket chain should exist");
        let head = manager
            .bucket_header(bucket.head)
            .expect("head bucket should exist");
        let memory = TestStableMemory::default();
        memory.write(head.addr.0, &expected);

        let source = StableMemoryRegionByteSource::from_region_manager(&manager, &memory)
            .expect("bucket-backed source should materialize");

        assert_eq!(source.region_bytes(region), Some(expected.as_slice()));
    }

    #[test]
    fn read_vertex_entry_from_stable_memory_reads_extent_backed_vertex_table() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                32,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        let extent = manager
            .region_extent(RegionKind::ForwardVertexTable)
            .expect("extent metadata should exist");
        let entries = [
            VertexEntry::new(EdgeIndex::new(7), 3, -1),
            VertexEntry::new(EdgeIndex::new(11), 2, 4),
        ];
        let memory = TestStableMemory::default();
        memory.write(extent.addr.0, &encode_vertex_entries(&entries));

        let entry = read_vertex_entry_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            1,
        )
        .expect("direct read should succeed");

        assert_eq!(entry, Some(entries[1]));
    }

    #[test]
    fn read_vertex_entry_from_stable_memory_reads_bucket_backed_vertex_table() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::ForwardVertexTable,
            BucketChain::new(BucketId::NULL, BucketId::NULL, 0),
        );
        let entry_count = 4_097usize;
        let logical_len = (entry_count * 16) as u64;
        manager
            .ensure_bucket_region_capacity(RegionKind::ForwardVertexTable, logical_len)
            .expect("bucket capacity should allocate");
        let entries: Vec<_> = (0..entry_count)
            .map(|ordinal| {
                VertexEntry::new(
                    EdgeIndex::new((ordinal as u64) * 2),
                    (ordinal % 5) as u32,
                    if ordinal % 2 == 0 { -1 } else { ordinal as i32 },
                )
            })
            .collect();
        let encoded = encode_vertex_entries(&entries);
        let chain = manager
            .bucket_chain(RegionKind::ForwardVertexTable)
            .expect("bucket chain should exist");
        let bucket_size = manager.bucket_size_bytes() as usize;
        let memory = TestStableMemory::default();
        let mut cursor = chain.head;
        let mut offset = 0usize;
        while !cursor.is_null() && offset < encoded.len() {
            let header = manager
                .bucket_header(cursor)
                .expect("bucket header should exist");
            let len = bucket_size.min(encoded.len() - offset);
            memory.write(header.addr.0, &encoded[offset..offset + len]);
            offset += len;
            cursor = header.next;
        }

        let entry = read_vertex_entry_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            4_096,
        )
        .expect("direct read should succeed");

        assert_eq!(entry, Some(entries[4_096]));
    }

    #[test]
    fn read_vertex_reserved_span_len_from_stable_memory_uses_next_vertex_or_extent_end() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                32,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                80,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        let extent = manager
            .region_extent(RegionKind::ForwardVertexTable)
            .expect("extent metadata should exist");
        let entries = [
            VertexEntry::new(EdgeIndex::new(3), 2, -1),
            VertexEntry::new(EdgeIndex::new(7), 1, -1),
        ];
        let memory = TestStableMemory::default();
        memory.write(extent.addr.0, &encode_vertex_entries(&entries));

        let first = read_vertex_reserved_span_len_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            0,
        )
        .expect("reserved span read should succeed");
        let second = read_vertex_reserved_span_len_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            1,
        )
        .expect("reserved span read should succeed");

        assert_eq!(first, Some(4));
        assert_eq!(second, Some(3));
    }

    #[test]
    fn read_edge_entries_by_ref_from_stable_memory_reads_contiguous_slice() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        let extent = manager
            .region_extent(RegionKind::ForwardEdgeEntries)
            .expect("extent metadata should exist");
        let entries = vec![
            EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
            EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(2, false)),
            EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(3, false)),
        ];
        let memory = TestStableMemory::default();
        memory.write(extent.addr.0, &encode_edge_entries(&entries));

        let got = read_edge_entries_by_ref_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardEdgeEntries,
            crate::low_level::EdgeRef::new(0, 1),
            2,
        )
        .expect("edge slice read should succeed");

        assert_eq!(got, entries[1..3].to_vec());
    }

    #[test]
    fn read_vertex_base_edge_ref_and_entry_from_stable_memory() {
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
                24,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        let vertex_extent = manager
            .region_extent(RegionKind::ForwardVertexTable)
            .expect("vertex extent");
        let edge_extent = manager
            .region_extent(RegionKind::ForwardEdgeEntries)
            .expect("edge extent");
        let vertex = VertexEntry::new(
            EdgeIndex::from(crate::low_level::EdgeRef::new(0, 1)),
            2,
            EMPTY_LOG_OFFSET,
        );
        let entries = vec![
            EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
            EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(2, false)),
            EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(3, false)),
        ];
        let memory = TestStableMemory::default();
        memory.write(vertex_extent.addr.0, &encode_vertex_entries(&[vertex]));
        memory.write(edge_extent.addr.0, &encode_edge_entries(&entries));

        let edge_ref = read_vertex_base_edge_ref_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            0,
            1,
        )
        .expect("edge ref read should succeed");
        let entry = read_vertex_base_entry_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            0,
            1,
        )
        .expect("base entry read should succeed");

        assert_eq!(edge_ref, Some(crate::low_level::EdgeRef::new(0, 2)));
        assert_eq!(entry, Some(entries[2]));
    }

    #[test]
    fn read_vertex_base_and_reserved_entries_from_stable_memory_use_degree_and_reserved_span() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                32,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        manager.define_extent_region(
            RegionKind::ForwardEdgeEntries,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                40,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        let vertex_extent = manager
            .region_extent(RegionKind::ForwardVertexTable)
            .expect("vertex extent metadata should exist");
        let edge_extent = manager
            .region_extent(RegionKind::ForwardEdgeEntries)
            .expect("edge extent metadata should exist");
        let vertices = [
            VertexEntry::new(EdgeIndex::new(0), 2, -1),
            VertexEntry::new(EdgeIndex::new(3), 1, -1),
        ];
        let edges = vec![
            EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
            EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(2, false)),
            EdgeEntry::new(VertexRef::from(99u8), EdgeMeta::new(9, true)),
            EdgeEntry::new(VertexRef::from(3u8), EdgeMeta::new(3, false)),
            EdgeEntry::new(VertexRef::from(4u8), EdgeMeta::new(4, false)),
        ];
        let memory = TestStableMemory::default();
        memory.write(vertex_extent.addr.0, &encode_vertex_entries(&vertices));
        memory.write(edge_extent.addr.0, &encode_edge_entries(&edges));

        let live = read_vertex_base_entries_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            0,
        )
        .expect("live base read should succeed")
        .expect("vertex should exist");
        let reserved = read_vertex_reserved_base_entries_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            0,
        )
        .expect("reserved base read should succeed")
        .expect("vertex should exist");

        assert_eq!(live, edges[0..2].to_vec());
        assert_eq!(reserved, edges[0..3].to_vec());
    }

    #[test]
    fn read_vertex_entry_from_stable_memory_returns_none_out_of_bounds() {
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
        let extent = manager
            .region_extent(RegionKind::ForwardVertexTable)
            .expect("extent metadata should exist");
        let memory = TestStableMemory::default();
        memory.write(
            extent.addr.0,
            &encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(7), 3, -1)]),
        );

        let entry = read_vertex_entry_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            1,
        )
        .expect("out of bounds should not error");

        assert_eq!(entry, None);
    }

    #[test]
    fn stable_vertex_table_reader_reads_extent_backed_vertex_table() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        manager.define_extent_region(
            RegionKind::ForwardVertexTable,
            ExtentChain::new(
                ExtentId::NULL,
                ExtentId::NULL,
                32,
                WasmPages::new(1),
                WasmPages::new(0),
            ),
        );
        let extent = manager
            .region_extent(RegionKind::ForwardVertexTable)
            .expect("extent metadata should exist");
        let entries = [
            VertexEntry::new(EdgeIndex::new(5), 1, -1),
            VertexEntry::new(EdgeIndex::new(9), 2, 3),
        ];
        let memory = TestStableMemory::default();
        memory.write(extent.addr.0, &encode_vertex_entries(&entries));
        let reader =
            StableVertexTableReader::new(&manager, &memory, RegionKind::ForwardVertexTable);

        assert_eq!(
            reader.read_vertex_entry(0).expect("read should succeed"),
            Some(entries[0])
        );
        assert_eq!(
            reader.read_vertex_entry(1).expect("read should succeed"),
            Some(entries[1])
        );
    }

    #[test]
    fn stable_vertex_table_reader_reads_bucket_backed_vertex_table_across_repeated_access() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::ForwardVertexTable,
            BucketChain::new(BucketId::NULL, BucketId::NULL, 0),
        );
        let entry_count = 4_100usize;
        let logical_len = (entry_count * 16) as u64;
        manager
            .ensure_bucket_region_capacity(RegionKind::ForwardVertexTable, logical_len)
            .expect("bucket capacity should allocate");
        let entries: Vec<_> = (0..entry_count)
            .map(|ordinal| VertexEntry::new(EdgeIndex::new(ordinal as u64), 1, ordinal as i32))
            .collect();
        let encoded = encode_vertex_entries(&entries);
        let chain = manager
            .bucket_chain(RegionKind::ForwardVertexTable)
            .expect("bucket chain should exist");
        let bucket_size = manager.bucket_size_bytes() as usize;
        let memory = TestStableMemory::default();
        let mut cursor = chain.head;
        let mut offset = 0usize;
        while !cursor.is_null() && offset < encoded.len() {
            let header = manager
                .bucket_header(cursor)
                .expect("bucket header should exist");
            let len = bucket_size.min(encoded.len() - offset);
            memory.write(header.addr.0, &encoded[offset..offset + len]);
            offset += len;
            cursor = header.next;
        }
        let reader =
            StableVertexTableReader::new(&manager, &memory, RegionKind::ForwardVertexTable);

        assert_eq!(
            reader
                .read_vertex_entry(4_095)
                .expect("read should succeed"),
            Some(entries[4_095])
        );
        assert_eq!(
            reader
                .read_vertex_entry(4_096)
                .expect("read should succeed"),
            Some(entries[4_096])
        );
        assert_eq!(
            reader
                .read_vertex_entry(4_097)
                .expect("read should succeed"),
            Some(entries[4_097])
        );
    }

    #[test]
    fn stable_vertex_table_reader_reads_vertex_ranges() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::ForwardVertexTable,
            BucketChain::new(BucketId::NULL, BucketId::NULL, 0),
        );
        let entry_count = 4_100usize;
        let logical_len = (entry_count * 16) as u64;
        manager
            .ensure_bucket_region_capacity(RegionKind::ForwardVertexTable, logical_len)
            .expect("bucket capacity should allocate");
        let entries: Vec<_> = (0..entry_count)
            .map(|ordinal| VertexEntry::new(EdgeIndex::new((ordinal * 3) as u64), 2, -1))
            .collect();
        let encoded = encode_vertex_entries(&entries);
        let chain = manager
            .bucket_chain(RegionKind::ForwardVertexTable)
            .expect("bucket chain should exist");
        let bucket_size = manager.bucket_size_bytes() as usize;
        let memory = TestStableMemory::default();
        let mut cursor = chain.head;
        let mut offset = 0usize;
        while !cursor.is_null() && offset < encoded.len() {
            let header = manager
                .bucket_header(cursor)
                .expect("bucket header should exist");
            let len = bucket_size.min(encoded.len() - offset);
            memory.write(header.addr.0, &encoded[offset..offset + len]);
            offset += len;
            cursor = header.next;
        }
        let reader =
            StableVertexTableReader::new(&manager, &memory, RegionKind::ForwardVertexTable);

        let actual = reader
            .read_vertex_entries(4_094, 4)
            .expect("range read should succeed");

        assert_eq!(actual, entries[4_094..4_098].to_vec());
    }

    #[test]
    fn summarize_vertex_window_from_stable_memory_summarizes_bucket_backed_window() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::ForwardVertexTable,
            BucketChain::new(BucketId::NULL, BucketId::NULL, 0),
        );
        let entries = vec![
            VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET),
            VertexEntry::new(EdgeIndex::new(4), 1, 7),
            VertexEntry::new(EdgeIndex::new(7), 3, EMPTY_LOG_OFFSET),
        ];
        let logical_len = (entries.len() * 16) as u64;
        manager
            .ensure_bucket_region_capacity(RegionKind::ForwardVertexTable, logical_len)
            .expect("bucket capacity should allocate");
        let encoded = encode_vertex_entries(&entries);
        let chain = manager
            .bucket_chain(RegionKind::ForwardVertexTable)
            .expect("bucket chain should exist");
        let head = manager
            .bucket_header(chain.head)
            .expect("bucket header should exist");
        let memory = TestStableMemory::default();
        memory.write(head.addr.0, &encoded);

        let summary = summarize_vertex_window_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            0,
            3,
        )
        .expect("window summary should succeed")
        .expect("window should exist");

        assert_eq!(summary.start_ordinal, 0);
        assert_eq!(summary.end_ordinal_exclusive, 3);
        assert_eq!(summary.base_start, EdgeIndex::new(0));
        assert_eq!(summary.live_end_exclusive, EdgeIndex::new(10));
        assert_eq!(summary.total_live_degree, 6);
        assert_eq!(summary.max_live_degree, 3);
        assert_eq!(summary.vertices_with_overflow, 1);
    }

    #[test]
    fn estimate_vertex_window_reserve_hint_from_stable_memory_uses_insert_policy() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::ForwardVertexTable,
            BucketChain::new(BucketId::NULL, BucketId::NULL, 0),
        );
        let entries = vec![
            VertexEntry::new(EdgeIndex::new(0), 2, EMPTY_LOG_OFFSET),
            VertexEntry::new(EdgeIndex::new(4), 1, 7),
            VertexEntry::new(EdgeIndex::new(7), 3, EMPTY_LOG_OFFSET),
        ];
        let logical_len = (entries.len() * 16) as u64;
        manager
            .ensure_bucket_region_capacity(RegionKind::ForwardVertexTable, logical_len)
            .expect("bucket capacity should allocate");
        let encoded = encode_vertex_entries(&entries);
        let chain = manager
            .bucket_chain(RegionKind::ForwardVertexTable)
            .expect("bucket chain should exist");
        let head = manager
            .bucket_header(chain.head)
            .expect("bucket header should exist");
        let memory = TestStableMemory::default();
        memory.write(head.addr.0, &encoded);

        let hint = estimate_vertex_window_reserve_hint_from_stable_memory(
            &manager,
            &memory,
            RegionKind::ForwardVertexTable,
            0,
            3,
            (GraphInsertPolicy::default(), 5),
            1,
        )
        .expect("reserve hint should succeed")
        .expect("reserve hint should exist");

        assert_eq!(hint.live_span_len_lower_bound, 10);
        assert_eq!(hint.target_base_len_lower_bound, 7);
        assert_eq!(hint.extra_slots_for_anchor_degree, 2);
        assert_eq!(hint.preferred_reserved_base_len_lower_bound, 9);
        assert_eq!(hint.total_weight, 9);
        assert_eq!(hint.vertices_with_overflow, 1);
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
                encode_edge_entries(&[EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(1, false),
                )]),
            ),
            (RegionKind::ForwardLabelIndex, Vec::new()),
            (RegionKind::ForwardSegmentLog, Vec::new()),
            (
                RegionKind::ReverseVertexTable,
                encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
            ),
            (
                RegionKind::ReverseEdgeEntries,
                encode_edge_entries(&[EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(2, false),
                )]),
            ),
            (RegionKind::ReverseLabelIndex, Vec::new()),
            (RegionKind::ReverseSegmentLog, Vec::new()),
        ] {
            let extent = manager.region_extent(kind).expect("extent should exist");
            memory.write(extent.addr.0, &bytes);
        }

        let runtimes = hydrate_surface_runtimes_from_stable_memory(&manager, &memory)
            .expect("stable-memory hydration should succeed");

        assert_eq!(
            u64::from(
                runtimes
                    .forward
                    .0
                    .base_entries
                    .get(0)
                    .copied()
                    .expect("forward base entry 0")
                    .target
            ),
            1
        );
        assert_eq!(
            u64::from(
                runtimes
                    .reverse
                    .0
                    .base_entries
                    .get(0)
                    .copied()
                    .expect("reverse base entry 0")
                    .target
            ),
            2
        );
    }

    #[test]
    fn hydrate_surface_runtimes_from_stable_memory_syncs_explicit_segment_capacities() {
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
        manager
            .register_edge_segment(
                RegionKind::ForwardEdgeEntries,
                EdgeSegmentHeader::new(7, ExtentId::new(70), 11, 0, EdgeSegmentState::Active),
            )
            .expect("forward explicit segment should register");
        manager
            .register_edge_segment(
                RegionKind::ReverseEdgeEntries,
                EdgeSegmentHeader::new(9, ExtentId::new(90), 13, 0, EdgeSegmentState::Active),
            )
            .expect("reverse explicit segment should register");

        let memory = TestStableMemory::default();
        for (kind, bytes) in [
            (
                RegionKind::ForwardVertexTable,
                encode_vertex_entries(&[VertexEntry::new(EdgeRef::new(7, 0).into(), 1, -1)]),
            ),
            (
                RegionKind::ForwardEdgeEntries,
                encode_edge_entries(&[EdgeEntry::new(
                    VertexRef::from(1u8),
                    EdgeMeta::new(1, false),
                )]),
            ),
            (RegionKind::ForwardLabelIndex, Vec::new()),
            (RegionKind::ForwardSegmentLog, Vec::new()),
            (
                RegionKind::ReverseVertexTable,
                encode_vertex_entries(&[VertexEntry::new(EdgeRef::new(9, 0).into(), 1, -1)]),
            ),
            (
                RegionKind::ReverseEdgeEntries,
                encode_edge_entries(&[EdgeEntry::new(
                    VertexRef::from(2u8),
                    EdgeMeta::new(2, false),
                )]),
            ),
            (RegionKind::ReverseLabelIndex, Vec::new()),
            (RegionKind::ReverseSegmentLog, Vec::new()),
        ] {
            let extent = manager.region_extent(kind).expect("extent should exist");
            memory.write(extent.addr.0, &bytes);
        }

        let runtimes = hydrate_surface_runtimes_from_stable_memory(&manager, &memory)
            .expect("stable-memory hydration should succeed");

        assert_eq!(runtimes.forward.0.base_segment_slot_capacity(0), Some(1));
        assert_eq!(runtimes.reverse.0.base_segment_slot_capacity(0), Some(1));
        assert_eq!(runtimes.forward.0.base_segment_slot_capacity(7), Some(11));
        assert_eq!(runtimes.reverse.0.base_segment_slot_capacity(9), Some(13));
    }

    #[test]
    fn hydrate_surface_runtimes_from_stable_memory_reads_explicit_segment_entries() {
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
        let forward_segment = manager
            .allocate_edge_segment(RegionKind::ForwardEdgeEntries, 1, EdgeSegmentState::Active)
            .expect("forward segment should allocate");
        let reverse_segment = manager
            .allocate_edge_segment(RegionKind::ReverseEdgeEntries, 1, EdgeSegmentState::Active)
            .expect("reverse segment should allocate");

        let memory = TestStableMemory::default();
        for (kind, bytes) in [
            (
                RegionKind::ForwardVertexTable,
                encode_vertex_entries(&[VertexEntry::new(
                    EdgeRef::new(forward_segment.segment_id, 0).into(),
                    1,
                    -1,
                )]),
            ),
            (RegionKind::ForwardEdgeEntries, Vec::new()),
            (RegionKind::ForwardLabelIndex, Vec::new()),
            (RegionKind::ForwardSegmentLog, Vec::new()),
            (
                RegionKind::ReverseVertexTable,
                encode_vertex_entries(&[VertexEntry::new(
                    EdgeRef::new(reverse_segment.segment_id, 0).into(),
                    1,
                    -1,
                )]),
            ),
            (RegionKind::ReverseEdgeEntries, Vec::new()),
            (RegionKind::ReverseLabelIndex, Vec::new()),
            (RegionKind::ReverseSegmentLog, Vec::new()),
        ] {
            let extent = manager.region_extent(kind).expect("extent should exist");
            memory.write(extent.addr.0, &bytes);
        }
        let (_, forward_extent) = manager
            .resolve_edge_ref(
                RegionKind::ForwardEdgeEntries,
                EdgeRef::new(forward_segment.segment_id, 0),
            )
            .expect("forward extent should resolve");
        memory.write(
            forward_extent.addr.0,
            &encode_edge_entries(&[EdgeEntry::new(
                VertexRef::from(33u8),
                EdgeMeta::new(1, false),
            )]),
        );
        let (_, reverse_extent) = manager
            .resolve_edge_ref(
                RegionKind::ReverseEdgeEntries,
                EdgeRef::new(reverse_segment.segment_id, 0),
            )
            .expect("reverse extent should resolve");
        memory.write(
            reverse_extent.addr.0,
            &encode_edge_entries(&[EdgeEntry::new(
                VertexRef::from(44u8),
                EdgeMeta::new(2, false),
            )]),
        );

        let runtimes = hydrate_surface_runtimes_from_stable_memory(&manager, &memory)
            .expect("stable-memory hydration should succeed");

        let forward_target = runtimes
            .forward
            .0
            .base_entries
            .get_by_ref(EdgeRef::new(forward_segment.segment_id, 0))
            .expect("forward explicit entry should be readable")
            .target;
        let reverse_target = runtimes
            .reverse
            .0
            .base_entries
            .get_by_ref(EdgeRef::new(reverse_segment.segment_id, 0))
            .expect("reverse explicit entry should be readable")
            .target;
        assert_eq!(forward_target, VertexRef::from(33u8));
        assert_eq!(reverse_target, VertexRef::from(44u8));
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
                encode_edge_entries(&[EdgeEntry::new(
                    VertexRef::from(10u8),
                    EdgeMeta::new(1, false),
                )]),
            ),
            (RegionKind::ForwardLabelIndex, Vec::new()),
            (RegionKind::ForwardSegmentLog, Vec::new()),
            (
                RegionKind::ReverseVertexTable,
                encode_vertex_entries(&[VertexEntry::new(EdgeIndex::new(0), 1, -1)]),
            ),
            (
                RegionKind::ReverseEdgeEntries,
                encode_edge_entries(&[EdgeEntry::new(
                    VertexRef::from(20u8),
                    EdgeMeta::new(2, false),
                )]),
            ),
            (RegionKind::ReverseLabelIndex, Vec::new()),
            (RegionKind::ReverseSegmentLog, Vec::new()),
        ] {
            let extent = manager.region_extent(kind).expect("extent should exist");
            memory.write(extent.addr.0, &bytes);
        }

        let runtimes = hydrate_surface_runtimes_from_stable_memory(&manager, &memory)
            .expect("vector memory hydration should succeed");

        assert_eq!(
            u64::from(
                runtimes
                    .forward
                    .0
                    .base_entries
                    .get(0)
                    .copied()
                    .expect("forward base entry 0")
                    .target
            ),
            10
        );
        assert_eq!(
            u64::from(
                runtimes
                    .reverse
                    .0
                    .base_entries
                    .get(0)
                    .copied()
                    .expect("reverse base entry 0")
                    .target
            ),
            20
        );
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(1, false)),
            ],
            vec![OverflowEntry::new(
                99,
                EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(3, false)),
                LogOffset::EMPTY,
            )],
            Vec::new(),
            Vec::new(),
        );
        let memory = VecMemory::default();

        write_surface_runtime_to_stable_memory(&mut manager, &memory, &runtime)
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
        assert_same_base_edge_payload(&hydrated.base_entries, &runtime.base_entries);
        assert_eq!(hydrated.overflow_entries, runtime.overflow_entries);
    }

    #[test]
    fn write_surface_runtime_to_stable_memory_round_trips_with_bucket_backed_vertex_table() {
        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::new(1));
        manager.define_bucket_region(
            RegionKind::ForwardVertexTable,
            BucketChain::new(BucketId::NULL, BucketId::NULL, 0),
        );
        manager
            .set_region_logical_len(RegionKind::ForwardVertexTable, 16)
            .expect("vertex logical length should update");
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(1, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(1, false)),
            ],
            vec![OverflowEntry::new(
                99,
                EdgeEntry::new(VertexRef::from(9u8), EdgeMeta::new(3, false)),
                LogOffset::EMPTY,
            )],
            Vec::new(),
            Vec::new(),
        );
        let memory = VecMemory::default();

        write_surface_runtime_to_stable_memory(&mut manager, &memory, &runtime)
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
        assert_same_base_edge_payload(&hydrated.base_entries, &runtime.base_entries);
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
                EdgeEntry::new(VertexRef::from(1u8), EdgeMeta::new(3, false)),
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(4, false)),
            ],
            Vec::new(),
            vec![VertexLabelIndexEntry::new(0, 2)],
            vec![
                VertexLabelRange::new(3, 0, 1),
                VertexLabelRange::new(4, 1, 1),
            ],
        );
        let memory = VecMemory::default();

        write_surface_runtime_to_stable_memory(&mut manager, &memory, &runtime)
            .expect("initial writeback should succeed");

        runtime
            .replace_base_entry(
                0,
                1,
                EdgeEntry::new(VertexRef::from(2u8), EdgeMeta::new(3, false)),
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

        assert_eq!(
            hydrated
                .base_entries
                .get(1)
                .expect("base entry 1")
                .meta
                .local_id(),
            Some(3)
        );
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
                vec![EdgeEntry::new(
                    VertexRef::from(10u8),
                    EdgeMeta::new(1, false),
                )],
                Vec::new(),
            ),
            super::ReverseSurfaceRuntime::new(
                reverse_surface_from_layout(&manager.layout).expect("reverse layout should exist"),
                vec![VertexEntry::new(EdgeIndex::new(0), 1, -1)],
                vec![EdgeEntry::new(
                    VertexRef::from(20u8),
                    EdgeMeta::new(2, false),
                )],
                Vec::new(),
            ),
        );
        let memory = VecMemory::default();

        write_surface_runtimes_to_stable_memory(&mut manager, &memory, &runtimes)
            .expect("pair writeback should succeed");
        let hydrated = hydrate_surface_runtimes_from_stable_memory(&manager, &memory)
            .expect("hydration should succeed");

        assert_eq!(
            u64::from(
                hydrated
                    .forward
                    .0
                    .base_entries
                    .get(0)
                    .copied()
                    .expect("forward base entry 0")
                    .target
            ),
            10
        );
        assert_eq!(
            u64::from(
                hydrated
                    .reverse
                    .0
                    .base_entries
                    .get(0)
                    .copied()
                    .expect("reverse base entry 0")
                    .target
            ),
            20
        );
    }

    #[test]
    fn write_surface_runtime_to_stable_memory_flushes_explicit_segments() {
        use std::collections::BTreeMap;

        let mut manager = RegionManager::with_bucket_size(BucketSizeInPages::DEFAULT);
        for (kind, logical_len) in [
            (RegionKind::ForwardVertexTable, 16_u64),
            (RegionKind::ForwardEdgeEntries, 8_u64),
            (RegionKind::ForwardLabelIndex, 16_u64),
            (RegionKind::ForwardSegmentLog, 0_u64),
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
        let explicit = manager
            .allocate_edge_segment(RegionKind::ForwardEdgeEntries, 1, EdgeSegmentState::Active)
            .expect("explicit segment should allocate");
        let mut runtime = super::SurfaceRuntime::new(
            forward_surface_from_layout(&manager.layout)
                .expect("forward layout should exist")
                .layout(),
            vec![VertexEntry::new(
                EdgeRef::new(explicit.segment_id, 0).into(),
                1,
                EMPTY_LOG_OFFSET,
            )],
            vec![],
            Vec::new(),
            vec![VertexLabelIndexEntry::new(0, 0)],
            Vec::new(),
        );
        runtime.set_base_storage(
            super::SurfaceBaseStorage::from_segmented_with_slot_capacities(
                BTreeMap::from([
                    (0, Vec::new()),
                    (
                        explicit.segment_id,
                        vec![EdgeEntry::new(
                            VertexRef::from(77u8),
                            EdgeMeta::new(9, false),
                        )],
                    ),
                ]),
                BTreeMap::from([(0, 0_u64), (explicit.segment_id, explicit.slot_capacity)]),
            ),
        );

        let memory = VecMemory::default();
        write_surface_runtime_to_stable_memory(&mut manager, &memory, &runtime)
            .expect("segmented writeback should succeed");
        let (_, explicit_extent) = manager
            .resolve_edge_ref(
                RegionKind::ForwardEdgeEntries,
                EdgeRef::new(explicit.segment_id, 0),
            )
            .expect("explicit segment extent should resolve");
        let mut bytes = [0u8; 8];
        memory.read(explicit_extent.addr.0, &mut bytes);
        let decoded = decode_edge_entries(RegionKind::ForwardEdgeEntries, &bytes)
            .expect("explicit segment payload should decode");
        assert_eq!(u64::from(decoded[0].target), 77);
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
            vec![EdgeEntry::new(
                VertexRef::from(1u8),
                EdgeMeta::new(1, false),
            )],
            Vec::new(),
        );
        let memory = VecMemory::default();

        let err = write_forward_surface_runtime_to_stable_memory(&mut manager, &memory, &runtime)
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
