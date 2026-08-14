//! Vector-index-owned composite slab page store (ADR 0064 §7 two-table format).
//!
//! Replaces the former ADR 0032 structure-of-arrays page store with the
//! `ic-stable-vector-page-store` two-table page format:
//!
//! ```text
//! [PageHeader] [run_table × run_capacity] [row_meta × capacity] [vector_bytes × capacity]
//! ```
//!
//! Two stable regions are opened as one composite store:
//!
//! - `VECTOR_PAGE_META` (`BTreeMap<PageKey, VectorPageMeta>`, MemoryId 10) — the page directory.
//! - `VECTOR_ROW_SLAB` (raw stable memory, MemoryId 13) — the physical row bytes behind a
//!   `VSL`/version-1 slab header.
//!
//! Each row stores only its packed 30-bit [`VertexPayload`] (vertex id + bit-31 tombstone); the shard
//! is shared across contiguous rows via the run table, so a shard is recorded once per run, not per
//! row. `vector_bytes` rows are `pad_stride_bytes` wide (16-byte aligned for SIMD), and the trailing
//! pad region is zero-filled so scoring kernels never observe non-finite garbage. Rows are
//! **write-once at tail positions**: superseded rows are tombstoned (bit 31), never rewritten, so
//! freshness is validated positionally (subject-map slot matches the scanned position) rather than by
//! a row-carried `vector_id`/`generation`.
//!
//! Allocation is tail-only; a page reserves its full span on creation. Page cleanup deletes
//! `VECTOR_PAGE_META` entries only — slab bytes are left in place as dead space, so `occupied_tail`
//! may exceed the highest referenced page end, and reopen validation allows that.
//! `VECTOR_PARTITION_HEADS` is the per-partition allocator/counter owner and lives outside this
//! composite store. The format lineage restarts at version 1 (breaking; dev data wiped); the discarded
//! ASCII-magic format is rejected fail-closed.

use super::memory::{Memory, StablePageMetaMap, init_page_meta, init_row_slab};
use crate::facade::stable::VECTOR_PARTITION_HEADS;
use crate::records::{PageKey, PartitionKey, SlotRef, VectorIndexDef};
use gleaph_graph_kernel::vector_index::{
    VectorCanisterError, VectorPartitionHealthStep, VectorPartitionPageHealth,
    VectorSlabGlobalStats, VectorSlabScopeStats, VectorSlabStats, VectorSlabStatsStep,
    VectorSlabVersionStats, VectorSubject,
};
use ic_stable_structures::Memory as _;
use ic_stable_structures::storable::{Bound, Storable};
use ic_stable_vector_page_store::{
    PAGE_HEADER_SIZE, PageHeader, PageLayout, RowMeta, RunEntry, SLAB_HEADER_SIZE, Slab,
    SlabHeader, VertexPayload, read_run,
};
use std::borrow::Cow;
use std::ops::Bound as RangeBound;

#[cfg(all(feature = "canbench", target_family = "wasm"))]
use canbench_rs::bench_scope;

/// WASM stable-memory page size in bytes.
const WASM_PAGE_SIZE: u64 = 65_536;

/// Per-step page-meta budget cap for [`VectorSlabStore::stats_step`] (mirrors `MAX_REBUILD_STEP_WORK`).
const MAX_SLAB_STATS_STEP_PAGES: u32 = 20_000;

/// Encoded length of a [`PageKey`] (its fixed `Storable` bound). A caller-supplied
/// [`VectorSlabStore::stats_step`] cursor must be exactly this many bytes.
const PAGE_KEY_LEN: usize = 24;

/// One page in a fully prevalidated [`VectorSlabStore::append_rows`] batch plan. Reserved page
/// headers remain unreachable until the batch's single fallible partition-head update succeeds.
struct BatchPagePlan {
    page_id: u64,
    slab_offset: u64,
    row_start: usize,
    row_end: usize,
}

/// Per-page directory metadata for the slab page store. Carries page-physical facts; the page header
/// on the slab is the format source of truth and the directory is cross-checked against it on reopen.
/// `row_stride` is the stored `pad_stride_bytes` (16-aligned), `meta_stride` is 4 | 8 | 12, and
/// `run_capacity` is the run-table width (`min(owned_shards, MAX_RUNS)`, frozen at def creation).
/// Fixed-width `Storable` (36 bytes) to keep the directory value cheap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VectorPageMeta {
    pub slab_offset: u64,
    pub capacity: u32,
    pub row_count: u32,
    pub live_count: u32,
    pub row_stride: u32,
    pub tombstone_count: u32,
    pub meta_stride: u32,
    pub run_capacity: u32,
}

impl VectorPageMeta {
    fn to_array(self) -> [u8; 36] {
        let mut out = [0u8; 36];
        out[0..8].copy_from_slice(&self.slab_offset.to_le_bytes());
        out[8..12].copy_from_slice(&self.capacity.to_le_bytes());
        out[12..16].copy_from_slice(&self.row_count.to_le_bytes());
        out[16..20].copy_from_slice(&self.live_count.to_le_bytes());
        out[20..24].copy_from_slice(&self.row_stride.to_le_bytes());
        out[24..28].copy_from_slice(&self.tombstone_count.to_le_bytes());
        out[28..32].copy_from_slice(&self.meta_stride.to_le_bytes());
        out[32..36].copy_from_slice(&self.run_capacity.to_le_bytes());
        out
    }

