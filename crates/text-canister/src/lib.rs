//! Text Index canister (`text-canister`) — v0 Candid surface over the ADR 0077 engine.
//!
//! Owns the production analyzer ([`analyzer`]) and all stable index state
//! ([`state`]): segments, dictionary, postings, block-max tables, tombstones, stats,
//! the pending ops log, and the resumable merge cursor. Physical kernels — posting
//! codecs, block-max tables, merge cursors, and the DAAT/block-max top-k driver — come
//! from `ic-stable-text-postings`; this canister supplies every score part at query
//! time (v0 identity scorer: weight + stored tf; catalog-driven formulas land later).
//!
//! ## Surface (v0)
//!
//! - `ingest_text(docs)` / `delete_docs(keys)`: durable pending-log appends; searchable
//!   after bounded `admin_flush` steps apply them (under-posted-until-flush lag).
//! - `search(query, k)`: promoted driver over live postings minus tombstones;
//!   deterministic (score desc, docid asc).
//! - `admin_flush()` / `admin_merge_step(budget)`: controller-guarded bounded steps.
//! - `get_stats()`: O(1)-ish counters from the meta/stats cells and log length.
//!
//! Per-call loop budgets are documented constants in [`state`]; violations reject the
//! whole call before any mutation. Router fan-out wiring is a later slice: DML/read
//! endpoints are intentionally unguarded at this stage.

#![cfg_attr(all(feature = "canbench", target_family = "wasm"), no_main)]

pub mod analyzer;
mod analyzer_mecab;
mod analyzer_multilingual;
mod backfill;
mod guards;
mod init;
mod state;

#[cfg(feature = "canbench")]
mod bench;

use candid::CandidType;
use ic_cdk_macros::{init, post_upgrade, query, update};
use serde::{Deserialize, Serialize};

#[allow(deprecated)] // deprecated alias kept for the v1 test references across the workspace
pub use analyzer::{
    ANALYZER_ID, ANALYZER_KOREAN, ANALYZER_MECAB, ANALYZER_MULTILINGUAL, ANALYZER_UNICODE_BIGRAM,
};
pub use backfill::{
    RegisterTextBackfillRequest, TextBackfillControl, TextBackfillPhase, TextBackfillScope,
    TextBackfillSealProof, TextBackfillStatus,
};
/// Dictionary-relay protocol types (plans 0335/0342). SINGLE SOURCE OF TRUTH is the shared
/// kernel module (`gleaph_graph_kernel::provisioning::dictionary`) — re-exported here for the
/// canister's Candid surface.
pub use gleaph_graph_kernel::provisioning::dictionary::{
    CompressedDictFinalize, CompressedDictUpload, DictState, DictStatus,
    MAX_DICT_COMPRESSED_CHUNK_BYTES,
};
pub use init::TextCanisterInitArgs;
pub use state::MAX_DICT_CHUNK_BYTES;
/// v0 identity-scorer constant weight (see `state` module docs); part of the observable
/// search contract until catalog-driven scoring lands.
pub use state::WEIGHT_BASE;

/// One document to index. Keys are caller-owned u64 identities (vertex ids once the
/// Router wires DML); re-ingesting a key updates it (delete + insert semantics).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TextDoc {
    /// Caller-owned document identity.
    pub key: u64,
    /// Raw document text; analyzed by the production analyzer at enqueue time.
    pub text: String,
}

/// One ranked hit. Ordering of the returned vector is the deterministic contract:
/// score descending, then docid ascending among equal scores.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct TextHit {
    /// Caller-owned key this docid was ingested under.
    pub key: u64,
    /// Internal dense docid (monotonic per ingest; exposes the tie-break order).
    pub docid: u32,
    /// Integer fixed-point score (identity scorer for v0).
    pub score: u32,
}

/// Bounded flush-step outcome; repeat until `done`.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct FlushReport {
    /// Pending ops applied during this call.
    pub drained_ops: u64,
    /// Ops still awaiting application.
    pub remaining_ops: u64,
    /// True when the pending log is fully applied.
    pub done: bool,
}

/// Bounded merge-step outcome; repeat until `done`.
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub struct MergeStepReport {
    /// Terms whose postings were examined this step.
    pub terms_processed: u64,
    /// Physically reclaimed (tombstoned) posting units this step.
    pub units_reclaimed: u64,
    /// True when every term has been reclaimed and tombstones cleared.
    pub done: bool,
}

