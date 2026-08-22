//! Federated property index canister (`gleaph-graph-index`).
//!
//! Owns global postings `(physical_index_id, property_id, value, shard_id, vertex_id)`.
//! Shard/canister attachments are configured by the router via `admin_attach_shard_canister`.
//!
//! ## API visibility
//!
//! Read APIs accept the configured router or any graph shard attached to this index canister
//! (`guard_router_or_attached_shard_canister`). Posting sync updates call `guard_shard_canister`
//! at the canister entrypoint before dispatch. Admin APIs are router-only.
//!
//! `lookup_range` uses the same lexicographic order on encoded value bytes as `lookup_equal` (`memcmp`).

#[cfg(feature = "canbench")]
mod bench;

mod build_key;
mod edge_key;
mod facade;
mod key;
mod label_key;
mod label_range;
mod posting_range;
pub mod state;
mod worker;

pub mod init;

mod canister;
mod guards;

pub use edge_key::EdgePostingKey;
pub use facade::IndexStore;
pub use gleaph_graph_kernel::index::{
    EdgePostingCursor, EdgePostingHit, EdgePostingHitPage, IndexBuildCleanupStatus,
    IndexBuildControlRequest, IndexBuildDmlRequest, IndexBuildError, IndexBuildPhase,
    IndexBuildProgress, IndexBuildSealRequest, IndexBuildSealStatus, IndexBuildSealTarget,
    IndexBuildSeedDisposition, IndexBuildSeedPageRequest, IndexBuildSeedPageResult,
    IndexBuildShardWatermark, IndexBuildStatus, IndexBuildSubject, IndexBuildTarget,
    IndexEqualSpec, IndexLabelIntersectionRequest, IndexPostingBatchProgress, IndexPostingMutation,
    IndexSubject, LabelIntersectionPageRequest, LabelLookupPageRequest, LabelLookupPageResult,
    LabelPostingCursor, LookupEdgeEqualBatchRequest, LookupEdgeEqualBatchResult,
    LookupEdgeEqualPageRequest, LookupEdgeRangePageRequest, LookupEqualBatchRequest,
    LookupEqualBatchResult, LookupEqualPageForLabelRequest, LookupEqualPageRequest,
    LookupIntersectionPageForLabelRequest, LookupIntersectionPageRequest,
    LookupPropertyIntersectionPageRequest, LookupRangeIntersectionPageForLabelRequest,
    LookupRangeIntersectionPageRequest, LookupRangePageForLabelRequest, LookupRangePageRequest,
    LookupValuePostingCountPageRequest, PhysicalIndexId, PostingHit, PostingHitPage,
    PostingRangeRequest, PropertyIntersectionPage, PropertyPostingCursor,
    RegisterIndexBuildRequest, ValuePostingCountCursor, ValuePostingCountPage,
};
pub use init::IndexInitArgs;
pub use key::PostingKey;
pub use label_key::LabelPostingKey;
pub use state::IndexError;

use crate::guards::{
    guard_router_canister, guard_router_or_attached_shard_canister, guard_shard_canister,
};
use candid::{Encode, Principal};
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
    physical_index_id: PhysicalIndexId,
    kind: IndexPurgeKind,
    property_id: u32,
    label_id: u16,
    resume: Option<IndexPostingPurgeCursor>,
) -> Result<IndexPostingPurgeStepResult, String> {
    canister::admin_purge_property_postings(physical_index_id, kind, property_id, label_id, resume)
}

#[update(guard = "guard_router_canister")]
fn register_index_build(
    request: RegisterIndexBuildRequest,
) -> Result<IndexBuildStatus, IndexBuildError> {
    canister::register_index_build(request)
}

#[query(guard = "guard_router_canister")]
fn index_build_status(
    physical_index_id: PhysicalIndexId,
) -> Result<IndexBuildStatus, IndexBuildError> {
    canister::index_build_status(physical_index_id)
}

#[update(guard = "guard_router_canister")]
async fn advance_index_build(
    request: IndexBuildControlRequest,
) -> Result<IndexBuildStatus, IndexBuildError> {
    canister::advance_index_build(request).await
}

#[update(guard = "guard_router_canister")]
fn seal_index_build(
    request: IndexBuildSealRequest,
) -> Result<IndexBuildSealStatus, IndexBuildError> {
    canister::seal_index_build(request)
}

#[update(guard = "guard_router_canister")]
fn abort_index_build(
    request: IndexBuildControlRequest,
) -> Result<IndexBuildCleanupStatus, IndexBuildError> {
    canister::abort_index_build(request)
}

#[update]
fn apply_index_build_dml(request: IndexBuildDmlRequest) -> Result<(), IndexBuildError> {
    guard_shard_canister_or_trap(request.subject.shard_id());
    canister::apply_index_build_dml(request)
}

#[update]
fn posting_insert(
    shard_id: ShardId,
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    value: Vec<u8>,
    vertex_id: u32,
) {
    guard_shard_canister_or_trap(shard_id);
    canister::posting_insert(shard_id, physical_index_id, property_id, value, vertex_id);
}

