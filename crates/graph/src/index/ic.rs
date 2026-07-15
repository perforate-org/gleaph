//! Inter-canister client for [`gleaph_graph_index`] (Wasm only).

use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use async_trait::async_trait;
use candid::Principal;
use gleaph_graph_kernel::index::{
    EdgePostingHit, EdgePostingHitPage, IndexEqualSpec, IndexIntersectionRequest,
    IndexIntersectionResult, IndexPostingBatchProgress, IndexPostingMutation,
    LookupEdgeEqualPageRequest, LookupEqualPageRequest, LookupPropertyIntersectionPageRequest,
    LookupRangePageRequest, PostingHit, PostingHitPage, PostingRangeRequest,
};
use ic_cdk::call::Call;
use ic_cdk::call::CallFailed;

/// Page size for paginated property / edge equality exports. Bounds per-message materialization on
/// the index canister so query reads never build a full bucket in heap.
const INDEX_PAGE_LIMIT: u32 = 10_000;

#[derive(Clone, Debug)]
pub struct IcPropertyIndexClient {
    pub index_principal: Principal,
    pub shard_id: gleaph_graph_kernel::federation::ShardId,
}

fn ic_wait_err(op: &'static str, err: CallFailed) -> PlanQueryError {
    PlanQueryError::FederatedIndexCall {
        op,
        detail: format!("{err:?}"),
    }
}

fn ic_candid_decode_err(op: &'static str) -> PlanQueryError {
    PlanQueryError::FederatedIndexCall {
        op,
        detail: "candid decode failed".into(),
    }
}

impl IcPropertyIndexClient {
    async fn collect_property_intersection(
        &self,
        specs: &[IndexEqualSpec],
    ) -> Result<IndexIntersectionResult, PlanQueryError> {
        let mut vertices = Vec::new();
        let mut edges = Vec::new();
        let mut after = None;
        let mut shape = None;
        loop {
            let page: gleaph_graph_kernel::index::PropertyIntersectionPage =
                Call::bounded_wait(self.index_principal, "lookup_property_intersection_page")
                    .with_args(&(LookupPropertyIntersectionPageRequest {
                        specs: specs.to_vec(),
                        after,
                        limit: INDEX_PAGE_LIMIT,
                    },))
                    .await
                    .map_err(|e| ic_wait_err("lookup_property_intersection_page", e))?
                    .candid()
                    .map_err(|_| ic_candid_decode_err("lookup_property_intersection_page"))?;
            match page.hits {
                IndexIntersectionResult::Vertices(hits) => {
                    if shape.is_none() {
                        shape = Some(false);
                    }
                    vertices.extend(hits);
                }
                IndexIntersectionResult::Edges(hits) => {
                    if shape.is_none() {
                        shape = Some(true);
                    }
                    edges.extend(hits);
                }
            }
            if page.done {
                break;
            }
            after = page.next;
        }
        Ok(if shape.unwrap_or(false) {
            IndexIntersectionResult::Edges(edges)
        } else {
            IndexIntersectionResult::Vertices(vertices)
        })
    }
}

#[async_trait(?Send)]
impl PropertyIndexLookup for IcPropertyIndexClient {
    fn supports_posting_batch(&self) -> bool {
        true
    }

    async fn posting_batch_at(
        &self,
        shard_id: gleaph_graph_kernel::federation::ShardId,
        operations: Vec<IndexPostingMutation>,
    ) -> Result<IndexPostingBatchProgress, PlanQueryError> {
        let progress: IndexPostingBatchProgress =
            Call::bounded_wait(self.index_principal, "posting_batch")
                .with_args(&(shard_id, operations))
                .await
                .map_err(|e| ic_wait_err("posting_batch", e))?
                .candid()
                .map_err(|_| ic_candid_decode_err("posting_batch"))?;
        Ok(progress)
    }

    fn local_shard_id(&self) -> gleaph_graph_kernel::federation::ShardId {
        self.shard_id
    }

