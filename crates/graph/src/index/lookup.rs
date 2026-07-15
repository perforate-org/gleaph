//! Async client trait for `gleaph-graph-index` (lookup + posting maintenance).

use crate::plan::PlanQueryError;
use async_trait::async_trait;
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::index::{
    EdgePostingHit, IndexIntersectionRequest, IndexIntersectionResult, IndexPostingBatchProgress,
    IndexPostingMutation, PostingHit, PostingRangeRequest,
};

#[async_trait(?Send)]
pub trait PropertyIndexLookup {
    fn supports_posting_batch(&self) -> bool {
        false
    }

    async fn posting_batch_at(
        &self,
        shard_id: ShardId,
        operations: Vec<IndexPostingMutation>,
    ) -> Result<IndexPostingBatchProgress, PlanQueryError> {
        let mut applied = 0u32;
        for operation in operations {
            match operation {
                IndexPostingMutation::VertexProperty {
                    remove,
                    property_id,
                    value,
                    vertex_id,
                } => {
                    if remove {
                        self.posting_remove_at(shard_id, property_id, value, vertex_id)
                            .await?
                    } else {
                        self.posting_insert_at(shard_id, property_id, value, vertex_id)
                            .await?
                    }
                }
                IndexPostingMutation::EdgeProperty {
                    remove,
                    property_id,
                    value,
                    label_id,
                    owner_vertex_id,
                    slot_index,
                } => {
                    if remove {
                        self.edge_posting_remove_at(
                            shard_id,
                            property_id,
                            value,
                            label_id,
                            owner_vertex_id,
                            slot_index,
                        )
                        .await?
                    } else {
                        self.edge_posting_insert_at(
                            shard_id,
                            property_id,
                            value,
                            label_id,
                            owner_vertex_id,
                            slot_index,
                        )
                        .await?
                    }
                }
                IndexPostingMutation::Label {
                    remove,
                    label_id,
                    vertex_id,
                } => {
                    if remove {
                        self.label_posting_remove_at(shard_id, label_id, vertex_id)
                            .await?
                    } else {
                        self.label_posting_insert_at(shard_id, label_id, vertex_id)
                            .await?
                    }
                }
            }
            applied = applied.saturating_add(1);
        }
        Ok(IndexPostingBatchProgress {
            applied,
            next_index: None,
            instruction_budget_exhausted: false,
        })
    }

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
    ) -> Result<IndexIntersectionResult, PlanQueryError>;

    async fn lookup_edge_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
        label_id: Option<u16>,
    ) -> Result<Vec<EdgePostingHit>, PlanQueryError> {
        let _ = (property_id, value, label_id);
        Ok(vec![])
    }

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

    async fn label_posting_insert(
        &self,
        label_id: u32,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        self.label_posting_insert_at(self.local_shard_id(), label_id, vertex_id)
            .await
    }

    async fn label_posting_remove(
        &self,
        label_id: u32,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        self.label_posting_remove_at(self.local_shard_id(), label_id, vertex_id)
            .await
    }

    async fn label_posting_insert_at(
        &self,
        shard_id: ShardId,
        label_id: u32,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError>;

    async fn label_posting_remove_at(
        &self,
        shard_id: ShardId,
        label_id: u32,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError>;

    async fn edge_posting_insert_at(
        &self,
        _shard_id: ShardId,
        _property_id: u32,
        _value: Vec<u8>,
        _label_id: u16,
        _owner_vertex_id: u32,
        _slot_index: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }

    async fn edge_posting_remove_at(
        &self,
        _shard_id: ShardId,
        _property_id: u32,
        _value: Vec<u8>,
        _label_id: u16,
        _owner_vertex_id: u32,
        _slot_index: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }
}

/// Used when no index canister is wired; mutations ignore postings and scans fail at runtime.
pub struct NoPropertyIndex;

#[async_trait(?Send)]
impl PropertyIndexLookup for NoPropertyIndex {
    fn local_shard_id(&self) -> ShardId {
        ShardId::new(0)
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
    ) -> Result<IndexIntersectionResult, PlanQueryError> {
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

    async fn label_posting_insert_at(
        &self,
        _shard_id: ShardId,
        _label_id: u32,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }

    async fn label_posting_remove_at(
        &self,
        _shard_id: ShardId,
        _label_id: u32,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }
}
