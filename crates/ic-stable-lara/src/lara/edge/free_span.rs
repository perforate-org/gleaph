//! Stable LARA free span store.
//!
//! Free spans are retired physical edge ranges that update and maintenance code can reuse.
//! The store uses size-class bins for best-fit allocation and a paged ordered map for
//! start-order predecessor/successor lookup during coalescing.
//! Best-fit allocation scans size-class bins with intrusive linked lists instead of a
//! secondary `(len, start)` BTree.

use std::{
    cell::{Cell, RefCell, RefMut},
    collections::BinaryHeap,
    fmt,
};

use ic_stable_paged_ordered_map::StablePagedOrderedMap;
use ic_stable_structures::Memory;

use crate::lara::reserved::{region_is_zero, write_zeroes};
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

/// Destination policy for one physical ownership replacement.
#[derive(Clone, Copy, Debug)]
pub(crate) enum PhysicalSpanSelection {
    BestFit(u64),
    ExactPrefixAt(u64, u64),
    NoAllocation,
}

/// Canonical reads used by physical cover subtraction, including kept holes.
/// This is a source witness, not a second allocator or an index reader facade.
enum PhysicalSpanSource {
    Header(HeaderV1),
    Record(SpanId, [u8; RECORD_STRIDE as usize]),
    BeforeOrAt(u64, Option<(u64, SpanId)>),
    After(u64, Option<(u64, SpanId)>),
    Exact(u64, Option<SpanId>),
    Bin(usize, SpanId),
}

/// Magic bytes that identify LARA free-span metadata.
pub const MAGIC: [u8; 3] = *b"LFS";
/// Layout version byte stored immediately after [`MAGIC`] (same pattern as the array-backed free span store).
const LAYOUT_VERSION: u8 = 1;

/// Exact bins for lengths `1..=64`, then power-of-two ranges `[65,128], [129,256], ...`.
pub const BIN_COUNT: usize = 128;

const OFFSET_MAGIC: u64 = 0;
const OFFSET_VERSION: u64 = 3;
/// Declared reserved header bytes between the version byte and [`OFFSET_RECORD_SLOTS`].
const OFFSET_RESERVED: u64 = 4;
const RESERVED_SIZE: usize = 4;
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
    /// Backing memory or transient edit scratch could not be reserved.
    OutOfMemory,
    /// The by-start directory cannot admit another page.
    IndexFull,
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
            Self::OutOfMemory => write!(f, "failed to reserve free-span edit memory"),
            Self::IndexFull => write!(f, "free-span by-start directory is full"),
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

#[derive(Clone, Copy, Default)]
struct SpanRecord {
    start_slot: u64,
    len: u64,
    prev_bin: SpanId,
    next_bin: SpanId,
    flags: u8,
    bin_idx: u8,
}

impl SpanRecord {
    fn decode(bytes: &[u8; RECORD_STRIDE as usize]) -> Self {
        let field = |offset: u64| {
            let mut value = [0; 8];
            value.copy_from_slice(&bytes[offset as usize..offset as usize + 8]);
            u64::from_le_bytes(value)
        };
        Self {
            start_slot: field(RECORD_OFFSET_START),
            len: field(RECORD_OFFSET_LEN),
            prev_bin: field(RECORD_OFFSET_PREV_BIN),
            next_bin: field(RECORD_OFFSET_NEXT_BIN),
            flags: bytes[RECORD_OFFSET_FLAGS as usize],
            bin_idx: bytes[RECORD_OFFSET_BIN_IDX as usize],
        }
    }

    fn encode(self) -> [u8; RECORD_STRIDE as usize] {
        let mut bytes = [0; RECORD_STRIDE as usize];
        bytes[RECORD_OFFSET_START as usize..RECORD_OFFSET_START as usize + 8]
            .copy_from_slice(&self.start_slot.to_le_bytes());
        bytes[RECORD_OFFSET_LEN as usize..RECORD_OFFSET_LEN as usize + 8]
            .copy_from_slice(&self.len.to_le_bytes());
        bytes[RECORD_OFFSET_PREV_BIN as usize..RECORD_OFFSET_PREV_BIN as usize + 8]
            .copy_from_slice(&self.prev_bin.to_le_bytes());
        bytes[RECORD_OFFSET_NEXT_BIN as usize..RECORD_OFFSET_NEXT_BIN as usize + 8]
            .copy_from_slice(&self.next_bin.to_le_bytes());
        bytes[RECORD_OFFSET_FLAGS as usize] = self.flags;
        bytes[RECORD_OFFSET_BIN_IDX as usize] = self.bin_idx;
        bytes
    }
}

/// Private prospective free-record/bin state for one span replacement. No stable writes.
/// The index's existing prepared-edit owner admits the corresponding key changes.
struct FreeSpanEdit<'a, M: Memory> {
    source: &'a FreeSpanStore<M>,
    index: RefMut<'a, StablePagedOrderedMap<M>>,
    // Empty for strict/immediate edits; physical edits retain source facts in
    // fallible scratch, not a large inline payload on the shared hot path.
    physical_source: Vec<PhysicalSpanSource>,
    taken_len: u64,
    #[cfg(test)]
    retired: Vec<FreeSpan>,
    // Captured source-row bound; `header.record_slots` is the prospective bound.
    source_slots: u64,
    header: HeaderV1,
    records: Vec<(SpanId, [u8; RECORD_STRIDE as usize])>,
    bin_heads: Vec<(u64, [u8; 8])>,
    index_edits: Vec<ic_stable_paged_ordered_map::Edit>,
    previous: Vec<Option<SpanId>>,
    first_candidate: Option<MaxSpanCandidate>,
    candidates: Vec<MaxSpanCandidate>,
    maximum_removed: bool,
}

impl<'a, M: Memory> FreeSpanEdit<'a, M> {
    fn new(
        source: &'a FreeSpanStore<M>,
        removals: usize,
        insertions: usize,
    ) -> Result<Self, FreeSpanError> {
        let index = source.by_start.borrow_mut();
        let header = source.header();
        let extent = record_extent(header.record_slots).ok_or(FreeSpanError::CorruptedFreeList)?;
        let backed = source
            .store
            .size()
            .checked_mul(crate::WASM_PAGE_SIZE)
            .ok_or(FreeSpanError::CorruptedFreeList)?;
        if extent > backed
            || header.active_count > header.record_slots
            || header.free_head > header.record_slots
        {
            return Err(FreeSpanError::CorruptedFreeList);
        }
        let mut edit = Self {
            source,
            index,
            physical_source: Vec::new(),
            taken_len: 0,
            #[cfg(test)]
            retired: Vec::new(),
            source_slots: header.record_slots,
            header,
            records: Vec::new(),
            bin_heads: Vec::new(),
            index_edits: Vec::new(),
            previous: Vec::new(),
            first_candidate: None,
            candidates: Vec::new(),
            maximum_removed: false,
        };
        edit.reserve_changes(removals, insertions)?;
        Ok(edit)
    }

    // Keep bounded scratch admission in its constructor without an edit-wide
    // call boundary on the shared immediate span path.
    #[inline(always)]
    fn reserve_changes(&mut self, removals: usize, insertions: usize) -> Result<(), FreeSpanError> {
        // Each unlink changes at most three records; each insertion changes two.
        let records = removals
            .checked_mul(3)
            .and_then(|n| {
                insertions
                    .checked_mul(2)
                    .and_then(|added| n.checked_add(added))
            })
            .ok_or(FreeSpanError::OutOfMemory)?;
        let changes = removals
            .checked_add(insertions)
            .ok_or(FreeSpanError::OutOfMemory)?;
        self.records
            .try_reserve(records)
            .map_err(|_| FreeSpanError::OutOfMemory)?;
        self.bin_heads
            .try_reserve(changes)
            .map_err(|_| FreeSpanError::OutOfMemory)?;
        self.index_edits
            .try_reserve(changes)
            .map_err(|_| FreeSpanError::OutOfMemory)?;
        self.previous
            .try_reserve(changes)
            .map_err(|_| FreeSpanError::OutOfMemory)?;
        self.candidates
            .try_reserve(insertions.saturating_sub(1))
            .map_err(|_| FreeSpanError::OutOfMemory)?;
        Ok(())
    }

    fn candidates(&self) -> impl Iterator<Item = &MaxSpanCandidate> {
        self.first_candidate.iter().chain(&self.candidates)
    }

    fn record(&self, id: SpanId) -> Result<SpanRecord, FreeSpanError> {
        match self.records.iter().find(|(key, _)| *key == id) {
            Some((_, record)) => Ok(SpanRecord::decode(record)),
            None if id == 0 || id > self.source_slots => Err(FreeSpanError::CorruptedFreeList),
            // The constructor proved the entire captured source-row extent backed.
            None => Ok(self.source.read_record(id)),
        }
    }

    fn set_record(&mut self, id: SpanId, record: SpanRecord) {
        let encoded = record.encode();
        if let Some((_, current)) = self.records.iter_mut().find(|(key, _)| *key == id) {
            *current = encoded;
        } else {
            self.records.push((id, encoded));
        }
    }

    fn bin_head(&self, bin: u8) -> SpanId {
        self.bin_heads
            .iter()
            .find(|(key, _)| *key == u64::from(bin))
            .map_or_else(
                || self.source.read_bin_head(usize::from(bin)),
                |(_, head)| u64::from_le_bytes(*head),
            )
    }

    fn set_bin_head(&mut self, bin: u8, head: SpanId) {
        let encoded = head.to_le_bytes();
        if let Some((_, current)) = self
            .bin_heads
            .iter_mut()
            .find(|(key, _)| *key == u64::from(bin))
        {
            *current = encoded;
        } else {
            self.bin_heads.push((u64::from(bin), encoded));
        }
    }

