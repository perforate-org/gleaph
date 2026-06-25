//! Vector-index-owned composite slab page store (ADR 0032).
//!
//! Replaces the former `VECTOR_PAGE` large-value store with two stable regions opened as one
//! composite store:
//!
//! - `VECTOR_PAGE_META` (`BTreeMap<PageKey, VectorPageMeta>`, MemoryId 10) — the page directory.
//! - `VECTOR_ROW_SLAB` (raw stable memory, MemoryId 13) — the physical row bytes, behind a
//!   magic/version header ([`VectorRowSlabHeaderV1`]).
//!
//! Each physical page is fixed-stride and laid out **structure-of-arrays** so the vector bytes form
//! one contiguous scan unit, separated from the per-row metadata tables:
//!
//! ```text
//! page header: page_magic | capacity:u32 | row_stride:u32 | reserved
//! tables:
//!   vector_id       [u64; capacity]
//!   generation      [u64; capacity]
//!   subject_locator [(shard:u32, vertex:u32); capacity]
//!   tombstone_bits  [ceil(capacity / 8)]
//!   vector_bytes    [capacity * row_stride]
//! ```
//!
//! Allocation is tail-only (no free-span allocator in this slice); a page reserves its full span on
//! creation. Page cleanup deletes `VECTOR_PAGE_META` entries only — slab bytes are left in place as
//! dead space, so `occupied_tail` may exceed the highest referenced page end, and reopen validation
//! allows that. `VECTOR_PARTITION_HEADS` is the per-partition allocator/counter owner and lives
//! outside this composite store.

use super::memory::{Memory, StablePageMetaMap, init_page_meta, init_row_slab};
use crate::facade::stable::VECTOR_PARTITION_HEADS;
use crate::records::{PageKey, PartitionKey, SlotRef, VectorIndexDef};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    VectorIndexError, VectorPartitionHealthStep, VectorPartitionPageHealth, VectorSlabGlobalStats,
    VectorSlabScopeStats, VectorSlabStats, VectorSlabStatsStep, VectorSlabVersionStats,
    VectorSubject,
};
use ic_stable_structures::Memory as _;
use ic_stable_structures::storable::{Bound, Storable};
use std::borrow::Cow;
use std::ops::Bound as RangeBound;

/// WASM stable-memory page size in bytes.
const WASM_PAGE_SIZE: u64 = 65_536;

/// Slab header magic (`"VSL1"`).
const SLAB_MAGIC: &[u8; 4] = b"VSL1";
/// Slab layout version.
const SLAB_LAYOUT_VERSION: u32 = 1;
/// Fixed slab header length: magic(4) + version(4) + occupied_tail(8) + flags(4) + reserved.
const SLAB_HEADER_LEN: u64 = 32;

/// Per-page header magic (`"VPG1"`).
const PAGE_MAGIC: &[u8; 4] = b"VPG1";
/// Fixed per-page header length: magic(4) + capacity(4) + row_stride(4) + reserved(4).
const PAGE_HEADER_LEN: u64 = 16;

/// Per-step page-meta budget cap for [`VectorSlabStore::stats_step`] (mirrors `MAX_REBUILD_STEP_WORK`).
const MAX_SLAB_STATS_STEP_PAGES: u32 = 20_000;

/// Encoded length of a [`PageKey`] (its fixed `Storable` bound). A caller-supplied
/// [`VectorSlabStore::stats_step`] cursor must be exactly this many bytes; `PageKey::from_bytes`
/// panics otherwise.
const PAGE_KEY_LEN: usize = 24;

/// `ceil(capacity / 8)` tombstone-bitmap bytes for a page.
const fn tombstone_bytes(capacity: u32) -> u64 {
    (capacity as u64).div_ceil(8)
}

/// Total slab bytes one physical page of `capacity` rows of `row_stride` occupies.
const fn page_span(capacity: u32, row_stride: u32) -> u64 {
    PAGE_HEADER_LEN
        + 24 * capacity as u64 // vector_id + generation + subject_locator tables
        + tombstone_bytes(capacity)
        + capacity as u64 * row_stride as u64
}

/// Overflow-checked [`page_span`] for reopen validation, where `capacity`/`row_stride` are
/// untrusted directory bytes that could otherwise overflow the unchecked const arithmetic and wrap
/// to a span that spuriously passes the bounds check.
fn checked_page_span(capacity: u32, row_stride: u32) -> Option<u64> {
    let cap = capacity as u64;
    let tables = cap.checked_mul(24)?;
    let vectors = cap.checked_mul(row_stride as u64)?;
    PAGE_HEADER_LEN
        .checked_add(tables)?
        .checked_add(tombstone_bytes(capacity))?
        .checked_add(vectors)
}

/// Byte offset of the `vector_id` table (relative to the slab) for a page at `base`.
const fn vid_table(base: u64) -> u64 {
    base + PAGE_HEADER_LEN
}

const fn gen_table(base: u64, capacity: u32) -> u64 {
    vid_table(base) + 8 * capacity as u64
}

const fn loc_table(base: u64, capacity: u32) -> u64 {
    vid_table(base) + 16 * capacity as u64
}

const fn tomb_table(base: u64, capacity: u32) -> u64 {
    vid_table(base) + 24 * capacity as u64
}

const fn vec_table(base: u64, capacity: u32) -> u64 {
    tomb_table(base, capacity) + tombstone_bytes(capacity)
}

/// Per-page directory metadata for the slab page store (ADR 0032). Carries only page-physical facts;
/// `index_id`/`index_version`/`partition_id`/`page_id` live in the [`PageKey`]. Fixed-width
/// `Storable` (28 bytes) to keep the directory value cheap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct VectorPageMeta {
    pub slab_offset: u64,
    pub capacity: u32,
    pub row_count: u32,
    pub live_count: u32,
    pub row_stride: u32,
    pub tombstone_count: u32,
}

impl VectorPageMeta {
    fn to_array(self) -> [u8; 28] {
        let mut out = [0u8; 28];
        out[0..8].copy_from_slice(&self.slab_offset.to_le_bytes());
        out[8..12].copy_from_slice(&self.capacity.to_le_bytes());
        out[12..16].copy_from_slice(&self.row_count.to_le_bytes());
        out[16..20].copy_from_slice(&self.live_count.to_le_bytes());
        out[20..24].copy_from_slice(&self.row_stride.to_le_bytes());
        out[24..28].copy_from_slice(&self.tombstone_count.to_le_bytes());
        out
    }

    fn from_array(raw: [u8; 28]) -> Self {
        Self {
            slab_offset: u64::from_le_bytes(raw[0..8].try_into().expect("meta field")),
            capacity: u32::from_le_bytes(raw[8..12].try_into().expect("meta field")),
            row_count: u32::from_le_bytes(raw[12..16].try_into().expect("meta field")),
            live_count: u32::from_le_bytes(raw[16..20].try_into().expect("meta field")),
            row_stride: u32::from_le_bytes(raw[20..24].try_into().expect("meta field")),
            tombstone_count: u32::from_le_bytes(raw[24..28].try_into().expect("meta field")),
        }
    }
}

impl Storable for VectorPageMeta {
    const BOUND: Bound = Bound::Bounded {
        max_size: 28,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_array()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_array())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0u8; 28];
        raw.copy_from_slice(bytes.as_ref());
        Self::from_array(raw)
    }
}

/// Decoded per-row metadata returned by [`VectorSlabStore::read_row_bytes`] and yielded to the
/// [`VectorSlabStore::visit_partition_pages`] visitor. `subject_locator` is `(shard_id, vertex_id)`,
/// a derived scan accelerator — `VECTOR_SUBJECT_TO_ID` remains the freshness source of truth.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RowHeader {
    pub vector_id: u64,
    pub generation: u64,
    pub subject_locator: (u32, u32),
}

impl RowHeader {
    /// Rebuilds the `VectorSubject` this row's locator points at.
    pub(crate) fn subject(&self) -> VectorSubject {
        VectorSubject::Vertex {
            shard_id: ShardId::new(self.subject_locator.0),
            vertex_id: self.subject_locator.1,
        }
    }
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
        let bytes = checked_page_span(m.capacity, m.row_stride).unwrap_or(0);
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
    capacity: u32,
    row_stride: u32,
}

