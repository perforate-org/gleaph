//! Graph canister stable-memory layout — see `design/storage/stable-memory-inventory.md`
//! and `facade/stable/layout.rs` (ADR 0007 registry).

use super::edge_alias::EdgeAliasIndex;
use super::edge_payload_profiles::EdgePayloadProfileStore;
use super::edge_properties::EdgePropertyStore;
use super::metadata::{GraphMetadata, StableGraphMetadata};
use super::vertex_labels::VertexLabelStore;
use super::vertex_properties::VertexPropertyStore;
use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledLaraGraph,
    lara::maintenance::DeferredConfig,
};
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{DefaultMemoryImpl, StableCell};
use std::cell::RefCell;

// --- Labeled graph: forward orientation (10 memories) ---
const FWD_VERTICES: MemoryId = MemoryId::new(0);
const FWD_BUCKETS: MemoryId = MemoryId::new(1);
const FWD_BUCKET_FREE_SPANS: MemoryId = MemoryId::new(2);
const FWD_BUCKET_FREE_SPAN_BY_START: MemoryId = MemoryId::new(3);
const FWD_EDGE_COUNTS: MemoryId = MemoryId::new(4);
const FWD_EDGES: MemoryId = MemoryId::new(5);
const FWD_EDGE_LOG: MemoryId = MemoryId::new(6);
const FWD_EDGE_SPAN_META: MemoryId = MemoryId::new(7);
const FWD_EDGE_FREE_SPANS: MemoryId = MemoryId::new(8);
const FWD_EDGE_FREE_SPAN_BY_START: MemoryId = MemoryId::new(9);

// --- Labeled graph: forward payload (5 memories) ---
const FWD_PAYLOAD_SLAB: MemoryId = MemoryId::new(10);
const FWD_PAYLOAD_FREE_SPANS: MemoryId = MemoryId::new(11);
const FWD_PAYLOAD_FREE_SPAN_BY_START: MemoryId = MemoryId::new(12);
const FWD_PAYLOAD_LOG: MemoryId = MemoryId::new(13);
const FWD_PAYLOAD_BLOBS: MemoryId = MemoryId::new(14);

// --- Labeled graph: reverse orientation (10 memories) ---
const REV_VERTICES: MemoryId = MemoryId::new(15);
const REV_BUCKETS: MemoryId = MemoryId::new(16);
const REV_BUCKET_FREE_SPANS: MemoryId = MemoryId::new(17);
const REV_BUCKET_FREE_SPAN_BY_START: MemoryId = MemoryId::new(18);
const REV_EDGE_COUNTS: MemoryId = MemoryId::new(19);
const REV_EDGES: MemoryId = MemoryId::new(20);
const REV_EDGE_LOG: MemoryId = MemoryId::new(21);
const REV_EDGE_SPAN_META: MemoryId = MemoryId::new(22);
const REV_EDGE_FREE_SPANS: MemoryId = MemoryId::new(23);
const REV_EDGE_FREE_SPAN_BY_START: MemoryId = MemoryId::new(24);

// --- Labeled graph: reverse payload (5 memories) ---
const REV_PAYLOAD_SLAB: MemoryId = MemoryId::new(25);
const REV_PAYLOAD_FREE_SPANS: MemoryId = MemoryId::new(26);
const REV_PAYLOAD_FREE_SPAN_BY_START: MemoryId = MemoryId::new(27);
const REV_PAYLOAD_LOG: MemoryId = MemoryId::new(28);
const REV_PAYLOAD_BLOBS: MemoryId = MemoryId::new(29);

// --- LARA maintenance (2 memories) ---
const MAINTENANCE_QUEUE: MemoryId = MemoryId::new(30);
const DIRTY_WORK_ITEMS: MemoryId = MemoryId::new(31);

// --- Graph facade (11 memories) ---
const VERTEX_LABEL_SETS: MemoryId = MemoryId::new(32);
const VERTEX_PROPERTIES: MemoryId = MemoryId::new(33);
const EDGE_PROPERTIES: MemoryId = MemoryId::new(34);
const EDGE_ALIASES: MemoryId = MemoryId::new(35);
const GRAPH_METADATA: MemoryId = MemoryId::new(36);
const EDGE_PAYLOAD_PROFILES: MemoryId = MemoryId::new(37);
const EDGE_EQUALITY_POSTINGS: MemoryId = MemoryId::new(38);
const LABEL_TELEMETRY_SEQ: MemoryId = MemoryId::new(39);
const LABEL_TELEMETRY_OUTBOX: MemoryId = MemoryId::new(40);
const APPLIED_MUTATION_REQUESTS: MemoryId = MemoryId::new(41);

