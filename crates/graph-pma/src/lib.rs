#![doc = include_str!("../../../docs/graph-pma-target-design.md")]
//!
//! This crate is the rewrite entrypoint for `graph-pma`.
//! The previous implementation was moved to `gleaph-graph-pma-legacy` and is
//! temporarily re-exported here as a compatibility bridge while the new
//! low-level-first implementation is rebuilt from the target design document.

mod facade;
mod integration;
pub mod low_level;
mod observability;
mod property_index;
mod property_store;
mod stable;

pub use facade::{
    RewriteAppendVertexWriteSummary, RewriteAppendVerticesWriteSummary,
    RewriteBootstrapEdgeProjection, RewriteBootstrapEdgeWriteSummary,
    RewriteBootstrapGraphProjection, RewriteBootstrapGraphWriteSummary,
    RewriteBootstrapVerticesProjection, RewriteEdgeLocatorMapping, RewriteEdgeWriteOperation,
    RewriteEdgeWriteProjection, RewriteEnsureCapacityProjection, RewriteFacadeWriteEvent,
    RewriteGraphMutationWriteSummary, RewriteGraphPma, RewriteGraphPmaBatchSession,
    RewriteGraphPmaError, RewriteGraphPmaResult, RewriteGraphService, RewriteGraphStore,
    RewriteGraphStoreAdapter, RewriteInsertEdgeProjection, RewriteNodeDeleteProjection,
    RewritePropertyIndexMutationSummary, RewritePropertyIndexTouchedSections,
    RewritePropertyMutationWriteSummary, RewritePropertyWriteProjection, RewriteRefreshedVertices,
    RewriteVertexOrdinalMapping, RewriteWriteEventProjection,
};
pub use integration::{
    bootstrap_graph, bootstrap_kernel_overlay_graph, BootstrapEdgeSpec, BootstrapGraphSpec,
    KernelBootstrapEdgeSpec, KernelBootstrapGraphSpec, KernelBootstrapGraphSummary,
    KernelBootstrapNodeSpec, RewriteGraphPmaKernelHarness, RewriteGraphPmaKernelOverlay,
    RewriteKernelBootstrapBridge, RewriteKernelOverlayGraph, RewriteKernelOverlayObservability,
    RewriteOverlayBootstrapGraphSummary, RewriteOverlayEdgeBootstrapSummary,
    RewriteOverlayEdgeMutationKind, RewriteOverlayEdgeWriteSummary,
    RewriteOverlayInsertEdgeSummary, RewriteOverlayNodeBootstrapSummary,
    RewriteOverlayNodeDeleteSummary, RewriteOverlayWriteEvent,
};
pub use observability::{
    format_last_write_event, format_write_event_history, format_write_event_projection,
    format_write_event_report, last_projected_facade_event, last_projected_overlay_event,
    project_facade_write_event, project_facade_write_history, project_overlay_write_event,
    project_overlay_write_history, RewriteDiagnosticsView,
};
pub use property_index::{
    read_edge_property_index_node_record_from_stable_memory,
    read_edge_property_index_paged_area_from_stable_memory,
    read_node_property_index_node_record_from_stable_memory,
    read_node_property_index_paged_area_from_stable_memory,
    read_property_index_region_header_from_stable_memory,
    read_property_index_snapshot_from_stable_memory,
    read_property_index_snapshot_section_from_stable_memory,
    read_property_index_storage_image_from_stable_memory,
    scan_edge_property_index_property_prefix_from_stable_memory,
    scan_edge_property_index_value_prefix_from_stable_memory,
    scan_node_property_index_property_prefix_from_stable_memory,
    scan_node_property_index_value_prefix_from_stable_memory,
    write_property_index_snapshot_to_stable_memory,
    write_property_index_storage_image_to_stable_memory, PropertyIndex,
    PropertyIndexAllocatorHeader, PropertyIndexEntityKind, PropertyIndexEntry, PropertyIndexError,
    PropertyIndexHeader, PropertyIndexKey, PropertyIndexLeafChainShapeError,
    PropertyIndexNodeHeader, PropertyIndexNodeId, PropertyIndexNodeKind, PropertyIndexNodeRecord,
    PropertyIndexNodeStore, PropertyIndexNodeStoreDelta, PropertyIndexNodeStoreMutationKind,
    PropertyIndexRegionHeader, PropertyIndexSnapshot, PropertyIndexStorageImage,
};
pub use property_store::{
    BlobPropertyAppendLog, GraphPropertyAppendLog, PropertyAppendLog, PropertyEntityKind,
    PropertyKey, PropertyRecord, PropertyRecordHeader, PropertyStoreError, PropertyValueBlob,
};
pub use stable::{
    Bound as RewriteBound, Memory as RewriteMemory, Storable as RewriteStorable,
    VecMemory as RewriteVecMemory,
};

pub use gleaph_graph_pma_legacy::*;
pub use low_level::{
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
    BaseNeighborhood, BucketChain, BucketHeader, BucketId, BucketRef, BucketSizeInPages,
    BucketTable, EdgeEntry, EdgeIndex, EdgeInsertPath, EdgeLocator, EdgeLocatorSidecar, EdgeMeta,
    ExtentChain, ExtentGrowthDecision, ExtentGrowthKind, ExtentGrowthPolicy, ExtentGrowthRequest,
    ExtentHeader, ExtentId, ExtentRef, ExtentTable, ForwardSurface, ForwardSurfaceRuntime,
    FreeBucketList, FreeExtentList, GraphAppliedRebalanceSummary,
    GraphAppliedRebalanceWriteSummary, GraphBatchMutationSession, GraphEnsureCapacityWriteSummary,
    GraphInsertDecision, GraphInsertPolicy, GraphInsertResult, GraphInsertWriteSummary,
    GraphLocalRebalanceDelta, GraphLocalRebalancePlan, GraphMutationPath, GraphRebalancePlan,
    GraphRuntime, HydratedSurfaceRuntimes, HydrationError, InMemoryRegionByteSource, LogOffset,
    MergedNeighborhoodView, OverflowChain, OverflowEntry, RegionByteSource, RegionDirectory,
    RegionDirectoryEntry, RegionKind, RegionManager, RegionManagerLayout, RegionRef,
    RegionStorageKind, ResolvedEdgeSlot, ReverseSurface, ReverseSurfaceRuntime, StableAddr,
    StableMemoryRegionByteSource, SurfaceAppliedRebalanceSummary, SurfaceDirtyRegions, SurfaceKind,
    SurfaceLayout, SurfaceLocalRebalanceDelta, SurfaceRebalancePlan, SurfaceRebalanceWindowPlan,
    SurfaceRegionKinds, SurfaceRegions, SurfaceRuntime, SurfaceWeightedWindowLayout,
    SurfaceWindowSlackSummary, VertexEntry, VertexLabelIndexEntry, VertexLabelRange, WasmPages,
    WritebackError, EMPTY_LOG_OFFSET, LABEL_ID_MASK, MAX_REGION_KINDS, TOMBSTONE_MASK,
    WASM_PAGE_SIZE,
};
