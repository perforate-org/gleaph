//! Stable state and operations for the Text Index canister (ADR 0077 engine, plan 0294;
//! hot-path structures swapped per plan 0295 `structures-swap`).
//!
//! ## Region map
//!
//! One `MemoryManager`; concrete numbering ratified at wiring time (plan 0294 slice 9) per
//! design/index/text-index.md ("Region map") and recorded here next to the manager:
//!
//! | MemoryId | Structure | Content |
//! |---|---|---|
//! | 0 | `Cell<TextMeta>` | magic/layout version, analyzer id, monotonic counters |
//! | 1 | `BTreeMap<u64, SegmentRow>` | segment registry (v0: one active segment row) |
//! | 2 | linear-hash-map `u128 → u32` | active-segment dictionary probes: dual xxh3_128 digests of the term → term_id (ADR 0067 V1 contract) |
//! | 3 | dense vector of [`arena::BlobRef`] | active-segment postings by term_id (freq-varint blob in the shared arena) |
//! | 4 | dense vector of [`arena::BlobRef`] | active-segment block-max tables by term_id (LE u32s in the shared arena) |
//! | 5 | dense vector of [`DocKeySlot`] | docid → doc key (hit projection; docids are sequential) |
//! | 6 | linear-hash-map `u64 → u32` | doc key → docid (delete/update addressing) |
//! | 7 | dense vector of `Tombstone` | tombstone bitset containers by ordinal (64 Ki docs each) |
//! | 8 | `Cell<TextStats>` | global stats record |
//! | 9 | stable `VecDeque` of [`arena::BlobRef`] | durable pending ops FIFO (op payloads live in the shared arena) |
//! | 10 | `Cell<Option<u32>>` | resumable merge cursor (last processed term_id) |
//! | 11 | `Cell<Principal>` | controller principal for admin guards |
//! | 12 | dense vector of [`arena::BlobChunk`] | shared variable-byte arena addressed by blob refs |
//! | 13 | dense vector of `TermEntrySlot` | term_id → canonical term string (arena ref) + live df |
//! | 14 | `Cell<Option<backfill::BackfillRegistration>>` | text backfill build identity + lifecycle phase (`crate::backfill`) |
//! | 15 | `Cell<Option<backfill::BackfillCursor>>` | text backfill resumable pull cursor: next page sequence, opaque Graph cursor, done flag, ingested count |
//! | 16 | raw contiguous bytes | MPD container (MeCab-format ipadic 4-image set, plain appends during upload, see "Dictionary region") |
//! | 18 | `Cell<Principal>` | provision relay caller (plan 0335 §5-2: the Provision canister principal allowed on the two relay endpoints) |
//!
//! Region 17 (plan 0335 compressed MPD staging) is DELETED by plan 0342: the catalog
//! artifact is N independent zstd frames, the relay sends ONE FRAME PER CALL, and the text
//! canister decodes THAT call's frame immediately into region 16 — compressed bytes are
//! never persisted. Layout version stays 1 (fresh-state policy; old staged state disposable).
//!
//! ## Dictionary region (plan 0331, container swap plan 0334, plan 0332 widening to id 0)
//!
//! The dictionary-carrying analyzers (ids 0 and 2 — the `dict_required` set) keep their
//! ipadic dictionary OUT of the wasm: region 16 is a PLAIN
//! byte string carrying the MPD container (magic + layout version + entry table {name, offset,
//! len, sha256} + the four MeCab-format images: sys.dic + unk.dic + matrix.bin +
//! char.bin, 52,930,923 bytes for ipadic 2.7.0 utf8), appended in controller-supplied
//! chunks (`MAX_DICT_CHUNK_BYTES` per call, raw appends at the running offset). The
//! identity is a single xxh3_128 over the container bytes, computed from a full region
//! re-read at `admin_finalize_dict_upload` (a ~53 MB transient copy at finalize ONLY)
//! and pinned in [`TextMeta`] with the declared length. States: `Absent` (fresh open),
//! `Uploading` (append-only, interrupted uploads fail the open loudly), `Finalized`.
//! With `dict_required(analyzer_id) == true` the open rebinds WITHOUT decode: structural container
//! validation + resident-set memcpy over batched stable reads (~21 MB: matrix.bin +
//! char.bin + unk.dic + sys.dic trie/word-params); the feature-string region stays lazy
//! over stable memory via the ic-morph-dict `StableImage` (eager rebind, NOT lazy
//! construction: the 5B query-call budget never pays dictionary construction). While
//! `Absent`, analyze-touching operations fail closed with a recorded message until
//! finalize lands. Id 1 (`unicode_bigram`) is dictionary-free and never touches region 16.
//!
//! Per-segment posting/dict stores materialize lazily on flush: the structures above bind
//! their regions at first open but stay empty until the first applied delta.
//!
//! ## Dictionary identity and collisions
//!
//! A term is identified by its verified probe, never by a digest alone: lookups accept a
//! probe hit only when the candidate's canonical string (read from the dense entry array)
//! equals the probe term, so cross-term digest collisions degrade to a miss, never a
//! false accept. Insertion places a new term at its first *absent* probe digest; if both
//! probe digests are occupied by other terms the operation fails closed. With 128-bit
//! xxh3 digests this is cryptographically unreachable for adversarially bounded inputs,
//! but the branch is defined and tested (`dictionary_verification_rejects_forced_digest_collisions`).
//!
//! ## v0 simplifications (documented per plan 0294)
//!
//! - **One active segment + tombstones.** Segments 1..n, timer-driven flushes, and level
//!   merges are later slices; v0 seals nothing. The registry exists as the structural home
//!   of segment rows; counts live only in [`TextStats`] (single source of truth).
//! - **Flush applies the pending log synchronously.** `ingest_text`/`delete_docs` append
//!   durable pending ops; `admin_flush` applies a bounded FIFO prefix into the active
//!   segment. Search therefore sees exactly the flushed prefix (the "under-posted until
//!   flush completes" lag class). A bounded `flush_step` is resumable by construction.
//! - **Tombstone reclaim clears bits only at merge-pass completion.** Stale bits over
//!   already-reclaimed postings are inert until the pass ends; this keeps mid-pass reads
//!   sound without per-doc reference counting.
//! - **Arena runs orphaned on size-class changes are inert.** Same-chunk-count blob
//!   rewrites happen in place ([`arena::BlobArena::write_over`]); growth/shrink relocates
//!   the run and leaves the old chunks as inert bytes until a compaction slice lands.
//!   Linear-hash-map split debt is serviced with bounded budgets at the end of every
//!   `flush_step` ([ADR 0067](../../design/adr/0067-stable-linear-hash-map-production-contract.md)).
//!
//! ## Scoring policy (v0 placeholder)
//!
//! Scoring formulas belong to the index definition catalog; the physical layer consumes
//! caller-supplied parts. Until catalog wiring lands, search uses the identity part model:
//! contribution = [`WEIGHT_BASE`] + stored term frequency, and block-max tables (stored as
//! max tf) are scaled by the constant weight at query time to satisfy the driver's
//! contribution-bound contract. Deterministic tie-break (score desc, docid asc) comes from
//! the promoted driver.
//!
//! ## Determinism
//!
//! No hash-order iteration anywhere: semantic orders are explicitly chosen — FIFO for the
//! pending log, ascending term_id for merge passes, arrival order (with lexicographic
//! order within one document) for term-id assignment, docid ascending for tie-breaks.
//! Linear-hash-map routing affects physical placement only, never observable order.

mod arena;

use std::borrow::Cow;
use std::cell::RefCell;

use candid::{CandidType, Decode, Encode, Principal};
use ic_stable_linear_hash_map::StableLinearHashMap;
use ic_stable_memory_backend::DefaultMemoryImpl;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::storable::{Bound as SBound, Storable};
use ic_stable_structures::vec::Vec as StableVec;
use ic_stable_structures::{BTreeMap, Cell};
use ic_stable_text_postings::blockmax::LOGICAL_BLOCK_SIZE;
use ic_stable_text_postings::enc::{FreqVarintReader, PostingReader, encode_freq_varint};
use ic_stable_text_postings::topk::{QueryList, TfPartTable, topk_disjunctive};
use ic_stable_vec_deque::VecDeque as StableVecDeque;
use serde::{Deserialize, Serialize};
use std::io::Read as _;
use xxhash_rust::xxh3::{xxh3_128, xxh3_128_with_seed};

use crate::analyzer::{
    ANALYZER_MECAB, ANALYZER_MULTILINGUAL, ANALYZER_UNICODE_BIGRAM, analyze_pinned, dict_required,
};
use crate::{FlushReport, MergeStepReport, TextDoc, TextHit, TextIndexStats};
use gleaph_graph_kernel::provisioning::dictionary::{
    CompressedDictFinalize, CompressedDictUpload, DictStatus, MAX_DICT_COMPRESSED_CHUNK_BYTES,
};

use arena::{BlobArena, BlobRef};

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;
/// Dictionary probes: digest → term_id (region 2).
type DictMap<M> = StableLinearHashMap<u128, u32, M>;
/// Reverse doc addressing: key → docid (region 6).
type KeyDocMap<M> = StableLinearHashMap<u64, u32, M>;

// -- Guard rails: bounded loop budgets per call (fail-closed preflight checks). ----------

/// Upper bound on `k` accepted by `search`; larger values clamp silently to keep query
/// work bounded.
pub(crate) const MAX_SEARCH_K: u32 = 100;
/// Upper bound on UTF-8 query bytes; larger queries are rejected before any work.
pub(crate) const MAX_QUERY_BYTES: usize = 4_096;
/// Upper bound on documents per `ingest_text` call.
pub(crate) const MAX_DOCS_PER_INGEST: usize = 1_000;
/// Upper bound on UTF-8 bytes per ingested document text.
pub(crate) const MAX_TEXT_BYTES_PER_DOC: usize = 65_536;
/// Upper bound on analyzed units per document (post-expansion, incl. CJK bigrams).
pub(crate) const MAX_UNITS_PER_DOC: usize = 4_096;
/// Upper bound on keys per `delete_docs` call.
pub(crate) const MAX_KEYS_PER_DELETE: usize = 1_000;
/// Upper bound on bytes per `admin_upload_dict_chunk` call (ADR 0087 chunk analogy).
pub const MAX_DICT_CHUNK_BYTES: usize = 1024 * 1024;
/// Upper bound on the total dictionary blob length (fail-closed runaway guard; the pinned
/// artifact is 8,045,952 bytes, so 16 MiB leaves comfortable headroom).
pub(crate) const MAX_DICT_TOTAL_BYTES: u64 = 64 * 1024 * 1024;
/// Pending ops applied per `admin_flush` call; repeat until [`FlushReport::done`].
pub(crate) const FLUSH_OPS_BUDGET: u64 = 512;
/// Terms reclaimed per `admin_merge_step` call (budget parameter clamps to this).
pub(crate) const MAX_MERGE_TERMS_PER_STEP: u32 = 1_024;
/// Constant weight every matched query list contributes (identity scorer; see module docs).
pub const WEIGHT_BASE: u32 = 1;

