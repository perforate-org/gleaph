//! Stable-memory-backed index fragments (admin set, shard registry, postings).
//!
//! Module visibility is `pub(crate)` within [`super`] (see [`crate::facade::store`]): only facade
//! code references these thread-locals directly.

use std::cell::RefCell;

pub(crate) mod memory;

thread_local! {
    pub(crate) static INDEX_ADMINS: RefCell<memory::StableIndexAdminSet> =
        RefCell::new(memory::init_index_admins());

    pub(crate) static INDEX_SHARDS: RefCell<memory::StableIndexShardMap> =
        RefCell::new(memory::init_index_shards());

    pub(crate) static INDEX_POSTINGS: RefCell<memory::StableIndexPostingSet> =
        RefCell::new(memory::init_index_postings());
}
