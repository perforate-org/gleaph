//! Mutation interface to the derived `graph-vector-index` canister (ADR 0031).
//!
//! Reads (vector search) are deferred to a later slice; Slice 2 only delivers derived embedding
//! mutations. Each [`gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp`] is self-describing
//! (it carries its `subject`, which embeds the owning [`ShardId`]), so the vector index validates
//! ownership from the op alone and the repair drain can replay a stored op without extra context.

use crate::plan::PlanQueryError;
use async_trait::async_trait;
use gleaph_graph_kernel::vector_index::{VectorEmbeddingSyncOp, VectorSyncBatchProgress};

#[async_trait(?Send)]
pub trait VectorIndexLookup {
    fn supports_sync_batch(&self) -> bool {
        false
    }

    async fn vector_sync_batch(
        &self,
        operations: Vec<VectorEmbeddingSyncOp>,
    ) -> Result<VectorSyncBatchProgress, PlanQueryError> {
        let mut applied = 0u32;
        for operation in operations {
            if operation.remove {
                self.vector_remove(operation).await?;
            } else {
                self.vector_upsert(operation).await?;
            }
            applied = applied.saturating_add(1);
        }
        Ok(VectorSyncBatchProgress {
            applied,
            next_index: None,
            instruction_budget_exhausted: false,
        })
    }

    async fn vector_upsert(&self, op: VectorEmbeddingSyncOp) -> Result<(), PlanQueryError>;
    async fn vector_remove(&self, op: VectorEmbeddingSyncOp) -> Result<(), PlanQueryError>;
}