/// Probe digests per dictionary term (primary + alternate domain).
const DICT_PROBES: usize = 2;
/// Alternate xxh3 seed for the second probe domain.
const DICT_PROBE_SEED: u64 = 0x9E37_79B9_7F4A_7C15;
/// Split-debt service budgets applied to both linear hash maps after each flush step
/// (crate-default magnitudes; see ADR 0067 admission/maintenance contract).
const SPLIT_DEBT_ENTRY_BUDGET: u64 = 1024;
const SPLIT_DEBT_BYTE_BUDGET: u64 = 16 * 1024 * 1024;

const MAGIC: u64 = u64::from_le_bytes(*b"GLEAPHTX");
/// The analyzer-2 dictionary (plan 0331): durable region 16 (analyzer-2 dictionary blob) and the
/// dictionary fields of [`TextMeta`]. Layouts 1–4 fail loudly at open; fresh state is
/// required (pre-production rule).
// Layout version stays 1 while pre-production (states are disposable — structural changes
// do not bump; the number resumes meaning as an install-time guard at production).
const LAYOUT_VERSION: u32 = 1;
/// The single active segment of v0 (`SegmentRow` holder; see module docs).
const ACTIVE_SEGMENT_ID: u64 = 0;
const TOMBSTONE_CONTAINER_BITS: usize = 65_536;
const TOMBSTONE_CONTAINER_BYTES: usize = TOMBSTONE_CONTAINER_BITS / 8;

// -- Region map: MemoryId constants recorded next to the manager initialization. ---------

thread_local! {
    static MEMORY_MANAGER: RefCell<MemoryManager<DefaultMemoryImpl>> =
        RefCell::new(MemoryManager::init(DefaultMemoryImpl::default()));
}

const TEXT_META: MemoryId = MemoryId::new(0);
const TEXT_SEGMENT_REGISTRY: MemoryId = MemoryId::new(1);
const TEXT_ACTIVE_TERM_DICT: MemoryId = MemoryId::new(2);
const TEXT_ACTIVE_POSTINGS: MemoryId = MemoryId::new(3);
const TEXT_ACTIVE_BLOCK_MAX: MemoryId = MemoryId::new(4);
const TEXT_DOC_KEY_BY_DOCID: MemoryId = MemoryId::new(5);
const TEXT_DOCID_BY_KEY: MemoryId = MemoryId::new(6);
const TEXT_TOMBSTONES: MemoryId = MemoryId::new(7);
const TEXT_STATS: MemoryId = MemoryId::new(8);
const TEXT_PENDING_OPS: MemoryId = MemoryId::new(9);
const TEXT_MERGE_CURSOR: MemoryId = MemoryId::new(10);
const TEXT_CONTROLLER: MemoryId = MemoryId::new(11);
const TEXT_BLOB_ARENA: MemoryId = MemoryId::new(12);
const TEXT_TERM_ENTRIES: MemoryId = MemoryId::new(13);
/// Analyzer-2 dictionary blob (plan 0331); the backfill cells own 14/15 (see `backfill`).
const TEXT_DICT_BLOB: MemoryId = MemoryId::new(16);
/// Provision relay caller (plan 0335 §5-2): the Provision canister principal allowed on the
/// two relay endpoints (`admin_upload_dict_chunk`, `admin_finalize_dict_upload`) in addition
/// to the stored controller (Router). Anonymous sentinel = deny everyone.
const TEXT_DICT_RELAY_CALLER: MemoryId = MemoryId::new(18);

/// Binds one production region through the single `MemoryManager`; exposed so the
/// sibling [`crate::backfill`] module can bind its own cells on dedicated MemoryIds.
pub(crate) fn region(id: MemoryId) -> Memory {
    MEMORY_MANAGER.with(|manager| manager.borrow().get(id))
}

/// The canister's stable regions, bound from the production `MemoryManager` once per
/// process (first use) or supplied explicitly by tests on fresh memories.
pub(crate) struct TextMemories<M: ic_stable_structures::Memory> {
    meta: M,
    segments: M,
    dict: M,
    postings: M,
    block_max: M,
    key_by_docid: M,
    docid_by_key: M,
    tombstones: M,
    stats: M,
    pending: M,
    merge_cursor: M,
    controller: M,
    arena: M,
    term_entries: M,
    dict_blob: M,
    /// Provision relay caller (plan 0335 §5-2): region 18, the Provision canister principal.
    dict_relay_caller: M,
}

impl TextMemories<Memory> {
    /// Binds all thirteen production regions through the single `MemoryManager`.
    pub(crate) fn production() -> Self {
        Self {
            meta: region(TEXT_META),
            segments: region(TEXT_SEGMENT_REGISTRY),
            dict: region(TEXT_ACTIVE_TERM_DICT),
            postings: region(TEXT_ACTIVE_POSTINGS),
            block_max: region(TEXT_ACTIVE_BLOCK_MAX),
            key_by_docid: region(TEXT_DOC_KEY_BY_DOCID),
            docid_by_key: region(TEXT_DOCID_BY_KEY),
            tombstones: region(TEXT_TOMBSTONES),
            stats: region(TEXT_STATS),
            pending: region(TEXT_PENDING_OPS),
            merge_cursor: region(TEXT_MERGE_CURSOR),
            controller: region(TEXT_CONTROLLER),
            arena: region(TEXT_BLOB_ARENA),
            term_entries: region(TEXT_TERM_ENTRIES),
            dict_blob: region(TEXT_DICT_BLOB),
            dict_relay_caller: region(TEXT_DICT_RELAY_CALLER),
        }
    }
}

// -- Stable records -----------------------------------------------------------------------

/// Layout header + monotonic counters. Counters never reset while bytes persist; layout
/// changes require fresh state (pre-production simplicity: no migrations).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
struct TextMeta {
    magic: u64,
    layout_version: u32,
    analyzer_id: u32,
    next_docid: u32,
    next_term_id: u32,
    /// Analyzer-2 dictionary identity: xxh3_128 over the concatenated region-16 chunk
    /// bytes, pinned at finalize (0 while absent/uploading).
    dict_digest: u128,
    /// Dictionary blob byte length (0 while absent/uploading; pinned at finalize).
    dict_len: u64,
    /// Dictionary lifecycle (see the module "Analyzer-2 dictionary" section):
    /// [`DICT_STATE_ABSENT`] / [`DICT_STATE_UPLOADING`] / [`DICT_STATE_FINALIZED`].
    dict_state: u8,
    /// Framed-mode progress (plan 0342): raw bytes appended to region 16 so far during the
    /// provision relay (0 in raw mode and after a compressed finalize).
    dict_raw_received_len: u64,
    /// Framed-mode progress (plan 0342): the accumulated xxh3_128 over the raw bytes appended
    /// to region 16 so far (0 in raw mode and after a compressed finalize).
    dict_raw_digest: u128,
}

/// Dictionary lifecycle states of [`TextMeta::dict_state`].
pub(crate) const DICT_STATE_ABSENT: u8 = 0;
/// Appending chunks; an interrupted upload fails the open loudly (no resume this slice).
pub(crate) const DICT_STATE_UPLOADING: u8 = 1;
/// Digest verified + pinned; the pinned tokenizer is resident (or rebuilt at open).
pub(crate) const DICT_STATE_FINALIZED: u8 = 2;

impl Default for TextMeta {
    fn default() -> Self {
        Self {
            magic: MAGIC,
            layout_version: LAYOUT_VERSION,
            analyzer_id: ANALYZER_UNICODE_BIGRAM,
            next_docid: 0,
            next_term_id: 0,
            dict_digest: 0,
            dict_len: 0,
            dict_state: DICT_STATE_ABSENT,
            dict_raw_received_len: 0,
            dict_raw_digest: 0,
        }
    }
}

impl Storable for TextMeta {
    const BOUND: SBound = SBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode TextMeta"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode TextMeta")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), TextMeta).expect("decode TextMeta")
    }
}

/// Registry row. v0 holds only the active-segment marker; per-segment counters land with
/// multi-segment slices so document/unit totals keep one owner ([`TextStats`]).
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
struct SegmentRow {
    active: bool,
}

impl Storable for SegmentRow {
    const BOUND: SBound = SBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode SegmentRow"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode SegmentRow")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), SegmentRow).expect("decode SegmentRow")
    }
}

/// Dictionary value (heap-side view): live document frequency plus the arena locator of
/// the canonical term string (df tracks postings after tombstone reclamation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct TermEntry {
    str_ref: BlobRef,
    df: u32,
}

/// Fixed-size stable carrier for [`TermEntry`] (20 bytes: 16-byte blob ref + LE u32 df).
#[derive(Clone, Copy)]
struct TermEntrySlot([u8; 20]);

impl From<TermEntry> for TermEntrySlot {
    fn from(e: TermEntry) -> Self {
        let mut out = [0u8; 20];
        out[0..16].copy_from_slice(&arena::BlobRefSlot::from(e.str_ref).0);
        out[16..20].copy_from_slice(&e.df.to_le_bytes());
        Self(out)
    }
}

impl From<TermEntrySlot> for TermEntry {
    fn from(slot: TermEntrySlot) -> Self {
        Self {
            str_ref: arena::BlobRefSlot(slot.0[0..16].try_into().expect("fixed width")).into(),
            df: u32::from_le_bytes(slot.0[16..20].try_into().expect("fixed width")),
        }
    }
}

impl Storable for TermEntrySlot {
    const BOUND: SBound = SBound::Bounded {
        max_size: 20,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.as_ref().try_into().expect("corrupt term entry width"))
    }
}

impl TermEntry {
    /// Absent marker: analyzed units are never empty, so an empty string ref cannot be
    /// live.
    fn is_absent(self) -> bool {
        self.str_ref.is_empty()
    }
}

/// Dense doc-key slot (region 5): present flag + key, packed into 9 fixed bytes. Docids
/// are sequential, so the vector index *is* the docid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DocKeySlot {
    present: bool,
    key: u64,
}

impl DocKeySlot {
    fn live(key: u64) -> Self {
        Self { present: true, key }
    }
}

