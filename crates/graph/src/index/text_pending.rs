//! Derived text-document mutations for the text canister (plan 0297): staging, durable
//! persistence, and delivery.
//!
//! ## Lifecycle
//!
//! DML stages each derived op in a heap buffer while the mutation runs ([`push_text_op`]). When
//! the mutation commits — and before any read inside the same program that must see it — the
//! staged ops move to the durable derived-index outbox (MemoryId 46) as
//! [`DerivedIndexOutboxOp::TextDocument`](crate::facade::stable::derived_index_outbox::DerivedIndexOutboxOp)
//! rows ([`persist_pending`]). Delivery reads only those durable rows ([`drain_text_outbox`]) and
//! removes a row only after its canister accepted it, so unconfirmed text work survives traps,
//! retries, and canister upgrades.
//!
//! Text rows are outbox-only: they never enter the failure-only repair journal (MemoryId 41) or
//! the mutation-linked index pending floor, and the head-ordered property/vector drain skips them.
//! They are ordered per TEXT definition only, so a definition whose canister is down or refuses
//! this shard never holds up property, vector, or other text work.
//!
//! ## Op model and idempotency
//!
//! Each [`TextDocumentOp`] addresses one document by `key` — the caller-owned `u64` vertex
//! identity — and ships the **raw** value; analysis happens canister-side. The canister surface
//! (`crates/text-canister`) makes replay convergent without compensation: re-ingesting a key is
//! delete+insert (last write wins), and deleting an unknown key is a deterministic no-op. Any
//! prefix of the op sequence may therefore be safely re-applied after an ambiguous delivery.
//!
//! ## Batch packing (deterministic)
//!
//! [`plan_batches`] stable-sorts pending rows by (definition, doc key) — intra-key durable order
//! preserved — and packs contiguous same-kind runs into batches that respect both the ≤2 MiB
//! payload budget and the per-call count limits of the canister surface (`ingest_text`: 1000
//! docs, `delete_docs`: 1000 keys; authoritative constants live in
//! `crates/text-canister/src/state.rs` — the graph crate deliberately does not depend on it).
//! Replicated timer paths stay deterministic: no hash-order iteration anywhere.
//!
//! ## Failure semantics
//!
//! A batch's rows leave the outbox only after its delivery call returns `Ok`. The first failure
//! of a definition stops that definition for the rest of the drain, so its later batches (which
//! may carry newer ops for the same keys) wait behind the unconfirmed one; other definitions keep
//! delivering. A retryable failure or a definition with no routed target returns
//! [`PlanQueryError::IndexFlushDeferred`] with the maintenance timer armed.
//!
//! ## Writer rejection (parking)
//!
//! A text canister accepts deliveries only from a shard the Router attached as a writer. When it
//! refuses this shard ([`TextDeliveryError::WriterRejected`]), replaying cannot succeed until the
//! Router re-attaches the shard, so the definition is **parked**: its rows stay durable, later
//! drains skip it without a call, and it no longer keeps the maintenance timer armed.
//! Re-attachment (`admin_set_text_canister`, the graph leg of the attach handshake, which always
//! follows the text canister's writer authorization) calls [`release_parked`]. The parked set is
//! heap state: after an upgrade a parked definition is tried once more and parks again if still
//! refused, while its rows were never at risk.
//!
//! ## Under-posted lag semantics (what index-only readers may miss)
//!
//! While rows sit unconfirmed, canonical reads see fresh state but text search sees the last
//! **confirmed** state:
//!
//! - docs whose upsert has not been shipped are absent from results;
//! - superseded upserts still pending serve stale text;
//! - deletes/retypes not yet shipped keep removed or retyped docs searchable (over-posted);
//! - values over the canister's per-doc byte limit are never staged (admitted at dispatch),
//!   mirroring how unencodable property values skip posting creation — the doc stays
//!   canonical-only until a smaller value or a backfill covers it.

use crate::facade::GraphStore;
pub(crate) use crate::facade::stable::derived_index_outbox::{TextDocumentOp, TextDocumentOpKind};
use crate::plan::PlanQueryError;
use std::cell::RefCell;
use std::collections::BTreeSet;

/// ≤2 MiB payload budget per inter-canister call (plan 0297 brief).
pub(crate) const TEXT_FLUSH_PAYLOAD_BUDGET_BYTES: usize = 2 * 1024 * 1024;

/// Canister-side admission caps mirrored from `crates/text-canister/src/state.rs`
/// (`MAX_DOCS_PER_INGEST` / `MAX_KEYS_PER_DELETE`). Batching must respect both these counts and
/// [`TEXT_FLUSH_PAYLOAD_BUDGET_BYTES`].
const TEXT_FLUSH_MAX_DOCS_PER_CALL: usize = 1_000;
const TEXT_FLUSH_MAX_KEYS_PER_CALL: usize = 1_000;

