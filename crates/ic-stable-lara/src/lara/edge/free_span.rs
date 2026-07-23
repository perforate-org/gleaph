//! Stable LARA free span store.
//!
//! Free spans are retired physical edge ranges that update and maintenance code can reuse.
//! The store uses size-class bins for best-fit allocation and a paged ordered map for
//! start-order predecessor/successor lookup during coalescing.
//! Best-fit allocation scans size-class bins with intrusive linked lists instead of a
//! secondary `(len, start)` BTree.

use std::{
    cell::{Cell, RefCell},
    collections::BinaryHeap,
    fmt,
};

use ic_stable_paged_ordered_map::StablePagedOrderedMap;
use ic_stable_structures::Memory;

use crate::safe_write;
use crate::{GrowFailed, types::Address};

#[cfg(feature = "canbench")]
mod bench;

/// Reusable physical edge-slab span.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreeSpan {
    /// First edge slot in the free physical range.
    pub start_slot: u64,
    /// Number of edge slots in the free physical range.
    pub len: u64,
}

/// Magic bytes that identify LARA free-span metadata.
pub const MAGIC: [u8; 3] = *b"LFS";
/// Layout version byte stored immediately after [`MAGIC`] (same pattern as the array-backed free span store).
const LAYOUT_VERSION: u8 = 1;

/// Exact bins for lengths `1..=64`, then power-of-two ranges `[65,128], [129,256], ...`.
pub const BIN_COUNT: usize = 128;

const OFFSET_MAGIC: u64 = 0;
const OFFSET_VERSION: u64 = 3;
const OFFSET_RECORD_SLOTS: u64 = 8;
const OFFSET_ACTIVE_COUNT: u64 = 16;
const OFFSET_FREE_HEAD: u64 = 24;
const OFFSET_BIN_HEADS: u64 = 32;
const OFFSET_FREE_BYTES: u64 = OFFSET_BIN_HEADS + (BIN_COUNT as u64) * 8;
const OFFSET_LARGEST_FREE_SPAN: u64 = OFFSET_FREE_BYTES + 8;
const MAX_HEAP_STALE_FACTOR: u64 = 2;
const MAX_HEAP_STALE_ALLOWANCE: u64 = 64;

const RECORDS_OFFSET: u64 = 4096;
const RECORD_STRIDE: u64 = 48;
const RECORD_OFFSET_START: u64 = 0;
const RECORD_OFFSET_LEN: u64 = 8;
const RECORD_OFFSET_PREV_BIN: u64 = 16;
const RECORD_OFFSET_NEXT_BIN: u64 = 24;
const RECORD_OFFSET_FLAGS: u64 = 32;
const RECORD_OFFSET_BIN_IDX: u64 = 33;

const FLAG_FREE: u8 = 0;
const FLAG_ACTIVE: u8 = 1;

/// Stable record index; `0` is invalid (sentinel for linked lists).
pub type SpanId = u64;

/// Errors returned by free-span allocation and release operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeSpanError {
    /// A zero-length span was supplied where a reusable span is required.
    EmptySpan,
    /// `start_slot + len` overflowed `u64`.
    SpanOverflow {
        /// Span whose end could not be represented.
        span: FreeSpan,
    },
    /// A span already exists with the same start slot.
    DuplicateStart {
        /// Duplicated start slot.
        start_slot: u64,
    },
    /// The inserted span overlaps the previous span in start order.
    OverlapPrevious {
        /// Existing previous span.
        previous: FreeSpan,
        /// Span being inserted.
        inserted: FreeSpan,
    },
    /// The inserted span overlaps the next span in start order.
    OverlapNext {
        /// Span being inserted.
        inserted: FreeSpan,
        /// Existing next span.
        next: FreeSpan,
    },
    /// An operation expected this exact span, but it was not present.
    MissingSpan {
        /// Missing span.
        span: FreeSpan,
    },
    /// A record free-list pointer was invalid.
    CorruptedFreeList,
    /// A size-class bin or by-start index invariant was violated.
    BinInvariant,
}

impl fmt::Display for FreeSpanError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::EmptySpan => write!(f, "free span length must be greater than zero"),
            Self::SpanOverflow { span } => write!(f, "free span overflows address space: {span:?}"),
            Self::DuplicateStart { start_slot } => {
                write!(f, "free span already exists at start slot {start_slot}")
            }
            Self::OverlapPrevious { previous, inserted } => {
                write!(
                    f,
                    "free span {inserted:?} overlaps previous span {previous:?}"
                )
            }
            Self::OverlapNext { inserted, next } => {
                write!(f, "free span {inserted:?} overlaps next span {next:?}")
            }
            Self::MissingSpan { span } => write!(f, "missing free span {span:?}"),
            Self::CorruptedFreeList => write!(f, "corrupted free record list"),
            Self::BinInvariant => write!(f, "bin linked-list invariant violated"),
        }
    }
}

impl std::error::Error for FreeSpanError {}

/// Errors returned when reopening a persisted free-span store.
#[derive(Debug)]
pub enum InitError {
    /// The memory header does not contain the LARA free-span magic bytes.
    BadMagic {
        /// Magic bytes read from stable memory.
        actual: [u8; 3],
    },
    /// The stored layout version is not supported by this crate version.
    IncompatibleVersion(u8),
    /// The memory does not contain a valid free-span layout.
    InvalidLayout,
    /// The store could not allocate or reopen its metadata.
    OutOfMemory,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::BadMagic { actual } => {
                write!(f, "bad magic number {actual:?}, expected {MAGIC:?}")
            }
            Self::IncompatibleVersion(v) => write!(f, "unsupported free span layout version {v}"),
            Self::InvalidLayout => write!(f, "invalid free span store layout"),
            Self::OutOfMemory => write!(f, "failed to allocate free span store metadata"),
        }
    }
}

impl std::error::Error for InitError {}

/// Maps `len > 0` to a bin index in `0..BIN_COUNT`.
#[inline]
pub fn size_class(len: u64) -> u32 {
    debug_assert!(len > 0);
    if len <= 64 {
        return (len - 1) as u32;
    }
    let mut bin = 64u32;
    let mut hi = 128u64;
    loop {
        if len <= hi {
            return bin;
        }
        bin += 1;
        let next = hi.saturating_mul(2);
        if next == hi {
            return bin;
        }
        hi = next;
    }
}

#[derive(Clone, Copy)]
struct SpanRecord {
    start_slot: u64,
    len: u64,
    prev_bin: SpanId,
    next_bin: SpanId,
    flags: u8,
    bin_idx: u8,
}

/// Persisted V1 free-span store header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    /// Magic bytes, always [`MAGIC`] for this layout.
    pub magic: [u8; 3],
    /// Layout version.
    pub version: u8,
    /// Number of allocated span-record slots.
    pub record_slots: u64,
    /// Number of active free spans.
    pub active_count: u64,
    /// Head of the recycled record free list, or `0` when empty.
    pub free_head: u64,
    /// Total slots represented by active free spans.
    pub free_bytes: u64,
    /// Largest active free span, or zero when the allocator is empty.
    pub largest_free_span: u64,
}

/// O(1) allocator fragmentation statistics persisted by [`FreeSpanStore`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreeSpanAllocatorStats {
    /// Total slots represented by active free spans.
    pub free_bytes: u64,
    /// Largest active free span.
    pub largest_free_span: u64,
    /// Number of active free spans.
    pub free_span_count: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct MaxSpanCandidate {
    len: u64,
    id: SpanId,
}

impl Ord for MaxSpanCandidate {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.len.cmp(&other.len).then(self.id.cmp(&other.id))
    }
}

