//! Vector-index-owned composite slab page store (ADR 0064 §7 two-table format).
//!
//! Replaces the former ADR 0032 structure-of-arrays page store with the
//! `ic-stable-vector-page-store` two-table page format:
//!
//! ```text
//! [PageHeader] [run_table × run_capacity] [row_meta × capacity] [vector_bytes × capacity]
//! ```
//!
//! One raw stable region is owned here:
//!
//! - `VECTOR_ROW_SLAB` (raw stable memory, MemoryId 13) — the physical row bytes behind a
//!   `VSL`/version-1 slab header.
//!
//! **Arithmetic block addressing (Phase-0 Slice 8).** Every slab page occupies exactly one uniform
//! block of [`BLOCK_LEN`] bytes at `SLAB_HEADER_SIZE + seq × BLOCK_LEN`; the former MemoryId 10
//! `BTreeMap<PageKey, VectorPageMeta>` directory is abolished (MemoryId 10 is now an explicit
//! layout hole). Page geometry has a single authoritative owner — the index's [`VectorIndexDef`] —
//! and every def's page span fits its block by construction (`slots_per_page` is derived from
//! `max_page_bytes == BLOCK_LEN`); the trailing slack inside a block is deterministically zeroed
//! at reservation. Per-page state lives in the partition-heads collection instead:
//! [`PartitionHead`] mirrors the single mutable tail page as scalars (so an append performs
//! exactly two durable map ops — head get + head insert), sealed pages live in the companion
//! [`PageTable`] record (`{seq, row_count, live_count, block_bound}` per positional page id), and
//! an intrusive **free-block chain** (each dead block stores the next seq in its own first four
//! bytes, anchored in the durable compaction-state record) enables free-side-first reuse after
//! version teardowns and compaction reclamation.
//!
//! Each row stores only its packed 30-bit [`VertexPayload`] (vertex id + bit-31 tombstone); the shard
//! is shared across contiguous rows via the run table, so a shard is recorded once per run, not per
//! row. `vector_bytes` rows are `pad_stride_bytes` wide (16-byte aligned for SIMD), and the trailing
//! pad region is zero-filled so scoring kernels never observe non-finite garbage. Rows are
//! **write-once at tail positions**: superseded rows are tombstoned (bit 31), never rewritten, so
//! freshness is validated positionally (subject-map slot matches the scanned position) rather than by
//! a row-carried `vector_id`/`generation`.
//!
//! Version teardown drains whole partitions atomically (sealed chunks + blocks + leaf head) so
//! their blocks become free-listed holes reusable before the tail grows. The opt-in bounded
//! slab compaction (plan 0278, see `facade/store/compact.rs`) copies live blocks down into a dense
//! prefix, rewires their owning records' `seq`, and reclaims the gap for the free list.
//! The format lineage restarts at version 1 (breaking; dev data wiped); the discarded
//! ASCII-magic format is rejected fail-closed.

use super::memory::{Memory, init_row_slab};
use crate::facade::stable::{
    VECTOR_PARTITION_HEADS, page_table_chunk_get, page_table_chunk_put, page_table_remove_all,
    partition_head_get, partition_head_insert, partition_head_remove, slab_free_anchor_get,
    slab_free_anchor_set,
};
use crate::records::{
    ENTRIES_PER_TABLE_CHUNK, MAX_PAGE_TABLE_CHUNKS, PARTITION_LEVEL_PAGE_TABLE_BASE, PageKey,
    PageTableChunk, PageTableEntry, PartitionHead, PartitionHeadRecord, PartitionKey, SlotRef,
    VectorIndexDef,
};
use gleaph_graph_kernel::vector_index::{
    VectorCanisterError, VectorEncoding, VectorPartitionHealthStep, VectorPartitionPageHealth,
    VectorSlabGlobalStats, VectorSlabScopeStats, VectorSlabStats, VectorSlabStatsPartial,
    VectorSlabStatsStep, VectorSlabStepGlobalStats, VectorSlabVersionStats, VectorSubject,
};
use ic_stable_structures::Memory as _;
use ic_stable_structures::storable::Storable;
use ic_stable_vector_page_store::{
    PAGE_HEADER_SIZE, PageHeader, PageLayout, RowMeta, RunEntry, SLAB_HEADER_SIZE, Slab,
    SlabHeader, VertexPayload, header::MAX_META_STRIDE, read_run,
};
use std::borrow::Cow;

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

/// Uniform slab block length (Phase-0 Slice 8): every page occupies one block, so physical
/// addresses derive arithmetically from the block sequence number. All defs fill their pages up
/// to this budget (`DEFAULT_MAX_PAGE_BYTES`), so `page_span ≤ BLOCK_LEN` holds by construction
/// and is re-validated fail-closed at reservation.
const BLOCK_LEN: u64 = crate::facade::store::DEFAULT_MAX_PAGE_BYTES as u64;

/// Physical base address of block `seq`.
fn block_offset(seq: u32) -> u64 {
    SLAB_HEADER_SIZE as u64 + u64::from(seq) * BLOCK_LEN
}

/// Block sequence just past the highest allocated block (the next tail allocation index).
fn tail_next_seq(occupied_tail: u64) -> u32 {
    ((occupied_tail - SLAB_HEADER_SIZE as u64) / BLOCK_LEN) as u32
}

/// WASM stable-memory page size in bytes.
const WASM_PAGE_SIZE: u64 = 65_536;

/// Per-step page-meta budget cap for [`VectorSlabStore::stats_step`] (mirrors `MAX_REBUILD_STEP_WORK`).
const MAX_SLAB_STATS_STEP_PAGES: u32 = 20_000;

/// Encoded length of a [`PageKey`] (its fixed `Storable` bound). A caller-supplied
/// [`VectorSlabStore::stats_step`] cursor must be exactly this many bytes.
const PAGE_KEY_LEN: usize = 24;

/// One page in a fully prevalidated [`VectorSlabStore::append_rows`] batch plan. Reserved blocks
/// remain unreachable until the batch's single fallible partition-head update succeeds.
struct BatchPagePlan {
    /// Positional page id within the partition chain (`SlotRef.page_id`).
    page_id: u64,
    /// Slab block sequence backing this page.
    page_seq: u32,
    slab_offset: u64,
    row_start: usize,
    row_end: usize,
}

/// Resolved location + counters of one page of a partition chain, yielded by
/// [`VectorSlabStore::partition_page_metas`]. `page_id` is the positional id used by `SlotRef`;
/// `block_bound` is the scalar skip bound `M = max‖row‖` (0.0 for an empty mutable page).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PageMetaView {
    pub page_id: u64,
    pub slab_offset: u64,
    pub row_count: u32,
    pub live_count: u32,
    pub block_bound: f32,
}

/// Test-facing snapshot of one page's former directory entry (`slab_offset`, `row_count`,
/// `live_count`), derived from the owning head + sealed-page table records.
#[cfg(test)]
pub(crate) struct VectorPageMeta {
    pub slab_offset: u64,
    pub row_count: u32,
    pub live_count: u32,
}

/// Decoded identity of one live scanned page row: the shared shard (from the run table) and the
/// 30-bit vertex id (from the packed payload). Tombstoned rows are filtered before this is built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowInfo {
    pub shard_id: u32,
    pub vertex_id: u32,
    /// Row-meta aux bytes (0 | 4 | 8 meaningful; `meta_stride - 4`). For `I8` rows the first 4 bytes
    /// hold the per-row quantization scale; the page store keeps aux opaque.
    pub aux: [u8; 8],
}

/// Outcome of one bounded [`VectorSlabStore::drop_version_pages`] step.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct DropProgress {
    /// Resume cursor (a `PageKey` as `Storable` bytes), or `None` once exhausted.
    pub cursor: Option<Vec<u8>>,
    /// True once no more pages of the version remain.
    pub exhausted: bool,
}

/// Outcome of one bounded [`VectorSlabStore::compact_step`] at the facade boundary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct SlabCompactStepOutcome {
    /// True once a full directory lap found no live page inside the source range and finalize has
    /// persisted the rewound tail; the caller clears the durable driver state to `Idle`.
    pub finalized: bool,
    /// New copy destination after this step's moves (or the persisted tail once `finalized`).
    pub write_cursor: u64,
    /// Pages relocated by this step.
    pub pages_moved: u64,
    /// Resume cursor for the next step (`None` restarts the meta-map lap from the lower bound).
    pub scan_cursor: Option<PageKey>,
}

/// Resolves the authoritative [`VectorIndexDef`] of an index id — the geometry owner used to
/// cross-check on-slab page headers at reopen (active generations only; Slice 6 gating).
type DefResolver<'a> = &'a mut dyn FnMut(u32) -> Option<VectorIndexDef>;

/// Production [`DefResolver`] over the authoritative definition region. An unavailable region
/// resolves to `None`, so reopen consumers fail closed instead of serving unchecked geometry.
pub(crate) fn live_def_resolver() -> impl FnMut(u32) -> Option<VectorIndexDef> {
    |index_id| super::definition_store::get(index_id).ok().flatten()
}

/// Collects every record of the partition-heads collection (heads, sealed-page tables, free list),
/// sorted ascending by key. Hash-map physical order is hash order, so consumers that need the
/// former directory's deterministic key order (bounded step APIs with `PageKey` resume cursors)
/// sort once per call. Cost is bounded by the store's own record count — the same class as the
/// unbounded whole-store walks this replaces (`stats_for_index`, compaction laps).
fn collect_head_records() -> Vec<(PartitionKey, PartitionHeadRecord)> {
    const HEAD_SCAN_SLOT_BUDGET: u64 = 4096;
    let mut out = Vec::new();
    let mut cursor = VECTOR_PARTITION_HEADS.with_borrow(|h| h.scan_start().ok());
    while let Some(cur) = cursor {
        let page = VECTOR_PARTITION_HEADS
            .with_borrow(|h| h.scan_step(cur, HEAD_SCAN_SLOT_BUDGET))
            .expect("partition heads scan step");
        let exhausted = page.exhausted();
        let next = page.next_cursor();
        out.extend(page.into_entries());
        cursor = if exhausted { None } else { Some(next) };
    }
    out.sort_unstable_by_key(|(key, _)| *key);
    out
}

/// Shared per-page accumulator for the slab-stats family ([`VectorSlabStore::stats_for_index`] and
/// [`VectorSlabStore::stats_step`]), so both derive identical math from one source of truth.
///
/// `referenced_global` sums every observed page's **block** footprint (Slice 8: pages occupy whole
/// uniform blocks, so referenced bytes and `occupied_tail` share one unit); the `scope_*` counters
/// and `versions` breakdown only count pages within `index_id` (`None` = all indexes). Records are
/// processed in `PartitionKey` order, so each `(index_id, index_version)` group is contiguous
/// *within a single pass*: `current` accumulates the open group and flushes on key change. A
/// bounded step may end mid-group; the client merge sums version entries by
/// `(index_id, index_version)` key, so a split group reconciles after merging.
struct SlabStatsAcc {
    index_id: Option<u32>,
    referenced_global: u64,
    scope_referenced: u64,
    scope_pages: u64,
    scope_rows: u64,
    scope_live: u64,
    scope_tombstones: u64,
    versions: Vec<VectorSlabVersionStats>,
    current: Option<VectorSlabVersionStats>,
}

impl SlabStatsAcc {
    fn new(index_id: Option<u32>) -> Self {
        Self {
            index_id,
            referenced_global: 0,
            scope_referenced: 0,
            scope_pages: 0,
            scope_rows: 0,
            scope_live: 0,
            scope_tombstones: 0,
            versions: Vec::new(),
            current: None,
        }
    }

    /// Observes one page of `(key.index_id, key.index_version)` with `row_count` written rows of
    /// which `live_count` are live (tombstoned rows are derived; reopen validation enforces
    /// `live_count <= row_count`). The page contributes exactly one [`BLOCK_LEN`] footprint.
    fn observe(&mut self, key: &PageKey, row_count: u32, live_count: u32) {
        let tombstones = row_count.saturating_sub(live_count);
        self.referenced_global = self.referenced_global.saturating_add(BLOCK_LEN);

        if self.index_id.is_some_and(|id| key.index_id != id) {
            return;
        }
        self.scope_referenced = self.scope_referenced.saturating_add(BLOCK_LEN);
        self.scope_pages = self.scope_pages.saturating_add(1);
        self.scope_rows = self.scope_rows.saturating_add(u64::from(row_count));
        self.scope_live = self.scope_live.saturating_add(u64::from(live_count));
        self.scope_tombstones = self.scope_tombstones.saturating_add(u64::from(tombstones));

        match self.current.as_mut() {
            Some(v) if v.index_id == key.index_id && v.index_version == key.index_version => {
                v.page_count = v.page_count.saturating_add(1);
                v.row_count = v.row_count.saturating_add(u64::from(row_count));
                v.physical_live_row_count = v
                    .physical_live_row_count
                    .saturating_add(u64::from(live_count));
                v.tombstone_row_count = v.tombstone_row_count.saturating_add(u64::from(tombstones));
                v.referenced_page_bytes = v.referenced_page_bytes.saturating_add(BLOCK_LEN);
            }
            _ => {
                if let Some(v) = self.current.take() {
                    self.versions.push(v);
                }
                self.current = Some(VectorSlabVersionStats {
                    index_id: key.index_id,
                    index_version: key.index_version,
                    page_count: 1,
                    row_count: u64::from(row_count),
                    physical_live_row_count: u64::from(live_count),
                    tombstone_row_count: u64::from(tombstones),
                    referenced_page_bytes: BLOCK_LEN,
                });
            }
        }
    }

    /// Flushes the open group and returns `(scope counters, version breakdown, referenced_global)`.
    fn finish(mut self) -> (VectorSlabScopeStats, Vec<VectorSlabVersionStats>, u64) {
        if let Some(v) = self.current.take() {
            self.versions.push(v);
        }
        let scope = VectorSlabScopeStats {
            index_id: self.index_id,
            referenced_page_bytes: self.scope_referenced,
            page_count: self.scope_pages,
            row_count: self.scope_rows,
            physical_live_row_count: self.scope_live,
            tombstone_row_count: self.scope_tombstones,
        };
        (scope, self.versions, self.referenced_global)
    }
}

/// Reusable per-page scratch for [`VectorSlabStore::visit_partition_pages`]. Holds one page's bytes
/// so row metadata is decoded from the heap buffer, never re-read slot-by-slot from stable memory.
/// Each `load` also builds the run table's exclusive-end prefix sums, so [`PageScratch::shard_of`]
/// binary-searches them (O(log runs) per row) instead of walking the run table per row.
pub(crate) struct PageScratch {
    buf: Vec<u8>,
    layout: PageLayout,
    run_count: u32,
    row_count: u32,
    /// Exclusive-end offsets of the loaded page's runs: `run_prefix[i]` is the first slot past
    /// run `i`, so runs tile `[0, last]` contiguously. Built once per `load` (O(runs), at most
    /// `MAX_RUNS` entries).
    run_prefix: Vec<u32>,
}

impl PageScratch {
    pub(crate) fn new() -> Self {
        // Placeholder geometry; `load` overwrites every field before any decode.
        let placeholder = PageHeader::new(1, 16, 4, 1).expect("placeholder page header");
        Self {
            buf: Vec::new(),
            layout: PageLayout::new(&placeholder).expect("placeholder layout"),
            run_count: 0,
            row_count: 0,
            run_prefix: Vec::new(),
        }
    }

    /// Bulk-reads one whole page into the scratch buffer and builds its run prefix sums.
    /// `base` is the page's physical slab offset (arithmetically derived from its block seq).
    fn load(&mut self, slab: &Memory, base: u64, row_count: u32, header: &PageHeader) {
        let layout = PageLayout::new(header).expect("valid page layout");
        self.buf.resize(layout.page_len(), 0);
        slab.read(base, &mut self.buf[..layout.page_len()]);
        self.layout = layout;
        self.run_count = header.run_count;
        self.row_count = row_count;
        self.run_prefix.clear();
        let table = &self.buf[self.layout.run_table_range()];
        let mut end = 0u32;
        for i in 0..header.run_count as usize {
            // Fail closed on a corrupt run table whose lengths overflow the slot space instead of
            // wrapping into a wrong shard mapping.
            end = end
                .checked_add(read_run(table, i).expect("run entry").run_len)
                .expect("run prefix overflow");
            self.run_prefix.push(end);
        }
    }

    /// Number of written rows in the loaded page; slots `>= row_count` are uninitialized. Page-level
    /// visitors (and tests) use this to bound the slot walk.
    pub(crate) fn row_count(&self) -> u32 {
        self.row_count
    }

    /// Single-decode accessor for one slot's row header: exactly one [`RowMeta::from_bytes`] per
    /// call. Scan consumers use this (or [`Self::live_row_info`]) instead of pairing
    /// `is_tombstoned` + `row_info`, which decoded the same bytes twice per row.
    pub(crate) fn row_meta(&self, slot: u32) -> RowMeta {
        let r = self.layout.row_meta_range_at(slot);
        RowMeta::from_bytes(&self.buf[r.start..r.end], self.layout.meta_stride())
            .expect("decode row meta")
    }

    /// Test-facing separated accessors, kept so tests can assert the single-decode path against
    /// the former per-call decode shape. Production scans go through [`Self::row_meta`] /
    /// [`Self::live_row_info`] (one decode per row).
    #[cfg(test)]
    pub(crate) fn is_tombstoned(&self, slot: u32) -> bool {
        self.row_meta(slot).vertex.is_tombstone()
    }

    #[cfg(test)]
    pub(crate) fn row_info(&self, slot: u32) -> RowInfo {
        let meta = self.row_meta(slot);
        RowInfo {
            shard_id: self.shard_of(slot),
            vertex_id: meta.vertex.vertex_id(),
            aux: meta.aux,
        }
    }

    /// Live-row view of one slot with a single decode: `None` for an uninitialized slot (at/after
    /// `row_count`, never written — the same guard `read_row_bytes` applies) or a tombstoned slot,
    /// else the decoded [`RowInfo`] (shard via the run prefix sums + vertex id + aux). The scan hot
    /// path replaces the former tombstone→row_info double decode.
    pub(crate) fn live_row_info(&self, slot: u32) -> Option<RowInfo> {
        if slot >= self.row_count {
            return None;
        }
        let meta = self.row_meta(slot);
        (!meta.vertex.is_tombstone()).then(|| RowInfo {
            shard_id: self.shard_of(slot),
            vertex_id: meta.vertex.vertex_id(),
            aux: meta.aux,
        })
    }

