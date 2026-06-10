//! Read-only property index client for router seed routing.

use candid::Principal;
use gleaph_graph_kernel::index::{IndexIntersectionRequest, PostingHit, ValuePostingCount};

#[derive(Clone, Debug)]
pub struct RouterIndexClient {
    pub index_canister: Principal,
}

impl RouterIndexClient {
    pub fn new(index_canister: Principal) -> Self {
        Self { index_canister }
    }

    pub async fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Result<Vec<PostingHit>, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let hits: Vec<PostingHit> = Call::bounded_wait(self.index_canister, "lookup_equal")
                .with_args(&(property_id, value))
                .await
                .map_err(|e| format!("lookup_equal: {e}"))?
                .candid()
                .map_err(|e| format!("lookup_equal decode: {e}"))?;
            return Ok(hits);
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (property_id, value);
            Err("lookup_equal unavailable in native builds".into())
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
            return Ok(counts);
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
            return Ok(filtered);
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (vertex_label_id, hits);
            Err("filter_hits_by_label unavailable in native builds".into())
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
            return Ok(counts);
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (property_id, vertex_label_id, min_count);
            Err("count_postings_by_value_for_label unavailable in native builds".into())
        }
    }

    pub async fn lookup_label(&self, vertex_label_id: u32) -> Result<Vec<PostingHit>, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let hits: Vec<PostingHit> = Call::bounded_wait(self.index_canister, "lookup_label")
                .with_args(&(vertex_label_id,))
                .await
                .map_err(|e| format!("lookup_label: {e}"))?
                .candid()
                .map_err(|e| format!("lookup_label decode: {e}"))?;
            return Ok(hits);
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = vertex_label_id;
            Err("lookup_label unavailable in native builds".into())
        }
    }

    pub async fn lookup_intersection(
        &self,
        req: IndexIntersectionRequest,
    ) -> Result<Vec<PostingHit>, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let hits: Vec<PostingHit> =
                Call::bounded_wait(self.index_canister, "lookup_intersection")
                    .with_args(&(req,))
                    .await
                    .map_err(|e| format!("lookup_intersection: {e}"))?
                    .candid()
                    .map_err(|e| format!("lookup_intersection decode: {e}"))?;
            return Ok(hits);
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = req;
            Err("lookup_intersection unavailable in native builds".into())
        }
    }
}