impl PartialOrd for MaxSpanCandidate {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

/// Stable free-span allocator for retired physical edge-slab ranges.
pub struct FreeSpanStore<M: Memory> {
    store: M,
    by_start: RefCell<StablePagedOrderedMap<M>>,
    max_spans: RefCell<BinaryHeap<MaxSpanCandidate>>,
    max_heap_ready: Cell<bool>,
}

impl<M: Memory> FreeSpanStore<M> {
    /// Creates a fresh free-span store and by-start index.
    pub fn new(store: M, by_start_ms: M) -> Result<Self, GrowFailed> {
        write_header(
            &store,
            &HeaderV1 {
                magic: MAGIC,
                version: LAYOUT_VERSION,
                record_slots: 0,
                active_count: 0,
                free_head: 0,
                free_bytes: 0,
                largest_free_span: 0,
            },
        )?;
        let zeros = [0u8; BIN_COUNT * 8];
        crate::safe_write(&store, OFFSET_BIN_HEADS, &zeros)?;
        Ok(Self {
            store,
            by_start: RefCell::new(StablePagedOrderedMap::new(by_start_ms).map_err(|_| {
                GrowFailed {
                    current_size: 0,
                    delta: 0,
                }
            })?),
            max_spans: RefCell::new(BinaryHeap::new()),
            max_heap_ready: Cell::new(true),
        })
    }

    /// Reopens an existing free-span store and by-start index.
    ///
    /// The records header and the by-start index are paired regions: a fresh
    /// create populates both. Reopening with only one populated (a miswired or
    /// partially lost memory set) is rejected, because a stale or empty by-start
    /// index would make overlap checks miss live spans and let the allocator
    /// hand out the same physical range twice.
    pub fn init(store: M, by_start_ms: M) -> Result<Self, InitError> {
        match crate::classify_composite_init([store.size(), by_start_ms.size()]) {
            crate::CompositeInit::Fresh => {
                return Self::new(store, by_start_ms).map_err(|_| InitError::OutOfMemory);
            }
            crate::CompositeInit::Partial => return Err(InitError::InvalidLayout),
            crate::CompositeInit::Reopen => {}
        }
        let header = read_header(&store);
        validate_header(&store, &header)?;
        let this = Self {
            store,
            by_start: RefCell::new(
                StablePagedOrderedMap::init(by_start_ms).map_err(|_| InitError::OutOfMemory)?,
            ),
            max_spans: RefCell::new(BinaryHeap::new()),
            // The persisted largest-span summary is sufficient for reopen. Rebuild the
            // transient candidate heap only when a mutation needs it.
            max_heap_ready: Cell::new(false),
        };
        // The by-start index must be internally sound, hold exactly one entry per
        // active span, and agree with the records header and size-class bins.
        this.by_start
            .borrow()
            .validate()
            .map_err(|_| InitError::InvalidLayout)?;
        if this.by_start.borrow().len() != header.active_count {
            return Err(InitError::InvalidLayout);
        }
        this.validate().map_err(|_| InitError::InvalidLayout)?;
        Ok(this)
    }

    /// Reads the current free-span store header.
    pub fn header(&self) -> HeaderV1 {
        read_header(&self.store)
    }

    /// Consumes the store and returns `(records_memory, by_start_memory)`.
    pub fn into_memories(self) -> (M, M) {
        (self.store, self.by_start.into_inner().into_memory())
    }

    /// Returns the number of active free spans.
    pub fn len(&self) -> u64 {
        crate::read_u64(&self.store, Address::from(OFFSET_ACTIVE_COUNT))
    }

    /// Returns persisted allocator statistics without scanning active spans.
    pub fn allocator_stats(&self) -> FreeSpanAllocatorStats {
        FreeSpanAllocatorStats {
            free_bytes: crate::read_u64(&self.store, Address::from(OFFSET_FREE_BYTES)),
            largest_free_span: crate::read_u64(
                &self.store,
                Address::from(OFFSET_LARGEST_FREE_SPAN),
            ),
            free_span_count: self.len(),
        }
    }

    /// Returns `true` when no free spans are available.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    #[cfg(test)]
    pub(crate) fn test_by_start_entries(&self) -> Vec<(u64, SpanId)> {
        self.by_start.borrow().iter().collect()
    }