impl Storable for DocKeySlot {
    const BOUND: SBound = SBound::Bounded {
        max_size: 9,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = [0u8; 9];
        out[0] = u8::from(self.present);
        out[1..9].copy_from_slice(&self.key.to_le_bytes());
        Cow::Owned(out.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b: [u8; 9] = bytes
            .as_ref()
            .try_into()
            .expect("corrupt doc key slot width");
        Self {
            present: b[0] != 0,
            key: u64::from_le_bytes(b[1..9].try_into().expect("fixed width")),
        }
    }
}

/// Durable pending op. Units are carried verbatim so `admin_flush` applies exactly what
/// was ingested (the analyzer runs once, at enqueue time). Encoded candid bytes live in
/// the shared blob arena; the FIFO deque holds only the locator.
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
enum PendingOp {
    Upsert { key: u64, units: Vec<String> },
    Delete { key: u64 },
}

/// One tombstone bitset container covering 64 Ki consecutive docids (container key =
/// `docid >> 16`, bit index = `docid & 0xFFFF`). Stored densely by container ordinal.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Tombstone([u8; TOMBSTONE_CONTAINER_BYTES]);

impl Default for Tombstone {
    fn default() -> Self {
        Self([0; TOMBSTONE_CONTAINER_BYTES])
    }
}

impl Tombstone {
    fn get(&self, docid: u32) -> bool {
        let bit = (docid & 0xFFFF) as usize;
        self.0[bit / 8] & (1 << (bit % 8)) != 0
    }

    fn set(&mut self, docid: u32) {
        let bit = (docid & 0xFFFF) as usize;
        self.0[bit / 8] |= 1 << (bit % 8);
    }
}

impl Storable for Tombstone {
    const BOUND: SBound = SBound::Bounded {
        max_size: TOMBSTONE_CONTAINER_BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0.to_vec()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut out = Tombstone::default();
        assert_eq!(
            bytes.len(),
            TOMBSTONE_CONTAINER_BYTES,
            "corrupt tombstone container length"
        );
        out.0.copy_from_slice(bytes.as_ref());
        out
    }
}

/// Global stats record — the single source of truth for document/unit/tombstone counts
/// (the registry intentionally carries none).
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, Default, PartialEq, Eq)]
struct TextStats {
    ndocs: u64,
    total_units: u64,
    tombstoned_docs: u64,
}

impl Storable for TextStats {
    const BOUND: SBound = SBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode TextStats"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode TextStats")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), TextStats).expect("decode TextStats")
    }
}

// -- Store ---------------------------------------------------------------------------------

/// All stable structures of the text index, generic over the memory backend so unit
/// tests run on fresh in-memory backends while production binds `DefaultMemoryImpl`.
pub struct TextStores<M: ic_stable_structures::Memory> {
    meta: Cell<TextMeta, M>,
    segments: BTreeMap<u64, SegmentRow, M>,
    dict: DictMap<M>,
    term_entries: StableVec<TermEntrySlot, M>,
    postings: StableVec<arena::BlobRefSlot, M>,
    block_max: StableVec<arena::BlobRefSlot, M>,
    key_by_docid: StableVec<DocKeySlot, M>,
    docid_by_key: KeyDocMap<M>,
    tombstones: StableVec<Tombstone, M>,
    stats: Cell<TextStats, M>,
    pending: StableVecDeque<arena::BlobRefSlot, M>,
    merge_cursor: Cell<Option<u32>, M>,
    controller: Cell<Principal, M>,
    arena: BlobArena<M>,
    /// Raw region 16 memory (the MeCab-format dictionary container; plan 0334 keeps it
    /// a PLAIN byte string addressed by offset accessors — no framing).
    dict_region: M,
    /// Provision relay caller (plan 0335 §5-2): region 18, the Provision canister principal.
    dict_relay_caller: Cell<Principal, M>,
}

/// Computes the probe digests of one term (dual-domain xxh3_128 over UTF-8 bytes).
fn dict_digests(term: &str) -> [u128; DICT_PROBES] {
    let bytes = term.as_bytes();
    [xxh3_128(bytes), xxh3_128_with_seed(bytes, DICT_PROBE_SEED)]
}

