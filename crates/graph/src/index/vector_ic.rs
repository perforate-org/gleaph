//! Inter-canister client for `vector-canister` (Wasm only at runtime).
//!
//! Mirrors [`crate::index::ic::IcPropertyIndexClient`]. Transport failures, typed endpoint
//! unavailability, malformed replies, and logical rejections are mapped to
//! [`PlanQueryError::FederatedIndexCall`] before the caller can acknowledge durable repair work.

use crate::index::vector_lookup::VectorCanisterLookup;
use crate::plan::PlanQueryError;
use async_trait::async_trait;
use candid::Principal;
use gleaph_graph_kernel::vector_index::{
    VectorCanisterError, VectorEmbeddingSyncOp, VectorSyncBatchOutcome, VectorSyncBatchProgress,
    VectorSyncBatchUnavailable,
};
use ic_cdk::call::Call;
use ic_cdk::call::CallFailed;

#[derive(Clone, Debug)]
pub struct IcVectorCanisterClient {
    pub vector_principal: Principal,
}

const VECTOR_SYNC_BATCH_OUTCOME_METHOD: &str = "vector_sync_batch_outcome";

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

fn validate_vector_sync_batch_outcome_reply(
    operation_count: usize,
    reply: Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable>,
) -> Result<VectorSyncBatchOutcome, PlanQueryError> {
    let outcome = reply.map_err(|_| PlanQueryError::FederatedIndexCall {
        op: VECTOR_SYNC_BATCH_OUTCOME_METHOD,
        detail: "index definition store unavailable".into(),
    })?;
    outcome
        .validate(operation_count)
        .map_err(|detail| PlanQueryError::FederatedIndexCall {
            op: VECTOR_SYNC_BATCH_OUTCOME_METHOD,
            detail: detail.into(),
        })?;
    Ok(outcome)
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

    async fn vector_sync_batch_outcome(
        &self,
        operations: Vec<VectorEmbeddingSyncOp>,
    ) -> Result<VectorSyncBatchOutcome, PlanQueryError> {
        let operation_count = operations.len();
        let reply: Result<VectorSyncBatchOutcome, VectorSyncBatchUnavailable> =
            Call::bounded_wait(self.vector_principal, VECTOR_SYNC_BATCH_OUTCOME_METHOD)
                .with_args(&(operations,))
                .await
                .map_err(|error| ic_wait_err(VECTOR_SYNC_BATCH_OUTCOME_METHOD, error))?
                .candid()
                .map_err(|_| ic_candid_decode_err(VECTOR_SYNC_BATCH_OUTCOME_METHOD))?;
        validate_vector_sync_batch_outcome_reply(operation_count, reply)
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

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::vector_index::VectorSyncTerminalError;

    #[test]
    fn typed_progress_reply_is_validated() {
        let outcome = validate_vector_sync_batch_outcome_reply(
            3,
            Ok(VectorSyncBatchOutcome::Progress { applied: 2 }),
        )
        .expect("typed progress reply");

        assert_eq!(outcome, VectorSyncBatchOutcome::Progress { applied: 2 });
    }

    #[test]
    fn typed_terminal_reply_at_index_zero_is_validated() {
        let outcome = validate_vector_sync_batch_outcome_reply(
            2,
            Ok(VectorSyncBatchOutcome::Terminal {
                applied: 0,
                failed_index: 0,
                error: VectorSyncTerminalError::IndexDefinitionTablePressure,
            }),
        )
        .expect("typed terminal reply at index zero");

        assert_eq!(
            outcome,
            VectorSyncBatchOutcome::Terminal {
                applied: 0,
                failed_index: 0,
                error: VectorSyncTerminalError::IndexDefinitionTablePressure,
            }
        );
    }

    #[test]
    fn typed_terminal_reply_after_nonempty_prefix_is_validated() {
        let outcome = validate_vector_sync_batch_outcome_reply(
            3,
            Ok(VectorSyncBatchOutcome::Terminal {
                applied: 2,
                failed_index: 2,
                error: VectorSyncTerminalError::IndexDefinitionTablePressure,
            }),
        )
        .expect("typed terminal reply after prefix");

        assert_eq!(
            outcome,
            VectorSyncBatchOutcome::Terminal {
                applied: 2,
                failed_index: 2,
                error: VectorSyncTerminalError::IndexDefinitionTablePressure,
            }
        );
    }

    #[test]
    fn typed_unavailable_reply_fails_closed_on_additive_endpoint() {
        let error = validate_vector_sync_batch_outcome_reply(
            1,
            Err(VectorSyncBatchUnavailable::IndexDefinitionStoreUnavailable),
        )
        .expect_err("unavailable reply must fail");

        assert!(matches!(
            error,
            PlanQueryError::FederatedIndexCall {
                op: VECTOR_SYNC_BATCH_OUTCOME_METHOD,
                ref detail,
            } if detail == "index definition store unavailable"
        ));
    }

    #[test]
    fn malformed_typed_reply_fails_closed_on_additive_endpoint() {
        let error = validate_vector_sync_batch_outcome_reply(
            1,
            Ok(VectorSyncBatchOutcome::Terminal {
                applied: 1,
                failed_index: 0,
                error: VectorSyncTerminalError::IndexDefinitionTablePressure,
            }),
        )
        .expect_err("malformed reply must fail");

        assert!(matches!(
            error,
            PlanQueryError::FederatedIndexCall {
                op: VECTOR_SYNC_BATCH_OUTCOME_METHOD,
                ref detail,
            } if detail.contains("failed_index == applied")
        ));
    }
}
