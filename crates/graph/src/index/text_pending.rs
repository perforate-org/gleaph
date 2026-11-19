//! Volatile queue of derived text-document mutations for the text canister (plan 0297).
//!
//! ## Op model and idempotency
//!
//! Each [`TextPendingOp`] addresses one document by `key` — the caller-owned `u64` vertex
//! identity — and ships the **raw** value; analysis happens canister-side. The canister surface
//! (`crates/text-canister`) makes replay convergent without compensation: re-ingesting a key is
//! delete+insert (last write wins), and deleting an unknown key is a deterministic no-op. Any
//! prefix of the op sequence may therefore be safely re-applied after an ambiguous delivery.
//! Text matches the vector model ([`crate::index::vector_pending`]), not the compensating
//! property-postings model ([`crate::index::pending`]), precisely because key-based replay
//! converges.
//!
//! ## Batch packing (deterministic)
//!
//! [`flush_pending`] stable-sorts queued ops by doc key (intra-key arrival order preserved) and
//! packs contiguous same-kind runs into batches that respect both the ≤2 MiB payload budget and
//! the per-call count limits of the canister surface (`ingest_text`: 1000 docs,
//! `delete_docs`: 1000 keys; authoritative constants live in `crates/text-canister/src/state.rs`
//! — the graph crate deliberately does not depend on it). Replicated timer paths stay
//! deterministic: no hash-order iteration anywhere.
//!
//! ## Ack watermark and failure semantics
//!
//! The queue head is the unacked boundary: a batch leaves the queue only after its delivery call
//! returns `Ok`. A failed batch leaves the remaining suffix queued **in order** (re-queued ahead
//! of any ops that arrived mid-flush) and returns
//! [`PlanQueryError::IndexFlushDeferred`](crate::plan::PlanQueryError::IndexFlushDeferred) with
//! the maintenance timer armed, so retries re-apply by key instead of dropping work. Unlike
//! property/vector flushes this batch is **not** appended to the durable repair journal in this
//! slice: the journal's wire envelope has no text variant yet and its owning files are outside
//! this slice's boundary (see "Upgrade window" below). When the journal is non-empty the timer
//! drains it first; text work defers until then so newer text ops never overtake older durable
//! entries.
//!
//! ## Writer rejection (parking)
//!
//! A text canister accepts deliveries only from a shard the Router attached as a writer. When it
//! refuses this shard ([`TextDeliveryError::WriterRejected`]), replaying the batch cannot succeed
//! until the Router re-attaches the shard, so the rejected definition is **parked**: its ops stay
//! queued in order, later flushes skip it without a call, and it no longer keeps the maintenance
//! timer armed. Other definitions keep delivering. Re-attachment (`admin_set_text_canister`, the
//! graph leg of the attach handshake, which always follows the text canister's writer
//! authorization) calls [`release_parked`], which delivers the parked log on the next flush. A
//! shard the Router unregistered is therefore idle instead of retrying a rejected batch forever.
//!
//! ## Under-posted lag semantics (what index-only readers may miss)
//!
//! While ops sit unconfirmed, canonical reads see fresh state but text search sees the last
//! **confirmed** state:
//!
//! - docs whose upsert has not been shipped are absent from results;
//! - superseded upserts still queued serve stale text;
//! - deletes/retypes not yet shipped keep removed or retyped docs searchable (over-posted);
//! - values over the canister's per-doc byte limit are never enqueued (admitted at dispatch),
//!   mirroring how unencodable property values skip posting creation — the doc stays
//!   canonical-only until a smaller value or a backfill covers it.
//!
//! ### Upgrade window (known gap, scope-forced)
//!
//! The queue is volatile: unconfirmed ops do not survive a canister upgrade in this slice.
//! Such loss converges only when the same property is written again or a text backfill replays
//! canonical data. Durable journaling for text ops lands with the slice that owns the repair
//! journal envelope and the Router TEXT attach handshake.

use crate::facade::GraphStore;
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

