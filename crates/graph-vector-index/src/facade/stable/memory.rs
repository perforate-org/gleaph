//! Graph-vector-index canister stable-memory layout — see `design/storage/stable-memory-inventory.md`
//! and `facade/stable/layout.rs` (ADR 0007 registry, ADR 0031 Slice 2).
//!
//! MemoryIds: router auth → shard catalog → ownership config → index defs → centroid meta →
//! reserved centroids → subject clock → id→slot → partition heads → pages.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{BTreeMap, Cell, DefaultMemoryImpl};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;

use crate::records::{
    IvfCentroidMeta, PageKey, PartitionHead, PartitionKey, SlotRef, SubjectKey, SubjectMapEntry,
    VectorIdKey, VectorIndexDef, VectorPage,
};

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
const VECTOR_ID_TO_SLOT: MemoryId = MemoryId::new(8);
const VECTOR_PARTITION_HEADS: MemoryId = MemoryId::new(9);
const VECTOR_PAGE: MemoryId = MemoryId::new(10);

pub(crate) type StableRouterCell = Cell<Principal, Memory>;
pub(crate) type StableOwnershipConfigCell = Cell<VectorIndexOwnershipConfig, Memory>;
pub(crate) type StableShardCanisterByShardMap = BTreeMap<ShardId, Principal, Memory>;
pub(crate) type StableShardByCanisterMap = BTreeMap<Principal, ShardId, Memory>;
pub(crate) type StableDefsMap = BTreeMap<u32, VectorIndexDef, Memory>;
pub(crate) type StableCentroidMetaMap = BTreeMap<u32, IvfCentroidMeta, Memory>;
pub(crate) type StableCentroidsMap = BTreeMap<PartitionKey, Vec<u8>, Memory>;
pub(crate) type StableSubjectMap = BTreeMap<SubjectKey, SubjectMapEntry, Memory>;
pub(crate) type StableIdToSlotMap = BTreeMap<VectorIdKey, SlotRef, Memory>;
pub(crate) type StablePartitionHeadsMap = BTreeMap<PartitionKey, PartitionHead, Memory>;
pub(crate) type StablePageMap = BTreeMap<PageKey, VectorPage, Memory>;

/// Graph/group ownership config — mirrors `graph-index` `IndexOwnershipConfig`.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct VectorIndexOwnershipConfig {
    pub initialized: bool,
    pub graph_id: GraphId,
    pub index_group_size: u32,
    pub group_index: u32,
}

impl Default for VectorIndexOwnershipConfig {
    fn default() -> Self {
        Self {
            initialized: false,
            graph_id: GraphId::from_raw(0),
            index_group_size: 1,
            group_index: 0,
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
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
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
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_INDEX_DEFS)))
}

pub(crate) fn init_centroid_meta() -> StableCentroidMetaMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(IVF_CENTROID_META)))
}

pub(crate) fn init_centroids() -> StableCentroidsMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(IVF_CENTROIDS)))
}

pub(crate) fn init_subject_map() -> StableSubjectMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_SUBJECT_TO_ID)))
}

pub(crate) fn init_id_to_slot() -> StableIdToSlotMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_ID_TO_SLOT)))
}

pub(crate) fn init_partition_heads() -> StablePartitionHeadsMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_PARTITION_HEADS)))
}

pub(crate) fn init_pages() -> StablePageMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(VECTOR_PAGE)))
}