impl<M> TextStores<M>
where
    M: ic_stable_structures::Memory + Clone + 'static,
{
    /// Opens every region load-or-create and validates the layout header FIRST: foreign
    /// or incompatible meta bytes fail closed (assert) before any structure binds its
    /// region, matching pre-production simplicity — layout changes require fresh state,
    /// not migrations.
    ///
    /// `init_analyzer` (plan 0331) is the install-arg analyzer id: `None` = default
    /// unicode-bigram, `Some(id)` validated fail-closed against the registered set and
    /// recorded into fresh meta (a reopen with a mismatching arg fails loudly). With
    /// analyzer 2 the open enforces the dictionary lifecycle: an interrupted upload fails
    /// the open; a finalized dictionary eagerly decompresses + builds the pinned tokenizer.
    /// [`init_with_analyzer`](Self::init_with_analyzer) with the default unicode-bigram
    /// pipeline (the pre-0331 open shape; production binds the install arg instead).
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn init(memories: TextMemories<M>) -> Self {
        Self::init_with_analyzer(memories, None)
    }

    /// [`init`](Self::init) with the install-arg analyzer id (validated ∈ {0, 1, 2};
    /// plan 0332 widens the registered set to include the multilingual composite
    /// ANALYZER_ID=0 — the DEFAULT for any absent `ANALYZER` clause. The init default
    /// stays `ANALYZER_UNICODE_BIGRAM` for the bare-wasm-install path (canbench +
    /// diagnostics) — the production default is set by the Router's `register_provisioned_graph`
    /// install-arg, NOT by an open-time fallback here.
    pub fn init_with_analyzer(memories: TextMemories<M>, init_analyzer: Option<u32>) -> Self {
        let meta = Cell::init(
            memories.meta,
            TextMeta {
                analyzer_id: init_analyzer.unwrap_or(ANALYZER_UNICODE_BIGRAM),
                ..TextMeta::default()
            },
        );
        let header = meta.get();
        assert!(
            header.magic == MAGIC && header.layout_version == LAYOUT_VERSION,
            "incompatible text index layout: magic {:#x} version {}",
            header.magic,
            header.layout_version
        );
        assert!(
            matches!(
                header.analyzer_id,
                ANALYZER_MULTILINGUAL | ANALYZER_UNICODE_BIGRAM | ANALYZER_MECAB
            ),
            "text index meta carries unregistered analyzer id {}",
            header.analyzer_id
        );
        if let Some(requested) = init_analyzer {
            assert!(
                requested == header.analyzer_id,
                "init analyzer {} does not match the persisted meta analyzer {}",
                requested,
                header.analyzer_id
            );
        }
        if dict_required(header.analyzer_id) && header.dict_state == DICT_STATE_UPLOADING {
            panic!(
                "analyzer-2 dictionary upload was interrupted (state Uploading); \
                 the canister cannot open — re-install with fresh state"
            );
        }

        let mut stores = Self {
            meta,
            segments: BTreeMap::init(memories.segments),
            dict: StableLinearHashMap::init(memories.dict).expect("bind dictionary map"),
            term_entries: StableVec::init(memories.term_entries),
            postings: StableVec::init(memories.postings),
            block_max: StableVec::init(memories.block_max),
            key_by_docid: StableVec::init(memories.key_by_docid),
            docid_by_key: StableLinearHashMap::init(memories.docid_by_key)
                .expect("bind doc key map"),
            tombstones: StableVec::init(memories.tombstones),
            stats: Cell::init(memories.stats, TextStats::default()),
            pending: StableVecDeque::init(memories.pending).expect("bind pending log"),
            merge_cursor: Cell::init(memories.merge_cursor, None),
            controller: Cell::init(memories.controller, Principal::anonymous()),
            arena: BlobArena::init(memories.arena),
            dict_region: memories.dict_blob,
            dict_relay_caller: Cell::init(memories.dict_relay_caller, Principal::anonymous()),
        };
        if stores.segments.is_empty() {
            stores
                .segments
                .insert(ACTIVE_SEGMENT_ID, SegmentRow { active: true });
        }
        // Eager dictionary rebind (plan 0334, plan 0332 widening to id 0): a
        // finalized region is validated structurally and the resident set
        // materialized NOW — the query budget never pays dictionary construction.
        // Corrupt bytes fail the open loudly — the canister cannot operate on a
        // broken dictionary. No decode, no full-container copy: the feature region
        // stays lazy over stable memory. The mecab analyzer is the SHARED resident
        // token surface for both id 0 and id 2 (id 0 dispatches `{kanji∪kana}` runs
        // through it via the composite; id 2 dispatches the whole text through it).
        if dict_required(stores.meta.get().analyzer_id)
            && stores.meta.get().dict_state == DICT_STATE_FINALIZED
        {
            crate::analyzer_mecab::load_dictionary_from_image(stores.dict_container_image())
                .unwrap_or_else(|error| panic!("dictionary open failed: {error}"));
        }
        stores
    }

    // -- Dictionary: verified digest probes over the linear hash map ---------------------

    /// Resolves a term to its verified entry: every probe hit whose stored canonical
    /// string differs from the probe term is treated as a digest collision (absent).
    fn dict_lookup_digests(
        &self,
        term: &str,
        digests: &[u128; DICT_PROBES],
    ) -> Option<(u32, TermEntry)> {
        for &digest in digests {
            if let Some(term_id) = self.dict_get(digest) {
                let entry = self.term_entry(term_id);
                if self.arena.read(entry.str_ref) == term.as_bytes() {
                    return Some((term_id, entry));
                }
            }
        }
        None
    }

    /// Returns the verified term id for `unit`, interning it (fresh dense id + canonical
    /// string in the arena + probe placement) when absent. Fails closed when every probe
    /// digest is occupied by another term (see module docs).
    fn dict_intern_digests(&mut self, unit: &str, digests: &[u128; DICT_PROBES]) -> u32 {
        if let Some((term_id, _)) = self.dict_lookup_digests(unit, digests) {
            return term_id;
        }
        let mut meta = self.meta.get().clone();
        let term_id = meta.next_term_id;
        meta.next_term_id = term_id.checked_add(1).expect("term id space exhausted");
        self.meta.set(meta);

        let str_ref = self.arena.put(unit.as_bytes());
        self.term_entries
            .push(&TermEntrySlot::from(TermEntry { str_ref, df: 0 }));

        for &digest in digests {
            if self.dict_get(digest).is_none() {
                let previous = self
                    .dict
                    .insert(digest, term_id)
                    .expect("dictionary map writable");
                assert!(previous.is_none(), "probe digest was just verified absent");
                return term_id;
            }
        }
        panic!("dictionary probe space exhausted for {unit:?}: both digests collide");
    }

    /// Removes a term's entry and probe keys once its df reaches zero.
    fn dict_remove_digests(&mut self, unit: &str, digests: &[u128; DICT_PROBES]) {
        let Some((term_id, _)) = self.dict_lookup_digests(unit, digests) else {
            return;
        };
        for &digest in digests {
            if let Some(occupant) = self.dict_get(digest)
                && occupant == term_id
            {
                self.dict
                    .remove(&digest)
                    .expect("dictionary map writable")
                    .expect("verified occupant was just read");
            }
        }
        self.clear_term_entry(term_id);
    }

    fn dict_get(&self, digest: u128) -> Option<u32> {
        self.dict.get(&digest).expect("dictionary map readable")
    }

    fn term_entry(&self, term_id: u32) -> TermEntry {
        self.term_entries
            .get(u64::from(term_id))
            .map(TermEntry::from)
            .filter(|entry| !entry.is_absent())
            .unwrap_or_else(|| panic!("term entry {term_id} absent"))
    }

    fn set_term_entry(&mut self, term_id: u32, entry: TermEntry) {
        self.term_entries
            .set(u64::from(term_id), &TermEntrySlot::from(entry));
    }

    fn clear_term_entry(&mut self, term_id: u32) {
        self.set_term_entry(
            term_id,
            TermEntry {
                str_ref: BlobRef::EMPTY,
                df: 0,
            },
        );
    }

    // -- Blob-backed regions --------------------------------------------------------------

    fn blob_at(refs: &StableVec<arena::BlobRefSlot, M>, index: u32) -> BlobRef {
        refs.get(u64::from(index))
            .map(|slot| slot.into())
            .unwrap_or_default()
    }

    fn set_blob(refs: &mut StableVec<arena::BlobRefSlot, M>, index: u32, r: BlobRef) {
        debug_assert!(
            !r.is_empty(),
            "detach refs with BlobRef::EMPTY via set_blob"
        );
        if u64::from(index) == refs.len() {
            refs.push(&arena::BlobRefSlot::from(r));
        } else {
            refs.set(u64::from(index), &arena::BlobRefSlot::from(r));
        }
    }

    fn detach_blob(refs: &mut StableVec<arena::BlobRefSlot, M>, index: u32) {
        if u64::from(index) < refs.len() {
            refs.set(u64::from(index), &arena::BlobRefSlot::from(BlobRef::EMPTY));
        }
    }

    /// Verified dictionary probe (canonical-string-checked). Absent/colliding terms miss.
    pub(crate) fn dict_term_id(&self, unit: &str) -> Option<u32> {
        self.dict_lookup_digests(unit, &dict_digests(unit))
            .map(|(term_id, _)| term_id)
    }

    /// Postings blob for one term id (None when the term has no stored list).
    pub(crate) fn postings_blob(&self, term_id: u32) -> Option<Vec<u8>> {
        let r = Self::blob_at(&self.postings, term_id);
        (!r.is_empty()).then(|| self.arena.read(r))
    }

    /// Dense-array index for one docid. Docids are allocated 1-based
    /// (`meta.next_docid` counts docs ever ingested), so slot `d - 1` is the docid's
    /// dense position.
    fn doc_key_index(docid: u32) -> u64 {
        debug_assert!(docid >= 1, "docid 0 is never allocated");
        u64::from(docid) - 1
    }

    /// Live doc key for one docid (None for deleted/never-assigned docids).
    pub(crate) fn key_of_docid(&self, docid: u32) -> Option<u64> {
        if docid == 0 {
            return None;
        }
        self.key_by_docid
            .get(Self::doc_key_index(docid))
            .filter(|slot| slot.present)
            .map(|slot| slot.key)
    }

    /// Docid currently addressed by `key` (None when unknown/deleted). Test
    /// introspection only; production addressing goes through [`Self::key_of_docid`].
    #[cfg(test)]
    pub(crate) fn docid_of_key(&self, key: u64) -> Option<u32> {
        self.docid_by_key.get(&key).expect("doc key map readable")
    }

    fn set_key_of_docid(&mut self, docid: u32, key: u64) {
        let slot = DocKeySlot::live(key);
        let index = Self::doc_key_index(docid);
        if index == self.key_by_docid.len() {
            self.key_by_docid.push(&slot);
        } else {
            self.key_by_docid.set(index, &slot);
        }
    }

    fn clear_key_of_docid(&mut self, docid: u32) {
        if docid >= 1 {
            let index = Self::doc_key_index(docid);
            if index < self.key_by_docid.len() {
                self.key_by_docid.set(index, &DocKeySlot::default());
            }
        }
    }

    // -- Analyzer-2 dictionary upload / finalize (plan 0331) -----------------------------

    /// The durable container as a byte IMAGE over region 16 (zero-copy: offset
    /// accessors address stable memory directly; batched `stable64_read` syscalls).
    /// The `CanisterStableImage` wrapper is Send/Sync by fiat — sound because IC
    /// canister code executes on a single thread (the image never crosses threads).
    fn dict_container_image(&self) -> ic_morph_dict::CanisterStableImage<M> {
        ic_morph_dict::CanisterStableImage::new(self.dict_region.clone(), self.meta.get().dict_len)
    }

    /// Streaming xxh3_128 over a stable region in bounded windows (no full materialization).
    /// The single streaming-hash helper (plan 0342): used by raw finalize (replacing the old
    /// full-container `dict_blob_bytes` heap copy) and by framed upload to accumulate the raw
    /// digest over the received region-16 prefix.
    fn stream_hash_region(&self, region: &M, len: u64) -> u128 {
        let mut hasher = xxhash_rust::xxh3::Xxh3::new();
        let mut buf = vec![0u8; MAX_DICT_CHUNK_BYTES];
        let mut pos = 0u64;
        while pos < len {
            let take = ((len - pos) as usize).min(MAX_DICT_CHUNK_BYTES);
            region.read(pos, &mut buf[..take]);
            hasher.update(&buf[..take]);
            pos += take as u64;
        }
        hasher.digest128()
    }

    /// Decodes ONE zstd frame from `bytes` (heap) and appends the raw output to `dst` at
    /// `dst_offset`, growing pages to cover. A FRESH ruzstd `StreamingDecoder` per call over
    /// the frame bytes — self-contained, no cross-call decoder state (the plan-0342 rejected
    /// alternative is binding). Bounded heap: one read window + ruzstd's internal decode
    /// window (no full materialization — the 0331 675 MB peak must NOT return). `max_total`
    /// bounds the cumulative raw length fail-closed (bomb overrun guard; the declared raw_len
    /// is only known at finalize, so the per-call guard is the runaway bound). Returns the
    /// number of raw bytes appended.
    fn decode_frame_append(
        &self,
        bytes: &[u8],
        dst: &M,
        dst_offset: u64,
        max_total: u64,
    ) -> Result<u64, String> {
        let mut decoder = ruzstd::StreamingDecoder::new(bytes)
            .map_err(|e| format!("compressed dictionary frame is corrupt or truncated: {e:?}"))?;
        let mut out = vec![0u8; MAX_DICT_CHUNK_BYTES];
        let mut raw_len: u64 = 0;
        loop {
            let n = decoder
                .read(&mut out)
                .map_err(|e| format!("compressed dictionary frame decode failed: {e:?}"))?;
            if n == 0 {
                break;
            }
            let end = dst_offset + raw_len + n as u64;
            if end > max_total {
                return Err(format!(
                    "decompressed dictionary exceeds the maximum raw length {max_total} (possible decompression bomb)"
                ));
            }
            let pages = end.div_ceil(65536);
            if dst.size() < pages {
                dst.grow(pages - dst.size());
            }
            dst.write(dst_offset + raw_len, &out[..n]);
            raw_len += n as u64;
        }
        Ok(raw_len)
    }

    /// Relay-guarded append of one dictionary chunk (plan 0335 wire: `(bytes, opt mode)`;
    /// `None` selects the RAW mode byte-identically, `Some` the framed mode). Shared
    /// fail-closed gates (analyzer in the `DICT_REQUIRED` set, not already finalized,
    /// non-empty chunk, runaway total) run before any mutation; the mode then selects the
    /// per-call cap: raw appends region 16 under the 1 MiB [`MAX_DICT_CHUNK_BYTES`] cap,
    /// framed decodes ONE zstd frame under the shared [`MAX_DICT_COMPRESSED_CHUNK_BYTES`]
    /// cap (~1.9 MiB, the cross-subnet 2 MiB payload limit minus Candid/envelope headroom)
    /// and appends the DECODED bytes to region 16 immediately (plan 0342 — compressed bytes
    /// are never persisted). Mixing modes within one upload session rejects. Returns the new
    /// TOTAL length of the selected staging region (raw bytes for framed mode).
    pub fn upload_dict_chunk(
        &mut self,
        bytes: Vec<u8>,
        mode: Option<CompressedDictUpload>,
    ) -> Result<u64, String> {
        let meta = self.meta.get();
        if !dict_required(meta.analyzer_id) {
            return Err(format!(
                "dictionary upload requires a dictionary-carrying analyzer (ids 0 or 2), this index pins analyzer {}",
                meta.analyzer_id
            ));
        }
        if meta.dict_state == DICT_STATE_FINALIZED {
            return Err("dictionary already finalized; re-upload requires fresh state".to_string());
        }
        if bytes.is_empty() {
            return Err("empty dictionary chunk".to_string());
        }
        let upload = match mode {
            None => {
                if meta.dict_raw_received_len > 0 {
                    return Err(
                        "dictionary upload started in framed mode; raw chunks are rejected"
                            .to_string(),
                    );
                }
                if bytes.len() > MAX_DICT_CHUNK_BYTES {
                    return Err(format!(
                        "dictionary chunk of {} bytes exceeds MAX_DICT_CHUNK_BYTES ({MAX_DICT_CHUNK_BYTES})",
                        bytes.len()
                    ));
                }
                let new_len = meta.dict_len + bytes.len() as u64;
                if new_len > MAX_DICT_TOTAL_BYTES {
                    return Err(format!(
                        "dictionary blob would exceed MAX_DICT_TOTAL_BYTES ({MAX_DICT_TOTAL_BYTES})"
                    ));
                }
                // Raw contiguous append at the running offset (plan 0334): region 16 is a
                // PLAIN byte string so the open path addresses it with offset accessors via
                // the ic-morph-dict `StableImage` — no framing, no decode. The raw region has
                // no auto-grow: grow to cover the new offset first.
                let pages = new_len.div_ceil(65536);
                if self.dict_region.size() < pages {
                    self.dict_region.grow(pages - self.dict_region.size());
                }
                self.dict_region.write(meta.dict_len, &bytes);
                let mut meta = meta.clone();
                meta.dict_state = DICT_STATE_UPLOADING;
                meta.dict_len = new_len;
                self.meta.set(meta);
                return Ok(new_len);
            }
            Some(upload) => upload,
        };
        // -- Framed mode: verify THIS frame's digest, decode it, append raw bytes to 16. --
        if meta.dict_len > 0 {
            return Err(
                "dictionary upload started in raw mode; framed chunks are rejected".to_string(),
            );
        }
        if bytes.len() > MAX_DICT_COMPRESSED_CHUNK_BYTES {
            return Err(format!(
                "compressed dictionary chunk of {} bytes exceeds MAX_DICT_COMPRESSED_CHUNK_BYTES ({MAX_DICT_COMPRESSED_CHUNK_BYTES})",
                bytes.len()
            ));
        }
        // Verify the frame digest FIRST (fail THAT call, no mutation — region 16 and meta
        // are untouched on a mismatch).
        let received_digest = xxh3_128(&bytes);
        if received_digest != upload.frame_digest {
            return Err(format!(
                "frame digest mismatch: received bytes hash {:#032x}, expected {:#032x}",
                received_digest, upload.frame_digest
            ));
        }
        // Decode THIS frame (a fresh ruzstd StreamingDecoder per call — self-contained, no
        // cross-call decoder state) and append the raw bytes to region 16 at the running raw
        // offset. The bomb-overrun gate bounds the cumulative raw length against the declared
        // raw_len (reject the call that exceeds it) AND the runaway guard (a maliciously large
        // raw_len must not let region 16 grow unboundedly).
        let raw_offset = meta.dict_raw_received_len;
        let max_total = upload.raw_len.min(MAX_DICT_TOTAL_BYTES);
        let decoded = self.decode_frame_append(&bytes, &self.dict_region, raw_offset, max_total)?;
        let new_raw_len = raw_offset + decoded;
        // Accumulate the raw streaming digest over the received region-16 prefix (windowed,
        // no full materialization) and record the new raw offset.
        let raw_digest = self.stream_hash_region(&self.dict_region, new_raw_len);
        let mut meta = meta.clone();
        meta.dict_state = DICT_STATE_UPLOADING;
        meta.dict_raw_received_len = new_raw_len;
        meta.dict_raw_digest = raw_digest;
        self.meta.set(meta);
        Ok(new_raw_len)
    }

    /// Relay-guarded finalize (plan 0335 wire: `(digest, opt mode)`; `None` selects the
    /// RAW mode byte-identically, `Some` the compressed mode). The leading digest argument
    /// keeps one uniform meaning in both modes: the expected RAW container identity.
    ///
    /// RAW mode: re-reads the full region, hashes the concatenated bytes (xxh3_128) and
    /// compares against `expected_digest`; idempotent no-op on an exact Finalized match;
    /// a mismatch rejects WITHOUT touching state.
    /// Relay-guarded finalize (plan 0335 wire: `(digest, opt mode)`; `None` selects the
    /// RAW mode byte-identically, `Some` the framed mode). The leading digest argument
    /// keeps one uniform meaning in both modes: the expected RAW container identity.
    ///
    /// RAW mode: streams one xxh3_128 over region 16 (bounded windows — no full-container
    /// heap copy) and compares against `expected_digest`; idempotent no-op on an exact
    /// Finalized match; a mismatch rejects WITHOUT touching state.
    ///
    /// FRAMED mode (plan 0342): the frames were decoded into region 16 on arrival, so this
    /// call verifies the ACCUMULATED raw digest (streamed over region 16 during upload)
    /// against the argument, verifies the total raw length against the declared raw_len
    /// (the final truncation gate), then runs the existing structural MPD validation +
    /// resident-set materialization. Every failure path leaves TextMeta non-Finalized and
    /// RESETS the framed progress (region 16 + frame counter) so a retry re-streams from
    /// frame 0. A Finalized exact replay (same raw digest) is an idempotent no-op in both
    /// modes. NO deactivate step — region 17 no longer exists.
    pub fn finalize_dict_upload(
        &mut self,
        expected_digest: u128,
        mode: Option<CompressedDictFinalize>,
    ) -> Result<DictStatus, String> {
        let meta = self.meta.get();
        if !dict_required(meta.analyzer_id) {
            return Err(format!(
                "dictionary finalize requires a dictionary-carrying analyzer (ids 0 or 2), this index pins analyzer {}",
                meta.analyzer_id
            ));
        }
        if meta.dict_state == DICT_STATE_FINALIZED {
            if meta.dict_digest == expected_digest {
                return Ok(self.dict_status());
            }
            return Err(format!(
                "dictionary already finalized with digest {:#032x}; expected {:#032x}",
                meta.dict_digest, expected_digest
            ));
        }
        if meta.dict_state == DICT_STATE_ABSENT {
            return Err("no dictionary chunks uploaded".to_string());
        }
        let expected = match mode {
            None => {
                if meta.dict_raw_received_len > 0 {
                    return Err(
                        "dictionary was staged in framed mode; finalize requires the framed record"
                            .to_string(),
                    );
                }
                // Digest: one streaming xxh3_128 over region 16 (bounded windows — no
                // full-container heap copy; the open/rebind path never copies the container).
                let digest = self.stream_hash_region(&self.dict_region, meta.dict_len);
                if digest != expected_digest {
                    return Err(format!(
                        "dictionary digest mismatch: region hashes {:#032x}, expected {:#032x}",
                        digest, expected_digest
                    ));
                }
                // Structural validation + resident-set materialization FIRST; a corrupt
                // artifact must not persist Finalized state.
                crate::analyzer_mecab::load_dictionary_from_image(self.dict_container_image())?;
                let mut meta = meta.clone();
                meta.dict_state = DICT_STATE_FINALIZED;
                meta.dict_digest = digest;
                self.meta.set(meta);
                return Ok(self.dict_status());
            }
            Some(expected) => expected,
        };
        // -- Framed mode: verify accumulated digest + total length → validate → pin. ------
        // Read the accumulated progress into locals so the reset calls below don't fight
        // the immutable `meta` borrow.
        let raw_received_len = meta.dict_raw_received_len;
        let raw_digest = meta.dict_raw_digest;
        if raw_received_len == 0 {
            return Err("no framed dictionary chunks uploaded".to_string());
        }
        if expected.raw_digest != expected_digest {
            // The shared record carries both digests; a mismatch with the leading digest
            // argument (uniform raw-digest vocabulary in both modes) is a malformed relay
            // call, rejected before any mutation.
            return Err(format!(
                "framed finalize record contradicts the digest argument: record raw digest {:#032x}, argument {:#032x}",
                expected.raw_digest, expected_digest
            ));
        }
        if expected.raw_len > MAX_DICT_TOTAL_BYTES {
            return Err(format!(
                "declared raw dictionary length {} exceeds MAX_DICT_TOTAL_BYTES ({MAX_DICT_TOTAL_BYTES})",
                expected.raw_len
            ));
        }
        // Verify the total raw length == declared raw_len (the raw-length gate: a truncated
        // frame sequence — finalize with frames missing — rejects here as truncated; a
        // bomb overrun — total decoded size across frames exceeding raw_len — rejects as
        // exceeding the declared length). Checked BEFORE the digest so a truncated sequence
        // reports the length gate (the plan's truncation contract).
        if raw_received_len != expected.raw_len {
            self.reset_framed_progress();
            if raw_received_len < expected.raw_len {
                return Err(format!(
                    "framed dictionary is truncated: received {} raw bytes, expected {}",
                    raw_received_len, expected.raw_len
                ));
            }
            return Err(format!(
                "framed dictionary exceeds the declared raw length {} (possible decompression bomb)",
                expected.raw_len
            ));
        }
        // Verify the ACCUMULATED raw digest (streamed over region 16 during upload) against
        // the argument. A mismatch means a corrupt-but-decodable frame slipped through the
        // per-frame digest — the final raw digest is the ultimate authority. Reject and
        // reset so a retry re-streams from frame 0.
        if raw_digest != expected_digest {
            self.reset_framed_progress();
            return Err(format!(
                "accumulated raw digest mismatch: region hashes {:#032x}, expected {:#032x}",
                raw_digest, expected_digest
            ));
        }
        // Structural validation + resident-set materialization over the durable region —
        // the exact open path the raw mode uses, so the end state is byte-identical.
        let image =
            ic_morph_dict::CanisterStableImage::new(self.dict_region.clone(), expected.raw_len);
        if let Err(e) = crate::analyzer_mecab::load_dictionary_from_image(image) {
            self.reset_framed_progress();
            return Err(e);
        }
        let mut meta = meta.clone();
        meta.dict_state = DICT_STATE_FINALIZED;
        meta.dict_digest = expected_digest;
        meta.dict_len = expected.raw_len;
        // NO deactivate step (plan 0342): region 17 no longer exists. The framed progress
        // fields are simply superseded by the pinned Finalized state.
        self.meta.set(meta);
        Ok(self.dict_status())
    }

    /// Resets the framed-mode progress (plan 0342): region 16 + frame counter re-stream
    /// from frame 0 after a failed finalize. The recorded raw offset and accumulated digest
    /// clear so the next upload appends at offset 0 (stale region-16 bytes beyond the new
    /// offset stay unreachable — the recorded length only pins on success).
    fn reset_framed_progress(&mut self) {
        let mut meta = self.meta.get().clone();
        meta.dict_raw_received_len = 0;
        meta.dict_raw_digest = 0;
        self.meta.set(meta);
    }

    /// Read-only dictionary status (state / raw digest+len). During `Uploading`, `len` is
    /// the cumulative raw offset (progress) in both modes — raw mode tracks it in `dict_len`,
    /// framed mode in `dict_raw_received_len`; mode mixing is rejected, so exactly one is
    /// non-zero. On `Finalized`, `len` is the pinned raw container length. There is no
    /// separate compressed progress field (region 17 is gone; the accumulated raw digest is
    /// internal and verified at finalize).
    pub fn dict_status(&self) -> DictStatus {
        let meta = self.meta.get();
        let state = match meta.dict_state {
            DICT_STATE_ABSENT => crate::DictState::Absent,
            DICT_STATE_UPLOADING => crate::DictState::Uploading,
            _ => crate::DictState::Finalized,
        };
        let len = if meta.dict_state == DICT_STATE_UPLOADING {
            meta.dict_raw_received_len.max(meta.dict_len)
        } else {
            meta.dict_len
        };
        DictStatus {
            state,
            digest: (meta.dict_state == DICT_STATE_FINALIZED).then_some(meta.dict_digest),
            len,
        }
    }

    // -- DML: durable pending appends (no searchable state changes here) ------------------

    /// Analyzes and appends one durable upsert op per document. Preflight validates every
    /// document before the first append, so rejection leaves the log untouched.
    pub fn enqueue_ingest(&mut self, docs: Vec<TextDoc>) -> Result<(), String> {
        if docs.len() > MAX_DOCS_PER_INGEST {
            return Err(format!(
                "batch of {} documents exceeds MAX_DOCS_PER_INGEST ({MAX_DOCS_PER_INGEST})",
                docs.len()
            ));
        }
        // Dictionary-carrying gate (plan 0331, plan 0332 widening to id 0):
        // ingestion analyzes every document, so it is fail-closed until the pinned
        // dictionary is finalized. The `DICT_REQUIRED` gate is the single source of
        // truth (ids {0, 2} both carry the dictionary; id 1 is dictionary-free).
        let meta = self.meta.get();
        if dict_required(meta.analyzer_id) && meta.dict_state != DICT_STATE_FINALIZED {
            return Err(
                "dictionary is not finalized; ingestion is rejected until finalize".to_string(),
            );
        }
        // Preflight-then-write: analyze everything up front so any cap violation rejects
        // the whole batch before the first durable append.
        let mut prepared = Vec::with_capacity(docs.len());
        for doc in docs {
            if doc.text.len() > MAX_TEXT_BYTES_PER_DOC {
                return Err(format!(
                    "doc key {} exceeds MAX_TEXT_BYTES_PER_DOC ({MAX_TEXT_BYTES_PER_DOC})",
                    doc.key
                ));
            }
            let units = analyze_pinned(self.meta.get().analyzer_id, &doc.text);
            if units.len() > MAX_UNITS_PER_DOC {
                return Err(format!(
                    "doc key {} expands to {} units, exceeding MAX_UNITS_PER_DOC \
                     ({MAX_UNITS_PER_DOC})",
                    doc.key,
                    units.len()
                ));
            }
            prepared.push(PendingOp::Upsert {
                key: doc.key,
                units,
            });
        }
        for op in prepared {
            self.append_pending(op);
        }
        Ok(())
    }

    /// Appends durable delete ops. Unknown keys apply as deterministic no-ops at flush.
    pub fn enqueue_delete(&mut self, keys: Vec<u64>) -> Result<(), String> {
        if keys.len() > MAX_KEYS_PER_DELETE {
            return Err(format!(
                "batch of {} keys exceeds MAX_KEYS_PER_DELETE ({MAX_KEYS_PER_DELETE})",
                keys.len()
            ));
        }
        for key in keys {
            self.append_pending(PendingOp::Delete { key });
        }
        Ok(())
    }

    /// Appends one op payload to the shared arena and its locator to the FIFO deque.
    fn append_pending(&mut self, op: PendingOp) {
        let bytes = Encode!(&op).expect("encode PendingOp");
        let r = self.arena.put(&bytes);
        self.pending
            .push_back(&arena::BlobRefSlot::from(r))
            .expect("pending log grow");
    }

    // -- Flush: apply a bounded FIFO prefix of the pending log ----------------------------

    /// Applies up to `max_ops` pending ops in FIFO order. Repeat until
    /// [`FlushReport::done`]; application order is fully determined by enqueue order.
    pub fn flush_step(&mut self, max_ops: u64) -> FlushReport {
        let mut drained = 0u64;
        while drained < max_ops {
            let Some(locator) = self.pending.pop_front() else {
                break;
            };
            let bytes = self.arena.read(locator.into());
            match Decode!(bytes.as_slice(), PendingOp).expect("decode PendingOp") {
                PendingOp::Upsert { key, units } => self.apply_upsert(key, &units),
                PendingOp::Delete { key } => self.apply_delete(key),
            }
            drained += 1;
        }
        // Both linear hash maps may carry split debt from this batch's insertions; serve
        // it under bounded budgets (ADR 0067). `Pending` simply defers to the next call.
        self.service_split_debt();
        let remaining_ops = self.pending.len();
        FlushReport {
            drained_ops: drained,
            remaining_ops,
            done: remaining_ops == 0,
        }
    }

    fn service_split_debt(&self) {
        let errors = [
            self.dict
                .maintenance_step(SPLIT_DEBT_ENTRY_BUDGET, SPLIT_DEBT_BYTE_BUDGET)
                .err(),
            self.docid_by_key
                .maintenance_step(SPLIT_DEBT_ENTRY_BUDGET, SPLIT_DEBT_BYTE_BUDGET)
                .err(),
        ];
        if let Some(error) = errors.into_iter().flatten().next() {
            panic!("linear hash map maintenance failed: {error}");
        }
    }

    /// Applies one upsert: update = delete + insert (the prior incarnation's docid is
    /// tombstoned first), then a fresh docid receives the new units.
    fn apply_upsert(&mut self, key: u64, units: &[String]) {
        if let Some(old_docid) = self
            .docid_by_key
            .remove(&key)
            .expect("doc key map writable")
        {
            self.clear_key_of_docid(old_docid);
            self.mark_tombstoned(old_docid);
        }

        let mut meta = self.meta.get().clone();
        let docid = meta
            .next_docid
            .checked_add(1)
            .expect("docid space exhausted");
        meta.next_docid = docid;
        self.meta.set(meta);

        // Occurrence counting over an ordered map keeps term-id assignment deterministic
        // (lexicographic within the document, arrival order across documents).
        let mut tfs: std::collections::BTreeMap<&str, u32> = std::collections::BTreeMap::new();
        for unit in units {
            *tfs.entry(unit.as_str()).or_insert(0) += 1;
        }
        let counted: Vec<(String, u32)> = tfs
            .into_iter()
            .map(|(unit, count)| (unit.to_string(), count))
            .collect();
        for (unit, tf) in &counted {
            let digests = dict_digests(unit);
            let term_id = self.dict_intern_digests(unit, &digests);
            let mut entry = self.term_entry(term_id);
            entry.df += 1;
            self.set_term_entry(term_id, entry);
            self.append_posting(term_id, docid, *tf);
        }

        self.set_key_of_docid(docid, key);
        self.docid_by_key
            .insert(key, docid)
            .expect("doc key map writable");
        let mut stats = *self.stats.get();
        stats.ndocs += 1;
        stats.total_units += units.len() as u64;
        self.stats.set(stats);
    }

    /// Applies one delete: unknown keys are no-ops; known keys tombstone their docid and
    /// drop the key mappings (physical reclaim defers to `merge_step`).
    fn apply_delete(&mut self, key: u64) {
        if let Some(docid) = self
            .docid_by_key
            .remove(&key)
            .expect("doc key map writable")
        {
            self.clear_key_of_docid(docid);
            self.mark_tombstoned(docid);
        }
    }

    fn mark_tombstoned(&mut self, docid: u32) {
        let ordinal = u64::from(docid >> 16);
        let mut container = self.tombstones.get(ordinal).unwrap_or_default();
        container.set(docid);
        if ordinal == self.tombstones.len() {
            self.tombstones.push(&container);
        } else {
            self.tombstones.set(ordinal, &container);
        }
        let mut stats = *self.stats.get();
        stats.ndocs -= 1;
        stats.tombstoned_docs += 1;
        self.stats.set(stats);
    }

    /// Extends one posting list by decode-all/re-encode append and rebuilds its block-max
    /// table. O(list length) per append — acceptable at v0 scale; incremental appends land
    /// with the multi-segment slices.
    fn append_posting(&mut self, term_id: u32, docid: u32, tf: u32) {
        let mut docs: Vec<u32> = Vec::new();
        let mut tfs: Vec<u32> = Vec::new();
        if let Some(blob) = self.postings_blob(term_id) {
            let mut reader = FreqVarintReader::new(&blob);
            while reader.peek().is_some() {
                let list_tf = reader.freq().expect("interleaved tf aligns with postings");
                docs.push(reader.next().expect("just peeked"));
                tfs.push(list_tf);
            }
        }
        debug_assert!(
            docs.last().is_none_or(|&last| last < docid),
            "docids must arrive strictly increasing per term"
        );
        docs.push(docid);
        tfs.push(tf.min(u32::from(u8::MAX)));
        let encoded = encode_freq_varint(&docs, &tfs);
        let old = Self::blob_at(&self.postings, term_id);
        let fresh = self.arena.write_over(old, &encoded);
        Self::set_blob(&mut self.postings, term_id, fresh);
        self.rebuild_block_max(term_id, &docs, &tfs);
    }

    /// Rebuilds the term's block-max table over DOCID-aligned logical blocks
    /// (`docid / LOGICAL_BLOCK_SIZE`): the promoted driver indexes bounds by docid
    /// block for its skip math, so positional windows would misalign on sparse posting
    /// lists. Values stay physical max-tf; query-time weight scaling happens in
    /// [`TextStores::search`].
    fn rebuild_block_max(&mut self, term_id: u32, docs: &[u32], tfs: &[u32]) {
        let mut bounds: Vec<u32> = Vec::new();
        for (docid, tf) in docs.iter().zip(tfs) {
            let block = (docid / LOGICAL_BLOCK_SIZE) as usize;
            if bounds.len() <= block {
                bounds.resize(block + 1, 0);
            }
            bounds[block] = bounds[block].max(*tf);
        }
        let mut bytes = Vec::with_capacity(bounds.len() * 4);
        for bound in bounds {
            bytes.extend_from_slice(&bound.to_le_bytes());
        }
        let old = Self::blob_at(&self.block_max, term_id);
        let fresh = self.arena.write_over(old, &bytes);
        Self::set_blob(&mut self.block_max, term_id, fresh);
    }

    fn load_bounds(&self, term_id: u32) -> Vec<u32> {
        let r = Self::blob_at(&self.block_max, term_id);
        if r.is_empty() {
            return Vec::new();
        }
        self.arena
            .read(r)
            .as_chunks::<4>()
            .0
            .iter()
            .map(|chunk| u32::from_le_bytes(*chunk))
            .collect()
    }

    // -- Search ---------------------------------------------------------------------------

    /// DAAT top-k over live postings minus tombstones, using the promoted driver.
    ///
    /// Query terms analyze through the production analyzer; duplicates collapse (each
    /// matched term contributes once). Unflushed terms simply miss the dictionary, which
    /// is the documented under-posted-until-flush lag, and tombstoned docids never reach
    /// the driver because posting readers filter them.
    ///
    /// Scoring is lazy (plan 0295): the driver reads each candidate's tf straight off
    /// the codec and applies the caller-built tf→part table inline — no eager decode of
    /// full lists before ranking. Identity part model: `part(tf) = tf`, so a hit's score
    /// is `Σ (WEIGHT_BASE + tf)` over its matched terms. Block-max bounds stay stored as
    /// physical max-tf and scale by the constant weight at query time, keeping them
    /// sound upper bounds of `WEIGHT_BASE + table[tf]` per docid block.
    pub fn search(&self, query: &str, k: u32) -> Result<Vec<TextHit>, String> {
        if query.len() > MAX_QUERY_BYTES {
            return Err(format!(
                "query of {} bytes exceeds MAX_QUERY_BYTES ({MAX_QUERY_BYTES})",
                query.len()
            ));
        }
        let k = k.min(MAX_SEARCH_K);
        if k == 0 {
            return Ok(Vec::new());
        }
        // Dictionary-carrying gate (plan 0331, plan 0332 widening to id 0): no
        // query analysis without a finalized dictionary. The `DICT_REQUIRED` gate
        // is the single source of truth (ids {0, 2} both carry the dictionary).
        let meta = self.meta.get();
        if dict_required(meta.analyzer_id) && meta.dict_state != DICT_STATE_FINALIZED {
            return Err("dictionary is not finalized; the index cannot serve queries".to_string());
        }

        // Caller-built scoring data: identity tf→part table (contribution part = tf)
        // plus the tombstone filter. Both are O(1) to construct per query. The filter
        // memoizes the last container internally (postings ascend within a list), so
        // filtering costs one stable read per container change plus one dense-bit
        // classification per posting — and hands [`LiveReader`] the dead-run hints that
        // turn contiguous tombstoned spans into single codec jumps.
        let identity_parts: Box<TfPartTable> = Box::new(std::array::from_fn(|tf| tf as u32));
        let tombs = TombFilter::new(&self.tombstones);
        let mut seen = std::collections::BTreeSet::new();
        let mut buffers: Vec<(Vec<u8>, Vec<u32>)> = Vec::new();
        for term in analyze_pinned(self.meta.get().analyzer_id, query) {
            if !seen.insert(term.clone()) {
                continue;
            }
            let Some(term_id) = self.dict_term_id(&term) else {
                continue;
            };
            let Some(blob) = self.postings_blob(term_id) else {
                continue;
            };
            // Driver contract: per-block bounds cap the TOTAL contribution (weight +
            // part), so the stored max-tf table scales by the constant weight here.
            let bounds: Vec<u32> = self
                .load_bounds(term_id)
                .iter()
                .map(|bound| bound + WEIGHT_BASE)
                .collect();
            buffers.push((blob, bounds));
        }
        if buffers.is_empty() {
            return Ok(Vec::new());
        }

        let mut lists: Vec<QueryList<'_, LiveReader<FreqVarintReader<'_>>>> =
            Vec::with_capacity(buffers.len());
        for (blob, bounds) in &buffers {
            lists.push(QueryList::new(
                LiveReader {
                    inner: FreqVarintReader::new(blob),
                    tombs: &tombs,
                    visible_pos: 0,
                    frontier_live: false,
                },
                WEIGHT_BASE,
                bounds,
                &identity_parts,
            ));
        }
        Ok(topk_disjunctive(&mut lists, k as usize)
            .into_iter()
            .map(|hit| TextHit {
                key: self.key_of_docid(hit.docid).expect("live docid has key"),
                docid: hit.docid,
                score: hit.score,
            })
            .collect())
    }

    /// Unscored live-docid window over one term's postings — the read path of the plan
    /// 0296 fair-pair matrix's custom unscored bench (never part of the Candid surface):
    /// production dictionary probe and postings fetch, fused codec stepping, tombstone
    /// filtering through the same memoized containers as [`Self::search`] (including
    /// bulk dead-range jumps), but NO tf→part lookup and no ranking driver. Returns
    /// `(docid, key)` pairs in ascending docid order, truncated to `limit`.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn first_live_docids(&self, term: &str, limit: u32) -> Vec<(u32, u64)> {
        let Some(term_id) = self.dict_term_id(term) else {
            return Vec::new();
        };
        let Some(blob) = self.postings_blob(term_id) else {
            return Vec::new();
        };
        let tombs = TombFilter::new(&self.tombstones);
        let mut reader = LiveReader {
            inner: FreqVarintReader::new(&blob),
            tombs: &tombs,
            visible_pos: 0,
            frontier_live: false,
        };
        let mut out = Vec::new();
        while out.len() < limit as usize {
            let Some((docid, _tf)) = reader.next_step() else {
                break;
            };
            out.push((docid, self.key_of_docid(docid).expect("live docid has key")));
        }
        out
    }

    /// Tombstone state of one docid (bench oracle uses the same filter as search).
    pub(crate) fn is_tombstoned(&self, docid: u32) -> bool {
        self.tombstones
            .get(u64::from(docid >> 16))
            .is_some_and(|container| container.get(docid))
    }

    // -- Merge: bounded, resumable tombstone reclaim ----------------------------------------

    /// Reclaims up to `min(budget, MAX_MERGE_TERMS_PER_STEP)` terms' tombstoned postings,
    /// resuming from the merge-cursor cell. Terms are visited in ascending dense term_id
    /// order (explicit, deterministic); tombstone containers clear only when the pass
    /// completes ([`MergeStepReport::done`]); stale bits over reclaimed postings are
    /// inert.
    pub fn merge_step(&mut self, budget: u32) -> MergeStepReport {
        let budget = budget.min(MAX_MERGE_TERMS_PER_STEP);
        let mut processed = 0u64;
        let mut reclaimed_units = 0u64;
        let mut done = false;
        while processed < u64::from(budget) {
            let start = self.merge_cursor.get().map_or(0, |last| last + 1);
            // Next live term strictly beyond the resume point, in dense id order
            // (term ids are dense: `next_term_id` equals the entry-array length).
            let next = (start..self.meta.get().next_term_id)
                .map(|term_id| {
                    let entry = self
                        .term_entries
                        .get(u64::from(term_id))
                        .map(TermEntry::from);
                    (term_id, entry)
                })
                .find(|(_, entry)| entry.is_some_and(|e| !e.is_absent()))
                .map(|(term_id, entry)| (term_id, entry.expect("matched live above")));
            let Some((term_id, entry)) = next else {
                self.finish_merge_pass();
                done = true;
                break;
            };
            let unit = String::from_utf8(self.arena.read(entry.str_ref))
                .expect("canonical term strings are valid UTF-8");

            if let Some(dropped) = self.reclaim_term(term_id)
                && dropped > 0
            {
                let remaining_df = self.live_posting_len(term_id);
                if remaining_df == 0 {
                    Self::detach_blob(&mut self.postings, term_id);
                    Self::detach_blob(&mut self.block_max, term_id);
                    let digests = dict_digests(&unit);
                    self.dict_remove_digests(&unit, &digests);
                } else {
                    self.set_term_entry(
                        term_id,
                        TermEntry {
                            str_ref: entry.str_ref,
                            df: remaining_df,
                        },
                    );
                }
                let mut stats = *self.stats.get();
                stats.total_units -= dropped;
                self.stats.set(stats);
                reclaimed_units += dropped;
            }
            self.merge_cursor.set(Some(term_id));
            processed += 1;
        }
        MergeStepReport {
            terms_processed: processed,
            units_reclaimed: reclaimed_units,
            done,
        }
    }

    /// Drops a term's tombstoned postings, returning the number of dropped units
    /// (empty when the term has no stored postings).
    fn reclaim_term(&mut self, term_id: u32) -> Option<u64> {
        let blob = self.postings_blob(term_id)?;
        let mut reader = FreqVarintReader::new(&blob);
        let mut docs = Vec::new();
        let mut tfs = Vec::new();
        let mut dropped = 0u64;
        while let Some(docid) = reader.peek() {
            let tf = reader.freq().expect("interleaved tf");
            let consumed = reader.next().expect("just peeked");
            debug_assert_eq!(docid, consumed);
            if self.is_tombstoned(docid) {
                dropped += 1;
            } else {
                docs.push(docid);
                tfs.push(tf);
            }
        }
        if dropped == 0 {
            return Some(0);
        }
        if docs.is_empty() {
            Self::detach_blob(&mut self.postings, term_id);
            Self::detach_blob(&mut self.block_max, term_id);
        } else {
            let encoded = encode_freq_varint(&docs, &tfs);
            let old = Self::blob_at(&self.postings, term_id);
            let fresh = self.arena.write_over(old, &encoded);
            Self::set_blob(&mut self.postings, term_id, fresh);
            self.rebuild_block_max(term_id, &docs, &tfs);
        }
        Some(dropped)
    }

    fn live_posting_len(&self, term_id: u32) -> u32 {
        self.postings_blob(term_id)
            .map(|blob| FreqVarintReader::new(&blob).len())
            .unwrap_or(0)
    }

    /// Ends a completed merge pass: stale tombstone bits become inert garbage until this
    /// unconditional clear, then the cursor resets for the next pass.
    fn finish_merge_pass(&mut self) {
        while self.tombstones.pop().is_some() {}
        self.merge_cursor.set(None);
        let mut stats = *self.stats.get();
        stats.tombstoned_docs = 0;
        self.stats.set(stats);
    }

    // -- Introspection / admin ---------------------------------------------------------------

    pub fn get_stats(&self) -> TextIndexStats {
        let meta = self.meta.get();
        let stats = self.stats.get();
        TextIndexStats {
            analyzer_id: meta.analyzer_id,
            ndocs: stats.ndocs,
            total_units: stats.total_units,
            tombstoned_docs: stats.tombstoned_docs,
            pending_ops: self.pending.len(),
            segments: self.segments.len() as u32,
            next_docid: meta.next_docid,
        }
    }

    pub fn set_controller(&mut self, controller: Option<Principal>) {
        // `None` stores the anonymous sentinel: on wasm the admin guard denies anonymous
        // callers outright, so an unset controller denies everyone rather than anyone.
        self.controller
            .set(controller.unwrap_or_else(Principal::anonymous));
    }

    /// Configured controller principal. The wasm admin guard reads it; native builds
    /// only reach it from tests, so the accessor stays allow-listed there.
    #[cfg_attr(not(any(target_family = "wasm", test)), allow(dead_code))]
    pub fn controller(&self) -> Principal {
        *self.controller.get()
    }

    /// Sets the provision relay caller (plan 0335 §5-2): the Provision canister principal
    /// allowed on the two relay endpoints in addition to the stored controller (Router).
    /// `None` stores the anonymous sentinel so the relay guard denies everyone.
    pub fn set_dict_relay_caller(&mut self, relay_caller: Option<Principal>) {
        self.dict_relay_caller
            .set(relay_caller.unwrap_or_else(Principal::anonymous));
    }

    /// Configured provision relay caller. The wasm relay guard reads it; native builds
    /// only reach it from tests, so the accessor stays allow-listed there.
    #[cfg_attr(not(any(target_family = "wasm", test)), allow(dead_code))]
    pub fn dict_relay_caller(&self) -> Principal {
        *self.dict_relay_caller.get()
    }
}

