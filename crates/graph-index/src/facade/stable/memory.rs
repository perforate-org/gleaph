//! Graph-index canister stable-memory layout — see `design/storage/stable-memory-inventory.md`
//! and `facade/stable/layout.rs` (ADR 0007 registry).
//!
//! MemoryIds: router authorization → shard catalog → ownership config → derived postings.

use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::ShardId;
use ic_stable_structures::memory_manager::MemoryId;
use ic_stable_structures::{BTreeMap, BTreeSet, Cell, DefaultMemoryImpl};
use ic_stable_variable_memory_manager::{
    MemoryManager as VariableMemoryManager, VirtualMemory as VariableVirtualMemory,
};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;

use crate::edge_key::EdgePostingKey;
use crate::key::PostingKey;
use crate::label_key::LabelPostingKey;

pub(crate) type Memory = VariableVirtualMemory<DefaultMemoryImpl>;

const INDEX_ROUTER: MemoryId = MemoryId::new(0);
const INDEX_SHARD_CANISTER_BY_SHARD: MemoryId = MemoryId::new(1);
const INDEX_SHARD_BY_CANISTER: MemoryId = MemoryId::new(2);
const INDEX_OWNERSHIP_CONFIG: MemoryId = MemoryId::new(3);
const INDEX_VERTEX_POSTINGS: MemoryId = MemoryId::new(4);
const INDEX_VERTEX_LABEL_POSTINGS: MemoryId = MemoryId::new(5);
const INDEX_EDGE_POSTINGS: MemoryId = MemoryId::new(6);

const INDEX_METADATA_BUCKET_PAGES: u16 = 4;
const INDEX_LABEL_POSTINGS_BUCKET_PAGES: u16 = 32;
const INDEX_PROPERTY_POSTINGS_BUCKET_PAGES: u16 = 128;

pub(crate) type StableIndexRouterCell = Cell<Principal, Memory>;
pub(crate) type StableIndexOwnershipConfigCell = Cell<IndexOwnershipConfig, Memory>;
pub(crate) type StableIndexShardCanisterByShardMap = BTreeMap<ShardId, Principal, Memory>;
pub(crate) type StableIndexShardByCanisterMap = BTreeMap<Principal, ShardId, Memory>;
pub(crate) type StableIndexVertexPostingSet = BTreeSet<PostingKey, Memory>;
pub(crate) type StableIndexVertexLabelPostingSet = BTreeSet<LabelPostingKey, Memory>;
pub(crate) type StableIndexEdgePostingSet = BTreeSet<EdgePostingKey, Memory>;

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub(crate) struct IndexOwnershipConfig {
    pub initialized: bool,
    pub graph_id: GraphId,
    pub index_group_size: u32,
    pub group_index: u32,
}

impl Default for IndexOwnershipConfig {
    fn default() -> Self {
        Self {
            initialized: false,
            graph_id: GraphId::from_raw(0),
            index_group_size: 1,
            group_index: 0,
        }
    }
}

impl ic_stable_structures::Storable for IndexOwnershipConfig {
    const BOUND: ic_stable_structures::storable::Bound =
        ic_stable_structures::storable::Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode IndexOwnershipConfig"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode IndexOwnershipConfig")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), IndexOwnershipConfig).expect("decode IndexOwnershipConfig")
    }
}

pub(crate) struct ShardCanisterCatalog {
    by_shard: StableIndexShardCanisterByShardMap,
    by_canister: StableIndexShardByCanisterMap,
}

impl ShardCanisterCatalog {
    pub(crate) fn init() -> Self {
        Self {
            by_shard: BTreeMap::init(
                MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_SHARD_CANISTER_BY_SHARD)),
            ),
            by_canister: BTreeMap::init(
                MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_SHARD_BY_CANISTER)),
            ),
        }
    }

    pub(crate) fn clear_new(&mut self) {
        self.by_shard.clear_new();
        self.by_canister.clear_new();
    }

    pub(crate) fn shard_canister(&self, shard_id: ShardId) -> Option<Principal> {
        self.by_shard.get(&shard_id)
    }

    #[allow(dead_code)]
    pub(crate) fn is_attached_shard_canister(&self, canister: Principal) -> bool {
        self.by_canister.contains_key(&canister)
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
            return Err(ShardCanisterCatalogInsertError::ShardAlreadyAttached {
                shard_id,
                existing_canister,
            });
        }
        if let Some(existing_shard) = self.by_canister.get(&canister) {
            if existing_shard == shard_id {
                return Ok(());
            }
            return Err(ShardCanisterCatalogInsertError::CanisterAlreadyAttached {
                canister,
                existing_shard,
            });
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
    ShardAlreadyAttached {
        shard_id: ShardId,
        existing_canister: Principal,
    },
    CanisterAlreadyAttached {
        canister: Principal,
        existing_shard: ShardId,
    },
}

thread_local! {
    pub(crate) static MEMORY_MANAGER: RefCell<VariableMemoryManager<DefaultMemoryImpl>> =
        RefCell::new(VariableMemoryManager::init_with_policies(
            DefaultMemoryImpl::default(),
            INDEX_METADATA_BUCKET_PAGES,
            &[
                (INDEX_ROUTER, INDEX_METADATA_BUCKET_PAGES),
                (
                    INDEX_SHARD_CANISTER_BY_SHARD,
                    INDEX_METADATA_BUCKET_PAGES,
                ),
                (INDEX_SHARD_BY_CANISTER, INDEX_METADATA_BUCKET_PAGES),
                (INDEX_OWNERSHIP_CONFIG, INDEX_METADATA_BUCKET_PAGES),
                (INDEX_VERTEX_POSTINGS, INDEX_PROPERTY_POSTINGS_BUCKET_PAGES),
                (
                    INDEX_VERTEX_LABEL_POSTINGS,
                    INDEX_LABEL_POSTINGS_BUCKET_PAGES,
                ),
                (INDEX_EDGE_POSTINGS, INDEX_PROPERTY_POSTINGS_BUCKET_PAGES),
            ],
        ));
}

pub(crate) fn init_index_router() -> StableIndexRouterCell {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_ROUTER)),
        Principal::anonymous(),
    )
}

pub(crate) fn init_index_shard_canister_catalog() -> ShardCanisterCatalog {
    ShardCanisterCatalog::init()
}

pub(crate) fn init_index_ownership_config() -> StableIndexOwnershipConfigCell {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_OWNERSHIP_CONFIG)),
        IndexOwnershipConfig::default(),
    )
}

pub(crate) fn init_index_vertex_postings() -> StableIndexVertexPostingSet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_VERTEX_POSTINGS)))
}

pub(crate) fn init_index_vertex_label_postings() -> StableIndexVertexLabelPostingSet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_VERTEX_LABEL_POSTINGS)))
}

pub(crate) fn init_index_edge_postings() -> StableIndexEdgePostingSet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_EDGE_POSTINGS)))
}