    fn remove(&mut self, span: FreeSpan) -> Result<(), FreeSpanError> {
        let id = self
            .indexed_id(span.start_slot)
            .ok_or(FreeSpanError::MissingSpan { span })?;
        let record = self.record(id)?;
        if record.flags != FLAG_ACTIVE
            || record.start_slot != span.start_slot
            || record.len != span.len
        {
            return Err(FreeSpanError::MissingSpan { span });
        }
        if span.len == 0
            || usize::from(record.bin_idx) >= BIN_COUNT
            || u32::from(record.bin_idx) != size_class(span.len)
            || record.prev_bin == id
            || record.next_bin == id
            || (record.prev_bin != 0 && record.prev_bin == record.next_bin)
        {
            return Err(FreeSpanError::BinInvariant);
        }
        if record.prev_bin == 0 {
            if self.bin_head(record.bin_idx) != id {
                return Err(FreeSpanError::BinInvariant);
            }
            self.set_bin_head(record.bin_idx, record.next_bin);
        } else {
            let mut previous = self.record(record.prev_bin)?;
            if previous.flags != FLAG_ACTIVE
                || previous.bin_idx != record.bin_idx
                || previous.next_bin != id
            {
                return Err(FreeSpanError::BinInvariant);
            }
            previous.next_bin = record.next_bin;
            self.set_record(record.prev_bin, previous);
        }
        if record.next_bin != 0 {
            let mut next = self.record(record.next_bin)?;
            if next.flags != FLAG_ACTIVE || next.bin_idx != record.bin_idx || next.prev_bin != id {
                return Err(FreeSpanError::BinInvariant);
            }
            next.prev_bin = record.prev_bin;
            self.set_record(record.next_bin, next);
        }
        self.set_record(
            id,
            SpanRecord {
                start_slot: self.header.free_head,
                bin_idx: record.bin_idx,
                ..SpanRecord::default()
            },
        );
        self.header.free_head = id;
        self.header.active_count = self
            .header
            .active_count
            .checked_sub(1)
            .ok_or(FreeSpanError::BinInvariant)?;
        self.header.free_bytes = self
            .header
            .free_bytes
            .checked_sub(span.len)
            .ok_or(FreeSpanError::BinInvariant)?;
        self.index_edits
            .push(ic_stable_paged_ordered_map::Edit::Remove {
                key: span.start_slot,
            });
        self.previous.push(Some(id));
        self.first_candidate = self.first_candidate.filter(|candidate| candidate.id != id);
        self.candidates.retain(|candidate| candidate.id != id);
        self.maximum_removed |= span.len == self.header.largest_free_span;
        Ok(())
    }

    fn insert(&mut self, span: FreeSpan) -> Result<(), FreeSpanError> {
        checked_span_end(span)?;
        // Paired-index preparation admits the insertion and checks its exact prior value.
        let id = if self.header.free_head == 0 {
            self.header.record_slots = self
                .header
                .record_slots
                .checked_add(1)
                .ok_or(FreeSpanError::CorruptedFreeList)?;
            self.header.record_slots
        } else {
            let id = self.header.free_head;
            let free = self.record(id)?;
            if free.flags != FLAG_FREE
                || free.start_slot == id
                || free.start_slot > self.header.record_slots
            {
                return Err(FreeSpanError::CorruptedFreeList);
            }
            self.header.free_head = free.start_slot;
            id
        };
        let bin = size_class(span.len) as u8;
        let head = self.bin_head(bin);
        if head != 0 {
            let mut first = self.record(head)?;
            if first.flags != FLAG_ACTIVE || first.bin_idx != bin || first.prev_bin != 0 {
                return Err(FreeSpanError::BinInvariant);
            }
            first.prev_bin = id;
            self.set_record(head, first);
        }
        self.set_record(
            id,
            SpanRecord {
                start_slot: span.start_slot,
                len: span.len,
                prev_bin: 0,
                next_bin: head,
                flags: FLAG_ACTIVE,
                bin_idx: bin,
            },
        );
        self.set_bin_head(bin, id);
        self.header.active_count = self
            .header
            .active_count
            .checked_add(1)
            .ok_or(FreeSpanError::BinInvariant)?;
        self.header.free_bytes = self
            .header
            .free_bytes
            .checked_add(span.len)
            .ok_or(FreeSpanError::BinInvariant)?;
        self.index_edits
            .push(ic_stable_paged_ordered_map::Edit::Insert {
                key: span.start_slot,
                value: id,
            });
        self.previous.push(None);
        let candidate = MaxSpanCandidate { id, len: span.len };
        if self.first_candidate.is_none() {
            self.first_candidate = Some(candidate);
        } else {
            self.candidates.push(candidate);
        }
        Ok(())
    }

    // Outer None means unchanged; inner None means a prospective removal.
    fn edited_id(&self, key: u64) -> Option<Option<SpanId>> {
        self.index_edits.iter().rev().find_map(|edit| match *edit {
            ic_stable_paged_ordered_map::Edit::Insert {
                key: changed,
                value,
            } if key == changed => Some(Some(value)),
            ic_stable_paged_ordered_map::Edit::Remove { key: changed } if key == changed => {
                Some(None)
            }
            _ => None,
        })
    }

    fn indexed_id(&self, key: u64) -> Option<SpanId> {
        self.edited_id(key).unwrap_or_else(|| self.index.get(key))
    }

    fn indexed_span(&self, key: u64, source_id: SpanId) -> Result<Option<FreeSpan>, FreeSpanError> {
        let Some(id) = self.edited_id(key).unwrap_or(Some(source_id)) else {
            return Ok(None);
        };
        let record = self.record(id)?;
        if record.flags != FLAG_ACTIVE || record.start_slot != key {
            return Err(FreeSpanError::BinInvariant);
        }
        Ok(Some(FreeSpan {
            start_slot: key,
            len: record.len,
        }))
    }

    fn previous_or_equal_span(&self, key: u64) -> Result<Option<FreeSpan>, FreeSpanError> {
        let mut bound = Some(key);
        let mut found = None;
        while let Some((start, id)) = bound.and_then(|bound| self.index.predecessor_or_equal(bound))
        {
            if let Some(span) = self.indexed_span(start, id)? {
                found = Some(span);
                break;
            }
            bound = start.checked_sub(1);
        }
        for edit in &self.index_edits {
            if let ic_stable_paged_ordered_map::Edit::Insert { key: start, value } = *edit
                && start <= key
                && found.is_none_or(|span| span.start_slot < start)
                && let Some(span) = self.indexed_span(start, value)?
            {
                found = Some(span);
            }
        }
        Ok(found)
    }

    fn next_span(&self, key: u64) -> Result<Option<FreeSpan>, FreeSpanError> {
        let mut bound = key;
        let mut found = None;
        while let Some((start, id)) = self.index.successor(bound) {
            if let Some(span) = self.indexed_span(start, id)? {
                found = Some(span);
                break;
            }
            bound = start;
        }
        for edit in &self.index_edits {
            if let ic_stable_paged_ordered_map::Edit::Insert { key: start, value } = *edit
                && start > key
                && found.is_none_or(|span| start < span.start_slot)
                && let Some(span) = self.indexed_span(start, value)?
            {
                found = Some(span);
            }
        }
        Ok(found)
    }

    fn coalesced_release(
        &self,
        span: FreeSpan,
    ) -> Result<(FreeSpan, [Option<FreeSpan>; 2]), FreeSpanError> {
        checked_span_end(span)?;
        let previous = self.previous_or_equal_span(span.start_slot)?;
        if previous.is_some_and(|previous| previous.start_slot == span.start_slot) {
            return Err(FreeSpanError::DuplicateStart {
                start_slot: span.start_slot,
            });
        }
        coalesced_span(span, previous, self.next_span(span.start_slot)?)
    }

    fn release(&mut self, span: FreeSpan) -> Result<(), FreeSpanError> {
        let (merged, neighbors) = self.coalesced_release(span)?;
        for neighbor in neighbors.into_iter().flatten() {
            self.remove(neighbor)?;
        }
        self.insert(merged)
    }

    /// The single prefix split used by immediate and prepared allocation.
    fn take_prefix(&mut self, whole: FreeSpan, len: u64) -> Result<FreeSpan, FreeSpanError> {
        if len == 0 {
            return Err(FreeSpanError::EmptySpan);
        }
        if len > whole.len {
            return Err(FreeSpanError::MissingSpan { span: whole });
        }
        let taken = FreeSpan {
            start_slot: whole.start_slot,
            len,
        };
        let end = checked_span_end(taken)?;
        self.remove(whole)?;
        if whole.len > len {
            self.insert(FreeSpan {
                start_slot: end,
                len: whole.len - len,
            })?;
        }
        self.taken_len = len;
        Ok(taken)
    }

    fn observe_source_span(
        &mut self,
        pair: Option<(u64, SpanId)>,
    ) -> Result<Option<FreeSpan>, FreeSpanError> {
        let Some((start, id)) = pair else {
            return Ok(None);
        };
        let record = self.source.checked_record(id)?;
        if record.flags != FLAG_ACTIVE
            || record.start_slot != start
            || record.len == 0
            || u32::from(record.bin_idx) != size_class(record.len)
        {
            return Err(FreeSpanError::BinInvariant);
        }
        let span = FreeSpan {
            start_slot: start,
            len: record.len,
        };
        checked_span_end(span)?;
        if !self
            .physical_source
            .iter()
            .any(|fact| matches!(fact, PhysicalSpanSource::Record(observed, _) if *observed == id))
        {
            self.retain_source(PhysicalSpanSource::Record(id, record.encode()))?;
        }
        Ok(Some(span))
    }

    fn retain_source(&mut self, fact: PhysicalSpanSource) -> Result<(), FreeSpanError> {
        self.physical_source
            .try_reserve(1)
            .map_err(|_| FreeSpanError::OutOfMemory)?;
        self.physical_source.push(fact);
        Ok(())
    }