/// Canister-side per-document admission cap mirrored from
/// `crates/text-canister/src/state.rs` (`MAX_TEXT_BYTES_PER_DOC`, UTF-8 bytes). Dispatch admits
/// only values at or under this bound; larger values would poison every batch containing them
/// (the canister rejects the whole call), so they are skipped under-posted instead.
pub(crate) const TEXT_INGEST_MAX_TEXT_BYTES_PER_DOC: usize = 65_536;

/// Candid wire width of one `u64` doc key.
const KEY_WIRE_BYTES: usize = 8;
/// Candid wire width of one `Vec<u8>`/`String` length prefix.
const LEN_PREFIX_WIRE_BYTES: usize = 4;

/// Deterministic estimate of one op's candid payload width used for batch budgeting. Exact for
/// deletes and an upper bound for upserts, so a packed batch can never exceed the budget through
/// underestimation.
fn payload_wire_bytes(op: &TextDocumentOp) -> usize {
    match &op.kind {
        TextDocumentOpKind::Upsert { text } => KEY_WIRE_BYTES + LEN_PREFIX_WIRE_BYTES + text.len(),
        TextDocumentOpKind::Delete => KEY_WIRE_BYTES,
    }
}

/// One document upsert handed to a [`TextCanisterLookup`] implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextDocUpsert {
    pub(crate) key: u64,
    pub(crate) text: String,
}

/// Why one delivery call did not confirm. The distinction decides what the drain does with the
/// unconfirmed rows: retry them later, or park their definition until the Router re-attaches the
/// shard.
#[derive(Debug)]
pub(crate) enum TextDeliveryError {
    /// Transport failure, unavailable endpoint, malformed reply, or logical (admission)
    /// rejection: the call may be ambiguous or succeed later, so the rows are replayed.
    Retryable(PlanQueryError),
    /// The text canister refused this shard as a document writer (its writer guard rejected the
    /// call before applying anything). Replays fail the same way until re-attachment.
    WriterRejected(PlanQueryError),
}

impl std::fmt::Display for TextDeliveryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            TextDeliveryError::Retryable(error) => write!(f, "{error}"),
            TextDeliveryError::WriterRejected(error) => {
                write!(
                    f,
                    "text writer rejected; parked until re-attachment: {error}"
                )
            }
        }
    }
}

/// Delivery interface to the text canister. Mirrors
/// [`crate::index::vector_lookup::VectorCanisterLookup`]: every failure surfaces as a
/// [`TextDeliveryError`] so the caller can defer or park without ambiguity about what was applied.
#[async_trait::async_trait(?Send)]
#[allow(
    clippy::double_must_use,
    reason = "see vector_lookup: async_trait stamps a redundant #[must_use] on generated methods"
)]
pub(crate) trait TextCanisterLookup {
    async fn ingest_text(&self, docs: &[TextDocUpsert]) -> Result<(), TextDeliveryError>;
    async fn delete_docs(&self, keys: &[u64]) -> Result<(), TextDeliveryError>;
}

/// Resolves the text-canister client that owns one TEXT index definition. Implementations map
/// the shard's routing targets (populated by the Router attach handshake); a definition with no
/// target yields `None`, which defers its rows without dropping them.
pub(crate) trait TextClientResolver {
    fn client_for(&self, label_id: u16, property_id: u32) -> Option<&dyn TextCanisterLookup>;
}

thread_local! {
    /// Ops staged by the running mutation; moved to the outbox by [`persist_pending`].
    static STAGED: RefCell<Vec<TextDocumentOp>> = const { RefCell::new(Vec::new()) };
    /// TEXT definitions `(label_id, property_id)` whose text canister rejected this shard as a
    /// writer (see "Writer rejection" in the module docs).
    static PARKED: RefCell<BTreeSet<(u16, u32)>> = const { RefCell::new(BTreeSet::new()) };
}

/// Number of staged text ops (test-only observability).
#[cfg(test)]
pub(crate) fn pending_len() -> usize {
    STAGED.with_borrow(Vec::len)
}

/// Clears the staging buffer and the parked definitions.
pub(crate) fn clear_pending() {
    STAGED.with(|p| p.borrow_mut().clear());
    PARKED.with(|p| p.borrow_mut().clear());
}

/// Stages one derived document mutation. Inert on shards with no federation routing: text sync
/// is derived-state work that only exists relative to a routed text target.
pub(crate) fn push_text_op(op: TextDocumentOp) {
    if !GraphStore::new().federation_configured() {
        return;
    }
    STAGED.with(|p| p.borrow_mut().push(op));
}

