//! Canister request handlers for `gleaph-graph-index`.

use crate::facade::IndexStore;
use crate::init::IndexInitArgs;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest, ValuePostingCount};
use ic_cdk::api::msg_caller;

fn trap_err(e: IndexError) {
    ic_cdk::trap(e.to_string());
}

pub(crate) fn init(args: IndexInitArgs) {
    IndexStore::new().init_from_args(&args);
}

pub(crate) fn admin_set_shard_owner(
    shard_id: ShardId,
    owner_principal: Principal,
) -> Result<(), String> {
    IndexStore::new()
        .admin_set_shard_owner(msg_caller(), shard_id, owner_principal)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_clear_shard_owner(shard_id: ShardId) -> Result<(), String> {
    IndexStore::new()
        .admin_clear_shard_owner(msg_caller(), shard_id)
        .map_err(|e| e.to_string())
}

pub(crate) fn posting_insert(shard_id: ShardId, property_id: u32, value: Vec<u8>, vertex_id: u32) {
    let caller = msg_caller();
    if let Err(e) =
        IndexStore::new().posting_insert(caller, shard_id, property_id, value, vertex_id)
    {
        trap_err(e);
    }
}

pub(crate) fn posting_remove(shard_id: ShardId, property_id: u32, value: Vec<u8>, vertex_id: u32) {
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

pub(crate) fn lookup_intersection(
    req: gleaph_graph_kernel::index::IndexIntersectionRequest,
) -> Vec<PostingHit> {
    IndexStore::new().lookup_intersection(&req)
}

pub(crate) fn lookup_range(property_id: u32, req: PostingRangeRequest) -> Vec<PostingHit> {
    IndexStore::new().lookup_range(property_id, &req)
}

pub(crate) fn count_postings_by_value(property_id: u32, min_count: u64) -> Vec<ValuePostingCount> {
    const MAX_GROUPS: usize = 10_000;

    IndexStore::new().count_postings_by_value(property_id, min_count, MAX_GROUPS)
}
