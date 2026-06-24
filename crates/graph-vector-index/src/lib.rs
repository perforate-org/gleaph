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
    VectorEmbeddingSyncOp, VectorSearchRequest, VectorSearchResult,
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

ic_cdk::export_candid!();
