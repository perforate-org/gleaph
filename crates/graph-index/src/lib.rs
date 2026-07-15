//! Federated property index canister (`gleaph-graph-index`).
//!
//! Owns global postings `(property_id, value, shard_id, vertex_id)`. Shard/canister attachments are
//! configured by the router via `admin_attach_shard_canister`.
//!
//! ## API visibility
//!
//! Read APIs are router-only (`guard_router_canister`). Posting sync updates call
//! `guard_shard_canister` at the canister entrypoint before dispatch. Admin APIs are router-only.
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
    EdgePostingCursor, EdgePostingHit, EdgePostingHitPage, IndexEqualSpec,
    IndexIntersectionRequest, IndexIntersectionResult, IndexLabelIntersectionRequest,
    IndexPostingBatchProgress, IndexPostingMutation, IndexSubject, LabelLookupPageRequest,
    LabelLookupPageResult, LabelPostingCursor, LookupEdgeEqualPageRequest,
    LookupEqualPageForLabelRequest, LookupEqualPageRequest, LookupIntersectionPageRequest,
    LookupRangeIntersectionPageRequest, LookupRangePageRequest, PostingHit, PostingHitPage,
    PostingRangeRequest, PropertyPostingCursor, ValuePostingCount,
};
pub use init::IndexInitArgs;
pub use key::PostingKey;
pub use label_key::LabelPostingKey;
pub use state::IndexError;

use crate::guards::{guard_router_canister, guard_shard_canister};
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{
    IndexPostingPurgeCursor, IndexPostingPurgeStepResult, IndexPurgeKind, ShardDetachCursor,
    ShardDetachStepResult, ShardId,
};
use ic_cdk_macros::{init, query, update};

fn guard_shard_canister_or_trap(shard_id: ShardId) {
    if let Err(e) = guard_shard_canister(shard_id) {
        ic_cdk::trap(&e);
    }
}

#[init]
fn init(args: IndexInitArgs) {
    canister::init(args);
}

#[update(guard = "guard_router_canister")]
fn admin_attach_shard_canister(
    graph_id: GraphId,
    index_group_size: u32,
    group_index: u32,
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    canister::admin_attach_shard_canister(
        graph_id,
        index_group_size,
        group_index,
        shard_id,
        shard_canister_principal,
    )
}

#[update(guard = "guard_router_canister")]
fn admin_detach_shard_canister(
    shard_id: ShardId,
    resume: Option<ShardDetachCursor>,
) -> Result<ShardDetachStepResult, String> {
    canister::admin_detach_shard_canister(shard_id, resume)
}

#[update(guard = "guard_router_canister")]
fn admin_purge_property_postings(
    kind: IndexPurgeKind,
    property_id: u32,
    label_id: u16,
    resume: Option<IndexPostingPurgeCursor>,
) -> Result<IndexPostingPurgeStepResult, String> {
    canister::admin_purge_property_postings(kind, property_id, label_id, resume)
}

#[update]
fn posting_insert(shard_id: ShardId, property_id: u32, value: Vec<u8>, vertex_id: u32) {
    guard_shard_canister_or_trap(shard_id);
    canister::posting_insert(shard_id, property_id, value, vertex_id);
}

#[update]
fn posting_remove(shard_id: ShardId, property_id: u32, value: Vec<u8>, vertex_id: u32) {
    guard_shard_canister_or_trap(shard_id);
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
    guard_shard_canister_or_trap(shard_id);
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
    guard_shard_canister_or_trap(shard_id);
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
    guard_shard_canister_or_trap(shard_id);
    canister::label_posting_insert(shard_id, vertex_label_id, vertex_id);
}

#[update]
fn label_posting_remove(shard_id: ShardId, vertex_label_id: u32, vertex_id: u32) {
    guard_shard_canister_or_trap(shard_id);
    canister::label_posting_remove(shard_id, vertex_label_id, vertex_id);
}

#[update]
fn posting_batch(
    shard_id: ShardId,
    operations: Vec<IndexPostingMutation>,
) -> IndexPostingBatchProgress {
    guard_shard_canister_or_trap(shard_id);
    match canister::posting_batch(shard_id, operations) {
        Ok(progress) => progress,
        Err(error) => ic_cdk::trap(&error),
    }
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
fn lookup_equal_page(req: LookupEqualPageRequest) -> PostingHitPage {
    canister::lookup_equal_page(req)
}

#[query(guard = "guard_router_canister")]
fn lookup_equal_page_for_label(req: LookupEqualPageForLabelRequest) -> PostingHitPage {
    canister::lookup_equal_page_for_label(req)
}

#[query(guard = "guard_router_canister")]
fn lookup_range_page(req: LookupRangePageRequest) -> PostingHitPage {
    canister::lookup_range_page(req)
}

#[query(guard = "guard_router_canister")]
fn lookup_edge_equal_page(req: LookupEdgeEqualPageRequest) -> EdgePostingHitPage {
    canister::lookup_edge_equal_page(req)
}

#[query(guard = "guard_router_canister")]
fn lookup_label(vertex_label_id: u32) -> Vec<PostingHit> {
    canister::lookup_label(vertex_label_id)
}

#[query(guard = "guard_router_canister")]
fn lookup_label_for_shard(vertex_label_id: u32, shard_id: ShardId) -> Vec<PostingHit> {
    canister::lookup_label_for_shard(vertex_label_id, shard_id)
}

#[query(guard = "guard_router_canister")]
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
fn lookup_intersection_page(req: LookupIntersectionPageRequest) -> PostingHitPage {
    canister::lookup_intersection_page(req)
}

#[query(guard = "guard_router_canister")]
fn lookup_range_intersection_page(req: LookupRangeIntersectionPageRequest) -> PostingHitPage {
    canister::lookup_range_intersection_page(req)
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