pub(crate) const GRAPH_DEFAULT_EDGE_LABEL: LaraLabelId = LaraLabelId::UNLABELED_DIRECTED;

/// Initial slab capacity for both labeled orientations (grows as needed).
const GRAPH_ELEM_CAPACITY: u64 = 1 << 20;

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

pub(crate) type StableGraph = DeferredBidirectionalLabeledLaraGraph<Edge, Memory>;
pub(crate) type StableVertexLabelStore = VertexLabelStore<Memory>;
pub(crate) type StableVertexPropertyStore = VertexPropertyStore<Memory>;
pub(crate) type StableEdgePropertyStore = EdgePropertyStore<Memory>;
pub(crate) type StableEdgeAliasIndex = EdgeAliasIndex<Memory>;
pub(crate) type StableMetadata = StableGraphMetadata<Memory>;
pub(crate) type StableEdgePayloadProfileStore = EdgePayloadProfileStore<Memory>;
pub(crate) type StableEdgeEqualityPostingStore =
    super::edge_equality_postings::EdgeEqualityPostingStore<Memory>;
pub(crate) type StableLabelTelemetrySeq = StableCell<u64, Memory>;
pub(crate) type StableLabelTelemetryOutbox = super::label_telemetry::LabelTelemetryOutbox<Memory>;
pub(crate) type StableAppliedMutationRequests =
    super::label_telemetry::AppliedMutationRequests<Memory>;

thread_local! {
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
}

pub(crate) fn init_graph() -> StableGraph {
    let graph = DeferredBidirectionalLabeledLaraGraph::init_with_config(
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_VERTICES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_BUCKETS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_BUCKET_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_BUCKET_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGE_COUNTS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGE_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGE_SPAN_META)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGE_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_EDGE_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_PAYLOAD_SLAB)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_PAYLOAD_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_PAYLOAD_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_PAYLOAD_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_PAYLOAD_BLOBS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_VERTICES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_BUCKETS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_BUCKET_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_BUCKET_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGE_COUNTS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGES)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGE_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGE_SPAN_META)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGE_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_EDGE_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_PAYLOAD_SLAB)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_PAYLOAD_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_PAYLOAD_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_PAYLOAD_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_PAYLOAD_BLOBS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(MAINTENANCE_QUEUE)),
        MEMORY_MANAGER.with(|m| m.borrow().get(DIRTY_WORK_ITEMS)),
        GRAPH_ELEM_CAPACITY,
        GRAPH_DEFAULT_EDGE_LABEL,
        DeferredConfig::default(),
    )
    .unwrap();

    crate::facade::init_ic_gql_extensions();

    graph
}

pub(crate) fn init_vertex_label_store() -> StableVertexLabelStore {
    VertexLabelStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(VERTEX_LABEL_SETS)))
}

pub(crate) fn init_vertex_property_store() -> StableVertexPropertyStore {
    VertexPropertyStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(VERTEX_PROPERTIES)))
}

pub(crate) fn init_edge_property_store() -> StableEdgePropertyStore {
    EdgePropertyStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_PROPERTIES)))
}

pub(crate) fn init_edge_alias_index() -> StableEdgeAliasIndex {
    EdgeAliasIndex::init(MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_ALIASES)))
}

pub(crate) fn init_edge_payload_profiles() -> StableEdgePayloadProfileStore {
    EdgePayloadProfileStore::init(MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_PAYLOAD_PROFILES)))
}

pub(crate) fn init_metadata() -> StableMetadata {
    StableGraphMetadata::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(GRAPH_METADATA)),
        GraphMetadata::default(),
    )
}

pub(crate) fn init_edge_equality_postings() -> StableEdgeEqualityPostingStore {
    super::edge_equality_postings::EdgeEqualityPostingStore::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(EDGE_EQUALITY_POSTINGS)),
    )
}

pub(crate) fn init_label_telemetry_seq() -> StableLabelTelemetrySeq {
    StableCell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(LABEL_TELEMETRY_SEQ)),
        0u64,
    )
}

pub(crate) fn init_label_telemetry_outbox() -> StableLabelTelemetryOutbox {
    super::label_telemetry::LabelTelemetryOutbox::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(LABEL_TELEMETRY_OUTBOX)),
    )
}

pub(crate) fn init_applied_mutation_requests() -> StableAppliedMutationRequests {
    super::label_telemetry::AppliedMutationRequests::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(APPLIED_MUTATION_REQUESTS)),
    )
}