    async fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        let mut hits = Vec::new();
        let mut after = None;
        loop {
            let page: PostingHitPage =
                Call::bounded_wait(self.index_principal, "lookup_equal_page")
                    .with_args(&(LookupEqualPageRequest {
                        property_id,
                        value: value.clone(),
                        after,
                        limit: INDEX_PAGE_LIMIT,
                    },))
                    .await
                    .map_err(|e| ic_wait_err("lookup_equal_page", e))?
                    .candid()
                    .map_err(|_| ic_candid_decode_err("lookup_equal_page"))?;
            hits.extend(page.hits);
            if page.done {
                break;
            }
            after = page.next;
        }
        Ok(hits)
    }

    async fn lookup_range(
        &self,
        property_id: u32,
        req: &PostingRangeRequest,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        let mut hits = Vec::new();
        let mut after = None;
        loop {
            let page: PostingHitPage =
                Call::bounded_wait(self.index_principal, "lookup_range_page")
                    .with_args(&(LookupRangePageRequest {
                        property_id,
                        range: req.clone(),
                        after,
                        limit: INDEX_PAGE_LIMIT,
                    },))
                    .await
                    .map_err(|e| ic_wait_err("lookup_range_page", e))?
                    .candid()
                    .map_err(|_| ic_candid_decode_err("lookup_range_page"))?;
            hits.extend(page.hits);
            if page.done {
                break;
            }
            after = page.next;
        }
        Ok(hits)
    }

    async fn lookup_intersection(
        &self,
        req: &IndexIntersectionRequest,
    ) -> Result<IndexIntersectionResult, PlanQueryError> {
        self.collect_property_intersection(&req.specs).await
    }

    async fn lookup_edge_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
        label_id: Option<u16>,
    ) -> Result<Vec<EdgePostingHit>, PlanQueryError> {
        let mut hits = Vec::new();
        let mut after = None;
        loop {
            let page: EdgePostingHitPage =
                Call::bounded_wait(self.index_principal, "lookup_edge_equal_page")
                    .with_args(&(LookupEdgeEqualPageRequest {
                        property_id,
                        value: value.clone(),
                        label_id,
                        after,
                        limit: INDEX_PAGE_LIMIT,
                    },))
                    .await
                    .map_err(|e| ic_wait_err("lookup_edge_equal_page", e))?
                    .candid()
                    .map_err(|_| ic_candid_decode_err("lookup_edge_equal_page"))?;
            hits.extend(page.hits);
            if page.done {
                break;
            }
            after = page.next;
        }
        Ok(hits)
    }

    async fn posting_insert_at(
        &self,
        shard_id: gleaph_graph_kernel::federation::ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        let (): () = Call::bounded_wait(self.index_principal, "posting_insert")
            .with_args(&(shard_id.raw(), property_id, value, vertex_id))
            .await
            .map_err(|e| ic_wait_err("posting_insert", e))?
            .candid()
            .map_err(|_| ic_candid_decode_err("posting_insert"))?;
        Ok(())
    }

    async fn posting_remove_at(
        &self,
        shard_id: gleaph_graph_kernel::federation::ShardId,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        let (): () = Call::bounded_wait(self.index_principal, "posting_remove")
            .with_args(&(shard_id.raw(), property_id, value, vertex_id))
            .await
            .map_err(|e| ic_wait_err("posting_remove", e))?
            .candid()
            .map_err(|_| ic_candid_decode_err("posting_remove"))?;
        Ok(())
    }

    async fn label_posting_insert_at(
        &self,
        shard_id: gleaph_graph_kernel::federation::ShardId,
        label_id: u32,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        let (): () = Call::bounded_wait(self.index_principal, "label_posting_insert")
            .with_args(&(shard_id.raw(), label_id, vertex_id))
            .await
            .map_err(|e| ic_wait_err("label_posting_insert", e))?
            .candid()
            .map_err(|_| ic_candid_decode_err("label_posting_insert"))?;
        Ok(())
    }

    async fn label_posting_remove_at(
        &self,
        shard_id: gleaph_graph_kernel::federation::ShardId,
        label_id: u32,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        let (): () = Call::bounded_wait(self.index_principal, "label_posting_remove")
            .with_args(&(shard_id.raw(), label_id, vertex_id))
            .await
            .map_err(|e| ic_wait_err("label_posting_remove", e))?
            .candid()
            .map_err(|_| ic_candid_decode_err("label_posting_remove"))?;
        Ok(())
    }

    async fn edge_posting_insert_at(
        &self,
        shard_id: gleaph_graph_kernel::federation::ShardId,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), PlanQueryError> {
        let (): () = Call::bounded_wait(self.index_principal, "edge_posting_insert")
            .with_args(&(
                shard_id.raw(),
                property_id,
                value,
                label_id,
                owner_vertex_id,
                slot_index,
            ))
            .await
            .map_err(|e| ic_wait_err("edge_posting_insert", e))?
            .candid()
            .map_err(|_| ic_candid_decode_err("edge_posting_insert"))?;
        Ok(())
    }

    async fn edge_posting_remove_at(
        &self,
        shard_id: gleaph_graph_kernel::federation::ShardId,
        property_id: u32,
        value: Vec<u8>,
        label_id: u16,
        owner_vertex_id: u32,
        slot_index: u32,
    ) -> Result<(), PlanQueryError> {
        let (): () = Call::bounded_wait(self.index_principal, "edge_posting_remove")
            .with_args(&(
                shard_id.raw(),
                property_id,
                value,
                label_id,
                owner_vertex_id,
                slot_index,
            ))
            .await
            .map_err(|e| ic_wait_err("edge_posting_remove", e))?
            .candid()
            .map_err(|_| ic_candid_decode_err("edge_posting_remove"))?;
        Ok(())
    }
}