#[update]
fn posting_remove(
    shard_id: ShardId,
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    value: Vec<u8>,
    vertex_id: u32,
) {
    guard_shard_canister_or_trap(shard_id);
    canister::posting_remove(shard_id, physical_index_id, property_id, value, vertex_id);
}

#[update]
fn edge_posting_insert(
    shard_id: ShardId,
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    value: Vec<u8>,
    label_id: u16,
    owner_vertex_id: u32,
    slot_index: u32,
) {
    guard_shard_canister_or_trap(shard_id);
    canister::edge_posting_insert(
        shard_id,
        physical_index_id,
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
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    value: Vec<u8>,
    label_id: u16,
    owner_vertex_id: u32,
    slot_index: u32,
) {
    guard_shard_canister_or_trap(shard_id);
    canister::edge_posting_remove(
        shard_id,
        physical_index_id,
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
    let request_bytes = Encode!(&(shard_id, &operations)).unwrap_or_else(|error| {
        ic_cdk::trap(format!("posting_batch request encode failed: {error}"));
    });
    if request_bytes.len() > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES {
        ic_cdk::trap(format!(
            "posting_batch request exceeds the safe payload limit of {} bytes",
            gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        ));
    }
    match canister::posting_batch(shard_id, operations) {
        Ok(progress) => progress,
        Err(error) => ic_cdk::trap(&error),
    }
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_equal(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    value: Vec<u8>,
) -> Vec<PostingHit> {
    canister::lookup_equal(physical_index_id, property_id, value)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_edge_equal(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    value: Vec<u8>,
    label_id: Option<u16>,
) -> Vec<EdgePostingHit> {
    canister::lookup_edge_equal(physical_index_id, property_id, value, label_id)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_equal_page(req: LookupEqualPageRequest) -> PostingHitPage {
    canister::lookup_equal_page(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_equal_batch(req: LookupEqualBatchRequest) -> LookupEqualBatchResult {
    canister::lookup_equal_batch(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_equal_page_for_label(req: LookupEqualPageForLabelRequest) -> PostingHitPage {
    canister::lookup_equal_page_for_label(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_range_page(req: LookupRangePageRequest) -> PostingHitPage {
    canister::lookup_range_page(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_range_page_for_label(req: LookupRangePageForLabelRequest) -> PostingHitPage {
    canister::lookup_range_page_for_label(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_edge_equal_batch(req: LookupEdgeEqualBatchRequest) -> LookupEdgeEqualBatchResult {
    canister::lookup_edge_equal_batch(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_edge_equal_page(req: LookupEdgeEqualPageRequest) -> EdgePostingHitPage {
    canister::lookup_edge_equal_page(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_edge_range_page(req: LookupEdgeRangePageRequest) -> EdgePostingHitPage {
    canister::lookup_edge_range_page(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_label(vertex_label_id: u32) -> Vec<PostingHit> {
    canister::lookup_label(vertex_label_id)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_label_for_shard(vertex_label_id: u32, shard_id: ShardId) -> Vec<PostingHit> {
    canister::lookup_label_for_shard(vertex_label_id, shard_id)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_label_page(req: LabelLookupPageRequest) -> LabelLookupPageResult {
    canister::lookup_label_page(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_label_intersection_page(req: LabelIntersectionPageRequest) -> LabelLookupPageResult {
    canister::lookup_label_intersection_page(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_label_intersection(req: IndexLabelIntersectionRequest) -> Vec<PostingHit> {
    canister::lookup_label_intersection(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_range(
    physical_index_id: PhysicalIndexId,
    property_id: u32,
    req: PostingRangeRequest,
) -> Vec<PostingHit> {
    canister::lookup_range(physical_index_id, property_id, req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn count_postings_by_value_page(req: LookupValuePostingCountPageRequest) -> ValuePostingCountPage {
    canister::count_postings_by_value_page(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn count_postings_by_value_for_label_page(
    req: LookupValuePostingCountPageRequest,
    vertex_label_id: u32,
) -> ValuePostingCountPage {
    canister::count_postings_by_value_for_label_page(req, vertex_label_id)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn filter_hits_by_label(vertex_label_id: u32, hits: Vec<PostingHit>) -> Vec<PostingHit> {
    canister::filter_hits_by_label(vertex_label_id, hits)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_intersection_page(req: LookupIntersectionPageRequest) -> PostingHitPage {
    canister::lookup_intersection_page(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_property_intersection_page(
    req: LookupPropertyIntersectionPageRequest,
) -> PropertyIntersectionPage {
    canister::lookup_property_intersection_page(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_intersection_page_for_label(
    req: LookupIntersectionPageForLabelRequest,
) -> PostingHitPage {
    canister::lookup_intersection_page_for_label(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_range_intersection_page(req: LookupRangeIntersectionPageRequest) -> PostingHitPage {
    canister::lookup_range_intersection_page(req)
}

#[query(guard = "guard_router_or_attached_shard_canister")]
fn lookup_range_intersection_page_for_label(
    req: LookupRangeIntersectionPageForLabelRequest,
) -> PostingHitPage {
    canister::lookup_range_intersection_page_for_label(req)
}

ic_cdk::export_candid!();
