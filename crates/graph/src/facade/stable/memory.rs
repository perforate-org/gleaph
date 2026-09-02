//! Graph canister stable-memory layout — see `design/storage/stable-memory-inventory.md`
//! and `facade/stable/layout.rs` (ADR 0007 registry).

use super::canonical_export::CanonicalExportScopeStore;
use super::edge_properties::EdgePropertyStore;
use super::metadata::{GraphMetadata, StableGraphMetadata};
use super::vertex_labels::VertexLabelStore;
use super::vertex_properties::VertexPropertyStore;
use gleaph_graph_kernel::entry::Edge;
use ic_stable_lara::{
    BucketLabelKey as LaraLabelId, DeferredBidirectionalLabeledLaraGraph,
    labeled::InitialCapacities, lara::maintenance::DeferredConfig,
};
use ic_stable_memory_backend::{DefaultMemoryImpl, default_memory_impl};
use ic_stable_roaring::StableRoaringBitmap;
use ic_stable_structures::memory_manager::MemoryId;
#[cfg(feature = "canbench_standard_manager")]
use ic_stable_structures::memory_manager::{MemoryManager, VirtualMemory};
use ic_stable_structures::{Memory as StableMemory, StableCell};
#[cfg(not(feature = "canbench_standard_manager"))]
use ic_stable_variable_memory_manager::{MemoryManager, VirtualMemory};
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

// --- Labeled graph: forward inline property bytes (5 memories) ---
const FWD_INLINE_PROPERTY_BYTES_SLAB: MemoryId = MemoryId::new(10);
const FWD_INLINE_PROPERTY_BYTES_FREE_SPANS: MemoryId = MemoryId::new(11);
const FWD_INLINE_PROPERTY_BYTES_FREE_SPAN_BY_START: MemoryId = MemoryId::new(12);
const FWD_INLINE_PROPERTY_BYTES_LOG: MemoryId = MemoryId::new(13);
const FWD_INLINE_PROPERTY_BYTES_BLOBS: MemoryId = MemoryId::new(14);

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

// --- Labeled graph: reverse inline property bytes (5 memories) ---
const REV_INLINE_PROPERTY_BYTES_SLAB: MemoryId = MemoryId::new(25);
const REV_INLINE_PROPERTY_BYTES_FREE_SPANS: MemoryId = MemoryId::new(26);
const REV_INLINE_PROPERTY_BYTES_FREE_SPAN_BY_START: MemoryId = MemoryId::new(27);
const REV_INLINE_PROPERTY_BYTES_LOG: MemoryId = MemoryId::new(28);
const REV_INLINE_PROPERTY_BYTES_BLOBS: MemoryId = MemoryId::new(29);

// --- Labeled graph: LARA Tree Block store (1 memory per orientation, Plan 0318 §Step 2) ---
const FWD_LTB: MemoryId = MemoryId::new(53);
const REV_LTB: MemoryId = MemoryId::new(54);

// --- LARA maintenance (1 memory) ---
const MAINTENANCE_QUEUE: MemoryId = MemoryId::new(30);
// MemoryId 31 was formerly DIRTY_WORK_ITEMS. It remains reserved and is not allocated.

// --- Graph facade core (8 memories) ---
const VERTEX_LABEL_SETS: MemoryId = MemoryId::new(32);
const VERTEX_PROPERTIES: MemoryId = MemoryId::new(33);
const EDGE_PROPERTIES: MemoryId = MemoryId::new(34);
// MemoryId 35 was formerly EDGE_ALIASES. It remains reserved and is not allocated.
const GRAPH_METADATA: MemoryId = MemoryId::new(36);
const LABEL_STATS_DELTA_SEQ: MemoryId = MemoryId::new(37);
const LABEL_STATS_DELTA_LOG: MemoryId = MemoryId::new(38);
const GRAPH_MUTATION_JOURNAL: MemoryId = MemoryId::new(39);

// --- Resumable super-node vertex purge (ADR 0021) (1 memory) ---
const PENDING_VERTEX_PURGES: MemoryId = MemoryId::new(40);

