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
    ) -> Result<Vec<ValuePostingCount>, String> {
        #[cfg(target_family = "wasm")]
        {
            use ic_cdk::call::Call;

            let counts: Vec<ValuePostingCount> =
                Call::bounded_wait(self.index_canister, "count_postings_by_value")
                    .with_args(&(property_id, min_count))
                    .await
                    .map_err(|e| format!("count_postings_by_value: {e}"))?
                    .candid()
                    .map_err(|e| format!("count_postings_by_value decode: {e}"))?;
            return Ok(counts);
        }
        #[cfg(not(target_family = "wasm"))]
        {
            let _ = (property_id, min_count);
            Err("count_postings_by_value unavailable in native builds".into())
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
