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

/// One derived document mutation awaiting delivery. The key is the vertex identity; no shard id
/// is embedded because Router placement targets exactly one text canister per graph shard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TextPendingOp {
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

/// Delivery interface to the text canister. Mirrors
/// [`crate::index::vector_lookup::VectorCanisterLookup`]: transport failures, unavailable
/// endpoints, malformed replies, and logical rejections all surface as [`PlanQueryError`] so the
/// caller can defer without ambiguity about what was applied.
#[async_trait::async_trait(?Send)]
// See vector_lookup: async_trait stamps a redundant `#[must_use]` on generated methods.
#[allow(clippy::double_must_use)]
pub(crate) trait TextCanisterLookup {
    async fn ingest_text(&self, docs: &[TextDocUpsert]) -> Result<(), PlanQueryError>;
    async fn delete_docs(&self, keys: &[u64]) -> Result<(), PlanQueryError>;
}

thread_local! {
    static PENDING: RefCell<Vec<TextPendingOp>> = const { RefCell::new(Vec::new()) };
}

/// Clears the pending queue. Not invoked at the start of each GQL run: [`flush_pending`] may
/// re-queue work after a failed delivery so a later update can retry.
pub(crate) fn clear_pending() {
    PENDING.with(|p| p.borrow_mut().clear());
}

/// Enqueues one derived document mutation. Inert on shards with no federation routing: text sync
/// is derived-state work that only exists relative to a routed text target.
pub(crate) fn push_text_op(op: TextPendingOp) {
    if !GraphStore::new().federation_configured() {
        return;
    }
    PENDING.with(|p| p.borrow_mut().push(op));
}

/// `true` while unconfirmed text work exists. Consulted by the maintenance-timer arm guard,
/// pass tail, and flush so a deferred queue keeps retrying even though it is not durable work.
pub(crate) fn pending_is_empty() -> bool {
    PENDING.with(|p| p.borrow().is_empty())
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
    Ingest(Vec<TextDocUpsert>),
    Delete(Vec<u64>),
}

impl TextFlushBatch {
    #[cfg(test)]
    fn len(&self) -> usize {
        match self {
            TextFlushBatch::Ingest(docs) => docs.len(),
            TextFlushBatch::Delete(keys) => keys.len(),
        }
    }

    fn to_pending_ops(&self) -> Vec<TextPendingOp> {
        match self {
            TextFlushBatch::Ingest(docs) => docs
                .iter()
                .map(|doc| TextPendingOp {
                    key: doc.key,
                    kind: TextPendingOpKind::Upsert {
                        text: doc.text.clone(),
                    },
                })
                .collect(),
            TextFlushBatch::Delete(keys) => keys
                .iter()
                .map(|key| TextPendingOp {
                    key: *key,
                    kind: TextPendingOpKind::Delete,
                })
                .collect(),
        }
    }
}