    #[cfg(test)]
    pub(crate) fn test_active_records(&self) -> Vec<(SpanId, u64, u64, SpanId, SpanId, u8, u8)> {
        let header = self.header();
        (1..=header.record_slots)
            .filter_map(|id| {
                let record = self.read_record(id);
                (record.flags == FLAG_ACTIVE).then_some((
                    id,
                    record.start_slot,
                    record.len,
                    record.prev_bin,
                    record.next_bin,
                    record.flags,
                    record.bin_idx,
                ))
            })
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn test_memory_size(&self) -> u64 {
        self.store.size()
    }

    #[cfg(test)]
    pub(crate) fn test_by_start_memory_size(&self) -> u64 {
        self.by_start.borrow().memory_size()
    }

    /// Grows the backing memories so the store can absorb `additional_active`
    /// more released spans without a grow failure during the commit phase.
    ///
    /// This is a preflight primitive for [`release`](Self::release): after a
    /// successful reservation, a sequence of releases adding at most
    /// `additional_active` total spans will not fail with a grow error, provided
    /// no other concurrent mutation interferes.
    pub(crate) fn reserve_for_releases(&self, additional_active: u64) -> Result<(), GrowFailed> {
        let header = self.header();
        let target_slots = header.record_slots.saturating_add(additional_active);
        let need_records =
            RECORDS_OFFSET.saturating_add(target_slots.saturating_mul(RECORD_STRIDE));
        if need_records > self.store.size().saturating_mul(crate::WASM_PAGE_SIZE) {
            safe_write(&self.store, need_records - 1, &[0])?;
        }
        self.by_start
            .borrow()
            .reserve_for_inserts(additional_active)
            .map_err(|err| Self::map_paged_grow_failed(err))?;
        Ok(())
    }

    fn map_paged_grow_failed(err: ic_stable_paged_ordered_map::GrowFailed) -> GrowFailed {
        GrowFailed {
            current_size: err.current_size,
            delta: err.delta,
        }
    }

    /// Checks internal bin, record, and by-start index invariants.
    ///
    /// Strategy (sorted merge against the by-start index):
    /// 1. Walk every size-class bin once, validating each record's flags,
    ///    `bin_idx`, `prev_bin` back-link, and size class, and collect its
    ///    `(start_slot, id)` pair. The walk is bounded by `active_count`, so a
    ///    cycle or duplicate bin link is rejected instead of looping.
    /// 2. Sort the collected pairs by start slot (pure in-heap CPU work).
    /// 3. Walk the by-start index in ascending key order with a sequential
    ///    cursor (no per-key directory search) and require it to reproduce
    ///    exactly the same `(start_slot, id)` pairs.
    ///
    /// Step 3 establishes a bijection between allocatable bin records and
    /// by-start entries: every bin record is indexed at its own start slot, and
    /// the index holds no extra, missing, or mismatched entry. This is the
    /// hazard that matters for correctness — an allocatable range absent from
    /// by-start would let overlap checks miss it and hand the same physical
    /// range out twice.
    ///
    /// Cost is `O(active)` reads plus an `O(active log active)` in-heap sort,
    /// trading the previous `active` random index lookups for one sequential
    /// index scan. The by-start cursor is advanced at most `active + 1` times,
    /// so the routine terminates even if that index were internally cyclic.
    pub fn validate(&self) -> Result<(), FreeSpanError> {
        let active = self.read_active_count();
        let slots = self.read_record_slots();
        // Bounded by the real record count, so a corrupt `active_count` cannot
        // force an oversized allocation here.
        let mut bin_pairs: Vec<(u64, SpanId)> = Vec::with_capacity(active.min(slots) as usize);
        for bin in 0..BIN_COUNT {
            let mut prev = 0;
            let mut cur = self.read_bin_head(bin);
            while cur != 0 {
                // More reachable records than `active_count` means a cycle or a
                // duplicate link; bound the walk instead of a heap visited set.
                if cur > slots || bin_pairs.len() as u64 >= active {
                    return Err(FreeSpanError::BinInvariant);
                }
                let rec = self.read_record(cur);
                if rec.flags != FLAG_ACTIVE
                    || rec.bin_idx as usize != bin
                    || rec.prev_bin != prev
                    || size_class(rec.len) as usize != bin
                {
                    return Err(FreeSpanError::BinInvariant);
                }
                bin_pairs.push((rec.start_slot, cur));
                prev = cur;
                cur = rec.next_bin;
            }
        }
        if bin_pairs.len() as u64 != active {
            return Err(FreeSpanError::BinInvariant);
        }
        bin_pairs.sort_unstable_by_key(|&(start, _)| start);
        let by_start = self.by_start.borrow();
        let mut index = by_start.iter();
        let mut prev_start: Option<u64> = None;
        for &(start, id) in &bin_pairs {
            // Two bin records sharing a start slot cannot both be indexed.
            if prev_start == Some(start) {
                return Err(FreeSpanError::BinInvariant);
            }
            prev_start = Some(start);
            match index.next() {
                Some((k, v)) if k == start && v == id => {}
                _ => return Err(FreeSpanError::BinInvariant),
            }
        }
        // Any leftover index entry is an untracked-record/orphan mismatch.
        if index.next().is_some() {
            return Err(FreeSpanError::BinInvariant);
        }
        Ok(())
    }

    /// Releases `span` back to the allocator, coalescing adjacent spans.
    pub fn release(&self, span: FreeSpan) -> Result<(), FreeSpanError> {
        if span.len == 0 {
            return Err(FreeSpanError::EmptySpan);
        }
        let span_end = span
            .start_slot
            .checked_add(span.len)
            .ok_or(FreeSpanError::SpanOverflow { span })?;
        if self.by_start.borrow().get(span.start_slot).is_some() {
            return Err(FreeSpanError::DuplicateStart {
                start_slot: span.start_slot,
            });
        }

        let prev = self.prev_span(span.start_slot);
        let adjacent_next = self.free_span_starting_at(span_end);
        let next = adjacent_next.or_else(|| self.next_span(span.start_slot));
        let mut merged = span;
        let mut merge_prev = None;
        let mut merge_next = None;

        if let Some(p) = prev {
            let end = p
                .start_slot
                .checked_add(p.len)
                .ok_or(FreeSpanError::SpanOverflow { span: p })?;
            if end > span.start_slot {
                return Err(FreeSpanError::OverlapPrevious {
                    previous: p,
                    inserted: span,
                });
            }
            if end == span.start_slot {
                merged.start_slot = p.start_slot;
                merged.len = merged.len.saturating_add(p.len);
                merge_prev = Some(p);
            }
        }
        if let Some(n) = next {
            if span_end > n.start_slot {
                return Err(FreeSpanError::OverlapNext {
                    inserted: span,
                    next: n,
                });
            }
            if span_end == n.start_slot {
                merged.len = merged.len.saturating_add(n.len);
                merge_next = Some(n);
            }
        }

        match (merge_prev, merge_next) {
            (Some(p), Some(n)) => {
                self.remove_span_exact(n)?;
                self.replace_existing_span(p, merged)?;
            }
            (Some(p), None) => {
                self.replace_existing_span(p, merged)?;
            }
            (None, Some(n)) => {
                self.replace_existing_span(n, merged)?;
            }
            (None, None) => {
                self.insert_span(merged)?;
            }
        }
        Ok(())
    }

    /// Releases a span described by start slot and length.
    pub fn release_span(&self, start_slot: u64, len: u64) -> Result<(), FreeSpanError> {
        self.release(FreeSpan { start_slot, len })
    }

    /// Restores a prefix that was just allocated by [`Self::take_best_fit`].
    ///
    /// The hot path for split allocation leaves the remainder as a free span
    /// starting exactly at `span.end()`. Rejoining through that known successor
    /// avoids the general release path's predecessor/successor range probes.
    pub fn restore_allocated_prefix(&self, span: FreeSpan) -> Result<(), FreeSpanError> {
        if span.len == 0 {
            return Err(FreeSpanError::EmptySpan);
        }
        let span_end = span
            .start_slot
            .checked_add(span.len)
            .ok_or(FreeSpanError::SpanOverflow { span })?;
        if self.by_start.borrow().get(span.start_slot).is_some() {
            return Err(FreeSpanError::DuplicateStart {
                start_slot: span.start_slot,
            });
        }

        if let Some(next) = self.free_span_starting_at(span_end) {
            let merged = FreeSpan {
                start_slot: span.start_slot,
                len: span.len.saturating_add(next.len),
            };
            self.replace_existing_span(next, merged)
        } else {
            self.release(span)
        }
    }

    /// Takes an entire free span of at least `min_len`, without splitting it.
    pub fn take_best_fit_whole(&self, min_len: u64) -> Result<Option<FreeSpan>, FreeSpanError> {
        if min_len == 0 {
            return Ok(None);
        }
        let start_bin = size_class(min_len) as usize;
        for bin in start_bin..BIN_COUNT {
            if let Some((_, span)) = self.pick_span_in_bin(bin as u32, min_len)? {
                self.remove_span_exact(span)?;
                return Ok(Some(span));
            }
        }
        Ok(None)
    }

    /// Returns the current best-fit span without removing it.
    pub fn peek_best_fit(&self, min_len: u64) -> Option<FreeSpan> {
        if min_len == 0 {
            return None;
        }
        let start_bin = size_class(min_len) as usize;
        for bin in start_bin..BIN_COUNT {
            if let Ok(Some((_, span))) = self.pick_span_in_bin(bin as u32, min_len) {
                return Some(span);
            }
        }
        None
    }

    /// Takes exactly `min_len` slots from a best-fit span, splitting the remainder if needed.
    pub fn take_best_fit(&self, min_len: u64) -> Result<Option<FreeSpan>, FreeSpanError> {
        if min_len == 0 {
            return Ok(None);
        }
        let start_bin = size_class(min_len) as usize;
        for bin in start_bin..BIN_COUNT {
            let Some((id, whole)) = self.pick_span_in_bin(bin as u32, min_len)? else {
                continue;
            };
            let taken = FreeSpan {
                start_slot: whole.start_slot,
                len: min_len,
            };
            if whole.len == min_len {
                self.remove_span_exact(whole)?;
            } else {
                let rec = self.read_record(id);
                self.unlink_from_bin(id, rec)?;
                self.by_start
                    .borrow_mut()
                    .remove(whole.start_slot)
                    .expect("paged by_start remove failed");
                let remainder_start = whole
                    .start_slot
                    .checked_add(min_len)
                    .ok_or(FreeSpanError::SpanOverflow { span: taken })?;
                let remainder = FreeSpan {
                    start_slot: remainder_start,
                    len: whole.len - min_len,
                };
                self.by_start
                    .borrow_mut()
                    .insert(remainder.start_slot, id)
                    .expect("paged by_start insert failed");
                self.relink_active_record(id, remainder)?;
                self.record_max_candidate(id, remainder.len);
                self.adjust_summary_after_replace(whole, remainder);
            }
            return Ok(Some(taken));
        }
        Ok(None)
    }

    /// Takes `len` slots from the front of the free span starting at `start_slot`.
    pub fn take_prefix_at(
        &self,
        start_slot: u64,
        len: u64,
    ) -> Result<Option<FreeSpan>, FreeSpanError> {
        if len == 0 {
            return Ok(None);
        }
        let Some(whole) = self.free_span_starting_at(start_slot) else {
            return Ok(None);
        };
        if whole.len < len {
            return Ok(None);
        }

        let taken = FreeSpan { start_slot, len };
        if whole.len == len {
            self.remove_span_exact(whole)?;
        } else {
            let remainder_start = start_slot
                .checked_add(len)
                .ok_or(FreeSpanError::SpanOverflow { span: taken })?;
            self.replace_existing_span(
                whole,
                FreeSpan {
                    start_slot: remainder_start,
                    len: whole.len - len,
                },
            )?;
        }
        Ok(Some(taken))
    }

    /// Returns the active free span that starts exactly at `start_slot`.
    pub fn free_span_starting_at(&self, start_slot: u64) -> Option<FreeSpan> {
        #[cfg(test)]
        super::scan_guard::record_free_span_read();
        let id = self.by_start.borrow().get(start_slot)?;
        let rec = self.read_record(id);
        if rec.flags != FLAG_ACTIVE {
            return None;
        }
        Some(FreeSpan {
            start_slot: rec.start_slot,
            len: rec.len,
        })
    }

    /// Returns the active free span that ends exactly at `end_slot`.
    pub fn free_span_ending_at(&self, end_slot: u64) -> Option<FreeSpan> {
        #[cfg(test)]
        super::scan_guard::record_free_span_read();
        self.by_start
            .borrow()
            .predecessor(end_slot)
            .and_then(|(_, id)| {
                let rec = self.read_record(id);
                if rec.flags != FLAG_ACTIVE {
                    return None;
                }
                let span = FreeSpan {
                    start_slot: rec.start_slot,
                    len: rec.len,
                };
                if span.start_slot.saturating_add(span.len) == end_slot {
                    Some(span)
                } else {
                    None
                }
            })
    }

    /// Returns all active free spans in increasing start-slot order.
    pub fn spans(&self) -> Vec<FreeSpan> {
        #[cfg(test)]
        super::scan_guard::record_free_span_read();
        let by_start = self.by_start.borrow();
        let mut spans = Vec::new();
        let Some((mut start, mut id)) = by_start.first() else {
            return spans;
        };
        loop {
            let rec = self.read_record(id);
            if rec.flags == FLAG_ACTIVE {
                spans.push(FreeSpan {
                    start_slot: rec.start_slot,
                    len: rec.len,
                });
            }
            let Some((next_start, next_id)) = by_start.successor(start) else {
                break;
            };
            start = next_start;
            id = next_id;
        }
        spans
    }

    /// Replaces two exact free spans with one replacement span.
    pub fn replace_exact_pair_with(
        &self,
        left: FreeSpan,
        right: FreeSpan,
        replacement: FreeSpan,
    ) -> Result<(), FreeSpanError> {
        self.ensure_span_exact(left)?;
        self.ensure_span_exact(right)?;
        if replacement.len == 0 {
            return Err(FreeSpanError::EmptySpan);
        }
        replacement
            .start_slot
            .checked_add(replacement.len)
            .ok_or(FreeSpanError::SpanOverflow { span: replacement })?;
        self.ensure_replacement_fits_after_removing(left, right, replacement)?;
        self.remove_span_exact(left)?;
        self.remove_span_exact(right)?;
        self.release(replacement)?;
        Ok(())
    }

    fn ensure_span_exact(&self, span: FreeSpan) -> Result<(), FreeSpanError> {
        match self.free_span_starting_at(span.start_slot) {
            Some(found) if found == span => Ok(()),
            _ => Err(FreeSpanError::MissingSpan { span }),
        }
    }

    fn ensure_replacement_fits_after_removing(
        &self,
        left: FreeSpan,
        right: FreeSpan,
        replacement: FreeSpan,
    ) -> Result<(), FreeSpanError> {
        let replacement_end = replacement
            .start_slot
            .checked_add(replacement.len)
            .ok_or(FreeSpanError::SpanOverflow { span: replacement })?;
        let ignored = [left.start_slot, right.start_slot];
        let prev = {
            let by_start = self.by_start.borrow();
            let mut cursor = replacement.start_slot;
            loop {
                let Some((start, id)) = by_start.predecessor(cursor) else {
                    break None;
                };
                if ignored.contains(&start) {
                    cursor = start;
                } else {
                    break Some(self.read_record(id));
                }
            }
        };
        if let Some(p) = prev {
            let previous = FreeSpan {
                start_slot: p.start_slot,
                len: p.len,
            };
            let prev_end = p
                .start_slot
                .checked_add(p.len)
                .ok_or(FreeSpanError::SpanOverflow { span: previous })?;
            if prev_end > replacement.start_slot {
                return Err(FreeSpanError::OverlapPrevious {
                    previous,
                    inserted: replacement,
                });
            }
        }

        let next = {
            let by_start = self.by_start.borrow();
            let mut cursor = replacement.start_slot;
            loop {
                let Some((start, id)) = by_start.successor(cursor) else {
                    break None;
                };
                if ignored.contains(&start) {
                    cursor = start;
                } else {
                    break Some(self.read_record(id));
                }
            }
        };
        if let Some(n) = next {
            let next = FreeSpan {
                start_slot: n.start_slot,
                len: n.len,
            };
            if replacement_end > next.start_slot {
                return Err(FreeSpanError::OverlapNext {
                    inserted: replacement,
                    next,
                });
            }
        }
        Ok(())
    }

    fn prev_span(&self, start_slot: u64) -> Option<FreeSpan> {
        self.by_start
            .borrow()
            .predecessor(start_slot)
            .and_then(|(_, id)| {
                let rec = self.read_record(id);
                if rec.flags != FLAG_ACTIVE {
                    return None;
                }
                Some(FreeSpan {
                    start_slot: rec.start_slot,
                    len: rec.len,
                })
            })
    }

    fn next_span(&self, start_slot: u64) -> Option<FreeSpan> {
        self.by_start
            .borrow()
            .successor(start_slot)
            .and_then(|(_, id)| {
                let rec = self.read_record(id);
                if rec.flags != FLAG_ACTIVE {
                    return None;
                }
                Some(FreeSpan {
                    start_slot: rec.start_slot,
                    len: rec.len,
                })
            })
    }

    fn pick_span_in_bin(
        &self,
        bin_idx: u32,
        min_len: u64,
    ) -> Result<Option<(SpanId, FreeSpan)>, FreeSpanError> {
        let head = self.read_bin_head(bin_idx as usize);
        if head == 0 {
            return Ok(None);
        }
        let mut best_id: SpanId = 0;
        let mut best_len: u64 = u64::MAX;
        let mut cur = head;
        const MAX_SCAN: u32 = 8;
        let mut scanned = 0;
        while cur != 0 && scanned < MAX_SCAN {
            let rec = self.read_record(cur);
            if rec.flags == FLAG_ACTIVE && rec.len >= min_len && rec.len < best_len {
                best_len = rec.len;
                best_id = cur;
            }
            cur = rec.next_bin;
            scanned += 1;
        }
        // The bounded scan approximates best-fit within the start bin, whose
        // members can be shorter than `min_len`. If it found nothing but records
        // remain unscanned, keep walking for the first fit so a usable span is
        // not skipped, which would otherwise force an unnecessary slab growth.
        // Higher bins always fit on their first member, so this only ever
        // continues within the requested start bin.
        if best_id == 0 {
            while cur != 0 {
                let rec = self.read_record(cur);
                if rec.flags == FLAG_ACTIVE && rec.len >= min_len {
                    best_id = cur;
                    break;
                }
                cur = rec.next_bin;
            }
        }
        if best_id == 0 {
            return Ok(None);
        }
        let rec = self.read_record(best_id);
        Ok(Some((
            best_id,
            FreeSpan {
                start_slot: rec.start_slot,
                len: rec.len,
            },
        )))
    }

    fn insert_span(&self, span: FreeSpan) -> Result<(), FreeSpanError> {
        let id = self.alloc_record()?;
        self.write_active_record(id, span)?;
        self.record_max_candidate(id, span.len);
        self.adjust_summary_after_insert(span.len);
        Ok(())
    }

    fn write_active_record(&self, id: SpanId, span: FreeSpan) -> Result<(), FreeSpanError> {
        let bin_idx = size_class(span.len) as u8;
        let b = bin_idx as usize;
        debug_assert!(b < BIN_COUNT);

        let head = self.read_bin_head(b);
        self.write_record(
            SpanRecord {
                start_slot: span.start_slot,
                len: span.len,
                prev_bin: 0,
                next_bin: head,
                flags: FLAG_ACTIVE,
                bin_idx,
            },
            id,
        )?;
        if head != 0 {
            self.write_record_fields(head, |r| {
                r.prev_bin = id;
            })?;
        }
        self.write_bin_head(b, id)?;
        self.by_start
            .borrow_mut()
            .insert(span.start_slot, id)
            .expect("paged by_start insert failed");
        self.inc_active(1)?;
        Ok(())
    }

    fn relink_active_record(&self, id: SpanId, span: FreeSpan) -> Result<(), FreeSpanError> {
        let bin_idx = size_class(span.len) as u8;
        let b = bin_idx as usize;
        let head = self.read_bin_head(b);
        self.write_record(
            SpanRecord {
                start_slot: span.start_slot,
                len: span.len,
                prev_bin: 0,
                next_bin: head,
                flags: FLAG_ACTIVE,
                bin_idx,
            },
            id,
        )?;
        if head != 0 {
            self.write_record_fields(head, |r| {
                r.prev_bin = id;
            })?;
        }
        self.write_bin_head(b, id)
    }

    fn replace_existing_span(&self, old: FreeSpan, new: FreeSpan) -> Result<(), FreeSpanError> {
        let id = self
            .by_start
            .borrow()
            .get(old.start_slot)
            .ok_or(FreeSpanError::MissingSpan { span: old })?;
        let rec = self.read_record(id);
        if rec.flags != FLAG_ACTIVE || rec.len != old.len || rec.start_slot != old.start_slot {
            return Err(FreeSpanError::MissingSpan { span: old });
        }
        self.unlink_from_bin(id, rec)?;
        if old.start_slot != new.start_slot {
            self.by_start
                .borrow_mut()
                .remove(old.start_slot)
                .expect("paged by_start remove failed");
            self.by_start
                .borrow_mut()
                .insert(new.start_slot, id)
                .expect("paged by_start insert failed");
        }
        self.relink_active_record(id, new)?;
        self.record_max_candidate(id, new.len);
        self.adjust_summary_after_replace(old, new);
        Ok(())
    }

    fn remove_span_exact(&self, span: FreeSpan) -> Result<(), FreeSpanError> {
        let id = self
            .by_start
            .borrow_mut()
            .remove(span.start_slot)
            .expect("paged by_start remove failed")
            .ok_or(FreeSpanError::MissingSpan { span })?;
        let rec = self.read_record(id);
        if rec.flags != FLAG_ACTIVE || rec.len != span.len || rec.start_slot != span.start_slot {
            self.by_start
                .borrow_mut()
                .insert(span.start_slot, id)
                .expect("paged by_start insert failed");
            return Err(FreeSpanError::MissingSpan { span });
        }
        self.unlink_from_bin(id, rec)?;
        self.free_record(id)?;
        self.inc_active(-1)?;
        self.adjust_summary_after_remove(span.len);
        Ok(())
    }

    fn unlink_from_bin(&self, id: SpanId, rec: SpanRecord) -> Result<(), FreeSpanError> {
        let b = rec.bin_idx as usize;
        let head = self.read_bin_head(b);
        if rec.prev_bin != 0 {
            self.write_record_fields(rec.prev_bin, |r| {
                r.next_bin = rec.next_bin;
            })?;
        }
        if rec.next_bin != 0 {
            self.write_record_fields(rec.next_bin, |r| {
                r.prev_bin = rec.prev_bin;
            })?;
        }
        if head == id {
            self.write_bin_head(b, rec.next_bin)?;
        }
        Ok(())
    }

    fn alloc_record(&self) -> Result<SpanId, FreeSpanError> {
        let fh = self.read_free_head();
        if fh != 0 {
            let next_free = crate::read_u64(
                &self.store,
                Address::from(record_offset(fh) + RECORD_OFFSET_START),
            );
            self.write_free_head(next_free)?;
            Ok(fh)
        } else {
            let slots = self.read_record_slots();
            let id = slots.saturating_add(1);
            let need = record_offset(id).saturating_add(RECORD_STRIDE);
            let last_byte = need;
            if last_byte > 0 {
                crate::safe_write(&self.store, last_byte - 1, &[0])
                    .map_err(|_| FreeSpanError::CorruptedFreeList)?;
            }
            self.write_record_slots(id)?;
            Ok(id)
        }
    }

    fn free_record(&self, id: SpanId) -> Result<(), FreeSpanError> {
        let head = self.read_free_head();
        crate::write_u64(
            &self.store,
            Address::from(record_offset(id) + RECORD_OFFSET_START),
            head,
        );
        crate::write_u64(
            &self.store,
            Address::from(record_offset(id) + RECORD_OFFSET_LEN),
            0,
        );
        crate::write_u64(
            &self.store,
            Address::from(record_offset(id) + RECORD_OFFSET_PREV_BIN),
            0,
        );
        crate::write_u64(
            &self.store,
            Address::from(record_offset(id) + RECORD_OFFSET_NEXT_BIN),
            0,
        );
        crate::safe_write(
            &self.store,
            record_offset(id) + RECORD_OFFSET_FLAGS,
            &[FLAG_FREE],
        )
        .map_err(|_| FreeSpanError::CorruptedFreeList)?;
        self.write_free_head(id)?;
        Ok(())
    }

    fn read_record(&self, id: SpanId) -> SpanRecord {
        let off = record_offset(id);
        SpanRecord {
            start_slot: crate::read_u64(&self.store, Address::from(off + RECORD_OFFSET_START)),
            len: crate::read_u64(&self.store, Address::from(off + RECORD_OFFSET_LEN)),
            prev_bin: crate::read_u64(&self.store, Address::from(off + RECORD_OFFSET_PREV_BIN)),
            next_bin: crate::read_u64(&self.store, Address::from(off + RECORD_OFFSET_NEXT_BIN)),
            flags: read_u8_at(&self.store, off + RECORD_OFFSET_FLAGS),
            bin_idx: read_u8_at(&self.store, off + RECORD_OFFSET_BIN_IDX),
        }
    }

    fn write_record(&self, rec: SpanRecord, id: SpanId) -> Result<(), FreeSpanError> {
        let off = record_offset(id);
        crate::write_u64(
            &self.store,
            Address::from(off + RECORD_OFFSET_START),
            rec.start_slot,
        );
        crate::write_u64(&self.store, Address::from(off + RECORD_OFFSET_LEN), rec.len);
        crate::write_u64(
            &self.store,
            Address::from(off + RECORD_OFFSET_PREV_BIN),
            rec.prev_bin,
        );
        crate::write_u64(
            &self.store,
            Address::from(off + RECORD_OFFSET_NEXT_BIN),
            rec.next_bin,
        );
        crate::safe_write(&self.store, off + RECORD_OFFSET_FLAGS, &[rec.flags])
            .map_err(|_| FreeSpanError::CorruptedFreeList)?;
        crate::safe_write(&self.store, off + RECORD_OFFSET_BIN_IDX, &[rec.bin_idx])
            .map_err(|_| FreeSpanError::CorruptedFreeList)?;
        Ok(())
    }

    fn write_record_fields<F>(&self, id: SpanId, f: F) -> Result<(), FreeSpanError>
    where
        F: FnOnce(&mut SpanRecord),
    {
        let mut rec = self.read_record(id);
        f(&mut rec);
        self.write_record(rec, id)
    }

    fn read_bin_head(&self, bin: usize) -> SpanId {
        crate::read_u64(
            &self.store,
            Address::from(OFFSET_BIN_HEADS + bin as u64 * 8),
        )
    }

    fn write_bin_head(&self, bin: usize, head: SpanId) -> Result<(), FreeSpanError> {
        crate::write_u64(
            &self.store,
            Address::from(OFFSET_BIN_HEADS + bin as u64 * 8),
            head,
        );
        Ok(())
    }

    fn read_record_slots(&self) -> u64 {
        crate::read_u64(&self.store, Address::from(OFFSET_RECORD_SLOTS))
    }

    fn write_record_slots(&self, slots: u64) -> Result<(), FreeSpanError> {
        crate::write_u64(&self.store, Address::from(OFFSET_RECORD_SLOTS), slots);
        Ok(())
    }

    fn read_active_count(&self) -> u64 {
        crate::read_u64(&self.store, Address::from(OFFSET_ACTIVE_COUNT))
    }

    fn inc_active(&self, delta: i64) -> Result<(), FreeSpanError> {
        let cur = self.read_active_count();
        let next = if delta >= 0 {
            cur.saturating_add(delta as u64)
        } else {
            cur.saturating_sub((-delta) as u64)
        };
        crate::write_u64(&self.store, Address::from(OFFSET_ACTIVE_COUNT), next);
        Ok(())
    }

    fn read_free_head(&self) -> SpanId {
        crate::read_u64(&self.store, Address::from(OFFSET_FREE_HEAD))
    }

    fn write_free_head(&self, h: SpanId) -> Result<(), FreeSpanError> {
        crate::write_u64(&self.store, Address::from(OFFSET_FREE_HEAD), h);
        Ok(())
    }

    fn adjust_summary_after_insert(&self, len: u64) {
        let free_bytes = crate::read_u64(&self.store, Address::from(OFFSET_FREE_BYTES));
        crate::write_u64(
            &self.store,
            Address::from(OFFSET_FREE_BYTES),
            free_bytes.saturating_add(len),
        );
        let largest = crate::read_u64(&self.store, Address::from(OFFSET_LARGEST_FREE_SPAN));
        if len > largest {
            crate::write_u64(&self.store, Address::from(OFFSET_LARGEST_FREE_SPAN), len);
        }
    }

    fn record_max_candidate(&self, id: SpanId, len: u64) {
        if !self.max_heap_ready.get() {
            self.rebuild_max_heap();
        }
        let should_rebuild = {
            let mut max_spans = self.max_spans.borrow_mut();
            max_spans.push(MaxSpanCandidate { len, id });
            (max_spans.len() as u64)
                > self
                    .len()
                    .saturating_mul(MAX_HEAP_STALE_FACTOR)
                    .saturating_add(MAX_HEAP_STALE_ALLOWANCE)
        };
        if should_rebuild {
            self.rebuild_max_heap();
        }
    }

    fn rebuild_max_heap(&self) {
        let mut max_spans = self.max_spans.borrow_mut();
        max_spans.clear();
        let by_start = self.by_start.borrow();
        let Some((mut start, mut id)) = by_start.first() else {
            self.max_heap_ready.set(true);
            return;
        };
        loop {
            let rec = self.read_record(id);
            if rec.flags == FLAG_ACTIVE {
                max_spans.push(MaxSpanCandidate { len: rec.len, id });
            }
            let Some((next_start, next_id)) = by_start.successor(start) else {
                break;
            };
            start = next_start;
            id = next_id;
        }
        self.max_heap_ready.set(true);
    }

    fn adjust_summary_after_remove(&self, len: u64) {
        let free_bytes = crate::read_u64(&self.store, Address::from(OFFSET_FREE_BYTES));
        crate::write_u64(
            &self.store,
            Address::from(OFFSET_FREE_BYTES),
            free_bytes.saturating_sub(len),
        );
        let largest = crate::read_u64(&self.store, Address::from(OFFSET_LARGEST_FREE_SPAN));
        if len == largest {
            self.rebuild_largest_free_span();
        }
    }

    fn adjust_summary_after_replace(&self, old: FreeSpan, new: FreeSpan) {
        let free_bytes = crate::read_u64(&self.store, Address::from(OFFSET_FREE_BYTES));
        crate::write_u64(
            &self.store,
            Address::from(OFFSET_FREE_BYTES),
            free_bytes.saturating_sub(old.len).saturating_add(new.len),
        );
        let largest = crate::read_u64(&self.store, Address::from(OFFSET_LARGEST_FREE_SPAN));
        if new.len >= largest {
            crate::write_u64(
                &self.store,
                Address::from(OFFSET_LARGEST_FREE_SPAN),
                new.len,
            );
        } else if old.len == largest {
            self.rebuild_largest_free_span();
        }
    }

    fn rebuild_largest_free_span(&self) {
        if !self.max_heap_ready.get() {
            self.rebuild_max_heap();
        }
        let mut max_spans = self.max_spans.borrow_mut();
        while let Some(candidate) = max_spans.peek().copied() {
            let rec = self.read_record(candidate.id);
            if rec.flags == FLAG_ACTIVE && rec.len == candidate.len {
                crate::write_u64(
                    &self.store,
                    Address::from(OFFSET_LARGEST_FREE_SPAN),
                    candidate.len,
                );
                return;
            }
            max_spans.pop();
        }
        crate::write_u64(&self.store, Address::from(OFFSET_LARGEST_FREE_SPAN), 0);
    }
}

fn record_offset(id: SpanId) -> u64 {
    debug_assert!(id != 0);
    RECORDS_OFFSET.saturating_add(id.saturating_sub(1).saturating_mul(RECORD_STRIDE))
}

fn write_header<M: Memory>(memory: &M, h: &HeaderV1) -> Result<(), GrowFailed> {
    crate::safe_write(memory, OFFSET_MAGIC, &h.magic)?;
    crate::safe_write(memory, OFFSET_VERSION, &[h.version])?;
    crate::safe_write(memory, 4, &[0; 4])?;
    crate::write_u64(memory, Address::from(OFFSET_RECORD_SLOTS), h.record_slots);
    crate::write_u64(memory, Address::from(OFFSET_ACTIVE_COUNT), h.active_count);
    crate::write_u64(memory, Address::from(OFFSET_FREE_HEAD), h.free_head);
    crate::write_u64(memory, Address::from(OFFSET_FREE_BYTES), h.free_bytes);
    crate::write_u64(
        memory,
        Address::from(OFFSET_LARGEST_FREE_SPAN),
        h.largest_free_span,
    );
    Ok(())
}

fn read_header<M: Memory>(memory: &M) -> HeaderV1 {
    let mut magic = [0u8; 3];
    let mut version = [0u8; 1];
    memory.read(OFFSET_MAGIC, &mut magic);
    memory.read(OFFSET_VERSION, &mut version);
    HeaderV1 {
        magic,
        version: version[0],
        record_slots: crate::read_u64(memory, Address::from(OFFSET_RECORD_SLOTS)),
        active_count: crate::read_u64(memory, Address::from(OFFSET_ACTIVE_COUNT)),
        free_head: crate::read_u64(memory, Address::from(OFFSET_FREE_HEAD)),
        free_bytes: crate::read_u64(memory, Address::from(OFFSET_FREE_BYTES)),
        largest_free_span: crate::read_u64(memory, Address::from(OFFSET_LARGEST_FREE_SPAN)),
    }
}

fn validate_header<M: Memory>(memory: &M, h: &HeaderV1) -> Result<(), InitError> {
    if h.magic != MAGIC {
        return Err(InitError::BadMagic { actual: h.magic });
    }
    if h.version != LAYOUT_VERSION {
        return Err(InitError::IncompatibleVersion(h.version));
    }
    if h.active_count > h.record_slots || h.free_head > h.record_slots {
        return Err(InitError::InvalidLayout);
    }
    let bytes = memory.size().saturating_mul(65_536);
    let need = if h.record_slots == 0 {
        OFFSET_BIN_HEADS + (BIN_COUNT as u64) * 8
    } else {
        record_offset(h.record_slots).saturating_add(RECORD_STRIDE)
    };
    if bytes < need {
        return Err(InitError::InvalidLayout);
    }
    Ok(())
}

#[inline]
fn read_u8_at<M: Memory>(memory: &M, offset: u64) -> u8 {
    let mut b = [0u8; 1];
    memory.read(offset, &mut b);
    b[0]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FailpointMemory;
    use ic_stable_structures::DefaultMemoryImpl;
    use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};