/// Moves the staged ops of the running mutation to the durable outbox and arms the maintenance
/// timer. Called wherever committed derived work becomes durable, and before a delivery attempt.
pub(crate) fn persist_pending(mutation_id: u64) {
    let staged = STAGED.with(|p| std::mem::take(&mut *p.borrow_mut()));
    if staged.is_empty() {
        return;
    }
    GraphStore::new().derived_index_text_outbox_append(mutation_id, staged);
    crate::facade::maintenance_timer::arm_if_needed();
}

/// `true` while durable text work exists for a definition that is not parked. Consulted by the
/// maintenance-timer arm guard and pass tail, so parked work waits for re-attachment without a
/// timer.
pub(crate) fn has_deliverable_work() -> bool {
    PARKED.with_borrow(|parked| {
        GraphStore::new()
            .derived_index_outbox_has_pending_text(|definition| !parked.contains(&definition))
    })
}

fn is_parked(definition: (u16, u32)) -> bool {
    PARKED.with_borrow(|parked| parked.contains(&definition))
}

/// Releases a parked definition once the Router re-attaches this shard to its text canister
/// (`admin_set_text_canister`), and arms the maintenance timer so its rows are delivered.
/// A no-op for a definition that is not parked.
pub(crate) fn release_parked(label_id: u16, property_id: u32) {
    let released = PARKED.with_borrow_mut(|parked| parked.remove(&(label_id, property_id)));
    if released {
        crate::facade::maintenance_timer::arm_if_needed();
    }
}

/// One delivery unit for one TEXT definition: a contiguous run of sorted, same-kind rows within
/// the canister call limits, with the outbox sequences it acknowledges.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TextFlushBatch {
    definition: (u16, u32),
    rows: Vec<(u64, TextDocumentOp)>,
    call: TextFlushCall,
    /// Sum of [`payload_wire_bytes`] over `rows`.
    payload_bytes: usize,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TextFlushCall {
    Ingest(Vec<TextDocUpsert>),
    Delete(Vec<u64>),
}

impl TextFlushBatch {
    fn new(definition: (u16, u32), kind: &TextDocumentOpKind) -> Self {
        let call = match kind {
            TextDocumentOpKind::Upsert { .. } => TextFlushCall::Ingest(Vec::new()),
            TextDocumentOpKind::Delete => TextFlushCall::Delete(Vec::new()),
        };
        Self {
            definition,
            rows: Vec::new(),
            call,
            payload_bytes: 0,
        }
    }

    /// `true` when `op` belongs to the same call kind as this batch.
    fn accepts_kind(&self, op: &TextDocumentOp) -> bool {
        matches!(
            (&self.call, &op.kind),
            (TextFlushCall::Ingest(_), TextDocumentOpKind::Upsert { .. })
                | (TextFlushCall::Delete(_), TextDocumentOpKind::Delete)
        )
    }

    /// `true` when adding `op` would exceed a per-call count cap or the payload budget.
    fn is_full_for(&self, op: &TextDocumentOp) -> bool {
        match &self.call {
            TextFlushCall::Ingest(docs) => {
                docs.len() >= TEXT_FLUSH_MAX_DOCS_PER_CALL
                    || self.payload_bytes + payload_wire_bytes(op) > TEXT_FLUSH_PAYLOAD_BUDGET_BYTES
            }
            TextFlushCall::Delete(keys) => keys.len() >= TEXT_FLUSH_MAX_KEYS_PER_CALL,
        }
    }

    fn push(&mut self, sequence: u64, op: TextDocumentOp) {
        match (&mut self.call, &op.kind) {
            (TextFlushCall::Ingest(docs), TextDocumentOpKind::Upsert { text }) => {
                docs.push(TextDocUpsert {
                    key: op.key,
                    text: text.clone(),
                });
            }
            (TextFlushCall::Delete(keys), TextDocumentOpKind::Delete) => keys.push(op.key),
            (TextFlushCall::Ingest(_), TextDocumentOpKind::Delete)
            | (TextFlushCall::Delete(_), TextDocumentOpKind::Upsert { .. }) => {
                unreachable!("plan_batches pushes only rows of the batch's kind")
            }
        }
        self.payload_bytes += payload_wire_bytes(&op);
        self.rows.push((sequence, op));
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.rows.len()
    }
}