/// Tombstone visibility verdict for one candidate docid.
enum Visibility {
    /// Live: the posting reaches the driver.
    Alive,
    /// Tombstoned. Every docid strictly between this candidate and `next_alive` is
    /// tombstoned too, so the whole span can be skipped without decoding any of it.
    /// `None`: every remaining docid of the final container is dead (nothing live can
    /// follow within docid space).
    Dead { next_alive: Option<u32> },
}

/// Filter view [`LiveReader`] consults while positioning: one call answers the alive
/// test AND yields the bulk dead-range hint, replacing per-posting closure bit tests.
trait TombstoneView {
    /// Classifies one candidate docid.
    fn classify(&self, docid: u32) -> Visibility;
}

/// [`TombstoneView`] over the dense container store. Containers are 8 KiB stable slots;
/// they are memoized per query because postings ascend within a list — one stable read
/// per container change instead of ~df reads. A store with NO containers has no
/// tombstoned docids by construction, so classification short-circuits on a flag
/// checked once per candidate instead of entering the memo machinery.
struct TombFilter<'a, M: ic_stable_structures::Memory> {
    containers: &'a StableVec<Tombstone, M>,
    /// `containers.len() > 0`, resolved once per query.
    any: bool,
    cache: RefCell<(u64, Option<Tombstone>)>,
}

