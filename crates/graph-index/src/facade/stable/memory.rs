//! Graph-index canister stable-memory layout — see `design/storage/stable-memory-inventory.md`
//! and `facade/stable/layout.rs` (ADR 0007 registry).

use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::{BTreeMap, BTreeSet, Cell, DefaultMemoryImpl};
use std::cell::RefCell;

use crate::edge_key::EdgePostingKey;
use crate::key::PostingKey;
use crate::label_key::LabelPostingKey;

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

const INDEX_ADMINS: MemoryId = MemoryId::new(0);
const INDEX_SHARD_OWNERS: MemoryId = MemoryId::new(1);
const INDEX_POSTINGS: MemoryId = MemoryId::new(2);
const INDEX_ROUTER: MemoryId = MemoryId::new(3);
const INDEX_LABEL_POSTINGS: MemoryId = MemoryId::new(4);
const INDEX_EDGE_POSTINGS: MemoryId = MemoryId::new(5);

pub(crate) type StableIndexAdminSet = BTreeSet<Principal, Memory>;
pub(crate) type StableIndexShardOwnerMap = BTreeMap<ShardId, Principal, Memory>;
pub(crate) type StableIndexPostingSet = BTreeSet<PostingKey, Memory>;
pub(crate) type StableIndexLabelPostingSet = BTreeSet<LabelPostingKey, Memory>;
pub(crate) type StableIndexEdgePostingSet = BTreeSet<EdgePostingKey, Memory>;
pub(crate) type StableIndexRouterCell = Cell<Principal, Memory>;

thread_local! {
    pub(crate) static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
}

pub(crate) fn init_index_admins() -> StableIndexAdminSet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_ADMINS)))
}

pub(crate) fn init_index_shard_owners() -> StableIndexShardOwnerMap {
    BTreeMap::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_SHARD_OWNERS)))
}

pub(crate) fn init_index_postings() -> StableIndexPostingSet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_POSTINGS)))
}

pub(crate) fn init_index_label_postings() -> StableIndexLabelPostingSet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_LABEL_POSTINGS)))
}

pub(crate) fn init_index_edge_postings() -> StableIndexEdgePostingSet {
    BTreeSet::init(MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_EDGE_POSTINGS)))
}

pub(crate) fn init_index_router() -> StableIndexRouterCell {
    Cell::init(
        MEMORY_MANAGER.with(|m| m.borrow().get(INDEX_ROUTER)),
        Principal::anonymous(),
    )
}
