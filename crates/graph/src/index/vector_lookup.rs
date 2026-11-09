//! Mutation interface to the derived `vector-canister` canister (ADR 0031).
//!
//! Reads (vector search) are deferred to a later slice; Slice 2 only delivers derived embedding
//! mutations. Each [`gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp`] is self-describing
//! (it carries its `subject`, which embeds the owning [`ShardId`]), so the vector index validates
//! ownership from the op alone and the repair drain can replay a stored op without extra context.

use crate::plan::PlanQueryError;
use async_trait::async_trait;
use gleaph_graph_kernel::vector_index::{VectorEmbeddingSyncOp, VectorSyncBatchOutcome};

#[async_trait(?Send)]
#[allow(
    clippy::double_must_use,
    reason = "async_trait stamps a redundant #[must_use] on each generated method; the method already returns a #[must_use] Result"
)]
pub trait VectorCanisterLookup {
    async fn vector_sync_batch_outcome(
        &self,
        operations: Vec<VectorEmbeddingSyncOp>,
    ) -> Result<VectorSyncBatchOutcome, PlanQueryError>;

    async fn vector_upsert(&self, op: VectorEmbeddingSyncOp) -> Result<(), PlanQueryError>;
    async fn vector_remove(&self, op: VectorEmbeddingSyncOp) -> Result<(), PlanQueryError>;
}
