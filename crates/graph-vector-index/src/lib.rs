//! Federated derived vector-index canister (`gleaph-graph-vector-index`).
//!
//! Owns the derived `ivf_flat` search structures rebuildable from the graph canonical
//! `VertexEmbeddingStore` (ADR 0031). Slice 2 is mutation-only: `vector_upsert` / `vector_remove`
//! over a degenerate `ivf_flat` page store (`nlist = 1`, `partition_id = 0`, no centroids, no
//! search). Shard/canister attachments are configured by the router via `admin_attach_shard_canister`.
//!
//! ## API visibility
//!
//! Admin APIs are router-only (`guard_router_canister`). Mutation updates authorize the caller
//! against the shard catalog inside the store and return [`VectorIndexError`] over the wire.

mod facade;
mod records;

#[cfg(feature = "canbench")]
mod bench;

pub mod init;
pub mod state;

mod canister;
mod guards;

pub use facade::VectorIndexStore;
pub use init::VectorIndexInitArgs;
pub use state::VectorIndexError;

use crate::guards::guard_router_canister;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardDetachCursor, ShardDetachStepResult, ShardId};
use gleaph_graph_kernel::vector_index::{
    VectorEmbeddingSyncOp, VectorRebuildStatus, VectorSearchRequest, VectorSearchResult,
};
use ic_cdk_macros::{init, query, update};

#[init]
fn init(args: VectorIndexInitArgs) {
    canister::init(args);
}

#[update(guard = "guard_router_canister")]
fn admin_attach_shard_canister(
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    canister::admin_attach_shard_canister(graph_id, shard_id, shard_canister_principal)
}

#[update(guard = "guard_router_canister")]
fn admin_detach_shard_canister(
    shard_id: ShardId,
    resume: Option<ShardDetachCursor>,
) -> Result<ShardDetachStepResult, String> {
    canister::admin_detach_shard_canister(shard_id, resume)
}

#[update]
fn vector_upsert(op: VectorEmbeddingSyncOp) -> Result<(), VectorIndexError> {
    canister::vector_upsert(op)
}

#[update]
fn vector_remove(op: VectorEmbeddingSyncOp) -> Result<(), VectorIndexError> {
    canister::vector_remove(op)
}

/// Read-only exact `ivf_flat` top-k search (ADR 0031 Slice 5). Router-guarded like the
/// property-index reads so derived vectors cannot be queried directly, bypassing the Slice 4
/// activation/readiness gate; the Router is the activation-gated public surface.
#[query(guard = "guard_router_canister")]
fn vector_search(req: VectorSearchRequest) -> Result<VectorSearchResult, VectorIndexError> {
    canister::vector_search(req)
}

/// Begins a production shadow-version rebuild for an `nlist > 1` index (ADR 0031 Slice 7). O(1);
/// the Router drives the subsequent bounded steps. Router-guarded like the other admin endpoints.
#[update(guard = "guard_router_canister")]
fn admin_start_vector_rebuild(index_id: u32, nlist: u32, sample_limit: u32) -> Result<(), String> {
    canister::admin_start_vector_rebuild(index_id, nlist, sample_limit)
}

/// Drives one bounded `Sampling`/`Building` step of an in-flight rebuild.
#[update(guard = "guard_router_canister")]
fn admin_vector_rebuild_step(
    index_id: u32,
    max_subjects: u32,
) -> Result<VectorRebuildStatus, String> {
    canister::admin_vector_rebuild_step(index_id, max_subjects)
}

/// Reports the O(1) scalar status of a rebuild.
#[query(guard = "guard_router_canister")]
fn admin_vector_rebuild_status(index_id: u32) -> Result<VectorRebuildStatus, String> {
    canister::admin_vector_rebuild_status(index_id)
}

/// Atomically publishes a `ReadyToPublish` rebuild (O(1) def + centroid metadata flip).
#[update(guard = "guard_router_canister")]
fn admin_publish_vector_rebuild(index_id: u32) -> Result<(), String> {
    canister::admin_publish_vector_rebuild(index_id)
}

/// Aborts an in-flight rebuild; bounded teardown follows via the cleanup step.
#[update(guard = "guard_router_canister")]
fn admin_abort_vector_rebuild(index_id: u32) -> Result<(), String> {
    canister::admin_abort_vector_rebuild(index_id)
}

/// Drives one bounded teardown step of a post-publish `Cleaning` or an `Aborting` rebuild.
#[update(guard = "guard_router_canister")]
fn admin_vector_rebuild_cleanup_step(
    index_id: u32,
    max_work: u32,
) -> Result<VectorRebuildStatus, String> {
    canister::admin_vector_rebuild_cleanup_step(index_id, max_work)
}

ic_cdk::export_candid!();