    fn from_array(raw: [u8; 36]) -> Self {
        Self {
            slab_offset: u64::from_le_bytes(raw[0..8].try_into().expect("meta field")),
            capacity: u32::from_le_bytes(raw[8..12].try_into().expect("meta field")),
            row_count: u32::from_le_bytes(raw[12..16].try_into().expect("meta field")),
            live_count: u32::from_le_bytes(raw[16..20].try_into().expect("meta field")),
            row_stride: u32::from_le_bytes(raw[20..24].try_into().expect("meta field")),
            tombstone_count: u32::from_le_bytes(raw[24..28].try_into().expect("meta field")),
            meta_stride: u32::from_le_bytes(raw[28..32].try_into().expect("meta field")),
            run_capacity: u32::from_le_bytes(raw[32..36].try_into().expect("meta field")),
        }
    }
}

impl Storable for VectorPageMeta {
    const BOUND: Bound = Bound::Bounded {
        max_size: 36,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_array()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_array())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0u8; 36];
        raw.copy_from_slice(bytes.as_ref());
        Self::from_array(raw)
    }
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

/// Shared per-page accumulator for the slab-stats family ([`VectorSlabStore::stats_for_index`] and
/// [`VectorSlabStore::stats_step`]), so both derive identical math from one source of truth.
///
/// `referenced_global` always sums every observed page span (the slab is one global allocation
/// domain); the `scope_*` counters and `versions` breakdown only count pages within `index_id`
/// (`None` = all indexes). Page-meta entries are iterated in `PageKey` order, so each
/// `(index_id, index_version)` group is contiguous *within a single pass*: `current` accumulates the
/// open group and flushes on key change. A bounded step may end mid-group; the client merge sums
/// version entries by `(index_id, index_version)` key, so a split group reconciles after merging.
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

    fn observe(&mut self, key: &PageKey, m: &VectorPageMeta) {
        let bytes = page_span_bytes(m).unwrap_or(0);
        self.referenced_global = self.referenced_global.saturating_add(bytes);

        if self.index_id.is_some_and(|id| key.index_id != id) {
            return;
        }
        self.scope_referenced = self.scope_referenced.saturating_add(bytes);
        self.scope_pages = self.scope_pages.saturating_add(1);
        self.scope_rows = self.scope_rows.saturating_add(m.row_count as u64);
        self.scope_live = self.scope_live.saturating_add(m.live_count as u64);
        self.scope_tombstones = self
            .scope_tombstones
            .saturating_add(m.tombstone_count as u64);

        match self.current.as_mut() {
            Some(v) if v.index_id == key.index_id && v.index_version == key.index_version => {
                v.page_count = v.page_count.saturating_add(1);
                v.row_count = v.row_count.saturating_add(m.row_count as u64);
                v.physical_live_row_count = v
                    .physical_live_row_count
                    .saturating_add(m.live_count as u64);
                v.tombstone_row_count = v
                    .tombstone_row_count
                    .saturating_add(m.tombstone_count as u64);
                v.referenced_page_bytes = v.referenced_page_bytes.saturating_add(bytes);
            }
            _ => {
                if let Some(v) = self.current.take() {
                    self.versions.push(v);
                }
                self.current = Some(VectorSlabVersionStats {
                    index_id: key.index_id,
                    index_version: key.index_version,
                    page_count: 1,
                    row_count: m.row_count as u64,
                    physical_live_row_count: m.live_count as u64,
                    tombstone_row_count: m.tombstone_count as u64,
                    referenced_page_bytes: bytes,
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
pub(crate) struct PageScratch {
    buf: Vec<u8>,
    layout: PageLayout,
    run_count: u32,
    row_count: u32,
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
        }
    }

    /// Bulk-reads one whole page into the scratch buffer.
    fn load(&mut self, slab: &Memory, meta: &VectorPageMeta, header: &PageHeader) {
        let layout = PageLayout::new(header).expect("valid page layout");
        self.buf.resize(layout.page_len(), 0);
        slab.read(meta.slab_offset, &mut self.buf[..layout.page_len()]);
        self.layout = layout;
        self.run_count = header.run_count;
        self.row_count = meta.row_count;
    }

    /// Number of written rows in the loaded page; slots `>= row_count` are uninitialized and must be
    /// skipped (the same guard `read_row_bytes` applies).
    pub(crate) fn row_count(&self) -> u32 {
        self.row_count
    }

    pub(crate) fn is_tombstoned(&self, slot: u32) -> bool {
        let r = self.layout.row_meta_range_at(slot);
        let meta = RowMeta::from_bytes(&self.buf[r.start..r.end], self.layout.meta_stride())
            .expect("decode row meta");
        meta.vertex.is_tombstone()
    }

    pub(crate) fn row_info(&self, slot: u32) -> RowInfo {
        let r = self.layout.row_meta_range_at(slot);
        let meta = RowMeta::from_bytes(&self.buf[r.start..r.end], self.layout.meta_stride())
            .expect("decode row meta");
        RowInfo {
            shard_id: self.shard_of(slot),
            vertex_id: meta.vertex.vertex_id(),
            aux: meta.aux,
        }
    }

    /// Resolves the shard of `slot` by walking the run table's cumulative lengths.
    fn shard_of(&self, slot: u32) -> u32 {
        let table = &self.buf[self.layout.run_table_range()];
        let mut pos = 0u32;
        for i in 0..self.run_count {
            let entry = read_run(table, i as usize).expect("run entry");
            if slot < pos + entry.run_len {
                return entry.shard_id;
            }
            pos += entry.run_len;
        }
        panic!(
            "slot {slot} not covered by run table (run_count {})",
            self.run_count
        );
    }

    pub(crate) fn vec_slice(&self, slot: u32) -> &[u8] {
        let r = self.layout.vector_range_at(slot);
        &self.buf[r.start..r.end]
    }
}

/// The composite slab page store: `VECTOR_PAGE_META` directory + raw `VECTOR_ROW_SLAB` region.
pub(crate) struct VectorSlabStore {
    meta: StablePageMetaMap,
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

/// Exact on-slab byte span of one page, `None` on invalid geometry/overflow.
fn page_span_bytes(m: &VectorPageMeta) -> Option<u64> {
    let header = PageHeader::new(m.capacity, m.row_stride, m.meta_stride, m.run_capacity).ok()?;
    PageLayout::new(&header).ok().map(|l| l.page_len() as u64)
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

impl VectorSlabStore {
    /// Opens both regions as one composite store, validating the reopen matrix (ADR 0064 §7
    /// invariant). Traps (fails closed) on any partial/corrupt layout.
    pub(crate) fn init() -> Self {
        Self::from_regions(init_page_meta(), init_row_slab())
    }

    /// Opens a store over already-resolved regions. The production path uses [`Self::init`]; tests
    /// pass regions from an isolated `MemoryManager` to exercise the reopen matrix in isolation.
    fn from_regions(meta: StablePageMetaMap, slab: Memory) -> Self {
        let occupied_tail = Self::open(&slab, &meta);
        Self {
            meta,
            slab,
            occupied_tail,
        }
    }

    /// Composite open. Freshness is keyed on raw slab size/magic, not the directory: an all-zero slab
    /// is fresh (and must pair with an empty directory), a non-empty slab must carry a valid
    /// `VSL`/version-1 header, and each directory entry must cross-check its page header and span.
    fn open(slab: &Memory, meta: &StablePageMetaMap) -> u64 {
        if slab.size() == 0 || slab_header_bytes_are_zero(slab) {
            assert!(
                meta.is_empty(),
                "vector slab: empty slab region with non-empty page meta (partial layout)"
            );
        }
        let occupied_tail = Slab::open_or_init(slab)
            .expect("vector slab: corrupt/unsupported slab header")
            .occupied_tail();

        for entry in meta.iter() {
            let m = entry.value();
            assert!(
                m.row_count <= m.capacity,
                "vector slab: page meta row_count {} exceeds capacity {} (corrupt directory)",
                m.row_count,
                m.capacity
            );
            assert!(
                m.live_count as u64 + m.tombstone_count as u64 <= m.row_count as u64,
                "vector slab: page meta live_count {} + tombstone_count {} exceeds row_count {} \
                 (corrupt directory)",
                m.live_count,
                m.tombstone_count,
                m.row_count
            );
            let header = read_page_header_at(slab, m.slab_offset);
            assert!(
                header.capacity == m.capacity
                    && header.row_stride == m.row_stride
                    && header.meta_stride == m.meta_stride
                    && header.run_capacity == m.run_capacity,
                "vector slab: page header geometry disagrees with directory at offset {}",
                m.slab_offset
            );
            let layout = PageLayout::new(&header).expect("vector slab: page span overflow");
            let end = m
                .slab_offset
                .checked_add(layout.page_len() as u64)
                .expect("page span overflow");
            assert!(
                m.slab_offset >= SLAB_HEADER_SIZE as u64 && end <= occupied_tail,
                "vector slab: page meta span [{}, {end}) outside [header, occupied_tail={occupied_tail})",
                m.slab_offset
            );
        }
        occupied_tail
    }

    /// Resets the store to empty-initialized (canister (re)install). Clears the directory and
    /// rewinds the slab tail to the header; slab pages are not shrunk (stable memory cannot shrink),
    /// the bytes are reused on subsequent appends.
    #[cfg(any(test, feature = "canbench"))]
    pub(crate) fn reset(&mut self) {
        self.meta.clear_new();
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

    /// Reserves and initializes a fresh page at `base`, writing its page header. Fallible on slab
    /// `grow`; must run before any directory mutation. Row/run-table bytes are written per-append
    /// (write-once), so only the header is pre-written here.
    fn reserve_page(
        &mut self,
        base: u64,
        capacity: u32,
        row_stride: u32,
        meta_stride: u32,
        run_capacity: u32,
    ) -> Result<PageHeader, VectorCanisterError> {
        let header = PageHeader::new(capacity, row_stride, meta_stride, run_capacity)
            .map_err(|_| VectorCanisterError::InvalidPageCapacity)?;
        let layout =
            PageLayout::new(&header).map_err(|_| VectorCanisterError::InvalidPageCapacity)?;
        let end = base
            .checked_add(layout.page_len() as u64)
            .expect("slab offset overflow");
        grow_to_at_least(&self.slab, end)?;
        self.slab.write(base, &header.to_bytes());
        Ok(header)
    }

    /// Writes one row's run-table entry and packed row meta + pad-zeroed vector bytes at `slot` of the
    /// page at `base`. The page region is already reserved/grown, so this is infallible.
    fn write_row(
        &self,
        base: u64,
        layout: &PageLayout,
        slot: u32,
        payload: VertexPayload,
        bytes: &[u8],
        aux: &[u8; 8],
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
        // Zero the trailing pad region so `decode_f32`/`vector_is_finite` never observe non-finite
        // stale slab bytes for a readable row.
        let pad = layout.vector_stride() - bytes.len();
        if pad > 0 {
            let zeros = vec![0u8; pad];
            self.slab.write(vec_start + bytes.len() as u64, &zeros);
        }
    }

    /// Writes the run entry for the append landing at `slot` of the page at `base`: either extends the
    /// open run (rewriting its `run_len`), starts a new run (rewriting the page header `run_count`),
    /// or, for a fresh page, creates run 0. `run-full` was already ruled out by the caller's roll
    /// check, so a new run never exceeds `run_capacity`.
    fn write_run_for_append(
        &self,
        base: u64,
        layout: &PageLayout,
        header: &PageHeader,
        slot: u32,
        shard: u32,
        run_capacity: u32,
    ) {
        if slot == 0 {
            debug_assert_eq!(header.run_count, 0, "fresh page starts with no runs");
            write_run_at(&self.slab, base, layout, 0, RunEntry::new(shard, 1));
            write_page_header_run_count(&self.slab, base, header, 1);
        } else {
            let last_index = header.run_count - 1;
            let last = read_run_at(&self.slab, base, layout, last_index);
            if last.shard_id == shard {
                write_run_at(
                    &self.slab,
                    base,
                    layout,
                    last_index,
                    RunEntry::new(shard, last.run_len + 1),
                );
            } else {
                debug_assert!(
                    header.run_count < run_capacity,
                    "new run would exceed run_capacity"
                );
                let idx = header.run_count;
                write_run_at(&self.slab, base, layout, idx, RunEntry::new(shard, 1));
                write_page_header_run_count(&self.slab, base, header, header.run_count + 1);
            }
        }
    }

    /// True when appending `shard` to the mutable page `m` would start a new run while the page's run
    /// table is already full (so the row must roll to a fresh page instead).
    fn run_roll_required(&self, m: &VectorPageMeta, shard: u32, run_capacity: u32) -> bool {
        if m.row_count == 0 {
            return false;
        }
        let header = self.read_page_header(m.slab_offset);
        let layout = PageLayout::new(&header).expect("valid page layout");
        let last = read_run_at(&self.slab, m.slab_offset, &layout, header.run_count - 1);
        last.shard_id != shard && header.run_count >= run_capacity
    }

    /// Appends a vector row into the partition's page chain, rolling a new page when the mutable page
    /// is full **or** its run table would overflow (shard change with `run_count == run_capacity`).
    /// Write-then-commit: slab grow + writes happen before any `VECTOR_PAGE_META` /
    /// `VECTOR_PARTITION_HEADS` mutation, and the head update is last, so a failed grow cannot leave a
    /// head/meta pointing at unwritten bytes.
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
        let meta_stride = def.meta_stride_bytes;
        let run_capacity = def.run_capacity;
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

        let head_key = PartitionKey::new(index_id, index_version, partition_id);
        let mut head = {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_head_get");
            VECTOR_PARTITION_HEADS
                .with_borrow(|h| h.get(&head_key))
                .unwrap_or_default()
        };

        let need_new_page = if head.page_count == 0 {
            true
        } else {
            let mutable_key =
                PageKey::new(index_id, index_version, partition_id, head.mutable_page);
            let m = {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("append_meta_get");
                self.meta
                    .get(&mutable_key)
                    .expect("mutable page meta present")
            };
            m.row_count >= m.capacity || self.run_roll_required(&m, shard, run_capacity)
        };

        let (page_id, mut meta, header) = if need_new_page {
            let page_id = head.next_page_id;
            let slab_offset = self.occupied_tail;
            // Fallible slab grow + page-header init BEFORE any directory mutation.
            let header = {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("append_reserve_page");
                self.reserve_page(slab_offset, capacity, row_stride, meta_stride, run_capacity)?
            };
            (
                page_id,
                VectorPageMeta {
                    slab_offset,
                    capacity,
                    row_count: 0,
                    live_count: 0,
                    row_stride,
                    tombstone_count: 0,
                    meta_stride,
                    run_capacity,
                },
                header,
            )
        } else {
            let page_id = head.mutable_page;
            let mutable_key = PageKey::new(index_id, index_version, partition_id, page_id);
            let meta = {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("append_meta_get");
                self.meta.get(&mutable_key).expect("mutable page meta")
            };
            let header = {
                #[cfg(all(feature = "canbench", target_family = "wasm"))]
                let _scope = bench_scope("append_header_read");
                self.read_page_header(meta.slab_offset)
            };
            (page_id, meta, header)
        };
        let layout = PageLayout::new(&header).expect("valid page layout");
        debug_assert_eq!(meta.row_stride, row_stride, "page stride mismatch");

        let slot = meta.row_count;
        let page_key = PageKey::new(index_id, index_version, partition_id, page_id);
        // Infallible page writes (the page region is already reserved/grown).
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_run_write");
            self.write_run_for_append(
                meta.slab_offset,
                &layout,
                &header,
                slot,
                shard,
                run_capacity,
            );
        }
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_row_write");
            self.write_row(meta.slab_offset, &layout, slot, payload, bytes, aux);
        }

        // Commit directory: occupied_tail (slab header) -> page meta -> partition head (last).
        if need_new_page {
            self.set_occupied_tail(meta.slab_offset + layout.page_len() as u64);
        }
        meta.row_count += 1;
        meta.live_count += 1;
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_meta_insert");
            self.meta.insert(page_key, meta);
        }

        if need_new_page {
            if head.page_count == 0 {
                head.first_page = page_id;
            }
            head.mutable_page = page_id;
            head.page_count += 1;
            head.next_page_id = page_id + 1;
        }
        head.live_len += 1;
        {
            #[cfg(all(feature = "canbench", target_family = "wasm"))]
            let _scope = bench_scope("append_head_insert");
            VECTOR_PARTITION_HEADS
                .with_borrow_mut(|h| h.insert(head_key, head))
                .map(|_| ())
                .map_err(|_| VectorCanisterError::StableGrowFailed)?;
        }

        Ok(SlotRef {
            index_version: index_version as u32,
            partition_id,
            page_id: page_id as u32,
            slot,
        })
    }

    /// Appends a run of rows into one partition's page chain, rolling a new page when the mutable
    /// page is full or its run table would overflow (the same rules as [`Self::append_row`]). Used by
    /// the rebuild shadow build (`building_step`), which appends a whole partition's batch at once;
    /// the dual-write upsert path keeps using the single-row `append_row`.
    ///
    /// Unlike `append_row`, the whole batch is prevalidated and planned before reserving every page.
    /// Reserved headers remain unreachable until the final partition head has been inserted. That
    /// single fallible directory update precedes all row, page-meta, and occupied-tail publication,
    /// so every returned error leaves the previously visible batch state unchanged.
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
        let meta_stride = def.meta_stride_bytes;
        let run_capacity = def.run_capacity;

        // Validate every row and the page geometry before reserving any slab bytes. The validated
        // payloads also keep the write phase free of returned errors.
        let page_header = PageHeader::new(capacity, row_stride, meta_stride, run_capacity)
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
        let head = VECTOR_PARTITION_HEADS
            .with_borrow(|h| h.get(&head_key))
            .unwrap_or_default();

        // Plan every page boundary and run-table roll without touching stable state.
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

        let page_len = layout.page_len() as u64;
        let mut plans = Vec::with_capacity(boundaries.len());
        let mut page_id = head.next_page_id;
        let mut slab_offset = self.occupied_tail;
        for (row_start, row_end) in boundaries {
            plans.push(BatchPagePlan {
                page_id,
                slab_offset,
                row_start,
                row_end,
            });
            page_id = page_id.checked_add(1).expect("page id overflow");
            slab_offset = slab_offset
                .checked_add(page_len)
                .expect("slab offset overflow");
        }

        // Reserve every planned page before publishing the batch. Headers written beyond the
        // current occupied tail are unreachable and will be overwritten by a later retry on error.
        for plan in &plans {
            #[cfg(test)]
            if take_injected_append_rows_reserve_failure() {
                return Err(VectorCanisterError::StableGrowFailed);
            }
            self.reserve_page(
                plan.slab_offset,
                capacity,
                row_stride,
                meta_stride,
                run_capacity,
            )?;
        }

        let mut final_head = head;
        if final_head.page_count == 0 {
            final_head.first_page = plans[0].page_id;
        }
        final_head.mutable_page = plans.last().expect("non-empty batch plan").page_id;
        final_head.page_count = final_head
            .page_count
            .checked_add(plans.len() as u64)
            .expect("partition page count overflow");
        final_head.live_len = final_head
            .live_len
            .checked_add(rows.len() as u64)
            .expect("partition live length overflow");
        final_head.next_page_id = page_id;

        // Perform the last returned-error path before any row, page-meta, or occupied-tail publish.
        VECTOR_PARTITION_HEADS
            .with_borrow_mut(|h| h.insert(head_key, final_head))
            .map(|_| ())
            .map_err(|_| VectorCanisterError::StableGrowFailed)?;

        let mut page_metas = Vec::with_capacity(plans.len());
        for plan in &plans {
            let mut meta = VectorPageMeta {
                slab_offset: plan.slab_offset,
                capacity,
                row_count: 0,
                live_count: 0,
                row_stride,
                tombstone_count: 0,
                meta_stride,
                run_capacity,
            };
            let mut header = page_header;
            let mut last_shard = None;
            let mut last_run_len = 0u32;
            for row_index in plan.row_start..plan.row_end {
                let (shard, payload) = validated[row_index];
                let (_, bytes, aux) = rows[row_index];
                let slot = meta.row_count;
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
                self.write_row(plan.slab_offset, &layout, slot, payload, bytes, &aux);
                meta.row_count += 1;
                meta.live_count += 1;
                slots.push(SlotRef {
                    index_version: index_version as u32,
                    partition_id,
                    page_id: plan.page_id as u32,
                    slot,
                });
            }
            page_metas.push((
                PageKey::new(index_id, index_version, partition_id, plan.page_id),
                meta,
            ));
        }

        self.set_occupied_tail(slab_offset);
        for (key, meta) in page_metas {
            self.meta.insert(key, meta);
        }

        Ok(slots)
    }

    /// Marks a slot tombstoned, owning all live/tombstone accounting idempotently: on the
    /// live->tombstoned transition it sets the payload tombstone bit and adjusts
    /// `VectorPageMeta.live_count` / `tombstone_count` and the row's `VECTOR_PARTITION_HEADS.live_len`
    /// exactly once. Returns `true` only when the row changed (was previously live and in range).
    pub(crate) fn tombstone_row(&mut self, index_id: u32, slot: SlotRef) -> bool {
        let page_key = PageKey::new(
            index_id,
            slot.index_version as u64,
            slot.partition_id,
            slot.page_id as u64,
        );
        let Some(mut meta) = self.meta.get(&page_key) else {
            return false;
        };
        if slot.slot >= meta.row_count {
            return false;
        }
        let header = self.read_page_header(meta.slab_offset);
        let layout = PageLayout::new(&header).expect("valid page layout");
        let meta_range = layout.row_meta_range_at(slot.slot);
        let mut buf = vec![0u8; layout.meta_stride()];
        self.slab
            .read(meta.slab_offset + meta_range.start as u64, &mut buf);
        let mut row_meta =
            RowMeta::from_bytes(&buf, layout.meta_stride()).expect("decode row meta");
        if row_meta.vertex.is_tombstone() {
            return false;
        }
        row_meta.vertex = row_meta.vertex.tombstoned();
        row_meta
            .write_into(&mut buf, layout.meta_stride())
            .expect("encode row meta");
        self.slab
            .write(meta.slab_offset + meta_range.start as u64, &buf);
        meta.tombstone_count += 1;
        meta.live_count = meta.live_count.saturating_sub(1);
        self.meta.insert(page_key, meta);

        let head_key = PartitionKey::new(index_id, slot.index_version as u64, slot.partition_id);
        VECTOR_PARTITION_HEADS.with_borrow_mut(|h| {
            if let Some(mut head) = h.get(&head_key) {
                head.live_len = head.live_len.saturating_sub(1);
                h.insert(head_key, head).expect("tombstone head insert");
            }
        });
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
        let page_key = PageKey::new(
            index_id,
            slot.index_version as u64,
            slot.partition_id,
            slot.page_id as u64,
        );
        let meta = self.meta.get(&page_key)?;
        if slot.slot >= meta.row_count {
            return None;
        }
        let header = self.read_page_header(meta.slab_offset);
        let layout = PageLayout::new(&header).ok()?;
        let meta_range = layout.row_meta_range_at(slot.slot);
        let mut buf = vec![0u8; layout.meta_stride()];
        self.slab
            .read(meta.slab_offset + meta_range.start as u64, &mut buf);
        let row_meta = RowMeta::from_bytes(&buf, layout.meta_stride()).ok()?;
        if row_meta.vertex.is_tombstone() {
            return None;
        }
        let vec_start = meta.slab_offset + layout.vector_range_at(slot.slot).start as u64;
        let mut out = vec![0u8; layout.vector_stride()];
        self.slab.read(vec_start, &mut out);
        Some((row_meta.vertex.vertex_id(), out, row_meta.aux))
    }

    /// Bulk-reads one specific page into `scratch`. Returns `false` when the page is absent from the
    /// directory or its header is invalid, so the caller skips the page group (the same fail path as
    /// `read_row_bytes`'s `None`). `scratch` is reused across pages, so a scan pays one bulk read per
    /// distinct page instead of one per row.
    pub(crate) fn load_page(&self, page_key: PageKey, scratch: &mut PageScratch) -> bool {
        let Some(meta) = self.meta.get(&page_key) else {
            return false;
        };
        let header = self.read_page_header(meta.slab_offset);
        if PageLayout::new(&header).is_err() {
            return false;
        }
        scratch.load(&self.slab, &meta, &header);
        true
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
        let lower = PageKey::new(index_id, index_version, partition_id, 0);
        for entry in self
            .meta
            .range((RangeBound::Included(lower), RangeBound::Unbounded))
        {
            let key = entry.key();
            if key.index_id != index_id
                || key.index_version != index_version
                || key.partition_id != partition_id
            {
                break; // partition-major order: past this partition's pages.
            }
            let meta = entry.value();
            let header = self.read_page_header(meta.slab_offset);
            scratch.load(&self.slab, &meta, &header);
            for slot in 0..meta.row_count {
                if scratch.is_tombstoned(slot) {
                    continue;
                }
                let info = scratch.row_info(slot);
                let slot_ref = SlotRef {
                    index_version: index_version as u32,
                    partition_id,
                    page_id: key.page_id as u32,
                    slot,
                };
                visitor(slot_ref, &info, scratch.vec_slice(slot));
            }
        }
    }

    /// Bounded, cursor-resumable delete of `VECTOR_PAGE_META` entries for `(index_id, version)`.
    /// No slab tail rewind in this slice: dropped pages leave their slab bytes as dead space and
    /// `occupied_tail` is unchanged (reopen validation allows that).
    pub(crate) fn drop_version_pages(
        &mut self,
        index_id: u32,
        version: u64,
        cursor: Option<Vec<u8>>,
        budget: u32,
    ) -> DropProgress {
        let mut to_remove: Vec<PageKey> = Vec::new();
        let mut last: Option<PageKey> = None;
        let mut exhausted = true;
        {
            let lower = match &cursor {
                None => RangeBound::Included(PageKey::new(index_id, version, 0, 0)),
                Some(bytes) => RangeBound::Excluded(PageKey::from_bytes(Cow::Borrowed(bytes))),
            };
            for entry in self.meta.range((lower, RangeBound::Unbounded)) {
                let key = entry.key();
                if key.index_id != index_id || key.index_version != version {
                    break;
                }
                if to_remove.len() as u32 >= budget {
                    exhausted = false;
                    break;
                }
                to_remove.push(*key);
                last = Some(*key);
            }
        }
        for key in &to_remove {
            self.meta.remove(key);
        }
        let cursor = if exhausted {
            None
        } else {
            last.map(Storable::into_bytes)
        };
        DropProgress { cursor, exhausted }
    }

    /// Derived, admin-only slab-space observability. Computes whole-slab physical facts plus logical
    /// counters scoped to `index_id` (`None` = all indexes), in a single pass over `VECTOR_PAGE_META`.
    ///
    /// **Unbounded**: it scans every page-meta entry (even for `Some(index_id)`, because the global
    /// dead-space estimate needs the whole slab). Reads only page meta + the slab header/size — never
    /// row bytes. `physical_live_row_count` is `VectorPageMeta.live_count` (physical non-tombstone),
    /// not subject-freshness.
    pub(crate) fn stats_for_index(&self, index_id: Option<u32>) -> VectorSlabStats {
        let mut acc = SlabStatsAcc::new(index_id);
        for entry in self.meta.iter() {
            let m = entry.value();
            acc.observe(entry.key(), &m);
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
    /// IC-safe `admin_vector_slab_stats_step` query. Scans at most `max_pages` `VECTOR_PAGE_META`
    /// entries (clamped to `1..=MAX_SLAB_STATS_STEP_PAGES`), returning an opaque `PageKey` cursor to
    /// resume from. Callers repeat until `exhausted` and merge the additive partials client-side.
    ///
    /// Reads only page meta + the slab header/size — never row bytes. The `cursor` is **external
    /// caller input**, so a malformed (wrong-length) cursor is rejected with
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
        {
            let lower = match &cursor {
                None => RangeBound::Unbounded,
                Some(bytes) => RangeBound::Excluded(PageKey::from_bytes(Cow::Borrowed(bytes))),
            };
            for entry in self.meta.range((lower, RangeBound::Unbounded)) {
                if processed >= budget {
                    exhausted = false;
                    break;
                }
                let key = entry.key();
                let m = entry.value();
                acc.observe(key, &m);
                last = Some(*key);
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
            partial: VectorSlabStats {
                slab: VectorSlabGlobalStats {
                    slab_size_bytes,
                    occupied_tail_bytes: self.occupied_tail,
                    referenced_page_bytes_global: referenced_global,
                    estimated_unreferenced_bytes: 0,
                },
                scope,
                versions,
            },
            cursor: cursor_out,
            exhausted,
        })
    }

    /// Bounded, cursor-resumable page-meta tombstone-health scan scoped to one
    /// `(index_id, active_version)`. Scans at most `max_pages` `VECTOR_PAGE_META` entries (clamped to
    /// `1..=MAX_SLAB_STATS_STEP_PAGES`), aggregating `row_count`/`live_count`/`tombstone_count` into
    /// `total_rows`/`physical_live_rows`/`tombstoned_rows`, and returns an opaque `PageKey` cursor to
    /// resume from. Reads only page meta — never row bytes.
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

        let mut page_count = 0u64;
        let mut total_rows = 0u64;
        let mut physical_live_rows = 0u64;
        let mut tombstoned_rows = 0u64;
        let mut last: Option<PageKey> = None;
        let mut exhausted = true;
        let mut processed: u32 = 0;
        {
            let lower = match &cursor {
                None => RangeBound::Included(PageKey::new(index_id, active_version, 0, 0)),
                Some(bytes) => RangeBound::Excluded(PageKey::from_bytes(Cow::Borrowed(bytes))),
            };
            for entry in self.meta.range((lower, RangeBound::Unbounded)) {
                let key = entry.key();
                if key.index_id != index_id || key.index_version != active_version {
                    break; // index/version-major order: past this generation's pages.
                }
                if processed >= budget {
                    exhausted = false;
                    break;
                }
                let m = entry.value();
                page_count += 1;
                total_rows = total_rows.saturating_add(m.row_count as u64);
                physical_live_rows = physical_live_rows.saturating_add(m.live_count as u64);
                tombstoned_rows = tombstoned_rows.saturating_add(m.tombstone_count as u64);
                last = Some(*key);
                processed += 1;
            }
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
        self.meta.get(&PageKey::new(
            index_id,
            index_version,
            partition_id,
            page_id,
        ))
    }

    #[cfg(test)]
    pub(crate) fn occupied_tail(&self) -> u64 {
        self.occupied_tail
    }

    /// Number of `VECTOR_PAGE_META` entries for `(index_id, index_version)` (all partitions).
    #[cfg(test)]
    pub(crate) fn version_page_count(&self, index_id: u32, index_version: u64) -> usize {
        let lower = PageKey::new(index_id, index_version, 0, 0);
        self.meta
            .range((RangeBound::Included(lower), RangeBound::Unbounded))
            .take_while(|e| {
                let k = e.key();
                k.index_id == index_id && k.index_version == index_version
            })
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::stable::VECTOR_PARTITION_HEADS;
    use crate::records::{PartitionKey, VectorIndexDef};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::vector_index::{
        VectorEncoding, VectorIndexKind, VectorMetric, VectorSubject,
    };
    use ic_stable_structures::BTreeMap;
    use ic_stable_structures::DefaultMemoryImpl;
    use ic_stable_structures::memory_manager::{MemoryId, MemoryManager};

    const META_ID: MemoryId = MemoryId::new(10);
    const SLAB_ID: MemoryId = MemoryId::new(13);

    type TestMm = MemoryManager<DefaultMemoryImpl>;

    fn fresh_mm() -> TestMm {
        MemoryManager::init(DefaultMemoryImpl::default())
    }

    fn open(mm: &TestMm) -> VectorSlabStore {
        let meta = BTreeMap::init(mm.get(META_ID));
        VectorSlabStore::from_regions(meta, mm.get(SLAB_ID))
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
        VECTOR_PARTITION_HEADS.with_borrow_mut(|h| h.clear_new());
    }

    fn head_live_len(index_id: u32, version: u64, partition: u32) -> u64 {
        VECTOR_PARTITION_HEADS
            .with_borrow(|h| h.get(&PartitionKey::new(index_id, version, partition)))
            .map(|head| head.live_len)
            .unwrap_or(0)
    }

    #[test]
    fn fresh_init_writes_header_and_empty_meta() {
        let mm = fresh_mm();
        let store = open(&mm);
        assert_eq!(store.occupied_tail(), SLAB_HEADER_SIZE as u64);
        assert_eq!(store.version_page_count(1, 1), 0);
    }

    #[test]
    fn empty_initialized_reopen_does_not_trap() {
        let mm = fresh_mm();
        open(&mm);
        open(&mm); // reopens cleanly
    }

    #[test]
    fn append_round_trip_and_reopen() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm);
        let slot = store
            .append_row(7, 1, 0, &d, subject(100), &bytes(1.0, 2.0), &zaux())
            .unwrap();
        assert_eq!(slot.page_id, 0);
        assert_eq!(slot.slot, 0);

        let store = open(&mm);
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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

        let store = open(&mm);
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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

        let store = open(&mm);
        assert_eq!(store.occupied_tail(), tail_before);
        assert_eq!(store.version_page_count(1, 1), page_count_before);
        assert_eq!(
            VECTOR_PARTITION_HEADS.with_borrow(|heads| heads.get(&head_key)),
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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
        assert_eq!(meta.tombstone_count, 1);
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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

    #[test]
    fn drop_version_pages_deletes_meta_and_keeps_occupied_tail() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm);
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
        let mut store = open(&mm);
        for v in 0..3u32 {
            store
                .append_row(1, 1, 0, &d, subject(v), &bytes(v as f32, 0.0), &zaux())
                .unwrap();
        }
        // 3 rows at capacity 2 -> 2 pages; a budget of 1 removes one page per step.
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
        open(&mm);
        mm.get(SLAB_ID).write(0, b"XXX".as_slice());
        open(&mm); // fail closed on a corrupt slab header
    }

    #[test]
    #[should_panic(expected = "empty slab region with non-empty page meta")]
    fn reopen_traps_on_empty_slab_with_nonempty_meta() {
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        // Wipe the slab to all zero but keep the directory: partial layout must trap.
        let slab = mm.get(SLAB_ID);
        let zeros = vec![0u8; slab.size() as usize * 65_536];
        slab.write(0, &zeros);
        open(&mm);
    }

    #[test]
    #[should_panic(expected = "row_count")]
    fn reopen_traps_on_counts_exceeding_capacity() {
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm);
        store
            .append_row(1, 1, 0, &d, subject(1), &bytes(0.0, 0.0), &zaux())
            .unwrap();
        // Corrupt the directory: row_count > capacity.
        store.meta.insert(
            PageKey::new(1, 1, 0, 0),
            VectorPageMeta {
                row_count: 99,
                capacity: 4,
                ..store.page_meta_for_test(1, 1, 0, 0).unwrap()
            },
        );
        open(&mm);
    }

    #[test]
    fn stats_fresh_store_is_empty() {
        let mm = fresh_mm();
        let store = open(&mm);
        let stats = store.stats_for_index(None);
        assert_eq!(stats.scope.page_count, 0);
        assert_eq!(stats.slab.referenced_page_bytes_global, 0);
    }

    #[test]
    fn stats_append_grows_pages_and_referenced_bytes() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm);
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
        // Referenced bytes = the exact page span (header + run table + row meta + vector bytes).
        let meta = store.page_meta_for_test(1, 1, 0, 0).unwrap();
        assert_eq!(
            stats.slab.referenced_page_bytes_global,
            page_span_bytes(&meta).unwrap()
        );
    }

    #[test]
    fn stats_tombstone_moves_live_to_tombstone_without_touching_bytes() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm);
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
        let mut store = open(&mm);
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
        let store = open(&mm);
        let step = store
            .partition_page_health_step(1, 1, None, 100)
            .expect("empty health step");
        assert!(step.exhausted);
        assert_eq!(step.partial.page_count, 0);
    }
}