/// One derived document mutation awaiting delivery. The key is the vertex identity; the
/// (label, property) pair names the TEXT index definition whose target canister receives it, so a
/// graph with several text indexes delivers each index's ops only to that index's canister.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextPendingOp {
    pub(crate) label_id: u16,
    pub(crate) property_id: u32,
    pub(crate) key: u64,
    pub(crate) kind: TextPendingOpKind,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum TextPendingOpKind {
    /// Analyze + index `text` under `key` (delete+insert semantics canister-side).
    Upsert { text: String },
    /// Remove the document for `key`; unknown keys are deterministic no-ops at flush.
    Delete,
}

impl TextPendingOp {
    /// Deterministic estimate of the op's candid payload width used for batch budgeting. The
    /// estimate is per-op exact for deletes and upper-bounds upserts, so a packed batch can never
    /// exceed the budget through underestimation.
    fn payload_wire_bytes(&self) -> usize {
        match &self.kind {
            TextPendingOpKind::Upsert { text } => {
                KEY_WIRE_BYTES + LEN_PREFIX_WIRE_BYTES + text.len()
            }
            TextPendingOpKind::Delete => KEY_WIRE_BYTES,
        }
    }
}

/// One document upsert handed to a [`TextCanisterLookup`] implementation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextDocUpsert {
    pub(crate) key: u64,
    pub(crate) text: String,
}

/// Why one delivery call did not confirm. The distinction decides what the flush does with the
/// unconfirmed ops: replay them, or park their definition until the Router re-attaches the shard.
#[derive(Debug)]
pub(crate) enum TextDeliveryError {
    /// Transport failure, unavailable endpoint, malformed reply, or logical (admission)
    /// rejection: the call may be ambiguous or succeed later, so the suffix is replayed.
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
/// target yields `None`, which defers its queued ops without dropping them.
pub(crate) trait TextClientResolver {
    fn client_for(&self, label_id: u16, property_id: u32) -> Option<&dyn TextCanisterLookup>;
}

thread_local! {
    static PENDING: RefCell<Vec<TextPendingOp>> = const { RefCell::new(Vec::new()) };
    /// TEXT definitions `(label_id, property_id)` whose text canister rejected this shard as a
    /// writer (see "Writer rejection" in the module docs).
    static PARKED: RefCell<BTreeSet<(u16, u32)>> = const { RefCell::new(BTreeSet::new()) };
}

/// Number of queued text ops (test-only observability).
#[cfg(test)]
pub(crate) fn pending_len() -> usize {
    PENDING.with_borrow(Vec::len)
}

/// Clears the pending queue and the parked definitions. Not invoked at the start of each GQL
/// run: [`flush_pending`] may re-queue work after a failed delivery so a later update can retry.
pub(crate) fn clear_pending() {
    PENDING.with(|p| p.borrow_mut().clear());
    PARKED.with(|p| p.borrow_mut().clear());
}

/// Enqueues one derived document mutation. Inert on shards with no federation routing: text sync
/// is derived-state work that only exists relative to a routed text target.
pub(crate) fn push_text_op(op: TextPendingOp) {
    if !GraphStore::new().federation_configured() {
        return;
    }
    PENDING.with(|p| p.borrow_mut().push(op));
}

/// `true` while unconfirmed text work exists for a definition that is not parked. Consulted by
/// the maintenance-timer arm guard, pass tail, and flush so a deferred queue keeps retrying even
/// though it is not durable work, while parked work waits for re-attachment without a timer.
pub(crate) fn has_deliverable_work() -> bool {
    PARKED.with_borrow(|parked| {
        PENDING.with_borrow(|pending| {
            pending
                .iter()
                .any(|op| !parked.contains(&(op.label_id, op.property_id)))
        })
    })
}

fn is_parked(target: (u16, u32)) -> bool {
    PARKED.with_borrow(|parked| parked.contains(&target))
}

/// Releases a parked definition once the Router re-attaches this shard to its text canister
/// (`admin_set_text_canister`), and arms the maintenance timer so the parked log is delivered.
/// A no-op for a definition that is not parked.
pub(crate) fn release_parked(label_id: u16, property_id: u32) {
    let released = PARKED.with_borrow_mut(|parked| parked.remove(&(label_id, property_id)));
    if released {
        crate::facade::maintenance_timer::arm_if_needed();
    }
}

fn take_pending() -> Vec<TextPendingOp> {
    PENDING.with(|p| std::mem::take(&mut *p.borrow_mut()))
}

/// Prepends `ops` (oldest first) ahead of anything enqueued meanwhile, preserving intra-suffix
/// arrival order so same-key sequences keep their original ordering across retries.
fn requeue_front(ops: Vec<TextPendingOp>) {
    if ops.is_empty() {
        return;
    }
    PENDING.with(|p| {
        let mut queue = p.borrow_mut();
        let mut combined = ops;
        combined.append(&mut *queue);
        *queue = combined;
    });
}

/// One delivery unit: a contiguous run of sorted, same-kind ops within the canister call limits.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum TextFlushBatch {
    Ingest {
        label_id: u16,
        property_id: u32,
        docs: Vec<TextDocUpsert>,
    },
    Delete {
        label_id: u16,
        property_id: u32,
        keys: Vec<u64>,
    },
}