impl PageScratch {
    pub(crate) fn new() -> Self {
        Self {
            buf: Vec::new(),
            capacity: 0,
            row_stride: 0,
        }
    }

    /// Bulk-reads one whole page into the scratch buffer.
    fn load(&mut self, slab: &Memory, meta: &VectorPageMeta) {
        let span = page_span(meta.capacity, meta.row_stride) as usize;
        self.buf.resize(span, 0);
        slab.read(meta.slab_offset, &mut self.buf[..span]);
        self.capacity = meta.capacity;
        self.row_stride = meta.row_stride;
    }

    fn is_tombstoned(&self, slot: u32) -> bool {
        let tomb = (24 * self.capacity as usize) + PAGE_HEADER_LEN as usize;
        let byte = self.buf[tomb + (slot / 8) as usize];
        (byte >> (slot % 8)) & 1 == 1
    }

    fn vector_id(&self, slot: u32) -> u64 {
        let off = PAGE_HEADER_LEN as usize + 8 * slot as usize;
        u64::from_le_bytes(self.buf[off..off + 8].try_into().expect("vid field"))
    }

    fn generation(&self, slot: u32) -> u64 {
        let off = PAGE_HEADER_LEN as usize + 8 * self.capacity as usize + 8 * slot as usize;
        u64::from_le_bytes(self.buf[off..off + 8].try_into().expect("gen field"))
    }

    fn locator(&self, slot: u32) -> (u32, u32) {
        let off = PAGE_HEADER_LEN as usize + 16 * self.capacity as usize + 8 * slot as usize;
        let shard = u32::from_le_bytes(self.buf[off..off + 4].try_into().expect("loc field"));
        let vertex = u32::from_le_bytes(self.buf[off + 4..off + 8].try_into().expect("loc field"));
        (shard, vertex)
    }

    fn vec_slice(&self, slot: u32) -> &[u8] {
        let vec = vec_table(0, self.capacity) as usize;
        let start = vec + slot as usize * self.row_stride as usize;
        &self.buf[start..start + self.row_stride as usize]
    }
}

/// The composite slab page store: `VECTOR_PAGE_META` directory + raw `VECTOR_ROW_SLAB` region.
pub(crate) struct VectorSlabStore {
    meta: StablePageMetaMap,
    slab: Memory,
    occupied_tail: u64,
}

/// Grows `slab` so its byte size is at least `min_bytes`, returning `Err` on `grow` failure.
fn grow_to_at_least(slab: &Memory, min_bytes: u64) -> Result<(), VectorIndexError> {
    let size_bytes = slab
        .size()
        .checked_mul(WASM_PAGE_SIZE)
        .expect("slab address space overflow");
    if size_bytes >= min_bytes {
        return Ok(());
    }
    let delta_pages = (min_bytes - size_bytes).div_ceil(WASM_PAGE_SIZE);
    if slab.grow(delta_pages) == -1 {
        return Err(VectorIndexError::StableGrowFailed);
    }
    Ok(())
}

fn write_slab_header(slab: &Memory, occupied_tail: u64) {
    let mut buf = [0u8; SLAB_HEADER_LEN as usize];
    buf[0..4].copy_from_slice(SLAB_MAGIC);
    buf[4..8].copy_from_slice(&SLAB_LAYOUT_VERSION.to_le_bytes());
    buf[8..16].copy_from_slice(&occupied_tail.to_le_bytes());
    // bytes 16..20 layout_flags (0), 20..32 reserved (0)
    slab.write(0, &buf);
}

#[cfg(test)]
thread_local! {
    /// Test-only fault-injection seam for [`VectorSlabStore::append_row`]. `None` disables injection;
    /// `Some(k)` lets the next `k` appends succeed and forces the `(k+1)`-th to fail with
    /// [`VectorIndexError::StableGrowFailed`] (then disarms). This exercises the dual-write rollback
    /// path — the scariest branch, otherwise only reachable by exhausting stable memory.
    static FAIL_APPEND_AFTER: std::cell::Cell<Option<u32>> = const { std::cell::Cell::new(None) };
}

/// Arms the [`append_row`](VectorSlabStore::append_row) failure seam: `skip` subsequent appends
/// succeed, then the next one fails once with [`VectorIndexError::StableGrowFailed`].
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

impl VectorSlabStore {
    /// Opens both regions as one composite store, validating the reopen matrix (ADR 0032 invariant
    /// 6). Traps (fail closed) on any partial/corrupt layout.
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