/// Packs pending outbox rows into deterministic batches: stable sort by (definition, doc key),
/// then contiguous same-definition, same-kind runs split whenever the next row would exceed the
/// per-call count cap or push the estimated payload past [`TEXT_FLUSH_PAYLOAD_BUDGET_BYTES`].
/// Sorting by definition first keeps every batch single-target, so one canister call never mixes
/// index doc-key spaces; the stable sort keeps same-key rows in durable (sequence) order.
pub(crate) fn plan_batches(mut rows: Vec<(u64, TextDocumentOp)>) -> Vec<TextFlushBatch> {
    rows.sort_by_key(|(_, op)| (op.definition(), op.key));
    let mut batches: Vec<TextFlushBatch> = Vec::new();
    for (sequence, op) in rows {
        let continues = batches.last().is_some_and(|batch| {
            batch.definition == op.definition()
                && batch.accepts_kind(&op)
                && !batch.is_full_for(&op)
        });
        if !continues {
            batches.push(TextFlushBatch::new(op.definition(), &op.kind));
        }
        batches
            .last_mut()
            .expect("a batch was just ensured")
            .push(sequence, op);
    }
    batches
}

fn deferred(detail: String) -> PlanQueryError {
    PlanQueryError::IndexFlushDeferred {
        op: "text_flush",
        detail,
    }
}

/// Persists the staged ops, then delivers every pending text row of a non-parked definition.
/// See "Failure semantics" and "Writer rejection" in the module docs. `resolvers == None` (no
/// client can be built, e.g. native runs) leaves every row durable and defers.
pub(crate) async fn drain_text_outbox(
    resolvers: Option<&dyn TextClientResolver>,
) -> Result<(), PlanQueryError> {
    persist_pending(0);
    let store = GraphStore::new();
    let rows: Vec<_> = store
        .derived_index_outbox_pending_text()
        .into_iter()
        .filter(|(_, op)| !is_parked(op.definition()))
        .collect();
    if rows.is_empty() {
        return Ok(());
    }
    let Some(resolvers) = resolvers else {
        crate::facade::maintenance_timer::arm_if_needed();
        return Err(deferred("no text canister client".into()));
    };

    let mut stopped: BTreeSet<(u16, u32)> = BTreeSet::new();
    let mut failure: Option<String> = None;
    for batch in plan_batches(rows) {
        let (label_id, property_id) = batch.definition;
        if stopped.contains(&batch.definition) {
            continue;
        }
        let Some(client) = resolvers.client_for(label_id, property_id) else {
            stopped.insert(batch.definition);
            failure = Some("no text canister target for index".into());
            continue;
        };
        let result = match &batch.call {
            TextFlushCall::Ingest(docs) => client.ingest_text(docs).await,
            TextFlushCall::Delete(keys) => client.delete_docs(keys).await,
        };
        match result {
            Ok(()) => {
                for (sequence, op) in &batch.rows {
                    store.derived_index_outbox_remove_text(*sequence, op);
                }
            }
            Err(error @ TextDeliveryError::WriterRejected(_)) => {
                // Nothing was applied, and a replay is rejected the same way until the Router
                // re-attaches the shard: park the definition instead of retrying it.
                PARKED.with_borrow_mut(|parked| parked.insert(batch.definition));
                stopped.insert(batch.definition);
                failure = Some(error.to_string());
            }
            Err(error @ TextDeliveryError::Retryable(_)) => {
                // The call may have applied; its rows and the definition's later batches stay
                // durable for idempotent key-based replay.
                stopped.insert(batch.definition);
                failure = Some(error.to_string());
            }
        }
    }
    match failure {
        Some(detail) => {
            crate::facade::maintenance_timer::arm_if_needed();
            Err(deferred(detail))
        }
        None => Ok(()),
    }
}

