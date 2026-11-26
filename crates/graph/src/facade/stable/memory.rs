//! Graph canister stable-memory layout — see `design/storage/stable-memory-inventory.md`
//! and `facade/stable/layout.rs` (ADR 0007 registry).

use super::canonical_export::CanonicalExportScopeStore;
use super::edge_properties::EdgePropertyStore;
use super::metadata::{GraphMetadata, StableGraphMetadata};
use super::vertex_properties::VertexPropertyStore;
use gleaph_graph_kernel::entry::TaggedEdgeLabelId;
use ic_stable_memory_backend::{DefaultMemoryImpl, default_memory_impl};
use ic_stable_roaring::StableRoaringBitmap;
use ic_stable_structures::memory_manager::MemoryId;
#[cfg(feature = "canbench_standard_manager")]
use ic_stable_structures::memory_manager::{MemoryManager, VirtualMemory};
use ic_stable_structures::{Memory as StableMemory, StableCell};
#[cfg(not(feature = "canbench_standard_manager"))]
use ic_stable_variable_memory_manager::{MemoryManager, VirtualMemory};
use std::cell::RefCell;

// MemoryIds 0-30 held ic-stable-lara's forward and reverse graphs (vertices, buckets, edge slabs,
// spill, free spans, inline value slabs) and its deferred maintenance queue; 53-54 its tree block
// stores. lara-graph replaced it (ADR 0136); they remain reserved and are not allocated.
// MemoryId 31 was formerly DIRTY_WORK_ITEMS. It remains reserved and is not allocated.

// --- Graph facade core (7 memories) ---
// MemoryId 32 was formerly VERTEX_LABEL_SETS; vertex labels live in the topology store (ADR 0136,
// Decision 8). It remains reserved and is not allocated.
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

// MemoryId 55 was formerly VERTEX_LABEL_POSTINGS (ADR 0123); the postings live in the topology
// store with the label sets (ADR 0136, Decision 8). It remains reserved and is not allocated.

// --- Topology store: lara-graph's seven memories, in `MemoryRole::ALL` order (ADR 0136,
// Decision 9) ---
const TOPOLOGY_NODES: MemoryId = MemoryId::new(56);
const TOPOLOGY_EDGE_PAGES: MemoryId = MemoryId::new(57);
const TOPOLOGY_EDGE_DIRECTORY: MemoryId = MemoryId::new(58);
const TOPOLOGY_LABEL_BUCKET_LIST_PAGES: MemoryId = MemoryId::new(59);
const TOPOLOGY_LABEL_BUCKET_LIST_DIRECTORY: MemoryId = MemoryId::new(60);
const TOPOLOGY_NODE_LABELS: MemoryId = MemoryId::new(61);
/// Proxy records; empty until sharding adds a proxy (ADR 0136, Decision 10).
const TOPOLOGY_PROXIES: MemoryId = MemoryId::new(62);
/// The memories of the topology store, indexed by `MemoryRole::index`.
const TOPOLOGY_MEMORIES: [MemoryId; 7] = [
    TOPOLOGY_NODES,
    TOPOLOGY_EDGE_PAGES,
    TOPOLOGY_EDGE_DIRECTORY,
    TOPOLOGY_LABEL_BUCKET_LIST_PAGES,
    TOPOLOGY_LABEL_BUCKET_LIST_DIRECTORY,
    TOPOLOGY_NODE_LABELS,
    TOPOLOGY_PROXIES,
];

// --- Topology maintenance work list (ADR 0136, Decision 6) (1 memory) ---
const TOPOLOGY_WORK_LIST: MemoryId = MemoryId::new(63);

pub(crate) const GRAPH_DEFAULT_EDGE_LABEL: TaggedEdgeLabelId =
    TaggedEdgeLabelId::UNLABELED_DIRECTED;
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
    // Topology store: edge pages hold every edge, value and target index and grow with the
    // edge count; label bucket list pages with the bucket count; node records and label cells
    // (6 and 5 bytes per vertex) with the vertex count. The directories hold one record per
    // page and proxies stay empty until sharding.
    (TOPOLOGY_NODES, 16),
    (TOPOLOGY_EDGE_PAGES, 128),
    (TOPOLOGY_EDGE_DIRECTORY, 8),
    (TOPOLOGY_LABEL_BUCKET_LIST_PAGES, 32),
    (TOPOLOGY_LABEL_BUCKET_LIST_DIRECTORY, 8),
    (TOPOLOGY_NODE_LABELS, 16),
    (TOPOLOGY_PROXIES, 1),
    (TOPOLOGY_WORK_LIST, 8),
    // Value-bearing facade stores.
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

/// The topology store's memory for lara-graph's `MemoryRole` of index `role` (0..7).
pub(crate) fn topology_memory(role: u8) -> Memory {
    let id = TOPOLOGY_MEMORIES[usize::from(role)];
    MEMORY_MANAGER.with(|m| m.borrow().get(id))
}

/// The topology store's maintenance work list memory.
pub(crate) fn work_list_memory() -> Memory {
    MEMORY_MANAGER.with(|m| m.borrow().get(TOPOLOGY_WORK_LIST))
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
        // MemoryIds 0-30 are reserved after ic-stable-lara's removal.
        // MemoryId 31 is reserved after DIRTY_WORK_ITEMS removal.
        // MemoryId 32 is reserved after VERTEX_LABEL_SETS removal.
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
        // MemoryIds 53-55 are reserved after ic-stable-lara's and VERTEX_LABEL_POSTINGS' removal.
        ("topology_nodes", 56, TOPOLOGY_NODES),
        ("topology_edge_pages", 57, TOPOLOGY_EDGE_PAGES),
        ("topology_edge_directory", 58, TOPOLOGY_EDGE_DIRECTORY),
        (
            "topology_label_bucket_list_pages",
            59,
            TOPOLOGY_LABEL_BUCKET_LIST_PAGES,
        ),
        (
            "topology_label_bucket_list_directory",
            60,
            TOPOLOGY_LABEL_BUCKET_LIST_DIRECTORY,
        ),
        ("topology_node_labels", 61, TOPOLOGY_NODE_LABELS),
        ("topology_proxies", 62, TOPOLOGY_PROXIES),
        ("topology_work_list", 63, TOPOLOGY_WORK_LIST),
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
                .find(|r| r.memory_id == 57)
                .unwrap()
                .bucket_pages,
            128
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
                .find(|r| r.memory_id == 56)
                .unwrap()
                .bucket_pages,
            16
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
            Some(33)
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
        assert_eq!(stats.regions.len(), 21);
        assert_eq!(
            stats
                .regions
                .last()
                .map(|region| (region.memory_id, region.name.as_str())),
            Some((63, "topology_work_list"))
        );
    }
}