// --- Federated index repair journal (ADR 0023 D5) (1 memory) ---
const INDEX_REPAIR_JOURNAL: MemoryId = MemoryId::new(41);

// --- Cross-shard uniqueness: pinned unique-effect outbox (ADR 0030) (1 memory) ---
const UNIQUE_EFFECT_OUTBOX: MemoryId = MemoryId::new(42);

// --- ShardLocalGlobal fast path: graph-shard-local unique value table (ADR 0030 slice 10) (1 memory) ---
const GRAPH_LOCAL_UNIQUE_VALUES: MemoryId = MemoryId::new(43);

// MemoryIds 44-45 were formerly VERTEX_EMBEDDINGS / VERTEX_EMBEDDING_INCARNATIONS (ADR 0031).
// They remain reserved and are not allocated: the vector canister owns embedding bytes (ADR 0064 §1).

// --- Durable derived-index outbox (0088) (1 memory) ---
const DERIVED_INDEX_OUTBOX: MemoryId = MemoryId::new(46);

// MemoryIds 47-50 remain reserved after removal of the former adaptive counterpart substrate.
// --- ADR 0059 canonical export scopes (1 memory) ---
const CANONICAL_EXPORT_SCOPES: MemoryId = MemoryId::new(51);

// --- Exact ordinary derived-index pending floor (1 memory) ---
const INDEX_PENDING_FLOOR: MemoryId = MemoryId::new(52);

pub(crate) const GRAPH_DEFAULT_EDGE_LABEL: LaraLabelId = LaraLabelId::UNLABELED_DIRECTED;

/// Initial label-bucket descriptor capacity for each labeled orientation (grows as needed).
const GRAPH_INITIAL_BUCKET_CAPACITY: u64 = 1 << 10;
/// Initial edge-slot capacity for each labeled orientation (grows as needed).
const GRAPH_INITIAL_EDGE_CAPACITY: u64 = 1 << 12;
/// Initial inline-property-bytes byte capacity for each labeled orientation (grows as needed).
const GRAPH_INITIAL_INLINE_PROPERTY_BYTES: u64 = 1 << 16;
/// Default policy for regions not listed in `GRAPH_MEMORY_MANAGER_POLICIES`.
#[cfg(all(
    any(feature = "canbench_uniform_4", feature = "canbench_standard_manager"),
    not(any(
        feature = "canbench_uniform_8",
        feature = "canbench_uniform_16",
        feature = "canbench_uniform_32",
    )),
))]
const GRAPH_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES: u16 = 4;
#[cfg(all(
    feature = "canbench_uniform_8",
    not(any(feature = "canbench_uniform_16", feature = "canbench_uniform_32")),
))]
const GRAPH_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES: u16 = 8;
#[cfg(all(feature = "canbench_uniform_16", not(feature = "canbench_uniform_32")))]
const GRAPH_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES: u16 = 16;
#[cfg(feature = "canbench_uniform_32")]
const GRAPH_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES: u16 = 32;
#[cfg(not(any(
    feature = "canbench_uniform_4",
    feature = "canbench_uniform_8",
    feature = "canbench_uniform_16",
    feature = "canbench_uniform_32",
    feature = "canbench_standard_manager",
)))]
const GRAPH_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES: u16 = 4;

