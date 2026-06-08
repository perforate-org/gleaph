//! Async client trait for `gleaph-graph-index` (lookup + posting maintenance).

use crate::plan::PlanQueryError;
use async_trait::async_trait;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{IndexIntersectionRequest, PostingHit, PostingRangeRequest};

#[async_trait(?Send)]
pub trait PropertyIndexLookup {
    async fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Result<Vec<PostingHit>, PlanQueryError>;

    async fn lookup_range(
        &self,
        property_id: u32,
        req: &PostingRangeRequest,
    ) -> Result<Vec<PostingHit>, PlanQueryError>;

    async fn lookup_intersection(
        &self,
        req: &IndexIntersectionRequest,
    ) -> Result<Vec<PostingHit>, PlanQueryError>;

    async fn posting_insert(
        &self,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        self.posting_insert_at(self.local_shard_id(), property_id, value, vertex_id)
            .await
    }

    async fn posting_remove(
        &self,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        self.posting_remove_at(self.local_shard_id(), property_id, value, vertex_id)
            .await
    }

    /// Shard that owns `vertex_id` in [`posting_insert`] / [`posting_remove`].
    fn local_shard_id(&self) -> ShardId;

    async fn posting_insert_at(
        &self,
        shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError>;

    async fn posting_remove_at(
        &self,
        shard_id: ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError>;
}

/// Used when no index canister is wired; mutations ignore postings and scans fail at runtime.
pub struct NoPropertyIndex;

#[async_trait(?Send)]
impl PropertyIndexLookup for NoPropertyIndex {
    fn local_shard_id(&self) -> ShardId {
        0
    }

    async fn lookup_equal(
        &self,
        _property_id: u32,
        _value: Vec<u8>,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        Err(PlanQueryError::UnsupportedOp("IndexScan(no index client)"))
    }

    async fn lookup_range(
        &self,
        _property_id: u32,
        _req: &PostingRangeRequest,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        Err(PlanQueryError::UnsupportedOp("IndexScan(no index client)"))
    }

    async fn lookup_intersection(
        &self,
        _req: &IndexIntersectionRequest,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        Err(PlanQueryError::UnsupportedOp(
            "IndexIntersection(no index client)",
        ))
    }

    async fn posting_insert_at(
        &self,
        _shard_id: ShardId,
        _property_id: u32,
        _value: Vec<u8>,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }

    async fn posting_remove_at(
        &self,
        _shard_id: ShardId,
        _property_id: u32,
        _value: Vec<u8>,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }
}
