#![doc = include_str!("../../../docs/graph-store-target-design.md")]
//!
//! Primary graph persistence and adjacency implementation for `graph-store`.

pub(crate) mod bench_profile;
pub(crate) mod byte_compare;
pub(crate) mod canbench_scope;
pub mod adjacency;
pub mod facade;
pub mod integration;
pub mod low_level;
pub mod observability;
pub mod property_index;
pub mod property_store;
pub mod maintenance_dirty;
pub(crate) use low_level::{GraphInsertDecision, GraphInsertResult, ResolvedEdgeSlot};
#[cfg(any(test, doctest))]
pub(crate) use property_index::PropertyIndexNodeId;
pub(crate) use property_store::PropertyEntityKind;

// Convenience aliases used by callers and tests.
pub use adjacency::{
    GraphAdjacency, GraphAdjacencyBackend, GraphStoreMemorySlots, GraphStoreSlotError, RcGraphMemory,
    GRAPH_STORE_FIXED_MEMORY_IDS,
    GRAPH_STORE_MEMORY_ID_ADJACENCY_GC_QUEUE,
    GRAPH_STORE_MEMORY_ID_DELETED_VERTICES, GRAPH_STORE_MEMORY_ID_EDGE_PROPERTY_STORE,
    GRAPH_STORE_MEMORY_ID_FORWARD_EDGES_AND_LOG,
    GRAPH_STORE_MEMORY_ID_FORWARD_SEGMENT_EDGE_COUNTS,
    GRAPH_STORE_MEMORY_ID_FORWARD_VERTEX_TABLE, GRAPH_STORE_MEMORY_ID_GC_STATE,
    graph_store_fixed_memory_ids,
    GRAPH_STORE_MEMORY_ID_LABEL_CATALOG, GRAPH_STORE_MEMORY_ID_MAINTENANCE_DIRTY_ORDINALS,
    GRAPH_STORE_MEMORY_ID_MAINTENANCE_QUEUE,
    GRAPH_STORE_MEMORY_ID_NODE_PROPERTY_STORE,
    GRAPH_STORE_MEMORY_ID_PROPERTY_INDEX, GRAPH_STORE_MEMORY_ID_REVERSE_EDGES_AND_LOG,
    GRAPH_STORE_MEMORY_ID_REVERSE_SEGMENT_EDGE_COUNTS,
    GRAPH_STORE_MEMORY_ID_REVERSE_VERTEX_TABLE, GRAPH_STORE_MEMORY_ID_SHARD_CANISTER_DIRECTORY,
};
pub use facade::{GraphStore, GraphStoreError, GraphStoreResult};
pub type VecMemory = ic_stable_structures::VectorMemory;
pub type GraphStoreVecMemory = ic_stable_structures::VectorMemory;
