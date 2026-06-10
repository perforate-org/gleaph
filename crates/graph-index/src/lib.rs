//! Federated property index canister (`gleaph-graph-index`).
//!
//! Owns global postings `(property_id, value, shard_id, vertex_id)`. Shard ownership is configured
//! by the router via `admin_set_shard_owner`.
//!
//! ## Read API visibility (v1)
//!
//! `lookup_equal` and `lookup_range` perform **no caller-based authorization**: any principal that
//! can reach this canister may read the full posting directory. Treat that as public metadata unless
//! you gate the canister at a higher layer.
//!
//! `lookup_range` uses the same lexicographic order on encoded value bytes as `lookup_equal` (`memcmp`).

mod facade;
mod key;
mod label_key;
mod label_range;
mod posting_range;
pub mod state;

pub mod init;

mod canister;

pub use facade::IndexStore;
pub use gleaph_graph_kernel::index::{
    IndexEqualSpec, IndexIntersectionRequest, PostingHit, PostingRangeRequest, ValuePostingCount,
};
pub use init::IndexInitArgs;
pub use key::PostingKey;
pub use label_key::LabelPostingKey;
pub use state::IndexError;

use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use ic_cdk_macros::{init, query, update};

#[init]
fn init(args: IndexInitArgs) {
    canister::init(args);
}

#[update]
fn admin_set_shard_owner(shard_id: ShardId, owner_principal: Principal) -> Result<(), String> {
    canister::admin_set_shard_owner(shard_id, owner_principal)
}

#[update]
fn admin_clear_shard_owner(shard_id: ShardId) -> Result<(), String> {
    canister::admin_clear_shard_owner(shard_id)
}

#[update]
fn posting_insert(shard_id: ShardId, property_id: u32, value: Vec<u8>, vertex_id: u32) {
    canister::posting_insert(shard_id, property_id, value, vertex_id);
}

#[update]
fn posting_remove(shard_id: ShardId, property_id: u32, value: Vec<u8>, vertex_id: u32) {
    canister::posting_remove(shard_id, property_id, value, vertex_id);
}

#[update]
fn label_posting_insert(shard_id: ShardId, vertex_label_id: u32, vertex_id: u32) {
    canister::label_posting_insert(shard_id, vertex_label_id, vertex_id);
}

#[update]
fn label_posting_remove(shard_id: ShardId, vertex_label_id: u32, vertex_id: u32) {
    canister::label_posting_remove(shard_id, vertex_label_id, vertex_id);
}

#[query]
fn lookup_equal(property_id: u32, value: Vec<u8>) -> Vec<PostingHit> {
    canister::lookup_equal(property_id, value)
}

#[query]
fn lookup_label(vertex_label_id: u32) -> Vec<PostingHit> {
    canister::lookup_label(vertex_label_id)
}

#[query]
fn lookup_intersection(req: IndexIntersectionRequest) -> Vec<PostingHit> {
    canister::lookup_intersection(req)
}

#[query]
fn lookup_range(property_id: u32, req: PostingRangeRequest) -> Vec<PostingHit> {
    canister::lookup_range(property_id, req)
}

#[query]
fn count_postings_by_value(
    property_id: u32,
    min_count: u64,
    vertex_filter_packed: Option<Vec<u64>>,
) -> Vec<ValuePostingCount> {
    canister::count_postings_by_value(property_id, min_count, vertex_filter_packed)
}

#[query]
fn filter_hits_by_label(vertex_label_id: u32, hits: Vec<PostingHit>) -> Vec<PostingHit> {
    canister::filter_hits_by_label(vertex_label_id, hits)
}

#[query]
fn count_postings_by_value_for_label(
    property_id: u32,
    vertex_label_id: u32,
    min_count: u64,
) -> Vec<ValuePostingCount> {
    canister::count_postings_by_value_for_label(property_id, vertex_label_id, min_count)
}
