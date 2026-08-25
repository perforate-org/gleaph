//! Stable-memory-backed vector index fragments (router auth, shard catalog, defs, pages).

use std::cell::RefCell;

pub(crate) mod definition_store;
pub(crate) mod layout;
pub(crate) mod memory;
pub(crate) mod page_store;
pub(crate) mod rebuild_pool;
pub(crate) mod region_store;
pub(crate) mod subject_store;

#[cfg(any(test, feature = "canbench"))]
use definition_store::DefinitionResetTicket;
#[cfg(any(test, feature = "canbench"))]
use region_store::RegionError;
#[cfg(any(test, feature = "canbench"))]
use subject_store::SubjectResetTicket;

#[cfg(any(test, feature = "canbench"))]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DefinitionDomainResetError {
    Region(RegionError),
    RegionHandleUnavailable(&'static str),
}

#[cfg(any(test, feature = "canbench"))]
impl From<RegionError> for DefinitionDomainResetError {
    fn from(error: RegionError) -> Self {
        Self::Region(error)
    }
}

/// Resets every stable region whose contents are interpreted through `VectorIndexDef`.
///
/// All mutable region handles are acquired before the definition reset performs the first stable
/// write. After that reset succeeds, the remaining clears are infallible over already-open handles;
/// an unexpected trap is left to IC update rollback rather than returned as a partial success.
#[cfg(any(test, feature = "canbench"))]
pub(crate) fn reset_definition_domain(
    ticket: DefinitionResetTicket,
    subject_ticket: SubjectResetTicket,
) -> Result<u64, DefinitionDomainResetError> {
    IVF_CENTROID_META.with(|centroid_meta| {
        let mut centroid_meta = centroid_meta.try_borrow_mut().map_err(|_| {
            DefinitionDomainResetError::RegionHandleUnavailable("IVF_CENTROID_META")
        })?;
        IVF_CENTROIDS.with(|centroids| {
            let mut centroids = centroids.try_borrow_mut().map_err(|_| {
                DefinitionDomainResetError::RegionHandleUnavailable("IVF_CENTROIDS")
            })?;
            VECTOR_DELETED_SUBJECTS.with(|deleted| {
                let mut deleted = deleted.try_borrow_mut().map_err(|_| {
                    DefinitionDomainResetError::RegionHandleUnavailable("VECTOR_DELETED_SUBJECTS")
                })?;
                VECTOR_PARTITION_HEADS.with(|heads| {
                    let heads = heads.try_borrow_mut().map_err(|_| {
                        DefinitionDomainResetError::RegionHandleUnavailable(
                            "VECTOR_PARTITION_HEADS",
                        )
                    })?;
                    PAGE_STORE.with(|pages| {
                        let mut pages = pages.try_borrow_mut().map_err(|_| {
                            DefinitionDomainResetError::RegionHandleUnavailable("PAGE_STORE")
                        })?;
                        VECTOR_REBUILD_STATE.with(|rebuild| {
                            let mut rebuild = rebuild.try_borrow_mut().map_err(|_| {
                                DefinitionDomainResetError::RegionHandleUnavailable(
                                    "VECTOR_REBUILD_STATE",
                                )
                            })?;
                            VECTOR_MAINTENANCE_STATE.with(|maintenance| {
                                let mut maintenance =
                                    maintenance.try_borrow_mut().map_err(|_| {
                                        DefinitionDomainResetError::RegionHandleUnavailable(
                                            "VECTOR_MAINTENANCE_STATE",
                                        )
                                    })?;

                                // The rebuild-pool region has no collection handle; its release
                                // (header zeroing) is infallible over the grown region.
                                crate::facade::stable::rebuild_pool::release();

                                let incarnation = definition_store::commit_reset(ticket)?;
                                subject_store::commit_reset(subject_ticket)?;
                                centroid_meta.clear_new();
                                centroids.clear_new();
                                deleted.clear_new();
                                heads.clear().expect("clear partition heads");
                                pages.reset();
                                rebuild.clear_new();
                                maintenance.clear_new();
                                Ok(incarnation)
                            })
                        })
                    })
                })
            })
        })
    })
}

thread_local! {
    pub(crate) static VECTOR_INDEX_ROUTER: RefCell<memory::StableRouterCell> =
        RefCell::new(memory::init_router());

    pub(crate) static SHARD_CANISTER_CATALOG: RefCell<memory::ShardCanisterCatalog> =
        RefCell::new(memory::init_shard_canister_catalog());

    pub(crate) static OWNERSHIP_CONFIG: RefCell<memory::StableOwnershipConfigCell> =
        RefCell::new(memory::init_ownership_config());

    pub(crate) static IVF_CENTROID_META: RefCell<memory::StableCentroidMetaMap> =
        RefCell::new(memory::init_centroid_meta());

    // Reserved empty in Slice 2; bound now to avoid a future MemoryId repack (ADR 0031).
    pub(crate) static IVF_CENTROIDS: RefCell<memory::StableCentroidsMap> =
        RefCell::new(memory::init_centroids());

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

    // ADR 0064 §5: per-shard watermark pair for conservative tombstone GC; the Router-only
    // frontier endpoint advances router_watermark monotonically for an exact attached shard and
    // runs one bounded GC step in the same no-await update.
    pub(crate) static VECTOR_SHARD_WATERMARKS: RefCell<memory::StableShardWatermarksMap> =
        RefCell::new(memory::init_shard_watermarks());

    // ADR 0064 §5: durable GC resume cursor (last examined SubjectKey).
    pub(crate) static VECTOR_GC_CURSOR: RefCell<memory::StableGcCursorCell> =
        RefCell::new(memory::init_gc_cursor());

    // Plan 0278: durable slab dead-space compaction driver state (retired VECTOR_ID_TO_SUBJECT
    // slot, MemoryId 11).
    pub(crate) static VECTOR_SLAB_COMPACTION_STATE: RefCell<memory::StableSlabCompactionStateCell> =
        RefCell::new(memory::init_slab_compaction_state());
}
