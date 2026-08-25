//! Inter-canister client for the text canister (Wasm only at runtime).
//!
//! Mirrors [`crate::index::vector_ic::IcVectorCanisterClient`]. Transport failures, unavailable
//! endpoints, malformed replies, and logical rejections are mapped to
//! [`PlanQueryError::FederatedIndexCall`] before the caller can ack the batch, so a deferral
//! always retains the full suffix for idempotent replay.
//!
//! The client is constructed from the shard's text routing target. That field arrives with the
//! Router TEXT attach handshake (a separate in-flight slice); until then no production caller
//! constructs this client and pending text work defers inertly.

use crate::index::text_pending::{TextCanisterLookup, TextDocUpsert};
use crate::plan::PlanQueryError;
use async_trait::async_trait;
use candid::{CandidType, Principal};
use ic_cdk::call::Call;
use ic_cdk::call::CallFailed;

/// Candid client for the shard's derived text canister. `text_principal` is set by the Router
/// attach handshake; it must never be the anonymous principal.
#[derive(Clone, Debug)]
pub struct IcTextCanisterClient {
    pub text_principal: Principal,
}

const INGEST_TEXT_METHOD: &str = "ingest_text";
const DELETE_DOCS_METHOD: &str = "delete_docs";

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

/// Maps the canister's logical reply to the flush error type. A rejected call (e.g. a batch over
/// an admission cap) is a federated index failure so the flush defers instead of dropping.
fn map_canister_reply(op: &'static str, reply: Result<(), String>) -> Result<(), PlanQueryError> {
    reply.map_err(|detail| PlanQueryError::FederatedIndexCall { op, detail })
}

#[derive(CandidType)]
struct WireTextDoc {
    key: u64,
    text: String,
}

#[async_trait(?Send)]
// See vector_lookup: async_trait stamps a redundant `#[must_use]` on generated methods.
#[allow(clippy::double_must_use)]
impl TextCanisterLookup for IcTextCanisterClient {
    async fn ingest_text(&self, docs: &[TextDocUpsert]) -> Result<(), PlanQueryError> {
        let wire: Vec<WireTextDoc> = docs
            .iter()
            .map(|doc| WireTextDoc {
                key: doc.key,
                text: doc.text.clone(),
            })
            .collect();
        let reply: Result<(), String> = Call::bounded_wait(self.text_principal, INGEST_TEXT_METHOD)
            .with_args(&(wire,))
            .await
            .map_err(|error| ic_wait_err(INGEST_TEXT_METHOD, error))?
            .candid()
            .map_err(|_| ic_candid_decode_err(INGEST_TEXT_METHOD))?;
        map_canister_reply(INGEST_TEXT_METHOD, reply)
    }

    async fn delete_docs(&self, keys: &[u64]) -> Result<(), PlanQueryError> {
        let reply: Result<(), String> = Call::bounded_wait(self.text_principal, DELETE_DOCS_METHOD)
            .with_args(&(keys.to_vec(),))
            .await
            .map_err(|error| ic_wait_err(DELETE_DOCS_METHOD, error))?
            .candid()
            .map_err(|_| ic_candid_decode_err(DELETE_DOCS_METHOD))?;
        map_canister_reply(DELETE_DOCS_METHOD, reply)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ok_reply_is_acked() {
        assert!(map_canister_reply("ingest_text", Ok(())).is_ok());
    }

    #[test]
    fn logical_rejection_maps_to_federated_index_call() {
        let error = map_canister_reply(
            "delete_docs",
            Err("batch of 2 keys exceeds MAX_KEYS_PER_DELETE (1)".into()),
        )
        .expect_err("rejection must fail");
        assert!(
            matches!(
                error,
                PlanQueryError::FederatedIndexCall {
                    op: "delete_docs",
                    ..
                }
            ),
            "{error:?}"
        );
    }
}
