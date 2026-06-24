//! Stable-memory-backed vector index fragments (router auth, shard catalog, defs, pages).

use std::cell::RefCell;

pub(crate) mod layout;
pub(crate) mod memory;

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

    pub(crate) static VECTOR_ID_TO_SLOT: RefCell<memory::StableIdToSlotMap> =
        RefCell::new(memory::init_id_to_slot());

    pub(crate) static VECTOR_PARTITION_HEADS: RefCell<memory::StablePartitionHeadsMap> =
        RefCell::new(memory::init_partition_heads());

    pub(crate) static VECTOR_PAGE: RefCell<memory::StablePageMap> =
        RefCell::new(memory::init_pages());
}