/// Larger policies are reserved for regions whose rows or payloads grow materially.
/// The policy is persisted by the custom manager and must not change on reopen.
#[cfg(not(any(
    feature = "canbench_uniform_4",
    feature = "canbench_uniform_8",
    feature = "canbench_uniform_16",
    feature = "canbench_uniform_32",
)))]
const GRAPH_MEMORY_MANAGER_POLICIES: &[(MemoryId, u16)] = &[
    // LARA vertex and bucket rows: primary shard cardinality, but fixed-width.
    (FWD_VERTICES, 8),
    (FWD_BUCKETS, 8),
    // LARA adjacency and overflow logs: PMA/relocation hot path.
    (FWD_EDGES, 16),
    (FWD_EDGE_LOG, 16),
    // Edge-local inline property bytes: larger variable-width inline property bytes domains.
    (FWD_INLINE_PROPERTY_BYTES_SLAB, 32),
    (FWD_INLINE_PROPERTY_BYTES_LOG, 32),
    (FWD_INLINE_PROPERTY_BYTES_BLOBS, 32),
    (REV_VERTICES, 8),
    (REV_BUCKETS, 8),
    (REV_EDGES, 16),
    (REV_EDGE_LOG, 16),
    (REV_INLINE_PROPERTY_BYTES_SLAB, 32),
    (REV_INLINE_PROPERTY_BYTES_LOG, 32),
    (REV_INLINE_PROPERTY_BYTES_BLOBS, 32),
    // LARA Tree Block stores: label-bucket tree blocks grow with adjacency.
    (FWD_LTB, 16),
    (REV_LTB, 16),
    // Label sidecars and value-bearing facade stores.
    (VERTEX_LABEL_SETS, 8),
    (VERTEX_PROPERTIES, 64),
    (EDGE_PROPERTIES, 64),
    // Bounded append/repair domains.
    (LABEL_STATS_DELTA_LOG, 8),
    (GRAPH_MUTATION_JOURNAL, 8),
    (INDEX_REPAIR_JOURNAL, 16),
    (UNIQUE_EFFECT_OUTBOX, 16),
    (GRAPH_LOCAL_UNIQUE_VALUES, 64),
    (DERIVED_INDEX_OUTBOX, 16),
    (CANONICAL_EXPORT_SCOPES, 8),
    (INDEX_PENDING_FLOOR, 8),
];
#[cfg(any(
    feature = "canbench_uniform_4",
    feature = "canbench_uniform_8",
    feature = "canbench_uniform_16",
    feature = "canbench_uniform_32",
))]
const GRAPH_MEMORY_MANAGER_POLICIES: &[(MemoryId, u16)] = &[];

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

pub(crate) type StableGraph = DeferredBidirectionalLabeledLaraGraph<Edge, Memory>;
pub(crate) type StableVertexLabelStore = VertexLabelStore<Memory>;
pub(crate) type StableVertexPropertyStore = VertexPropertyStore<Memory>;
pub(crate) type StableEdgePropertyStore = EdgePropertyStore<Memory>;
pub(crate) type StableMetadata = StableGraphMetadata<Memory>;
pub(crate) type StableLabelStatsDeltaSeq = StableCell<u64, Memory>;
pub(crate) type StableLabelStatsDeltaLog = super::label_stats_delta::LabelStatsDeltaLog<Memory>;
pub(crate) type StableGraphMutationJournal = super::label_stats_delta::GraphMutationJournal<Memory>;
/// Vertices mid-purge after a tombstone-first `DETACH DELETE` (ADR 0021).
pub(crate) type StablePendingPurges = StableRoaringBitmap<Memory>;
/// Durable failed-flush index postings awaiting re-application (ADR 0023 D5).
pub(crate) type StableRepairJournal = super::repair_journal::RepairJournal<Memory>;
/// Pinned unique-effect receipts awaiting Router ack (ADR 0030).
pub(crate) type StableUniqueEffectOutbox = super::unique_effect_outbox::UniqueEffectOutbox<Memory>;
/// Graph-shard-local unique values for `ShardLocalGlobal` constraints (ADR 0030 slice 10).
pub(crate) type StableGraphLocalUniqueTable = super::local_unique::GraphLocalUniqueTable<Memory>;
/// Durable derived-index operations awaiting their first delivery attempt (0088).
pub(crate) type StableDerivedIndexOutbox = super::derived_index_outbox::DerivedIndexOutbox<Memory>;
/// Frozen Graph-owned canonical export scope per physical posting namespace (ADR 0059).
pub(crate) type StableCanonicalExportScopes = CanonicalExportScopeStore<Memory>;
/// Exact owner-tagged floor for ordinary work in MemoryIds 41 and 46.
pub(crate) type StableIndexPendingFloor = super::index_pending_floor::IndexPendingFloor<Memory>;

