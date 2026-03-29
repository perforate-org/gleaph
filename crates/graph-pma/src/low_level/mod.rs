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

pub use edge::{EdgeEntry, EdgeLocator, EdgeMeta, SurfaceKind, SurfaceRegions};
pub use extent::{
    BucketChain, BucketHeader, BucketId, BucketRef, BucketTable, ExtentChain, ExtentGrowthDecision,
    ExtentGrowthKind, ExtentGrowthPolicy, ExtentGrowthRequest, ExtentHeader, ExtentId, ExtentRef,
    ExtentTable, FreeBucketList, FreeExtentList,
};
pub use graph::{
    GraphAppliedRebalanceSummary, GraphAppliedRebalanceWriteSummary, GraphBatchMutationSession,
    GraphEnsureCapacityWriteSummary, GraphInsertDecision, GraphInsertPolicy, GraphInsertResult,
    GraphInsertWriteSummary, GraphLocalRebalanceDelta, GraphLocalRebalancePlan, GraphMutationPath,
    GraphRebalancePlan, GraphRuntime, SurfaceRebalancePlan, SurfaceRebalanceWindowPlan,
};
pub use hydration::{
    decode_edge_entries, decode_label_index_region, decode_overflow_entries, decode_vertex_entries,
    encode_edge_entries, encode_label_index_region, encode_overflow_entries, encode_vertex_entries,
    forward_surface_from_layout, hydrate_forward_surface_runtime, hydrate_reverse_surface_runtime,
    hydrate_surface_runtime, hydrate_surface_runtimes_from_layout,
    hydrate_surface_runtimes_from_stable_memory, reverse_surface_from_layout,
    write_dirty_forward_surface_runtime_to_stable_memory,
    write_dirty_reverse_surface_runtime_to_stable_memory,
    write_dirty_surface_runtime_to_stable_memory, write_dirty_surface_runtimes_to_stable_memory,
    write_forward_surface_runtime_to_stable_memory, write_reverse_surface_runtime_to_stable_memory,
    write_surface_runtime_to_stable_memory, write_surface_runtimes_to_stable_memory,
    HydratedSurfaceRuntimes, HydrationError, InMemoryRegionByteSource, RegionByteSource,
    StableMemoryRegionByteSource, WritebackError,
};
pub use ids::StableAddr;
pub use locator::EdgeLocatorSidecar;
pub use manager::RegionManager;
pub use overflow::{LogOffset, OverflowChain, OverflowEntry};
pub use region::{
    BucketSizeInPages, RegionDirectory, RegionDirectoryEntry, RegionKind, RegionManagerLayout,
    RegionRef, RegionStorageKind, WasmPages, MAX_REGION_KINDS, WASM_PAGE_SIZE,
};
pub use runtime::{
    EdgeInsertPath, ForwardSurfaceRuntime, ResolvedEdgeSlot, ReverseSurfaceRuntime,
    SurfaceAppliedRebalanceSummary, SurfaceDirtyRegions, SurfaceLocalRebalanceDelta,
    SurfaceRuntime, SurfaceWeightedWindowLayout, SurfaceWindowSlackSummary,
};
pub use surface::{
    BaseNeighborhood, ForwardSurface, MergedNeighborhoodView, ReverseSurface, SurfaceLayout,
    SurfaceRegionKinds,
};
pub use vertex::{
    EdgeIndex, VertexEntry, VertexLabelIndexEntry, VertexLabelRange, EMPTY_LOG_OFFSET,
    LABEL_ID_MASK, TOMBSTONE_MASK,
};