    /// Composite open. Freshness is keyed on raw slab size, not magic: a non-empty region with bad
    /// magic is corruption, not freshness.
    fn open(slab: &Memory, meta: &StablePageMetaMap) -> u64 {
        if slab.size() == 0 {
            assert!(
                meta.is_empty(),
                "vector slab: empty slab region with non-empty page meta (partial layout)"
            );
            grow_to_at_least(slab, SLAB_HEADER_LEN).expect("grow fresh vector slab header");
            write_slab_header(slab, SLAB_HEADER_LEN);
            return SLAB_HEADER_LEN;
        }

        let mut magic = [0u8; 4];
        slab.read(0, &mut magic);
        assert_eq!(
            &magic, SLAB_MAGIC,
            "vector slab: invalid magic on a non-empty slab region (corrupt/partial layout)"
        );
        let mut ver = [0u8; 4];
        slab.read(4, &mut ver);
        assert_eq!(
            u32::from_le_bytes(ver),
            SLAB_LAYOUT_VERSION,
            "vector slab: unsupported layout version"
        );
        let mut tail = [0u8; 8];
        slab.read(8, &mut tail);
        let occupied_tail = u64::from_le_bytes(tail);
        let size_bytes = slab.size() * WASM_PAGE_SIZE;
        assert!(
            occupied_tail >= SLAB_HEADER_LEN && occupied_tail <= size_bytes,
            "vector slab: occupied_tail {occupied_tail} out of bounds (header {SLAB_HEADER_LEN}, \
             size {size_bytes})"
        );
        // Empty meta is a valid empty-initialized store (or drop-all-then-reopen). A non-empty meta
        // must satisfy the ADR 0032 storage-boundary invariants and lie within the allocated region;
        // `occupied_tail` may exceed the highest page end (leaked dead space) and that is allowed.
        // Fail-closed here so a corrupt directory cannot be accepted only to trap later when
        // `visit_partition_pages` scans `0..row_count` past a scratch buffer sized to `capacity`.
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
            let span = checked_page_span(m.capacity, m.row_stride)
                .expect("vector slab: page span overflow (corrupt directory)");
            let end = m.slab_offset.checked_add(span).expect("page span overflow");
            assert!(
                m.slab_offset >= SLAB_HEADER_LEN && end <= occupied_tail,
                "vector slab: page meta span [{}, {end}) outside [header, occupied_tail={occupied_tail})",
                m.slab_offset
            );
            // Cross-check the on-slab page header against the directory: a valid reopen requires the
            // physical page magic and fixed-stride layout to match what the directory entry claims.
            let mut hdr = [0u8; PAGE_HEADER_LEN as usize];
            slab.read(m.slab_offset, &mut hdr);
            let mut page_magic = [0u8; 4];
            page_magic.copy_from_slice(&hdr[0..4]);
            assert_eq!(
                &page_magic, PAGE_MAGIC,
                "vector slab: page at offset {} missing page magic (corrupt/partial layout)",
                m.slab_offset
            );
            let hdr_capacity =
                u32::from_le_bytes(hdr[4..8].try_into().expect("page header capacity"));
            let hdr_stride = u32::from_le_bytes(hdr[8..12].try_into().expect("page header stride"));
            assert!(
                hdr_capacity == m.capacity && hdr_stride == m.row_stride,
                "vector slab: page header (capacity {hdr_capacity}, stride {hdr_stride}) disagrees \
                 with directory (capacity {}, stride {}) at offset {}",
                m.capacity,
                m.row_stride,
                m.slab_offset
            );
        }
        occupied_tail
    }

    /// Resets the store to empty-initialized (canister (re)install). Clears the directory and
    /// rewinds the slab tail to the header; slab pages are not shrunk (stable memory cannot shrink),
    /// the bytes are reused on subsequent appends.
    pub(crate) fn reset(&mut self) {
        self.meta.clear_new();
        grow_to_at_least(&self.slab, SLAB_HEADER_LEN).expect("grow vector slab header on reset");
        write_slab_header(&self.slab, SLAB_HEADER_LEN);
        self.occupied_tail = SLAB_HEADER_LEN;
    }

    fn set_occupied_tail(&mut self, tail: u64) {
        self.occupied_tail = tail;
        self.slab.write(8, &tail.to_le_bytes());
    }

    /// Reserves and zero-initializes a fresh page at `base`, writing its page header. Fallible on
    /// slab `grow`; must run before any directory mutation.
    fn reserve_page(
        &mut self,
        base: u64,
        capacity: u32,
        row_stride: u32,
    ) -> Result<(), VectorIndexError> {
        let span = page_span(capacity, row_stride);
        let end = base.checked_add(span).expect("slab offset overflow");
        grow_to_at_least(&self.slab, end)?;
        let mut hdr = [0u8; PAGE_HEADER_LEN as usize];
        hdr[0..4].copy_from_slice(PAGE_MAGIC);
        hdr[4..8].copy_from_slice(&capacity.to_le_bytes());
        hdr[8..12].copy_from_slice(&row_stride.to_le_bytes());
        self.slab.write(base, &hdr);
        // Zero the tombstone bitmap: dead space reused after a reset can hold stale bits, and rows
        // are appended without re-clearing their bit.
        let tomb = tomb_table(base, capacity);
        let zeros = vec![0u8; tombstone_bytes(capacity) as usize];
        self.slab.write(tomb, &zeros);
        Ok(())
    }

    /// Writes one row's SoA tables + vector bytes at `slot` of the page at `base`. The page region
    /// is already reserved/grown, so this is infallible.
    fn write_row(
        &self,
        base: u64,
        capacity: u32,
        row_stride: u32,
        slot: u32,
        vector_id: u64,
        generation: u64,
        subject: VectorSubject,
        bytes: &[u8],
    ) {
        self.slab
            .write(vid_table(base) + 8 * slot as u64, &vector_id.to_le_bytes());
        self.slab.write(
            gen_table(base, capacity) + 8 * slot as u64,
            &generation.to_le_bytes(),
        );
        let VectorSubject::Vertex {
            shard_id,
            vertex_id,
        } = subject;
        let mut loc = [0u8; 8];
        loc[0..4].copy_from_slice(&shard_id.raw().to_le_bytes());
        loc[4..8].copy_from_slice(&vertex_id.to_le_bytes());
        self.slab
            .write(loc_table(base, capacity) + 8 * slot as u64, &loc);
        self.slab.write(
            vec_table(base, capacity) + slot as u64 * row_stride as u64,
            bytes,
        );
    }

    fn read_tombstone(&self, base: u64, capacity: u32, slot: u32) -> bool {
        let mut byte = [0u8; 1];
        self.slab
            .read(tomb_table(base, capacity) + (slot / 8) as u64, &mut byte);
        (byte[0] >> (slot % 8)) & 1 == 1
    }

    fn set_tombstone(&self, base: u64, capacity: u32, slot: u32) {
        let addr = tomb_table(base, capacity) + (slot / 8) as u64;
        let mut byte = [0u8; 1];
        self.slab.read(addr, &mut byte);
        byte[0] |= 1 << (slot % 8);
        self.slab.write(addr, &byte);
    }

    fn read_generation(&self, base: u64, capacity: u32, slot: u32) -> u64 {
        let mut buf = [0u8; 8];
        self.slab
            .read(gen_table(base, capacity) + 8 * slot as u64, &mut buf);
        u64::from_le_bytes(buf)
    }

    fn read_vector_id(&self, base: u64, slot: u32) -> u64 {
        let mut buf = [0u8; 8];
        self.slab.read(vid_table(base) + 8 * slot as u64, &mut buf);
        u64::from_le_bytes(buf)
    }

    fn read_locator(&self, base: u64, capacity: u32, slot: u32) -> (u32, u32) {
        let mut buf = [0u8; 8];
        self.slab
            .read(loc_table(base, capacity) + 8 * slot as u64, &mut buf);
        (
            u32::from_le_bytes(buf[0..4].try_into().expect("loc field")),
            u32::from_le_bytes(buf[4..8].try_into().expect("loc field")),
        )
    }

    fn read_vec(&self, base: u64, capacity: u32, row_stride: u32, slot: u32) -> Vec<u8> {
        let mut out = vec![0u8; row_stride as usize];
        self.slab.read(
            vec_table(base, capacity) + slot as u64 * row_stride as u64,
            &mut out,
        );
        out
    }

    /// Appends a vector row into the partition's page chain, rolling a new page when the mutable
    /// page is full. Write-then-commit: slab grow + writes happen before any `VECTOR_PAGE_META` /
    /// `VECTOR_PARTITION_HEADS` mutation, and the head update is last, so a failed grow cannot leave
    /// a head/meta pointing at unwritten bytes.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn append_row(
        &mut self,
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        def: &VectorIndexDef,
        vector_id: u64,
        generation: u64,
        subject: VectorSubject,
        bytes: &[u8],
    ) -> Result<SlotRef, VectorIndexError> {
        // Test-only: simulate a slab `grow` failure before any state mutation (see seam above).
        #[cfg(test)]
        if take_injected_append_failure() {
            return Err(VectorIndexError::StableGrowFailed);
        }
        let capacity = def.slots_per_page;
        let row_stride = def.stride_bytes;
        debug_assert_eq!(
            bytes.len(),
            row_stride as usize,
            "append row stride mismatch"
        );
        let head_key = PartitionKey::new(index_id, index_version, partition_id);
        let mut head = VECTOR_PARTITION_HEADS
            .with_borrow(|h| h.get(&head_key))
            .unwrap_or_default();

        let need_new_page = if head.page_count == 0 {
            true
        } else {
            let mutable_key =
                PageKey::new(index_id, index_version, partition_id, head.mutable_page);
            let m = self
                .meta
                .get(&mutable_key)
                .expect("mutable page meta present");
            m.row_count >= m.capacity
        };

        let (page_id, mut meta) = if need_new_page {
            let page_id = head.next_page_id;
            let slab_offset = self.occupied_tail;
            // Fallible slab grow + page-header/tombstone init BEFORE any directory mutation.
            self.reserve_page(slab_offset, capacity, row_stride)?;
            (
                page_id,
                VectorPageMeta {
                    slab_offset,
                    capacity,
                    row_count: 0,
                    live_count: 0,
                    row_stride,
                    tombstone_count: 0,
                },
            )
        } else {
            let page_id = head.mutable_page;
            let mutable_key = PageKey::new(index_id, index_version, partition_id, page_id);
            (
                page_id,
                self.meta.get(&mutable_key).expect("mutable page meta"),
            )
        };
        debug_assert_eq!(meta.row_stride, row_stride, "page stride mismatch");

        let slot = meta.row_count;
        let page_key = PageKey::new(index_id, index_version, partition_id, page_id);
        // Write row bytes (infallible: the page region is already reserved/grown).
        self.write_row(
            meta.slab_offset,
            capacity,
            row_stride,
            slot,
            vector_id,
            generation,
            subject,
            bytes,
        );

        // Commit directory: occupied_tail (header) -> page meta -> partition head (last).
        if need_new_page {
            self.set_occupied_tail(meta.slab_offset + page_span(capacity, row_stride));
        }
        meta.row_count += 1;
        meta.live_count += 1;
        self.meta.insert(page_key, meta);

        if need_new_page {
            if head.page_count == 0 {
                head.first_page = page_id;
            }
            head.mutable_page = page_id;
            head.page_count += 1;
            head.next_page_id = page_id + 1;
        }
        head.live_len += 1;
        VECTOR_PARTITION_HEADS.with_borrow_mut(|h| h.insert(head_key, head));

        Ok(SlotRef {
            index_version,
            partition_id,
            page_id,
            slot,
            generation,
        })
    }

    /// Marks a slot tombstoned, owning all live/tombstone accounting idempotently: on the
    /// live->tombstoned transition it sets the bit and adjusts `VectorPageMeta.live_count` /
    /// `tombstone_count` and the row's `VECTOR_PARTITION_HEADS.live_len` exactly once. Returns
    /// `true` only when the row changed (was previously live and in range).
    pub(crate) fn tombstone_row(&mut self, index_id: u32, slot: SlotRef) -> bool {
        let page_key = PageKey::new(
            index_id,
            slot.index_version,
            slot.partition_id,
            slot.page_id,
        );
        let Some(mut meta) = self.meta.get(&page_key) else {
            return false;
        };
        if slot.slot >= meta.row_count {
            return false;
        }
        if self.read_tombstone(meta.slab_offset, meta.capacity, slot.slot) {
            return false;
        }
        self.set_tombstone(meta.slab_offset, meta.capacity, slot.slot);
        meta.tombstone_count += 1;
        meta.live_count = meta.live_count.saturating_sub(1);
        self.meta.insert(page_key, meta);

        let head_key = PartitionKey::new(index_id, slot.index_version, slot.partition_id);
        VECTOR_PARTITION_HEADS.with_borrow_mut(|h| {
            if let Some(mut head) = h.get(&head_key) {
                head.live_len = head.live_len.saturating_sub(1);
                h.insert(head_key, head);
            }
        });
        true
    }

    /// Reads a slot's vector bytes + decoded row header, rejecting out-of-range slots, tombstoned
    /// rows, and generation mismatches.
    pub(crate) fn read_row_bytes(
        &self,
        index_id: u32,
        slot: SlotRef,
    ) -> Option<(RowHeader, Vec<u8>)> {
        let page_key = PageKey::new(
            index_id,
            slot.index_version,
            slot.partition_id,
            slot.page_id,
        );
        let meta = self.meta.get(&page_key)?;
        if slot.slot >= meta.row_count {
            return None;
        }
        if self.read_tombstone(meta.slab_offset, meta.capacity, slot.slot) {
            return None;
        }
        let generation = self.read_generation(meta.slab_offset, meta.capacity, slot.slot);
        if generation != slot.generation {
            return None;
        }
        let vector_id = self.read_vector_id(meta.slab_offset, slot.slot);
        let subject_locator = self.read_locator(meta.slab_offset, meta.capacity, slot.slot);
        let bytes = self.read_vec(meta.slab_offset, meta.capacity, meta.row_stride, slot.slot);
        Some((
            RowHeader {
                vector_id,
                generation,
                subject_locator,
            },
            bytes,
        ))
    }

    /// Page/batch visitor over one partition's page chain. Each page is bulk-read once into
    /// `scratch`; the visitor is invoked per live (non-tombstoned) slot with the decoded
    /// [`RowHeader`] and a zero-copy slice into the contiguous `vector_bytes` table.
    pub(crate) fn visit_partition_pages<F: FnMut(SlotRef, &RowHeader, &[u8])>(
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
            scratch.load(&self.slab, &meta);
            for slot in 0..meta.row_count {
                if scratch.is_tombstoned(slot) {
                    continue;
                }
                let header = RowHeader {
                    vector_id: scratch.vector_id(slot),
                    generation: scratch.generation(slot),
                    subject_locator: scratch.locator(slot),
                };
                let slot_ref = SlotRef {
                    index_version,
                    partition_id,
                    page_id: key.page_id,
                    slot,
                    generation: header.generation,
                };
                visitor(slot_ref, &header, scratch.vec_slice(slot));
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

    /// Derived, admin-only slab-space observability (ADR 0032 follow-up slice). Computes whole-slab
    /// physical facts plus logical counters scoped to `index_id` (`None` = all indexes), in a single
    /// pass over `VECTOR_PAGE_META`.
    ///
    /// **Unbounded**: it scans every page-meta entry (even for `Some(index_id)`, because the global
    /// dead-space estimate needs the whole slab). This is acceptable for admin/debug use; a bounded
    /// `cursor + max_pages` snapshot is a deferred follow-up.
    ///
    /// Reads only page meta + the slab header/size — never row bytes, `VECTOR_SUBJECT_TO_ID`, or any
    /// mutation. `physical_live_row_count` is `VectorPageMeta.live_count` (physical non-tombstone),
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
            .saturating_sub(SLAB_HEADER_LEN)
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
    /// Like [`stats_for_index`](Self::stats_for_index) this reads only page meta + the slab
    /// header/size. Each step's `partial.slab.referenced_page_bytes_global` sums every page observed
    /// in the step (even outside `index_id`), so the merged total covers the whole slab; the per-step
    /// `partial.slab.estimated_unreferenced_bytes` is always `0` and the caller recomputes it after
    /// merging. The `cursor` is **external caller input**, so a malformed (wrong-length) cursor is
    /// rejected with [`VectorIndexError::InvalidStatsCursor`] rather than trapping.
    ///
    /// This is a bounded best-effort scan, **not** a point-in-time snapshot: the cursor is only a
    /// `PageKey`, so `VECTOR_PAGE_META` writes between steps are not isolated (see
    /// [`VectorSlabStatsStep`] for the exact drift modes). Run the steps during a quiescent window, or
    /// use the single-call [`stats_for_index`](Self::stats_for_index), for an exact whole-slab figure.
    pub(crate) fn stats_step(
        &self,
        cursor: Option<Vec<u8>>,
        max_pages: u32,
        index_id: Option<u32>,
    ) -> Result<VectorSlabStatsStep, VectorIndexError> {
        let budget = max_pages.clamp(1, MAX_SLAB_STATS_STEP_PAGES);
        // Validate the caller-supplied cursor before decoding: `PageKey::from_bytes` panics on any
        // length other than `PAGE_KEY_LEN`, and this cursor is untrusted Candid input.
        if let Some(bytes) = &cursor
            && bytes.len() != PAGE_KEY_LEN
        {
            return Err(VectorIndexError::InvalidStatsCursor);
        }

        let mut acc = SlabStatsAcc::new(index_id);
        let mut last: Option<PageKey> = None;
        let mut exhausted = true;
        let mut processed: u32 = 0;
        {
            // Scan the whole map (not one index) so global referenced bytes stay correct under a
            // `Some(index_id)` filter.
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
    /// `(index_id, active_version)` (ADR 0031 Slice 9). Scans at most `max_pages` `VECTOR_PAGE_META`
    /// entries (clamped to `1..=MAX_SLAB_STATS_STEP_PAGES`), aggregating `row_count`/`live_count`/
    /// `tombstone_count` into `total_rows`/`physical_live_rows`/`tombstoned_rows`, and returns an
    /// opaque `PageKey` cursor to resume from. Reads only page meta — never row bytes or
    /// `VECTOR_SUBJECT_TO_ID`.
    ///
    /// Because the scan is scoped to one generation (unlike the global [`Self::stats_step`]), the
    /// caller-supplied `cursor` is **scope-checked**: a wrong-length cursor, or one whose
    /// `(index_id, index_version)` does not match `(index_id, active_version)`, returns
    /// [`VectorIndexError::InvalidStatsCursor`] rather than silently yielding an empty exhausted
    /// result. `PageKey` is `index -> version -> partition -> page` ordered, so the scan starts at the
    /// scope's lower bound (or the validated cursor) and breaks once the key leaves the scope.
    ///
    /// Bounded best-effort, **not** a snapshot: concurrent `VECTOR_PAGE_META` writes between steps are
    /// not isolated (see [`VectorPartitionHealthStep`]).
    pub(crate) fn partition_page_health_step(
        &self,
        index_id: u32,
        active_version: u64,
        cursor: Option<Vec<u8>>,
        max_pages: u32,
    ) -> Result<VectorPartitionHealthStep, VectorIndexError> {
        let budget = max_pages.clamp(1, MAX_SLAB_STATS_STEP_PAGES);
        // Validate + scope-check the caller-supplied cursor before decoding for the range bound.
        if let Some(bytes) = &cursor {
            if bytes.len() != PAGE_KEY_LEN {
                return Err(VectorIndexError::InvalidStatsCursor);
            }
            let key = PageKey::from_bytes(Cow::Borrowed(bytes));
            if key.index_id != index_id || key.index_version != active_version {
                return Err(VectorIndexError::InvalidStatsCursor);
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
            max_page_bytes: 65_536,
            slots_per_page: capacity,
            next_vector_id: 1,
        }
    }

    fn subject(v: u32) -> VectorSubject {
        VectorSubject::Vertex {
            shard_id: ShardId::new(0),
            vertex_id: v,
        }
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
        assert_eq!(store.occupied_tail(), SLAB_HEADER_LEN);
        assert_eq!(store.version_page_count(1, 1), 0);
    }

    #[test]
    fn empty_initialized_reopen_does_not_trap() {
        let mm = fresh_mm();
        drop(open(&mm)); // fresh-init writes the header, leaves meta empty
        let store = open(&mm); // reopen over the same regions
        assert_eq!(store.occupied_tail(), SLAB_HEADER_LEN);
        assert_eq!(store.version_page_count(1, 1), 0);
    }

    #[test]
    fn append_round_trip_and_reopen() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let slot = {
            let mut store = open(&mm);
            store
                .append_row(7, 1, 0, &d, 100, 1, subject(100), &bytes(1.0, 2.0))
                .expect("append")
        };
        assert_eq!(slot.index_version, 1);
        assert_eq!(slot.partition_id, 0);
        assert_eq!(slot.page_id, 0);
        assert_eq!(slot.slot, 0);
        assert_eq!(slot.generation, 1);
        // Reopen and read the row back.
        let store = open(&mm);
        let (header, vec) = store
            .read_row_bytes(7, slot)
            .expect("row present after reopen");
        assert_eq!(header.vector_id, 100);
        assert_eq!(header.generation, 1);
        assert_eq!(header.subject(), subject(100));
        assert_eq!(vec, bytes(1.0, 2.0));
    }

    #[test]
    fn append_rolls_new_page_at_capacity() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2); // 2 slots per page
        let mut store = open(&mm);
        let s0 = store
            .append_row(1, 1, 0, &d, 1, 1, subject(1), &bytes(0.0, 0.0))
            .unwrap();
        let s1 = store
            .append_row(1, 1, 0, &d, 2, 1, subject(2), &bytes(1.0, 1.0))
            .unwrap();
        let s2 = store
            .append_row(1, 1, 0, &d, 3, 1, subject(3), &bytes(2.0, 2.0))
            .unwrap();
        assert_eq!((s0.page_id, s0.slot), (0, 0));
        assert_eq!((s1.page_id, s1.slot), (0, 1));
        assert_eq!(
            (s2.page_id, s2.slot),
            (1, 0),
            "third row rolls to a new page"
        );
        assert_eq!(store.version_page_count(1, 1), 2);
    }

    #[test]
    fn tombstone_is_idempotent_across_counters() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm);
        let slot = store
            .append_row(1, 1, 0, &d, 1, 1, subject(1), &bytes(0.0, 0.0))
            .unwrap();
        store
            .append_row(1, 1, 0, &d, 2, 1, subject(2), &bytes(1.0, 1.0))
            .unwrap();
        assert_eq!(head_live_len(1, 1, 0), 2);

        assert!(
            store.tombstone_row(1, slot),
            "first tombstone changes the row"
        );
        assert!(!store.tombstone_row(1, slot), "second tombstone is a no-op");

        let meta = store.page_meta_for_test(1, 1, 0, 0).expect("page meta");
        assert_eq!(meta.row_count, 2);
        assert_eq!(meta.live_count, 1, "live_count decremented exactly once");
        assert_eq!(
            meta.tombstone_count, 1,
            "tombstone_count incremented exactly once"
        );
        assert_eq!(
            head_live_len(1, 1, 0),
            1,
            "PartitionHead.live_len decremented once"
        );
        assert!(
            store.read_row_bytes(1, slot).is_none(),
            "tombstoned row is not readable"
        );
    }

    #[test]
    fn read_row_bytes_rejects_stale_generation_and_out_of_range() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(4);
        let mut store = open(&mm);
        let slot = store
            .append_row(1, 1, 0, &d, 1, 5, subject(1), &bytes(0.0, 0.0))
            .unwrap();
        // Stale generation.
        let stale = SlotRef {
            generation: 4,
            ..slot
        };
        assert!(store.read_row_bytes(1, stale).is_none());
        // Out-of-range slot.
        let oob = SlotRef { slot: 9, ..slot };
        assert!(store.read_row_bytes(1, oob).is_none());
        // Exact match still reads.
        assert!(store.read_row_bytes(1, slot).is_some());
    }

    #[test]
    fn visit_partition_pages_yields_live_rows_only() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm);
        let s0 = store
            .append_row(1, 1, 0, &d, 1, 1, subject(1), &bytes(0.0, 0.0))
            .unwrap();
        store
            .append_row(1, 1, 0, &d, 2, 1, subject(2), &bytes(1.0, 1.0))
            .unwrap();
        store
            .append_row(1, 1, 0, &d, 3, 1, subject(3), &bytes(2.0, 2.0))
            .unwrap();
        store.tombstone_row(1, s0);

        let mut seen = Vec::new();
        let mut scratch = PageScratch::new();
        store.visit_partition_pages(1, 1, 0, &mut scratch, |slot, header, vec| {
            assert_eq!(vec.len(), d.stride_bytes as usize);
            seen.push((slot.page_id, slot.slot, header.vector_id));
        });
        // Tombstoned vector_id 1 is skipped; rows span both pages.
        assert_eq!(seen, vec![(0, 1, 2), (1, 0, 3)]);
    }

    #[test]
    fn drop_version_pages_deletes_meta_and_keeps_occupied_tail() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1); // one slot per page → one page per row
        let mut store = open(&mm);
        for v in 0..3u32 {
            store
                .append_row(1, 1, 0, &d, v as u64, 1, subject(v), &bytes(v as f32, 0.0))
                .unwrap();
        }
        assert_eq!(store.version_page_count(1, 1), 3);
        let tail_before = store.occupied_tail();

        let progress = store.drop_version_pages(1, 1, None, 100);
        assert!(progress.exhausted);
        assert_eq!(store.version_page_count(1, 1), 0, "all page meta dropped");
        assert_eq!(
            store.occupied_tail(),
            tail_before,
            "no slab tail rewind this slice"
        );

        // Reopen with leaked dead space (occupied_tail > header, empty meta) must not trap.
        let reopened = open(&mm);
        assert_eq!(reopened.occupied_tail(), tail_before);
        assert_eq!(reopened.version_page_count(1, 1), 0);
    }

    #[test]
    fn drop_version_pages_is_cursor_resumable() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1);
        let mut store = open(&mm);
        for v in 0..4u32 {
            store
                .append_row(1, 1, 0, &d, v as u64, 1, subject(v), &bytes(v as f32, 0.0))
                .unwrap();
        }
        let step = store.drop_version_pages(1, 1, None, 2);
        assert!(!step.exhausted);
        assert_eq!(store.version_page_count(1, 1), 2);
        let done = store.drop_version_pages(1, 1, step.cursor, 2);
        assert!(done.exhausted);
        assert_eq!(store.version_page_count(1, 1), 0);
    }

    #[test]
    #[should_panic(expected = "invalid magic")]
    fn reopen_traps_on_bad_magic() {
        let mm = fresh_mm();
        // Grow the slab and write non-magic bytes, leaving meta empty → corrupt, not fresh.
        let slab = mm.get(SLAB_ID);
        slab.grow(1);
        slab.write(0, &[0xAB, 0xCD, 0xEF, 0x01]);
        let _ = open(&mm);
    }

    #[test]
    #[should_panic(expected = "non-empty page meta")]
    fn reopen_traps_on_empty_slab_with_nonempty_meta() {
        let mm = fresh_mm();
        // Insert a page-meta entry while the slab region stays size 0 → partial layout.
        {
            let mut meta: StablePageMetaMap = BTreeMap::init(mm.get(META_ID));
            meta.insert(
                PageKey::new(1, 1, 0, 0),
                VectorPageMeta {
                    slab_offset: SLAB_HEADER_LEN,
                    capacity: 1,
                    row_count: 0,
                    live_count: 0,
                    row_stride: 8,
                    tombstone_count: 0,
                },
            );
        }
        let _ = open(&mm);
    }

    /// Seeds a single valid page (capacity 2, one row), then returns its `(stride, slab_offset)` so a
    /// reopen test can overwrite the directory entry with a corrupt counter set.
    fn seed_one_page(mm: &TestMm) -> (u32, u64) {
        clear_heads();
        let d = def(2);
        let mut store = open(mm);
        store
            .append_row(1, 1, 0, &d, 1, 1, subject(1), &bytes(0.0, 0.0))
            .unwrap();
        (d.stride_bytes, SLAB_HEADER_LEN)
    }

    fn overwrite_meta(mm: &TestMm, m: VectorPageMeta) {
        let mut meta: StablePageMetaMap = BTreeMap::init(mm.get(META_ID));
        meta.insert(PageKey::new(1, 1, 0, 0), m);
    }

    #[test]
    #[should_panic(expected = "row_count")]
    fn reopen_traps_on_row_count_exceeding_capacity() {
        let mm = fresh_mm();
        let (stride, slab_offset) = seed_one_page(&mm);
        overwrite_meta(
            &mm,
            VectorPageMeta {
                slab_offset,
                capacity: 2,
                row_count: 5, // > capacity → reopen must fail closed
                live_count: 0,
                row_stride: stride,
                tombstone_count: 0,
            },
        );
        let _ = open(&mm);
    }

    #[test]
    #[should_panic(expected = "live_count")]
    fn reopen_traps_on_counts_exceeding_row_count() {
        let mm = fresh_mm();
        let (stride, slab_offset) = seed_one_page(&mm);
        overwrite_meta(
            &mm,
            VectorPageMeta {
                slab_offset,
                capacity: 2,
                row_count: 1,
                live_count: 1,
                row_stride: stride,
                tombstone_count: 1, // live + tombstone = 2 > row_count = 1
            },
        );
        let _ = open(&mm);
    }

    #[test]
    #[should_panic(expected = "missing page magic")]
    fn reopen_traps_on_page_header_magic_mismatch() {
        let mm = fresh_mm();
        let (_stride, slab_offset) = seed_one_page(&mm);
        // Clobber the on-slab page magic while the directory entry stays internally valid: a reopen
        // must reject the physical/directory disagreement rather than scan a bogus page later.
        let slab = mm.get(SLAB_ID);
        slab.write(slab_offset, &[0u8; 4]);
        let _ = open(&mm);
    }

    #[test]
    fn stats_fresh_store_is_empty() {
        let mm = fresh_mm();
        let stats = open(&mm).stats_for_index(None);
        assert_eq!(stats.slab.occupied_tail_bytes, SLAB_HEADER_LEN);
        assert_eq!(stats.slab.referenced_page_bytes_global, 0);
        assert_eq!(stats.slab.estimated_unreferenced_bytes, 0);
        assert_eq!(stats.scope.page_count, 0);
        assert_eq!(stats.scope.row_count, 0);
        assert_eq!(stats.scope.physical_live_row_count, 0);
        assert_eq!(stats.scope.tombstone_row_count, 0);
        assert!(stats.versions.is_empty());
    }

    #[test]
    fn stats_append_grows_pages_and_referenced_bytes() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm);
        store
            .append_row(1, 1, 0, &d, 1, 1, subject(1), &bytes(0.0, 0.0))
            .unwrap();
        store
            .append_row(1, 1, 0, &d, 2, 1, subject(2), &bytes(1.0, 1.0))
            .unwrap();
        let span = page_span(2, d.stride_bytes); // one full page
        let stats = store.stats_for_index(None);
        assert_eq!(stats.scope.page_count, 1);
        assert_eq!(stats.scope.row_count, 2);
        assert_eq!(stats.scope.physical_live_row_count, 2);
        assert_eq!(stats.scope.tombstone_row_count, 0);
        assert_eq!(stats.scope.referenced_page_bytes, span);
        assert_eq!(stats.slab.referenced_page_bytes_global, span);
        // tail == header + the one reserved page span; no dead space yet.
        assert_eq!(stats.slab.occupied_tail_bytes, SLAB_HEADER_LEN + span);
        assert_eq!(stats.slab.estimated_unreferenced_bytes, 0);
    }

    #[test]
    fn stats_tombstone_moves_live_to_tombstone_without_touching_bytes() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2);
        let mut store = open(&mm);
        let s0 = store
            .append_row(1, 1, 0, &d, 1, 1, subject(1), &bytes(0.0, 0.0))
            .unwrap();
        store
            .append_row(1, 1, 0, &d, 2, 1, subject(2), &bytes(1.0, 1.0))
            .unwrap();
        let before = store.stats_for_index(None);
        assert!(store.tombstone_row(1, s0));
        let after = store.stats_for_index(None);
        assert_eq!(
            after.scope.physical_live_row_count,
            before.scope.physical_live_row_count - 1
        );
        assert_eq!(
            after.scope.tombstone_row_count,
            before.scope.tombstone_row_count + 1
        );
        assert_eq!(after.scope.row_count, before.scope.row_count);
        assert_eq!(
            after.scope.referenced_page_bytes,
            before.scope.referenced_page_bytes
        );
        assert_eq!(
            after.slab.referenced_page_bytes_global,
            before.slab.referenced_page_bytes_global
        );
    }

    #[test]
    fn stats_drop_version_pages_increases_estimated_dead_space() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1); // one page per row
        let mut store = open(&mm);
        for v in 0..3u32 {
            store
                .append_row(1, 1, 0, &d, v as u64, 1, subject(v), &bytes(v as f32, 0.0))
                .unwrap();
        }
        let before = store.stats_for_index(None);
        assert_eq!(before.scope.page_count, 3);
        assert_eq!(before.slab.estimated_unreferenced_bytes, 0);
        let tail_before = before.slab.occupied_tail_bytes;

        assert!(store.drop_version_pages(1, 1, None, 100).exhausted);
        let after = store.stats_for_index(None);
        assert_eq!(after.scope.page_count, 0);
        assert_eq!(after.slab.referenced_page_bytes_global, 0);
        assert_eq!(
            after.slab.occupied_tail_bytes, tail_before,
            "no slab tail rewind this slice"
        );
        assert_eq!(
            after.slab.estimated_unreferenced_bytes,
            tail_before - SLAB_HEADER_LEN,
            "all referenced bytes became leaked dead space"
        );
        assert!(after.slab.estimated_unreferenced_bytes > before.slab.estimated_unreferenced_bytes);
    }

    #[test]
    fn stats_scope_filters_logical_counters_but_slab_facts_stay_global() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1); // one page per row → easy page counting
        let mut store = open(&mm);
        // index 1 version 1: 2 rows; index 1 version 2: 1 row; index 2 version 1: 1 row.
        store
            .append_row(1, 1, 0, &d, 1, 1, subject(1), &bytes(0.0, 0.0))
            .unwrap();
        store
            .append_row(1, 1, 0, &d, 2, 1, subject(2), &bytes(1.0, 0.0))
            .unwrap();
        store
            .append_row(1, 2, 0, &d, 3, 1, subject(3), &bytes(2.0, 0.0))
            .unwrap();
        store
            .append_row(2, 1, 0, &d, 4, 1, subject(4), &bytes(3.0, 0.0))
            .unwrap();
        let span = page_span(1, d.stride_bytes);

        let global = store.stats_for_index(None);
        assert_eq!(global.scope.page_count, 4);
        assert_eq!(global.scope.row_count, 4);
        assert_eq!(global.slab.referenced_page_bytes_global, 4 * span);
        assert_eq!(global.versions.len(), 3);
        assert_eq!(
            (
                global.versions[0].index_id,
                global.versions[0].index_version,
                global.versions[0].page_count
            ),
            (1, 1, 2)
        );
        assert_eq!(
            (
                global.versions[1].index_id,
                global.versions[1].index_version,
                global.versions[1].page_count
            ),
            (1, 2, 1)
        );
        assert_eq!(
            (
                global.versions[2].index_id,
                global.versions[2].index_version,
                global.versions[2].page_count
            ),
            (2, 1, 1)
        );

        let scoped = store.stats_for_index(Some(1));
        assert_eq!(scoped.scope.index_id, Some(1));
        assert_eq!(
            scoped.scope.page_count, 3,
            "index 1 across versions 1 and 2"
        );
        assert_eq!(scoped.scope.referenced_page_bytes, 3 * span);
        assert_eq!(scoped.versions.len(), 2);
        assert!(scoped.versions.iter().all(|v| v.index_id == 1));
        // Physical slab facts are whole-slab global regardless of the index filter.
        assert_eq!(scoped.slab, global.slab);
    }

    /// Drives [`VectorSlabStore::stats_step`] to exhaustion, returning every partial step.
    fn collect_steps(
        store: &VectorSlabStore,
        max_pages: u32,
        index_id: Option<u32>,
    ) -> Vec<VectorSlabStatsStep> {
        let mut steps = Vec::new();
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            let step = store
                .stats_step(cursor.clone(), max_pages, index_id)
                .expect("step");
            let done = step.exhausted;
            cursor = step.cursor.clone();
            steps.push(step);
            if done {
                break;
            }
        }
        steps
    }

    /// Client-side merge per [`VectorSlabStatsStep`]'s contract: additive logical counters + global
    /// referenced bytes, last physical snapshot, dead space recomputed once after merging.
    fn merge_steps(steps: &[VectorSlabStatsStep]) -> VectorSlabStats {
        let last = steps.last().expect("at least one step");
        let slab_size_bytes = last.partial.slab.slab_size_bytes;
        let occupied_tail_bytes = last.partial.slab.occupied_tail_bytes;
        let index_id = last.partial.scope.index_id;

        let mut referenced_global = 0u64;
        let mut scope = VectorSlabScopeStats {
            index_id,
            referenced_page_bytes: 0,
            page_count: 0,
            row_count: 0,
            physical_live_row_count: 0,
            tombstone_row_count: 0,
        };
        let mut versions: Vec<VectorSlabVersionStats> = Vec::new();
        for step in steps {
            referenced_global =
                referenced_global.saturating_add(step.partial.slab.referenced_page_bytes_global);
            let s = &step.partial.scope;
            scope.referenced_page_bytes = scope
                .referenced_page_bytes
                .saturating_add(s.referenced_page_bytes);
            scope.page_count = scope.page_count.saturating_add(s.page_count);
            scope.row_count = scope.row_count.saturating_add(s.row_count);
            scope.physical_live_row_count = scope
                .physical_live_row_count
                .saturating_add(s.physical_live_row_count);
            scope.tombstone_row_count = scope
                .tombstone_row_count
                .saturating_add(s.tombstone_row_count);
            for v in &step.partial.versions {
                match versions
                    .iter_mut()
                    .find(|e| e.index_id == v.index_id && e.index_version == v.index_version)
                {
                    Some(e) => {
                        e.page_count = e.page_count.saturating_add(v.page_count);
                        e.row_count = e.row_count.saturating_add(v.row_count);
                        e.physical_live_row_count = e
                            .physical_live_row_count
                            .saturating_add(v.physical_live_row_count);
                        e.tombstone_row_count =
                            e.tombstone_row_count.saturating_add(v.tombstone_row_count);
                        e.referenced_page_bytes = e
                            .referenced_page_bytes
                            .saturating_add(v.referenced_page_bytes);
                    }
                    None => versions.push(*v),
                }
            }
        }
        let estimated_unreferenced_bytes = occupied_tail_bytes
            .saturating_sub(SLAB_HEADER_LEN)
            .saturating_sub(referenced_global);
        VectorSlabStats {
            slab: VectorSlabGlobalStats {
                slab_size_bytes,
                occupied_tail_bytes,
                referenced_page_bytes_global: referenced_global,
                estimated_unreferenced_bytes,
            },
            scope,
            versions,
        }
    }

    fn seed_rows(
        store: &mut VectorSlabStore,
        d: &VectorIndexDef,
        index_id: u32,
        version: u64,
        n: u32,
    ) {
        for v in 0..n {
            store
                .append_row(
                    index_id,
                    version,
                    0,
                    d,
                    v as u64,
                    1,
                    subject(v),
                    &bytes(v as f32, 0.0),
                )
                .unwrap();
        }
    }

    #[test]
    fn stats_step_single_step_matches_unbounded() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1); // one page per row
        let mut store = open(&mm);
        seed_rows(&mut store, &d, 1, 1, 3);

        let steps = collect_steps(&store, 100, None);
        assert_eq!(steps.len(), 1, "budget covers every page in one step");
        assert!(steps[0].exhausted);
        assert!(steps[0].cursor.is_none());
        assert_eq!(
            steps[0].partial.slab.estimated_unreferenced_bytes, 0,
            "per-step dead space is always 0"
        );
        assert_eq!(merge_steps(&steps), store.stats_for_index(None));
    }

    #[test]
    fn stats_step_multi_step_merge_matches_unbounded() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1);
        let mut store = open(&mm);
        seed_rows(&mut store, &d, 1, 1, 5);

        let steps = collect_steps(&store, 1, None);
        assert_eq!(
            steps.len(),
            5,
            "one single-page step per page, no phantom trailing step"
        );
        assert!(steps.last().expect("last").exhausted);
        assert!(steps.last().expect("last").cursor.is_none());
        assert!(
            steps[..4]
                .iter()
                .all(|s| !s.exhausted && s.cursor.is_some()),
            "non-final steps carry a resume cursor"
        );
        // Merged partials reconstruct the unbounded snapshot exactly (dead space via the formula).
        assert_eq!(merge_steps(&steps), store.stats_for_index(None));
    }

    #[test]
    fn stats_step_scoped_counts_global_bytes_but_scopes_logical() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1);
        let mut store = open(&mm);
        // index 1: 2 pages; index 2: 2 pages.
        seed_rows(&mut store, &d, 1, 1, 2);
        store
            .append_row(2, 1, 0, &d, 10, 1, subject(10), &bytes(0.0, 0.0))
            .unwrap();
        store
            .append_row(2, 1, 0, &d, 11, 1, subject(11), &bytes(1.0, 0.0))
            .unwrap();
        let span = page_span(1, d.stride_bytes);

        let steps = collect_steps(&store, 1, Some(1));
        let merged = merge_steps(&steps);
        assert_eq!(merged, store.stats_for_index(Some(1)));
        assert_eq!(
            merged.slab.referenced_page_bytes_global,
            4 * span,
            "whole slab, even pages outside the index filter"
        );
        assert_eq!(merged.scope.page_count, 2, "only index 1 pages scoped");
        assert_eq!(merged.scope.referenced_page_bytes, 2 * span);
        assert!(merged.versions.iter().all(|v| v.index_id == 1));
    }

    #[test]
    fn stats_step_cursor_resumes_without_dup_or_skip() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1);
        let mut store = open(&mm);
        seed_rows(&mut store, &d, 1, 1, 3);

        let steps = collect_steps(&store, 1, None);
        let total_pages: u64 = steps.iter().map(|s| s.partial.scope.page_count).sum();
        assert_eq!(total_pages, 3, "every page counted exactly once");
        assert!(
            steps.iter().all(|s| s.partial.scope.page_count == 1),
            "each single-page step processes exactly one page (no dup)"
        );
        assert_eq!(merge_steps(&steps), store.stats_for_index(None));
    }

    #[test]
    fn stats_step_budget_clamps_low_and_high() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1);
        let mut store = open(&mm);
        seed_rows(&mut store, &d, 1, 1, 3);

        // max_pages = 0 clamps up to 1 -> exactly one page, more remain.
        let low = store.stats_step(None, 0, None).expect("step");
        assert_eq!(low.partial.scope.page_count, 1);
        assert!(!low.exhausted);
        assert!(low.cursor.is_some());

        // A huge budget clamps to MAX_SLAB_STATS_STEP_PAGES; a tiny store still finishes in one step.
        let high = store.stats_step(None, u32::MAX, None).expect("step");
        assert!(high.exhausted);
        assert!(high.cursor.is_none());
        assert_eq!(high.partial.scope.page_count, 3);
    }

    #[test]
    fn stats_step_empty_store_is_exhausted() {
        let mm = fresh_mm();
        let step = open(&mm).stats_step(None, 10, None).expect("step");
        assert!(step.exhausted);
        assert!(step.cursor.is_none());
        assert_eq!(step.partial.scope.page_count, 0);
        assert_eq!(step.partial.slab.referenced_page_bytes_global, 0);
        assert_eq!(step.partial.slab.estimated_unreferenced_bytes, 0);
        assert_eq!(step.partial.slab.occupied_tail_bytes, SLAB_HEADER_LEN);
        assert!(step.partial.versions.is_empty());
    }

    #[test]
    fn stats_step_rejects_malformed_cursor() {
        let mm = fresh_mm();
        let store = open(&mm);
        assert_eq!(
            store.stats_step(Some(vec![0u8; 5]), 10, None).unwrap_err(),
            VectorIndexError::InvalidStatsCursor
        );
        assert_eq!(
            store.stats_step(Some(Vec::new()), 10, None).unwrap_err(),
            VectorIndexError::InvalidStatsCursor
        );
        // A correctly sized cursor decodes to a `PageKey` and is accepted (matches nothing here).
        let ok = store
            .stats_step(Some(vec![0u8; PAGE_KEY_LEN]), 10, None)
            .expect("well-formed cursor");
        assert!(ok.exhausted);
    }

    // --- ADR 0031 Slice 9: bounded partition page-meta health step ---

    /// Drives [`VectorSlabStore::partition_page_health_step`] to exhaustion, returning every step.
    fn collect_health_steps(
        store: &VectorSlabStore,
        index_id: u32,
        active_version: u64,
        max_pages: u32,
    ) -> Vec<VectorPartitionHealthStep> {
        let mut steps = Vec::new();
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            let step = store
                .partition_page_health_step(index_id, active_version, cursor.clone(), max_pages)
                .expect("health step");
            let done = step.exhausted;
            cursor = step.cursor.clone();
            steps.push(step);
            if done {
                break;
            }
        }
        steps
    }

    /// Sums the additive partials per the [`VectorPartitionHealthStep`] merge contract.
    fn merge_health(steps: &[VectorPartitionHealthStep]) -> VectorPartitionPageHealth {
        let first = &steps[0].partial;
        let mut merged = VectorPartitionPageHealth {
            index_id: first.index_id,
            index_version: first.index_version,
            page_count: 0,
            total_rows: 0,
            physical_live_rows: 0,
            tombstoned_rows: 0,
        };
        for step in steps {
            merged.page_count += step.partial.page_count;
            merged.total_rows += step.partial.total_rows;
            merged.physical_live_rows += step.partial.physical_live_rows;
            merged.tombstoned_rows += step.partial.tombstoned_rows;
        }
        merged
    }

    #[test]
    fn health_step_counts_rows_live_and_tombstones() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(2); // 2 slots per page
        let mut store = open(&mm);
        // 3 rows over 2 pages, then tombstone one.
        let s0 = store
            .append_row(1, 1, 0, &d, 1, 1, subject(1), &bytes(0.0, 0.0))
            .unwrap();
        store
            .append_row(1, 1, 0, &d, 2, 1, subject(2), &bytes(1.0, 1.0))
            .unwrap();
        store
            .append_row(1, 1, 0, &d, 3, 1, subject(3), &bytes(2.0, 2.0))
            .unwrap();
        store.tombstone_row(1, s0);

        let steps = collect_health_steps(&store, 1, 1, 100);
        assert_eq!(steps.len(), 1);
        let h = merge_health(&steps);
        assert_eq!(h.index_id, 1);
        assert_eq!(h.index_version, 1);
        assert_eq!(h.page_count, 2);
        assert_eq!(h.total_rows, 3);
        assert_eq!(h.physical_live_rows, 2);
        assert_eq!(h.tombstoned_rows, 1);
    }

    #[test]
    fn health_step_multi_step_merge_matches_single() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1); // one page per row
        let mut store = open(&mm);
        seed_rows(&mut store, &d, 1, 1, 5);

        let single = collect_health_steps(&store, 1, 1, 100);
        assert_eq!(single.len(), 1);
        let stepped = collect_health_steps(&store, 1, 1, 1);
        assert_eq!(
            stepped.len(),
            5,
            "one single-page step per page, no phantom step"
        );
        assert!(stepped.last().expect("last").exhausted);
        assert!(stepped.last().expect("last").cursor.is_none());
        assert!(
            stepped[..4]
                .iter()
                .all(|s| !s.exhausted && s.cursor.is_some()),
            "non-final steps carry a resume cursor"
        );
        assert_eq!(merge_health(&stepped), merge_health(&single));
    }

    #[test]
    fn health_step_scopes_to_active_version_only() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1);
        let mut store = open(&mm);
        // version 1: 2 rows; version 2: 3 rows (e.g. a shadow/old generation).
        seed_rows(&mut store, &d, 1, 1, 2);
        seed_rows(&mut store, &d, 1, 2, 3);

        let v1 = merge_health(&collect_health_steps(&store, 1, 1, 100));
        assert_eq!(v1.index_version, 1);
        assert_eq!(v1.page_count, 2);
        assert_eq!(v1.total_rows, 2);

        let v2 = merge_health(&collect_health_steps(&store, 1, 2, 100));
        assert_eq!(v2.index_version, 2);
        assert_eq!(v2.page_count, 3);
        assert_eq!(v2.total_rows, 3);
    }

    #[test]
    fn health_step_empty_partition_is_valid() {
        clear_heads();
        let mm = fresh_mm();
        let store = open(&mm);
        let steps = collect_health_steps(&store, 1, 1, 10);
        assert_eq!(steps.len(), 1);
        assert!(steps[0].exhausted);
        assert!(steps[0].cursor.is_none());
        let h = &steps[0].partial;
        assert_eq!(
            (
                h.page_count,
                h.total_rows,
                h.physical_live_rows,
                h.tombstoned_rows
            ),
            (0, 0, 0, 0)
        );
    }

    #[test]
    fn health_step_budget_clamps_low_and_high() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1);
        let mut store = open(&mm);
        seed_rows(&mut store, &d, 1, 1, 3);

        // max_pages = 0 clamps to 1 -> one page, more remain.
        let low = store
            .partition_page_health_step(1, 1, None, 0)
            .expect("step");
        assert_eq!(low.partial.page_count, 1);
        assert!(!low.exhausted);
        assert!(low.cursor.is_some());

        // Huge budget clamps to the cap; a tiny store finishes in one step.
        let high = store
            .partition_page_health_step(1, 1, None, u32::MAX)
            .expect("step");
        assert!(high.exhausted);
        assert_eq!(high.partial.page_count, 3);
    }

    #[test]
    fn health_step_rejects_malformed_and_wrong_scope_cursor() {
        clear_heads();
        let mm = fresh_mm();
        let d = def(1);
        let mut store = open(&mm);
        seed_rows(&mut store, &d, 1, 1, 2);

        // Wrong length.
        assert_eq!(
            store
                .partition_page_health_step(1, 1, Some(vec![0u8; 5]), 10)
                .unwrap_err(),
            VectorIndexError::InvalidStatsCursor
        );
        // Well-formed cursor but for a different index -> rejected (scope check).
        let wrong_index = PageKey::new(2, 1, 0, 0).into_bytes();
        assert_eq!(
            store
                .partition_page_health_step(1, 1, Some(wrong_index), 10)
                .unwrap_err(),
            VectorIndexError::InvalidStatsCursor
        );
        // Well-formed cursor but for a different version -> rejected.
        let wrong_version = PageKey::new(1, 2, 0, 0).into_bytes();
        assert_eq!(
            store
                .partition_page_health_step(1, 1, Some(wrong_version), 10)
                .unwrap_err(),
            VectorIndexError::InvalidStatsCursor
        );
        // In-scope well-formed cursor is accepted.
        let ok = PageKey::new(1, 1, 0, 0).into_bytes();
        assert!(store.partition_page_health_step(1, 1, Some(ok), 10).is_ok());
    }
}