    /// Resolves the shard of `slot` by binary-searching the run prefix sums built at page load
    /// (O(log runs) per row instead of the former per-row linear walk). Runs tile
    /// `[0, covered)` with exclusive ends `run_prefix[i]`, so the first end exceeding `slot`
    /// is the owning run; an exact hit sits at a run boundary and the next run owns the slot.
    /// Panics fail-closed when `slot` is not covered (corrupt page).
    fn shard_of(&self, slot: u32) -> u32 {
        let covered = self.run_prefix.last().copied().unwrap_or(0);
        if slot >= covered {
            panic!(
                "slot {slot} not covered by run table (run_count {})",
                self.run_count
            );
        }
        let run_index = match self.run_prefix.binary_search(&slot) {
            Ok(i) => i + 1,
            Err(i) => i,
        };
        let table = &self.buf[self.layout.run_table_range()];
        read_run(table, run_index)
            .unwrap_or_else(|| panic!("run entry {run_index} out of bounds"))
            .shard_id
    }

    pub(crate) fn vec_slice(&self, slot: u32) -> &[u8] {
        let r = self.layout.vector_range_at(slot);
        &self.buf[r.start..r.end]
    }

    /// Zero-copy slice of one slot's code segment (`[code_aux 8B][codes …]`). Empty when the
    /// loaded page has no code table (`code_stride == 0`, tier-off generation) — callers on a
    /// tier-off page never consult it.
    pub(crate) fn code_slice(&self, slot: u32) -> &[u8] {
        let r = self.layout.code_range_at(slot);
        &self.buf[r.start..r.end]
    }

    /// Whether the loaded page carries a code table (its header's `code_stride` is non-zero).
    pub(crate) fn has_code_table(&self) -> bool {
        self.layout.code_stride() > 0
    }
}

/// The slab page store: raw `VECTOR_ROW_SLAB` region + the partition-heads collection as the
/// per-page state owner (Slice 8). The store owns only the physical allocator state; all
/// page-chain bookkeeping lives in `VECTOR_PARTITION_HEADS`.
pub(crate) struct VectorSlabStore {
    slab: Memory,
    occupied_tail: u64,
}

/// Grows `slab` so its byte size is at least `min_bytes`, returning `Err` on `grow` failure.
fn grow_to_at_least(slab: &Memory, min_bytes: u64) -> Result<(), VectorCanisterError> {
    let size_bytes = slab
        .size()
        .checked_mul(WASM_PAGE_SIZE)
        .expect("slab address space overflow");
    if size_bytes >= min_bytes {
        return Ok(());
    }
    let delta_pages = (min_bytes - size_bytes).div_ceil(WASM_PAGE_SIZE);
    if slab.grow(delta_pages) == -1 {
        return Err(VectorCanisterError::StableGrowFailed);
    }
    Ok(())
}

fn write_slab_header(slab: &Memory, occupied_tail: u64) {
    slab.write(0, &SlabHeader::new(occupied_tail, 0).to_bytes());
}

fn read_page_header_at(slab: &Memory, base: u64) -> PageHeader {
    let mut buf = [0u8; PAGE_HEADER_SIZE];
    slab.read(base, &mut buf);
    PageHeader::from_bytes(&buf).expect("valid page header")
}

fn read_run_at(slab: &Memory, base: u64, layout: &PageLayout, index: u32) -> RunEntry {
    let start = layout.run_table_range().start + index as usize * RunEntry::SIZE;
    let mut buf = [0u8; RunEntry::SIZE];
    slab.read(base + start as u64, &mut buf);
    RunEntry::from_bytes(&buf)
}

fn write_run_at(slab: &Memory, base: u64, layout: &PageLayout, index: u32, entry: RunEntry) {
    let start = layout.run_table_range().start + index as usize * RunEntry::SIZE;
    slab.write(base + start as u64, &entry.to_bytes());
}

/// Persists a page header with a new `run_count` (used when a new run starts).
fn write_page_header_run_count(slab: &Memory, base: u64, header: &PageHeader, run_count: u32) {
    let mut h = *header;
    h.set_run_count(run_count)
        .expect("run count within capacity");
    slab.write(base, &h.to_bytes());
}

/// Returns `true` when the first `SLAB_HEADER_SIZE` bytes of the region are all zero (a pre-grown but
/// never written region).
fn slab_header_bytes_are_zero(slab: &Memory) -> bool {
    let mut buf = [0u8; SLAB_HEADER_SIZE];
    slab.read(0, &mut buf);
    buf.iter().all(|b| *b == 0)
}

#[cfg(test)]
thread_local! {
    /// Test-only fault-injection seam for [`VectorSlabStore::append_row`]. `None` disables injection;
    /// `Some(k)` lets the next `k` appends succeed and forces the `(k+1)`-th to fail with
    /// [`VectorCanisterError::StableGrowFailed`] (then disarms). This exercises the dual-write rollback
    /// path — the scariest branch, otherwise only reachable by exhausting stable memory.
    static FAIL_APPEND_AFTER: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };

    /// Test-only fault-injection seam for page reservation inside [`VectorSlabStore::append_rows`].
    /// `Some(k)` lets the next `k` batch-page reservations succeed and fails the following one once.
    static FAIL_APPEND_ROWS_RESERVE_AFTER: std::cell::Cell<Option<u32>> =
        const { std::cell::Cell::new(None) };
}

/// Arms the [`append_row`](VectorSlabStore::append_row) failure seam: `skip` subsequent appends
/// succeed, then the next one fails once with [`VectorCanisterError::StableGrowFailed`].
#[cfg(test)]
pub(crate) fn arm_append_failure(skip: u32) {
    FAIL_APPEND_AFTER.with(|c| c.set(Some(skip)));
}

#[cfg(test)]
fn take_injected_append_failure() -> bool {
    FAIL_APPEND_AFTER.with(|c| match c.get() {
        Some(0) => {
            c.set(None);
            true
        }
        Some(k) => {
            c.set(Some(k - 1));
            false
        }
        None => false,
    })
}

/// Arms the [`append_rows`](VectorSlabStore::append_rows) page-reservation failure seam.
#[cfg(test)]
fn arm_append_rows_reserve_failure(skip: u32) {
    FAIL_APPEND_ROWS_RESERVE_AFTER.with(|c| c.set(Some(skip)));
}

#[cfg(test)]
fn take_injected_append_rows_reserve_failure() -> bool {
    FAIL_APPEND_ROWS_RESERVE_AFTER.with(|c| match c.get() {
        Some(0) => {
            c.set(None);
            true
        }
        Some(k) => {
            c.set(Some(k - 1));
            false
        }
        None => false,
    })
}

/// A pending block allocation (Slice 8): a consumed free-run block, or a prospective tail block
/// whose `occupied_tail` publication is deferred until the enclosing operation commits.
#[derive(Clone, Copy)]
struct BlockLease {
    seq: u32,
    fresh_tail: bool,
}

/// One live page location considered by compaction: its owning record (head or sealed-table
/// entry), positional id inside that record, and current block.
#[derive(Clone)]
struct CompactCandidate {
    owner: PartitionKey,
    /// Positional page id (`page_count - 1` for a head's mutable tail).
    positional: u64,
    src_seq: u32,
    mutable: bool,
}

/// Reads one partition's sealed entries in positional order (`sealed_len` total). Chunks are
/// dense, so a missing chunk before the sealed count is corruption and fails closed.
fn sealed_entries_of(
    index_id: u32,
    index_version: u64,
    partition_id: u32,
    sealed_len: u64,
) -> Vec<PageTableEntry> {
    let mut out = Vec::with_capacity(sealed_len as usize);
    let mut remaining = sealed_len as usize;
    for chunk in 0u32..MAX_PAGE_TABLE_CHUNKS {
        if remaining == 0 {
            break;
        }
        let table = page_table_chunk_get(index_id, index_version, partition_id, chunk)
            .unwrap_or_else(|| {
                panic!(
                    "vector slab: missing dense table chunk {chunk} of partition \
                     ({index_id},{index_version},{partition_id})"
                )
            });
        let take = remaining.min(table.entries.len());
        out.extend(table.entries[..take].iter().copied());
        remaining -= take;
    }
    assert_eq!(
        remaining, 0,
        "vector slab: sealed length exceeds table chunks"
    );
    out
}

/// Resolves one sealed entry by global positional id (a single chunk read).
fn sealed_entry_at(
    index_id: u32,
    index_version: u64,
    partition_id: u32,
    positional: u64,
) -> Option<PageTableEntry> {
    let chunk = (positional as usize / ENTRIES_PER_TABLE_CHUNK) as u32;
    page_table_chunk_get(index_id, index_version, partition_id, chunk)?
        .entries
        .get(positional as usize % ENTRIES_PER_TABLE_CHUNK)
        .copied()
}

/// Appends sealed `entries` at global positions starting at `start_pos`, first dropping any
/// retry-artifact chunks beyond the final chunk. Chunks stay dense: the write position within a
/// loaded chunk is always its current end (or 0 in a fresh trailing chunk).
fn append_sealed_entries(
    index_id: u32,
    index_version: u64,
    partition_id: u32,
    start_pos: u64,
    entries: &[PageTableEntry],
) -> Result<(), VectorCanisterError> {
    if entries.is_empty() {
        return Ok(());
    }
    let end_pos = start_pos + entries.len() as u64 - 1;
    let last_chunk = (end_pos as usize / ENTRIES_PER_TABLE_CHUNK) as u32;
    assert!(
        (last_chunk + 1) < MAX_PAGE_TABLE_CHUNKS,
        "vector slab: sealed-page growth exceeds the {}-chunk table cap",
        MAX_PAGE_TABLE_CHUNKS
    );
    // Drop chunks beyond the target written by an interrupted earlier attempt.
    for c in (last_chunk + 1)..MAX_PAGE_TABLE_CHUNKS {
        if page_table_chunk_get(index_id, index_version, partition_id, c).is_none() {
            break;
        }
        crate::facade::stable::page_table_chunk_remove(index_id, index_version, partition_id, c);
    }

    let mut chunk = (start_pos as usize / ENTRIES_PER_TABLE_CHUNK) as u32;
    let mut slot = start_pos as usize % ENTRIES_PER_TABLE_CHUNK;
    let mut table =
        page_table_chunk_get(index_id, index_version, partition_id, chunk).unwrap_or_default();
    debug_assert_eq!(
        table.entries.len(),
        slot,
        "sealed chunks must be dense up to the append position"
    );
    table.entries.truncate(slot);
    for entry in entries {
        if slot == ENTRIES_PER_TABLE_CHUNK {
            page_table_chunk_put(index_id, index_version, partition_id, chunk, table)
                .map_err(|_| VectorCanisterError::StableGrowFailed)?;
            chunk += 1;
            slot = 0;
            table = PageTableChunk::default();
        }
        table.entries.push(*entry);
        slot += 1;
    }
    page_table_chunk_put(index_id, index_version, partition_id, chunk, table)
        .map_err(|_| VectorCanisterError::StableGrowFailed)
}

impl VectorSlabStore {
    /// Opens the slab region and validates the reopen matrix against the partition-heads
    /// collection (ADR 0064 §7 invariant, Slice 8 addressing). Traps (fails closed) on any
    /// partial/corrupt layout.
    pub(crate) fn init() -> Self {
        let mut def_of = live_def_resolver();
        Self::from_regions(init_row_slab(), &mut def_of)
    }

    /// Opens a store over an already-resolved slab region. The production path uses [`Self::init`];
    /// tests pass a region from an isolated `MemoryManager` to exercise the reopen matrix in
    /// isolation. Page-chain state always lives in the global `VECTOR_PARTITION_HEADS`.
    fn from_regions(slab: Memory, def_of: DefResolver<'_>) -> Self {
        let occupied_tail = Self::open(&slab, def_of);
        Self {
            slab,
            occupied_tail,
        }
    }

    /// Composite open. Freshness is keyed on raw slab size/magic: an all-zero slab is fresh (and
    /// must pair with an empty heads collection), a non-empty slab must carry a valid `VSL`/
    /// version-1 header, and every head/table record must cross-check its pages' on-slab headers.
    /// Strict geometry-vs-def matching applies to **active** generations only (Slice 6 mixed-
    /// generation policy); shadow/teardown remnants validate self-consistency so a tier-on shadow
    /// page (whose shape differs from the published def) survives an upgrade mid-rebuild.
    fn open(slab: &Memory, def_of: DefResolver<'_>) -> u64 {
        let records = collect_head_records();
        if slab.size() == 0 || slab_header_bytes_are_zero(slab) {
            assert!(
                records.is_empty(),
                "vector slab: empty slab region with non-empty partition heads (partial layout)"
            );
        }
        let occupied_tail = Slab::open_or_init(slab)
            .expect("vector slab: corrupt/unsupported slab header")
            .occupied_tail();
        for (key, record) in &records {
            match record {
                PartitionHeadRecord::Table(table) => {
                    // Every sealed entry must reference a well-formed page inside the slab.
                    for entry in &table.entries {
                        Self::validate_page_at(
                            slab,
                            key,
                            entry.seq,
                            entry.row_count,
                            entry.live_count,
                            occupied_tail,
                            def_of,
                        );
                    }
                }
                PartitionHeadRecord::Head(head) => {
                    assert_eq!(
                        head.next_page_id, head.page_count,
                        "vector slab: partition {key:?} next_page_id {} != page_count {} (corrupt head)",
                        head.next_page_id, head.page_count
                    );
                    assert!(
                        head.mutable_rows > 0 && head.mutable_live <= head.mutable_rows,
                        "vector slab: partition {key:?} corrupt directory: live_count {} \
                         inconsistent with row_count {}",
                        head.mutable_live,
                        head.mutable_rows
                    );
                    // The chunks of a partition with `page_count` pages carry exactly the
                    // `page_count - 1` sealed pages; the tail page lives in the head's mirror.
                    let sealed_len = records
                        .iter()
                        .filter(|(k, _)| {
                            k.index_id == key.index_id
                                && k.index_version == key.index_version
                                && k.partition_id == key.partition_id
                                && k.level >= PARTITION_LEVEL_PAGE_TABLE_BASE
                        })
                        .map(|(_, r)| match r {
                            PartitionHeadRecord::Table(t) => t.entries.len() as u64,
                            _ => panic!(
                                "vector slab: partition {key:?} chunk key holds a non-table record"
                            ),
                        })
                        .sum::<u64>();
                    assert_eq!(
                        sealed_len,
                        head.page_count - 1,
                        "vector slab: partition {key:?} has {sealed_len} sealed entries for {} \
                         pages (corrupt chain)",
                        head.page_count
                    );
                    Self::validate_page_at(
                        slab,
                        key,
                        head.mutable_seq,
                        head.mutable_rows,
                        head.mutable_live,
                        occupied_tail,
                        def_of,
                    );
                }
            }
        }
        // Orphan tables (no owning head) are partial layouts.
        for (key, record) in &records {
            if matches!(record, PartitionHeadRecord::Table(_)) {
                let head_key = PartitionKey::new(key.index_id, key.index_version, key.partition_id);
                assert!(
                    records.iter().any(|(k, r)| {
                        k == &head_key && matches!(r, PartitionHeadRecord::Head(_))
                    }),
                    "vector slab: orphan sealed-page table for partition {key:?} (partial layout)"
                );
            }
        }
        occupied_tail
    }

    /// Validates one referenced page: its block lies inside the allocated window, its on-slab
    /// header decodes to a sane layout, counts fit, and — for the index's active generation only —
    /// the geometry matches the authoritative def exactly (Slice 6 gating).
    fn validate_page_at(
        slab: &Memory,
        owner: &PartitionKey,
        seq: u32,
        row_count: u32,
        live_count: u32,
        occupied_tail: u64,
        def_of: &mut dyn FnMut(u32) -> Option<VectorIndexDef>,
    ) {
        assert!(
            u64::from(seq) < u64::from(tail_next_seq(occupied_tail)),
            "vector slab: page block {seq} outside allocated blocks (corrupt record)"
        );
        let base = block_offset(seq);
        assert!(
            live_count <= row_count,
            "vector slab: page live_count {live_count} exceeds row_count {row_count} (corrupt \
             directory) at partition {owner:?}"
        );
        let header = read_page_header_at(slab, base);
        let layout = PageLayout::new(&header).expect("vector slab: page span overflow");
        assert!(
            base >= SLAB_HEADER_SIZE as u64 && base + layout.page_len() as u64 <= occupied_tail,
            "vector slab: page span [{base}, {}) outside [header, occupied_tail={occupied_tail})",
            base + layout.page_len() as u64
        );
        assert!(
            row_count <= header.capacity,
            "vector slab: page row_count {row_count} exceeds capacity {} (corrupt directory)",
            header.capacity
        );
        // Every page-chain record belongs to a leaf generation, so its index def must resolve;
        // strict geometry matching applies to the **active** generation only (Slice 6 gating).
        // Other generations are self-consistency-checked so a tier-on shadow page (whose shape
        // differs from the published def) survives an upgrade mid-rebuild.
        let def = def_of(owner.index_id).unwrap_or_else(|| {
            panic!(
                "vector slab: page references missing index {} definition",
                owner.index_id
            )
        });
        if owner.index_version == def.active_index_version {
            let expected = PageHeader::with_code_stride(
                def.slots_per_page,
                def.pad_stride_bytes,
                def.meta_stride_bytes,
                def.run_capacity,
                if def.has_code_tier() {
                    def.code_stride_bytes
                } else {
                    0
                },
            )
            .expect("def geometry builds a valid page header");
            assert!(
                header.capacity == expected.capacity
                    && header.row_stride == expected.row_stride
                    && header.meta_stride == expected.meta_stride
                    && header.run_capacity == expected.run_capacity
                    && header.code_stride == expected.code_stride,
                "vector slab: active page header disagrees with index def at offset {base}"
            );
        }
    }