    /// Union of checked covers minus the guarded ORIGINAL free coverage. Called
    /// before a prefix is removed, so newly acquired destination bytes stay live.
    fn owned_cover_parts(&mut self, covers: &[FreeSpan]) -> Result<Vec<FreeSpan>, FreeSpanError> {
        let mut ordered = Vec::new();
        ordered
            .try_reserve(covers.len())
            .map_err(|_| FreeSpanError::OutOfMemory)?;
        for cover in covers {
            checked_span_end(*cover)?;
            ordered.push(*cover);
        }
        ordered.sort_unstable_by_key(|span| span.start_slot);
        let mut union: Vec<FreeSpan> = Vec::new();
        union
            .try_reserve(ordered.len())
            .map_err(|_| FreeSpanError::OutOfMemory)?;
        for span in ordered {
            if let Some(last) = union.last_mut()
                && span.start_slot <= checked_span_end(*last)?
            {
                last.len = checked_span_end(*last)?.max(checked_span_end(span)?) - last.start_slot;
            } else {
                union.push(span);
            }
        }
        let mut owned = Vec::new();
        for cover in union {
            let end = checked_span_end(cover)?;
            let mut cursor = cover.start_slot;
            let pair = self.index.predecessor_or_equal(cursor);
            self.retain_source(PhysicalSpanSource::BeforeOrAt(cursor, pair))?;
            let mut free = self.observe_source_span(pair)?;
            let mut bound = free.map_or(cursor, |span| span.start_slot);
            loop {
                if let Some(span) = free {
                    if span.start_slot >= end {
                        break;
                    }
                    if span.start_slot > cursor {
                        owned
                            .try_reserve(1)
                            .map_err(|_| FreeSpanError::OutOfMemory)?;
                        owned.push(FreeSpan {
                            start_slot: cursor,
                            len: span.start_slot.min(end) - cursor,
                        });
                    }
                    cursor = cursor.max(checked_span_end(span)?).min(end);
                }
                let pair = self.index.successor(bound);
                self.retain_source(PhysicalSpanSource::After(bound, pair))?;
                free = self.observe_source_span(pair)?;
                if cursor == end {
                    break;
                }
                if let Some(span) = free {
                    bound = span.start_slot;
                } else {
                    break;
                }
            }
            if cursor < end {
                owned
                    .try_reserve(1)
                    .map_err(|_| FreeSpanError::OutOfMemory)?;
                owned.push(FreeSpan {
                    start_slot: cursor,
                    len: end - cursor,
                });
            }
        }
        Ok(owned)
    }

    // A live heap node bounds its entire subtree. Only stale branches need descent;
    // querying the prospective maximum neither pops nor warms the source cache.
    fn largest_in_heap(&self, heap: &[MaxSpanCandidate], at: usize) -> Result<u64, FreeSpanError> {
        let Some(candidate) = heap.get(at) else {
            return Ok(0);
        };
        let record = self.record(candidate.id)?;
        if record.flags == FLAG_ACTIVE && record.len == candidate.len {
            return Ok(candidate.len);
        }
        let left = at
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or(FreeSpanError::BinInvariant)?;
        let mut largest = self.largest_in_heap(heap, left)?;
        let right = left.checked_add(1).ok_or(FreeSpanError::BinInvariant)?;
        if heap
            .get(right)
            .is_some_and(|candidate| candidate.len > largest)
        {
            largest = self.largest_in_heap(heap, right)?;
        }
        Ok(largest)
    }

    fn rebuild_heap(&self) -> Result<BinaryHeap<MaxSpanCandidate>, FreeSpanError> {
        let count =
            usize::try_from(self.header.active_count).map_err(|_| FreeSpanError::OutOfMemory)?;
        let mut heap = BinaryHeap::new();
        heap.try_reserve(count)
            .map_err(|_| FreeSpanError::OutOfMemory)?;
        for (_, id) in self.index.iter() {
            let record = self.record(id)?;
            if record.flags == FLAG_ACTIVE {
                if heap.len() == count {
                    return Err(FreeSpanError::BinInvariant);
                }
                heap.push(MaxSpanCandidate {
                    id,
                    len: record.len,
                });
            }
        }
        for candidate in self.candidates() {
            // An originally active ID was already visited through the source index,
            // even if this edit recycled it at another key.
            if candidate.id <= self.source_slots
                && self.source.read_record(candidate.id).flags == FLAG_ACTIVE
            {
                continue;
            }
            if heap.len() == count {
                return Err(FreeSpanError::BinInvariant);
            }
            heap.push(*candidate);
        }
        if heap.len() != count {
            return Err(FreeSpanError::BinInvariant);
        }
        Ok(heap)
    }
}

/// Local map guard and encoded allocator image. No logical span change is published.
/// The guard lives separately from the ready capability that borrows it.
pub(crate) struct FreeSpanPreparation<'a, M: Memory> {
    owner: &'a FreeSpanStore<M>,
    index: RefMut<'a, StablePagedOrderedMap<M>>,
    image: Option<FreeSpanImage>,
    selected: Option<FreeSpan>,
    taken_len: u64,
    physical_source: Vec<PhysicalSpanSource>,
    #[cfg(test)]
    retired: Vec<FreeSpan>,
}

struct FreeSpanImage {
    records: Vec<(u64, [u8; RECORD_STRIDE as usize])>,
    bins: Vec<(u64, [u8; 8])>,
    allocation: [u8; 24],
    summary: [u8; 16],
    index_edits: Vec<ic_stable_paged_ordered_map::Edit>,
    previous: Vec<Option<SpanId>>,
    rebuilt_heap: Option<BinaryHeap<MaxSpanCandidate>>,
    first_candidate: Option<MaxSpanCandidate>,
    candidates: Vec<MaxSpanCandidate>,
}

pub(crate) struct PreparedFreeSpans<'a, M: Memory> {
    owner: &'a FreeSpanStore<M>,
    index: ic_stable_paged_ordered_map::PreparedEdits<'a, M>,
    image: FreeSpanImage,
}

impl<M: Memory> FreeSpanPreparation<'_, M> {
    /// Actual whole reusable span selected by the allocator, not a guessed origin.
    pub(crate) fn selected_span(&self) -> Option<FreeSpan> {
        self.selected
    }

    pub(crate) fn taken_span(&self) -> Option<FreeSpan> {
        self.selected.map(|whole| FreeSpan {
            start_slot: whole.start_slot,
            len: self.taken_len,
        })
    }

    #[cfg(test)]
    pub(crate) fn retired_intervals(&self) -> &[FreeSpan] {
        &self.retired
    }

    /// Corrupt a real canonical record through the held source view for native
    /// source-revalidation tests; stable row addressing stays at this owner.
    #[cfg(test)]
    pub(crate) fn change_source_span_len_for_test(&self, start: u64, len: u64) {
        let id = self.index.get(start).expect("existing source record");
        let mut record = self.owner.read_record(id);
        record.len = len;
        self.owner.store.write(
            record_offset(id).expect("source record offset"),
            &record.encode(),
        );
    }

    // Physical source walks must not inflate the inlined immediate-edit path.
    #[inline(never)]
    fn revalidate_physical_source(&self) -> Result<(), FreeSpanError> {
        if self.physical_source.iter().any(|fact| match fact {
            PhysicalSpanSource::Header(header) => self.owner.header() != *header,
            PhysicalSpanSource::Record(id, bytes) => self.owner.read_record(*id).encode() != *bytes,
            PhysicalSpanSource::BeforeOrAt(bound, pair) => {
                self.index.predecessor_or_equal(*bound) != *pair
            }
            PhysicalSpanSource::After(bound, pair) => self.index.successor(*bound) != *pair,
            PhysicalSpanSource::Exact(start, id) => self.index.get(*start) != *id,
            PhysicalSpanSource::Bin(bin, id) => self.owner.read_bin_head(*bin) != *id,
        }) {
            return Err(FreeSpanError::BinInvariant);
        }
        Ok(())
    }

    #[inline(always)]
    pub(crate) fn ready(&mut self) -> Result<PreparedFreeSpans<'_, M>, FreeSpanError> {
        if !self.physical_source.is_empty() {
            self.revalidate_physical_source()?;
        }
        let image = self.image.take().ok_or(FreeSpanError::BinInvariant)?;
        let index = self
            .index
            .prepare_edits(&image.index_edits)
            .map_err(FreeSpanStore::<M>::map_index_prepare_error)?;
        for ((edit, expected), actual) in image
            .index_edits
            .iter()
            .zip(&image.previous)
            .zip(index.previous_values())
        {
            if actual != expected {
                return Err(match *edit {
                    ic_stable_paged_ordered_map::Edit::Insert { key, .. } => {
                        FreeSpanError::DuplicateStart { start_slot: key }
                    }
                    ic_stable_paged_ordered_map::Edit::Remove { .. } => FreeSpanError::BinInvariant,
                });
            }
        }
        Ok(PreparedFreeSpans {
            owner: self.owner,
            index,
            image,
        })
    }
}

