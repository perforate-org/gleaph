//! Inter-canister client for `vector-canister` (Wasm only at runtime).
//!
//! Mirrors [`crate::index::ic::IcPropertyIndexClient`]. The canister mutation endpoints return
//! `Result<(), VectorCanisterError>`, so a transport failure and a logical rejection are both mapped
//! to [`PlanQueryError::FederatedIndexCall`] for the caller's deferral / repair path.

use crate::index::vector_lookup::VectorCanisterLookup;
use crate::plan::PlanQueryError;
use async_trait::async_trait;
use candid::Principal;
use gleaph_graph_kernel::vector_index::{
    VectorCanisterError, VectorEmbeddingSyncOp, VectorSyncBatchProgress,
};
use ic_cdk::call::Call;
use ic_cdk::call::CallFailed;

#[derive(Clone, Debug)]
pub struct IcVectorCanisterClient {
    pub vector_principal: Principal,
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

fn map_canister_err(op: &'static str, err: VectorCanisterError) -> PlanQueryError {
    PlanQueryError::FederatedIndexCall {
        op,
        detail: err.to_string(),
    }
}

#[async_trait(?Send)]
impl VectorCanisterLookup for IcVectorCanisterClient {
    fn supports_sync_batch(&self) -> bool {
        true
    }

    async fn vector_sync_batch(
        &self,
        operations: Vec<VectorEmbeddingSyncOp>,
    ) -> Result<VectorSyncBatchProgress, PlanQueryError> {
        let progress: VectorSyncBatchProgress =
            Call::bounded_wait(self.vector_principal, "vector_sync_batch")
                .with_args(&(operations,))
                .await
                .map_err(|e| ic_wait_err("vector_sync_batch", e))?
                .candid()
                .map_err(|_| ic_candid_decode_err("vector_sync_batch"))?;
        Ok(progress)
    }

    async fn vector_upsert(&self, op: VectorEmbeddingSyncOp) -> Result<(), PlanQueryError> {
        let result: Result<(), VectorCanisterError> =
            Call::bounded_wait(self.vector_principal, "vector_upsert")
                .with_args(&(op,))
                .await
                .map_err(|e| ic_wait_err("vector_upsert", e))?
                .candid()
                .map_err(|_| ic_candid_decode_err("vector_upsert"))?;
        result.map_err(|e| map_canister_err("vector_upsert", e))
    }

    async fn vector_remove(&self, op: VectorEmbeddingSyncOp) -> Result<(), PlanQueryError> {
        let result: Result<(), VectorCanisterError> =
            Call::bounded_wait(self.vector_principal, "vector_remove")
                .with_args(&(op,))
                .await
                .map_err(|e| ic_wait_err("vector_remove", e))?
                .candid()
                .map_err(|_| ic_candid_decode_err("vector_remove"))?;
        result.map_err(|e| map_canister_err("vector_remove", e))
    }
}