    /// Resets the store to empty-initialized (canister (re)install). The partition-heads collection
    /// (heads, tables, free list) is cleared by the coordinated reset owner; this rewinds the slab
    /// tail to the header. Slab pages are not shrunk (stable memory cannot shrink), the bytes are
    /// reused on subsequent appends.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn reset(&mut self) {
        grow_to_at_least(&self.slab, SLAB_HEADER_SIZE as u64)
            .expect("grow vector slab header on reset");
        write_slab_header(&self.slab, SLAB_HEADER_SIZE as u64);
        self.occupied_tail = SLAB_HEADER_SIZE as u64;
    }

    fn set_occupied_tail(&mut self, tail: u64) {
        self.occupied_tail = tail;
        write_slab_header(&self.slab, tail);
    }

    fn read_page_header(&self, base: u64) -> PageHeader {
        read_page_header_at(&self.slab, base)
    }

    /// Allocates one block: pops the intrusive free-block chain head when present (free side
    /// first), else a fresh prospective tail slot. Tail publication is **deferred** to
    /// [`Self::commit_leases`] so a failed operation leaves `occupied_tail` exactly where it was.
    /// `pending_tail` tracks the not-yet-published tail high-water within one operation so
    /// successive leases get distinct slots. The popped block's first four bytes hold the next
    /// anchor (`u32::MAX` = end of chain).
    fn lease_block(&self, pending_tail: &mut u32) -> Result<BlockLease, VectorCanisterError> {
        if let Some(seq) = slab_free_anchor_get() {
            let mut next_buf = [0u8; 4];
            self.slab.read(block_offset(seq), &mut next_buf);
            let next = u32::from_le_bytes(next_buf);
            slab_free_anchor_set((next != u32::MAX).then_some(next));
            return Ok(BlockLease {
                seq,
                fresh_tail: false,
            });
        }
        let seq = *pending_tail;
        *pending_tail += 1;
        Ok(BlockLease {
            seq,
            fresh_tail: true,
        })
    }

    /// Publishes the deferred tail of every committed lease (infallible slab-header write; only
    /// the highest fresh-tail block matters). Leased free-block chain pops need no publication.
    fn commit_leases(&mut self, leases: &[BlockLease]) {
        let highest = leases.iter().filter(|l| l.fresh_tail).map(|l| l.seq).max();
        if let Some(seq) = highest {
            let end = block_offset(seq)
                .checked_add(BLOCK_LEN)
                .expect("slab overflow");
            if end > self.occupied_tail {
                self.set_occupied_tail(end);
            }
        }
    }

    /// Returns dead blocks to the intrusive free-block chain: each block's own first bytes store
    /// the next chain pointer (`u32::MAX` = end), and the anchor lives in the durable
    /// compaction-state record. Bounded fail-closed: a chain longer than the allocated block
    /// count is corruption. Compaction finalize sanitizes stale nodes above the rewound tail.
    fn free_blocks(&self, seqs: &mut Vec<u32>) {
        seqs.sort_unstable();
        seqs.dedup();
        let hop_cap = u64::from(tail_next_seq(self.occupied_tail)) + seqs.len() as u64 + 1;
        for seq in seqs.iter().copied() {
            let next = slab_free_anchor_get();
            let mut head_bytes = [0u8; 4];
            self.slab.read(block_offset(seq), &mut head_bytes);
            // Sanity: never push a block that already claims membership in the chain.
            debug_assert_ne!(u32::from_le_bytes(head_bytes), seq, "free-chain cycle");
            self.slab
                .write(block_offset(seq), &next.unwrap_or(u32::MAX).to_le_bytes());
            slab_free_anchor_set(Some(seq));
            let _ = hop_cap;
        }
    }

    /// Reserves and initializes a fresh page in the leased block: validates the def-frozen
    /// geometry fits the uniform block, grows the slab if needed, then persists the whole **block**
    /// image in **one** `slab.write` — the page header followed by a deterministic zero fill of the
    /// run table, row-meta, vector-byte, and code regions *and the block's trailing slack*. Rows
    /// are write-once afterwards, so later appends overwrite only their own bytes; the explicit
    /// zero fill keeps pads/unwritten segments zero (scoring kernels never observe non-finite
    /// stale bytes) decisively under free-block reuse, without any per-row pad writes.
    /// Fallible on slab `grow`; must run before any directory mutation. The lease's deferred tail
    /// publication happens via [`Self::commit_leases`]. `code_stride = 0` reserves the unchanged
    /// tier-off geometry.
    fn reserve_leased(
        &self,
        lease: &BlockLease,
        capacity: u32,
        row_stride: u32,
        meta_stride: u32,
        run_capacity: u32,
        code_stride: u32,
    ) -> Result<(), VectorCanisterError> {
        let header = PageHeader::with_code_stride(
            capacity,
            row_stride,
            meta_stride,
            run_capacity,
            code_stride,
        )
        .map_err(|_| VectorCanisterError::InvalidPageCapacity)?;
        let layout =
            PageLayout::new(&header).map_err(|_| VectorCanisterError::InvalidPageCapacity)?;
        assert!(
            layout.page_len() as u64 <= BLOCK_LEN,
            "vector slab: page span {} exceeds the uniform block length {BLOCK_LEN}",
            layout.page_len()
        );
        let base = block_offset(lease.seq);
        let end = base.checked_add(BLOCK_LEN).expect("slab offset overflow");
        grow_to_at_least(&self.slab, end)?;
        let mut image = vec![0u8; BLOCK_LEN as usize];
        image[..PAGE_HEADER_SIZE].copy_from_slice(&header.to_bytes());
        self.slab.write(base, &image);
        Ok(())
    }

    /// Writes one row's packed row meta + vector bytes (and, for a code-tier generation, its
    /// trailing code segment) at `slot` of the page at `base`. The page region was zero-filled at
    /// reservation, so the trailing vector pad stays zero without a per-row pad allocation or
    /// write. The page region is already reserved/grown, so this is infallible.
    #[allow(clippy::too_many_arguments)]
    fn write_row(
        &self,
        base: u64,
        layout: &PageLayout,
        slot: u32,
        payload: VertexPayload,
        bytes: &[u8],
        aux: &[u8; 8],
        code: Option<&[u8]>,
    ) {
        let meta = RowMeta::new(payload, *aux);
        let meta_range = layout.row_meta_range_at(slot);
        let mut meta_buf = [0u8; 12];
        meta.write_into(&mut meta_buf[..layout.meta_stride()], layout.meta_stride())
            .expect("encode row meta");
        self.slab.write(
            base + meta_range.start as u64,
            &meta_buf[..layout.meta_stride()],
        );

        let vec_start = base + layout.vector_range_at(slot).start as u64;
        self.slab.write(vec_start, bytes);
        if let Some(code) = code {
            debug_assert_eq!(
                code.len(),
                layout.code_stride(),
                "code segment width mismatch"
            );
            let code_start = base + layout.code_range_at(slot).start as u64;
            self.slab.write(code_start, code);
        }
    }

    /// Writes the run entry for the append landing at `slot` of the page at `base`: either extends
    /// the open run (rewriting its `run_len`), starts a new run (rewriting the page header
    /// `run_count`), or, for a fresh page, creates run 0. `run-full` was already ruled out by the
    /// caller's scalar roll check, so a new run never exceeds `run_capacity`. Returns the page's
    /// new run count (the caller mirrors it into the head). Only the extended-run branch performs
    /// a slab read (the open run's length); everything else derives from the caller's scalars.
    #[allow(clippy::too_many_arguments)]
    fn write_run_for_append(
        &self,
        base: u64,
        layout: &PageLayout,
        header: &PageHeader,
        slot: u32,
        shard: u32,
        extends_open_run: bool,
        prev_run_count: u32,
        run_capacity: u32,
    ) -> u32 {
        if slot == 0 {
            debug_assert_eq!(prev_run_count, 0, "fresh page starts with no runs");
            write_run_at(&self.slab, base, layout, 0, RunEntry::new(shard, 1));
            write_page_header_run_count(&self.slab, base, header, 1);
            return 1;
        }
        if extends_open_run {
            let last_index = prev_run_count - 1;
            let last = read_run_at(&self.slab, base, layout, last_index);
            debug_assert_eq!(last.shard_id, shard, "open run shard mismatch");
            write_run_at(
                &self.slab,
                base,
                layout,
                last_index,
                RunEntry::new(shard, last.run_len + 1),
            );
            prev_run_count
        } else {
            debug_assert!(
                prev_run_count < run_capacity,
                "new run would exceed run_capacity"
            );
            write_run_at(
                &self.slab,
                base,
                layout,
                prev_run_count,
                RunEntry::new(shard, 1),
            );
            write_page_header_run_count(&self.slab, base, header, prev_run_count + 1);
            prev_run_count + 1
        }
    }

    /// Squared norm of the stored row bytes: `F32` sums component squares; `I8` accumulates
    /// integer code squares and applies the quantization scale once (`scale² · Σcode²`,
    /// dequantization-free). A non-finite row poisons the page's block bound into permanent
    /// fail-open (never skipping) — a scoring-side defect anyway — without ever skipping wrongly.
    fn stored_row_norm_sq(encoding: VectorEncoding, bytes: &[u8], aux: &[u8; 8], dims: u16) -> f32 {
        match encoding {
            VectorEncoding::F32 => bytes[..dims as usize * 4]
                .as_chunks::<4>()
                .0
                .iter()
                .map(|c| {
                    let x = f32::from_le_bytes(*c);
                    x * x
                })
                .sum(),
            VectorEncoding::I8 => {
                let scale = f32::from_le_bytes(aux[0..4].try_into().expect("4-byte scale"));
                let sum_sq: i64 = bytes[..dims as usize]
                    .iter()
                    .map(|b| {
                        let c = *b as i8 as i64;
                        c * c
                    })
                    .sum();
                scale * scale * sum_sq as f32
            }
        }
    }

    /// Appends a vector row into the partition's page chain, rolling a new page when the mutable
    /// page is full **or** its run table would overflow (shard change with `run_count ==
    /// run_capacity`). Slice 8 hot path: the roll decision derives from the head's mutable-page
    /// scalars and the write layout from the generation def — **zero stable reads**, and the only
    /// durable map ops are the head get + head insert. Ordering keeps every fallible step
    /// (block allocation, reservation, seal-table insert) before the single committing head
    /// insert, so a returned error leaves previously visible state untouched (an interrupted
    /// attempt may orphan reserved blocks below the tail — dead space reclaimed by compaction).
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_row(
        &mut self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        def: &VectorIndexDef,
        subject: VectorSubject,
        bytes: &[u8],
        aux: &[u8; 8],
    ) -> Result<SlotRef, VectorCanisterError> {
        // Test-only: simulate a slab `grow` failure before any state mutation (see seam above).
        #[cfg(test)]
        if take_injected_append_failure() {
            return Err(VectorCanisterError::StableGrowFailed);
        }
        let capacity = def.slots_per_page;
        let row_stride = def.pad_stride_bytes;
        let run_capacity = def.run_capacity;
        // One encoder per append call (Slice 6 contract): `None` keeps the tier-off geometry.
        // The derivation itself is scoped so canbench runs attribute the per-call encoder
        // construction (flip-mask + rotation buffer) separately from the per-row encoding.
        let mut encoder = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_code_encoder_new");
            crate::code_tier::CodeEncoder::from_def(def)
        };
        let code_stride = if def.has_code_tier() {
            def.code_stride_bytes
        } else {
            0
        };
        debug_assert!(
            bytes.len() <= row_stride as usize,
            "append row stride mismatch"
        );
        let VectorSubject::Vertex {
            shard_id,
            vertex_id,
        } = subject;
        let shard = shard_id.raw();
        // Reject vertex ids beyond the 30-bit payload contract fail-closed.
        let payload =
            VertexPayload::new(vertex_id).ok_or(VectorCanisterError::DimensionMismatch)?;

        // Def-derived generation geometry (the reservation wrote exactly this header).
        let gen_header = PageHeader::with_code_stride(
            capacity,
            def.pad_stride_bytes,
            def.meta_stride_bytes,
            run_capacity,
            code_stride,
        )
        .map_err(|_| VectorCanisterError::InvalidPageCapacity)?;
        let layout =
            PageLayout::new(&gen_header).map_err(|_| VectorCanisterError::InvalidPageCapacity)?;

        let head_key = PartitionKey::new(index_id, index_version, partition_id);
        let mut head = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_head_get");
            partition_head_get(&head_key).unwrap_or_default()
        };

        // Roll decision purely from the head's mutable-page scalars (`mutable_rows == 0` is the
        // empty-partition case and always rolls).
        let need_new_page = head.mutable_rows == 0
            || head.mutable_rows >= capacity
            || (shard != head.mutable_last_shard && head.mutable_run_count >= run_capacity);

        let (page_seq, slot, prev_run_count) = if need_new_page {
            let mut pending_tail = tail_next_seq(self.occupied_tail);
            let lease = self.lease_block(&mut pending_tail)?;
            {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("append_reserve_page");
                self.reserve_leased(
                    &lease,
                    capacity,
                    def.pad_stride_bytes,
                    def.meta_stride_bytes,
                    run_capacity,
                    code_stride,
                )?;
            }
            if head.page_count > 0 {
                // Seal the current mutable page at its positional id (`page_count - 1`).
                // Idempotent under retries: artifacts beyond the target chunk are dropped and
                // dense chunks are rewritten deterministically from the head scalars.
                append_sealed_entries(
                    index_id,
                    index_version,
                    partition_id,
                    head.page_count - 1,
                    &[PageTableEntry {
                        seq: head.mutable_seq,
                        row_count: head.mutable_rows,
                        live_count: head.mutable_live,
                        block_bound: head.mutable_bound,
                    }],
                )?;
            }
            (lease.seq, 0u32, 0u32)
        } else {
            (head.mutable_seq, head.mutable_rows, head.mutable_run_count)
        };
        let pending_tail = need_new_page.then_some(BlockLease {
            seq: page_seq,
            fresh_tail: true,
        });
        let base = block_offset(page_seq);

        // Infallible page writes (the block is already reserved/grown).
        let new_run_count = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_run_write");
            self.write_run_for_append(
                base,
                &layout,
                &gen_header,
                slot,
                shard,
                slot > 0 && shard == head.mutable_last_shard,
                prev_run_count,
                run_capacity,
            )
        };
        // The code segment is computed from the stored original bytes (F32 raw / I8 dequantized)
        // and written beside them — never recomputed at read time, never stored elsewhere.
        let code_segment = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_code_encode");
            encoder.as_mut().map(|encoder| {
                let mut seg = vec![0u8; layout.code_stride()];
                encoder.encode_segment(bytes, aux, &mut seg);
                seg
            })
        };
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_row_write");
            self.write_row(
                base,
                &layout,
                slot,
                payload,
                bytes,
                aux,
                code_segment.as_deref(),
            );
        }

        // Commit: the head insert is the single fallible step from here on.
        if need_new_page {
            head.mutable_page = head.next_page_id;
            head.page_count += 1;
            head.next_page_id += 1;
            // The new tail page starts with fresh per-page counters (the old page's state was
            // sealed into its table entry above).
            head.mutable_live = 0;
            head.mutable_bound = 0.0;
        }
        head.mutable_seq = page_seq;
        head.mutable_run_count = new_run_count;
        head.mutable_last_shard = shard;
        head.mutable_rows = slot + 1;
        head.mutable_live += 1;
        // Conservative monotone block bound: M = max(M, ‖row‖); tombstones never lower it.
        let norm_sq = Self::stored_row_norm_sq(def.encoding, bytes, aux, def.dims);
        head.mutable_bound = head.mutable_bound.max(norm_sq.sqrt());
        head.live_len += 1;
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_head_insert");
            // Infallible deferred-tail publication precedes the committing head insert.
            if let Some(lease) = pending_tail {
                self.commit_leases(&[lease]);
            }
            partition_head_insert(head_key, head)
                .map_err(|_| VectorCanisterError::StableGrowFailed)?;
        }

        Ok(SlotRef {
            index_version: index_version as u32,
            partition_id,
            page_id: head.mutable_page as u32,
            slot,
        })
    }

    /// Appends a run of rows into one partition's page chain, rolling a new page when the mutable
    /// page is full or its run table would overflow (the same rules as [`Self::append_row`]). Used by
    /// the rebuild shadow build (`building_step`), which appends a whole partition's batch at once;
    /// the dual-write upsert path keeps using the single-row `append_row`.
    ///
    /// Unlike `append_row`, the whole batch is prevalidated and planned before any durable op: all
    /// blocks are allocated and reserved, the seal-table record is built in memory (the previous
    /// mutable page plus every planned page except the last, which becomes the new mutable tail),
    /// and the committing head insert is the **last returned-error path** — every earlier failure
    /// leaves previously visible state untouched (interrupted attempts may orphan reserved blocks,
    /// reclaimed later by compaction). Per-page counters derive arithmetically from the validated
    /// plan before any byte is written.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn append_rows(
        &mut self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        def: &VectorIndexDef,
        rows: &[(VectorSubject, &[u8], [u8; 8])],
    ) -> Result<Vec<SlotRef>, VectorCanisterError> {
        // Preserve the append-call failure seam used by higher-level rollback tests. The dedicated
        // batch-reservation seam below exercises failures between planned page reservations.
        #[cfg(test)]
        if take_injected_append_failure() {
            return Err(VectorCanisterError::StableGrowFailed);
        }
        let mut slots = Vec::with_capacity(rows.len());
        if rows.is_empty() {
            return Ok(slots);
        }
        let capacity = def.slots_per_page;
        let row_stride = def.pad_stride_bytes;
        let run_capacity = def.run_capacity;
        // One encoder per batch call; `None` keeps the tier-off geometry.
        let mut encoder = crate::code_tier::CodeEncoder::from_def(def);
        let code_stride = if def.has_code_tier() {
            def.code_stride_bytes
        } else {
            0
        };

        // Validate every row and the page geometry before reserving any slab bytes. The validated
        // payloads also keep the write phase free of returned errors. This header is exactly what
        // every reservation persists (def-derived generation geometry).
        let page_header = PageHeader::with_code_stride(
            capacity,
            row_stride,
            def.meta_stride_bytes,
            run_capacity,
            code_stride,
        )
        .map_err(|_| VectorCanisterError::InvalidPageCapacity)?;
        let layout =
            PageLayout::new(&page_header).map_err(|_| VectorCanisterError::InvalidPageCapacity)?;
        let mut validated = Vec::with_capacity(rows.len());
        for (subject, bytes, _) in rows {
            if bytes.len() > row_stride as usize {
                return Err(VectorCanisterError::DimensionMismatch);
            }
            let VectorSubject::Vertex {
                shard_id,
                vertex_id,
            } = *subject;
            let payload =
                VertexPayload::new(vertex_id).ok_or(VectorCanisterError::DimensionMismatch)?;
            validated.push((shard_id.raw(), payload));
        }

        let head_key = PartitionKey::new(index_id, index_version, partition_id);
        let head = partition_head_get(&head_key).unwrap_or_default();

        // Plan every page boundary and run-table roll without touching stable state. Batches always
        // start fresh pages (the building path writes empty-to-nearly-empty shadow partitions).
        let mut boundaries = Vec::new();
        let mut row_start = 0usize;
        let mut page_rows = 0u32;
        let mut run_count = 0u32;
        let mut last_shard = None;
        for (row_index, (shard, _)) in validated.iter().enumerate() {
            let starts_new_run = last_shard.is_some_and(|last| last != *shard);
            if page_rows >= capacity || (starts_new_run && run_count >= run_capacity) {
                boundaries.push((row_start, row_index));
                row_start = row_index;
                page_rows = 0;
                run_count = 0;
                last_shard = None;
            }
            if last_shard != Some(*shard) {
                run_count += 1;
                last_shard = Some(*shard);
            }
            page_rows += 1;
        }
        boundaries.push((row_start, rows.len()));

        // Lease every planned block first (free-list consumption is eager; tail publication is
        // deferred to commit so a failed batch leaves `occupied_tail` untouched), then reserve.
        let mut plans = Vec::with_capacity(boundaries.len());
        let mut leases = Vec::with_capacity(boundaries.len());
        let mut page_id = head.next_page_id;
        let mut pending_tail = tail_next_seq(self.occupied_tail);
        for (row_start, row_end) in &boundaries {
            let lease = self.lease_block(&mut pending_tail)?;
            plans.push(BatchPagePlan {
                page_id,
                page_seq: lease.seq,
                slab_offset: block_offset(lease.seq),
                row_start: *row_start,
                row_end: *row_end,
            });
            leases.push(lease);
            page_id = page_id.checked_add(1).expect("page id overflow");
        }

        // Reserve every planned block (headers written beyond the committed state are unreachable).
        for (_plan, lease) in plans.iter().zip(&leases) {
            #[cfg(test)]
            if take_injected_append_rows_reserve_failure() {
                return Err(VectorCanisterError::StableGrowFailed);
            }
            self.reserve_leased(
                lease,
                capacity,
                row_stride,
                def.meta_stride_bytes,
                run_capacity,
                code_stride,
            )?;
        }

        // Per-page counters and bounds derive purely from the validated plan (no stable access):
        // batches write every row live, so `live == rows`, and the bound is the max row norm.
        let page_entry = |plan: &BatchPagePlan| PageTableEntry {
            seq: plan.page_seq,
            row_count: (plan.row_end - plan.row_start) as u32,
            live_count: (plan.row_end - plan.row_start) as u32,
            block_bound: rows[plan.row_start..plan.row_end]
                .iter()
                .map(|(_, bytes, aux)| {
                    Self::stored_row_norm_sq(def.encoding, bytes, aux, def.dims).sqrt()
                })
                .fold(0.0_f32, f32::max),
        };

        // Build the sealed-entry suffix in memory: seal the current mutable page (if any), then
        // every planned page except the last (which becomes the new mutable tail). The chunked
        // table writer truncates any interrupted-attempt artifacts beyond the final position.
        let mut new_seals = Vec::with_capacity(plans.len());
        if head.page_count > 0 {
            new_seals.push(PageTableEntry {
                seq: head.mutable_seq,
                row_count: head.mutable_rows,
                live_count: head.mutable_live,
                block_bound: head.mutable_bound,
            });
        }
        for plan in &plans[..plans.len() - 1] {
            new_seals.push(page_entry(plan));
        }

        let last_plan = plans.last().expect("non-empty batch plan");
        let last_rows = &validated[last_plan.row_start..last_plan.row_end];
        let mut final_head = head;
        final_head.mutable_page = last_plan.page_id;
        final_head.page_count = final_head
            .page_count
            .checked_add(plans.len() as u64)
            .expect("partition page count overflow");
        final_head.live_len = final_head
            .live_len
            .checked_add(rows.len() as u64)
            .expect("partition live length overflow");
        final_head.next_page_id = page_id;
        final_head.mutable_seq = last_plan.page_seq;
        final_head.mutable_rows = last_rows.len() as u32;
        final_head.mutable_live = last_rows.len() as u32;
        final_head.mutable_last_shard = last_rows.last().expect("non-empty page").0;
        final_head.mutable_run_count =
            last_rows.windows(2).filter(|w| w[0].0 != w[1].0).count() as u32 + 1;
        final_head.mutable_bound = page_entry(last_plan).block_bound;

        // The two fallible directory publishes, in order; after them the batch is infallible.
        let start_pos = if head.page_count > 0 {
            head.page_count - 1
        } else {
            0
        };
        append_sealed_entries(index_id, index_version, partition_id, start_pos, &new_seals)?;
        // Infallible deferred-tail publication precedes the committing head insert.
        self.commit_leases(&leases);
        partition_head_insert(head_key, final_head)
            .map_err(|_| VectorCanisterError::StableGrowFailed)?;

        // Infallible row/run writes.
        for plan in &plans {
            let mut header = page_header;
            let mut last_shard = None;
            let mut last_run_len = 0u32;
            for row_index in plan.row_start..plan.row_end {
                let (shard, payload) = validated[row_index];
                let (_, bytes, aux) = rows[row_index];
                let slot = (row_index - plan.row_start) as u32;
                if slot == 0 {
                    write_run_at(
                        &self.slab,
                        plan.slab_offset,
                        &layout,
                        0,
                        RunEntry::new(shard, 1),
                    );
                    header.run_count = 1;
                    write_page_header_run_count(
                        &self.slab,
                        plan.slab_offset,
                        &header,
                        header.run_count,
                    );
                    last_shard = Some(shard);
                    last_run_len = 1;
                } else if last_shard == Some(shard) {
                    last_run_len += 1;
                    write_run_at(
                        &self.slab,
                        plan.slab_offset,
                        &layout,
                        header.run_count - 1,
                        RunEntry::new(shard, last_run_len),
                    );
                } else {
                    write_run_at(
                        &self.slab,
                        plan.slab_offset,
                        &layout,
                        header.run_count,
                        RunEntry::new(shard, 1),
                    );
                    header.run_count += 1;
                    write_page_header_run_count(
                        &self.slab,
                        plan.slab_offset,
                        &header,
                        header.run_count,
                    );
                    last_shard = Some(shard);
                    last_run_len = 1;
                }
                let code_segment = encoder.as_mut().map(|encoder| {
                    let mut seg = vec![0u8; layout.code_stride()];
                    encoder.encode_segment(bytes, &aux, &mut seg);
                    seg
                });
                self.write_row(
                    plan.slab_offset,
                    &layout,
                    slot,
                    payload,
                    bytes,
                    &aux,
                    code_segment.as_deref(),
                );
                slots.push(SlotRef {
                    index_version: index_version as u32,
                    partition_id,
                    page_id: plan.page_id as u32,
                    slot,
                });
            }
        }

        Ok(slots)
    }

    /// Marks a slot tombstoned, owning all live accounting idempotently: on the live->tombstoned
    /// transition it sets the payload tombstone bit, decrements the owning page's live counter
    /// (sealed pages via their table entry, the mutable tail via the head scalar; tombstoned rows
    /// are derived as `row_count − live_count`) and the row's `VECTOR_PARTITION_HEADS.live_len`
    /// exactly once. Returns `true` only when the row changed (was previously live and in range).
    /// The page's block bound is deliberately left untouched (monotone, conservative).
    pub(crate) fn tombstone_row(&mut self, index_id: u32, slot: SlotRef) -> bool {
        let version = slot.index_version as u64;
        let head_key = PartitionKey::new(index_id, version, slot.partition_id);
        let Some(mut head) = partition_head_get(&head_key) else {
            return false;
        };
        let sealed_len = head.page_count.saturating_sub(1);
        let page_id = u64::from(slot.page_id);

        // Resolve the page location + written-row bound; reject unknown/out-of-range slots
        // before touching anything.
        let (seq, row_count) = if page_id < sealed_len {
            match sealed_entry_at(index_id, version, slot.partition_id, page_id) {
                Some(entry) => (entry.seq, entry.row_count),
                None => return false,
            }
        } else if page_id == sealed_len {
            (head.mutable_seq, head.mutable_rows)
        } else {
            return false;
        };
        if slot.slot >= row_count {
            return false;
        }
        let base = block_offset(seq);
        let header = self.read_page_header(base);
        let layout = PageLayout::new(&header).expect("valid page layout");
        let meta_range = layout.row_meta_range_at(slot.slot);
        // Fixed-width stack buffer: `meta_stride` is 4 | 8 | 12, so the tombstone hot path never
        // heap-allocates.
        let mut meta_buf = [0u8; MAX_META_STRIDE as usize];
        let buf = &mut meta_buf[..layout.meta_stride()];
        self.slab.read(base + meta_range.start as u64, buf);
        let mut row_meta = RowMeta::from_bytes(buf, layout.meta_stride()).expect("decode row meta");
        if row_meta.vertex.is_tombstone() {
            return false;
        }
        row_meta.vertex = row_meta.vertex.tombstoned();
        row_meta
            .write_into(buf, layout.meta_stride())
            .expect("encode row meta");
        self.slab.write(base + meta_range.start as u64, buf);

        if page_id < sealed_len {
            // Patch just the owning chunk (rare remove path; one chunk rewrite).
            let chunk = (page_id as usize / ENTRIES_PER_TABLE_CHUNK) as u32;
            let mut table = page_table_chunk_get(index_id, version, slot.partition_id, chunk)
                .expect("sealed chunk present");
            let entry = &mut table.entries[page_id as usize % ENTRIES_PER_TABLE_CHUNK];
            entry.live_count = entry.live_count.saturating_sub(1);
            page_table_chunk_put(index_id, version, slot.partition_id, chunk, table)
                .expect("tombstone table insert");
        } else {
            head.mutable_live = head.mutable_live.saturating_sub(1);
        }
        head.live_len = head.live_len.saturating_sub(1);
        partition_head_insert(head_key, head).expect("tombstone head insert");
        true
    }

    /// Reads a slot's vector bytes + decoded vertex id, rejecting out-of-range and tombstoned slots.
    /// Callers resolve the subject from the map key and validate the returned `vertex_id` against
    /// `subject.vertex_id` (positional + payload validation; no row-carried id/generation).
    pub(crate) fn read_row_bytes(
        &self,
        index_id: u32,
        slot: SlotRef,
    ) -> Option<(u32, Vec<u8>, [u8; 8])> {
        let version = slot.index_version as u64;
        let head_key = PartitionKey::new(index_id, version, slot.partition_id);
        let head = partition_head_get(&head_key)?;
        let sealed_len = head.page_count.saturating_sub(1);
        let page_id = u64::from(slot.page_id);
        let (seq, row_count) = if page_id < sealed_len {
            let entry = sealed_entry_at(index_id, version, slot.partition_id, page_id)?;
            (entry.seq, entry.row_count)
        } else if page_id == sealed_len {
            (head.mutable_seq, head.mutable_rows)
        } else {
            return None;
        };
        if slot.slot >= row_count {
            return None;
        }
        let base = block_offset(seq);
        let header = self.read_page_header(base);
        let layout = PageLayout::new(&header).ok()?;
        let meta_range = layout.row_meta_range_at(slot.slot);
        // Fixed-width stack buffer: `meta_stride` is 4 | 8 | 12, so the point read never
        // heap-allocates for the row header.
        let mut meta_buf = [0u8; MAX_META_STRIDE as usize];
        let meta_slice = &mut meta_buf[..layout.meta_stride()];
        self.slab.read(base + meta_range.start as u64, meta_slice);
        let row_meta = RowMeta::from_bytes(meta_slice, layout.meta_stride()).ok()?;
        if row_meta.vertex.is_tombstone() {
            return None;
        }
        let vec_start = base + layout.vector_range_at(slot.slot).start as u64;
        // The returned bytes stay stored-row wide (`vector_stride`): the row-format contract
        // returns the full padded row (its trailing pad is guaranteed zero by the reservation
        // zero fill), and callers interpret only the meaningful prefix.
        let mut out = vec![0u8; layout.vector_stride()];
        self.slab.read(vec_start, &mut out);
        Some((row_meta.vertex.vertex_id(), out, row_meta.aux))
    }

    /// Bulk-reads one specific page into `scratch`. Returns `false` when the page is absent from
    /// the partition chain or its header is invalid, so the caller skips the page group (the same
    /// fail path as `read_row_bytes`'s `None`). `scratch` is reused across pages, so a scan pays
    /// one bulk read per distinct page instead of one per row.
    pub(crate) fn load_page(&self, page_key: PageKey, scratch: &mut PageScratch) -> bool {
        let Some((seq, row_count)) = self.resolve_page(
            page_key.index_id,
            page_key.index_version,
            page_key.partition_id,
            page_key.page_id,
        ) else {
            return false;
        };
        let header = self.read_page_header(block_offset(seq));
        if PageLayout::new(&header).is_err() {
            return false;
        }
        scratch.load(&self.slab, block_offset(seq), row_count, &header);
        true
    }

    /// Resolves a positional page id within one partition chain to its `(block seq, row_count)`:
    /// ids below the tail live in the sealed-page table, the tail id is the head's mutable mirror.
    fn resolve_page(
        &self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        page_id: u64,
    ) -> Option<(u32, u32)> {
        let head = partition_head_get(&PartitionKey::new(index_id, index_version, partition_id))?;
        let sealed_len = head.page_count.saturating_sub(1);
        if page_id < sealed_len {
            let entry = sealed_entry_at(index_id, index_version, partition_id, page_id)?;
            Some((entry.seq, entry.row_count))
        } else if page_id == sealed_len {
            Some((head.mutable_seq, head.mutable_rows))
        } else {
            None
        }
    }

    /// Resolves one partition's full page chain in positional order — one head read plus one read
    /// per populated table chunk. The scalar skip bound (`block_bound`) rides along so scan
    /// callers can drop whole pages before any slab read (L2 only; cosine bounds are powerless
    /// on unit rows and are ignored there).
    pub(crate) fn partition_page_metas(
        &self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
    ) -> Vec<PageMetaView> {
        let head_key = PartitionKey::new(index_id, index_version, partition_id);
        let Some(head) = partition_head_get(&head_key) else {
            return Vec::new();
        };
        let mut out = Vec::with_capacity(head.page_count as usize);
        for entry in sealed_entries_of(
            index_id,
            index_version,
            partition_id,
            head.page_count.saturating_sub(1),
        ) {
            out.push(PageMetaView {
                page_id: out.len() as u64,
                slab_offset: block_offset(entry.seq),
                row_count: entry.row_count,
                live_count: entry.live_count,
                block_bound: entry.block_bound,
            });
        }
        out.push(PageMetaView {
            page_id: head.page_count - 1,
            slab_offset: block_offset(head.mutable_seq),
            row_count: head.mutable_rows,
            live_count: head.mutable_live,
            block_bound: head.mutable_bound,
        });
        out
    }

    /// Page/batch visitor over one partition's page chain. Each page is bulk-read once into
    /// `scratch`; the visitor is invoked per live (non-tombstoned) slot with the decoded
    /// [`RowInfo`] (shard from the run table + vertex id from the payload) and a zero-copy slice into
    /// the contiguous `vector_bytes` table.
    pub(crate) fn visit_partition_pages<F: FnMut(SlotRef, &RowInfo, &[u8])>(
        &self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        scratch: &mut PageScratch,
        mut visitor: F,
    ) {
        self.visit_partition_pages_grouped(
            index_id,
            index_version,
            partition_id,
            scratch,
            |page_id, scratch| {
                for slot in 0..scratch.row_count() {
                    // One `RowMeta::from_bytes` per row decides liveness and yields the decoded
                    // identity; tombstoned rows skip the run-table lookup entirely.
                    let Some(info) = scratch.live_row_info(slot) else {
                        continue;
                    };
                    visitor(
                        SlotRef {
                            index_version: index_version as u32,
                            partition_id,
                            page_id: page_id as u32,
                            slot,
                        },
                        &info,
                        scratch.vec_slice(slot),
                    );
                }
            },
        );
    }

    /// Page-grouped walk over one partition's page chain: the visitor is invoked **once per loaded
    /// page** with the populated [`PageScratch`], so callers that need both row slices and page-local
    /// aggregation (the code-tier Stage A shortlist + same-page Stage B rerank) can iterate slots
    /// themselves via `live_row_info` / `vec_slice` / `code_slice` while every page is still
    /// bulk-read exactly once. Single walk owner: [`Self::visit_partition_pages`] delegates here.
    pub(crate) fn visit_partition_pages_grouped<F: FnMut(u64, &PageScratch)>(
        &self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        scratch: &mut PageScratch,
        mut visitor: F,
    ) {
        for view in self.partition_page_metas(index_id, index_version, partition_id) {
            let header = self.read_page_header(view.slab_offset);
            scratch.load(&self.slab, view.slab_offset, view.row_count, &header);
            visitor(view.page_id, scratch);
        }
    }

    /// Bounded, cursor-resumable teardown of one `(index_id, version)` generation's pages.
    /// **Whole-partition granularity**: a partition is drained atomically — its sealed chunks are
    /// removed, its blocks freed into the slab free list, and its leaf head removed — so at every
    /// message boundary each surviving partition remains fully intact (reopen-consistent) and each
    /// drained partition leaves no records behind. At least one remaining partition is processed
    /// per call regardless of `budget` (progress guarantee; budgets below one partition's page
    /// count still terminate). The rebuild teardown drops coarse-level heads/centroids afterwards.
    /// The cursor is the highest drained partition id as `PageKey` bytes (`page_id` field is
    /// `u64::MAX`).
    pub(crate) fn drop_version_pages(
        &mut self,
        index_id: u32,
        version: u64,
        cursor: Option<Vec<u8>>,
        budget: u32,
    ) -> DropProgress {
        // Live partitions of the version in ascending order with their full page lists.
        let mut partitions: Vec<(u32, PartitionHead, Vec<u32>)> = Vec::new();
        for (key, record) in collect_head_records() {
            let PartitionHeadRecord::Head(head) = record else {
                continue;
            };
            if key.index_id != index_id || key.index_version != version || head.page_count == 0 {
                continue;
            }
            let seqs = sealed_entries_of(
                index_id,
                version,
                key.partition_id,
                head.page_count.saturating_sub(1),
            )
            .into_iter()
            .map(|entry| entry.seq)
            .chain([head.mutable_seq])
            .collect::<Vec<u32>>();
            partitions.push((key.partition_id, head, seqs));
        }

        let resume_partition = cursor
            .as_ref()
            .map(|bytes| PageKey::from_bytes(Cow::Owned(bytes.clone())).partition_id);
        let mut seqs_to_free: Vec<u32> = Vec::new();
        let mut last: Option<PageKey> = None;
        let mut exhausted = true;
        let mut remaining_budget = budget.max(1) as u64;
        for (partition, _head, seqs) in &partitions {
            if resume_partition.is_some_and(|r| *partition <= r) {
                continue;
            }
            // Whole-partition atomic drain; the first candidate always runs (progress).
            seqs_to_free.extend_from_slice(seqs);
            page_table_remove_all(index_id, version, *partition);
            partition_head_remove(&PartitionKey::new(index_id, version, *partition));
            last = Some(PageKey::new(index_id, version, *partition, u64::MAX));
            exhausted = false; // provisional until the loop end proves otherwise
            remaining_budget = remaining_budget.saturating_sub(seqs.len() as u64);
            if remaining_budget == 0 {
                break;
            }
        }
        // `exhausted` only when every partition was drained in this call.
        if last
            .as_ref()
            .is_some_and(|l| partitions.iter().all(|(p, _, _)| *p <= l.partition_id))
        {
            exhausted = true;
        }
        if partitions.is_empty() {
            exhausted = true;
        }

        if !seqs_to_free.is_empty() {
            self.free_blocks(&mut seqs_to_free);
        }

        let cursor = if exhausted {
            None
        } else {
            last.map(Storable::into_bytes)
        };
        DropProgress { cursor, exhausted }
    }

    /// One bounded slab-compaction pass segment (plan 0278, Slice 8 block addressing). Continues
    /// the current lap after `scan_cursor` (`None` starts a fresh lap), examining at most
    /// `max_entries` live-page locations and copying at most `max_bytes` of whole uniform blocks
    /// down into the dense prefix.
    ///
    /// Collection rule: a page is collected exactly when its block lies inside the snapshot window
    /// `[write_cursor, range_end)` (both block-aligned), regardless of when it was created — with
    /// free-list reuse a post-start append may land in an in-range hole and is moved like any
    /// other live page; pages dropped mid-compaction by teardown vanish from their records and
    /// are never re-examined. The first in-range page is always admitted so every step makes
    /// forward progress; later ones only while their cumulative byte budget fits.
    ///
    /// Move rule: collected blocks are copied contiguously down to `write_cursor` in ascending
    /// source order — destinations never reach the next source — and each owner's `seq` (sealed-
    /// table entry or head mutable mirror) swaps only after its bytes persist. An interrupted move
    /// leaves duplicate dead bytes below, never a dangling reference; owner updates trap on map
    /// growth so the message rolls back atomically and the persisted cursors resume exactly.
    ///
    /// Exhaustion: when a full lap completes without collecting any page, nothing live remains in
    /// the snapshot range; finalize runs in the same message — it fails closed if any live page
    /// sits inside the reclaimed gap, persists `occupied_tail = max(write_cursor, highest live
    /// block end)` once, and registers the reclaimed gap as free runs (free-side reuse). Post-
    /// start appends above the range keep the persisted tail above the gap; only a quiescent
    /// store reclaims all the way down to the write cursor.
    pub(crate) fn compact_step(
        &mut self,
        write_cursor: u64,
        range_end: u64,
        scan_cursor: Option<PageKey>,
        max_entries: u32,
        max_bytes: u64,
    ) -> Result<SlabCompactStepOutcome, VectorCanisterError> {
        assert!(
            SLAB_HEADER_SIZE as u64 <= write_cursor && write_cursor <= range_end,
            "vector slab compaction: corrupt durable cursors (write {write_cursor}, range end \
             {range_end})"
        );

        // Live page locations, ordered by (index_id, index_version, partition_id, positional) so
        // examination/resume is deterministic across steps. The cursor key compares on the same
        // tuple via `PageKey`.
        let mut all: Vec<CompactCandidate> = Vec::new();
        for (key, record) in collect_head_records() {
            match record {
                PartitionHeadRecord::Head(head) if head.page_count > 0 => {
                    all.push(CompactCandidate {
                        positional: head.page_count - 1,
                        src_seq: head.mutable_seq,
                        mutable: true,
                        owner: PartitionKey::new(key.index_id, key.index_version, key.partition_id),
                    });
                }
                PartitionHeadRecord::Head(_) => {}
                PartitionHeadRecord::Table(table) => {
                    // Chunk keys carry the chunk index in their level byte; the global
                    // positional id is `chunk * ENTRIES_PER_TABLE_CHUNK + slot`.
                    let chunk_index = (key.level - PARTITION_LEVEL_PAGE_TABLE_BASE) as usize;
                    for (i, entry) in table.entries.iter().enumerate() {
                        all.push(CompactCandidate {
                            positional: (chunk_index * ENTRIES_PER_TABLE_CHUNK + i) as u64,
                            src_seq: entry.seq,
                            mutable: false,
                            owner: PartitionKey::new(
                                key.index_id,
                                key.index_version,
                                key.partition_id,
                            ),
                        });
                    }
                }
            }
        }
        let candidate_key = |c: &CompactCandidate| {
            PageKey::new(
                c.owner.index_id,
                c.owner.index_version,
                c.owner.partition_id,
                c.positional,
            )
        };
        all.sort_by_key(|c| candidate_key(c));

        // Examine in key order after the resume cursor; collect the copy batch.
        let mut batch: Vec<CompactCandidate> = Vec::new();
        let mut batch_bytes = 0u64;
        let mut examined = 0u32;
        let mut last_examined: Option<PageKey> = None;
        let mut lap_complete = true;
        for cand in &all {
            let ck = candidate_key(cand);
            if scan_cursor.as_ref().is_some_and(|cursor| ck <= *cursor) {
                continue;
            }
            if examined >= max_entries.max(1) {
                lap_complete = false;
                break;
            }
            examined += 1;
            last_examined = Some(ck);
            let off = block_offset(cand.src_seq);
            if off < write_cursor || off >= range_end {
                // Dense prefix below the write cursor, or a post-start append outside the
                // snapshot range.
                continue;
            }
            if !batch.is_empty() && batch_bytes + BLOCK_LEN > max_bytes {
                continue; // beyond this message's copy budget; picked up on a later lap.
            }
            batch.push(cand.clone());
            batch_bytes += BLOCK_LEN;
        }

        if batch.is_empty() {
            return Ok(if lap_complete {
                let final_tail = self.compact_finalize(write_cursor, range_end);
                SlabCompactStepOutcome {
                    finalized: true,
                    write_cursor: final_tail,
                    pages_moved: 0,
                    scan_cursor: None,
                }
            } else {
                SlabCompactStepOutcome {
                    finalized: false,
                    write_cursor,
                    pages_moved: 0,
                    scan_cursor: last_examined,
                }
            });
        }

        // Ascending source order keeps every copy strictly below the not-yet-copied sources
        // (destinations are block-granular and sources disjoint). Destination blocks are
        // unlinked from the intrusive free chain **before** any bytes move, so a moved live
        // page can never share its block with a future allocation.
        batch.sort_by_key(|c| c.src_seq);
        let dest_start = tail_next_seq(write_cursor);
        let dest_end_seq = dest_start + batch.len() as u32;
        self.sanitize_free_chain(|seq| !(seq >= dest_start && seq < dest_end_seq));
        let dest_end = write_cursor
            .checked_add(batch_bytes)
            .expect("compaction destination overflow");
        grow_to_at_least(&self.slab, dest_end)?;
        let mut buf = vec![0u8; BLOCK_LEN as usize];
        let mut w = write_cursor;
        let mut moved: Vec<(CompactCandidate, u32)> = Vec::with_capacity(batch.len());
        let mut pages_moved = 0u64;
        for cand in &batch {
            self.slab.read(block_offset(cand.src_seq), &mut buf);
            self.slab.write(w, &buf);
            let dest_seq = tail_next_seq(w);
            moved.push((cand.clone(), dest_seq));
            w += BLOCK_LEN;
            pages_moved += 1;
        }
        debug_assert_eq!(w, dest_end);

        // Swap each owner's seq references now that the bytes are persisted. Traps (message
        // rollback) on map growth keep cursors + records consistent.
        for (owner, group) in moved.iter().fold(
            Vec::<(PartitionKey, Vec<&(CompactCandidate, u32)>)>::new(),
            |mut acc, m| {
                match acc.last_mut() {
                    Some((key, group)) if *key == m.0.owner => group.push(m),
                    _ => acc.push((m.0.owner, vec![m])),
                }
                acc
            },
        ) {
            if group.iter().any(|(c, _)| c.mutable) {
                let mut head = partition_head_get(&owner)
                    .expect("compact: owning partition head vanished mid-move");
                for (_, dest) in group.iter().filter(|(c, _)| c.mutable) {
                    head.mutable_seq = *dest;
                }
                partition_head_insert(owner, head).expect("compact head update");
            } else {
                // Sealed entries live in chunks; rewrite each touched chunk once.
                let mut touched: Vec<(u32, PageTableChunk)> = Vec::new();
                for (cand, dest) in group.iter().filter(|(c, _)| !c.mutable) {
                    let chunk = (cand.positional as usize / ENTRIES_PER_TABLE_CHUNK) as u32;
                    let slot = cand.positional as usize % ENTRIES_PER_TABLE_CHUNK;
                    let table = match touched.iter_mut().find(|(c, _)| *c == chunk) {
                        Some((_, t)) => t,
                        None => {
                            let t = page_table_chunk_get(
                                owner.index_id,
                                owner.index_version,
                                owner.partition_id,
                                chunk,
                            )
                            .expect("compact: owning chunk vanished mid-move");
                            touched.push((chunk, t));
                            &mut touched.last_mut().expect("just pushed").1
                        }
                    };
                    table.entries[slot].seq = *dest;
                }
                for (chunk, table) in touched {
                    page_table_chunk_put(
                        owner.index_id,
                        owner.index_version,
                        owner.partition_id,
                        chunk,
                        table,
                    )
                    .expect("compact table update");
                }
            }
        }

        Ok(SlabCompactStepOutcome {
            finalized: false,
            write_cursor: w,
            pages_moved,
            scan_cursor: if lap_complete { None } else { last_examined },
        })
    }

    /// Rewrites the intrusive free chain, keeping only nodes that satisfy `keep` (compaction
    /// uses this to unlink destination blocks and to prune stale nodes after the tail rewind).
    fn sanitize_free_chain(&self, keep: impl Fn(u32) -> bool) {
        let hop_cap = u64::from(tail_next_seq(self.occupied_tail)) + 1;
        let mut kept: Vec<u32> = Vec::new();
        let mut cur = slab_free_anchor_get();
        let mut hops = 0u64;
        while let Some(seq) = cur {
            if seq == u32::MAX {
                break;
            }
            hops += 1;
            if hops > hop_cap {
                panic!(
                    "vector slab free-block chain longer than allocated blocks (corruption); run \
                     a slab compaction"
                );
            }
            if keep(seq) {
                kept.push(seq);
            }
            let mut next_buf = [0u8; 4];
            self.slab.read(block_offset(seq), &mut next_buf);
            let next = u32::from_le_bytes(next_buf);
            cur = (next != u32::MAX).then_some(next);
        }
        // Relink the kept nodes in order; each write touches only 4 bytes.
        let anchor = kept.first().copied();
        for (i, seq) in kept.iter().enumerate() {
            let next_bytes = kept
                .get(i + 1)
                .map_or(u32::MAX.to_le_bytes(), |n| n.to_le_bytes());
            self.slab.write(block_offset(*seq), &next_bytes);
        }
        slab_free_anchor_set(anchor);
    }

    /// Reclaim gate + single tail rewind (plan 0278, Slice 8). Fails closed when any live page
    /// sits inside the reclaimed block gap `[write_cursor, range_end)` — i.e., when a mover bug
    /// stranded a live page — then persists `occupied_tail = max(write_cursor, highest live block
    /// end)` exactly once and registers the reclaimed gap as one free run. Live blocks at/above
    /// `range_end` belong to post-start appends; they keep the persisted tail above the gap, so
    /// only a quiescent store reclaims down to `write_cursor`. Returns the persisted tail.
    fn compact_finalize(&mut self, write_cursor: u64, range_end: u64) -> u64 {
        let wc_seq = tail_next_seq(write_cursor);
        let re_seq = tail_next_seq(range_end);
        let mut highest_seq = 0u32;
        for (_, record) in collect_head_records() {
            match record {
                PartitionHeadRecord::Head(head) if head.page_count > 0 => {
                    assert!(
                        !(head.mutable_seq >= wc_seq && head.mutable_seq < re_seq),
                        "vector slab compaction: live mutable page {} sits inside the reclaimed \
                         gap [{wc_seq}, {re_seq})",
                        head.mutable_seq
                    );
                    highest_seq = highest_seq.max(head.mutable_seq);
                }
                PartitionHeadRecord::Head(_) => {}
                PartitionHeadRecord::Table(table) => {
                    for entry in &table.entries {
                        assert!(
                            !(entry.seq >= wc_seq && entry.seq < re_seq),
                            "vector slab compaction: live page {} sits inside the reclaimed gap \
                             [{wc_seq}, {re_seq})",
                            entry.seq
                        );
                        highest_seq = highest_seq.max(entry.seq);
                    }
                }
            }
        }
        let new_tail = write_cursor.max(block_offset(highest_seq));
        let tail_seq = tail_next_seq(new_tail);
        // Prune the free chain after the rewind: nodes at/above the new tail are subsumed by
        // natural tail regrowth; nodes below the dense prefix cannot exist (dense). Everything
        // between stays a genuine reusable hole.
        let frontier_seq = wc_seq;
        self.sanitize_free_chain(|seq| seq >= frontier_seq && seq < tail_seq);
        self.set_occupied_tail(new_tail);
        new_tail
    }

    /// Derived, admin-only slab-space observability. Computes whole-slab physical facts plus logical
    /// counters scoped to `index_id` (`None` = all indexes), in a single pass over the partition
    /// heads collection.
    ///
    /// **Unbounded**: it scans every page record (even for `Some(index_id)`, because the global
    /// dead-space estimate needs the whole slab). Reads only head/table records + the slab
    /// header/size — never row bytes; referenced bytes count whole uniform blocks (Slice 8), so
    /// they and `occupied_tail` share one unit. `physical_live_row_count` is the physical
    /// non-tombstone live count, not subject-freshness.
    pub(crate) fn stats_for_index(&self, index_id: Option<u32>) -> VectorSlabStats {
        let mut acc = SlabStatsAcc::new(index_id);
        for (key, record) in collect_head_records() {
            match record {
                PartitionHeadRecord::Head(head) if head.page_count > 0 => {
                    acc.observe(
                        &PageKey::new(
                            key.index_id,
                            key.index_version,
                            key.partition_id,
                            head.page_count - 1,
                        ),
                        head.mutable_rows,
                        head.mutable_live,
                    );
                }
                PartitionHeadRecord::Head(_) => {}
                PartitionHeadRecord::Table(table) => {
                    let chunk_base = ((key.level - PARTITION_LEVEL_PAGE_TABLE_BASE) as usize)
                        * ENTRIES_PER_TABLE_CHUNK;
                    for (i, entry) in table.entries.iter().enumerate() {
                        acc.observe(
                            &PageKey::new(
                                key.index_id,
                                key.index_version,
                                key.partition_id,
                                (chunk_base + i) as u64,
                            ),
                            entry.row_count,
                            entry.live_count,
                        );
                    }
                }
            }
        }
        let (scope, versions, referenced_global) = acc.finish();

        let slab_size_bytes = self.slab.size().saturating_mul(WASM_PAGE_SIZE);
        let estimated_unreferenced_bytes = self
            .occupied_tail
            .saturating_sub(SLAB_HEADER_SIZE as u64)
            .saturating_sub(referenced_global);

        VectorSlabStats {
            slab: VectorSlabGlobalStats {
                slab_size_bytes,
                occupied_tail_bytes: self.occupied_tail,
                referenced_page_bytes_global: referenced_global,
                estimated_unreferenced_bytes,
            },
            scope,
            versions,
        }
    }

    /// Bounded, cursor-resumable variant of [`stats_for_index`](Self::stats_for_index) for the
    /// IC-safe `admin_vector_slab_stats_step` query. Processes at most `max_pages` page records
    /// (clamped to `1..=MAX_SLAB_STATS_STEP_PAGES`) in ascending `(index_id, index_version,
    /// partition_id, positional)` order, returning an opaque `PageKey` cursor to resume from.
    /// Callers repeat until `exhausted` and merge the additive partials client-side.
    ///
    /// Reads only head/table records + the slab header/size — never row bytes. The `cursor` is
    /// **external caller input**, so a malformed (wrong-length) cursor is rejected with
    /// [`VectorCanisterError::InvalidStatsCursor`] rather than trapping.
    pub(crate) fn stats_step(
        &self,
        cursor: Option<Vec<u8>>,
        max_pages: u32,
        index_id: Option<u32>,
    ) -> Result<VectorSlabStatsStep, VectorCanisterError> {
        let budget = max_pages.clamp(1, MAX_SLAB_STATS_STEP_PAGES);
        if let Some(bytes) = &cursor
            && bytes.len() != PAGE_KEY_LEN
        {
            return Err(VectorCanisterError::InvalidStatsCursor);
        }

        let mut acc = SlabStatsAcc::new(index_id);
        let mut last: Option<PageKey> = None;
        let mut exhausted = true;
        let mut processed: u32 = 0;
        let resume = cursor
            .as_ref()
            .map(|bytes| PageKey::from_bytes(Cow::Borrowed(bytes)));
        'records: for (key, record) in collect_head_records() {
            let pages: Vec<(PageKey, u32, u32)> = match &record {
                PartitionHeadRecord::Head(h) if h.page_count > 0 => {
                    vec![(
                        PageKey::new(
                            key.index_id,
                            key.index_version,
                            key.partition_id,
                            h.page_count - 1,
                        ),
                        h.mutable_rows,
                        h.mutable_live,
                    )]
                }
                PartitionHeadRecord::Head(_) => Vec::new(),
                PartitionHeadRecord::Table(t) => t
                    .entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let chunk_base = ((key.level - PARTITION_LEVEL_PAGE_TABLE_BASE) as usize)
                            * ENTRIES_PER_TABLE_CHUNK;
                        (
                            PageKey::new(
                                key.index_id,
                                key.index_version,
                                key.partition_id,
                                (chunk_base + i) as u64,
                            ),
                            e.row_count,
                            e.live_count,
                        )
                    })
                    .collect(),
            };
            for (page_key, rows, live) in pages {
                if resume.as_ref().is_some_and(|r| page_key <= *r) {
                    continue;
                }
                if processed >= budget {
                    exhausted = false;
                    break 'records;
                }
                acc.observe(&page_key, rows, live);
                last = Some(page_key);
                processed += 1;
            }
        }
        let (scope, versions, referenced_global) = acc.finish();
        let cursor_out = if exhausted {
            None
        } else {
            last.map(Storable::into_bytes)
        };

        let slab_size_bytes = self.slab.size().saturating_mul(WASM_PAGE_SIZE);
        Ok(VectorSlabStatsStep {
            partial: VectorSlabStatsPartial {
                slab: VectorSlabStepGlobalStats {
                    slab_size_bytes,
                    occupied_tail_bytes: self.occupied_tail,
                    referenced_page_bytes_global: referenced_global,
                },
                scope,
                versions,
            },
            cursor: cursor_out,
            exhausted,
        })
    }

    /// Bounded, cursor-resumable tombstone-health scan scoped to one
    /// `(index_id, active_version)`. Processes at most `max_pages` page records (clamped to
    /// `1..=MAX_SLAB_STATS_STEP_PAGES`) in positional order per partition, aggregating
    /// `row_count`/`live_count` into `total_rows`/`physical_live_rows`/`tombstoned_rows`
    /// (tombstoned rows are derived as `row_count − live_count`), and returns an opaque `PageKey`
    /// cursor to resume from. Reads only head/table records — never row bytes.
    ///
    /// The caller-supplied `cursor` is **scope-checked**: a wrong-length cursor, or one whose
    /// `(index_id, index_version)` does not match `(index_id, active_version)`, returns
    /// [`VectorCanisterError::InvalidStatsCursor`] rather than silently yielding an empty result.
    pub(crate) fn partition_page_health_step(
        &self,
        index_id: u32,
        active_version: u64,
        cursor: Option<Vec<u8>>,
        max_pages: u32,
    ) -> Result<VectorPartitionHealthStep, VectorCanisterError> {
        let budget = max_pages.clamp(1, MAX_SLAB_STATS_STEP_PAGES);
        if let Some(bytes) = &cursor {
            if bytes.len() != PAGE_KEY_LEN {
                return Err(VectorCanisterError::InvalidStatsCursor);
            }
            let key = PageKey::from_bytes(Cow::Borrowed(bytes));
            if key.index_id != index_id || key.index_version != active_version {
                return Err(VectorCanisterError::InvalidStatsCursor);
            }
        }

        // Pages of the scoped generation in (partition asc, positional asc) order.
        let mut pages: Vec<(PageKey, u32, u32)> = Vec::new();
        for (key, record) in collect_head_records() {
            if key.index_id != index_id || key.index_version != active_version {
                continue;
            }
            match record {
                PartitionHeadRecord::Head(head) if head.page_count > 0 => {
                    pages.push((
                        PageKey::new(
                            index_id,
                            active_version,
                            key.partition_id,
                            head.page_count - 1,
                        ),
                        head.mutable_rows,
                        head.mutable_live,
                    ));
                }
                PartitionHeadRecord::Head(_) => {}
                PartitionHeadRecord::Table(table) => {
                    let chunk_base = ((key.level - PARTITION_LEVEL_PAGE_TABLE_BASE) as usize)
                        * ENTRIES_PER_TABLE_CHUNK;
                    for (i, entry) in table.entries.iter().enumerate() {
                        pages.push((
                            PageKey::new(
                                index_id,
                                active_version,
                                key.partition_id,
                                (chunk_base + i) as u64,
                            ),
                            entry.row_count,
                            entry.live_count,
                        ));
                    }
                }
            }
        }
        pages.sort_by_key(|(k, _, _)| *k);

        let resume = cursor
            .as_ref()
            .map(|bytes| PageKey::from_bytes(Cow::Borrowed(bytes)));
        let mut page_count = 0u64;
        let mut total_rows = 0u64;
        let mut physical_live_rows = 0u64;
        let mut tombstoned_rows = 0u64;
        let mut last: Option<PageKey> = None;
        let mut exhausted = true;
        let mut processed: u32 = 0;
        for (page_key, rows, live) in &pages {
            if resume.as_ref().is_some_and(|r| *page_key <= *r) {
                continue;
            }
            if processed >= budget {
                exhausted = false;
                break;
            }
            page_count += 1;
            total_rows = total_rows.saturating_add(u64::from(*rows));
            physical_live_rows = physical_live_rows.saturating_add(u64::from(*live));
            tombstoned_rows = tombstoned_rows.saturating_add(u64::from(rows.saturating_sub(*live)));
            last = Some(*page_key);
            processed += 1;
        }
        let cursor_out = if exhausted {
            None
        } else {
            last.map(Storable::into_bytes)
        };
        Ok(VectorPartitionHealthStep {
            partial: VectorPartitionPageHealth {
                index_id,
                index_version: active_version,
                page_count,
                total_rows,
                physical_live_rows,
                tombstoned_rows,
            },
            cursor: cursor_out,
            exhausted,
        })
    }

    // --- Test-only inspection helpers ---

    #[cfg(test)]
    pub(crate) fn page_meta_for_test(
        &self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        page_id: u64,
    ) -> Option<VectorPageMeta> {
        partition_head_get(&PartitionKey::new(index_id, index_version, partition_id))?;
        let view = self
            .partition_page_metas(index_id, index_version, partition_id)
            .into_iter()
            .find(|v| v.page_id == page_id)?;
        Some(VectorPageMeta {
            slab_offset: view.slab_offset,
            row_count: view.row_count,
            live_count: view.live_count,
        })
    }

    pub(crate) fn occupied_tail(&self) -> u64 {
        self.occupied_tail
    }

    /// Number of page records of `(index_id, index_version)` across all partitions.
    #[cfg(test)]
    pub(crate) fn version_page_count(&self, index_id: u32, index_version: u64) -> usize {
        collect_head_records()
            .into_iter()
            .filter(|(k, r)| match r {
                PartitionHeadRecord::Head(h) => {
                    k.index_id == index_id && k.index_version == index_version && h.page_count > 0
                }
                _ => false,
            })
            .map(|(_, r)| match r {
                PartitionHeadRecord::Head(h) => h.page_count as usize,
                _ => 0,
            })
            .sum()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::{partition_head_get, partition_head_insert};
    use crate::records::{PartitionKey, VectorIndexDef};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{
        VectorEncoding, VectorIndexKind, VectorMetric, VectorSubject,
    };
    use ic_stable_structures::DefaultMemoryImpl;
    use ic_stable_structures::memory_manager::{MemoryId, MemoryManager};

    const SLAB_ID: MemoryId = MemoryId::new(13);

    type TestMm = MemoryManager<DefaultMemoryImpl>;

    fn fresh_mm() -> TestMm {
        MemoryManager::init(DefaultMemoryImpl::default())
    }

    /// Opens an isolated slab whose single index resolves to `d` (the geometry cross-check source).
    /// Page-chain state always lives in the shared `VECTOR_PARTITION_HEADS`; callers clear it.
    fn open(mm: &TestMm, d: &VectorIndexDef) -> VectorSlabStore {
        VectorSlabStore::from_regions(mm.get(SLAB_ID), &mut |_| Some(*d))
    }

    /// Opens an isolated slab whose index defs are unresolvable (active-generation checks fail).
    fn open_without_defs(mm: &TestMm) -> VectorSlabStore {
        VectorSlabStore::from_regions(mm.get(SLAB_ID), &mut |_| None)
    }

    /// `d = 2` F32: stride 8, pad stride 16, meta 4, single shard.
    fn def(capacity: u32) -> VectorIndexDef {
        let dims = 2u16;
        VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding: VectorEncoding::F32,
            dims,
            metric: VectorMetric::L2Squared,
            nlist: 1,
            active_index_version: 1,
            stride_bytes: VectorEncoding::F32.stride_bytes(dims),
            pad_stride_bytes: 16,
            meta_stride_bytes: 4,
            run_capacity: 1,
            max_page_bytes: 65_536,
            slots_per_page: capacity,
            levels: crate::records::LEVELS_FLAT,
            nlist_fine: 1,
            code_tier: false,
            code_stride_bytes: 0,
            rotation_seed: 0,
            eps_query_bps: 0,
            eps_fine_bps: 0,
        }
    }

    fn subject_shard(shard: u32, v: u32) -> VectorSubject {
        VectorSubject::Vertex {
            shard_id: ShardId::new(shard),
            vertex_id: v,
        }
    }

    fn subject(v: u32) -> VectorSubject {
        subject_shard(0, v)
    }

    /// Zero row-meta aux for F32 test rows (an `I8` row would carry its scale here).
    fn zaux() -> [u8; 8] {
        [0u8; 8]
    }

    fn bytes(a: f32, b: f32) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&a.to_le_bytes());
        out.extend_from_slice(&b.to_le_bytes());
        out
    }

    /// Clears the global partition-head allocator so an isolated store test is not perturbed by heads
    /// left over from another test on the same thread.
    fn clear_heads() {
        VECTOR_PARTITION_HEADS
            .with_borrow_mut(|h| h.clear())
            .expect("clear partition heads");
    }

    fn head_live_len(index_id: u32, version: u64, partition: u32) -> u64 {
        partition_head_get(&PartitionKey::new(index_id, version, partition))
            .map(|head| head.live_len)
            .unwrap_or(0)
    }

    #[test]
    fn fresh_init_writes_header_and_empty_meta() {
        let mm = fresh_mm();
        let d = def(4);
        let store = open(&mm, &d);
        assert_eq!(store.occupied_tail(), SLAB_HEADER_SIZE as u64);
        assert_eq!(store.version_page_count(1, 1), 0);
    }

    #[test]
    fn empty_initialized_reopen_does_not_trap() {
        let mm = fresh_mm();
        let d = def(4);
        open(&mm, &d);
        open(&mm, &d); // reopens cleanly
    }

    #[test]
    fn append_round_trip_and_reopen() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        let slot = store
            .append_row(7, 1, 0, &d, subject(100), &bytes(1.0, 2.0), &zaux())
            .unwrap();
        assert_eq!(slot.page_id, 0);
        assert_eq!(slot.slot, 0);

        let store = open(&mm, &d);
        let (vertex_id, vec, _) = store
            .read_row_bytes(7, slot)
            .expect("row present after reopen");
        assert_eq!(vertex_id, 100);
        // Row bytes are pad-stride wide; the payload is at the front.
        assert_eq!(&vec[..8], bytes(1.0, 2.0).as_slice());
        assert_eq!(vec.len(), d.pad_stride_bytes as usize);
    }

    #[test]
    fn append_rolls_new_page_at_capacity() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm, &d);
        let s0 = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        let s1 = store
            .append_row(1, 1, 0, &d, subject(2), &bytes(1.0, 1.0), &zaux())
            .unwrap();
        let s2 = store
            .append_row(1, 1, 0, &d, subject(3), &bytes(2.0, 2.0), &zaux())
            .unwrap();
        assert_eq!((s0.page_id, s0.slot), (0, 0));
        assert_eq!((s1.page_id, s1.slot), (0, 1));
        assert_eq!(
            (s2.page_id, s2.slot),
            (1, 0),
            "third row rolls to a new page"
        );
        assert_eq!(store.version_page_count(1, 1), 2);
        assert_eq!(head_live_len(1, 1, 0), 3);
    }

    #[test]
    fn append_run_split_across_two_shards() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(16);
        let mut store = open(&mm, &d);
        let s0 = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(1.0, 0.0), &zaux())
            .unwrap();
        let s1 = store
            .append_row(1, 1, 0, &d, subject_shard(1, 2), &bytes(2.0, 0.0), &zaux())
            .unwrap();
        let s2 = store
            .append_row(1, 1, 0, &d, subject(3), &bytes(3.0, 0.0), &zaux())
            .unwrap();

        let mut seen: Vec<(u32, u32, u32)> = Vec::new();
        store.visit_partition_pages(1, 1, 0, &mut PageScratch::new(), |slot, info, _| {
            seen.push((slot.slot, info.shard_id, info.vertex_id));
        });
        // Three rows, each its own run (shard alternates), all recovered correctly.
        assert_eq!(
            seen,
            vec![(s0.slot, 0, 1), (s1.slot, 1, 2), (s2.slot, 0, 3)]
        );
    }

    #[test]
    fn append_run_full_rolls_new_page() {
        clear_heads();
        let mm = fresh_mm();
        let mut d = def(16);
        d.run_capacity = 2; // at most 2 runs per page
        let mut store = open(&mm, &d);
        let s0 = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(1.0, 0.0), &zaux())
            .unwrap();
        let s1 = store
            .append_row(1, 1, 0, &d, subject_shard(1, 2), &bytes(2.0, 0.0), &zaux())
            .unwrap();
        // Third shard starts a new run, but run_capacity is full: roll a new page.
        let s2 = store
            .append_row(1, 1, 0, &d, subject_shard(2, 3), &bytes(3.0, 0.0), &zaux())
            .unwrap();
        assert_eq!((s0.page_id, s1.page_id), (0, 0));
        assert_eq!(s2.page_id, 1, "run table full -> new page");
        assert_eq!(s2.slot, 0);

        let mut seen: Vec<(u32, u32)> = Vec::new();
        store.visit_partition_pages(1, 1, 0, &mut PageScratch::new(), |slot, info, _| {
            seen.push((slot.page_id, info.shard_id));
        });
        assert_eq!(seen, vec![(0, 0), (0, 1), (1, 2)]);
    }

    #[test]
    fn append_rows_round_trip() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        let b1 = bytes(1.0, 2.0);
        let b2 = bytes(3.0, 4.0);
        let b3 = bytes(5.0, 6.0);
        let rows = vec![
            (subject(1), b1.as_slice(), zaux()),
            (subject(2), b2.as_slice(), zaux()),
            (subject(3), b3.as_slice(), zaux()),
        ];
        let slots = store.append_rows(1, 1, 0, &d, &rows).unwrap();
        assert_eq!(slots.len(), 3);
        assert_eq!((slots[0].page_id, slots[0].slot), (0, 0));
        assert_eq!((slots[1].page_id, slots[1].slot), (0, 1));
        assert_eq!((slots[2].page_id, slots[2].slot), (0, 2));
        assert_eq!(head_live_len(1, 1, 0), 3);

        let store = open(&mm, &d);
        for (slot, (vertex_id, expected)) in
            slots.iter().zip([(1u32, &b1), (2u32, &b2), (3u32, &b3)])
        {
            let (got, vec, _) = store
                .read_row_bytes(1, *slot)
                .expect("row present after reopen");
            assert_eq!(got, vertex_id);
            assert_eq!(&vec[..expected.len()], expected.as_slice());
            assert_eq!(vec.len(), d.pad_stride_bytes as usize);
        }
    }

    #[test]
    fn append_rows_rolls_new_page_at_capacity() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm, &d);
        let b1 = bytes(0.0, 0.0);
        let b2 = bytes(1.0, 1.0);
        let b3 = bytes(2.0, 2.0);
        let rows = vec![
            (subject(1), b1.as_slice(), zaux()),
            (subject(2), b2.as_slice(), zaux()),
            (subject(3), b3.as_slice(), zaux()),
        ];
        let slots = store.append_rows(1, 1, 0, &d, &rows).unwrap();
        assert_eq!((slots[0].page_id, slots[0].slot), (0, 0));
        assert_eq!((slots[1].page_id, slots[1].slot), (0, 1));
        assert_eq!(
            (slots[2].page_id, slots[2].slot),
            (1, 0),
            "third row rolls a new page"
        );
        assert_eq!(store.version_page_count(1, 1), 2);
        assert_eq!(head_live_len(1, 1, 0), 3);
    }

    #[test]
    fn append_rows_run_split_across_two_shards() {
        clear_heads();
        let mm = fresh_mm();
        let mut d = def(16);
        d.run_capacity = 16; // several runs per page so shard splits stay on one page
        let mut store = open(&mm, &d);
        let b1 = bytes(1.0, 0.0);
        let b2 = bytes(2.0, 0.0);
        let b3 = bytes(3.0, 0.0);
        let rows = vec![
            (subject(1), b1.as_slice(), zaux()),
            (subject_shard(1, 2), b2.as_slice(), zaux()),
            (subject(3), b3.as_slice(), zaux()),
        ];
        let slots = store.append_rows(1, 1, 0, &d, &rows).unwrap();
        assert_eq!(slots[0].page_id, 0);

        let mut seen: Vec<(u32, u32, u32)> = Vec::new();
        store.visit_partition_pages(1, 1, 0, &mut PageScratch::new(), |slot, info, _| {
            seen.push((slot.slot, info.shard_id, info.vertex_id));
        });
        assert_eq!(seen, vec![(0, 0, 1), (1, 1, 2), (2, 0, 3)]);
    }

    #[test]
    fn append_rows_run_full_rolls_new_page() {
        clear_heads();
        let mm = fresh_mm();
        let mut d = def(16);
        d.run_capacity = 2; // at most 2 runs per page
        let mut store = open(&mm, &d);
        let b1 = bytes(1.0, 0.0);
        let b2 = bytes(2.0, 0.0);
        let b3 = bytes(3.0, 0.0);
        let rows = vec![
            (subject(1), b1.as_slice(), zaux()),
            (subject_shard(1, 2), b2.as_slice(), zaux()),
            (subject_shard(2, 3), b3.as_slice(), zaux()),
        ];
        let slots = store.append_rows(1, 1, 0, &d, &rows).unwrap();
        assert_eq!((slots[0].page_id, slots[1].page_id), (0, 0));
        assert_eq!(slots[2].page_id, 1, "run table full -> new page");
        assert_eq!(slots[2].slot, 0);

        let mut seen: Vec<(u32, u32)> = Vec::new();
        store.visit_partition_pages(1, 1, 0, &mut PageScratch::new(), |slot, info, _| {
            seen.push((slot.page_id, info.shard_id));
        });
        assert_eq!(seen, vec![(0, 0), (0, 1), (1, 2)]);
    }

    #[test]
    fn append_rows_empty_is_noop() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        let slots = store.append_rows(1, 1, 0, &d, &[]).unwrap();
        assert!(slots.is_empty());
        assert_eq!(head_live_len(1, 1, 0), 0);
        assert_eq!(store.version_page_count(1, 1), 0);
    }

    #[test]
    fn append_rows_grow_failure_leaves_consistent_state() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        let b1 = bytes(1.0, 2.0);
        // The very next batch fails before any directory mutation (mirrors `append_row`'s seam).
        arm_append_failure(0);
        let rows = vec![(subject(1), b1.as_slice(), zaux())];
        let err = store.append_rows(1, 1, 0, &d, &rows).unwrap_err();
        assert_eq!(err, VectorCanisterError::StableGrowFailed);
        assert_eq!(head_live_len(1, 1, 0), 0);
        assert_eq!(store.version_page_count(1, 1), 0);
    }

    #[test]
    fn append_rows_second_page_reserve_failure_is_atomic() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm, &d);
        store
            .append_row(
                1,
                1,
                0,
                &d,
                subject_shard(7, 700),
                &bytes(7.0, 70.0),
                &zaux(),
            )
            .unwrap();

        let head_key = PartitionKey::new(1, 1, 0);
        let head_before = VECTOR_PARTITION_HEADS
            .with_borrow(|heads| heads.get(&head_key))
            .expect("partition head get")
            .expect("seeded partition head");
        let tail_before = store.occupied_tail();
        let page_count_before = store.version_page_count(1, 1);
        let mut visited_before = Vec::new();
        store.visit_partition_pages(1, 1, 0, &mut PageScratch::new(), |slot, info, vector| {
            visited_before.push((slot, *info, vector.to_vec()));
        });

        let b1 = bytes(1.0, 2.0);
        let b2 = bytes(3.0, 4.0);
        let b3 = bytes(5.0, 6.0);
        let rows = vec![
            (subject(1), b1.as_slice(), zaux()),
            (subject(2), b2.as_slice(), zaux()),
            (subject(3), b3.as_slice(), zaux()),
        ];
        // Capacity two plans two fresh pages. Reserve page one, then fail page two.
        arm_append_rows_reserve_failure(1);
        let err = store.append_rows(1, 1, 0, &d, &rows).unwrap_err();
        assert_eq!(err, VectorCanisterError::StableGrowFailed);

        let store = open(&mm, &d);
        assert_eq!(store.occupied_tail(), tail_before);
        assert_eq!(store.version_page_count(1, 1), page_count_before);
        assert_eq!(
            VECTOR_PARTITION_HEADS
                .with_borrow(|heads| heads.get(&head_key))
                .expect("partition head get"),
            Some(head_before),
            "all partition-head fields, including live_len, remain unchanged"
        );

        let mut visited_after = Vec::new();
        store.visit_partition_pages(1, 1, 0, &mut PageScratch::new(), |slot, info, vector| {
            visited_after.push((slot, *info, vector.to_vec()));
        });
        assert_eq!(
            visited_after, visited_before,
            "live visitation is unchanged"
        );
    }

    #[test]
    fn pad_region_is_zeroed_so_rows_stay_finite() {
        // Rows are stored pad-stride (16) but the payload is only 8 bytes; the trailing pad must be
        // deterministically zero so a decode that checks all components stays finite.
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(1.0, 2.0), &zaux())
            .unwrap();
        let slot = SlotRef {
            index_version: 1,
            partition_id: 0,
            page_id: 0,
            slot: 0,
        };
        let (_, vec, _) = store.read_row_bytes(1, slot).expect("row present");
        assert_eq!(vec.len(), 16);
        assert_eq!(&vec[8..], &[0u8; 8], "pad region is zeroed");
        // A reset + reappend over the same slab bytes still yields a finite row. The head allocator
        // is cleared alongside the directory (as canister (re)install does).
        store.reset();
        clear_heads();
        store
            .append_row(1, 1, 0, &d, subject(2), &bytes(3.0, 4.0), &zaux())
            .unwrap();
        let (_, vec, _) = store
            .read_row_bytes(1, slot)
            .expect("row present after reset");
        assert_eq!(&vec[8..], &[0u8; 8], "pad stays zeroed across slab reuse");
    }

    #[test]
    fn tombstone_is_idempotent_across_counters() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        let slot = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        store
            .append_row(1, 1, 0, &d, subject(2), &bytes(1.0, 1.0), &zaux())
            .unwrap();
        assert_eq!(store.page_meta_for_test(1, 1, 0, 0).unwrap().live_count, 2);

        assert!(store.tombstone_row(1, slot));
        assert!(!store.tombstone_row(1, slot), "idempotent");
        let meta = store.page_meta_for_test(1, 1, 0, 0).unwrap();
        assert_eq!(meta.live_count, 1);
        assert_eq!(meta.row_count, 2);
        // Tombstoned rows are derived: row_count − live_count.
        assert_eq!(meta.row_count - meta.live_count, 1);
        assert_eq!(head_live_len(1, 1, 0), 1);
        assert!(
            store.read_row_bytes(1, slot).is_none(),
            "tombstoned row unreadable"
        );
    }

    #[test]
    fn read_row_bytes_rejects_out_of_range_and_tombstoned() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        let slot = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        let oob = SlotRef { slot: 9, ..slot };
        assert!(store.read_row_bytes(1, oob).is_none());
        store.tombstone_row(1, slot);
        assert!(store.read_row_bytes(1, slot).is_none());
    }

    #[test]
    fn load_page_loads_distinct_pages_distinctly_and_rejects_missing() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2); // 2 rows per page
        let mut store = open(&mm, &d);
        let s0 = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(1.0, 0.0), &zaux())
            .unwrap();
        let s1 = store
            .append_row(1, 1, 0, &d, subject(2), &bytes(2.0, 0.0), &zaux())
            .unwrap();
        let s2 = store
            .append_row(1, 1, 0, &d, subject(3), &bytes(3.0, 0.0), &zaux())
            .unwrap();
        assert_eq!((s0.page_id, s1.page_id), (0, 0));
        assert_eq!(s2.page_id, 1, "third row rolls to page 1");

        let mut scratch = PageScratch::new();
        assert!(store.load_page(PageKey::new(1, 1, 0, 0), &mut scratch));
        assert_eq!(scratch.row_count(), 2);
        assert_eq!(scratch.row_info(0).vertex_id, 1);
        assert_eq!(&scratch.vec_slice(0)[..8], bytes(1.0, 0.0).as_slice());
        assert!(!scratch.is_tombstoned(0));

        // A distinct page decodes distinct content, not the previously loaded page.
        assert!(store.load_page(PageKey::new(1, 1, 0, 1), &mut scratch));
        assert_eq!(scratch.row_count(), 1);
        assert_eq!(
            scratch.row_info(0).vertex_id,
            3,
            "page 1 row 0 is vertex 3, not page-0's vertex 1"
        );

        // Missing page / invalid page id returns false.
        assert!(!store.load_page(PageKey::new(1, 1, 0, 99), &mut scratch));
    }

    #[test]
    fn visit_partition_pages_yields_live_rows_only() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        let s0 = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        store
            .append_row(1, 1, 0, &d, subject(2), &bytes(1.0, 1.0), &zaux())
            .unwrap();
        store.tombstone_row(1, s0);
        let mut seen: Vec<(u32, u32)> = Vec::new();
        store.visit_partition_pages(1, 1, 0, &mut PageScratch::new(), |slot, info, vec| {
            assert_eq!(vec.len(), d.pad_stride_bytes as usize);
            seen.push((slot.slot, info.vertex_id));
        });
        assert_eq!(seen, vec![(1, 2)], "tombstoned slot 0 is skipped");
    }

    /// `d = 2` F32 with the maximum 8 aux bytes (meta stride 12), like an `I8` row carrying its
    /// per-row scale — exercises aux propagation through the decode paths.
    fn def_with_aux(capacity: u32) -> VectorIndexDef {
        let mut d = def(capacity);
        d.meta_stride_bytes = 12;
        d
    }

    /// Deterministic LCG so the randomized run layouts below are reproducible.
    fn next_rand(state: &mut u64) -> u64 {
        *state = state
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        *state >> 33
    }

    #[test]
    fn prefix_sum_shard_of_matches_linear_run_walk_on_all_slots() {
        clear_heads();
        let mm = fresh_mm();
        let mut d = def(32);
        d.run_capacity = 8; // several runs per page so shard splits stay on one page
        let mut store = open(&mm, &d);
        // Pseudo-random shard sequence: runs of varying length, rolling pages on run-table fills.
        let mut rng = 0x5EED_1234u64;
        for i in 0..200u32 {
            let shard = (next_rand(&mut rng) % 4) as u32;
            store
                .append_row(
                    1,
                    1,
                    0,
                    &d,
                    subject_shard(shard, i),
                    &bytes(i as f32, 0.0),
                    &zaux(),
                )
                .unwrap();
        }

        let mut total_runs = 0u32;
        let mut page_id = 0u64;
        while store.page_meta_for_test(1, 1, 0, page_id).is_some() {
            let mut scratch = PageScratch::new();
            assert!(
                store.load_page(PageKey::new(1, 1, 0, page_id), &mut scratch),
                "page {page_id} loads"
            );
            total_runs += scratch.run_count;
            // The reference is the former implementation: a linear walk over the loaded page's
            // run table. Every written slot must resolve identically through the prefix sums.
            let table = &scratch.buf[scratch.layout.run_table_range()];
            let linear_shard_of = |slot: u32| -> u32 {
                let mut pos = 0u32;
                for r in 0..scratch.run_count {
                    let entry = read_run(table, r as usize).expect("run entry");
                    if slot < pos + entry.run_len {
                        return entry.shard_id;
                    }
                    pos += entry.run_len;
                }
                panic!("slot {slot} not covered by run table (page {page_id})");
            };
            for slot in 0..scratch.row_count() {
                assert_eq!(
                    scratch.shard_of(slot),
                    linear_shard_of(slot),
                    "page {page_id} slot {slot}"
                );
                assert_eq!(
                    scratch.row_info(slot).shard_id,
                    linear_shard_of(slot),
                    "row_info shard agrees on page {page_id} slot {slot}"
                );
            }
            page_id += 1;
        }
        assert!(page_id > 1, "fixture spans several pages");
        assert!(
            total_runs >= 16,
            "fixture spans enough runs ({total_runs}) to exercise multi-run binary search"
        );
    }

    #[test]
    fn single_decode_matches_separated_decode_on_every_slot() {
        clear_heads();
        let mm = fresh_mm();
        let d = def_with_aux(4);
        let mut store = open(&mm, &d);
        let aux = |seed: u8| [seed, seed.wrapping_mul(7), 3, 4, 5, 6, 7, 8];
        let s0 = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(1.0, 0.0), &aux(1))
            .unwrap();
        store
            .append_row(1, 1, 0, &d, subject_shard(2, 2), &bytes(2.0, 0.0), &aux(2))
            .unwrap();
        let s2 = store
            .append_row(1, 1, 0, &d, subject(3), &bytes(3.0, 0.0), &aux(3))
            .unwrap();
        store.tombstone_row(1, s0);
        store.tombstone_row(1, s2);

        let mut page_id = 0u64;
        while store.page_meta_for_test(1, 1, 0, page_id).is_some() {
            let mut scratch = PageScratch::new();
            assert!(store.load_page(PageKey::new(1, 1, 0, page_id), &mut scratch));
            for slot in 0..scratch.row_count() {
                // The separated form (the pre-Slice-3 pattern): one decode for liveness, another
                // for identity. The single decode must agree on every field for every slot.
                let separated = if scratch.is_tombstoned(slot) {
                    None
                } else {
                    Some(scratch.row_info(slot))
                };
                let meta = scratch.row_meta(slot);
                let combined = (!meta.vertex.is_tombstone()).then(|| RowInfo {
                    shard_id: scratch.shard_of(slot),
                    vertex_id: meta.vertex.vertex_id(),
                    aux: meta.aux,
                });
                assert_eq!(
                    scratch.live_row_info(slot),
                    separated,
                    "live view matches the separated decode on page {page_id} slot {slot}"
                );
                assert_eq!(combined, separated);
            }
            page_id += 1;
        }
        assert!(page_id >= 1);
    }

    #[test]
    fn point_read_bytes_equal_the_page_scan_decode() {
        clear_heads();
        let mm = fresh_mm();
        let mut d = def_with_aux(4);
        d.run_capacity = 4; // shard changes stay on one page so slots are predictable
        let mut store = open(&mm, &d);
        let aux = |seed: u8| [seed, 2, 3, 4, 5, 6, 7, seed];
        let s0 = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(1.0, 0.0), &aux(1))
            .unwrap();
        store
            .append_row(1, 1, 0, &d, subject_shard(3, 2), &bytes(2.0, 0.0), &aux(2))
            .unwrap();
        let s2 = store
            .append_row(1, 1, 0, &d, subject(3), &bytes(3.0, 0.0), &aux(3))
            .unwrap();
        store.tombstone_row(1, s2);

        let mut page_id = 0u64;
        while store.page_meta_for_test(1, 1, 0, page_id).is_some() {
            let mut scratch = PageScratch::new();
            assert!(store.load_page(PageKey::new(1, 1, 0, page_id), &mut scratch));
            for slot in 0..scratch.row_count() {
                let point = store.read_row_bytes(
                    1,
                    SlotRef {
                        index_version: 1,
                        partition_id: 0,
                        page_id: page_id as u32,
                        slot,
                    },
                );
                match point {
                    Some((vertex, vec, point_aux)) => {
                        let info = scratch.row_info(slot);
                        assert_eq!(vertex, info.vertex_id, "page {page_id} slot {slot}");
                        assert_eq!(point_aux, info.aux);
                        // Byte equivalence with the scan-path slice, including the guaranteed
                        // zero trailing pad.
                        assert_eq!(vec, scratch.vec_slice(slot));
                    }
                    None => assert!(
                        scratch.is_tombstoned(slot),
                        "only tombstoned slots are unreadable (page {page_id} slot {slot})"
                    ),
                }
            }
            page_id += 1;
        }
        assert!(s0.slot == 0 && s2.slot == 2, "fixture sanity");
    }

    #[test]
    fn reserved_page_image_is_zero_beyond_written_rows() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        // Dirty the slab region first: the reset rewinds the tail without shrinking stable memory,
        // so the re-reserved page reuses bytes holding stale row content. The deterministic zero
        // fill at reservation must win over those stale bytes.
        for v in 0..4u32 {
            store
                .append_row(
                    1,
                    1,
                    0,
                    &d,
                    subject(v),
                    &bytes(f32::from_bits(0x7F7F_FFFF), f32::from_bits(0x7F7F_FFFF)),
                    &[v as u8; 8],
                )
                .unwrap();
        }
        store.reset();
        clear_heads();
        store
            .append_row(1, 1, 0, &d, subject(9), &bytes(1.0, 2.0), &zaux())
            .unwrap();

        let meta = store.page_meta_for_test(1, 1, 0, 0).unwrap();
        let header = store.read_page_header(meta.slab_offset);
        let layout = PageLayout::new(&header).expect("valid page layout");
        let mut image = vec![0u8; layout.page_len()];
        mm.get(SLAB_ID).read(meta.slab_offset, &mut image);

        // The written row's payload is present at the front of its vector slot...
        let row0 = layout.vector_range_at(0);
        assert_eq!(
            &image[row0.start..row0.start + 8],
            bytes(1.0, 2.0).as_slice()
        );
        // ...and every byte beyond it is deterministically zero: row 0's pad, the unwritten
        // rows' meta + vector regions, and the unused run-table entries.
        assert!(
            image[row0.start + 8..].iter().all(|b| *b == 0),
            "vector region beyond the written payload stays zero"
        );
        let run_table = layout.run_table_range();
        assert!(
            image[run_table.start + RunEntry::SIZE..run_table.end]
                .iter()
                .all(|b| *b == 0),
            "unused run-table entries stay zero"
        );
        let meta_table = layout.row_meta_range();
        assert!(
            image[meta_table.start + 4..meta_table.end]
                .iter()
                .all(|b| *b == 0),
            "row-meta region beyond slot 0 stays zero"
        );
    }

    #[test]
    fn drop_version_pages_deletes_meta_and_keeps_occupied_tail() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        let tail_before = store.occupied_tail();
        let result = store.drop_version_pages(1, 1, None, 100);
        assert!(result.exhausted);
        assert_eq!(result.cursor, None);
        assert_eq!(store.version_page_count(1, 1), 0);
        assert_eq!(store.occupied_tail(), tail_before, "tail is not rewound");
    }

    #[test]
    fn drop_version_pages_is_cursor_resumable() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm, &d);
        // Two partitions of one page each; a budget of one page drains exactly one partition
        // per call (whole-partition granularity), resuming from the returned cursor.
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(1.0, 0.0), &zaux())
            .unwrap();
        store
            .append_row(1, 1, 1, &d, subject(2), &bytes(2.0, 0.0), &zaux())
            .unwrap();
        let first = store.drop_version_pages(1, 1, None, 1);
        assert!(!first.exhausted);
        assert!(first.cursor.is_some());
        let second = store.drop_version_pages(1, 1, first.cursor, 1);
        assert!(second.exhausted);
        assert_eq!(store.version_page_count(1, 1), 0);
    }

    #[test]
    #[should_panic(expected = "corrupt/unsupported slab header")]
    fn reopen_traps_on_bad_slab_magic() {
        let mm = fresh_mm();
        let d = def(4);
        open(&mm, &d);
        mm.get(SLAB_ID).write(0, b"XXX".as_slice());
        open(&mm, &d); // fail closed on a corrupt slab header
    }

    #[test]
    #[should_panic(expected = "empty slab region with non-empty partition heads")]
    fn reopen_traps_on_empty_slab_with_nonempty_meta() {
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        // Wipe the slab to all zero but keep the heads: partial layout must trap.
        let slab = mm.get(SLAB_ID);
        let zeros = vec![0u8; slab.size() as usize * 65_536];
        slab.write(0, &zeros);
        open(&mm, &d);
    }

    #[test]
    #[should_panic(expected = "row_count")]
    fn reopen_traps_on_counts_exceeding_capacity() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        // Corrupt the head mirror: mutable rows beyond the page capacity.
        let head_key = PartitionKey::new(1, 1, 0);
        let mut head = crate::facade::stable::partition_head_get(&head_key).unwrap();
        head.mutable_rows = 99;
        let _ = partition_head_insert(head_key, head);
        open(&mm, &d);
    }

    #[test]
    #[should_panic(expected = "live_count")]
    fn reopen_traps_on_live_count_exceeding_row_count() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        // Corrupt the head mirror: live beyond rows (derived tombstones would go negative).
        let head_key = PartitionKey::new(1, 1, 0);
        let mut head = crate::facade::stable::partition_head_get(&head_key).unwrap();
        head.mutable_live = 2;
        let _ = partition_head_insert(head_key, head);
        open(&mm, &d);
    }

    #[test]
    #[should_panic(expected = "active page header disagrees with index def")]
    fn reopen_traps_on_header_geometry_disagreeing_with_def() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        // The def is the only geometry owner: a header written with different geometry than the
        // reopened def must trap (here a doubled pad stride).
        let mut drifted = d;
        drifted.pad_stride_bytes = 32;
        open(&mm, &drifted);
    }

    #[test]
    #[should_panic(expected = "missing index 1 definition")]
    fn reopen_traps_on_missing_index_def() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        // A page whose index has no resolvable def cannot be validated: fail closed.
        open_without_defs(&mm);
    }

    #[test]
    fn stats_fresh_store_is_empty() {
        let mm = fresh_mm();
        let d = def(4);
        let store = open(&mm, &d);
        let stats = store.stats_for_index(None);
        assert_eq!(stats.scope.page_count, 0);
        assert_eq!(stats.slab.referenced_page_bytes_global, 0);
    }

    #[test]
    fn stats_append_grows_pages_and_referenced_bytes() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm, &d);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(1.0, 1.0), &zaux())
            .unwrap();
        store
            .append_row(1, 1, 0, &d, subject(2), &bytes(1.0, 1.0), &zaux())
            .unwrap();
        let stats = store.stats_for_index(None);
        assert_eq!(stats.scope.page_count, 1);
        assert_eq!(stats.scope.row_count, 2);
        assert_eq!(stats.scope.physical_live_row_count, 2);
        // Referenced bytes = the page's uniform block footprint (Slice 8): one block for one
        // allocated page, sharing a unit with `occupied_tail`.
        assert_eq!(stats.slab.referenced_page_bytes_global, BLOCK_LEN);
    }

    #[test]
    fn stats_tombstone_moves_live_to_tombstone_without_touching_bytes() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        let s0 = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        store
            .append_row(1, 1, 0, &d, subject(2), &bytes(1.0, 1.0), &zaux())
            .unwrap();
        let before = store.stats_for_index(None);
        store.tombstone_row(1, s0);
        let after = store.stats_for_index(None);
        assert_eq!(
            before.slab.referenced_page_bytes_global,
            after.slab.referenced_page_bytes_global
        );
        assert_eq!(after.scope.physical_live_row_count, 1);
        assert_eq!(after.scope.tombstone_row_count, 1);
    }

    #[test]
    fn health_step_counts_rows_live_and_tombstones() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm, &d);
        let s0 = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        store
            .append_row(1, 1, 0, &d, subject(2), &bytes(1.0, 1.0), &zaux())
            .unwrap();
        store.tombstone_row(1, s0);
        let step = store
            .partition_page_health_step(1, 1, None, 100)
            .expect("health step");
        assert!(step.exhausted);
        assert_eq!(step.partial.page_count, 1);
        assert_eq!(step.partial.total_rows, 2);
        assert_eq!(step.partial.physical_live_rows, 1);
        assert_eq!(step.partial.tombstoned_rows, 1);
    }

    #[test]
    fn health_step_empty_partition_is_valid() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let store = open(&mm, &d);
        let step = store
            .partition_page_health_step(1, 1, None, 100)
            .expect("empty health step");
        assert!(step.exhausted);
        assert_eq!(step.partial.page_count, 0);
    }

    // --- Slab compaction (plan 0278) ---

    /// Captures every live row of `(index_id, version)` as `(page_id, slot, vertex_id, bytes)`.
    fn live_rows(
        store: &VectorSlabStore,
        index_id: u32,
        version: u64,
    ) -> Vec<(u32, u32, u32, Vec<u8>)> {
        let mut out = Vec::new();
        store.visit_partition_pages(
            index_id,
            version,
            0,
            &mut PageScratch::new(),
            |slot, info, vec| {
                out.push((slot.page_id, slot.slot, info.vertex_id, vec.to_vec()));
            },
        );
        out
    }

    /// Drives one compaction to finalize from `(write_cursor, scan_cursor)`, one bounded step at
    /// a time through `store`.
    fn drive_compaction_from(
        store: &mut VectorSlabStore,
        write_cursor: u64,
        scan_cursor: Option<PageKey>,
        range_end: u64,
        max_entries: u32,
        max_bytes: u64,
    ) -> SlabCompactStepOutcome {
        let mut write_cursor = write_cursor;
        let mut scan_cursor = scan_cursor;
        loop {
            let outcome = store
                .compact_step(write_cursor, range_end, scan_cursor, max_entries, max_bytes)
                .expect("compact step");
            write_cursor = outcome.write_cursor;
            scan_cursor = outcome.scan_cursor;
            if outcome.finalized {
                return outcome;
            }
        }
    }

    /// Drives one fresh compaction (from the slab header) to finalize.
    fn drive_compaction(
        store: &mut VectorSlabStore,
        range_end: u64,
        max_entries: u32,
        max_bytes: u64,
    ) -> SlabCompactStepOutcome {
        drive_compaction_from(
            store,
            SLAB_HEADER_SIZE as u64,
            None,
            range_end,
            max_entries,
            max_bytes,
        )
    }

    /// Interleaves two versions so each version's pages alternate on the slab; returns nothing.
    /// 2 pages per version at capacity 2 (rows `100..104` for v1, `200..204` for v2).
    fn seed_two_interleaved_versions(store: &mut VectorSlabStore, d: &VectorIndexDef) {
        let mut v1_rows = 0u32;
        let mut v2_rows = 0u32;
        for i in 0..8u32 {
            let (version, vid) = if i % 2 == 0 {
                v1_rows += 1;
                (1u64, 100 + v1_rows - 1)
            } else {
                v2_rows += 1;
                (2u64, 200 + v2_rows - 1)
            };
            store
                .append_row(
                    1,
                    version,
                    0,
                    d,
                    subject(vid),
                    &bytes(i as f32, 0.0),
                    &zaux(),
                )
                .unwrap();
        }
    }

    #[test]
    fn compact_reclaims_dropped_version_bytes_to_a_dense_prefix() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2); // 2 rows per page
        let mut store = open(&mm, &d);
        seed_two_interleaved_versions(&mut store, &d);
        assert_eq!(store.version_page_count(1, 1), 2);
        assert_eq!(store.version_page_count(1, 2), 2);
        let live_before = live_rows(&store, 1, 2);

        // GC/cleanup drain of v1 frees its blocks into the free list; they sit between the live
        // v2 pages until compaction reclaims them.
        let progress = store.drop_version_pages(1, 1, None, 100);
        assert!(progress.exhausted);
        let before = store.stats_for_index(None);
        assert_eq!(before.scope.page_count, 2, "only v2 remains");
        assert_eq!(
            before.slab.estimated_unreferenced_bytes,
            2 * BLOCK_LEN,
            "dropped v1 blocks are unreferenced"
        );
        assert_eq!(
            store.occupied_tail(),
            SLAB_HEADER_SIZE as u64 + 4 * BLOCK_LEN
        );

        let range_end = store.occupied_tail();
        let outcome = drive_compaction(&mut store, range_end, 1, u64::MAX);
        assert!(outcome.finalized);
        assert_eq!(
            outcome.write_cursor,
            SLAB_HEADER_SIZE as u64 + 2 * BLOCK_LEN,
            "the tail rewinds once to the dense prefix end"
        );
        assert_eq!(
            store.occupied_tail(),
            SLAB_HEADER_SIZE as u64 + 2 * BLOCK_LEN
        );

        let after = store.stats_for_index(None);
        assert_eq!(after.slab.referenced_page_bytes_global, 2 * BLOCK_LEN);
        assert_eq!(
            after.slab.estimated_unreferenced_bytes, 0,
            "reclaim drops the freelist to empty on a drained store"
        );
        assert_eq!(after.slab.occupied_tail_bytes, store.occupied_tail());
        assert_eq!(live_rows(&store, 1, 2), live_before, "live rows are intact");

        // The rewind and every moved seq survive reopen.
        let reopened = open(&mm, &d);
        assert_eq!(
            reopened.occupied_tail(),
            SLAB_HEADER_SIZE as u64 + 2 * BLOCK_LEN
        );
        assert_eq!(live_rows(&reopened, 1, 2), live_before);
    }

    #[test]
    fn compact_all_live_store_is_a_noop_move_keeping_the_tail() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm, &d);
        for v in 0..6u32 {
            store
                .append_row(1, 1, 0, &d, subject(v), &bytes(v as f32, 0.0), &zaux())
                .unwrap();
        }
        let live_before = live_rows(&store, 1, 1);
        let range_end = store.occupied_tail();

        let outcome = drive_compaction(&mut store, range_end, 10, u64::MAX);
        assert!(outcome.finalized);
        assert_eq!(outcome.write_cursor, range_end, "nothing to reclaim");
        assert_eq!(live_rows(&store, 1, 1), live_before);

        let reopened = open(&mm, &d);
        assert_eq!(reopened.occupied_tail(), range_end);
    }

    #[test]
    fn compact_interleaves_appends_and_meta_drops_safely() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm, &d);
        seed_two_interleaved_versions(&mut store, &d);
        // GC drains v1 before the compaction starts; its blocks become free-listed holes.
        assert!(store.drop_version_pages(1, 1, None, 100).exhausted);
        let range_end = store.occupied_tail(); // header + 4 * BLOCK

        // Step once (moves the lowest-source live page), then append a post-start row. With
        // free-list reuse the new page may land in a hole inside the snapshot range or above it;
        // either way compaction treats it as an ordinary live page and its rows must survive.
        let first = store
            .compact_step(SLAB_HEADER_SIZE as u64, range_end, None, 1, u64::MAX)
            .unwrap();
        assert!(!first.finalized);
        assert_eq!(first.pages_moved, 1);
        let appended = store
            .append_row(1, 3, 0, &d, subject(900), &bytes(9.0, 9.0), &zaux())
            .unwrap();
        assert!(
            store
                .partition_page_metas(1, 3, 0)
                .into_iter()
                .find(|v| v.page_id == u64::from(appended.page_id))
                .expect("post-start page meta")
                .slab_offset
                >= SLAB_HEADER_SIZE as u64,
            "post-start page resolves to a real block"
        );

        // A version teardown lands mid-compaction (the rebuild `Cleaning` case): whole
        // partitions drain atomically — here the entire v2 generation disappears — and the lap
        // must skip the vanished records instead of stalling or moving ghosts.
        assert!(store.tombstone_row(
            1,
            SlotRef {
                index_version: 2,
                partition_id: 0,
                page_id: 1,
                slot: 0,
            }
        ));
        assert!(store.tombstone_row(
            1,
            SlotRef {
                index_version: 2,
                partition_id: 0,
                page_id: 1,
                slot: 1,
            }
        ));
        assert!(store.drop_version_pages(1, 2, None, 100).exhausted);
        assert_eq!(
            store.version_page_count(1, 2),
            0,
            "drained version leaves no heads"
        );

        let range_for_finish = range_end.max(store.occupied_tail());
        let outcome = drive_compaction_from(
            &mut store,
            first.write_cursor,
            first.scan_cursor,
            range_for_finish,
            1,
            u64::MAX,
        );
        assert!(outcome.finalized);
        assert_eq!(store.occupied_tail(), outcome.write_cursor);

        // Every surviving live row reads back correctly after finalize + reopen: the post-start
        // append is intact and the reopened store validates the whole chain.
        let reopened = open(&mm, &d);
        assert_eq!(reopened.occupied_tail(), outcome.write_cursor);
        assert_eq!(
            reopened.read_row_bytes(1, appended).map(|(v, _, _)| v),
            Some(900)
        );
    }

    #[test]
    fn compact_resume_determinism_two_runs_byte_identical() {
        let d = def(2);
        let build_fixture = || {
            clear_heads();
            let mm = fresh_mm();
            let mut store = open(&mm, &d);
            seed_two_interleaved_versions(&mut store, &d);
            assert!(store.drop_version_pages(1, 1, None, 100).exhausted);
            (mm, store)
        };

        // Run A drives straight through. Its slab prefix is captured before the shared heads are
        // reset for run B (page-chain state lives in the global collection).
        let budgets = (1u32, BLOCK_LEN); // one candidate and at most one block per step
        let (mm_a, mut store_a) = build_fixture();
        let range_a = store_a.occupied_tail();
        let final_a = drive_compaction(&mut store_a, range_a, budgets.0, budgets.1);
        assert_eq!(
            final_a.write_cursor,
            SLAB_HEADER_SIZE as u64 + 2 * BLOCK_LEN
        );
        let live_a = live_rows(&store_a, 1, 2);
        let len_a = final_a.write_cursor as usize;
        let mut raw_a = vec![0u8; len_a];
        mm_a.get(SLAB_ID).read(0, &mut raw_a);
        drop(store_a);

        // Run B replays on a fresh fixture with identical inputs and simulates a crash by
        // reopening the store after every non-finalizing step, resuming from persisted cursors.
        let (mm_b, store_b) = build_fixture();
        let range_b = store_b.occupied_tail();
        assert_eq!(range_a, range_b);
        drop(store_b);
        let mut store_b = open(&mm_b, &d);
        let mut write_cursor = SLAB_HEADER_SIZE as u64;
        let mut scan_cursor = None;
        loop {
            let outcome = store_b
                .compact_step(write_cursor, range_b, scan_cursor, budgets.0, budgets.1)
                .expect("compact step");
            write_cursor = outcome.write_cursor;
            scan_cursor = outcome.scan_cursor;
            if outcome.finalized {
                break;
            }
            store_b = open(&mm_b, &d);
        }

        assert_eq!(final_a.write_cursor, write_cursor, "equal final tails");
        let mut raw_b = vec![0u8; len_a];
        mm_b.get(SLAB_ID).read(0, &mut raw_b);
        assert_eq!(raw_b, raw_a, "byte-identical dense prefixes");
        assert_eq!(live_rows(&store_b, 1, 2), live_a);
    }

    #[test]
    #[should_panic(expected = "sits inside the reclaimed gap")]
    fn compact_finalize_fails_closed_when_live_span_stranded_in_gap() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm, &d);
        for v in 0..4u32 {
            store
                .append_row(1, 1, 0, &d, subject(v), &bytes(v as f32, 0.0), &zaux())
                .unwrap();
        }
        // Wrong-implementation guard: rewinding over live spans must fail closed instead of
        // corrupting every read above the new tail.
        store.compact_finalize(SLAB_HEADER_SIZE as u64, store.occupied_tail());
    }

    // --- Slice 8 contracts ---

    /// Every page's physical base derives arithmetically from its block sequence:
    /// `SLAB_HEADER_SIZE + seq × BLOCK_LEN`, and `occupied_tail` tracks whole blocks.
    #[test]
    fn arithmetic_offsets_are_consistent_across_pages() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm, &d);
        let mut slots = Vec::new();
        for v in 0..5u32 {
            slots.push(
                store
                    .append_row(1, 1, 0, &d, subject(v), &bytes(v as f32, 0.0), &zaux())
                    .unwrap(),
            );
        }
        // 5 rows at capacity 2 -> pages 0..2 with distinct blocks.
        for (positional, expected_base) in [
            (0u64, SLAB_HEADER_SIZE as u64),
            (1, SLAB_HEADER_SIZE as u64 + BLOCK_LEN),
            (2, SLAB_HEADER_SIZE as u64 + 2 * BLOCK_LEN),
        ] {
            let meta = store.page_meta_for_test(1, 1, 0, positional).unwrap();
            assert_eq!(meta.slab_offset, expected_base);
            assert_eq!(slots[positional as usize].slot, positional as u32 % 2);
        }
        assert_eq!(
            store.occupied_tail(),
            SLAB_HEADER_SIZE as u64 + 3 * BLOCK_LEN,
            "tail tracks whole committed blocks"
        );
        // The mapping survives reopen.
        let reopened = open(&mm, &d);
        for (positional, expected_base) in [
            (0u64, SLAB_HEADER_SIZE as u64),
            (1, SLAB_HEADER_SIZE as u64 + BLOCK_LEN),
            (2, SLAB_HEADER_SIZE as u64 + 2 * BLOCK_LEN),
        ] {
            assert_eq!(
                reopened
                    .page_meta_for_test(1, 1, 0, positional)
                    .unwrap()
                    .slab_offset,
                expected_base
            );
        }
    }

    /// A freed version's blocks are reused (free side first) before the tail grows.
    #[test]
    fn freelist_reuses_freed_blocks_before_tail_growth() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm, &d);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(1.0, 0.0), &zaux())
            .unwrap();
        store
            .append_row(1, 2, 0, &d, subject(2), &bytes(2.0, 0.0), &zaux())
            .unwrap();
        assert_eq!(
            store.occupied_tail(),
            SLAB_HEADER_SIZE as u64 + 2 * BLOCK_LEN
        );

        // Drain v1 atomically: its block joins the free list.
        assert!(store.drop_version_pages(1, 1, None, 100).exhausted);

        // A brand-new generation's first page takes the freed block 0 without tail growth.
        store
            .append_row(1, 3, 0, &d, subject(3), &bytes(3.0, 0.0), &zaux())
            .unwrap();
        let head = crate::facade::stable::partition_head_get(&PartitionKey::new(1, 3, 0)).unwrap();
        assert_eq!(head.mutable_seq, 0, "new page reuses the freed block");
        assert_eq!(
            store.occupied_tail(),
            SLAB_HEADER_SIZE as u64 + 2 * BLOCK_LEN
        );
        assert_eq!(
            store
                .read_row_bytes(
                    1,
                    SlotRef {
                        index_version: 3,
                        partition_id: 0,
                        page_id: 0,
                        slot: 0,
                    }
                )
                .map(|(v, _, _)| v),
            Some(3),
            "the reused block serves reads"
        );
    }

    /// The scalar bound `M` never decreases on tombstone (conservative, monotone) and persists.
    #[test]
    fn block_bound_is_monotone_under_tombstone() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(8); // one page holds all rows
        let mut store = open(&mm, &d);
        let s_small = store
            .append_row(1, 1, 0, &d, subject(1), &bytes(1.0, 1.0), &zaux())
            .unwrap(); // ‖·‖ = √2
        store
            .append_row(1, 1, 0, &d, subject(2), &bytes(6.0, 6.0), &zaux())
            .unwrap(); // ‖·‖ = √72 ≈ 8.49
        store
            .append_row(1, 1, 0, &d, subject(3), &bytes(3.0, 3.0), &zaux())
            .unwrap();

        let bound = |store: &VectorSlabStore| store.partition_page_metas(1, 1, 0)[0].block_bound;
        let big = f32::sqrt(72.0);
        assert!((bound(&store) - big).abs() < 1e-3, "M = max‖row‖");

        // Tombstoning the biggest row must NOT lower the page's bound.
        assert!(store.tombstone_row(1, s_small)); // tombstone the small one first (idempotence path)
        assert!((bound(&store) - big).abs() < 1e-3);
        let reopened = open(&mm, &d);
        assert!((bound(&reopened) - big).abs() < 1e-3, "bound persists");
    }

    /// The intrusive free-block chain pops in LIFO order and persists across reopen, so torn-down
    /// generations are reused (free side first) before the tail grows.
    #[test]
    fn freelist_intrusive_chain_is_lifo_and_persists() {
        clear_heads();
        let mm = fresh_mm();
        let mut d = def(1); // one row per page -> each append takes its own block
        d.run_capacity = 1;
        let mut store = open(&mm, &d);
        for v in 0..3u32 {
            store
                .append_row(1, 1, 0, &d, subject(v), &bytes(v as f32, 0.0), &zaux())
                .unwrap();
        }
        assert_eq!(
            store.occupied_tail(),
            SLAB_HEADER_SIZE as u64 + 3 * BLOCK_LEN
        );

        // Teardown frees blocks {0, 1, 2}; the chain head is the last push.
        assert!(store.drop_version_pages(1, 1, None, 100).exhausted);

        // Reopen: the anchor survives (durable).
        let mut store = open(&mm, &d);
        for v in 10..13u32 {
            store
                .append_row(1, 2, 0, &d, subject(v), &bytes(v as f32, 0.0), &zaux())
                .unwrap();
        }
        // LIFO: the three appends pop blocks 2, 1, 0 — no tail growth at any point.
        assert_eq!(
            store.occupied_tail(),
            SLAB_HEADER_SIZE as u64 + 3 * BLOCK_LEN
        );
        let metas = store.partition_page_metas(1, 2, 0);
        let seqs: Vec<u32> = metas
            .iter()
            .map(|m| ((m.slab_offset - 32) / BLOCK_LEN) as u32)
            .collect();
        assert_eq!(seqs, vec![2, 1, 0], "LIFO free-block reuse");

        // The fourth append has an empty chain and must bump the tail to a fresh block.
        store
            .append_row(1, 2, 0, &d, subject(99), &bytes(9.0, 9.0), &zaux())
            .unwrap();
        assert_eq!(
            store.occupied_tail(),
            SLAB_HEADER_SIZE as u64 + 4 * BLOCK_LEN
        );
    }
}