impl<M: Memory> PreparedFreeSpans<'_, M> {
    #[inline(always)]
    pub(crate) fn publish(self) {
        let Self {
            owner,
            index,
            image,
        } = self;
        #[cfg(test)]
        owner
            .publication_count
            .set(owner.publication_count.get() + 1);
        for (address, record) in image.records {
            owner.store.write(address, &record);
        }
        for (address, head) in image.bins {
            owner.store.write(address, &head);
        }
        owner.store.write(OFFSET_RECORD_SLOTS, &image.allocation);
        owner.store.write(OFFSET_FREE_BYTES, &image.summary);
        index.publish();
        // Cache installation does not read the guarded index or allocate scratch.
        if let Some(heap) = image.rebuilt_heap {
            *owner.max_spans.borrow_mut() = heap;
            owner.max_heap_ready.set(true);
        } else {
            let mut heap = owner.max_spans.borrow_mut();
            for candidate in image.first_candidate.into_iter().chain(image.candidates) {
                heap.push(candidate);
            }
        }
    }
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
    #[cfg(test)]
    publication_count: Cell<u64>,
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
            #[cfg(test)]
            publication_count: Cell::new(0),
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
            #[cfg(test)]
            publication_count: Cell::new(0),
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

    #[cfg(test)]
    pub(crate) fn publication_count(&self) -> u64 {
        self.publication_count.get()
    }

    /// Snapshot actual lookup-cache contents without warming or refilling them.
    #[cfg(test)]
    pub(crate) fn physical_cache_snapshot(&self) -> (bool, Vec<(SpanId, u64)>) {
        (
            self.max_heap_ready.get(),
            self.max_spans
                .borrow()
                .iter()
                .map(|node| (node.id, node.len))
                .collect(),
        )
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

    /// Reserves record/index backing for `additional_active` more free spans,
    /// without changing logical span ownership.
    ///
    /// This reserves physical backing only, not index-directory admission, edit
    /// scratch or release-range validation. Each mutation separately prepares its
    /// complete record/bin/index publication; this is not a ready commit token.
    pub(crate) fn reserve_for_releases(&self, additional_active: u64) -> Result<(), GrowFailed> {
        let target_slots = self
            .read_record_slots()
            .checked_add(additional_active)
            .ok_or(GrowFailed {
                current_size: self.store.size(),
                delta: u64::MAX,
            })?;
        self.reserve_record_slots(target_slots)?;
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
        let active = self.len();
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
        #[cfg(test)]
        record_free_span_release_call();
        self.release_coalesced(span)
    }

    fn release_coalesced(&self, span: FreeSpan) -> Result<(), FreeSpanError> {
        let mut edit = FreeSpanEdit::new(self, 2, 1)?;
        edit.release(span)?;
        let mut preparation = self.prepare_edit(edit, None)?;
        preparation.ready()?.publish();
        Ok(())
    }

    /// Prepare one tentative best-fit prefix and all old-span retirements together.
    /// Discard leaves original ownership intact; only backing/scratch may have grown.
    pub(crate) fn prepare_replacement(
        &self,
        len: u64,
        retired: &[FreeSpan],
    ) -> Result<FreeSpanPreparation<'_, M>, FreeSpanError> {
        let selected = self.find_best_fit(len)?;
        let removals = retired
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or(FreeSpanError::OutOfMemory)?;
        let insertions = retired
            .len()
            .checked_add(1)
            .ok_or(FreeSpanError::OutOfMemory)?;
        let mut edit = FreeSpanEdit::new(self, removals, insertions)?;
        // Old spans must be live in the source, even if allocation would hide an
        // overlap by removing the free span first. The same coalescer validates both stages.
        for span in retired {
            edit.coalesced_release(*span)?;
        }
        if let Some(whole) = selected {
            edit.take_prefix(whole, len)?;
        }
        for span in retired {
            edit.release(*span)?;
        }
        self.prepare_edit(edit, selected)
    }

    /// One source-aware physical edit; exact-prefix no-fit is distinct from an
    /// admission error. The same guard owns source subtraction and publication.
    pub(crate) fn prepare_physical_replacement(
        &self,
        choice: PhysicalSpanSelection,
        covers: &[FreeSpan],
    ) -> Result<Option<FreeSpanPreparation<'_, M>>, FreeSpanError> {
        let mut edit = FreeSpanEdit::new(self, 0, 0)?;
        edit.retain_source(PhysicalSpanSource::Header(edit.header))?;
        let owned = edit.owned_cover_parts(covers)?;
        let removals = owned
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or(FreeSpanError::OutOfMemory)?;
        let insertions = owned
            .len()
            .checked_add(1)
            .ok_or(FreeSpanError::OutOfMemory)?;
        edit.reserve_changes(removals, insertions)?;
        let (selected, len) = match choice {
            PhysicalSpanSelection::NoAllocation => (None, 0),
            PhysicalSpanSelection::BestFit(len) => (self.find_best_fit(len)?, len),
            PhysicalSpanSelection::ExactPrefixAt(start, len) => {
                if len == 0 {
                    return Ok(None);
                }
                let id = edit.index.get(start);
                edit.retain_source(PhysicalSpanSource::Exact(start, id))?;
                let whole = edit.observe_source_span(id.map(|id| (start, id)))?;
                let Some(whole) = whole.filter(|span| span.len >= len) else {
                    return Ok(None);
                };
                (Some(whole), len)
            }
        };
        if let Some(whole) = selected {
            let id = edit
                .index
                .get(whole.start_slot)
                .ok_or(FreeSpanError::MissingSpan { span: whole })?;
            edit.observe_source_span(Some((whole.start_slot, id)))?;
            edit.take_prefix(whole, len)?;
        }
        for span in &owned {
            edit.release(*span)?;
        }
        #[cfg(test)]
        {
            edit.retired = owned;
        }
        self.prepare_edit(edit, selected).map(Some)
    }

    fn find_best_fit(&self, min_len: u64) -> Result<Option<FreeSpan>, FreeSpanError> {
        if min_len == 0 {
            return Ok(None);
        }
        for bin in size_class(min_len) as usize..BIN_COUNT {
            if let Some(span) = self.pick_span_in_bin(bin as u32, min_len)? {
                return Ok(Some(span));
            }
        }
        Ok(None)
    }

    /// Releases a span described by start slot and length.
    pub fn release_span(&self, start_slot: u64, len: u64) -> Result<(), FreeSpanError> {
        self.release(FreeSpan { start_slot, len })
    }

    /// Restores a prefix that was just allocated by [`Self::take_best_fit`].
    ///
    /// Shares release validation and publication, including intervening-overlap
    /// checks; an adjacent remainder alone does not prove prefix ownership.
    pub fn restore_allocated_prefix(&self, span: FreeSpan) -> Result<(), FreeSpanError> {
        self.release_coalesced(span)
    }

    /// Takes an entire free span of at least `min_len`, without splitting it.
    pub fn take_best_fit_whole(&self, min_len: u64) -> Result<Option<FreeSpan>, FreeSpanError> {
        let Some(span) = self.find_best_fit(min_len)? else {
            return Ok(None);
        };
        self.remove_span_exact(span)?;
        Ok(Some(span))
    }

    /// Returns the current best-fit span without removing it.
    pub fn peek_best_fit(&self, min_len: u64) -> Option<FreeSpan> {
        self.find_best_fit(min_len).ok().flatten()
    }

    /// Takes exactly `min_len` slots from a best-fit span, splitting the remainder if needed.
    pub fn take_best_fit(&self, min_len: u64) -> Result<Option<FreeSpan>, FreeSpanError> {
        self.find_best_fit(min_len)?
            .map(|whole| self.take_span_prefix(whole, min_len))
            .transpose()
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

        self.take_span_prefix(whole, len).map(Some)
    }

    fn take_span_prefix(&self, whole: FreeSpan, len: u64) -> Result<FreeSpan, FreeSpanError> {
        let mut edit = FreeSpanEdit::new(self, 1, usize::from(whole.len > len))?;
        let taken = edit.take_prefix(whole, len)?;
        let mut preparation = self.prepare_edit(edit, Some(whole))?;
        preparation.ready()?.publish();
        Ok(taken)
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
        let left_id = self.exact_span_record(left)?.0;
        let right_id = self.exact_span_record(right)?.0;
        checked_span_end(replacement)?;
        let (previous, next) =
            self.neighbors_after_removing(left, right, replacement.start_slot)?;
        let (merged, neighbors) = coalesced_span(replacement, previous, next)?;
        if left_id == right_id {
            return Err(FreeSpanError::MissingSpan { span: right });
        }
        if replacement.start_slot != left.start_slot
            && replacement.start_slot != right.start_slot
            && self.by_start.borrow().get(replacement.start_slot).is_some()
        {
            return Err(FreeSpanError::DuplicateStart {
                start_slot: replacement.start_slot,
            });
        }
        let mut removed = [left, right, FreeSpan::default(), FreeSpan::default()];
        let mut count = 2;
        for neighbor in neighbors.into_iter().flatten() {
            removed[count] = neighbor;
            count += 1;
        }
        self.replace_spans(&removed[..count], Some(merged))
    }

    fn exact_span_record(&self, span: FreeSpan) -> Result<(SpanId, SpanRecord), FreeSpanError> {
        let id = self
            .by_start
            .borrow()
            .get(span.start_slot)
            .ok_or(FreeSpanError::MissingSpan { span })?;
        let rec = self.checked_record(id)?;
        if rec.flags != FLAG_ACTIVE || rec.len != span.len || rec.start_slot != span.start_slot {
            return Err(FreeSpanError::MissingSpan { span });
        }
        Ok((id, rec))
    }

    fn neighbors_after_removing(
        &self,
        left: FreeSpan,
        right: FreeSpan,
        start_slot: u64,
    ) -> Result<(Option<FreeSpan>, Option<FreeSpan>), FreeSpanError> {
        let ignored = [left.start_slot, right.start_slot];
        let prev = {
            let by_start = self.by_start.borrow();
            let mut cursor = start_slot;
            loop {
                let Some((start, id)) = by_start.predecessor(cursor) else {
                    break None;
                };
                if ignored.contains(&start) {
                    cursor = start;
                } else {
                    let record = self.checked_record(id)?;
                    break Some(FreeSpan {
                        start_slot: record.start_slot,
                        len: record.len,
                    });
                }
            }
        };
        let next = {
            let by_start = self.by_start.borrow();
            let mut cursor = start_slot;
            loop {
                let Some((start, id)) = by_start.successor(cursor) else {
                    break None;
                };
                if ignored.contains(&start) {
                    cursor = start;
                } else {
                    let record = self.checked_record(id)?;
                    break Some(FreeSpan {
                        start_slot: record.start_slot,
                        len: record.len,
                    });
                }
            }
        };
        Ok((prev, next))
    }

    /// Resolves a `by_start` value to its active free span, `None` when the
    /// record is a stale (already-unlinked) entry.
    fn active_span(&self, id: SpanId) -> Option<FreeSpan> {
        let rec = self.read_record(id);
        if rec.flags != FLAG_ACTIVE {
            return None;
        }
        Some(FreeSpan {
            start_slot: rec.start_slot,
            len: rec.len,
        })
    }

    fn next_span(&self, start_slot: u64) -> Option<FreeSpan> {
        self.by_start
            .borrow()
            .successor(start_slot)
            .and_then(|(_, id)| self.active_span(id))
    }

    fn pick_span_in_bin(
        &self,
        bin_idx: u32,
        min_len: u64,
    ) -> Result<Option<FreeSpan>, FreeSpanError> {
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
        Ok(Some(FreeSpan {
            start_slot: rec.start_slot,
            len: rec.len,
        }))
    }

    fn remove_span_exact(&self, span: FreeSpan) -> Result<(), FreeSpanError> {
        self.replace_spans(&[span], None)
    }

    /// Prepares one complete free-span replacement, including all paired index edits.
    /// The first record write begins publication: below it no grow, validation,
    /// serialization, scratch reservation or recoverable error remains.
    fn replace_spans(
        &self,
        removed: &[FreeSpan],
        replacement: Option<FreeSpan>,
    ) -> Result<(), FreeSpanError> {
        let mut edit = FreeSpanEdit::new(self, removed.len(), usize::from(replacement.is_some()))?;
        for span in removed {
            edit.remove(*span)?;
        }
        if let Some(span) = replacement {
            edit.insert(span)?;
        }
        let mut preparation = self.prepare_edit(edit, None)?;
        preparation.ready()?.publish();
        Ok(())
    }

    #[inline(always)]
    fn prepare_edit<'a>(
        &'a self,
        mut edit: FreeSpanEdit<'a, M>,
        selected: Option<FreeSpan>,
    ) -> Result<FreeSpanPreparation<'a, M>, FreeSpanError> {
        let heap_bound = edit
            .header
            .active_count
            .checked_mul(MAX_HEAP_STALE_FACTOR)
            .and_then(|n| n.checked_add(MAX_HEAP_STALE_ALLOWANCE))
            .ok_or(FreeSpanError::BinInvariant)?;
        let candidate_count = edit.candidates().count();
        let heap_len = self
            .max_spans
            .borrow()
            .len()
            .checked_add(candidate_count)
            .ok_or(FreeSpanError::OutOfMemory)?;
        let rebuilt_heap = if !self.max_heap_ready.get() || heap_len as u64 > heap_bound {
            Some(edit.rebuild_heap()?)
        } else {
            None
        };
        let new_largest = edit
            .candidates()
            .map(|candidate| candidate.len)
            .max()
            .unwrap_or(0);
        edit.header.largest_free_span = if let Some(heap) = &rebuilt_heap {
            heap.peek().map_or(0, |candidate| candidate.len)
        } else if new_largest >= edit.header.largest_free_span {
            new_largest
        } else if edit.maximum_removed {
            edit.largest_in_heap(self.max_spans.borrow().as_slice(), 0)?
                .max(new_largest)
        } else {
            edit.header.largest_free_span
        };
        if rebuilt_heap.is_none() && candidate_count != 0 {
            // Capacity is scratch, not cache contents: a later rejection keeps
            // the same candidates and ready flag, even if capacity increased.
            self.max_spans
                .borrow_mut()
                .try_reserve(candidate_count)
                .map_err(|_| FreeSpanError::OutOfMemory)?;
        }
        self.reserve_record_slots(edit.header.record_slots)
            .map_err(|_| FreeSpanError::OutOfMemory)?;
        let backed = self
            .store
            .size()
            .checked_mul(crate::WASM_PAGE_SIZE)
            .ok_or(FreeSpanError::CorruptedFreeList)?;
        // Also retain original records/bin heads changed by the physical image,
        // not just kept holes. Immediate/strict edits retain their existing cost.
        if !edit.physical_source.is_empty() {
            let additional = edit
                .records
                .len()
                .checked_add(edit.bin_heads.len())
                .ok_or(FreeSpanError::OutOfMemory)?;
            edit.physical_source
                .try_reserve(additional)
                .map_err(|_| FreeSpanError::OutOfMemory)?;
            for (id, _) in &edit.records {
                if *id <= edit.source_slots
                    && !edit.physical_source.iter().any(
                        |fact| matches!(fact, PhysicalSpanSource::Record(seen, _) if seen == id),
                    )
                {
                    edit.physical_source.push(PhysicalSpanSource::Record(
                        *id,
                        self.read_record(*id).encode(),
                    ));
                }
            }
            for (bin, _) in &edit.bin_heads {
                edit.physical_source.push(PhysicalSpanSource::Bin(
                    *bin as usize,
                    self.read_bin_head(*bin as usize),
                ));
            }
        }
        // Prospective lookups are complete. Convert keys to backed addresses and
        // move the existing encoded buffers into the publication image.
        for (id, _) in &mut edit.records {
            let address = record_offset(*id).ok_or(FreeSpanError::CorruptedFreeList)?;
            if address
                .checked_add(RECORD_STRIDE)
                .is_none_or(|end| end > backed)
            {
                return Err(FreeSpanError::CorruptedFreeList);
            }
            *id = address;
        }
        for (bin, _) in &mut edit.bin_heads {
            *bin = OFFSET_BIN_HEADS + *bin * 8;
        }
        let mut allocation = [0; 24];
        allocation[0..8].copy_from_slice(&edit.header.record_slots.to_le_bytes());
        allocation[8..16].copy_from_slice(&edit.header.active_count.to_le_bytes());
        allocation[16..24].copy_from_slice(&edit.header.free_head.to_le_bytes());
        let mut summary = [0; 16];
        summary[0..8].copy_from_slice(&edit.header.free_bytes.to_le_bytes());
        summary[8..16].copy_from_slice(&edit.header.largest_free_span.to_le_bytes());
        Ok(FreeSpanPreparation {
            owner: self,
            index: edit.index,
            selected,
            taken_len: edit.taken_len,
            physical_source: edit.physical_source,
            #[cfg(test)]
            retired: edit.retired,
            image: Some(FreeSpanImage {
                records: edit.records,
                bins: edit.bin_heads,
                allocation,
                summary,
                index_edits: edit.index_edits,
                previous: edit.previous,
                rebuilt_heap,
                first_candidate: edit.first_candidate,
                candidates: edit.candidates,
            }),
        })
    }

    fn map_index_prepare_error(error: ic_stable_paged_ordered_map::PrepareError) -> FreeSpanError {
        use ic_stable_paged_ordered_map::{Error, PrepareError};
        match error {
            PrepareError::Grow { .. }
            | PrepareError::Scratch { .. }
            | PrepareError::Edit {
                source: Error::OutOfMemory,
                ..
            } => FreeSpanError::OutOfMemory,
            PrepareError::Edit {
                source: Error::DirectoryFull,
                ..
            } => FreeSpanError::IndexFull,
            PrepareError::Edit {
                source: Error::Corrupt,
                ..
            } => FreeSpanError::BinInvariant,
            PrepareError::AddressOverflow { .. } => FreeSpanError::CorruptedFreeList,
        }
    }

    fn reserve_record_slots(&self, slots: u64) -> Result<(), GrowFailed> {
        let need = record_extent(slots).ok_or(GrowFailed {
            current_size: self.store.size(),
            delta: u64::MAX,
        })?;
        if need.div_ceil(crate::WASM_PAGE_SIZE) > self.store.size() {
            safe_write(&self.store, need - 1, &[0])?;
        }
        Ok(())
    }

    fn checked_record(&self, id: SpanId) -> Result<SpanRecord, FreeSpanError> {
        let backed = self
            .store
            .size()
            .checked_mul(crate::WASM_PAGE_SIZE)
            .ok_or(FreeSpanError::CorruptedFreeList)?;
        if id == 0
            || id > self.read_record_slots()
            || record_extent(id).is_none_or(|end| end > backed)
        {
            return Err(FreeSpanError::CorruptedFreeList);
        }
        Ok(self.read_record(id))
    }

    #[inline]
    fn read_record(&self, id: SpanId) -> SpanRecord {
        let off = record_offset(id).expect("backed span record index");
        let mut bytes = [0; RECORD_STRIDE as usize];
        self.store.read(off, &mut bytes);
        SpanRecord::decode(&bytes)
    }

    #[inline]
    fn read_bin_head(&self, bin: usize) -> SpanId {
        crate::read_u64(
            &self.store,
            Address::from(OFFSET_BIN_HEADS + bin as u64 * 8),
        )
    }

    #[inline]
    fn read_record_slots(&self) -> u64 {
        crate::read_u64(&self.store, Address::from(OFFSET_RECORD_SLOTS))
    }
}