    fn test_store() -> FreeSpanStore<VirtualMemory<DefaultMemoryImpl>> {
        let m = MemoryManager::init(DefaultMemoryImpl::default());
        FreeSpanStore::init(m.get(MemoryId::new(10)), m.get(MemoryId::new(11))).unwrap()
    }

    fn test_paged_store() -> FreeSpanStore<VirtualMemory<DefaultMemoryImpl>> {
        let m = MemoryManager::init(DefaultMemoryImpl::default());
        FreeSpanStore::init(m.get(MemoryId::new(40)), m.get(MemoryId::new(41))).unwrap()
    }

    fn assert_allocator_stats_match_spans<M: Memory>(store: &FreeSpanStore<M>) {
        let spans = store.spans();
        assert_eq!(
            store.allocator_stats(),
            FreeSpanAllocatorStats {
                free_bytes: spans.iter().map(|span| span.len).sum(),
                largest_free_span: spans.iter().map(|span| span.len).max().unwrap_or(0),
                free_span_count: spans.len() as u64,
            }
        );
    }

    #[test]
    fn binned_release_coalesces_three() {
        let s = test_store();
        s.release_span(100, 20).unwrap();
        s.release_span(140, 10).unwrap();
        s.release_span(120, 20).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(
            s.free_span_starting_at(100),
            Some(FreeSpan {
                start_slot: 100,
                len: 50
            })
        );
        s.validate().unwrap();
    }

