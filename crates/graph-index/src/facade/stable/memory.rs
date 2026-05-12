//! Stable-memory fragments for the federation index (admins, shard registry, postings).

use crate::key::PostingKey;
use candid::Principal;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{BTreeMap, BTreeSet, DefaultMemoryImpl};
use std::cell::RefCell;

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

const INDEX_ADMINS: MemoryId = MemoryId::new(0);
const INDEX_SHARDS: MemoryId = MemoryId::new(1);
const INDEX_POSTINGS: MemoryId = MemoryId::new(2);

pub(crate) type StableIndexAdminSet = BTreeSet<Principal, Memory>;
pub(crate) type StableIndexShardMap = BTreeMap<u64, Principal, Memory>;
pub(crate) type StableIndexPostingSet = BTreeSet<PostingKey, Memory>;

thread_local! {
    pub(crate) static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
}

pub(crate) fn init_index_admins() -> StableIndexAdminSet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_ADMINS)))
}

pub(crate) fn init_index_shards() -> StableIndexShardMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_SHARDS)))
}

pub(crate) fn init_index_postings() -> StableIndexPostingSet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_POSTINGS)))
}