impl<'a, M: ic_stable_structures::Memory> TombFilter<'a, M> {
    fn new(containers: &'a StableVec<Tombstone, M>) -> Self {
        Self {
            containers,
            any: !containers.is_empty(),
            cache: RefCell::new((u64::MAX, None)),
        }
    }

    fn container(&self, ordinal: u64) -> Option<Tombstone> {
        let mut cached = self.cache.borrow_mut();
        if cached.0 != ordinal {
            cached.1 = self.containers.get(ordinal);
            cached.0 = ordinal;
        }
        cached.1.clone()
    }
}

impl<M: ic_stable_structures::Memory> TombstoneView for TombFilter<'_, M> {
    fn classify(&self, docid: u32) -> Visibility {
        if !self.any {
            return Visibility::Alive;
        }
        let ordinal = u64::from(docid >> 16);
        let Some(container) = self.container(ordinal) else {
            return Visibility::Alive; // no container ⇒ nothing tombstoned in range
        };
        if !container.get(docid) {
            return Visibility::Alive;
        }
        Visibility::Dead {
            next_alive: first_clear_bit(&container.0, docid & 0xFFFF)
                .map(|bit| ((ordinal << 16) as u32) + bit),
        }
    }
}

/// Index of the first clear bit strictly after `bit`; `None` when every later bit of
/// `bits` is set (all remaining docids of the container are tombstoned). Byte-wise scan:
/// it runs once per dead run, and bulk-jumped runs cost no per-posting visits at all.
///
/// # Panics
/// Panics (debug) when `bit` itself is clear — callers invoke this only on proven-dead
/// docids.
fn first_clear_bit(bits: &[u8], bit: u32) -> Option<u32> {
    debug_assert!(
        bits[(bit / 8) as usize] & (1 << (bit % 8)) != 0,
        "dead-range hint requested for a live docid"
    );
    let total_bits = (bits.len() * 8) as u32;
    let mut candidate = bit + 1;
    while candidate < total_bits {
        let byte = bits[(candidate / 8) as usize];
        let mask = u8::MAX << (candidate % 8); // candidate ..= end of its byte
        if byte & mask != mask {
            for offset in 0..8 - candidate % 8 {
                let q = candidate + offset;
                if bits[(q / 8) as usize] & (1 << (q % 8)) == 0 {
                    return Some(q);
                }
            }
            unreachable!("mask proved a clear bit inside this byte");
        }
        candidate |= 7; // hop to the next byte boundary
        candidate += 1;
    }
    None
}

