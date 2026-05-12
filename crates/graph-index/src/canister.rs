//! Canister request handlers for `gleaph-graph-index`.
//! `#[init]` / `#[update]` entrypoints live in `lib.rs`.

use crate::facade::IndexStore;
use crate::init::IndexInitArgs;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest};
use ic_cdk::api::msg_caller;

fn trap_err(e: IndexError) {
    ic_cdk::trap(e.to_string());
}

pub(crate) fn init(args: IndexInitArgs) {
    IndexStore::new().init_from_args(&args);
}

pub(crate) fn admin_register_shard(shard_id: u64, shard_principal: Principal) {
    let caller = msg_caller();
    if let Err(e) = IndexStore::new().admin_register_shard(caller, shard_id, shard_principal) {
        trap_err(e);
    }
}

pub(crate) fn posting_insert(shard_id: u64, property_id: u32, value: Vec<u8>, vertex_id: u32) {
    let caller = msg_caller();
    if let Err(e) =
        IndexStore::new().posting_insert(caller, shard_id, property_id, value, vertex_id)
    {
        trap_err(e);
    }
}

pub(crate) fn posting_remove(shard_id: u64, property_id: u32, value: Vec<u8>, vertex_id: u32) {
    let caller = msg_caller();
    if let Err(e) =
        IndexStore::new().posting_remove(caller, shard_id, property_id, value, vertex_id)
    {
        trap_err(e);
    }
}

pub(crate) fn lookup_equal(property_id: u32, value: Vec<u8>) -> Vec<PostingHit> {
    IndexStore::new().lookup_equal(property_id, &value)
}

pub(crate) fn lookup_range(property_id: u32, req: PostingRangeRequest) -> Vec<PostingHit> {
    IndexStore::new().lookup_range(property_id, &req)
}

pub(crate) fn resolve_shard_principal(shard_id: u64) -> Option<Principal> {
    IndexStore::new().resolve_shard_principal(shard_id)
}
