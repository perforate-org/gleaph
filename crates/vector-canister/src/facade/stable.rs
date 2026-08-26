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
    // Force the page-store thread-local to initialize *before* any coupled-region borrow is
    // taken: its reopen validation reads `VECTOR_PARTITION_HEADS`, so the acquisition order
    // must stay `PAGE_STORE` → `VECTOR_PARTITION_HEADS` everywhere.
    PAGE_STORE.with(|_| {});
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

    // Slice 8 composite slab page store: pages live in VECTOR_ROW_SLAB (id 13) addressed
    // arithmetically as uniform blocks; the per-partition page state lives in
    // VECTOR_PARTITION_HEADS (heads + sealed-page tables + slab free-list). Opened with
    // reopen validation over both collections.
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

// --- Intrusive free-block chain anchor (Slice 8) ---
//
// The free list is a LIFO chain threaded through the free blocks themselves (each block's first
// four bytes hold the next block's seq); the anchor lives in the durable compaction-state record
// so it survives upgrades and is sanitized by compaction finalize.

// --- Typed accessors over `VECTOR_PARTITION_HEADS` (Slice 8) ---
//
// One hash-map collection carries three record kinds under disjoint key-level tags
// (`PartitionHeadRecord`). These accessors are the only place that unwraps the tag, so a record
// whose tag disagrees with its key's level is a fail-closed corruption trap at the boundary.

use crate::records::{
    MAX_PAGE_TABLE_CHUNKS, PageTableChunk, PartitionHead, PartitionHeadRecord, PartitionKey,
    VectorSlabCompactionState,
};

fn unwrap_head_record(
    key: &PartitionKey,
    record: Option<PartitionHeadRecord>,
) -> Option<PartitionHead> {
    match record {
        None => None,
        Some(PartitionHeadRecord::Head(head)) => Some(head),
        Some(_) => panic!("partition heads: non-head record under head key {key:?}"),
    }
}

/// The leaf partition head of `key`, or `None` when the partition has no pages.
pub(crate) fn partition_head_get(key: &PartitionKey) -> Option<PartitionHead> {
    let record = VECTOR_PARTITION_HEADS.with_borrow(|h| h.get(key).expect("partition heads read"));
    unwrap_head_record(key, record)
}

/// Inserts (or overwrites) the head of `key`. `Err(())` surfaces the map's grow failure so
/// hot-path callers keep their existing `StableGrowFailed` mapping instead of trapping.
pub(crate) fn partition_head_insert(key: PartitionKey, head: PartitionHead) -> Result<(), ()> {
    VECTOR_PARTITION_HEADS.with_borrow_mut(|h| {
        h.insert(key, PartitionHeadRecord::Head(head))
            .map(|_| ())
            .map_err(|_| ())
    })
}

/// Removes the head of `key` (teardown). Panics when absent — callers own the lifecycle order.
pub(crate) fn partition_head_remove(key: &PartitionKey) {
    VECTOR_PARTITION_HEADS
        .with_borrow_mut(|h| h.remove(key).expect("partition head remove"))
        .expect("partition head present");
}

/// Reads one sealed-page-table chunk of a leaf partition; `None` when that chunk does not exist.
pub(crate) fn page_table_chunk_get(
    index_id: u32,
    index_version: u64,
    partition_id: u32,
    chunk: u32,
) -> Option<PageTableChunk> {
    let key = PartitionKey::page_table_chunk(index_id, index_version, partition_id, chunk);
    match VECTOR_PARTITION_HEADS.with_borrow(|h| h.get(&key).expect("partition heads read")) {
        None => None,
        Some(PartitionHeadRecord::Table(table)) => Some(table),
        Some(other) => panic!(
            "partition heads: unexpected record kind under table chunk key {key:?}: {other:?}"
        ),
    }
}

/// Writes one sealed-page-table chunk of a leaf partition. `Err(())` surfaces grow failure.
pub(crate) fn page_table_chunk_put(
    index_id: u32,
    index_version: u64,
    partition_id: u32,
    chunk: u32,
    table: PageTableChunk,
) -> Result<(), ()> {
    let key = PartitionKey::page_table_chunk(index_id, index_version, partition_id, chunk);
    VECTOR_PARTITION_HEADS.with_borrow_mut(|h| {
        h.insert(key, PartitionHeadRecord::Table(table))
            .map(|_| ())
            .map_err(|_| ())
    })
}

/// Removes every sealed-page-table chunk of one leaf partition (teardown).
pub(crate) fn page_table_remove_all(index_id: u32, index_version: u64, partition_id: u32) {
    for chunk in 0..MAX_PAGE_TABLE_CHUNKS {
        let key = PartitionKey::page_table_chunk(index_id, index_version, partition_id, chunk);
        match VECTOR_PARTITION_HEADS.with_borrow_mut(|h| h.remove(&key)) {
            Ok(None) => break, // chunks are dense; the first absent one ends the table.
            Ok(Some(_)) => {}
            Err(error) => panic!("partition table chunk remove failed: {error:?}"),
        }
    }
}

/// Removes one sealed-page-table chunk of a leaf partition; `Err(())` surfaces grow failure of
/// the underlying map operation (removal itself cannot grow — mapped for uniformity).
pub(crate) fn page_table_chunk_remove(
    index_id: u32,
    index_version: u64,
    partition_id: u32,
    chunk: u32,
) {
    let key = PartitionKey::page_table_chunk(index_id, index_version, partition_id, chunk);
    VECTOR_PARTITION_HEADS
        .with_borrow_mut(|h| h.remove(&key).expect("partition table chunk remove"))
        .expect("partition table chunk present");
}

/// Reads the intrusive free-block chain anchor (`None` = no reusable holes below the tail).
pub(crate) fn slab_free_anchor_get() -> Option<u32> {
    match VECTOR_SLAB_COMPACTION_STATE.with_borrow(|c| *c.get()) {
        VectorSlabCompactionState::Idle { free_head } => free_head,
        VectorSlabCompactionState::Compacting { free_head, .. } => free_head,
    }
}

/// Writes the intrusive free-block chain anchor, preserving whichever driver variant is current.
pub(crate) fn slab_free_anchor_set(free_head: Option<u32>) {
    VECTOR_SLAB_COMPACTION_STATE.with_borrow_mut(|c| {
        let next = match *c.get() {
            VectorSlabCompactionState::Idle { .. } => VectorSlabCompactionState::Idle { free_head },
            VectorSlabCompactionState::Compacting {
                write_cursor,
                range_end,
                scan_cursor,
                pages_moved,
                ..
            } => VectorSlabCompactionState::Compacting {
                write_cursor,
                range_end,
                scan_cursor,
                pages_moved,
                free_head,
            },
        };
        c.set(next);
    });
}