    #[test]
    fn binned_paged_release_coalesces_three() {
        let s = test_paged_store();
        s.release_span(100, 20).unwrap();
        s.release_span(140, 10).unwrap();
        s.release_span(120, 20).unwrap();
        assert_eq!(s.len(), 1);
        assert_eq!(
            s.free_span_starting_at(100),
            Some(FreeSpan {
                start_slot: 100,
                len: 50
            })
        );
        s.validate().unwrap();
    }

    #[test]
    fn binned_take_best_fit_splits() {
        let s = test_store();
        s.release_span(1000, 80).unwrap();
        s.release_span(2000, 32).unwrap();
        assert_eq!(
            s.take_best_fit(40).unwrap(),
            Some(FreeSpan {
                start_slot: 1000,
                len: 40
            })
        );
        assert_eq!(
            s.free_span_starting_at(1040),
            Some(FreeSpan {
                start_slot: 1040,
                len: 40
            })
        );
        s.validate().unwrap();
    }

    #[test]
    fn allocator_stats_track_mutations_and_reopen() {
        let m = MemoryManager::init(DefaultMemoryImpl::default());
        let meta = m.get(MemoryId::new(60));
        let bs = m.get(MemoryId::new(61));
        let s = FreeSpanStore::init(meta, bs).unwrap();
        s.release_span(1000, 80).unwrap();
        s.release_span(2000, 32).unwrap();
        assert_eq!(
            s.allocator_stats(),
            FreeSpanAllocatorStats {
                free_bytes: 112,
                largest_free_span: 80,
                free_span_count: 2,
            }
        );
        s.take_prefix_at(1000, 24).unwrap();
        assert_eq!(
            s.allocator_stats(),
            FreeSpanAllocatorStats {
                free_bytes: 88,
                largest_free_span: 56,
                free_span_count: 2,
            }
        );
        let (meta, bs) = s.into_memories();
        let reopened = FreeSpanStore::init(meta, bs).unwrap();
        assert_eq!(
            reopened.allocator_stats(),
            FreeSpanAllocatorStats {
                free_bytes: 88,
                largest_free_span: 56,
                free_span_count: 2,
            }
        );
        assert_eq!(reopened.take_prefix_at(2000, 32).unwrap().unwrap().len, 32);
        assert_eq!(reopened.take_prefix_at(1024, 56).unwrap().unwrap().len, 56);
        assert_eq!(reopened.allocator_stats().largest_free_span, 0);
    }

