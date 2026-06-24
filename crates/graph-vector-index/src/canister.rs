//! Canister request handlers for `gleaph-graph-vector-index`.

use crate::facade::VectorIndexStore;
use crate::init::VectorIndexInitArgs;
use crate::state::VectorIndexError;
use candid::Principal;
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::federation::{ShardDetachCursor, ShardDetachStepResult, ShardId};
use gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp;
use ic_cdk::api::msg_caller;

pub(crate) fn init(args: VectorIndexInitArgs) {
    if let Err(e) = VectorIndexStore::new().init_from_args(&args) {
        ic_cdk::trap(e.to_string());
    }
}

pub(crate) fn admin_attach_shard_canister(
    graph_id: GraphId,
    shard_id: ShardId,
    shard_canister_principal: Principal,
) -> Result<(), String> {
    VectorIndexStore::new()
        .admin_attach_shard_canister(msg_caller(), graph_id, shard_id, shard_canister_principal)
        .map_err(|e| e.to_string())
}

pub(crate) fn admin_detach_shard_canister(
    shard_id: ShardId,
    resume: Option<ShardDetachCursor>,
) -> Result<ShardDetachStepResult, String> {
    VectorIndexStore::new()
        .admin_detach_shard_canister(msg_caller(), shard_id, resume)
        .map_err(|e| e.to_string())
}

pub(crate) fn vector_upsert(op: VectorEmbeddingSyncOp) -> Result<(), VectorIndexError> {
    VectorIndexStore::new().vector_upsert(msg_caller(), &op)
}

pub(crate) fn vector_remove(op: VectorEmbeddingSyncOp) -> Result<(), VectorIndexError> {
    VectorIndexStore::new().vector_remove(msg_caller(), &op)
}
