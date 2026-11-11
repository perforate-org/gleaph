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

use crate::index::text_pending::{TextCanisterLookup, TextClientResolver, TextDocUpsert};
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

/// One client per attached TEXT index definition (label, property). Resolves a queued op's
/// definition identity to the canister that owns that index's postings.
pub struct RoutedTextClients {
    clients: Vec<(u16, u32, IcTextCanisterClient)>,
}

impl RoutedTextClients {
    /// Builds the resolver from the shard's routing targets `(label_id, property_id, canister)`.
    pub fn new(targets: impl IntoIterator<Item = (u16, u32, Principal)>) -> Self {
        Self {
            clients: targets
                .into_iter()
                .map(|(label_id, property_id, canister)| {
                    (
                        label_id,
                        property_id,
                        IcTextCanisterClient {
                            text_principal: canister,
                        },
                    )
                })
                .collect(),
        }
    }
}

impl TextClientResolver for RoutedTextClients {
    fn client_for(&self, label_id: u16, property_id: u32) -> Option<&dyn TextCanisterLookup> {
        self.clients
            .iter()
            .find(|(label, property, _)| *label == label_id && *property == property_id)
            .map(|(_, _, client)| client as &dyn TextCanisterLookup)
    }
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

/// Flush every queued text op through the shard's routed text targets (wasm callers only).
///
/// The routing map is the single source of truth for which canister owns each (label, property)
/// definition, so DML and timer flushes share this one construction site.
#[cfg(target_family = "wasm")]
pub(crate) async fn flush_pending_from_routing() -> Result<(), PlanQueryError> {
    let routing = crate::facade::GraphStore::new().federation_routing();
    let clients = routing.as_ref().map(|routing| {
        RoutedTextClients::new(
            routing
                .text_targets
                .iter()
                .map(|target| (target.label_id, target.property_id, target.canister)),
        )
    });
    let resolvers = clients
        .as_ref()
        .map(|clients| clients as &dyn TextClientResolver);
    crate::index::text_pending::flush_pending(resolvers).await
}

#[derive(CandidType)]
struct WireTextDoc {
    key: u64,
    text: String,
}

#[async_trait(?Send)]
#[allow(
    clippy::double_must_use,
    reason = "see vector_lookup: async_trait stamps a redundant #[must_use] on generated methods"
)]
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