    #[test]
    fn allocator_summary_matches_every_mutation_shape() {
        let s = test_store();

        s.release_span(100, 10).unwrap();
        assert_allocator_stats_match_spans(&s);
        s.release_span(120, 30).unwrap();
        assert_allocator_stats_match_spans(&s);
        s.release_span(110, 10).unwrap();
        assert_allocator_stats_match_spans(&s);

        let taken = s.take_best_fit(8).unwrap().unwrap();
        assert_allocator_stats_match_spans(&s);
        s.restore_allocated_prefix(taken).unwrap();
        assert_allocator_stats_match_spans(&s);

        let taken = s.take_prefix_at(100, 20).unwrap().unwrap();
        assert_allocator_stats_match_spans(&s);
        assert_eq!(taken.len, 20);

        s.release_span(200, 10).unwrap();
        assert_allocator_stats_match_spans(&s);
        s.replace_exact_pair_with(
            FreeSpan {
                start_slot: 120,
                len: 30,
            },
            FreeSpan {
                start_slot: 200,
                len: 10,
            },
            FreeSpan {
                start_slot: 120,
                len: 90,
            },
        )
        .unwrap();
        assert_allocator_stats_match_spans(&s);
    }

    #[test]
    fn max_heap_rebuild_bounds_stale_candidates() {
        let s = test_store();
        s.release_span(0, 10_000).unwrap();

        for i in 0..256u64 {
            let start = 1_000_000u64.saturating_add(i.saturating_mul(1_000));
            s.release_span(start, 1).unwrap();
            assert_eq!(
                s.take_prefix_at(start, 1).unwrap(),
                Some(FreeSpan {
                    start_slot: start,
                    len: 1,
                })
            );
        }

        let heap_len = s.max_spans.borrow().len() as u64;
        assert!(
            heap_len
                <= s.len()
                    .saturating_mul(MAX_HEAP_STALE_FACTOR)
                    .saturating_add(MAX_HEAP_STALE_ALLOWANCE),
            "max heap retained too many stale candidates: {heap_len}"
        );
        assert_allocator_stats_match_spans(&s);
    }