/// Global stats record (mirrors the stable `TextStats` cell plus meta counters).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct TextIndexStats {
    /// Registered analyzer pipeline id ([`ANALYZER_ID`] for v0).
    pub analyzer_id: u32,
    /// Live (non-tombstoned) documents.
    pub ndocs: u64,
    /// Live posted units across all terms (tombstoned units excluded only after reclaim).
    pub total_units: u64,
    /// Tombstoned docids awaiting physical reclaim (transiently includes inert bits
    /// mid-merge-pass).
    pub tombstoned_docs: u64,
    /// Pending ops not yet applied by `admin_flush`.
    pub pending_ops: u64,
    /// Segment registry size (1 in v0: one active segment).
    pub segments: u32,
    /// Next docid to allocate (monotonic; equals docs ever ingested).
    pub next_docid: u32,
}

/// Init accepts the candid-encoded [`TextCanisterInitArgs`]; EMPTY argument bytes (as
/// sent by bare wasm installs such as canbench) fall back to defaults instead of
/// trapping, leaving the deny-all controller sentinel.
#[init]
fn init() {
    let raw = ic_cdk::api::msg_arg_data();
    let args: Option<TextCanisterInitArgs> = if raw.is_empty() {
        None
    } else {
        Some(
            candid::decode_args::<(TextCanisterInitArgs,)>(&raw)
                .expect("decode init args")
                .0,
        )
    };
    // Plan 0331: the pinned analyzer id rides the init args, validated fail-closed at
    // the open (∈ {1, 2}); an unset/omitted field defaults to the unicode-bigram
    // pipeline (bare wasm installs such as canbench keep working unchanged).
    state::set_init_analyzer(args.as_ref().and_then(|a| a.analyzer_id));
    state::with_stores(|stores| {
        stores.set_controller(args.as_ref().and_then(|a| a.controller));
        // Plan 0335 §5-2: the Provision relay caller rides the init args; `None`/anonymous
        // denies everyone on the two relay endpoints (fail-closed).
        stores.set_dict_relay_caller(args.as_ref().and_then(|a| a.dict_relay_caller));
    });
}

/// Upgrade reopen: rebinds the store through the same validated open path as first use;
/// incompatible layout bytes trap here instead of serving wrong state.
#[post_upgrade]
fn post_upgrade() {
    state::with_stores(|_| {});
}

/// Analyzes and durably logs one upsert op per document. Bounded by
/// `MAX_DOCS_PER_INGEST` / `MAX_TEXT_BYTES_PER_DOC` / `MAX_UNITS_PER_DOC`; any violation
/// rejects the whole batch before the first append.
#[update]
fn ingest_text(docs: Vec<TextDoc>) -> Result<(), String> {
    state::with_stores(|stores| stores.enqueue_ingest(docs))
}

/// Durably logs delete ops (update = delete + insert on re-ingest). Bounded by
/// `MAX_KEYS_PER_DELETE`. Unknown keys are deterministic no-ops at flush.
#[update]
fn delete_docs(keys: Vec<u64>) -> Result<(), String> {
    state::with_stores(|stores| stores.enqueue_delete(keys))
}

/// Read-only ranked search over flushed live postings minus tombstones. `k` clamps to
/// `MAX_SEARCH_K`; an empty/unknown-term query returns an empty result rather than an
/// error.
#[query]
fn search(query: String, k: u32) -> Result<Vec<TextHit>, String> {
    state::with_stores(|stores| stores.search(&query, k))
}

/// Controller-guarded bounded tombstone-reclaim step (physical exactness after deletes).
/// `budget = 0` is rejected fail-closed; larger budgets clamp to
/// `MAX_MERGE_TERMS_PER_STEP`. Repeat until `done`.
#[update(guard = "guards::guard_controller")]
fn admin_merge_step(budget: u32) -> Result<MergeStepReport, String> {
    if budget == 0 {
        return Err("merge budget must be >= 1".to_string());
    }
    Ok(state::with_stores(|stores| stores.merge_step(budget)))
}

// -- Analyzer-2 dictionary upload (plan 0331, ADR 0087 chunk analogy) -----------------------

