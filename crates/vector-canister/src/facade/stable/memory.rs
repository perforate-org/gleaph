//! Graph-vector canister stable-memory layout — see `design/storage/stable-memory-inventory.md`
//! and `facade/stable/layout.rs` (ADR 0007 registry, ADR 0031 Slice 2).
//!
//! MemoryIds: router auth → shard catalog → ownership config → index defs → centroid meta →
//! reserved centroids → subject clock → partition heads → pages → rebuild state → row slab →
//! maintenance state. MemoryIds 8 and 11 are unallocated (retired id reverse maps).

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use ic_stable_memory_backend::{DefaultMemoryImpl, default_memory_impl};
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{BTreeMap, Cell};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;

use crate::records::{
    DeletedSubjectKey, FixedSubjectMapEntry, IvfCentroidMeta, PageKey, PartitionHead, PartitionKey,
    RawMaintenanceState, RawRebuildState, ShardWatermarks, SubjectKey, VectorIndexDef,
};
use ic_stable_clustered_hash_map::{InitError, StableClusteredHashMap};

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

const VECTOR_INDEX_ROUTER: MemoryId = MemoryId::new(0);
const VECTOR_INDEX_SHARD_CANISTER_BY_SHARD: MemoryId = MemoryId::new(1);
const VECTOR_INDEX_SHARD_BY_CANISTER: MemoryId = MemoryId::new(2);
const VECTOR_INDEX_OWNERSHIP_CONFIG: MemoryId = MemoryId::new(3);
const VECTOR_INDEX_DEFS: MemoryId = MemoryId::new(4);
const IVF_CENTROID_META: MemoryId = MemoryId::new(5);
// MemoryId 6 (IVF_CENTROIDS) is reserved empty in Slice 2; see layout registry. Allocating the
// BTreeMap now binds the id so Slice 4 can populate centroid bytes without a MemoryId repack.
const IVF_CENTROIDS: MemoryId = MemoryId::new(6);
const VECTOR_SUBJECT_TO_ID: MemoryId = MemoryId::new(7);
const VECTOR_PARTITION_HEADS: MemoryId = MemoryId::new(9);
// ADR 0032: the former `VECTOR_PAGE` large-value store is replaced by a composite slab page store.
// MemoryId 10 is reused for the page-metadata directory; MemoryId 13 is the raw row slab.
pub(crate) const VECTOR_PAGE_META: MemoryId = MemoryId::new(10);
const VECTOR_REBUILD_STATE: MemoryId = MemoryId::new(12);
pub(crate) const VECTOR_ROW_SLAB: MemoryId = MemoryId::new(13);
// ADR 0031 Slice 10: Router-forwarded maintenance orchestration. Holds the vector-canister-owned
// page-health scan execution state (cursor + merged counters). Stable execution state — it must
// survive upgrade and is cleared only on canister init/reset.
const VECTOR_MAINTENANCE_STATE: MemoryId = MemoryId::new(14);
// ADR 0064 §5: per-shard watermark pair bounding the subject map (graph_watermark, router_watermark).
const VECTOR_SHARD_WATERMARKS: MemoryId = MemoryId::new(15);
// ADR 0064 §5: durable GC resume cursor (last examined SubjectKey) so a bounded GC step never
// starves deleted entries that sort after a long run of live entries.
const VECTOR_GC_CURSOR: MemoryId = MemoryId::new(16);
// ADR 0064 §5: deleted-subjects list `(shard, tombstone stamp, subject) -> ()` giving the GC a
// stable key-based cursor (the subject map's slot order is unstable under removal).
const VECTOR_DELETED_SUBJECTS: MemoryId = MemoryId::new(17);

