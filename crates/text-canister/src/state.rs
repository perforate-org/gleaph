//! Stable state and operations for the Text Index canister (ADR 0077 engine, plan 0294).
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
//! | 2 | `BTreeMap<String, TermEntry>` | active-segment term dictionary (unit → term_id/df) |
//! | 3 | `BTreeMap<u32, Vec<u8>>` | active-segment postings (term_id → freq-varint blob) |
//! | 4 | `BTreeMap<u32, Vec<u8>>` | active-segment block-max tables (term_id → LE u32) |
//! | 5 | `BTreeMap<u32, u64>` | docid → doc key (hit projection) |
//! | 6 | `BTreeMap<u64, u32>` | doc key → docid (delete/update addressing) |
//! | 7 | `BTreeMap<u16, Tombstone>` | tombstone bitset containers (64 Ki docs each) |
//! | 8 | `Cell<TextStats>` | global stats record |
//! | 9 | `BTreeMap<u64, PendingOp>` | durable pending ops log (FIFO by op seq) |
//! | 10 | `Cell<Option<String>>` | resumable merge cursor (last reclaimed unit) |
//! | 11 | `Cell<Principal>` | controller principal for admin guards |
//!
//! Per-segment posting/dict stores materialize lazily on flush: the maps above bind their
//! regions at first open but stay empty until the first applied delta.
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
//!
//! ## Scoring policy (v0 placeholder)
//!
//! Scoring formulas belong to the index definition catalog; the physical layer consumes
//! caller-supplied parts. Until catalog wiring lands, search uses the identity part model:
//! contribution = [`WEIGHT_BASE`] + stored term frequency, and block-max tables (stored as
//! max tf) are scaled by the constant weight at query time to satisfy the driver's
//! contribution-bound contract. Deterministic tie-break (score desc, docid asc) comes from
//! the promoted driver.

use std::borrow::Cow;
use std::cell::RefCell;
use std::ops::Bound;

use candid::{CandidType, Decode, Encode, Principal};
use ic_stable_memory_backend::DefaultMemoryImpl;
use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};
use ic_stable_structures::storable::{Bound as SBound, Storable};
use ic_stable_structures::{BTreeMap, Cell};
use ic_stable_text_postings::blockmax::LOGICAL_BLOCK_SIZE;
use ic_stable_text_postings::enc::{FreqVarintReader, PostingReader, encode_freq_varint};
use ic_stable_text_postings::topk::{QueryList, topk_disjunctive};
use serde::{Deserialize, Serialize};

use crate::analyzer::analyze;
use crate::{FlushReport, MergeStepReport, TextDoc, TextHit, TextIndexStats};

pub(crate) type Memory = VirtualMemory<DefaultMemoryImpl>;

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
/// Pending ops applied per `admin_flush` call; repeat until [`FlushReport::done`].
pub(crate) const FLUSH_OPS_BUDGET: u64 = 512;
/// Terms reclaimed per `admin_merge_step` call (budget parameter clamps to this).
pub(crate) const MAX_MERGE_TERMS_PER_STEP: u32 = 1_024;
/// Constant weight every matched query list contributes (identity scorer; see module docs).
pub const WEIGHT_BASE: u32 = 1;

const MAGIC: u64 = u64::from_le_bytes(*b"GLEAPHTX");
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

fn region(id: MemoryId) -> Memory {
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
}

impl TextMemories<Memory> {
    /// Binds all twelve production regions through the single `MemoryManager`.
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
    next_op_seq: u64,
}

impl Default for TextMeta {
    fn default() -> Self {
        Self {
            magic: MAGIC,
            layout_version: LAYOUT_VERSION,
            analyzer_id: crate::analyzer::ANALYZER_ID,
            next_docid: 0,
            next_term_id: 0,
            next_op_seq: 0,
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

/// Dictionary value: dense term id plus live document frequency (df tracks postings after
/// tombstone reclamation).
#[derive(CandidType, Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
struct TermEntry {
    term_id: u32,
    df: u32,
}

impl Storable for TermEntry {
    const BOUND: SBound = SBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode TermEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode TermEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), TermEntry).expect("decode TermEntry")
    }
}

/// Durable pending op. Units are carried verbatim so `admin_flush` applies exactly what
/// was ingested (the analyzer runs once, at enqueue time).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
enum PendingOp {
    Upsert { key: u64, units: Vec<String> },
    Delete { key: u64 },
}

impl Storable for PendingOp {
    const BOUND: SBound = SBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode PendingOp"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode PendingOp")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), PendingOp).expect("decode PendingOp")
    }
}