fn checked_span_end(span: FreeSpan) -> Result<u64, FreeSpanError> {
    if span.len == 0 {
        return Err(FreeSpanError::EmptySpan);
    }
    span.start_slot
        .checked_add(span.len)
        .ok_or(FreeSpanError::SpanOverflow { span })
}

fn coalesced_span(
    span: FreeSpan,
    previous: Option<FreeSpan>,
    next: Option<FreeSpan>,
) -> Result<(FreeSpan, [Option<FreeSpan>; 2]), FreeSpanError> {
    let end = checked_span_end(span)?;
    let mut merged = span;
    let mut neighbors = [None, None];
    if let Some(previous) = previous {
        let previous_end = checked_span_end(previous)?;
        if previous_end > span.start_slot {
            return Err(FreeSpanError::OverlapPrevious {
                previous,
                inserted: span,
            });
        }
        if previous_end == span.start_slot {
            merged.start_slot = previous.start_slot;
            merged.len = merged
                .len
                .checked_add(previous.len)
                .ok_or(FreeSpanError::SpanOverflow { span })?;
            neighbors[0] = Some(previous);
        }
    }
    if let Some(next) = next {
        checked_span_end(next)?;
        if end > next.start_slot {
            return Err(FreeSpanError::OverlapNext {
                inserted: span,
                next,
            });
        }
        if end == next.start_slot {
            merged.len = merged
                .len
                .checked_add(next.len)
                .ok_or(FreeSpanError::SpanOverflow { span })?;
            neighbors[1] = Some(next);
        }
    }
    Ok((merged, neighbors))
}