pub(crate) type StableRouterCell = Cell<Principal, Memory>;
pub(crate) type StableOwnershipConfigCell = Cell<VectorIndexOwnershipConfig, Memory>;
pub(crate) type StableShardCanisterByShardMap = BTreeMap<ShardId, Principal, Memory>;
pub(crate) type StableShardByCanisterMap = BTreeMap<Principal, ShardId, Memory>;
pub(crate) type StableDefsMap = StableClusteredHashMap<u32, VectorIndexDef, Memory>;
pub(crate) type StableCentroidMetaMap = BTreeMap<u32, IvfCentroidMeta, Memory>;
pub(crate) type StableCentroidsMap = BTreeMap<PartitionKey, Vec<u8>, Memory>;
pub(crate) type StableSubjectMap = StableClusteredHashMap<SubjectKey, FixedSubjectMapEntry, Memory>;
pub(crate) type StableDeletedSubjectsMap = BTreeMap<DeletedSubjectKey, u8, Memory>;
pub(crate) type StablePartitionHeadsMap = BTreeMap<PartitionKey, PartitionHead, Memory>;
pub(crate) type StablePageMetaMap = BTreeMap<PageKey, super::page_store::VectorPageMeta, Memory>;
pub(crate) type StableRebuildStateMap = BTreeMap<u32, RawRebuildState, Memory>;
pub(crate) type StableMaintenanceStateMap = BTreeMap<u32, RawMaintenanceState, Memory>;
pub(crate) type StableShardWatermarksMap = BTreeMap<ShardId, ShardWatermarks, Memory>;
pub(crate) type StableGcCursorCell = Cell<Option<DeletedSubjectKey>, Memory>;

/// Graph ownership config (ADR 0031 Slice 4 target model B). Unlike `graph-index`
/// `IndexOwnershipConfig`, a derived vector index has **one target per graph** that owns *every*
/// shard, so ownership is keyed by `graph_id` alone — there is no property-index group sharding
/// (`index_group_size` / `group_index`) here.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct VectorIndexOwnershipConfig {
    pub initialized: bool,
    pub graph_id: GraphId,
}

impl Default for VectorIndexOwnershipConfig {
    fn default() -> Self {
        Self {
            initialized: false,
            graph_id: GraphId::from_raw(0),
        }
    }
}

impl ic_stable_structures::Storable for VectorIndexOwnershipConfig {
    const BOUND: ic_stable_structures::storable::Bound =
        ic_stable_structures::storable::Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode VectorIndexOwnershipConfig"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode VectorIndexOwnershipConfig")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), VectorIndexOwnershipConfig)
            .expect("decode VectorIndexOwnershipConfig")
    }
}

/// Bidirectional shard↔canister attachment catalog — mirrors `graph-index`.
pub(crate) struct ShardCanisterCatalog {
    by_shard: StableShardCanisterByShardMap,
    by_canister: StableShardByCanisterMap,
}

impl ShardCanisterCatalog {
    pub(crate) fn init() -> Self {
        Self {
            by_shard: BTreeMap::init(
                MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_INDEX_SHARD_CANISTER_BY_SHARD)),
            ),
            by_canister: BTreeMap::init(
                MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_INDEX_SHARD_BY_CANISTER)),
            ),
        }
    }

    pub(crate) fn clear_new(&mut self) {
        self.by_shard.clear_new();
        self.by_canister.clear_new();
    }

    pub(crate) fn shard_for_canister(&self, canister: Principal) -> Option<ShardId> {
        self.by_canister.get(&canister)
    }

    /// Number of owned shards (the def-time source of `run_capacity`, ADR 0064 §7).
    pub(crate) fn owned_shard_count(&self) -> usize {
        self.by_shard.len() as usize
    }

    pub(crate) fn insert(
        &mut self,
        shard_id: ShardId,
        canister: Principal,
    ) -> Result<(), ShardCanisterCatalogInsertError> {
        if let Some(existing_canister) = self.by_shard.get(&shard_id) {
            if existing_canister == canister {
                return Ok(());
            }
            return Err(ShardCanisterCatalogInsertError::ShardAlreadyAttached);
        }
        if let Some(existing_shard) = self.by_canister.get(&canister) {
            if existing_shard == shard_id {
                return Ok(());
            }
            return Err(ShardCanisterCatalogInsertError::CanisterAlreadyAttached);
        }
        self.by_shard.insert(shard_id, canister);
        self.by_canister.insert(canister, shard_id);
        Ok(())
    }

    pub(crate) fn remove_shard(&mut self, shard_id: ShardId) -> Option<Principal> {
        let canister = self.by_shard.remove(&shard_id)?;
        self.by_canister.remove(&canister);
        Some(canister)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ShardCanisterCatalogInsertError {
    ShardAlreadyAttached,
    CanisterAlreadyAttached,
}

thread_local! {
    pub(crate) static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(default_memory_impl()));
}

