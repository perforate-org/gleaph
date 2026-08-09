//! Stable-memory-backed vector index fragments (router auth, shard catalog, defs, pages).

use std::cell::RefCell;

pub(crate) mod layout;
pub(crate) mod memory;
pub(crate) mod page_store;

thread_local! {
    pub(crate) static VECTOR_INDEX_ROUTER: RefCell<memory::StableRouterCell> =
        RefCell::new(memory::init_router());

    pub(crate) static SHARD_CANISTER_CATALOG: RefCell<memory::ShardCanisterCatalog> =
        RefCell::new(memory::init_shard_canister_catalog());

    pub(crate) static OWNERSHIP_CONFIG: RefCell<memory::StableOwnershipConfigCell> =
        RefCell::new(memory::init_ownership_config());

    pub(crate) static VECTOR_INDEX_DEFS: RefCell<memory::StableDefsMap> =
        RefCell::new(memory::init_defs());

    pub(crate) static IVF_CENTROID_META: RefCell<memory::StableCentroidMetaMap> =
        RefCell::new(memory::init_centroid_meta());

    // Reserved empty in Slice 2; bound now to avoid a future MemoryId repack (ADR 0031).
    pub(crate) static IVF_CENTROIDS: RefCell<memory::StableCentroidsMap> =
        RefCell::new(memory::init_centroids());

    pub(crate) static VECTOR_SUBJECT_TO_ID: RefCell<memory::StableSubjectMap> =
        RefCell::new(memory::init_subject_map());

    // ADR 0064 §5: deleted-subjects list giving the GC a stable key-based cursor.
    pub(crate) static VECTOR_DELETED_SUBJECTS: RefCell<memory::StableDeletedSubjectsMap> =
        RefCell::new(memory::init_deleted_subjects());

    pub(crate) static VECTOR_PARTITION_HEADS: RefCell<memory::StablePartitionHeadsMap> =
        RefCell::new(memory::init_partition_heads());

    // ADR 0032 composite slab page store: VECTOR_PAGE_META (id 10) + VECTOR_ROW_SLAB (id 13),
    // opened together with reopen validation.
    pub(crate) static PAGE_STORE: RefCell<page_store::VectorSlabStore> =
        RefCell::new(page_store::VectorSlabStore::init());

    pub(crate) static VECTOR_REBUILD_STATE: RefCell<memory::StableRebuildStateMap> =
        RefCell::new(memory::init_rebuild_state());

    // ADR 0031 Slice 10: vector-canister-owned maintenance scan execution state.
    pub(crate) static VECTOR_MAINTENANCE_STATE: RefCell<memory::StableMaintenanceStateMap> =
        RefCell::new(memory::init_maintenance_state());

    // ADR 0064 §5: per-shard watermark pair bounding the subject map.
    pub(crate) static VECTOR_SHARD_WATERMARKS: RefCell<memory::StableShardWatermarksMap> =
        RefCell::new(memory::init_shard_watermarks());

    // ADR 0064 §5: durable GC resume cursor (last examined SubjectKey).
    pub(crate) static VECTOR_GC_CURSOR: RefCell<memory::StableGcCursorCell> =
        RefCell::new(memory::init_gc_cursor());
}