impl TextFlushBatch {
    /// The TEXT definition `(label_id, property_id)` this batch is delivered to.
    fn target(&self) -> (u16, u32) {
        match self {
            TextFlushBatch::Ingest {
                label_id,
                property_id,
                ..
            }
            | TextFlushBatch::Delete {
                label_id,
                property_id,
                ..
            } => (*label_id, *property_id),
        }
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        match self {
            TextFlushBatch::Ingest { docs, .. } => docs.len(),
            TextFlushBatch::Delete { keys, .. } => keys.len(),
        }
    }

    fn to_pending_ops(&self) -> Vec<TextPendingOp> {
        match self {
            TextFlushBatch::Ingest {
                label_id,
                property_id,
                docs,
            } => docs
                .iter()
                .map(|doc| TextPendingOp {
                    label_id: (*label_id).into(),
                    property_id: (*property_id).into(),
                    key: doc.key,
                    kind: TextPendingOpKind::Upsert {
                        text: doc.text.clone(),
                    },
                })
                .collect(),
            TextFlushBatch::Delete {
                label_id,
                property_id,
                keys,
            } => keys
                .iter()
                .map(|key| TextPendingOp {
                    label_id: (*label_id).into(),
                    property_id: (*property_id).into(),
                    key: *key,
                    kind: TextPendingOpKind::Delete,
                })
                .collect(),
        }
    }
}