#[cfg(feature = "canbench_standard_manager")]
fn init_memory_manager() -> MemoryManager<DefaultMemoryImpl> {
    MemoryManager::init_with_bucket_size(
        default_memory_impl(),
        GRAPH_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES,
    )
}

#[cfg(not(feature = "canbench_standard_manager"))]
fn init_memory_manager() -> MemoryManager<DefaultMemoryImpl> {
    MemoryManager::init_with_policies(
        default_memory_impl(),
        GRAPH_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES,
        GRAPH_MEMORY_MANAGER_POLICIES,
    )
}

thread_local! {
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(init_memory_manager());
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
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_INLINE_PROPERTY_BYTES_SLAB)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_INLINE_PROPERTY_BYTES_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_INLINE_PROPERTY_BYTES_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_INLINE_PROPERTY_BYTES_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_INLINE_PROPERTY_BYTES_BLOBS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(FWD_LTB)),
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
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_INLINE_PROPERTY_BYTES_SLAB)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_INLINE_PROPERTY_BYTES_FREE_SPANS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_INLINE_PROPERTY_BYTES_FREE_SPAN_BY_START)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_INLINE_PROPERTY_BYTES_LOG)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_INLINE_PROPERTY_BYTES_BLOBS)),
        MEMORY_MANAGER.with(|m| m.borrow().get(REV_LTB)),
        MEMORY_MANAGER.with(|m| m.borrow().get(MAINTENANCE_QUEUE)),
        InitialCapacities {
            bucket_slots: GRAPH_INITIAL_BUCKET_CAPACITY,
            edge_slots: GRAPH_INITIAL_EDGE_CAPACITY,
            inline_property_bytes: GRAPH_INITIAL_INLINE_PROPERTY_BYTES,
        },
        GRAPH_DEFAULT_EDGE_LABEL,
        DeferredConfig::default(),
    )
    .unwrap_or_else(|err| {
        // The LARA stores validate magic/version/stride against this build's fixed-width
        // rows, so the only way init fails on a populated canister is a layout-changing
        // upgrade shipped without a stable-memory migration. Trap with an actionable
        // message instead of serving corrupted reads from a mismatched layout.
        panic!(
            "graph stable layout is incompatible with this canister build ({err:?}); \
             a stable-memory migration is required before this upgrade"
        )
    });

    crate::facade::init_ic_gql_extensions();

    graph
}

fn graph_bucket_size(id: MemoryId) -> u16 {
    GRAPH_MEMORY_MANAGER_POLICIES
        .iter()
        .find_map(|(policy_id, bucket_pages)| (*policy_id == id).then_some(*bucket_pages))
        .unwrap_or(GRAPH_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES)
}