    #[test]
    fn binned_take_prefix_at_splits_exact_start() {
        let s = test_store();
        s.release_span(1000, 80).unwrap();
        s.release_span(2000, 32).unwrap();
        assert_eq!(
            s.take_prefix_at(1000, 24).unwrap(),
            Some(FreeSpan {
                start_slot: 1000,
                len: 24
            })
        );
        assert_eq!(
            s.free_span_starting_at(1024),
            Some(FreeSpan {
                start_slot: 1024,
                len: 56
            })
        );
        assert_eq!(
            s.free_span_starting_at(2000),
            Some(FreeSpan {
                start_slot: 2000,
                len: 32
            })
        );
        s.validate().unwrap();
    }

    #[test]
    fn binned_take_prefix_at_requires_exact_start_and_sufficient_len() {
        let s = test_store();
        s.release_span(1000, 80).unwrap();
        assert_eq!(s.take_prefix_at(1001, 24).unwrap(), None);
        assert_eq!(s.take_prefix_at(1000, 81).unwrap(), None);
        assert_eq!(
            s.free_span_starting_at(1000),
            Some(FreeSpan {
                start_slot: 1000,
                len: 80
            })
        );
        s.validate().unwrap();
    }

    #[test]
    fn binned_restore_allocated_prefix_rejoins_remainder() {
        let s = test_store();
        s.release_span(1000, 80).unwrap();
        let span = s.take_best_fit(40).unwrap().unwrap();
        assert_eq!(
            s.free_span_starting_at(1040),
            Some(FreeSpan {
                start_slot: 1040,
                len: 40
            })
        );
        s.restore_allocated_prefix(span).unwrap();
        assert_eq!(
            s.free_span_starting_at(1000),
            Some(FreeSpan {
                start_slot: 1000,
                len: 80
            })
        );
        s.validate().unwrap();
    }

    #[test]
    fn binned_paged_take_split_restore_and_reopen() {
        let m = MemoryManager::init(DefaultMemoryImpl::default());
        let meta = m.get(MemoryId::new(50));
        let bs = m.get(MemoryId::new(51));
        let s = FreeSpanStore::init(meta, bs).unwrap();
        s.release_span(1000, 80).unwrap();
        s.release_span(2000, 32).unwrap();
        let span = s.take_best_fit(40).unwrap().unwrap();
        assert_eq!(
            s.free_span_starting_at(1040),
            Some(FreeSpan {
                start_slot: 1040,
                len: 40
            })
        );
        s.restore_allocated_prefix(span).unwrap();
        let (meta, bs) = s.into_memories();
        let reopened = FreeSpanStore::init(meta, bs).unwrap();
        assert_eq!(
            reopened.free_span_starting_at(1000),
            Some(FreeSpan {
                start_slot: 1000,
                len: 80
            })
        );
        assert_eq!(
            reopened.free_span_ending_at(1080),
            Some(FreeSpan {
                start_slot: 1000,
                len: 80
            })
        );
        reopened.validate().unwrap();
    }