/// Packs `ops` into deterministic batches: stable sort by (index, doc key), then contiguous
/// same-index, same-kind runs split whenever the next op would exceed the per-call count cap or
/// push the estimated payload past [`TEXT_FLUSH_PAYLOAD_BUDGET_BYTES`]. Sorting by index first
/// keeps every batch single-target, so one canister call never mixes index doc-key spaces; the
/// stable sort keeps same-key intra-index ops in arrival order. Delete runs are bounded only by
/// their count cap: 1000 keys × 8 wire bytes is far below the payload budget.
pub(crate) fn plan_batches(mut ops: Vec<TextPendingOp>) -> Vec<TextFlushBatch> {
    ops.sort_by_key(|op| (op.label_id, op.property_id, op.key));

    let mut batches = Vec::new();
    let mut current: Option<(u16, u32)> = None;
    let mut docs: Vec<TextDocUpsert> = Vec::new();
    let mut doc_payload_bytes = 0usize;
    let mut keys: Vec<u64> = Vec::new();

    let flush_docs = |batches: &mut Vec<TextFlushBatch>,
                      docs: &mut Vec<TextDocUpsert>,
                      doc_payload_bytes: &mut usize,
                      current: Option<(u16, u32)>| {
        if let Some((label_id, property_id)) = current
            && !docs.is_empty()
        {
            batches.push(TextFlushBatch::Ingest {
                label_id,
                property_id,
                docs: std::mem::take(docs),
            });
            *doc_payload_bytes = 0;
        }
    };
    let flush_keys =
        |batches: &mut Vec<TextFlushBatch>, keys: &mut Vec<u64>, current: Option<(u16, u32)>| {
            if let Some((label_id, property_id)) = current
                && !keys.is_empty()
            {
                batches.push(TextFlushBatch::Delete {
                    label_id,
                    property_id,
                    keys: std::mem::take(keys),
                });
            }
        };

    for op in ops {
        let spec = (op.label_id, op.property_id);
        if current != Some(spec) {
            flush_docs(&mut batches, &mut docs, &mut doc_payload_bytes, current);
            flush_keys(&mut batches, &mut keys, current);
            current = Some(spec);
        }
        let cost = op.payload_wire_bytes();
        match op.kind {
            TextPendingOpKind::Upsert { text } => {
                if !keys.is_empty() {
                    flush_keys(&mut batches, &mut keys, current);
                }
                if !docs.is_empty()
                    && (docs.len() >= TEXT_FLUSH_MAX_DOCS_PER_CALL
                        || doc_payload_bytes + cost > TEXT_FLUSH_PAYLOAD_BUDGET_BYTES)
                {
                    flush_docs(&mut batches, &mut docs, &mut doc_payload_bytes, current);
                }
                doc_payload_bytes += cost;
                docs.push(TextDocUpsert { key: op.key, text });
            }
            TextPendingOpKind::Delete => {
                if !docs.is_empty() {
                    flush_docs(&mut batches, &mut docs, &mut doc_payload_bytes, current);
                }
                if keys.len() >= TEXT_FLUSH_MAX_KEYS_PER_CALL {
                    flush_keys(&mut batches, &mut keys, current);
                }
                keys.push(op.key);
            }
        }
    }
    flush_docs(&mut batches, &mut docs, &mut doc_payload_bytes, current);
    flush_keys(&mut batches, &mut keys, current);
    batches
}

fn deferred(detail: String) -> PlanQueryError {
    PlanQueryError::IndexFlushDeferred {
        op: "text_flush",
        detail,
    }
}

/// Delivers queued text mutations to the text canister in deterministic batch order. A batch is
/// acked — dropped from the queue — only after its call returns `Ok`; everything from the first
/// retryable failure onward stays queued in order and the error is deferred, never dropped. A
/// writer rejection parks that batch's definition (its ops stay queued, see "Writer rejection")
/// and the flush continues with the other definitions.
pub(crate) async fn flush_pending(
    routers: Option<&dyn TextClientResolver>,
) -> Result<(), PlanQueryError> {
    if !GraphStore::new().federation_configured() {
        clear_pending();
        return Ok(());
    }
    if !has_deliverable_work() {
        return Ok(());
    }
    let Some(routers) = routers else {
        // No resolver to deliver through: leave the queue intact (unconfirmed work is never
        // dropped) and arm the timer so the maintenance pass retries.
        crate::facade::maintenance_timer::arm_if_needed();
        return Err(deferred("no text canister client".into()));
    };

    let batches = plan_batches(take_pending());
    // Ops of parked definitions, kept in batch order and requeued ahead of anything newer.
    let mut parked_ops: Vec<TextPendingOp> = Vec::new();
    let mut rejection: Option<TextDeliveryError> = None;
    for (index, batch) in batches.iter().enumerate() {
        let target = batch.target();
        if is_parked(target) {
            parked_ops.extend(batch.to_pending_ops());
            continue;
        }
        let Some(client) = routers.client_for(target.0, target.1) else {
            // A definition without a target defers its own queued work only; batches for other
            // indexes may still deliver. The untargeted suffix stays queued in order.
            requeue_unconfirmed(parked_ops, &batches[index..]);
            crate::facade::maintenance_timer::arm_if_needed();
            return Err(deferred("no text canister target for index".into()));
        };
        let result = match batch {
            TextFlushBatch::Ingest { docs, .. } => client.ingest_text(docs).await,
            TextFlushBatch::Delete { keys, .. } => client.delete_docs(keys).await,
        };
        match result {
            Ok(()) => {}
            Err(error @ TextDeliveryError::WriterRejected(_)) => {
                // Nothing was applied, and a replay is rejected the same way until the Router
                // re-attaches the shard: park the definition instead of retrying it.
                PARKED.with_borrow_mut(|parked| parked.insert(target));
                parked_ops.extend(batch.to_pending_ops());
                rejection = Some(error);
            }
            Err(error @ TextDeliveryError::Retryable(_)) => {
                // A transport failure makes the failed call itself ambiguous, so the whole suffix
                // from that batch onward is retained for idempotent key-based replay.
                requeue_unconfirmed(parked_ops, &batches[index..]);
                crate::facade::maintenance_timer::arm_if_needed();
                return Err(deferred(error.to_string()));
            }
        }
    }
    requeue_unconfirmed(parked_ops, &[]);
    match rejection {
        Some(error) => Err(deferred(error.to_string())),
        None => Ok(()),
    }
}

