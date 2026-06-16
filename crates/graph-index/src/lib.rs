//! Federated property index canister (`gleaph-graph-index`).
//!
//! Owns global postings `(property_id, value, shard_id, vertex_id)`. Shard/canister attachments are
//! configured by the router via `admin_attach_shard_canister`.
//!
//! ## Read API visibility
//!
//! Public read APIs are guarded for federation control-plane callers:
//! - Router-only for global scans/lookups.
//! - Router or owning graph shard for shard-scoped label exports.
//!
//! `lookup_range` uses the same lexicographic order on encoded value bytes as `lookup_equal` (`memcmp`).

#[cfg(feature = "canbench")]
mod bench;

mod edge_key;
mod facade;
mod key;
mod label_key;
mod label_range;
mod posting_range;
pub mod state;

pub mod init;

mod canister;
mod guards;

pub use edge_key::EdgePostingKey;
pub use facade::IndexStore;
pub use gleaph_graph_kernel::index::{
    EdgePostingHit, IndexEqualSpec, IndexIntersectionRequest, IndexIntersectionResult,
    IndexLabelIntersectionRequest, IndexSubject, LabelLookupPageRequest, LabelLookupPageResult,
    LabelPostingCursor, PostingHit, PostingRangeRequest, ValuePostingCount,
};
pub use init::IndexInitArgs;
pub use key::PostingKey;
pub use label_key::LabelPostingKey;
pub use state::IndexError;

use crate::guards::guard_router_canister;
use candid::Principal;
use gleaph_graph_kernel::federation::ShardId;
use ic_cdk_macros::{init, query, update};

#[init]
fn init(args: IndexInitArgs) {
    canister::init(args);
}

#[update]
fn admin_attach_shard_canister(
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    canister::admin_attach_shard_canister(shard_id, shard_canister_principal)
}

#[update]
fn admin_detach_shard_canister(shard_id: ShardId) -> Result<(), String> {
    canister::admin_detach_shard_canister(shard_id)
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
fn edge_posting_insert(
    shard_id: ShardId,
    property_id: u32,
    value: Vec<u8>,
    label_id: u16,
    owner_vertex_id: u32,
    slot_index: u32,
) {
    canister::edge_posting_insert(
        shard_id,
        property_id,
        value,
        label_id,
        owner_vertex_id,
        slot_index,
    );
}

#[update]
fn edge_posting_remove(
    shard_id: ShardId,
    property_id: u32,
    value: Vec<u8>,
    label_id: u16,
    owner_vertex_id: u32,
    slot_index: u32,
) {
    canister::edge_posting_remove(
        shard_id,
        property_id,
        value,
        label_id,
        owner_vertex_id,
        slot_index,
    );
}

#[update]
fn label_posting_insert(shard_id: ShardId, vertex_label_id: u32, vertex_id: u32) {
    canister::label_posting_insert(shard_id, vertex_label_id, vertex_id);
}

#[update]
fn label_posting_remove(shard_id: ShardId, vertex_label_id: u32, vertex_id: u32) {
    canister::label_posting_remove(shard_id, vertex_label_id, vertex_id);
}

#[query(guard = "guard_router_canister")]
fn lookup_equal(property_id: u32, value: Vec<u8>) -> Vec<PostingHit> {
    canister::lookup_equal(property_id, value)
}

#[query(guard = "guard_router_canister")]
fn lookup_edge_equal(
    property_id: u32,
    value: Vec<u8>,
    label_id: Option<u16>,
) -> Vec<EdgePostingHit> {
    canister::lookup_edge_equal(property_id, value, label_id)
}

#[query(guard = "guard_router_canister")]
fn lookup_label(vertex_label_id: u32) -> Vec<PostingHit> {
    canister::lookup_label(vertex_label_id)
}

#[query]
fn lookup_label_for_shard(vertex_label_id: u32, shard_id: ShardId) -> Vec<PostingHit> {
    canister::lookup_label_for_shard(vertex_label_id, shard_id)
}

#[query]
fn lookup_label_page(req: LabelLookupPageRequest) -> LabelLookupPageResult {
    canister::lookup_label_page(req)
}

#[query(guard = "guard_router_canister")]
fn lookup_intersection(req: IndexIntersectionRequest) -> IndexIntersectionResult {
    canister::lookup_intersection(req)
}

#[query(guard = "guard_router_canister")]
fn lookup_label_intersection(req: IndexLabelIntersectionRequest) -> Vec<PostingHit> {
    canister::lookup_label_intersection(req)
}

#[query(guard = "guard_router_canister")]
fn lookup_range(property_id: u32, req: PostingRangeRequest) -> Vec<PostingHit> {
    canister::lookup_range(property_id, req)
}

#[query(guard = "guard_router_canister")]
fn count_postings_by_value(
    property_id: u32,
    min_count: u64,
    vertex_filter_packed: Option<Vec<u64>>,
) -> Vec<ValuePostingCount> {
    canister::count_postings_by_value(property_id, min_count, vertex_filter_packed)
}

#[query(guard = "guard_router_canister")]
fn filter_hits_by_label(vertex_label_id: u32, hits: Vec<PostingHit>) -> Vec<PostingHit> {
    canister::filter_hits_by_label(vertex_label_id, hits)
}

#[query(guard = "guard_router_canister")]
fn count_postings_by_value_for_label(
    property_id: u32,
    vertex_label_id: u32,
    min_count: u64,
) -> Vec<ValuePostingCount> {
    canister::count_postings_by_value_for_label(property_id, vertex_label_id, min_count)
}

ic_cdk::export_candid!();