    #[test]
    fn binned_take_best_fit_finds_fit_beyond_bounded_scan() {
        let s = test_store();
        // A fitting span in the [65,128] size class, released first so later
        // releases bury it past the bounded best-fit scan window.
        s.release_span(0, 120).unwrap();
        // Eight non-fitting spans (len 99) in the same size class, each linked at
        // the bin-list head, pushing the 120-length span to the ninth position.
        for i in 0..8u64 {
            s.release_span(1000 + i * 200, 99).unwrap();
        }
        assert_eq!(
            s.take_best_fit(100).unwrap(),
            Some(FreeSpan {
                start_slot: 0,
                len: 100
            })
        );
        assert_eq!(
            s.free_span_starting_at(100),
            Some(FreeSpan {
                start_slot: 100,
                len: 20
            })
        );
        s.validate().unwrap();
    }

    #[test]
    fn binned_reopen_rejects_one_sided_region_loss() {
        let m = MemoryManager::init(DefaultMemoryImpl::default());
        {
            let s =
                FreeSpanStore::init(m.get(MemoryId::new(60)), m.get(MemoryId::new(61))).unwrap();
            s.release_span(100, 20).unwrap();
            s.release_span(500, 30).unwrap();
        }
        // Records populated (id 60), by-start lost (empty id 62): reopening must
        // fail rather than open an empty index that hides live spans.
        assert!(matches!(
            FreeSpanStore::init(m.get(MemoryId::new(60)), m.get(MemoryId::new(62))),
            Err(InitError::InvalidLayout)
        ));
        // Records lost (empty id 63), by-start populated (id 61): also rejected.
        assert!(matches!(
            FreeSpanStore::init(m.get(MemoryId::new(63)), m.get(MemoryId::new(61))),
            Err(InitError::InvalidLayout)
        ));
        // Both original regions still reopen cleanly.
        let reopened =
            FreeSpanStore::init(m.get(MemoryId::new(60)), m.get(MemoryId::new(61))).unwrap();
        assert_eq!(reopened.len(), 2);
        reopened.validate().unwrap();
    }

    #[test]
    fn binned_take_best_fit_zero_returns_none() {
        let s = test_store();
        s.release_span(1000, 80).unwrap();
        assert_eq!(s.take_best_fit(0).unwrap(), None);
        assert_eq!(
            s.free_span_starting_at(1000),
            Some(FreeSpan {
                start_slot: 1000,
                len: 80
            })
        );
    }

    #[test]
    fn binned_reopen_preserves_state() {
        let m = MemoryManager::init(DefaultMemoryImpl::default());
        let meta = m.get(MemoryId::new(30));
        let bs = m.get(MemoryId::new(31));
        let s = FreeSpanStore::init(meta, bs).unwrap();
        s.release_span(42, 100).unwrap();
        s.release_span(500, 25).unwrap();
        assert_eq!(s.len(), 2);
        let (meta, bs) = s.into_memories();
        let s2 = FreeSpanStore::init(meta, bs).unwrap();
        assert_eq!(s2.len(), 2);
        assert_eq!(
            s2.free_span_starting_at(42),
            Some(FreeSpan {
                start_slot: 42,
                len: 100
            })
        );
        s2.validate().unwrap();
    }

    #[test]
    fn binned_duplicate_start_rejected() {
        let s = test_store();
        s.release_span(1, 5).unwrap();
        assert_eq!(
            s.release_span(1, 3),
            Err(FreeSpanError::DuplicateStart { start_slot: 1 })
        );
    }

    #[test]
    fn binned_overlap_next_rejected() {
        let s = test_store();
        s.release_span(10, 10).unwrap();
        s.release_span(30, 10).unwrap();
        assert_eq!(
            s.release_span(25, 10),
            Err(FreeSpanError::OverlapNext {
                inserted: FreeSpan {
                    start_slot: 25,
                    len: 10
                },
                next: FreeSpan {
                    start_slot: 30,
                    len: 10
                }
            })
        );
    }

    #[test]
    fn binned_free_span_ending_at_lookup() {
        let s = test_store();
        s.release_span(100, 17).unwrap();
        assert_eq!(
            s.free_span_ending_at(117),
            Some(FreeSpan {
                start_slot: 100,
                len: 17
            })
        );
        assert!(s.free_span_ending_at(116).is_none());
    }

    #[test]
    fn binned_replace_pair() {
        let s = test_store();
        s.release_span(6, 4).unwrap();
        s.release_span(13, 5).unwrap();
        let left = s.free_span_ending_at(10).unwrap();
        let right = s.free_span_starting_at(13).unwrap();
        s.replace_exact_pair_with(
            left,
            right,
            FreeSpan {
                start_slot: 6,
                len: 12,
            },
        )
        .unwrap();
        assert_eq!(
            s.free_span_starting_at(6),
            Some(FreeSpan {
                start_slot: 6,
                len: 12
            })
        );
        s.validate().unwrap();
    }

    #[test]
    fn binned_replace_pair_is_non_destructive_on_missing_right() {
        let s = test_store();
        let left = FreeSpan {
            start_slot: 6,
            len: 4,
        };
        s.release(left).unwrap();
        let err = s
            .replace_exact_pair_with(
                left,
                FreeSpan {
                    start_slot: 13,
                    len: 5,
                },
                FreeSpan {
                    start_slot: 6,
                    len: 12,
                },
            )
            .unwrap_err();
        assert_eq!(
            err,
            FreeSpanError::MissingSpan {
                span: FreeSpan {
                    start_slot: 13,
                    len: 5,
                }
            }
        );
        assert_eq!(s.free_span_starting_at(6), Some(left));
        s.validate().unwrap();
    }

    #[test]
    fn binned_release_rejects_overflowing_span() {
        let s = test_store();
        let span = FreeSpan {
            start_slot: u64::MAX,
            len: 1,
        };
        assert_eq!(s.release(span), Err(FreeSpanError::SpanOverflow { span }));
    }

    #[test]
    fn reserve_for_releases_fails_atomically_on_records_grow() {
        let store_mem = FailpointMemory::new();
        let by_start_mem = FailpointMemory::new();
        let store = FreeSpanStore::new(store_mem.clone(), by_start_mem.clone()).unwrap();
        store.release_span(0, 10).unwrap();
        let active = store.len();

        store_mem.fail_at_grow(store_mem.grow_count().saturating_add(1));
        let result = store.reserve_for_releases(10_000);
        assert!(result.is_err(), "expected records-memory grow to fail");
        assert_eq!(store.len(), active, "no new active spans may be recorded");
        assert_allocator_stats_match_spans(&store);

        store_mem.fail_never();
        store.reserve_for_releases(10_000).unwrap();
        // The store must still accept a real release after the reservation.
        store.release_span(20, 5).unwrap();
        assert_eq!(store.len(), active + 1);
        assert_allocator_stats_match_spans(&store);
    }

    #[test]
    fn reserve_for_releases_fails_atomically_on_by_start_grow() {
        let store_mem = FailpointMemory::new();
        let by_start_mem = FailpointMemory::new();
        let store = FreeSpanStore::new(store_mem.clone(), by_start_mem.clone()).unwrap();
        store.release_span(0, 10).unwrap();
        let active = store.len();

        by_start_mem.fail_at_grow(by_start_mem.grow_count().saturating_add(1));
        let result = store.reserve_for_releases(10_000);
        assert!(result.is_err(), "expected by-start-memory grow to fail");
        assert_eq!(store.len(), active, "no new active spans may be recorded");
        assert_allocator_stats_match_spans(&store);

        by_start_mem.fail_never();
        store.reserve_for_releases(10_000).unwrap();
        store.release_span(30, 5).unwrap();
        assert_eq!(store.len(), active + 1);
        assert_allocator_stats_match_spans(&store);
    }
}
