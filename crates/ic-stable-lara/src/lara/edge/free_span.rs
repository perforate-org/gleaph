//! Stable LARA free span store.
//!
//! Free spans are retired physical edge ranges that update and maintenance code can reuse.
//! The store uses size-class bins for best-fit allocation and a paged ordered map for
//! start-order predecessor/successor lookup during coalescing.
//! Best-fit allocation scans size-class bins with intrusive linked lists instead of a
//! secondary `(len, start)` BTree.

use std::{cell::RefCell, fmt};

use ic_stable_paged_ordered_map::StablePagedOrderedMap;
use ic_stable_structures::Memory;

use crate::{GrowFailed, types::Address};

#[cfg(feature = "canbench")]
mod bench;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreeSpan {
    pub start_slot: u64,
    pub len: u64,
}

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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeSpanError {
    EmptySpan,
    SpanOverflow {
        span: FreeSpan,
    },
    DuplicateStart {
        start_slot: u64,
    },
    OverlapPrevious {
        previous: FreeSpan,
        inserted: FreeSpan,
    },
    OverlapNext {
        inserted: FreeSpan,
        next: FreeSpan,
    },
    MissingSpan {
        span: FreeSpan,
    },
    CorruptedFreeList,
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

#[derive(Debug)]
pub enum InitError {
    BadMagic { actual: [u8; 3] },
    IncompatibleVersion(u8),
    InvalidLayout,
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct HeaderV1 {
    pub magic: [u8; 3],
    pub version: u8,
    pub record_slots: u64,
    pub active_count: u64,
    pub free_head: u64,
}

pub struct FreeSpanStore<M: Memory> {
    store: M,
    by_start: RefCell<StablePagedOrderedMap<M>>,
}

impl<M: Memory> FreeSpanStore<M> {
    pub fn new(store: M, by_start_ms: M) -> Result<Self, GrowFailed> {
        write_header(
            &store,
            &HeaderV1 {
                magic: MAGIC,
                version: LAYOUT_VERSION,
                record_slots: 0,
                active_count: 0,
                free_head: 0,
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
        })
    }

    pub fn init(store: M, by_start_ms: M) -> Result<Self, InitError> {
        if store.size() == 0 {
            return Self::new(store, by_start_ms).map_err(|_| InitError::OutOfMemory);
        }
        let header = read_header(&store);
        validate_header(&store, &header)?;
        Ok(Self {
            store,
            by_start: RefCell::new(
                StablePagedOrderedMap::init(by_start_ms).map_err(|_| InitError::OutOfMemory)?,
            ),
        })
    }

    pub fn header(&self) -> HeaderV1 {
        read_header(&self.store)
    }

    pub fn into_memories(self) -> (M, M) {
        (self.store, self.by_start.into_inner().into_memory())
    }

    pub fn len(&self) -> u64 {
        crate::read_u64(&self.store, Address::from(OFFSET_ACTIVE_COUNT))
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn validate(&self) -> Result<(), FreeSpanError> {
        let active = self.read_active_count();
        let mut counted = 0u64;
        let slots = self.read_record_slots();
        let mut reached = std::collections::BTreeSet::new();
        for bin in 0..BIN_COUNT {
            let mut prev = 0;
            let mut cur = self.read_bin_head(bin);
            while cur != 0 {
                if cur > slots || !reached.insert(cur) {
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
                prev = cur;
                cur = rec.next_bin;
            }
        }
        for id in 1..=slots {
            let rec = self.read_record(id);
            if rec.flags == FLAG_ACTIVE {
                counted += 1;
                if !reached.contains(&id) {
                    return Err(FreeSpanError::BinInvariant);
                }
                let got = self.by_start.borrow().get(rec.start_slot);
                if got != Some(id) {
                    return Err(FreeSpanError::BinInvariant);
                }
                let b = size_class(rec.len) as usize;
                if b >= BIN_COUNT || rec.bin_idx as usize != b {
                    return Err(FreeSpanError::BinInvariant);
                }
            }
        }
        if counted != active {
            return Err(FreeSpanError::BinInvariant);
        }
        Ok(())
    }

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
            }
            return Ok(Some(taken));
        }
        Ok(None)
    }

    pub fn free_span_starting_at(&self, start_slot: u64) -> Option<FreeSpan> {
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

    pub fn free_span_ending_at(&self, end_slot: u64) -> Option<FreeSpan> {
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

    pub fn spans(&self) -> Vec<FreeSpan> {
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
        self.write_active_record(id, span)
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
        self.relink_active_record(id, new)
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
}