/// Posting-reader wrapper that hides tombstoned docids from the promoted driver.
///
/// `pos()` reports *visible* positions only, keeping [`QueryList`]'s per-position
/// score table aligned; stored block-max bounds remain valid upper bounds because
/// filtering can only lower per-block maxima.
///
/// Positioning economics (plan 0296): each posting's visibility is classified exactly
/// once — `frontier_live` memoizes the verdict across the driver's peek-then-step
/// pattern instead of re-testing per accessor — and contiguous tombstoned runs reaching
/// past the current logical block jump through the codec's bi-level skip trailer via
/// `advance(next_live_hint)` rather than decoding dead postings. The exposed sequence
/// is exactly the alive subsequence, so filtering equivalence holds by construction.
struct LiveReader<'a, R: ic_stable_text_postings::enc::PostingReader> {
    inner: R,
    tombs: &'a dyn TombstoneView,
    visible_pos: u32,
    /// True while `inner`'s frontier is already verified live (or exhausted): all
    /// tombstone work is skipped until the inner cursor moves again.
    frontier_live: bool,
}

impl<'a, R: ic_stable_text_postings::enc::PostingReader> LiveReader<'a, R> {
    /// Positions `inner` at the next live posting (or exhaustion), cheaply when the
    /// cached verdict still holds.
    fn skip_dead(&mut self) {
        if self.frontier_live {
            return;
        }
        while let Some(docid) = self.inner.peek() {
            match self.tombs.classify(docid) {
                Visibility::Alive => break,
                Visibility::Dead { next_alive } => {
                    #[cfg(test)]
                    driver_counters::filter_test();
                    match next_alive {
                        // Dead run reaches into a later logical block: jump there via
                        // the skip trailer instead of decoding dead postings one by one.
                        Some(hint) if hint.saturating_sub(docid) > LOGICAL_BLOCK_SIZE => {
                            self.inner.advance(hint);
                            #[cfg(test)]
                            driver_counters::block_jump();
                        }
                        // Short run: linear consumption beats skip-trailer search.
                        _ => {
                            self.inner.next();
                            #[cfg(test)]
                            driver_counters::dead_linear_step();
                        }
                    }
                }
            }
        }
        self.frontier_live = true;
    }
}

