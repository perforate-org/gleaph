//! Async client trait for `gleaph-graph-index` (lookup + posting maintenance).

use crate::plan::PlanQueryError;
use async_trait::async_trait;
use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest};

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

    async fn posting_insert(
        &self,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError>;

    async fn posting_remove(
        &self,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError>;
}

/// Used when no index canister is wired; mutations ignore postings and scans fail at runtime.
pub struct NoPropertyIndex;

#[async_trait(?Send)]
impl PropertyIndexLookup for NoPropertyIndex {
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

    async fn posting_insert(
        &self,
        _property_id: u32,
        _value: Vec<u8>,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }

    async fn posting_remove(
        &self,
        _property_id: u32,
        _value: Vec<u8>,
        _vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        Ok(())
    }
}
