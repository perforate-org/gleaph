//! Canister request handlers for `gleaph-graph-index`.

use crate::facade::{DEFAULT_COUNT_POSTINGS_MAX_GROUPS, IndexStore};
use crate::init::IndexInitArgs;
use crate::state::IndexError;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{
    IndexPostingPurgeCursor, IndexPostingPurgeStepResult, IndexPurgeKind, ShardDetachCursor,
    ShardDetachStepResult, ShardId,
};
use gleaph_graph_kernel::index::{
    EdgePostingHit, EdgePostingHitPage, IndexIntersectionResult, LookupEdgeEqualPageRequest,
    LookupEqualPageRequest, LookupIntersectionPageRequest, LookupRangeIntersectionPageRequest,
    LookupRangePageRequest, PostingHit, PostingHitPage, PostingRangeRequest, ValuePostingCount,
};
use ic_cdk::api::msg_caller;

fn trap_err(e: IndexError) {
    ic_cdk::trap(e.to_string());
}

pub(crate) fn init(args: IndexInitArgs) {
    if let Err(e) = IndexStore::new().init_from_args(&args) {
        trap_err(e);
    }
}

pub(crate) fn admin_attach_shard_canister(
    graph_id: GraphId,
    index_group_size: u32,
    group_index: u32,
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    IndexStore::new()
        .admin_attach_shard_canister(
            msg_caller(),
            graph_id,
            index_group_size,
            group_index,
            shard_id,
            shard_canister_principal,
        )
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_detach_shard_canister(
    shard_id: ShardId,
    resume: Option<ShardDetachCursor>,
) -> Result<ShardDetachStepResult, String> {
    IndexStore::new()
        .admin_detach_shard_canister(msg_caller(), shard_id, resume)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_purge_property_postings(
    kind: IndexPurgeKind,
    property_id: u32,
    label_id: u16,
    resume: Option<IndexPostingPurgeCursor>,
) -> Result<IndexPostingPurgeStepResult, String> {
    IndexStore::new()
        .admin_purge_property_postings(msg_caller(), kind, property_id, label_id, resume)
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

pub(crate) fn edge_posting_insert(
    shard_id: ShardId,
    property_id: u32,
    value: Vec<u8>,
    label_id: u16,
    owner_vertex_id: u32,
    slot_index: u32,
) {
    let caller = msg_caller();
    if let Err(e) = IndexStore::new().edge_posting_insert(
        caller,
        shard_id,
        property_id,
        value,
        label_id,
        owner_vertex_id,
        slot_index,
    ) {
        trap_err(e);
    }
}

pub(crate) fn edge_posting_remove(
    shard_id: ShardId,
    property_id: u32,
    value: Vec<u8>,
    label_id: u16,
    owner_vertex_id: u32,
    slot_index: u32,
) {
    let caller = msg_caller();
    if let Err(e) = IndexStore::new().edge_posting_remove(
        caller,
        shard_id,
        property_id,
        value,
        label_id,
        owner_vertex_id,
        slot_index,
    ) {
        trap_err(e);
    }
}

pub(crate) fn label_posting_insert(shard_id: ShardId, vertex_label_id: u32, vertex_id: u32) {
    let caller = msg_caller();
    if let Err(e) =
        IndexStore::new().label_posting_insert(caller, shard_id, vertex_label_id, vertex_id)
    {
        trap_err(e);
    }
}

pub(crate) fn label_posting_remove(shard_id: ShardId, vertex_label_id: u32, vertex_id: u32) {
    let caller = msg_caller();
    if let Err(e) =
        IndexStore::new().label_posting_remove(caller, shard_id, vertex_label_id, vertex_id)
    {
        trap_err(e);
    }
}

pub(crate) fn lookup_label(vertex_label_id: u32) -> Vec<PostingHit> {
    IndexStore::new().lookup_label(vertex_label_id)
}

pub(crate) fn lookup_label_for_shard(
    vertex_label_id: u32,
    shard_id: gleaph_graph_kernel::federation::ShardId,
) -> Vec<PostingHit> {
    IndexStore::new().lookup_label_for_shard(vertex_label_id, shard_id)
}

pub(crate) fn lookup_label_page(
    req: gleaph_graph_kernel::index::LabelLookupPageRequest,
) -> gleaph_graph_kernel::index::LabelLookupPageResult {
    IndexStore::new().lookup_label_page(&req)
}

pub(crate) fn lookup_equal(property_id: u32, value: Vec<u8>) -> Vec<PostingHit> {
    IndexStore::new()
        .lookup_equal(property_id, &value)
        .unwrap_or_else(|e| {
            trap_err(e);
            unreachable!()
        })
}

pub(crate) fn lookup_edge_equal(
    property_id: u32,
    value: Vec<u8>,
    label_id: Option<u16>,
) -> Vec<EdgePostingHit> {
    IndexStore::new()
        .lookup_edge_equal(property_id, &value, label_id)
        .unwrap_or_else(|e| {
            trap_err(e);
            unreachable!()
        })
}

pub(crate) fn lookup_equal_page(req: LookupEqualPageRequest) -> PostingHitPage {
    IndexStore::new()
        .lookup_equal_page(&req)
        .unwrap_or_else(|e| {
            trap_err(e);
            unreachable!()
        })
}

pub(crate) fn lookup_range_page(req: LookupRangePageRequest) -> PostingHitPage {
    IndexStore::new()
        .lookup_range_page(&req)
        .unwrap_or_else(|e| {
            trap_err(e);
            unreachable!()
        })
}

pub(crate) fn lookup_edge_equal_page(req: LookupEdgeEqualPageRequest) -> EdgePostingHitPage {
    IndexStore::new()
        .lookup_edge_equal_page(&req)
        .unwrap_or_else(|e| {
            trap_err(e);
            unreachable!()
        })
}

pub(crate) fn lookup_intersection(
    req: gleaph_graph_kernel::index::IndexIntersectionRequest,
) -> IndexIntersectionResult {
    IndexStore::new()
        .lookup_intersection(&req)
        .unwrap_or_else(|e| {
            trap_err(e);
            unreachable!()
        })
}

pub(crate) fn lookup_label_intersection(
    req: gleaph_graph_kernel::index::IndexLabelIntersectionRequest,
) -> Vec<PostingHit> {
    IndexStore::new().lookup_label_intersection(&req.vertex_label_ids)
}

pub(crate) fn lookup_range(property_id: u32, req: PostingRangeRequest) -> Vec<PostingHit> {
    IndexStore::new()
        .lookup_range(property_id, &req)
        .unwrap_or_else(|e| {
            trap_err(e);
            unreachable!()
        })
}

pub(crate) fn count_postings_by_value(
    property_id: u32,
    min_count: u64,
    vertex_filter_packed: Option<Vec<u64>>,
) -> Vec<ValuePostingCount> {
    let filter: Option<nohash_hasher::IntSet<u64>> =
        vertex_filter_packed.map(|packed| packed.into_iter().collect());
    IndexStore::new().count_postings_by_value(
        property_id,
        min_count,
        DEFAULT_COUNT_POSTINGS_MAX_GROUPS,
        filter.as_ref(),
    )
}

pub(crate) fn filter_hits_by_label(vertex_label_id: u32, hits: Vec<PostingHit>) -> Vec<PostingHit> {
    IndexStore::new().filter_hits_by_label(vertex_label_id, &hits)
}

pub(crate) fn lookup_intersection_page(req: LookupIntersectionPageRequest) -> PostingHitPage {
    IndexStore::new()
        .lookup_intersection_page(&req)
        .unwrap_or_else(|e| {
            trap_err(e);
            unreachable!()
        })
}

pub(crate) fn lookup_range_intersection_page(
    req: LookupRangeIntersectionPageRequest,
) -> PostingHitPage {
    IndexStore::new()
        .lookup_range_intersection_page(&req)
        .unwrap_or_else(|e| {
            trap_err(e);
            unreachable!()
        })
}

pub(crate) fn count_postings_by_value_for_label(
    property_id: u32,
    vertex_label_id: u32,
    min_count: u64,
) -> Vec<ValuePostingCount> {
    IndexStore::new().count_postings_by_value_for_label(
        property_id,
        vertex_label_id,
        min_count,
        DEFAULT_COUNT_POSTINGS_MAX_GROUPS,
    )
}
