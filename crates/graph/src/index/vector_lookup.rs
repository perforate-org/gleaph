//! Mutation interface to the derived `graph-vector-index` canister (ADR 0031).
//!
//! Reads (vector search) are deferred to a later slice; Slice 2 only delivers derived embedding
//! mutations. Each [`gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp`] is self-describing
//! (it carries its `subject`, which embeds the owning [`ShardId`]), so the vector index validates
//! ownership from the op alone and the repair drain can replay a stored op without extra context.

use crate::plan::PlanQueryError;
use async_trait::async_trait;
use gleaph_graph_kernel::vector_index::VectorEmbeddingSyncOp;

#[async_trait(?Send)]
pub trait VectorIndexLookup {
    async fn vector_upsert(&self, op: VectorEmbeddingSyncOp) -> Result<(), PlanQueryError>;
    async fn vector_remove(&self, op: VectorEmbeddingSyncOp) -> Result<(), PlanQueryError>;
}