/// Requeues the parked ops followed by the unconfirmed batch suffix, ahead of any op enqueued
/// while the flush awaited.
fn requeue_unconfirmed(mut ops: Vec<TextPendingOp>, suffix: &[TextFlushBatch]) {
    ops.extend(suffix.iter().flat_map(TextFlushBatch::to_pending_ops));
    requeue_front(ops);
}

#[cfg(test)]
pub(crate) fn pending_snapshot() -> Vec<TextPendingOp> {
    PENDING.with(|p| p.borrow().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::FederationRouting;
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardId;
    use std::sync::Mutex;

    fn upsert(key: u64, text: &str) -> TextPendingOp {
        TextPendingOp {
            label_id: 1,
            property_id: 1,
            key,
            kind: TextPendingOpKind::Upsert {
                text: text.to_string(),
            },
        }
    }

    fn delete(key: u64) -> TextPendingOp {
        TextPendingOp {
            label_id: 1,
            property_id: 1,
            key,
            kind: TextPendingOpKind::Delete,
        }
    }

    fn indexed_upsert(label_id: u16, property_id: u32, key: u64, text: &str) -> TextPendingOp {
        TextPendingOp {
            label_id,
            property_id,
            key,
            kind: TextPendingOpKind::Upsert {
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
        let out = body(&graph);
        clear_pending();
        graph.set_federation_routing(None).expect("clear routing");
        out
    }

    #[test]
    fn plan_batches_sorts_by_key_and_splits_on_kind() {
        let batches = plan_batches(vec![delete(9), upsert(3, "b"), upsert(1, "a"), delete(2)]);
        assert_eq!(
            batches,
            vec![
                TextFlushBatch::Ingest {
                    label_id: 1,
                    property_id: 1,
                    docs: vec![TextDocUpsert {
                        key: 1,
                        text: "a".into()
                    }],
                },
                TextFlushBatch::Delete {
                    label_id: 1,
                    property_id: 1,
                    keys: vec![2],
                },
                TextFlushBatch::Ingest {
                    label_id: 1,
                    property_id: 1,
                    docs: vec![TextDocUpsert {
                        key: 3,
                        text: "b".into()
                    }],
                },
                TextFlushBatch::Delete {
                    label_id: 1,
                    property_id: 1,
                    keys: vec![9],
                },
            ]
        );
    }

    #[test]
    fn plan_batches_never_mix_index_targets_in_one_batch() {
        // Two indexes: their doc-key spaces live in different canisters, so a call must never
        // carry both. Interleaved enqueue order still plans two single-target batches.
        let batches = plan_batches(vec![
            indexed_upsert(1, 10, 1, "body"),
            indexed_upsert(1, 20, 1, "title"),
            indexed_upsert(1, 10, 2, "body2"),
        ]);
        assert_eq!(batches.len(), 2);
        assert_eq!(
            batches[0],
            TextFlushBatch::Ingest {
                label_id: 1,
                property_id: 10,
                docs: vec![
                    TextDocUpsert {
                        key: 1,
                        text: "body".into()
                    },
                    TextDocUpsert {
                        key: 2,
                        text: "body2".into()
                    },
                ],
            }
        );
        assert_eq!(
            batches[1],
            TextFlushBatch::Ingest {
                label_id: 1,
                property_id: 20,
                docs: vec![TextDocUpsert {
                    key: 1,
                    text: "title".into()
                }],
            }
        );
    }

    #[test]
    fn plan_batches_respects_per_call_count_limits() {
        let docs: Vec<TextPendingOp> = (0..TEXT_FLUSH_MAX_DOCS_PER_CALL as u64 + 1)
            .map(|k| upsert(k, "x"))
            .collect();
        let batches = plan_batches(docs);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), TEXT_FLUSH_MAX_DOCS_PER_CALL);
        assert_eq!(batches[1].len(), 1);

        let keys: Vec<TextPendingOp> = (0..TEXT_FLUSH_MAX_KEYS_PER_CALL as u64 + 1)
            .map(delete)
            .collect();
        let batches = plan_batches(keys);
        assert_eq!(batches.len(), 2);
        assert_eq!(batches[0].len(), TEXT_FLUSH_MAX_KEYS_PER_CALL);
        assert_eq!(batches[1].len(), 1);
    }

    #[test]
    fn plan_batches_respects_payload_budget() {
        // Two ~1.25 MiB documents: the second cannot join the first batch.
        let big = "x".repeat(TEXT_FLUSH_PAYLOAD_BUDGET_BYTES * 3 / 5);
        let batches = plan_batches(vec![upsert(1, &big), upsert(2, &big)]);
        assert_eq!(batches.len(), 2);
        for batch in &batches {
            let TextFlushBatch::Ingest { docs, .. } = batch else {
                panic!("expected ingest batches");
            };
            assert!(doc_payload_estimate(docs) <= TEXT_FLUSH_PAYLOAD_BUDGET_BYTES);
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
    fn same_key_arrival_order_survives_sorting() {
        // Stable sort preserves enqueue order within one key: remove-then-set stays
        // delete-before-upsert, so replay converges to the same final state.
        let batches = plan_batches(vec![delete(1), upsert(1, "later")]);
        assert_eq!(batches.len(), 2);
        assert_eq!(
            batches[0],
            TextFlushBatch::Delete {
                label_id: 1,
                property_id: 1,
                keys: vec![1],
            }
        );
        assert_eq!(
            batches[1],
            TextFlushBatch::Ingest {
                label_id: 1,
                property_id: 1,
                docs: vec![TextDocUpsert {
                    key: 1,
                    text: "later".into()
                }],
            }
        );
    }

    #[test]
    fn flush_delivers_sorted_batches_and_acks_everything() {
        with_routing(|_| {
            let canister = RecordingCanister::failing_at(0);
            for op in [upsert(3, "c"), delete(9), upsert(1, "a"), delete(2)] {
                push_text_op(op);
            }
            pollster::block_on(flush_pending(Some(&canister))).expect("flush succeeds");
            assert_eq!(
                canister.ingested_texts(),
                vec![(1, "a".into()), (3, "c".into())]
            );
            assert_eq!(canister.deleted_keys(), vec![2, 9]);
            assert!(pending_snapshot().is_empty());
        });
    }

    #[test]
    fn missing_client_defers_without_dropping_queue() {
        with_routing(|_| {
            push_text_op(upsert(7, "kept"));
            let err = pollster::block_on(flush_pending(None)).expect_err("no client → deferred");
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
            assert_eq!(pending_snapshot(), vec![upsert(7, "kept")]);
        });
    }

    #[test]
    fn failed_delivery_defers_and_keeps_full_suffix_in_order() {
        with_routing(|graph| {
            let canister = RecordingCanister::failing_at(1);
            for op in [upsert(1, "a"), upsert(2, "b")] {
                push_text_op(op);
            }
            let err =
                pollster::block_on(flush_pending(Some(&canister))).expect_err("first ingest fails");
            assert!(matches!(err, PlanQueryError::IndexFlushDeferred { .. }));
            // Nothing was acked: the ambiguous failed batch plus everything after it stays
            // queued in original order.
            assert_eq!(pending_snapshot(), vec![upsert(1, "a"), upsert(2, "b")]);
            assert!(graph.derived_index_outbox_pending_is_empty());

            // Retry against a healthy canister delivers every op exactly once, in key order.
            let healthy = RecordingCanister::failing_at(0);
            pollster::block_on(flush_pending(Some(&healthy))).expect("retry succeeds");
            assert_eq!(
                healthy.ingested_texts(),
                vec![(1, "a".into()), (2, "b".into())]
            );
            assert!(pending_snapshot().is_empty());
        });
    }

    #[test]
    fn confirmed_prefix_is_not_redelivered_after_mid_failure() {
        with_routing(|_| {
            // 1001 tiny docs → two batches; the second ingest call fails.
            let ops: Vec<TextPendingOp> = (0..=TEXT_FLUSH_MAX_DOCS_PER_CALL as u64)
                .map(|k| upsert(k, "x"))
                .collect();
            for op in ops {
                push_text_op(op);
            }
            let canister = RecordingCanister::failing_at(2);
            let err =
                pollster::block_on(flush_pending(Some(&canister))).expect_err("second batch fails");
            assert!(matches!(err, PlanQueryError::IndexFlushDeferred { .. }));

            let delivered = canister.ingested_texts();
            assert_eq!(delivered.len(), TEXT_FLUSH_MAX_DOCS_PER_CALL);
            assert_eq!(
                delivered.last().map(|(k, _)| *k),
                Some(TEXT_FLUSH_MAX_DOCS_PER_CALL as u64 - 1)
            );

            // Exactly the failed suffix remains, oldest-first.
            let remaining = pending_snapshot();
            assert_eq!(remaining.len(), 1);
            assert_eq!(remaining[0].key, TEXT_FLUSH_MAX_DOCS_PER_CALL as u64);

            let healthy = RecordingCanister::failing_at(0);
            pollster::block_on(flush_pending(Some(&healthy))).expect("suffix flush succeeds");
            assert_eq!(
                healthy.ingested_texts(),
                vec![(TEXT_FLUSH_MAX_DOCS_PER_CALL as u64, "x".into())]
            );
        });
    }

    #[test]
    fn unconfigured_shard_drops_pending() {
        let graph = GraphStore::new();
        graph.set_federation_routing(None).expect("clear routing");
        push_text_op(upsert(1, "a"));
        pollster::block_on(flush_pending(None)).expect("no-op when unconfigured");
        assert!(pending_snapshot().is_empty());
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

            let err = pollster::block_on(flush_pending(Some(&routes)))
                .expect_err("a writer rejection is reported");
            assert!(
                matches!(err, PlanQueryError::IndexFlushDeferred { op: "text_flush", ref detail } if detail.contains("parked")),
                "{err:?}"
            );
            // The rejected definition parks on its first refused call; its later batch is not
            // attempted, and the other definition still delivers.
            assert_eq!(rejecting.attempts.get(), 1);
            assert_eq!(healthy.ingested_texts(), vec![(5, "other index".into())]);
            assert_eq!(pending_snapshot(), vec![upsert(1, "a"), delete(3)]);
            assert!(
                !has_deliverable_work(),
                "parked work must not keep the maintenance timer armed"
            );

            // Later flushes and newer ops for the parked definition make no call at all.
            push_text_op(upsert(2, "b"));
            pollster::block_on(flush_pending(Some(&routes))).expect("parked work is idle");
            assert_eq!(rejecting.attempts.get(), 1);
            assert_eq!(pending_snapshot().len(), 3);

            // Re-attachment releases the definition and the parked log delivers in key order.
            release_parked(1, 1);
            assert!(has_deliverable_work());
            rejecting.rejects_writer.set(false);
            pollster::block_on(flush_pending(Some(&routes))).expect("re-attached flush");
            assert_eq!(
                rejecting.ingested_texts(),
                vec![(1, "a".into()), (2, "b".into())]
            );
            assert_eq!(rejecting.deleted_keys(), vec![3]);
            assert!(pending_snapshot().is_empty());
        });
    }

    #[test]
    fn release_of_an_unparked_definition_is_a_noop() {
        with_routing(|_| {
            push_text_op(upsert(1, "a"));
            release_parked(1, 1);
            assert!(has_deliverable_work());
            assert_eq!(pending_snapshot(), vec![upsert(1, "a")]);
        });
    }
}