pub(crate) fn init_router() -> StableRouterCell {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_INDEX_ROUTER)),
        Principal::anonymous(),
    )
}

pub(crate) fn init_shard_canister_catalog() -> ShardCanisterCatalog {
    ShardCanisterCatalog::init()
}

pub(crate) fn init_ownership_config() -> StableOwnershipConfigCell {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_INDEX_OWNERSHIP_CONFIG)),
        VectorIndexOwnershipConfig::default(),
    )
}

pub(crate) fn init_defs() -> StableDefsMap {
    let memory = MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_INDEX_DEFS));
    match StableClusteredHashMap::init(memory.clone()) {
        Ok(map) => map,
        // Empty (fresh) memory has no `CHM` magic; create the map like `BTreeMap::init` does for a
        // fresh region. A non-zero wrong magic is genuine corruption.
        Err(InitError::BadMagic { actual: [0, 0, 0] }) => {
            StableClusteredHashMap::new(memory).expect("init defs")
        }
        Err(e) => panic!("init defs: {e}"),
    }
}

pub(crate) fn init_centroid_meta() -> StableCentroidMetaMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(IVF_CENTROID_META)))
}

pub(crate) fn init_centroids() -> StableCentroidsMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(IVF_CENTROIDS)))
}

pub(crate) fn init_subject_map() -> StableSubjectMap {
    let memory = MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_SUBJECT_TO_ID));
    match StableClusteredHashMap::init(memory.clone()) {
        Ok(map) => map,
        // Empty (fresh) memory has no `CHM` magic; create the map like `ic-stable-structures`
        // `BTreeMap::init` does for a fresh region. A non-zero wrong magic is genuine corruption.
        Err(InitError::BadMagic { actual: [0, 0, 0] }) => {
            StableClusteredHashMap::new(memory).expect("init subject map")
        }
        Err(e) => panic!("init subject map: {e}"),
    }
}

pub(crate) fn init_deleted_subjects() -> StableDeletedSubjectsMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_DELETED_SUBJECTS)))
}

pub(crate) fn init_partition_heads() -> StablePartitionHeadsMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_PARTITION_HEADS)))
}

pub(crate) fn init_page_meta() -> StablePageMetaMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_PAGE_META)))
}

pub(crate) fn init_row_slab() -> Memory {
    MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_ROW_SLAB))
}

pub(crate) fn init_rebuild_state() -> StableRebuildStateMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_REBUILD_STATE)))
}

pub(crate) fn init_maintenance_state() -> StableMaintenanceStateMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_MAINTENANCE_STATE)))
}

pub(crate) fn init_shard_watermarks() -> StableShardWatermarksMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_SHARD_WATERMARKS)))
}

pub(crate) fn init_gc_cursor() -> StableGcCursorCell {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_GC_CURSOR)),
        None,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use ic_stable_structures::Storable;

    /// The pre-Slice-4 ownership record carried property-index group fields. The vector target is
    /// now graph-scoped, so the struct dropped them. This guards the stable-layout change: old
    /// `{ initialized, graph_id, index_group_size, group_index }` bytes must still decode (Candid
    /// ignores the surplus trailing fields), keeping the only meaningful field, `graph_id`.
    #[derive(CandidType, Serialize)]
    struct LegacyVectorIndexOwnershipConfig {
        initialized: bool,
        graph_id: GraphId,
        index_group_size: u32,
        group_index: u32,
    }

    #[test]
    fn decodes_legacy_ownership_bytes_dropping_group_fields() {
        let legacy = LegacyVectorIndexOwnershipConfig {
            initialized: true,
            graph_id: GraphId::from_raw(7),
            index_group_size: 4,
            group_index: 3,
        };
        let bytes = Encode!(&legacy).expect("encode legacy ownership config");
        let decoded = VectorIndexOwnershipConfig::from_bytes(Cow::Owned(bytes));
        assert_eq!(
            decoded,
            VectorIndexOwnershipConfig {
                initialized: true,
                graph_id: GraphId::from_raw(7),
            }
        );
    }
}