impl<R: ic_stable_text_postings::enc::PostingReader> ic_stable_text_postings::enc::PostingReader
    for LiveReader<'_, R>
{
    fn len(&self) -> u32 {
        self.inner.len()
    }

    fn pos(&self) -> u32 {
        self.visible_pos
    }

    fn peek(&mut self) -> Option<u32> {
        self.skip_dead();
        self.inner.peek()
    }

    fn next(&mut self) -> Option<u32> {
        self.skip_dead();
        let value = self.inner.next();
        if value.is_some() {
            self.visible_pos += 1;
            self.frontier_live = false;
        }
        value
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        self.inner.advance(target);
        self.frontier_live = false;
        self.skip_dead();
        self.inner.peek()
    }

    /// Forwards the inner codec's stored tf for the (already tombstone-filtered)
    /// frontier so lazy scoring sees real frequencies, not the tf-less default.
    fn tf(&mut self) -> Option<u32> {
        self.skip_dead();
        if self.inner.peek().is_some() {
            self.inner.tf()
        } else {
            None
        }
    }

    /// Fused visible step: one inner dispatch consumes the verified-live frontier
    /// posting with its tf; the verdict cache resets so the next positioning re-filters
    /// from wherever the cursor lands.
    fn next_step(&mut self) -> Option<(u32, u32)> {
        self.skip_dead();
        let step = self.inner.next_step()?;
        self.visible_pos += 1;
        self.frontier_live = false;
        #[cfg(test)]
        driver_counters::visible_step();
        Some(step)
    }
}

/// Slice-12 decomposition counters (`cfg(test)` only): hot-path events of the
/// tombstone-filtered driver. The decomposition test resets them around a search and
/// reports postings visited / filter tests / jumps to price the filtered-vs-bare gap.
#[cfg(test)]
mod driver_counters {
    use std::cell::Cell;

    thread_local! {
        static VISIBLE_STEPS: Cell<u64> = const { Cell::new(0) };
        static DEAD_LINEAR_STEPS: Cell<u64> = const { Cell::new(0) };
        static FILTER_TESTS: Cell<u64> = const { Cell::new(0) };
        static BLOCK_JUMPS: Cell<u64> = const { Cell::new(0) };
    }

    pub(super) fn reset() {
        VISIBLE_STEPS.with(|c| c.set(0));
        DEAD_LINEAR_STEPS.with(|c| c.set(0));
        FILTER_TESTS.with(|c| c.set(0));
        BLOCK_JUMPS.with(|c| c.set(0));
    }

    pub(super) fn visible_step() {
        VISIBLE_STEPS.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn dead_linear_step() {
        DEAD_LINEAR_STEPS.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn filter_test() {
        FILTER_TESTS.with(|c| c.set(c.get() + 1));
    }

    pub(super) fn block_jump() {
        BLOCK_JUMPS.with(|c| c.set(c.get() + 1));
    }

    /// One hot-path event report (see module docs).
    pub(super) struct Snapshot {
        pub(super) visible_steps: u64,
        pub(super) dead_linear_steps: u64,
        pub(super) filter_tests: u64,
        pub(super) block_jumps: u64,
    }

    pub(super) fn snapshot() -> Snapshot {
        Snapshot {
            visible_steps: VISIBLE_STEPS.with(Cell::get),
            dead_linear_steps: DEAD_LINEAR_STEPS.with(Cell::get),
            filter_tests: FILTER_TESTS.with(Cell::get),
            block_jumps: BLOCK_JUMPS.with(Cell::get),
        }
    }
}

// -- Process-wide store binding ------------------------------------------------------------

thread_local! {
    static STORES: RefCell<Option<TextStores<Memory>>> = const { RefCell::new(None) };
    /// Install-arg analyzer id (plan 0331), captured by the init handler BEFORE the
    /// lazily-opened store reads it. `None` = default unicode-bigram; post-upgrade
    /// reopens leave it `None` (the persisted meta is the pinned analyzer SSOT).
    static INIT_ANALYZER: RefCell<Option<u32>> = const { RefCell::new(None) };
}

/// Records the install-arg analyzer id for the first open (the init handler must call
/// it before any `with_stores`).
pub(crate) fn set_init_analyzer(analyzer_id: Option<u32>) {
    INIT_ANALYZER.with(|slot| *slot.borrow_mut() = analyzer_id);
}

/// Runs `f` against the lazily-opened production store. First use performs the one
/// `MemoryManager::init` and the layout validation; upgrade reopen reuses the same path
/// (the persisted meta stays the pinned analyzer SSOT on reopen).
pub(crate) fn with_stores<R>(f: impl FnOnce(&mut TextStores<Memory>) -> R) -> R {
    let init_analyzer = INIT_ANALYZER.with(|slot| *slot.borrow());
    STORES.with(|slot| {
        let mut slot = slot.borrow_mut();
        let stores = slot.get_or_insert_with(|| {
            TextStores::init_with_analyzer(TextMemories::production(), init_analyzer)
        });
        f(stores)
    })
}

/// Fresh, independent in-memory regions for sibling-module tests (the struct
/// fields are private to this module, so construction is offered here instead).
#[cfg(test)]
pub(crate) fn fresh_vector_memories() -> TextMemories<ic_stable_structures::VectorMemory> {
    TextMemories {
        meta: Default::default(),
        segments: Default::default(),
        dict: Default::default(),
        postings: Default::default(),
        block_max: Default::default(),
        key_by_docid: Default::default(),
        docid_by_key: Default::default(),
        tombstones: Default::default(),
        stats: Default::default(),
        pending: Default::default(),
        merge_cursor: Default::default(),
        controller: Default::default(),
        arena: Default::default(),
        term_entries: Default::default(),
        dict_blob: Default::default(),
        dict_relay_caller: Default::default(),
    }
}

#[cfg(test)]
mod tests;