/// Reports the logical size of every graph-owned virtual stable-memory region.
///
/// `VirtualMemory::size()` excludes the memory manager's bucket rounding. The
/// canister's total physical stable-memory usage remains observable through the
/// management API / `icp canister status`.
pub(crate) fn stable_memory_stats() -> gleaph_graph_kernel::stable_memory::StableMemoryStats {
    const WASM_PAGE_SIZE: u64 = 65_536;
    const REGIONS: &[(&str, u8, MemoryId)] = &[
        ("fwd_vertices", 0, FWD_VERTICES),
        ("fwd_buckets", 1, FWD_BUCKETS),
        ("fwd_bucket_free_spans", 2, FWD_BUCKET_FREE_SPANS),
        (
            "fwd_bucket_free_span_by_start",
            3,
            FWD_BUCKET_FREE_SPAN_BY_START,
        ),
        ("fwd_edge_counts", 4, FWD_EDGE_COUNTS),
        ("fwd_edges", 5, FWD_EDGES),
        ("fwd_edge_log", 6, FWD_EDGE_LOG),
        ("fwd_edge_span_meta", 7, FWD_EDGE_SPAN_META),
        ("fwd_edge_free_spans", 8, FWD_EDGE_FREE_SPANS),
        (
            "fwd_edge_free_span_by_start",
            9,
            FWD_EDGE_FREE_SPAN_BY_START,
        ),
        (
            "fwd_inline_property_bytes_slab",
            10,
            FWD_INLINE_PROPERTY_BYTES_SLAB,
        ),
        (
            "fwd_inline_property_bytes_free_spans",
            11,
            FWD_INLINE_PROPERTY_BYTES_FREE_SPANS,
        ),
        (
            "fwd_inline_property_bytes_free_span_by_start",
            12,
            FWD_INLINE_PROPERTY_BYTES_FREE_SPAN_BY_START,
        ),
        (
            "fwd_inline_property_bytes_log",
            13,
            FWD_INLINE_PROPERTY_BYTES_LOG,
        ),
        (
            "fwd_inline_property_bytes_blobs",
            14,
            FWD_INLINE_PROPERTY_BYTES_BLOBS,
        ),
        ("rev_vertices", 15, REV_VERTICES),
        ("rev_buckets", 16, REV_BUCKETS),
        ("rev_bucket_free_spans", 17, REV_BUCKET_FREE_SPANS),
        (
            "rev_bucket_free_span_by_start",
            18,
            REV_BUCKET_FREE_SPAN_BY_START,
        ),
        ("rev_edge_counts", 19, REV_EDGE_COUNTS),
        ("rev_edges", 20, REV_EDGES),
        ("rev_edge_log", 21, REV_EDGE_LOG),
        ("rev_edge_span_meta", 22, REV_EDGE_SPAN_META),
        ("rev_edge_free_spans", 23, REV_EDGE_FREE_SPANS),
        (
            "rev_edge_free_span_by_start",
            24,
            REV_EDGE_FREE_SPAN_BY_START,
        ),
        (
            "rev_inline_property_bytes_slab",
            25,
            REV_INLINE_PROPERTY_BYTES_SLAB,
        ),
        (
            "rev_inline_property_bytes_free_spans",
            26,
            REV_INLINE_PROPERTY_BYTES_FREE_SPANS,
        ),
        (
            "rev_inline_property_bytes_free_span_by_start",
            27,
            REV_INLINE_PROPERTY_BYTES_FREE_SPAN_BY_START,
        ),
        (
            "rev_inline_property_bytes_log",
            28,
            REV_INLINE_PROPERTY_BYTES_LOG,
        ),
        (
            "rev_inline_property_bytes_blobs",
            29,
            REV_INLINE_PROPERTY_BYTES_BLOBS,
        ),
        ("maintenance_queue", 30, MAINTENANCE_QUEUE),
        // MemoryId 31 is reserved after DIRTY_WORK_ITEMS removal.
        ("vertex_label_sets", 32, VERTEX_LABEL_SETS),
        ("vertex_properties", 33, VERTEX_PROPERTIES),
        ("edge_properties", 34, EDGE_PROPERTIES),
        // MemoryId 35 is reserved after EDGE_ALIASES removal.
        ("graph_metadata", 36, GRAPH_METADATA),
        ("label_stats_delta_seq", 37, LABEL_STATS_DELTA_SEQ),
        ("label_stats_delta_log", 38, LABEL_STATS_DELTA_LOG),
        ("graph_mutation_journal", 39, GRAPH_MUTATION_JOURNAL),
        ("pending_vertex_purges", 40, PENDING_VERTEX_PURGES),
        ("index_repair_journal", 41, INDEX_REPAIR_JOURNAL),
        ("unique_effect_outbox", 42, UNIQUE_EFFECT_OUTBOX),
        ("graph_local_unique_values", 43, GRAPH_LOCAL_UNIQUE_VALUES),
        // MemoryIds 44-45 are reserved after VERTEX_EMBEDDINGS / VERTEX_EMBEDDING_INCARNATIONS removal.
        ("derived_index_outbox", 46, DERIVED_INDEX_OUTBOX),
        ("canonical_export_scopes", 51, CANONICAL_EXPORT_SCOPES),
        ("index_pending_floor", 52, INDEX_PENDING_FLOOR),
        ("fwd_ltb", 53, FWD_LTB),
        ("rev_ltb", 54, REV_LTB),
    ];

    let regions: Vec<_> = REGIONS
        .iter()
        .map(|(name, memory_id, id)| {
            let logical_pages = MEMORY_MANAGER.with(|manager| manager.borrow().get(*id).size());
            let bucket_pages = graph_bucket_size(*id);
            let allocated_pages = logical_pages
                .div_ceil(u64::from(bucket_pages))
                .saturating_mul(u64::from(bucket_pages));
            gleaph_graph_kernel::stable_memory::StableMemoryRegionStats {
                name: (*name).to_string(),
                memory_id: *memory_id,
                bucket_pages,
                logical_pages,
                logical_bytes: logical_pages.saturating_mul(WASM_PAGE_SIZE),
                allocated_pages,
                slack_pages: allocated_pages.saturating_sub(logical_pages),
            }
        })
        .collect();
    let logical_total_pages = regions
        .iter()
        .map(|region| region.logical_pages)
        .fold(0, u64::saturating_add);
    let allocated_region_pages = regions
        .iter()
        .map(|region| region.allocated_pages)
        .fold(0, u64::saturating_add);
    let estimated_allocated_pages = 2u64.saturating_add(allocated_region_pages);
    gleaph_graph_kernel::stable_memory::StableMemoryStats {
        bucket_pages: GRAPH_MEMORY_MANAGER_DEFAULT_BUCKET_SIZE_PAGES,
        logical_total_pages,
        logical_total_bytes: logical_total_pages.saturating_mul(WASM_PAGE_SIZE),
        estimated_allocated_pages,
        estimated_allocated_bytes: estimated_allocated_pages.saturating_mul(WASM_PAGE_SIZE),
        regions,
    }
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

pub(crate) fn init_metadata() -> StableMetadata {
    StableGraphMetadata::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(GRAPH_METADATA)),
        GraphMetadata::default(),
    )
}