fn record_extent(slots: u64) -> Option<u64> {
    slots
        .checked_mul(RECORD_STRIDE)
        .and_then(|bytes| RECORDS_OFFSET.checked_add(bytes))
}

fn record_offset(id: SpanId) -> Option<u64> {
    id.checked_sub(1).and_then(record_extent)
}

fn write_header<M: Memory>(memory: &M, h: &HeaderV1) -> Result<(), GrowFailed> {
    crate::safe_write(memory, OFFSET_MAGIC, &h.magic)?;
    crate::safe_write(memory, OFFSET_VERSION, &[h.version])?;
    write_zeroes(memory, OFFSET_RESERVED, RESERVED_SIZE)?;
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
    if !region_is_zero(memory, OFFSET_RESERVED, RESERVED_SIZE) {
        return Err(InitError::InvalidLayout);
    }
    if h.active_count > h.record_slots || h.free_head > h.record_slots {
        return Err(InitError::InvalidLayout);
    }
    let bytes = memory
        .size()
        .checked_mul(crate::WASM_PAGE_SIZE)
        .ok_or(InitError::InvalidLayout)?;
    let need = record_extent(h.record_slots).ok_or(InitError::InvalidLayout)?;
    if bytes < need {
        return Err(InitError::InvalidLayout);
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    /// Test-only count of public free-span releases (`release`/`release_span`).
    /// Lets tests pin "one store insert per merged run" contracts: a drain that
    /// batches its emptied ranges must not pay one insert per emptied bucket
    /// (GAP-2026-09-20-002). Bytes or span counts cannot express this — the store
    /// merges adjacent spans on insert either way.
    static FREE_SPAN_RELEASE_CALLS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_free_span_release_calls() {
    FREE_SPAN_RELEASE_CALLS.with(|c| c.set(0));
}

#[cfg(test)]
pub(crate) fn free_span_release_calls() -> u64 {
    FREE_SPAN_RELEASE_CALLS.with(|c| c.get())
}

#[cfg(test)]
fn record_free_span_release_call() {
    FREE_SPAN_RELEASE_CALLS.with(|c| c.set(c.get().saturating_add(1)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::FailpointMemory;
    use ic_stable_structures::DefaultMemoryImpl;
    use ic_stable_structures::memory_manager::{MemoryId, MemoryManager, VirtualMemory};

    /// A release that is exactly adjacent to a free span *and* overlaps another
    /// free span strictly inside its range must report the overlap, not merge
    /// over the inner span. The earlier lookup shape preferred the span starting
    /// exactly at the range end, so it merged over the inner span and left the
    /// store with two overlapping free spans (caught by `validate`).
    #[test]
    fn release_prefers_inner_overlap_over_adjacent_merge() {
        let store = test_store();
        store
            .release(FreeSpan {
                start_slot: 10,
                len: 5,
            })
            .expect("adjacent free span");
        store
            .release(FreeSpan {
                start_slot: 5,
                len: 2,
            })
            .expect("inner free span");
        let err = store
            .release(FreeSpan {
                start_slot: 0,
                len: 10,
            })
            .unwrap_err();
        assert!(
            matches!(err, FreeSpanError::OverlapNext { .. }),
            "inner overlap must win over the adjacent merge, got {err:?}"
        );
        store.validate().unwrap();
        assert_eq!(store.len(), 2, "both pre-existing spans survive");
    }

    fn test_store() -> FreeSpanStore<VirtualMemory<DefaultMemoryImpl>> {
        let m = MemoryManager::init(DefaultMemoryImpl::default());
        FreeSpanStore::init(m.get(MemoryId::new(10)), m.get(MemoryId::new(11))).unwrap()
    }

    fn test_paged_store() -> FreeSpanStore<VirtualMemory<DefaultMemoryImpl>> {
        let m = MemoryManager::init(DefaultMemoryImpl::default());
        FreeSpanStore::init(m.get(MemoryId::new(40)), m.get(MemoryId::new(41))).unwrap()
    }

    fn admission_snapshot<M: Memory>(
        store: &FreeSpanStore<M>,
        memories: &[M; 2],
    ) -> ([Vec<u8>; 2], bool, Vec<MaxSpanCandidate>) {
        let backing = memories.each_ref().map(|memory| {
            let mut bytes = vec![0; (memory.size() * crate::WASM_PAGE_SIZE) as usize];
            memory.read(0, &mut bytes);
            bytes
        });
        // Observe the cache as-is, without rebuilding or querying its maximum.
        let heap = store.max_spans.borrow().as_slice().to_vec();
        (backing, store.max_heap_ready.get(), heap)
    }

    fn copy_failpoint_memory(source: &FailpointMemory) -> FailpointMemory {
        let copy = FailpointMemory::new();
        assert_eq!(copy.grow(source.size()), 0);
        let mut bytes = vec![0; (source.size() * crate::WASM_PAGE_SIZE) as usize];
        source.read(0, &mut bytes);
        copy.write(0, &bytes);
        copy
    }

    // Find a real page-growth boundary through the map API, without copying its layout.
    // Only the independent probe index is mutated; the allocator remains valid.
    fn index_growth_boundary() -> (FreeSpanStore<FailpointMemory>, [FailpointMemory; 2]) {
        let memories = [FailpointMemory::new(), FailpointMemory::new()];
        let store = FreeSpanStore::new(memories[0].clone(), memories[1].clone()).unwrap();
        let half_page = ic_stable_paged_ordered_map::PAGE_CAP / 2;
        for i in 0..4096u64 {
            store
                .release_span(
                    1000 * i,
                    if i == ic_stable_paged_ordered_map::PAGE_CAP {
                        10
                    } else {
                        1
                    },
                )
                .unwrap();
            if (i + 1) % half_page != 0 {
                continue;
            }
            let probe_memory = copy_failpoint_memory(&memories[1]);
            let failed_call = probe_memory.grow_count() + 1;
            probe_memory.fail_at_grow(failed_call);
            let probe = StablePagedOrderedMap::init(probe_memory.clone()).unwrap();
            let result = probe.insert(u64::MAX - 1, 0);
            if result == Err(ic_stable_paged_ordered_map::Error::OutOfMemory) {
                assert_eq!(probe_memory.grow_count(), failed_call);
                assert_eq!(store.by_start.borrow().header().free_head, 0);
                store.validate().unwrap();
                return (store, memories);
            }
            assert_eq!(result, Ok(None));
        }
        panic!("bounded fixture did not reach a real index grow");
    }

    fn fill_predecessor_page(store: &FreeSpanStore<FailpointMemory>) {
        let before = store.by_start.borrow().header();
        for i in 0..ic_stable_paged_ordered_map::PAGE_CAP / 2 {
            store.release_span(1000 * i + 500, 1).unwrap();
        }
        let after = store.by_start.borrow().header();
        assert_eq!(after.page_count, before.page_count);
        assert_eq!(after.free_head, 0);
        store.validate().unwrap();
    }

    fn assert_failed_edit_preserves_owner(
        store: FreeSpanStore<FailpointMemory>,
        memories: [FailpointMemory; 2],
        operation: impl FnOnce(&FreeSpanStore<FailpointMemory>) -> Result<(), FreeSpanError>,
    ) {
        let before = admission_snapshot(&store, &memories);
        let spans = store.spans();
        let stats = store.allocator_stats();
        let returned = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| operation(&store)));
        assert_eq!(
            returned.expect("edit must return a native error, not panic"),
            Err(FreeSpanError::OutOfMemory)
        );
        assert!(
            admission_snapshot(&store, &memories) == before,
            "returned error changed paired bytes or cache"
        );
        assert_eq!(store.spans(), spans);
        assert_eq!(store.allocator_stats(), stats);
        store.validate().unwrap();
        let (records, index) = store.into_memories();
        let reopened = FreeSpanStore::init(records, index).expect("failed edit must reopen");
        assert_eq!(reopened.spans(), spans);
        assert_eq!(reopened.allocator_stats(), stats);
        reopened.validate().unwrap();
    }

    #[test]
    fn prepared_exact_prior_checks_late_edits() {
        for prior_position in [3, 4] {
            for stale in [false, true] {
                let memories = [FailpointMemory::new(), FailpointMemory::new()];
                let store = FreeSpanStore::new(memories[0].clone(), memories[1].clone()).unwrap();
                let whole = FreeSpan {
                    start_slot: 0,
                    len: 100,
                };
                let neighbor = FreeSpan {
                    start_slot: 500,
                    len: 10,
                };
                store.release(whole).unwrap();
                store.release(neighbor).unwrap();
                let wrong_id = store.by_start.borrow().get(whole.start_slot).unwrap();
                let original_id = store.by_start.borrow().get(neighbor.start_slot).unwrap();
                assert_ne!(wrong_id, original_id);
                let mut retired = Vec::new();
                if prior_position == 4 {
                    retired.push(FreeSpan {
                        start_slot: 300,
                        len: 10,
                    });
                }
                retired.push(FreeSpan {
                    start_slot: 510,
                    len: 10,
                });
                let mut preparation = store.prepare_replacement(40, &retired).unwrap();
                assert_eq!(preparation.selected_span(), Some(whole));
                let edits = &preparation.image.as_ref().unwrap().index_edits;
                assert_eq!(edits.len(), prior_position + 1);
                assert_eq!(
                    edits[prior_position - 1],
                    ic_stable_paged_ordered_map::Edit::Remove {
                        key: neighbor.start_slot
                    },
                );
                if stale {
                    assert_eq!(
                        preparation
                            .index
                            .insert(neighbor.start_slot, wrong_id)
                            .unwrap(),
                        Some(original_id),
                    );
                    let before = admission_snapshot(&store, &memories);
                    match preparation.ready() {
                        Err(error) => assert_eq!(error, FreeSpanError::BinInvariant),
                        Ok(_) => panic!("exact-prior change at edit {prior_position} was admitted"),
                    }
                    drop(preparation);
                    assert!(admission_snapshot(&store, &memories) == before);
                    assert_eq!(
                        store.by_start.borrow().get(neighbor.start_slot),
                        Some(wrong_id)
                    );
                    eprintln!(
                        "EXACT_PRIOR edit {prior_position}: exact BinInvariant, bytes/cache unchanged"
                    );
                } else {
                    preparation.ready().unwrap().publish();
                    drop(preparation);
                    let mut expected = vec![FreeSpan {
                        start_slot: 40,
                        len: 60,
                    }];
                    if prior_position == 4 {
                        expected.push(FreeSpan {
                            start_slot: 300,
                            len: 10,
                        });
                    }
                    expected.push(FreeSpan {
                        start_slot: 500,
                        len: 20,
                    });
                    assert_eq!(store.spans(), expected);
                    assert_allocator_stats_match_spans(&store);
                    store.validate().unwrap();
                    let (records, index) = store.into_memories();
                    let reopened = FreeSpanStore::init(records, index).unwrap();
                    assert_eq!(reopened.spans(), expected);
                    assert_allocator_stats_match_spans(&reopened);
                    reopened.validate().unwrap();
                }
            }
        }
    }

    #[test]
    fn width_allocator_record_grow_is_unpublished() {
        let memories = [FailpointMemory::new(), FailpointMemory::new()];
        let store = FreeSpanStore::new(memories[0].clone(), memories[1].clone()).unwrap();
        let capacity =
            (memories[0].size() * crate::WASM_PAGE_SIZE - RECORDS_OFFSET) / RECORD_STRIDE;
        for i in 0..capacity {
            store.release_span(1000 * i, 1).unwrap();
        }
        assert_eq!(store.header().record_slots, capacity);
        let failing = memories[0].clone();
        let failed_call = failing.grow_count() + 1;
        failing.fail_at_grow(failed_call);
        assert_failed_edit_preserves_owner(store, memories, |s| s.release_span(capacity * 1000, 1));
        assert_eq!(failing.grow_count(), failed_call);
        eprintln!("WIDTH_ALLOCATOR record grow: exact Err, bytes/cache and reopen preserved");
    }

    #[test]
    fn width_allocator_index_grow_is_unpublished() {
        let (store, memories) = index_growth_boundary();
        let failing = memories[1].clone();
        let failed_call = failing.grow_count() + 1;
        failing.fail_at_grow(failed_call);
        assert_failed_edit_preserves_owner(store, memories, |s| s.release_span(u64::MAX - 1, 1));
        assert_eq!(failing.grow_count(), failed_call);
        eprintln!("WIDTH_ALLOCATOR index grow: exact Err, distinct spans and reopen preserved");
    }

    #[test]
    fn width_allocator_restore_grow_is_unpublished() {
        let (store, memories) = index_growth_boundary();
        let start = 1000 * ic_stable_paged_ordered_map::PAGE_CAP;
        fill_predecessor_page(&store);
        let taken = store.take_best_fit(2).unwrap().unwrap();
        assert_eq!(
            taken,
            FreeSpan {
                start_slot: start,
                len: 2
            }
        );
        assert_eq!(
            store.free_span_starting_at(start + 2),
            Some(FreeSpan {
                start_slot: start + 2,
                len: 8
            })
        );
        // Move the remainder to the first key of its page while its predecessor
        // page stays full; consume only unrelated free spans, not the prefix.
        for i in ic_stable_paged_ordered_map::PAGE_CAP / 2..ic_stable_paged_ordered_map::PAGE_CAP {
            assert_eq!(
                store.take_prefix_at(1000 * i, 1).unwrap(),
                Some(FreeSpan {
                    start_slot: 1000 * i,
                    len: 1
                })
            );
        }
        let page_count = store.by_start.borrow().header().page_count;
        // Keep this page nonempty after removing the remainder. Otherwise the
        // map correctly recycles that empty page and the restore needs no grow.
        store.release_span(start + 20, 1).unwrap();
        assert_eq!(store.by_start.borrow().header().page_count, page_count);
        assert_eq!(store.by_start.borrow().header().free_head, 0);
        store.validate().unwrap();
        let probe_memory = copy_failpoint_memory(&memories[1]);
        probe_memory.fail_at_grow(probe_memory.grow_count() + 1);
        let mut probe = StablePagedOrderedMap::init(probe_memory).unwrap();
        let id = store.by_start.borrow().get(start + 2).unwrap();
        assert!(matches!(
            probe.prepare_edits(&[
                ic_stable_paged_ordered_map::Edit::Remove { key: start + 2 },
                ic_stable_paged_ordered_map::Edit::Insert {
                    key: start,
                    value: id
                },
            ]),
            Err(ic_stable_paged_ordered_map::PrepareError::Grow { .. })
        ));
        let failing = memories[1].clone();
        let failed_call = failing.grow_count() + 1;
        failing.fail_at_grow(failed_call);
        assert_failed_edit_preserves_owner(store, memories, |s| s.restore_allocated_prefix(taken));
        assert_eq!(failing.grow_count(), failed_call);
        eprintln!(
            "WIDTH_ALLOCATOR restore grow: actual allocated prefix and unrelated spans preserved"
        );
    }

    #[test]
    fn width_allocator_pair_grow_is_unpublished() {
        let (store, memories) = index_growth_boundary();
        fill_predecessor_page(&store);
        let left = FreeSpan {
            start_slot: 0,
            len: 1,
        };
        let right = FreeSpan {
            start_slot: 1000,
            len: 1,
        };
        let target = FreeSpan {
            start_slot: u64::MAX - 1,
            len: 1,
        };
        let probe_memory = copy_failpoint_memory(&memories[1]);
        probe_memory.fail_at_grow(probe_memory.grow_count() + 1);
        let mut probe = StablePagedOrderedMap::init(probe_memory).unwrap();
        assert!(matches!(
            probe.prepare_edits(&[
                ic_stable_paged_ordered_map::Edit::Remove {
                    key: left.start_slot
                },
                ic_stable_paged_ordered_map::Edit::Remove {
                    key: right.start_slot
                },
                ic_stable_paged_ordered_map::Edit::Insert {
                    key: target.start_slot,
                    value: 1
                },
            ]),
            Err(ic_stable_paged_ordered_map::PrepareError::Grow { .. })
        ));
        let (records, index) = store.into_memories();
        let store = FreeSpanStore::init(records, index).unwrap();
        assert!(
            !store.max_heap_ready.get(),
            "reopen starts with a cold cache"
        );
        let failing = memories[1].clone();
        let failed_call = failing.grow_count() + 1;
        failing.fail_at_grow(failed_call);
        assert_failed_edit_preserves_owner(store, memories, |s| {
            s.replace_exact_pair_with(left, right, target)
        });
        assert_eq!(failing.grow_count(), failed_call);
        eprintln!(
            "WIDTH_ALLOCATOR pair grow: both inputs, unrelated spans and cold cache preserved"
        );
    }

    #[test]
    fn width_allocator_success_paths_reopen() {
        let store = test_store();
        for span in [
            FreeSpan {
                start_slot: 100,
                len: 10,
            },
            FreeSpan {
                start_slot: 130,
                len: 10,
            },
            FreeSpan {
                start_slot: 120,
                len: 10,
            },
            FreeSpan {
                start_slot: 110,
                len: 10,
            },
            FreeSpan {
                start_slot: 140,
                len: 10,
            },
            FreeSpan {
                start_slot: 200,
                len: 5,
            },
        ] {
            store.release(span).unwrap();
            store.validate().unwrap();
            assert_allocator_stats_match_spans(&store);
        }
        let expected = vec![
            FreeSpan {
                start_slot: 100,
                len: 50,
            },
            FreeSpan {
                start_slot: 200,
                len: 5,
            },
        ];
        assert_eq!(store.spans(), expected);
        let best = store.take_best_fit(7).unwrap().unwrap();
        assert_eq!(
            best,
            FreeSpan {
                start_slot: 100,
                len: 7
            }
        );
        store.restore_allocated_prefix(best).unwrap();
        let prefix = store.take_prefix_at(100, 11).unwrap().unwrap();
        assert_eq!(
            prefix,
            FreeSpan {
                start_slot: 100,
                len: 11
            }
        );
        store.restore_allocated_prefix(prefix).unwrap();
        let whole = store.take_best_fit_whole(50).unwrap().unwrap();
        assert_eq!(whole, expected[0]);
        store.release(whole).unwrap();
        assert_eq!(store.spans(), expected);
        assert_allocator_stats_match_spans(&store);
        let (records, index) = store.into_memories();
        let reopened = FreeSpanStore::init(records, index).unwrap();
        assert_eq!(reopened.spans(), expected);
        reopened.validate().unwrap();

        let store = test_store();
        let left = FreeSpan {
            start_slot: 110,
            len: 10,
        };
        let right = FreeSpan {
            start_slot: 140,
            len: 10,
        };
        for span in [
            left,
            right,
            FreeSpan {
                start_slot: 80,
                len: 20,
            },
            FreeSpan {
                start_slot: 160,
                len: 10,
            },
            FreeSpan {
                start_slot: 300,
                len: 15,
            },
        ] {
            store.release(span).unwrap();
        }
        store
            .replace_exact_pair_with(
                left,
                right,
                FreeSpan {
                    start_slot: 100,
                    len: 60,
                },
            )
            .unwrap();
        let expected = vec![
            FreeSpan {
                start_slot: 80,
                len: 90,
            },
            FreeSpan {
                start_slot: 300,
                len: 15,
            },
        ];
        assert_eq!(
            store.spans(),
            expected,
            "pair replacement coalesces both outside neighbors"
        );
        store.validate().unwrap();
        assert_allocator_stats_match_spans(&store);
        let (records, index) = store.into_memories();
        let reopened = FreeSpanStore::init(records, index).unwrap();
        assert_eq!(reopened.spans(), expected);
        assert_allocator_stats_match_spans(&reopened);
        reopened.validate().unwrap();
    }

    #[test]
    fn width_allocator_rejections_preserve_guards() {
        let memories = [FailpointMemory::new(), FailpointMemory::new()];
        let store = FreeSpanStore::new(memories[0].clone(), memories[1].clone()).unwrap();
        let left = FreeSpan {
            start_slot: 100,
            len: 10,
        };
        let right = FreeSpan {
            start_slot: 200,
            len: 20,
        };
        store.release(left).unwrap();
        store.release(right).unwrap();
        let before = admission_snapshot(&store, &memories);
        assert_eq!(
            FreeSpanStore::<FailpointMemory>::map_index_prepare_error(
                ic_stable_paged_ordered_map::PrepareError::Edit {
                    at: 0,
                    source: ic_stable_paged_ordered_map::Error::DirectoryFull,
                }
            ),
            FreeSpanError::IndexFull
        );
        let empty = FreeSpan {
            start_slot: 100,
            len: 0,
        };
        let overflow = FreeSpan {
            start_slot: u64::MAX,
            len: 1,
        };
        let previous_overlap = FreeSpan {
            start_slot: 105,
            len: 2,
        };
        let next_overlap = FreeSpan {
            start_slot: 195,
            len: 10,
        };
        for (span, error) in [
            (empty, FreeSpanError::EmptySpan),
            (overflow, FreeSpanError::SpanOverflow { span: overflow }),
            (
                FreeSpan {
                    start_slot: 100,
                    len: 3,
                },
                FreeSpanError::DuplicateStart { start_slot: 100 },
            ),
            (
                previous_overlap,
                FreeSpanError::OverlapPrevious {
                    previous: left,
                    inserted: previous_overlap,
                },
            ),
            (
                next_overlap,
                FreeSpanError::OverlapNext {
                    inserted: next_overlap,
                    next: right,
                },
            ),
        ] {
            assert_eq!(store.release(span), Err(error));
            assert!(admission_snapshot(&store, &memories) == before);
        }
        let wrong = FreeSpan {
            start_slot: 100,
            len: 11,
        };
        assert_eq!(
            store.replace_exact_pair_with(wrong, right, empty),
            Err(FreeSpanError::MissingSpan { span: wrong })
        );
        assert_eq!(
            store.replace_exact_pair_with(left, left, empty),
            Err(FreeSpanError::EmptySpan)
        );
        assert_eq!(
            store.remove_span_exact(wrong),
            Err(FreeSpanError::MissingSpan { span: wrong })
        );
        let inner_overlap = FreeSpan {
            start_slot: 95,
            len: 105,
        };
        assert_eq!(
            store.restore_allocated_prefix(inner_overlap),
            Err(FreeSpanError::OverlapNext {
                inserted: inner_overlap,
                next: left
            })
        );
        assert!(
            admission_snapshot(&store, &memories) == before,
            "rejection changed canonical bytes or cache"
        );
        assert_eq!(store.spans(), vec![left, right]);
        store.validate().unwrap();
        let (records, index) = store.into_memories();
        let reopened = FreeSpanStore::init(records, index).unwrap();
        assert_eq!(reopened.spans(), vec![left, right]);
        reopened.validate().unwrap();
    }

    #[test]
    fn a0_admission_exact_span() {
        let manager = MemoryManager::init(DefaultMemoryImpl::default());
        let memories = [
            manager.get(MemoryId::new(40)),
            manager.get(MemoryId::new(41)),
        ];
        let store = FreeSpanStore::init(memories[0].clone(), memories[1].clone()).unwrap();
        for i in 0..96 {
            store.release_span(10 * i, 1).unwrap();
        }
        for i in 0..31 {
            assert_eq!(
                store.take_prefix_at(10 * i, 1).unwrap(),
                Some(FreeSpan {
                    start_slot: 10 * i,
                    len: 1
                })
            );
        }
        let expected: Vec<_> = (31..96)
            .map(|i| FreeSpan {
                start_slot: 10 * i,
                len: 1,
            })
            .collect();
        assert_eq!(store.spans(), expected);
        store.validate().unwrap();
        store.by_start.borrow().validate().unwrap();
        let index_before = store.by_start.borrow().header();
        assert_eq!(index_before.len, 65);
        assert_eq!(index_before.dir_len, 2);
        assert_eq!(index_before.page_count, 2);
        assert_eq!(index_before.first_page, 1);
        assert_eq!(index_before.last_page, 2);
        assert_eq!(store.by_start.borrow().first(), Some((310, 32)));
        assert_eq!(store.by_start.borrow().last(), Some((950, 96)));
        let before = admission_snapshot(&store, &memories);
        let wrong = FreeSpan {
            start_slot: 310,
            len: 2,
        };
        let error = store.remove_span_exact(wrong).unwrap_err();
        let after = admission_snapshot(&store, &memories);
        let index_after = store.by_start.borrow().header();
        eprintln!("ADMISSION_EXACT reached mismatch: {index_before:?} -> {index_after:?}");
        assert_eq!(error, FreeSpanError::MissingSpan { span: wrong });
        assert_eq!(store.spans(), expected);
        assert!(
            before == after,
            "exact-span rejection changed backing or heap"
        );
        assert_eq!(index_after, index_before);
        assert_allocator_stats_match_spans(&store);
        store.validate().unwrap();
    }

    #[test]
    fn a0_admission_pair() {
        let left = FreeSpan {
            start_slot: 6,
            len: 4,
        };
        let middle = FreeSpan {
            start_slot: 20,
            len: 4,
        };
        let peer = FreeSpan {
            start_slot: 40,
            len: 4,
        };
        for (right, replacement, expected_error, phase) in [
            (
                left,
                left,
                FreeSpanError::MissingSpan { span: left },
                "same-input",
            ),
            (
                middle,
                peer,
                FreeSpanError::DuplicateStart { start_slot: 40 },
                "replacement-key",
            ),
        ] {
            let manager = MemoryManager::init(DefaultMemoryImpl::default());
            let memories = [
                manager.get(MemoryId::new(10)),
                manager.get(MemoryId::new(11)),
            ];
            let store = FreeSpanStore::init(memories[0].clone(), memories[1].clone()).unwrap();
            for span in [left, middle, peer] {
                store.release(span).unwrap();
            }
            store.validate().unwrap();
            let before = admission_snapshot(&store, &memories);
            let empty = FreeSpan {
                start_slot: 6,
                len: 0,
            };
            let missing_left = FreeSpan {
                start_slot: 6,
                len: 5,
            };
            let missing_right = FreeSpan {
                start_slot: 20,
                len: 5,
            };
            let overflow = FreeSpan {
                start_slot: u64::MAX,
                len: 1,
            };
            let overlap = FreeSpan {
                start_slot: 39,
                len: 6,
            };
            for (a, b, target, expected) in [
                (
                    missing_left,
                    missing_right,
                    empty,
                    FreeSpanError::MissingSpan { span: missing_left },
                ),
                (
                    left,
                    missing_right,
                    empty,
                    FreeSpanError::MissingSpan {
                        span: missing_right,
                    },
                ),
                (left, right, empty, FreeSpanError::EmptySpan),
                (
                    left,
                    right,
                    overflow,
                    FreeSpanError::SpanOverflow { span: overflow },
                ),
                (
                    left,
                    right,
                    overlap,
                    FreeSpanError::OverlapNext {
                        inserted: overlap,
                        next: peer,
                    },
                ),
            ] {
                assert_eq!(store.replace_exact_pair_with(a, b, target), Err(expected));
                assert!(
                    admission_snapshot(&store, &memories) == before,
                    "{phase}: precedence rejection changed state"
                );
            }
            eprintln!("ADMISSION_PAIR {phase}: precedence passed; reached primary rejection");
            assert_eq!(
                store.replace_exact_pair_with(left, right, replacement),
                Err(expected_error)
            );
            let after = admission_snapshot(&store, &memories);
            assert!(
                before == after,
                "{phase}: pair rejection changed backing or heap"
            );
            assert_eq!(store.spans(), vec![left, middle, peer]);
            assert_allocator_stats_match_spans(&store);
            store.validate().unwrap();
            eprintln!("ADMISSION_PAIR {phase}: unchanged ownership verified");
        }
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
    fn header_reserved_region_is_zeroed_and_validated_on_reopen() {
        let m = MemoryManager::init(DefaultMemoryImpl::default());
        let records = m.get(MemoryId::new(12));
        let by_start = m.get(MemoryId::new(13));

        let store = FreeSpanStore::init(records.clone(), by_start.clone()).unwrap();
        store.release_span(100, 20).unwrap();
        drop(store);

        let mut reserved = [0u8; RESERVED_SIZE];
        records.read(OFFSET_RESERVED, &mut reserved);
        assert!(reserved.iter().all(|&byte| byte == 0));

        // Normal operate -> drop -> reopen stays Ok.
        let store = FreeSpanStore::init(records.clone(), by_start).unwrap();
        assert_eq!(store.len(), 1);
        drop(store);

        records.write(OFFSET_RESERVED, &[1]);
        assert!(matches!(
            FreeSpanStore::init(records, m.get(MemoryId::new(13))),
            Err(InitError::InvalidLayout)
        ));
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
    fn drained_store_reopens_and_allocates() {
        // Draining the final span (exact-fit take) must leave reopenable
        // store state, and the reopened store must allocate again. Covers the
        // leaf-relocate path that grows a block in place by consuming the
        // adjacent free span whole, then crosses an upgrade boundary.
        let s = test_store();
        s.release_span(1000, 16).unwrap();
        let taken = s.take_prefix_at(1000, 16).unwrap();
        assert_eq!(
            taken,
            Some(super::FreeSpan {
                start_slot: 1000,
                len: 16
            })
        );
        assert!(s.spans().is_empty());
        let (meta, bs) = s.into_memories();
        let reopened = super::FreeSpanStore::init(meta, bs).expect("drained store must reopen");
        assert!(reopened.spans().is_empty());
        reopened.release_span(2000, 32).unwrap();
        let taken = reopened.take_best_fit(16).unwrap().unwrap();
        assert_eq!((taken.start_slot, taken.len), (2000, 16));
        assert_allocator_stats_match_spans(&reopened);
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
