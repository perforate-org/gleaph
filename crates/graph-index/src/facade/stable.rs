//! Stable-memory-backed index fragments (admin set, shard owners, postings).

use std::cell::RefCell;

pub(crate) mod memory;

thread_local! {
    pub(crate) static INDEX_ADMINS: RefCell<memory::StableIndexAdminSet> =
        RefCell::new(memory::init_index_admins());

    pub(crate) static INDEX_SHARD_OWNERS: RefCell<memory::StableIndexShardOwnerMap> =
        RefCell::new(memory::init_index_shard_owners());

    pub(crate) static INDEX_POSTINGS: RefCell<memory::StableIndexPostingSet> =
        RefCell::new(memory::init_index_postings());

    pub(crate) static INDEX_ROUTER: RefCell<memory::StableIndexRouterCell> =
        RefCell::new(memory::init_index_router());
}