pub(crate) fn init_label_stats_delta_seq() -> StableLabelStatsDeltaSeq {
    StableCell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(LABEL_STATS_DELTA_SEQ)),
        0u64,
    )
}

pub(crate) fn init_label_stats_delta_log() -> StableLabelStatsDeltaLog {
    super::label_stats_delta::LabelStatsDeltaLog::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(LABEL_STATS_DELTA_LOG)),
    )
}

pub(crate) fn init_graph_mutation_journal() -> StableGraphMutationJournal {
    super::label_stats_delta::GraphMutationJournal::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(GRAPH_MUTATION_JOURNAL)),
    )
}

pub(crate) fn init_pending_vertex_purges() -> StablePendingPurges {
    StableRoaringBitmap::init(MEMORY_MANAGER.with(|m| m.borrow().get(PENDING_VERTEX_PURGES)))
        .expect("init pending vertex purge bitmap")
}

pub(crate) fn init_index_repair_journal() -> StableRepairJournal {
    super::repair_journal::RepairJournal::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_REPAIR_JOURNAL)),
    )
}

pub(crate) fn init_unique_effect_outbox() -> StableUniqueEffectOutbox {
    super::unique_effect_outbox::UniqueEffectOutbox::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(UNIQUE_EFFECT_OUTBOX)),
    )
}

pub(crate) fn init_graph_local_unique_table() -> StableGraphLocalUniqueTable {
    super::local_unique::GraphLocalUniqueTable::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(GRAPH_LOCAL_UNIQUE_VALUES)),
    )
}

pub(crate) fn init_derived_index_outbox() -> StableDerivedIndexOutbox {
    super::derived_index_outbox::DerivedIndexOutbox::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(DERIVED_INDEX_OUTBOX)),
    )
}

pub(crate) fn init_canonical_export_scopes() -> StableCanonicalExportScopes {
    CanonicalExportScopeStore::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(CANONICAL_EXPORT_SCOPES)),
    )
}