#[cfg(test)]
pub(crate) fn pending_snapshot() -> Vec<TextDocumentOp> {
    STAGED.with(|p| p.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardId;
    use std::sync::Mutex;

    fn upsert(key: u64, text: &str) -> TextDocumentOp {
        TextDocumentOp {
            label_id: 1,
            property_id: 1,
            key,
            kind: TextDocumentOpKind::Upsert {
                text: text.to_string(),
            },
        }
    }

    fn delete(key: u64) -> TextDocumentOp {
        TextDocumentOp {
            label_id: 1,
            property_id: 1,
            key,
            kind: TextDocumentOpKind::Delete,
        }
    }

    fn indexed_upsert(label_id: u16, property_id: u32, key: u64, text: &str) -> TextDocumentOp {
        TextDocumentOp {
            label_id,
            property_id,
            key,
            kind: TextDocumentOpKind::Upsert {
                text: text.to_string(),
            },
        }
    }

    #[derive(Default)]
    enum RecordedCall {
        #[default]
        None,
        Ingest(Vec<TextDocUpsert>),
        Delete(Vec<u64>),
    }

    struct RecordingCanister {
        /// Calls in delivery order; empty-string texts stand in for delete key lists via
        /// [`RecordedCall::Delete`].
        calls: Mutex<Vec<RecordedCall>>,
        /// Number of the initial successful ingest calls; the ingest call at this 1-based index
        /// fails (0 = never fail). Delete calls always succeed.
        fail_ingest_at: usize,
        /// When set, every call is refused as an unattached writer (nothing is applied).
        rejects_writer: std::cell::Cell<bool>,
        /// Delivery calls attempted, including refused ones.
        attempts: std::cell::Cell<usize>,
    }

    impl RecordingCanister {
        fn failing_at(fail_ingest_at: usize) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_ingest_at,
                rejects_writer: std::cell::Cell::new(false),
                attempts: std::cell::Cell::new(0),
            }
        }

        fn rejecting_writer() -> Self {
            let canister = Self::failing_at(0);
            canister.rejects_writer.set(true);
            canister
        }

        /// Counts one attempt and answers the writer guard.
        fn guard(&self) -> Result<(), TextDeliveryError> {
            self.attempts.set(self.attempts.get() + 1);
            if self.rejects_writer.get() {
                return Err(TextDeliveryError::WriterRejected(
                    PlanQueryError::UnsupportedOp("test_writer_rejected"),
                ));
            }
            Ok(())
        }

        fn ingested_texts(&self) -> Vec<(u64, String)> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .flat_map(|call| match call {
                    RecordedCall::Ingest(docs) => docs
                        .iter()
                        .map(|d| (d.key, d.text.clone()))
                        .collect::<Vec<_>>(),
                    _ => Vec::new(),
                })
                .collect()
        }

        fn deleted_keys(&self) -> Vec<u64> {
            self.calls
                .lock()
                .unwrap()
                .iter()
                .flat_map(|call| match call {
                    RecordedCall::Delete(keys) => keys.clone(),
                    _ => Vec::new(),
                })
                .collect()
        }
    }

    impl TextClientResolver for RecordingCanister {
        fn client_for(&self, _label_id: u16, _property_id: u32) -> Option<&dyn TextCanisterLookup> {
            Some(self)
        }
    }

    #[async_trait::async_trait(?Send)]
    impl TextCanisterLookup for RecordingCanister {
        async fn ingest_text(&self, docs: &[TextDocUpsert]) -> Result<(), TextDeliveryError> {
            self.guard()?;
            let mut calls = self.calls.lock().unwrap();
            let ingest_count = calls
                .iter()
                .filter(|c| matches!(c, RecordedCall::Ingest(_)))
                .count()
                + 1;
            if ingest_count == self.fail_ingest_at {
                return Err(TextDeliveryError::Retryable(PlanQueryError::UnsupportedOp(
                    "test_ingest_fail",
                )));
            }
            // Record only confirmed deliveries, mirroring the canister's all-or-nothing batch
            // admission: a rejected call applies nothing.
            calls.push(RecordedCall::Ingest(docs.to_vec()));
            Ok(())
        }

        async fn delete_docs(&self, keys: &[u64]) -> Result<(), TextDeliveryError> {
            self.guard()?;
            self.calls
                .lock()
                .unwrap()
                .push(RecordedCall::Delete(keys.to_vec()));
            Ok(())
        }
    }

    /// Routes property 1 to `first` and every other definition to `second`.
    struct TwoTargets<'a> {
        first: &'a RecordingCanister,
        second: &'a RecordingCanister,
    }

    impl TextClientResolver for TwoTargets<'_> {
        fn client_for(&self, _label_id: u16, property_id: u32) -> Option<&dyn TextCanisterLookup> {
            Some(if property_id == 1 {
                self.first
            } else {
                self.second
            })
        }
    }

    fn with_routing<R>(body: impl FnOnce(&GraphStore) -> R) -> R {
        let graph = GraphStore::new();
        graph
            .set_federation_routing(Some(FederationRouting {
                router_canister: Principal::management_canister(),
                index_canister: Principal::management_canister(),
                shard_id: ShardId::new(0),
                vector_canister: Some(Principal::management_canister()),
                text_targets: Default::default(),
            }))
            .expect("set routing");
        clear_pending();
        graph.derived_index_owners_clear();
        let out = body(&graph);
        clear_pending();
        graph.derived_index_owners_clear();
        graph.set_federation_routing(None).expect("clear routing");
        out
    }

    /// Pending text ops in the durable outbox, oldest first.
    fn durable() -> Vec<TextDocumentOp> {
        GraphStore::new()
            .derived_index_outbox_pending_text()
            .into_iter()
            .map(|(_, op)| op)
            .collect()
    }

    /// `ops` as outbox rows with consecutive sequences, in the given (durable) order.
    fn rows(ops: Vec<TextDocumentOp>) -> Vec<(u64, TextDocumentOp)> {
        (0u64..).zip(ops).collect()
    }

    /// The (definition, call) of every planned batch.
    fn calls(batches: &[TextFlushBatch]) -> Vec<((u16, u32), &TextFlushCall)> {
        batches
            .iter()
            .map(|batch| (batch.definition, &batch.call))
            .collect()
    }

    fn ingest(docs: &[(u64, &str)]) -> TextFlushCall {
        TextFlushCall::Ingest(
            docs.iter()
                .map(|(key, text)| TextDocUpsert {
                    key: *key,
                    text: (*text).into(),
                })
                .collect(),
        )
    }

    #[test]
    fn plan_batches_sorts_by_key_and_splits_on_kind() {
        let batches = plan_batches(rows(vec![
            delete(9),
            upsert(3, "b"),
            upsert(1, "a"),
            delete(2),
        ]));
        assert_eq!(
            calls(&batches),
            vec![
                ((1, 1), &ingest(&[(1, "a")])),
                ((1, 1), &TextFlushCall::Delete(vec![2])),
                ((1, 1), &ingest(&[(3, "b")])),
                ((1, 1), &TextFlushCall::Delete(vec![9])),
            ]
        );
        // Each batch acknowledges exactly the outbox rows it carries.
        let sequences: Vec<Vec<u64>> = batches
            .iter()
            .map(|batch| batch.rows.iter().map(|(sequence, _)| *sequence).collect())
            .collect();
        assert_eq!(sequences, vec![vec![2], vec![3], vec![1], vec![0]]);
    }

    #[test]
    fn plan_batches_never_mix_index_targets_in_one_batch() {
        // Two indexes: their doc-key spaces live in different canisters, so a call must never
        // carry both. Interleaved durable order still plans two single-target batches.
        let batches = plan_batches(rows(vec![
            indexed_upsert(1, 10, 1, "body"),
            indexed_upsert(1, 20, 1, "title"),
            indexed_upsert(1, 10, 2, "body2"),
        ]));
        assert_eq!(
            calls(&batches),
            vec![
                ((1, 10), &ingest(&[(1, "body"), (2, "body2")])),
                ((1, 20), &ingest(&[(1, "title")])),
            ]
        );
    }

    #[test]
    fn plan_batches_respects_per_call_count_limits() {
        let docs: Vec<TextDocumentOp> = (0..TEXT_FLUSH_MAX_DOCS_PER_CALL as u64 + 1)
            .map(|k| upsert(k, "x"))
            .collect();
        let batches = plan_batches(rows(docs));
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), TEXT_FLUSH_MAX_DOCS_PER_CALL);
        assert_eq!(batches[1].len(), 1);

        let keys: Vec<TextDocumentOp> = (0..TEXT_FLUSH_MAX_KEYS_PER_CALL as u64 + 1)
            .map(delete)
            .collect();
        let batches = plan_batches(rows(keys));
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), TEXT_FLUSH_MAX_KEYS_PER_CALL);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn plan_batches_respects_payload_budget() {
        // Two ~1.25 MiB documents: the second cannot join the first batch.
        let big = "x".repeat(TEXT_FLUSH_PAYLOAD_BUDGET_BYTES * 3 / 5);
        let batches = plan_batches(rows(vec![upsert(1, &big), upsert(2, &big)]));
        assert_eq!(batches.len(), 2);
        for batch in &batches {
            let TextFlushCall::Ingest(docs) = &batch.call else {
                panic!("expected ingest batches");
            };
            assert!(doc_payload_estimate(docs) <= TEXT_FLUSH_PAYLOAD_BUDGET_BYTES);
            assert_eq!(batch.payload_bytes, doc_payload_estimate(docs));
        }
        assert_eq!(batches[0].len(), 1);
        assert_eq!(batches[1].len(), 1);
    }

    fn doc_payload_estimate(docs: &[TextDocUpsert]) -> usize {
        docs.iter()
            .map(|d| KEY_WIRE_BYTES + LEN_PREFIX_WIRE_BYTES + d.text.len())
            .sum()
    }

    #[test]
    fn same_key_durable_order_survives_sorting() {
        // Stable sort preserves durable order within one key: remove-then-set stays
        // delete-before-upsert, so replay converges to the same final state.
        let batches = plan_batches(rows(vec![delete(1), upsert(1, "later")]));
        assert_eq!(
            calls(&batches),
            vec![
                ((1, 1), &TextFlushCall::Delete(vec![1])),
                ((1, 1), &ingest(&[(1, "later")])),
            ]
        );
    }

    #[test]
    fn persist_moves_staged_ops_to_the_outbox_untracked_by_the_floor() {
        with_routing(|graph| {
            push_text_op(upsert(1, "a"));
            push_text_op(delete(2));
            assert_eq!(pending_len(), 2);
            assert!(durable().is_empty());

            persist_pending(42);

            assert_eq!(pending_len(), 0);
            assert_eq!(durable(), vec![upsert(1, "a"), delete(2)]);
            let entries = graph.derived_index_outbox_peek(usize::MAX);
            assert!(entries.iter().all(|(_, entry)| entry.mutation_id == 42));
            // Text rows are not mutation-linked index postings: no pending-floor key, and they
            // are invisible to the head-ordered (property/vector) drain.
            assert_eq!(graph.index_pending_min_mutation_id(), None);
            assert!(graph.derived_index_outbox_ordered_pending_is_empty());
            assert!(
                graph
                    .derived_index_outbox_peek_ordered(usize::MAX)
                    .is_empty()
            );
            assert!(has_deliverable_work());
        });
    }

    #[test]
    fn durable_rows_survive_loss_of_heap_state() {
        with_routing(|_| {
            push_text_op(upsert(1, "a"));
            persist_pending(7);
            // An upgrade discards every heap structure: the staging buffer and the parked set.
            clear_pending();

            assert_eq!(durable(), vec![upsert(1, "a")]);
            let canister = RecordingCanister::failing_at(0);
            pollster::block_on(drain_text_outbox(Some(&canister))).expect("drain succeeds");
            assert_eq!(canister.ingested_texts(), vec![(1, "a".into())]);
            assert!(durable().is_empty());
        });
    }

    #[test]
    fn drain_persists_staged_ops_then_delivers_and_acks_everything() {
        with_routing(|_| {
            let canister = RecordingCanister::failing_at(0);
            for op in [upsert(3, "c"), delete(9), upsert(1, "a"), delete(2)] {
                push_text_op(op);
            }
            pollster::block_on(drain_text_outbox(Some(&canister))).expect("drain succeeds");
            assert_eq!(
                canister.ingested_texts(),
                vec![(1, "a".into()), (3, "c".into())]
            );
            assert_eq!(canister.deleted_keys(), vec![2, 9]);
            assert_eq!(pending_len(), 0);
            assert!(durable().is_empty());
        });
    }

    #[test]
    fn missing_client_defers_and_keeps_rows_durable() {
        with_routing(|_| {
            push_text_op(upsert(7, "kept"));
            let err =
                pollster::block_on(drain_text_outbox(None)).expect_err("no client → deferred");
            assert!(
                matches!(
                    err,
                    PlanQueryError::IndexFlushDeferred {
                        op: "text_flush",
                        ..
                    }
                ),
                "{err:?}"
            );
            assert_eq!(durable(), vec![upsert(7, "kept")]);
        });
    }

    #[test]
    fn failed_delivery_defers_and_keeps_rows_in_order() {
        with_routing(|_| {
            let canister = RecordingCanister::failing_at(1);
            for op in [upsert(1, "a"), upsert(2, "b")] {
                push_text_op(op);
            }
            let err = pollster::block_on(drain_text_outbox(Some(&canister)))
                .expect_err("first ingest fails");
            assert!(matches!(err, PlanQueryError::IndexFlushDeferred { .. }));
            // Nothing was acked: the ambiguous batch stays durable in original order.
            assert_eq!(durable(), vec![upsert(1, "a"), upsert(2, "b")]);

            // Retry against a healthy canister delivers every op exactly once, in key order.
            let healthy = RecordingCanister::failing_at(0);
            pollster::block_on(drain_text_outbox(Some(&healthy))).expect("retry succeeds");
            assert_eq!(
                healthy.ingested_texts(),
                vec![(1, "a".into()), (2, "b".into())]
            );
            assert!(durable().is_empty());
        });
    }

    #[test]
    fn confirmed_prefix_is_not_redelivered_after_mid_failure() {
        with_routing(|_| {
            // 1001 tiny docs → two batches; the second ingest call fails.
            for key in 0..=TEXT_FLUSH_MAX_DOCS_PER_CALL as u64 {
                push_text_op(upsert(key, "x"));
            }
            let canister = RecordingCanister::failing_at(2);
            let err = pollster::block_on(drain_text_outbox(Some(&canister)))
                .expect_err("second batch fails");
            assert!(matches!(err, PlanQueryError::IndexFlushDeferred { .. }));

            let delivered = canister.ingested_texts();
            assert_eq!(delivered.len(), TEXT_FLUSH_MAX_DOCS_PER_CALL);
            assert_eq!(
                delivered.last().map(|(k, _)| *k),
                Some(TEXT_FLUSH_MAX_DOCS_PER_CALL as u64 - 1)
            );

            // Exactly the failed batch remains durable.
            let remaining = durable();
            assert_eq!(remaining.len(), 1);
            assert_eq!(remaining[0].key, TEXT_FLUSH_MAX_DOCS_PER_CALL as u64);

            let healthy = RecordingCanister::failing_at(0);
            pollster::block_on(drain_text_outbox(Some(&healthy))).expect("remaining row drains");
            assert_eq!(
                healthy.ingested_texts(),
                vec![(TEXT_FLUSH_MAX_DOCS_PER_CALL as u64, "x".into())]
            );
        });
    }

    #[test]
    fn a_failed_definition_stops_its_later_batches_but_not_other_definitions() {
        with_routing(|_| {
            // Property 1 fails its first ingest; its later delete batch (same definition) must
            // wait behind it, while property 2 still delivers.
            let failing = RecordingCanister::failing_at(1);
            let healthy = RecordingCanister::failing_at(0);
            let routes = TwoTargets {
                first: &failing,
                second: &healthy,
            };
            push_text_op(upsert(1, "a"));
            push_text_op(delete(5));
            push_text_op(indexed_upsert(1, 2, 9, "other"));

            pollster::block_on(drain_text_outbox(Some(&routes))).expect_err("deferred");
            assert_eq!(failing.attempts.get(), 1, "later batch was not attempted");
            assert!(failing.deleted_keys().is_empty());
            assert_eq!(healthy.ingested_texts(), vec![(9, "other".into())]);
            assert_eq!(durable(), vec![upsert(1, "a"), delete(5)]);
        });
    }

    #[test]
    fn unconfigured_shard_stages_nothing() {
        let graph = GraphStore::new();
        graph.set_federation_routing(None).expect("clear routing");
        push_text_op(upsert(1, "a"));
        assert_eq!(pending_len(), 0);
        pollster::block_on(drain_text_outbox(None)).expect("no-op when unconfigured");
        assert!(durable().is_empty());
    }

    #[test]
    fn acknowledgement_skips_a_row_that_no_longer_holds_the_delivered_op() {
        with_routing(|graph| {
            push_text_op(upsert(1, "a"));
            persist_pending(0);
            let (sequence, op) = graph.derived_index_outbox_pending_text().remove(0);
            // A concurrent drain acknowledged the row; a newer append reuses the sequence.
            graph.derived_index_outbox_remove(sequence);
            push_text_op(upsert(1, "newer"));
            persist_pending(0);
            assert_eq!(graph.derived_index_outbox_pending_text()[0].0, sequence);

            graph.derived_index_outbox_remove_text(sequence, &op);

            assert_eq!(durable(), vec![upsert(1, "newer")]);
        });
    }

    #[test]
    fn writer_rejection_parks_only_that_definition_until_reattached() {
        with_routing(|_| {
            let rejecting = RecordingCanister::rejecting_writer();
            let healthy = RecordingCanister::failing_at(0);
            let routes = TwoTargets {
                first: &rejecting,
                second: &healthy,
            };
            push_text_op(upsert(1, "a"));
            push_text_op(delete(3));
            push_text_op(indexed_upsert(1, 2, 5, "other index"));

            let err = pollster::block_on(drain_text_outbox(Some(&routes)))
                .expect_err("a writer rejection is reported");
            assert!(
                matches!(err, PlanQueryError::IndexFlushDeferred { op: "text_flush", ref detail } if detail.contains("parked")),
                "{err:?}"
            );
            // The rejected definition parks on its first refused call; its later batch is not
            // attempted, and the other definition still delivers.
            assert_eq!(rejecting.attempts.get(), 1);
            assert_eq!(healthy.ingested_texts(), vec![(5, "other index".into())]);
            assert_eq!(durable(), vec![upsert(1, "a"), delete(3)]);
            assert!(
                !has_deliverable_work(),
                "parked work must not keep the maintenance timer armed"
            );

            // Later drains and newer ops for the parked definition make no call at all.
            push_text_op(upsert(2, "b"));
            pollster::block_on(drain_text_outbox(Some(&routes))).expect("parked work is idle");
            assert_eq!(rejecting.attempts.get(), 1);
            assert_eq!(durable().len(), 3);

            // Re-attachment releases the definition and the parked rows deliver in key order.
            release_parked(1, 1);
            assert!(has_deliverable_work());
            rejecting.rejects_writer.set(false);
            pollster::block_on(drain_text_outbox(Some(&routes))).expect("re-attached drain");
            assert_eq!(
                rejecting.ingested_texts(),
                vec![(1, "a".into()), (2, "b".into())]
            );
            assert_eq!(rejecting.deleted_keys(), vec![3]);
            assert!(durable().is_empty());
        });
    }

    #[test]
    fn release_of_an_unparked_definition_is_a_noop() {
        with_routing(|_| {
            push_text_op(upsert(1, "a"));
            persist_pending(0);
            release_parked(1, 1);
            assert!(has_deliverable_work());
            assert_eq!(durable(), vec![upsert(1, "a")]);
        });
    }
}