/// Packs `ops` into deterministic batches: stable sort by doc key, then contiguous same-kind runs
/// split whenever the next op would exceed the per-call count cap or push the estimated payload
/// past [`TEXT_FLUSH_PAYLOAD_BUDGET_BYTES`]. Delete runs are bounded only by their count cap:
/// 1000 keys × 8 wire bytes is far below the payload budget.
pub(crate) fn plan_batches(mut ops: Vec<TextPendingOp>) -> Vec<TextFlushBatch> {
    // Stable sort keeps same-key ops in arrival order (an upsert before its later delete must
    // stay ordered); cross-key order becomes canonical regardless of enqueue interleaving.
    ops.sort_by_key(|op| op.key);

    let mut batches = Vec::new();
    let mut docs: Vec<TextDocUpsert> = Vec::new();
    let mut doc_payload_bytes = 0usize;
    let mut keys: Vec<u64> = Vec::new();

    for op in ops {
        let cost = op.payload_wire_bytes();
        match op.kind {
            TextPendingOpKind::Upsert { text } => {
                if !keys.is_empty() {
                    batches.push(TextFlushBatch::Delete(std::mem::take(&mut keys)));
                }
                if !docs.is_empty()
                    && (docs.len() >= TEXT_FLUSH_MAX_DOCS_PER_CALL
                        || doc_payload_bytes + cost > TEXT_FLUSH_PAYLOAD_BUDGET_BYTES)
                {
                    batches.push(TextFlushBatch::Ingest(std::mem::take(&mut docs)));
                    doc_payload_bytes = 0;
                }
                doc_payload_bytes += cost;
                docs.push(TextDocUpsert { key: op.key, text });
            }
            TextPendingOpKind::Delete => {
                if !docs.is_empty() {
                    batches.push(TextFlushBatch::Ingest(std::mem::take(&mut docs)));
                    doc_payload_bytes = 0;
                }
                if keys.len() >= TEXT_FLUSH_MAX_KEYS_PER_CALL {
                    batches.push(TextFlushBatch::Delete(std::mem::take(&mut keys)));
                }
                keys.push(op.key);
            }
        }
    }
    if !docs.is_empty() {
        batches.push(TextFlushBatch::Ingest(docs));
    }
    if !keys.is_empty() {
        batches.push(TextFlushBatch::Delete(keys));
    }
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
/// failed batch onward stays queued in order and the error is deferred, never dropped.
pub(crate) async fn flush_pending(
    text: Option<&dyn TextCanisterLookup>,
) -> Result<(), PlanQueryError> {
    if !GraphStore::new().federation_configured() {
        clear_pending();
        return Ok(());
    }
    if pending_is_empty() {
        return Ok(());
    }
    let Some(text) = text else {
        // No client to deliver to: leave the queue intact (unconfirmed work is never dropped)
        // and arm the timer so the maintenance pass retries.
        crate::facade::maintenance_timer::arm_if_needed();
        return Err(deferred("no text canister client".into()));
    };

    let batches = plan_batches(take_pending());
    for (index, batch) in batches.iter().enumerate() {
        let result = match batch {
            TextFlushBatch::Ingest(docs) => text.ingest_text(docs).await,
            TextFlushBatch::Delete(keys) => text.delete_docs(keys).await,
        };
        if let Err(error) = result {
            // A transport failure makes the failed call itself ambiguous, so the whole suffix
            // from that batch onward is retained for idempotent key-based replay.
            let mut suffix: Vec<TextPendingOp> = batch.to_pending_ops();
            suffix.extend(batches[index + 1..].iter().flat_map(|b| b.to_pending_ops()));
            requeue_front(suffix);
            crate::facade::maintenance_timer::arm_if_needed();
            return Err(deferred(error.to_string()));
        }
    }
    Ok(())
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
            key,
            kind: TextPendingOpKind::Upsert {
                text: text.to_string(),
            },
        }
    }

    fn delete(key: u64) -> TextPendingOp {
        TextPendingOp {
            key,
            kind: TextPendingOpKind::Delete,
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
    }

    impl RecordingCanister {
        fn failing_at(fail_ingest_at: usize) -> Self {
            Self {
                calls: Mutex::new(Vec::new()),
                fail_ingest_at,
            }
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

    #[async_trait::async_trait(?Send)]
    impl TextCanisterLookup for RecordingCanister {
        async fn ingest_text(&self, docs: &[TextDocUpsert]) -> Result<(), PlanQueryError> {
            let mut calls = self.calls.lock().unwrap();
            let ingest_count = calls
                .iter()
                .filter(|c| matches!(c, RecordedCall::Ingest(_)))
                .count()
                + 1;
            if ingest_count == self.fail_ingest_at {
                return Err(PlanQueryError::UnsupportedOp("test_ingest_fail"));
            }
            // Record only confirmed deliveries, mirroring the canister's all-or-nothing batch
            // admission: a rejected call applies nothing.
            calls.push(RecordedCall::Ingest(docs.to_vec()));
            Ok(())
        }

        async fn delete_docs(&self, keys: &[u64]) -> Result<(), PlanQueryError> {
            self.calls
                .lock()
                .unwrap()
                .push(RecordedCall::Delete(keys.to_vec()));
            Ok(())
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
                TextFlushBatch::Ingest(vec![TextDocUpsert {
                    key: 1,
                    text: "a".into()
                }]),
                TextFlushBatch::Delete(vec![2]),
                TextFlushBatch::Ingest(vec![TextDocUpsert {
                    key: 3,
                    text: "b".into()
                }]),
                TextFlushBatch::Delete(vec![9]),
            ]
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
            let TextFlushBatch::Ingest(docs) = batch else {
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
        assert_eq!(batches[0], TextFlushBatch::Delete(vec![1]));
        assert_eq!(
            batches[1],
            TextFlushBatch::Ingest(vec![TextDocUpsert {
                key: 1,
                text: "later".into()
            }])
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
}