/// Relay-guarded append of one dictionary chunk (≤ [`MAX_DICT_CHUNK_BYTES`] raw / [`MAX_DICT_COMPRESSED_CHUNK_BYTES`] framed).
/// Accepts the stored controller (Router) OR the provision relay caller (plan 0335 §5-2).
/// Wire (plan 0353): trailing `opt` mode — `None` = raw (byte-identical behavior), `Some`
/// = framed (plan 0342: verify THIS frame's digest, decode it immediately, append the raw
/// bytes to region 16 — compressed bytes are never persisted). Fail-closed unless this
/// index pins a dictionary-carrying analyzer (id 0 or id 2) and the dictionary is not yet
/// finalized; returns the new TOTAL raw length of region 16.
#[update(guard = "guards::guard_controller_or_relay_caller")]
fn admin_upload_dict_chunk(
    bytes: Vec<u8>,
    compressed: Option<CompressedDictUpload>,
) -> Result<u64, String> {
    state::with_stores(|stores| stores.upload_dict_chunk(bytes, compressed))
}

/// Relay-guarded dictionary finalize (trailing `opt` mode per plan 0335). RAW mode:
/// verifies the streaming identity (xxh3_128 over the concatenated region) against
/// `expected_digest`, then structurally validates + materializes the resident set
/// (plan 0334 rebind: validate + resident memcpy, NO decode). FRAMED mode (plan 0342):
/// verifies the ACCUMULATED raw digest (streamed over region 16 during upload) + the total
/// raw length against the declared record, then runs the same validation. Accepts the
/// stored controller (Router) OR the provision relay caller (plan 0335 §5-2). Idempotent
/// on an exact Finalized replay in both modes; any failure rejects WITHOUT recording
/// Finalized state (framed failures reset region 16 + frame counter so a retry re-streams
/// from frame 0).
#[update(guard = "guards::guard_controller_or_relay_caller")]
fn admin_finalize_dict_upload(
    expected_digest: u128,
    compressed: Option<CompressedDictFinalize>,
) -> Result<DictStatus, String> {
    state::with_stores(|stores| stores.finalize_dict_upload(expected_digest, compressed))
}

/// Read-only dictionary status (state / digest / len).
#[query]
fn admin_get_dict_status() -> DictStatus {
    state::with_stores(|stores| stores.dict_status())
}

/// Controller-guarded bounded flush step: applies pending ops FIFO into the active
/// segment. Repeat until `done` to make ingested documents searchable.
#[update(guard = "guards::guard_controller")]
fn admin_flush() -> FlushReport {
    state::with_stores(|stores| stores.flush_step(state::FLUSH_OPS_BUDGET))
}

/// O(1)-ish counters from the meta cell, stats cell, and pending log length.
#[query]
fn get_stats() -> TextIndexStats {
    state::with_stores(|stores| stores.get_stats())
}

// -- Text backfill pull worker (ADR 0059 §Text build kind) ---------------------------------

/// Controller-guarded registration of one immutable text backfill identity. Fail-closed
/// validation precedes any effect; an exact replay returns the durable status, a
/// conflicting identity is rejected without touching the registered build.
#[update(guard = "guards::guard_controller")]
fn admin_register_text_backfill(
    registration: RegisterTextBackfillRequest,
) -> Result<TextBackfillStatus, String> {
    backfill::with_cells(|cells| {
        // Plan 0331: analyzer-2 registrations hold until the dictionary is finalized.
        let dict_finalized =
            state::with_stores(|stores| stores.dict_status().state == crate::DictState::Finalized);
        backfill::register_text_backfill(cells, registration, dict_finalized)
    })
}

/// Controller-guarded bounded pull step: up to `min(budget, MAX_INDEX_BUILD_ADVANCE_PAGES)`
/// iterations of prepare → fetch one raw-text canonical export page from the home Graph
/// shard → analyze + ingest + cursor advance. No stable mutation happens before a fully
/// decoded successful reply; repeat until [`TextBackfillStatus::done`].
#[update(guard = "guards::guard_controller")]
async fn admin_advance_text_backfill(
    control: TextBackfillControl,
    budget: u32,
) -> Result<TextBackfillStatus, String> {
    backfill::advance_text_backfill_with(control, budget, backfill::fetch_index_export_page).await
}