pub(crate) fn init_index_pending_floor() -> StableIndexPendingFloor {
    super::index_pending_floor::IndexPendingFloor::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_PENDING_FLOOR)),
    )
}

#[cfg(test)]
mod tests {
    use super::{
        MEMORY_MANAGER, init_canonical_export_scopes, init_derived_index_outbox,
        init_index_pending_floor, init_index_repair_journal, init_memory_manager,
        stable_memory_stats,
    };
    use crate::facade::stable::layout::GRAPH_STABLE_LAYOUT;
    use crate::facade::stable::repair_journal;
    use gleaph_graph_kernel::canonical_export::{
        CanonicalExportPhase, CanonicalExportRecord, CanonicalExportScope, CanonicalExportTarget,
        CanonicalRecordSource,
    };
    use gleaph_graph_kernel::entry::{GraphId, IndexNameId, PropertyId};
    use gleaph_graph_kernel::index::PhysicalIndexId;
    use std::panic::{AssertUnwindSafe, catch_unwind};

    fn sentinel_op(label_id: u32, vertex_id: u32) -> repair_journal::RepairPostingOp {
        repair_journal::RepairPostingOp::Label {
            remove: false,
            label_id,
            vertex_id,
        }
    }

    #[test]
    fn current_layout_regions_remain_separate_across_reopen() {
        let previous_manager =
            MEMORY_MANAGER.with(|manager| manager.replace(init_memory_manager()));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let (repair_sequence, outbox_sequence, floor_key) = {
                let mut repair = init_index_repair_journal();
                let mut outbox = init_derived_index_outbox();
                let mut floor = init_index_pending_floor();

                let repair_rows = repair
                    .prepare_append_all(41_041, [sentinel_op(41, 1)])
                    .expect("prepare repair sentinel");
                let repair_sequence = repair_rows[0].0;
                repair.append_prepared(repair_rows);

                let outbox_rows = outbox
                    .prepare_append_all(46_046, [sentinel_op(46, 2)])
                    .expect("prepare outbox sentinel");
                let outbox_sequence = outbox_rows[0].0;
                outbox.append_prepared(outbox_rows);

                let floor_key = super::super::index_pending_floor::IndexPendingFloorKey::new(
                    52_052,
                    super::super::index_pending_floor::IndexPendingFloorOwner::DerivedIndexOutbox,
                    52,
                )
                .expect("prepare floor sentinel");
                floor.insert(floor_key);

                assert_eq!(
                    repair.get(repair_sequence).map(|row| row.mutation_id),
                    Some(41_041)
                );
                assert_eq!(
                    outbox.get(outbox_sequence).map(|row| row.mutation_id),
                    Some(46_046)
                );
                assert_eq!(floor.min_mutation_id(), Some(52_052));

                (repair_sequence, outbox_sequence, floor_key)
            };

            let repair = init_index_repair_journal();
            let outbox = init_derived_index_outbox();
            let floor = init_index_pending_floor();

            assert_eq!(repair.len(), 1);
            assert_eq!(outbox.len(), 1);
            assert_eq!(
                repair.get(repair_sequence).map(|row| row.mutation_id),
                Some(41_041)
            );
            assert_eq!(
                outbox.get(outbox_sequence).map(|row| row.mutation_id),
                Some(46_046)
            );
            assert!(floor.contains(&floor_key));
            assert_eq!(floor.min_mutation_id(), Some(52_052));
        }));
        MEMORY_MANAGER.with(|manager| manager.replace(previous_manager));
        result.expect("current layout stable regions must remain independent");
    }

    #[test]
    fn nested_canonical_export_scope_reopens_on_memory_id_51() {
        // The region binding itself is part of the contract: the canonical-export scope store
        // owns exactly MemoryId 51 and no other Graph region may claim it.
        assert_eq!(
            GRAPH_STABLE_LAYOUT.regions[51].symbol,
            "CANONICAL_EXPORT_SCOPES"
        );
        let previous_manager =
            MEMORY_MANAGER.with(|manager| manager.replace(init_memory_manager()));
        let result = catch_unwind(AssertUnwindSafe(|| {
            let physical = PhysicalIndexId::new(51_051).expect("non-zero physical id");
            let record = CanonicalExportRecord {
                scope: CanonicalExportScope {
                    graph_id: GraphId::from_raw(3),
                    index_name_id: IndexNameId::from_raw(5),
                    catalog_epoch: 11,
                    target: CanonicalExportTarget::Vertex {
                        label_id: 7,
                        property_id: PropertyId::from_raw(9),
                        record_source: Some(CanonicalRecordSource {
                            ancestor_property_id: PropertyId::from_raw(8),
                            field_tail: "meta.deep".to_owned(),
                        }),
                    },
                    inline: None,
                },
                phase: CanonicalExportPhase::Building,
                epoch: 11,
                admitted_through: 4,
                drained_through: 2,
                authorized_puller: candid::Principal::from_slice(&[0x5E, 0x11]),
            };
            init_canonical_export_scopes().insert(physical, record.clone());

            // Reopen the same stable memory through the MemoryId 51 binding and require the
            // exact nested `record_source` plus lifecycle watermarks — a kernel-only byte
            // round trip does not satisfy this persistence gate.
            let reopened = init_canonical_export_scopes()
                .get(physical)
                .expect("canonical export record survives reopen on MemoryId 51");
            assert_eq!(reopened, record);
            assert_eq!(
                reopened.scope.target,
                CanonicalExportTarget::Vertex {
                    label_id: 7,
                    property_id: PropertyId::from_raw(9),
                    record_source: Some(CanonicalRecordSource {
                        ancestor_property_id: PropertyId::from_raw(8),
                        field_tail: "meta.deep".to_owned(),
                    }),
                }
            );
        }));
        MEMORY_MANAGER.with(|manager| manager.replace(previous_manager));
        result.expect("nested canonical export scope must reopen on MemoryId 51");
    }

    #[test]
    fn stable_memory_stats_covers_every_graph_region() {
        let stats = stable_memory_stats();

        let allocated_region_count = GRAPH_STABLE_LAYOUT
            .regions
            .iter()
            .filter(|region| region.owner_domain != "reserved")
            .count();
        assert_eq!(stats.regions.len(), allocated_region_count);
        assert_eq!(
            stats.logical_total_pages,
            stats
                .regions
                .iter()
                .map(|region| region.logical_pages)
                .sum::<u64>()
        );
        assert_eq!(stats.bucket_pages, 4);
        assert_eq!(
            stats
                .regions
                .iter()
                .find(|r| r.memory_id == 5)
                .unwrap()
                .bucket_pages,
            16
        );
        assert_eq!(
            stats
                .regions
                .iter()
                .find(|r| r.memory_id == 33)
                .unwrap()
                .bucket_pages,
            64
        );
        assert_eq!(
            stats
                .regions
                .iter()
                .find(|r| r.memory_id == 0)
                .unwrap()
                .bucket_pages,
            8
        );
        assert_eq!(
            stats.estimated_allocated_pages,
            2 + stats
                .regions
                .iter()
                .map(|region| region.allocated_pages)
                .sum::<u64>()
        );
        assert_eq!(
            stats
                .regions
                .iter()
                .map(|region| region.slack_pages)
                .sum::<u64>(),
            stats.estimated_allocated_pages - 2 - stats.logical_total_pages
        );
        assert_eq!(
            stats.regions.first().map(|region| region.memory_id),
            Some(0)
        );
        assert_eq!(
            stats.regions.last().map(|region| region.memory_id),
            GRAPH_STABLE_LAYOUT
                .regions
                .iter()
                .filter(|region| region.owner_domain != "reserved")
                .map(|region| region.memory_id)
                .max()
        );
        assert_eq!(stats.regions.len(), 47);
        assert_eq!(
            stats
                .regions
                .last()
                .map(|region| (region.memory_id, region.name.as_str())),
            Some((54, "rev_ltb"))
        );
    }
}
