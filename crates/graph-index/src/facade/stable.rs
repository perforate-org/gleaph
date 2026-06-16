//! Stable-memory-backed index fragments (router, shard/canister catalog, postings).

use std::cell::RefCell;

pub(crate) mod layout;
pub(crate) mod memory;

thread_local! {
    pub(crate) static INDEX_ROUTER: RefCell<memory::StableIndexRouterCell> =
        RefCell::new(memory::init_index_router());

    pub(crate) static INDEX_SHARD_CANISTER_CATALOG: RefCell<memory::ShardCanisterCatalog> =
        RefCell::new(memory::init_index_shard_canister_catalog());

    pub(crate) static INDEX_POSTINGS: RefCell<memory::StableIndexPostingSet> =
        RefCell::new(memory::init_index_postings());

    pub(crate) static INDEX_LABEL_POSTINGS: RefCell<memory::StableIndexLabelPostingSet> =
        RefCell::new(memory::init_index_label_postings());

    pub(crate) static INDEX_EDGE_POSTINGS: RefCell<memory::StableIndexEdgePostingSet> =
        RefCell::new(memory::init_index_edge_postings());
}
