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
use gleaph_graph_kernel::vector_index::{VectorIndexError, VectorSubject};
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
}