/// Controller-guarded seal fence: captures the Router proof epoch after the base scan
/// converged. An identical proof replays exactly; anything else fails closed.
#[update(guard = "guards::guard_controller")]
fn admin_seal_text_backfill(
    control: TextBackfillControl,
    proof: TextBackfillSealProof,
) -> Result<TextBackfillStatus, String> {
    backfill::with_cells(|cells| backfill::seal_text_backfill(cells, &control, &proof))
}

/// Controller-guarded terminal abort: clears the resumable pull state; ingested documents
/// remain replay-safe. Idempotent; the aborted identity is never reusable.
#[update(guard = "guards::guard_controller")]
fn admin_abort_text_backfill(control: TextBackfillControl) -> Result<TextBackfillStatus, String> {
    backfill::with_cells(|cells| backfill::abort_text_backfill(cells, &control))
}

/// Read-only status for the Router convergence poll (`None` before any registration).
#[query]
fn get_text_backfill_status() -> Result<Option<TextBackfillStatus>, String> {
    Ok(backfill::with_cells(|cells| {
        backfill::text_backfill_status(cells)
    }))
}

#[cfg(test)]
mod wire_contract_tests {
    use candid::{decode_args, encode_args};
    use gleaph_graph_kernel::provisioning::dictionary::MAX_DICT_COMPRESSED_CHUNK_BYTES;

    use crate::{CompressedDictFinalize, CompressedDictUpload};

    /// Sender/receiver encoding match (the agreed plan-0335 completion condition): the
    /// relay endpoints gained a trailing `opt` mode argument, so OLD single-arg raw
    /// senders must decode under the NEW receivers with the mode filled as `None`
    /// (Candid optional-field evolution), and the framed sender shape must round-trip.
    #[test]
    fn old_raw_single_arg_sender_decodes_as_none_mode() {
        // Old upload sender `(blob)` under the new receiver `(blob, opt CompressedDictUpload)`.
        let raw = encode_args((vec![1u8, 2, 3],)).unwrap();
        let decoded: (Vec<u8>, Option<CompressedDictUpload>) = decode_args(&raw).unwrap();
        assert_eq!(decoded, (vec![1, 2, 3], None));

        // Old finalize sender `(nat)` under the new receiver `(nat, opt CompressedDictFinalize)`.
        let raw = encode_args((u128::MAX,)).unwrap();
        let decoded: (u128, Option<CompressedDictFinalize>) = decode_args(&raw).unwrap();
        assert_eq!(decoded, (u128::MAX, None));
    }

    #[test]
    fn framed_sender_shapes_roundtrip_against_receivers() {
        let upload = CompressedDictUpload {
            frame_digest: 0xDEADBEEF,
            raw_len: 52_931_159,
        };
        let bytes = encode_args((vec![7u8; 16], Some(upload.clone()))).unwrap();
        let decoded: (Vec<u8>, Option<CompressedDictUpload>) = decode_args(&bytes).unwrap();
        assert_eq!(decoded, (vec![7; 16], Some(upload)));

        let finalize = CompressedDictFinalize {
            raw_digest: 0xCCDD,
            raw_len: 52_931_159,
        };
        let bytes = encode_args((finalize.raw_digest, Some(finalize.clone()))).unwrap();
        let decoded: (u128, Option<CompressedDictFinalize>) = decode_args(&bytes).unwrap();
        assert_eq!(decoded, (finalize.raw_digest, Some(finalize)));
    }

    #[test]
    fn max_compressed_chunk_call_fits_the_2mib_cross_subnet_limit() {
        // A full-size compressed chunk call plus its Candid metadata (the receiver's arg
        // tuple) must stay under the 2 MiB cross-subnet inter-canister payload limit.
        let args = (
            vec![0u8; MAX_DICT_COMPRESSED_CHUNK_BYTES],
            Some(CompressedDictUpload {
                frame_digest: 0xABCD,
                raw_len: 52_931_159,
            }),
        );
        let bytes = encode_args(args).unwrap();
        assert!(
            bytes.len() <= 2 * 1024 * 1024,
            "max compressed chunk call encoded to {} bytes, exceeding 2 MiB",
            bytes.len()
        );
    }
}

ic_cdk::export_candid!();
