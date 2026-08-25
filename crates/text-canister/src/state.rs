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
use xxhash_rust::xxh3::{xxh3_128, xxh3_128_with_seed};

use crate::analyzer::analyze;
use crate::{FlushReport, MergeStepReport, TextDoc, TextHit, TextIndexStats};

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
/// Layout 4 (plan 0297 backfill-pull): adds the ADR 0059 §Text build kind durable regions
/// 14/15 (backfill registration cell + resumable cursor cell). Layouts 1–3 fail loudly at
/// open; fresh state is required (pre-production rule).
const LAYOUT_VERSION: u32 = 4;
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
}

impl TextMemories<Memory> {
    /// Binds all fourteen production regions through the single `MemoryManager`.
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
}

impl Default for TextMeta {
    fn default() -> Self {
        Self {
            magic: MAGIC,
            layout_version: LAYOUT_VERSION,
            analyzer_id: crate::analyzer::ANALYZER_ID,
            next_docid: 0,
            next_term_id: 0,
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
}

/// Computes the probe digests of one term (dual-domain xxh3_128 over UTF-8 bytes).
fn dict_digests(term: &str) -> [u128; DICT_PROBES] {
    let bytes = term.as_bytes();
    [xxh3_128(bytes), xxh3_128_with_seed(bytes, DICT_PROBE_SEED)]
}

impl<M: ic_stable_structures::Memory> TextStores<M> {
    /// Opens every region load-or-create and validates the layout header FIRST: foreign
    /// or incompatible meta bytes fail closed (assert) before any structure binds its
    /// region, matching pre-production simplicity — layout changes require fresh state,
    /// not migrations.
    pub fn init(memories: TextMemories<M>) -> Self {
        let meta = Cell::init(memories.meta, TextMeta::default());
        let header = meta.get();
        assert!(
            header.magic == MAGIC && header.layout_version == LAYOUT_VERSION,
            "incompatible text index layout: magic {:#x} version {}",
            header.magic,
            header.layout_version
        );

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
        };
        if stores.segments.is_empty() {
            stores
                .segments
                .insert(ACTIVE_SEGMENT_ID, SegmentRow { active: true });
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
        for term in analyze(query) {
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

/// Fourteen fresh, independent in-memory regions for sibling-module tests (the struct
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
    }
}

#[cfg(test)]
mod tests;
