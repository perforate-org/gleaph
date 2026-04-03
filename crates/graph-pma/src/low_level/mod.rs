//! Low-level building blocks for the `graph-pma` rewrite.
//!
//! This module tree is intentionally organized from physical layout upward:
//!
//! - ids / region / extent: stable-memory addressing and allocator metadata
//! - edge / vertex / overflow / surface: adjacency-kernel layout vocabulary
//! - runtime / locator / graph: in-memory coordination for read/write paths
//! - hydration: byte-format adapters between runtime state and stable memory

mod edge;
mod extent;
mod graph;
mod hydration;
mod ids;
mod locator;
mod manager;
mod overflow;
mod region;
mod runtime;
mod surface;
mod vertex;

pub use edge::{EdgeEntry, EdgeMeta, LogicalEdgeLocator, SurfaceKind, SurfaceRegions};
pub use extent::{
    BucketChain, BucketHeader, BucketId, BucketRef, BucketTable, EdgeSegmentDirectory,
    EdgeSegmentHeader, EdgeSegmentState, ExtentChain, ExtentGrowthDecision, ExtentGrowthKind,
    ExtentGrowthPolicy, ExtentGrowthRequest, ExtentHeader, ExtentId, ExtentRef, ExtentTable,
    FreeBucketList, FreeExtentList,
};
pub use graph::{
    EdgePairEndpoints, EdgePairLogicalLocators, EdgeReplaceSpec, EdgeTombstoneSpec,
    GraphAppliedRebalanceSummary, GraphAppliedRebalanceWriteSummary, GraphBatchMutationSession,
    GraphEnsureCapacityWriteSummary, GraphInsertDecision, GraphInsertPolicy, GraphInsertResult,
    GraphInsertWriteSummary, GraphLocalRebalanceDelta, GraphLocalRebalancePlan, GraphMutationPath,
    GraphRebalancePlan, GraphRuntime, RebalanceInsertSpec, RebalancePrepareSpec,
    SurfaceRebalancePlan, SurfaceRebalanceWindowPlan, SurfaceVertexWindowReserveHint,
};
pub use graph::{
    GraphAppliedSegmentRebalanceSummary, GraphAppliedSegmentRebalanceWriteSummary,
    GraphEnsureCapacitySegmentSummary, GraphEnsureCapacitySegmentWriteSummary,
    GraphInsertSegmentSummary, GraphInsertSegmentWriteSummary, GraphMaintenanceBatchWriteSummary,
    GraphMaintenanceCandidate, GraphMaintenanceCyclePlan, GraphMaintenanceCycleWriteSummary,
    GraphMaintenanceQueueStorageSnapshot, GraphMaintenanceWorkItem,
};
pub use hydration::{
    HydratedSurfaceRuntimes, HydrationError, InMemoryRegionByteSource, RegionByteSource,
    StableMemoryRegionByteSource, StableVertexTableReader, WritebackError, decode_edge_entries,
    decode_label_index_region, decode_overflow_entries, decode_vertex_entries, encode_edge_entries,
    encode_label_index_region, encode_overflow_entries, encode_vertex_entries,
    estimate_vertex_window_reserve_hint_from_stable_memory, forward_surface_from_layout,
    hydrate_forward_surface_runtime, hydrate_reverse_surface_runtime, hydrate_surface_runtime,
    hydrate_surface_runtimes_from_layout, hydrate_surface_runtimes_from_stable_memory,
    read_edge_entries_by_ref_from_stable_memory, read_vertex_base_edge_ref_from_stable_memory,
    read_vertex_base_entries_from_stable_memory, read_vertex_base_entry_from_stable_memory,
    read_vertex_entries_from_stable_memory, read_vertex_entry_by_ref_from_stable_memory,
    read_vertex_entry_from_stable_memory, read_vertex_reserved_base_entries_from_stable_memory,
    read_vertex_reserved_span_len_from_stable_memory, reverse_surface_from_layout,
    summarize_vertex_window_from_stable_memory,
    write_dirty_forward_surface_runtime_to_stable_memory,
    write_dirty_reverse_surface_runtime_to_stable_memory,
    write_dirty_surface_runtime_to_stable_memory, write_dirty_surface_runtimes_to_stable_memory,
    write_forward_surface_runtime_to_stable_memory, write_reverse_surface_runtime_to_stable_memory,
    write_surface_runtime_to_stable_memory, write_surface_runtimes_to_stable_memory,
};
pub use ids::{EdgeRef, StableAddr, VertexRef};
pub use locator::EdgeLogicalLocatorSidecar;
pub use manager::RegionManager;
pub use overflow::{LogOffset, OverflowChain, OverflowEntry};
pub use region::{
    BucketSizeInPages, MAX_REGION_KINDS, RegionDirectory, RegionDirectoryEntry, RegionKind,
    RegionManagerLayout, RegionRef, RegionStorageKind, WASM_PAGE_SIZE, WasmPages,
};
pub use runtime::{
    EdgeInsertPath, ForwardSurfaceRuntime, ResolvedEdgeSlot, ReverseSurfaceRuntime,
    SurfaceAppliedRebalanceSummary, SurfaceBaseStorage, SurfaceDirtyRegions,
    SurfaceLocalRebalanceDelta, SurfaceRuntime, SurfaceVertexWindowSummary,
    SurfaceWeightedWindowLayout, SurfaceWindowSlackSummary,
};
pub use surface::{
    BaseNeighborhood, ForwardSurface, MergedNeighborhoodView, ReverseSurface, SurfaceLayout,
    SurfaceRegionKinds,
};
pub use vertex::{
    EMPTY_LOG_OFFSET, EdgeIndex, LABEL_ID_MASK, TOMBSTONE_MASK, VertexEntry, VertexLabelIndexEntry,
    VertexLabelRange,
};
