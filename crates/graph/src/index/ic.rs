//! Inter-canister client for [`gleaph_graph_index`] (Wasm only).

use crate::index::lookup::PropertyIndexLookup;
use crate::plan::PlanQueryError;
use async_trait::async_trait;
use candid::Principal;
use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest};
use ic_cdk::call::Call;
use ic_cdk::call::CallFailed;

#[derive(Clone, Debug)]
pub struct IcPropertyIndexClient {
    pub index_principal: Principal,
    pub shard_id: u64,
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

#[async_trait(?Send)]
impl PropertyIndexLookup for IcPropertyIndexClient {
    async fn lookup_equal(
        &self,
        property_id: u32,
        value: Vec<u8>,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        let hits: Vec<PostingHit> = Call::bounded_wait(self.index_principal, "lookup_equal")
            .with_args(&(property_id, value))
            .await
            .map_err(|e| ic_wait_err("lookup_equal", e))?
            .candid()
            .map_err(|_| ic_candid_decode_err("lookup_equal"))?;
        Ok(hits)
    }

    async fn lookup_range(
        &self,
        property_id: u32,
        req: &PostingRangeRequest,
    ) -> Result<Vec<PostingHit>, PlanQueryError> {
        let hits: Vec<PostingHit> = Call::bounded_wait(self.index_principal, "lookup_range")
            .with_args(&(property_id, req.clone()))
            .await
            .map_err(|e| ic_wait_err("lookup_range", e))?
            .candid()
            .map_err(|_| ic_candid_decode_err("lookup_range"))?;
        Ok(hits)
    }

    async fn posting_insert(
        &self,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        let (): () = Call::bounded_wait(self.index_principal, "posting_insert")
            .with_args(&(self.shard_id, property_id, value, vertex_id))
            .await
            .map_err(|e| ic_wait_err("posting_insert", e))?
            .candid()
            .map_err(|_| ic_candid_decode_err("posting_insert"))?;
        Ok(())
    }

    async fn posting_remove(
        &self,
        property_id: u32,
        value: Vec<u8>,
        vertex_id: u32,
    ) -> Result<(), PlanQueryError> {
        let (): () = Call::bounded_wait(self.index_principal, "posting_remove")
            .with_args(&(self.shard_id, property_id, value, vertex_id))
            .await
            .map_err(|e| ic_wait_err("posting_remove", e))?
            .candid()
            .map_err(|_| ic_candid_decode_err("posting_remove"))?;
        Ok(())
    }
}
