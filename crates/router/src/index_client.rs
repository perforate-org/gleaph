//! Read-only property index client for router seed routing.

use candid::Principal;
use gleaph_graph_kernel::index::{
    EdgePostingHitPage, IndexIntersectionRequest, IndexIntersectionResult,
    IndexLabelIntersectionRequest, LabelLookupPageRequest, LabelLookupPageResult,
    LookupEdgeEqualPageRequest, LookupEqualPageForLabelRequest, LookupEqualPageRequest,
    LookupIntersectionPageRequest, LookupRangeIntersectionPageRequest,
    LookupRangePageForLabelRequest, PostingHit, PostingHitPage, PostingRangeRequest,
    ValuePostingCount,
};

#[derive(Clone, Debug)]
pub struct RouterIndexClient {
    pub index_canister: Principal,
}

impl RouterIndexClient {
    pub fn new(index_canister: Principal) -> Self {
        Self { index_canister }
    }

    pub async fn lookup_equal_page(
        &self,
        req: LookupEqualPageRequest,
    ) -> Result<PostingHitPage, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let page: PostingHitPage = Call::bounded_wait(self.index_canister, "lookup_equal_page")
                .with_args(&(req,))
                .await
                .map_err(|e| format!("lookup_equal_page: {e}"))?
                .candid()
                .map_err(|e| format!("lookup_equal_page decode: {e}"))?;
            Ok(page)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = req;
            Err("lookup_equal_page unavailable in native builds".into())
        }
    }

    pub async fn lookup_equal_page_for_label(
        &self,
        req: LookupEqualPageForLabelRequest,
    ) -> Result<PostingHitPage, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let page: PostingHitPage =
                Call::bounded_wait(self.index_canister, "lookup_equal_page_for_label")
                    .with_args(&(req,))
                    .await
                    .map_err(|e| format!("lookup_equal_page_for_label: {e}"))?
                    .candid()
                    .map_err(|e| format!("lookup_equal_page_for_label decode: {e}"))?;
            Ok(page)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = req;
            Err("lookup_equal_page_for_label unavailable in native builds".into())
        }
    }

    pub async fn lookup_edge_equal_page(
        &self,
        req: LookupEdgeEqualPageRequest,
    ) -> Result<EdgePostingHitPage, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let page: EdgePostingHitPage =
                Call::bounded_wait(self.index_canister, "lookup_edge_equal_page")
                    .with_args(&(req,))
                    .await
                    .map_err(|e| format!("lookup_edge_equal_page: {e}"))?
                    .candid()
                    .map_err(|e| format!("lookup_edge_equal_page decode: {e}"))?;
            Ok(page)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = req;
            Err("lookup_edge_equal_page unavailable in native builds".into())
        }
    }

    pub async fn count_postings_by_value(
        &self,
        property_id: u32,
        min_count: u64,
        vertex_filter_packed: Option<Vec<u64>>,
    ) -> Result<Vec<ValuePostingCount>, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let counts: Vec<ValuePostingCount> =
                Call::bounded_wait(self.index_canister, "count_postings_by_value")
                    .with_args(&(property_id, min_count, vertex_filter_packed))
                    .await
                    .map_err(|e| format!("count_postings_by_value: {e}"))?
                    .candid()
                    .map_err(|e| format!("count_postings_by_value decode: {e}"))?;
            Ok(counts)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (property_id, min_count, vertex_filter_packed);
            Err("count_postings_by_value unavailable in native builds".into())
        }
    }

    pub async fn filter_hits_by_label(
        &self,
        vertex_label_id: u32,
        hits: Vec<PostingHit>,
    ) -> Result<Vec<PostingHit>, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let filtered: Vec<PostingHit> =
                Call::bounded_wait(self.index_canister, "filter_hits_by_label")
                    .with_args(&(vertex_label_id, hits))
                    .await
                    .map_err(|e| format!("filter_hits_by_label: {e}"))?
                    .candid()
                    .map_err(|e| format!("filter_hits_by_label decode: {e}"))?;
            Ok(filtered)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (vertex_label_id, hits);
            Err("filter_hits_by_label unavailable in native builds".into())
        }
    }

    pub async fn lookup_intersection_page(
        &self,
        req: LookupIntersectionPageRequest,
    ) -> Result<PostingHitPage, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let page: PostingHitPage =
                Call::bounded_wait(self.index_canister, "lookup_intersection_page")
                    .with_args(&(req,))
                    .await
                    .map_err(|e| format!("lookup_intersection_page: {e}"))?
                    .candid()
                    .map_err(|e| format!("lookup_intersection_page decode: {e}"))?;
            Ok(page)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = req;
            Err("lookup_intersection_page unavailable in native builds".into())
        }
    }

    pub async fn lookup_range_page(
        &self,
        property_id: u32,
        range: PostingRangeRequest,
        after: Option<gleaph_graph_kernel::index::PropertyPostingCursor>,
        limit: u32,
    ) -> Result<PostingHitPage, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let req = gleaph_graph_kernel::index::LookupRangePageRequest {
                property_id,
                range,
                after,
                limit,
            };
            let page: PostingHitPage = Call::bounded_wait(self.index_canister, "lookup_range_page")
                .with_args(&(req,))
                .await
                .map_err(|e| format!("lookup_range_page: {e}"))?
                .candid()
                .map_err(|e| format!("lookup_range_page decode: {e}"))?;
            Ok(page)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (property_id, range, after, limit);
            Err("lookup_range_page unavailable in native builds".into())
        }
    }

    pub async fn lookup_range_page_for_label(
        &self,
        req: LookupRangePageForLabelRequest,
    ) -> Result<PostingHitPage, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let page: PostingHitPage =
                Call::bounded_wait(self.index_canister, "lookup_range_page_for_label")
                    .with_args(&(req,))
                    .await
                    .map_err(|e| format!("lookup_range_page_for_label: {e}"))?
                    .candid()
                    .map_err(|e| format!("lookup_range_page_for_label decode: {e}"))?;
            Ok(page)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = req;
            Err("lookup_range_page_for_label unavailable in native builds".into())
        }
    }

    pub async fn lookup_range_intersection_page(
        &self,
        req: LookupRangeIntersectionPageRequest,
    ) -> Result<PostingHitPage, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let page: PostingHitPage =
                Call::bounded_wait(self.index_canister, "lookup_range_intersection_page")
                    .with_args(&(req,))
                    .await
                    .map_err(|e| format!("lookup_range_intersection_page: {e}"))?
                    .candid()
                    .map_err(|e| format!("lookup_range_intersection_page decode: {e}"))?;
            Ok(page)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = req;
            Err("lookup_range_intersection_page unavailable in native builds".into())
        }
    }

    pub async fn count_postings_by_value_for_label(
        &self,
        property_id: u32,
        vertex_label_id: u32,
        min_count: u64,
    ) -> Result<Vec<ValuePostingCount>, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let counts: Vec<ValuePostingCount> =
                Call::bounded_wait(self.index_canister, "count_postings_by_value_for_label")
                    .with_args(&(property_id, vertex_label_id, min_count))
                    .await
                    .map_err(|e| format!("count_postings_by_value_for_label: {e}"))?
                    .candid()
                    .map_err(|e| format!("count_postings_by_value_for_label decode: {e}"))?;
            Ok(counts)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (property_id, vertex_label_id, min_count);
            Err("count_postings_by_value_for_label unavailable in native builds".into())
        }
    }

    pub async fn lookup_label_page(
        &self,
        req: LabelLookupPageRequest,
    ) -> Result<LabelLookupPageResult, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let page: LabelLookupPageResult =
                Call::bounded_wait(self.index_canister, "lookup_label_page")
                    .with_args(&(req,))
                    .await
                    .map_err(|e| format!("lookup_label_page: {e}"))?
                    .candid()
                    .map_err(|e| format!("lookup_label_page decode: {e}"))?;
            Ok(page)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = req;
            Err("lookup_label_page unavailable in native builds".into())
        }
    }

    #[expect(
        dead_code,
        reason = "reserved for label-export fast path; graph-index API wired ahead of router use"
    )]
    pub async fn lookup_label_for_shard(
        &self,
        vertex_label_id: u32,
        shard_id: gleaph_graph_kernel::federation::ShardId,
    ) -> Result<Vec<PostingHit>, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let hits: Vec<PostingHit> =
                Call::bounded_wait(self.index_canister, "lookup_label_for_shard")
                    .with_args(&(vertex_label_id, shard_id.raw()))
                    .await
                    .map_err(|e| format!("lookup_label_for_shard: {e}"))?
                    .candid()
                    .map_err(|e| format!("lookup_label_for_shard decode: {e}"))?;
            Ok(hits)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (vertex_label_id, shard_id);
            Err("lookup_label_for_shard unavailable in native builds".into())
        }
    }

    pub async fn lookup_label_intersection(
        &self,
        req: IndexLabelIntersectionRequest,
    ) -> Result<Vec<PostingHit>, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let hits: Vec<PostingHit> =
                Call::bounded_wait(self.index_canister, "lookup_label_intersection")
                    .with_args(&(req,))
                    .await
                    .map_err(|e| format!("lookup_label_intersection: {e}"))?
                    .candid()
                    .map_err(|e| format!("lookup_label_intersection decode: {e}"))?;
            Ok(hits)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = req;
            Err("lookup_label_intersection unavailable in native builds".into())
        }
    }

    pub async fn lookup_intersection(
        &self,
        req: IndexIntersectionRequest,
    ) -> Result<IndexIntersectionResult, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let result: IndexIntersectionResult =
                Call::bounded_wait(self.index_canister, "lookup_intersection")
                    .with_args(&(req,))
                    .await
                    .map_err(|e| format!("lookup_intersection: {e}"))?
                    .candid()
                    .map_err(|e| format!("lookup_intersection decode: {e}"))?;
            Ok(result)
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = req;
            Err("lookup_intersection unavailable in native builds".into())
        }
    }
}