/// One tombstone bitset container covering 64 Ki consecutive docids (container key =
/// `docid >> 16`, bit index = `docid & 0xFFFF`).
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
    dict: BTreeMap<String, TermEntry, M>,
    postings: BTreeMap<u32, Vec<u8>, M>,
    block_max: BTreeMap<u32, Vec<u8>, M>,
    key_by_docid: BTreeMap<u32, u64, M>,
    docid_by_key: BTreeMap<u64, u32, M>,
    tombstones: BTreeMap<u16, Tombstone, M>,
    stats: Cell<TextStats, M>,
    pending: BTreeMap<u64, PendingOp, M>,
    merge_cursor: Cell<Option<String>, M>,
    controller: Cell<Principal, M>,
}

impl<M: ic_stable_structures::Memory> TextStores<M> {
    /// Opens every region load-or-create and validates the layout header. Foreign or
    /// incompatible bytes fail closed (assert), matching pre-production simplicity:
    /// layout changes require fresh state, not migrations.
    pub fn init(memories: TextMemories<M>) -> Self {
        let mut stores = Self {
            meta: Cell::init(memories.meta, TextMeta::default()),
            segments: BTreeMap::init(memories.segments),
            dict: BTreeMap::init(memories.dict),
            postings: BTreeMap::init(memories.postings),
            block_max: BTreeMap::init(memories.block_max),
            key_by_docid: BTreeMap::init(memories.key_by_docid),
            docid_by_key: BTreeMap::init(memories.docid_by_key),
            tombstones: BTreeMap::init(memories.tombstones),
            stats: Cell::init(memories.stats, TextStats::default()),
            pending: BTreeMap::init(memories.pending),
            merge_cursor: Cell::init(memories.merge_cursor, None),
            controller: Cell::init(memories.controller, Principal::anonymous()),
        };
        let meta = stores.meta.get();
        assert!(
            meta.magic == MAGIC && meta.layout_version == LAYOUT_VERSION,
            "incompatible text index layout: magic {:#x} version {}",
            meta.magic,
            meta.layout_version
        );
        if stores.segments.is_empty() {
            stores
                .segments
                .insert(ACTIVE_SEGMENT_ID, SegmentRow { active: true });
        }
        stores
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
            let units = analyze(&doc.text);
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

    fn append_pending(&mut self, op: PendingOp) {
        let mut meta = self.meta.get().clone();
        let seq = meta.next_op_seq;
        meta.next_op_seq = seq.checked_add(1).expect("op sequence exhausted");
        self.meta.set(meta);
        self.pending.insert(seq, op);
    }

    // -- Flush: apply a bounded FIFO prefix of the pending log ----------------------------

    /// Applies up to `max_ops` pending ops in FIFO order. Repeat until
    /// [`FlushReport::done`]; application order is fully determined by op sequence.
    pub fn flush_step(&mut self, max_ops: u64) -> FlushReport {
        let mut drained = 0u64;
        while drained < max_ops {
            let Some((_seq, op)) = self.pending.pop_first() else {
                break;
            };
            match op {
                PendingOp::Upsert { key, units } => self.apply_upsert(key, &units),
                PendingOp::Delete { key } => self.apply_delete(key),
            }
            drained += 1;
        }
        let remaining_ops = self.pending.len();
        FlushReport {
            drained_ops: drained,
            remaining_ops,
            done: remaining_ops == 0,
        }
    }

    /// Applies one upsert: update = delete + insert (the prior incarnation's docid is
    /// tombstoned first), then a fresh docid receives the new units.
    fn apply_upsert(&mut self, key: u64, units: &[String]) {
        if let Some(old_docid) = self.docid_by_key.remove(&key) {
            self.key_by_docid.remove(&old_docid);
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
            let term_entry = match self.dict.get(unit) {
                Some(entry) => entry,
                None => {
                    let mut meta = self.meta.get().clone();
                    let term_id = meta.next_term_id;
                    meta.next_term_id = term_id.checked_add(1).expect("term id space exhausted");
                    self.meta.set(meta);
                    TermEntry { term_id, df: 0 }
                }
            };
            self.append_posting(term_entry.term_id, docid, *tf);
            self.dict.insert(
                unit.clone(),
                TermEntry {
                    term_id: term_entry.term_id,
                    df: term_entry.df + 1,
                },
            );
        }

        self.key_by_docid.insert(docid, key);
        self.docid_by_key.insert(key, docid);
        let mut stats = *self.stats.get();
        stats.ndocs += 1;
        stats.total_units += units.len() as u64;
        self.stats.set(stats);
    }

    /// Applies one delete: unknown keys are no-ops; known keys tombstone their docid and
    /// drop the key mappings (physical reclaim defers to `merge_step`).
    fn apply_delete(&mut self, key: u64) {
        if let Some(docid) = self.docid_by_key.remove(&key) {
            self.key_by_docid.remove(&docid);
            self.mark_tombstoned(docid);
        }
    }

    fn mark_tombstoned(&mut self, docid: u32) {
        let container_key = (docid >> 16) as u16;
        let mut container = self.tombstones.get(&container_key).unwrap_or_default();
        container.set(docid);
        self.tombstones.insert(container_key, container);
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
        if let Some(blob) = self.postings.get(&term_id) {
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
        self.postings
            .insert(term_id, encode_freq_varint(&docs, &tfs));
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
        self.block_max.insert(term_id, bytes);
    }

    fn load_bounds(&self, term_id: u32) -> Vec<u32> {
        self.block_max
            .get(&term_id)
            .map(|bytes| {
                bytes
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .map(|chunk| u32::from_le_bytes(*chunk))
                    .collect()
            })
            .unwrap_or_default()
    }

    // -- Search ---------------------------------------------------------------------------

    /// DAAT top-k over live postings minus tombstones, using the promoted driver.
    ///
    /// Query terms analyze through the production analyzer; duplicates collapse (each
    /// matched term contributes once). Unflushed terms simply miss the dictionary, which
    /// is the documented under-posted-until-flush lag, and tombstoned docids never reach
    /// the driver because posting readers filter them.
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

        // Materialize per-term buffers (cloned blob, decoded bounds, live parts) so the
        // readers below borrow locals instead of `&self`.
        let alive = |docid: u32| !self.is_tombstoned(docid);
        let mut seen = std::collections::BTreeSet::new();
        let mut buffers: Vec<(Vec<u8>, Vec<u32>, Vec<u32>)> = Vec::new();
        for term in analyze(query) {
            if !seen.insert(term.clone()) {
                continue;
            }
            let Some(TermEntry { term_id, .. }) = self.dict.get(&term) else {
                continue;
            };
            let Some(blob) = self.postings.get(&term_id) else {
                continue;
            };
            // Live-only parts walk: identical filter/order to the driver's consumption,
            // so parts indexes align with visible posting positions.
            let mut probe = FreqVarintReader::new(&blob);
            let mut parts = Vec::new();
            while let Some(docid) = probe.peek() {
                let tf = probe.freq().expect("interleaved tf");
                let consumed = probe.next().expect("just peeked");
                debug_assert_eq!(docid, consumed);
                if alive(docid) {
                    parts.push(tf);
                }
            }
            if parts.is_empty() {
                continue;
            }
            // Driver contract: per-block bounds cap the TOTAL contribution (weight +
            // part), so the stored max-tf table scales by the constant weight here.
            let bounds: Vec<u32> = self
                .load_bounds(term_id)
                .iter()
                .map(|bound| bound + WEIGHT_BASE)
                .collect();
            buffers.push((blob, bounds, parts));
        }
        if buffers.is_empty() {
            return Ok(Vec::new());
        }

        let mut lists: Vec<QueryList<'_, LiveReader<FreqVarintReader<'_>>>> =
            Vec::with_capacity(buffers.len());
        for (blob, bounds, parts) in &buffers {
            lists.push(QueryList::new(
                LiveReader {
                    inner: FreqVarintReader::new(blob),
                    alive: &alive,
                    visible_pos: 0,
                },
                WEIGHT_BASE,
                bounds,
                parts,
            ));
        }
        Ok(topk_disjunctive(&mut lists, k as usize)
            .into_iter()
            .map(|hit| TextHit {
                key: self
                    .key_by_docid
                    .get(&hit.docid)
                    .expect("live docid has key"),
                docid: hit.docid,
                score: hit.score,
            })
            .collect())
    }

    fn is_tombstoned(&self, docid: u32) -> bool {
        self.tombstones
            .get(&((docid >> 16) as u16))
            .is_some_and(|container| container.get(docid))
    }

    // -- Merge: bounded, resumable tombstone reclaim ----------------------------------------

    /// Reclaims up to `min(budget, MAX_MERGE_TERMS_PER_STEP)` terms' tombstoned postings,
    /// resuming from the merge-cursor cell. Tombstone containers clear only when the pass
    /// completes ([`MergeStepReport::done`]); stale bits over reclaimed postings are inert.
    pub fn merge_step(&mut self, budget: u32) -> MergeStepReport {
        let budget = budget.min(MAX_MERGE_TERMS_PER_STEP);
        let mut processed = 0u64;
        let mut reclaimed_units = 0u64;
        let mut done = false;
        while processed < u64::from(budget) {
            let resume = self.merge_cursor.get();
            // Peek the next term beyond the resume point without holding borrows across
            // mutation; iteration order is UTF-8 byte order (deterministic).
            let next: Option<(String, u32, u32)> = match resume.as_deref() {
                Some(last_unit) => self
                    .dict
                    .range((Bound::Excluded(last_unit.to_string()), Bound::Unbounded))
                    .next()
                    .map(|entry| (entry.key().clone(), entry.value().term_id, entry.value().df)),
                None => self
                    .dict
                    .iter()
                    .next()
                    .map(|entry| (entry.key().clone(), entry.value().term_id, entry.value().df)),
            };
            let Some((unit, term_id, _df)) = next else {
                self.finish_merge_pass();
                done = true;
                break;
            };

            if let Some(dropped) = self.reclaim_term(term_id)
                && dropped > 0
            {
                let remaining_df = self.live_posting_len(term_id);
                if remaining_df == 0 {
                    self.postings.remove(&term_id);
                    self.block_max.remove(&term_id);
                    self.dict.remove(&unit);
                } else {
                    self.dict.insert(
                        unit.clone(),
                        TermEntry {
                            term_id,
                            df: remaining_df,
                        },
                    );
                }
                let mut stats = *self.stats.get();
                stats.total_units -= dropped;
                self.stats.set(stats);
                reclaimed_units += dropped;
            }
            self.merge_cursor.set(Some(unit));
            processed += 1;
        }
        MergeStepReport {
            terms_processed: processed,
            units_reclaimed: reclaimed_units,
            done,
        }
    }

    /// Drops a term's tombstoned postings, returning the number of dropped units
    /// (`None` when the term has no stored postings).
    fn reclaim_term(&mut self, term_id: u32) -> Option<u64> {
        let blob = self.postings.get(&term_id)?;
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
            self.postings.remove(&term_id);
            self.block_max.remove(&term_id);
        } else {
            self.postings
                .insert(term_id, encode_freq_varint(&docs, &tfs));
            self.rebuild_block_max(term_id, &docs, &tfs);
        }
        Some(dropped)
    }

