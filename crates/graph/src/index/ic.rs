//! Inter-canister client for [`gleaph_graph_index`] (Wasm only).

use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use async_trait::async_trait;
use candid::Principal;
use gleaph_graph_kernel::index::{
    EdgePostingHit, EdgePostingHitPage, IndexEqualSpec, IndexIntersectionRequest,
    IndexIntersectionResult, IndexSubject, LookupEdgeEqualPageRequest, LookupEqualPageRequest,
    LookupIntersectionPageRequest, LookupRangePageRequest, MAX_EQUALITY_INTERSECTION_ARMS,
    PostingHit, PostingHitPage, PostingRangeRequest,
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

/// `true` when every arm targets a vertex property (the planner's vertex-only `IndexIntersection`).
fn all_vertex_specs(specs: &[IndexEqualSpec]) -> bool {
    (2..=MAX_EQUALITY_INTERSECTION_ARMS).contains(&specs.len())
        && specs
            .iter()
            .all(|s| matches!(s.subject, IndexSubject::VertexProperty))
}

impl IcPropertyIndexClient {
    /// Streaming all-vertex intersection via the server-side `lookup_intersection_page`: the index
    /// walks the first arm one page at a time and sieves each page against the remaining arms
    /// in-heap, so no arm's full bucket is materialized and the walk + sieve fold into a single
    /// inter-canister call per page (vs one call per arm per page).
    async fn collect_vertex_intersection_hits(
        &self,
        specs: &[IndexEqualSpec],
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        let mut hits = Vec::new();
        let mut after = None;
        loop {
            let page: PostingHitPage =
                Call::bounded_wait(self.index_principal, "lookup_intersection_page")
                    .with_args(&(LookupIntersectionPageRequest {
                        specs: specs.to_vec(),
                        after,
                        limit: INDEX_PAGE_LIMIT,
                    },))
                    .await
                    .map_err(|e| ic_wait_err("lookup_intersection_page", e))?
                    .candid()
                    .map_err(|_| ic_candid_decode_err("lookup_intersection_page"))?;
            hits.extend(page.hits);
            if page.done {
                break;
            }
            after = page.next;
        }
        Ok(hits)
    }
}

#[async_trait(?Send)]
impl PropertyIndexLookup for IcPropertyIndexClient {
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
        // All-vertex intersection (the planner's `IndexIntersection` shape) uses the server-side
        // paged `lookup_intersection_page`, which walks the first arm in pages and sieves the
        // remaining arms in-heap, so the index never materializes a full posting bucket per arm.
        // Edge / mixed arms still use the server-side `lookup_intersection`.
        if all_vertex_specs(&req.specs) {
            let hits = self.collect_vertex_intersection_hits(&req.specs).await?;
            return Ok(IndexIntersectionResult::Vertices(hits));
        }
        let result: IndexIntersectionResult =
            Call::bounded_wait(self.index_principal, "lookup_intersection")
                .with_args(&(req.clone(),))
                .await
                .map_err(|e| ic_wait_err("lookup_intersection", e))?
                .candid()
                .map_err(|_| ic_candid_decode_err("lookup_intersection"))?;
        Ok(result)
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