    fn live_posting_len(&self, term_id: u32) -> u32 {
        self.postings
            .get(&term_id)
            .map(|blob| FreqVarintReader::new(&blob).len())
            .unwrap_or(0)
    }

    /// Ends a completed merge pass: stale tombstone bits become inert garbage until this
    /// unconditional clear, then the cursor resets for the next pass.
    fn finish_merge_pass(&mut self) {
        while self.tombstones.pop_first().is_some() {}
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
}

/// Posting-reader wrapper that hides tombstoned docids from the promoted driver.
///
/// `pos()` reports *visible* positions only, keeping [`QueryList`]'s per-position
/// `score_parts` aligned; stored block-max bounds remain valid upper bounds because
/// filtering can only lower per-block maxima.
struct LiveReader<'a, R: ic_stable_text_postings::enc::PostingReader> {
    inner: R,
    alive: &'a dyn Fn(u32) -> bool,
    visible_pos: u32,
}

impl<'a, R: ic_stable_text_postings::enc::PostingReader> LiveReader<'a, R> {
    fn skip_dead(&mut self) {
        while let Some(docid) = self.inner.peek() {
            if (self.alive)(docid) {
                break;
            }
            self.inner.next();
        }
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
        }
        value
    }

    fn advance(&mut self, target: u32) -> Option<u32> {
        self.inner.advance(target);
        self.skip_dead();
        self.peek()
    }
}

// -- Process-wide store binding ------------------------------------------------------------

thread_local! {
    static STORES: RefCell<Option<TextStores<Memory>>> = const { RefCell::new(None) };
}

/// Runs `f` against the lazily-opened production store. First use performs the one
/// `MemoryManager::init` and the layout validation; upgrade reopen reuses the same path.
pub(crate) fn with_stores<R>(f: impl FnOnce(&mut TextStores<Memory>) -> R) -> R {
    STORES.with(|slot| {
        let mut slot = slot.borrow_mut();
        let stores = slot.get_or_insert_with(|| TextStores::init(TextMemories::production()));
        f(stores)
    })
}

#[cfg(test)]
mod tests;
