//! Out-edge iterators and slab prefetch helpers.

use crate::lara::operation_error::{LaraOperationError, VertexAccess};
use crate::{
    VertexId,
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex, CsrVertexTombstoneScan},
};
use ic_stable_structures::Memory;
use std::{iter::FusedIterator, num::NonZero};

use super::log::HeaderV1 as LogHeaderV1;
use super::{
    DeleteTarget, EdgeStore, INLINE_EDGE_BYTES, OUT_EDGE_SLAB_CHUNK_SLOTS,
    OUT_EDGE_SLAB_PREFETCH_MIN_BYTES, decode_delete_target,
};
/// Descending scan for a **log-backed** row without prefetching every live log edge into a `Vec`.
///
/// Same logical order as [`OutEdgesIter`]: scan the core-LARA overflow log chain (newest first),
/// then walk the slab prefix in descending slot order, skipping slab slots targeted by overflow-log
/// delete entries.
pub(super) struct LogBackedDescIter<'a, E: CsrEdge, M: Memory> {
    pub(super) store: &'a EdgeStore<E, M>,
    pub(super) leaf: u32,
    pub(super) next_log: i32,
    pub(super) remaining_log: u32,
    pub(super) base_slot_start: u64,
    pub(super) remaining_slab: u32,
    pub(super) yield_remaining: u32,
    pub(super) log_header: LogHeaderV1,
    pub(super) log_table: Option<Vec<u8>>,
    pub(super) slab_chunk: Option<OutEdgeSlabChunk>,
    pub(super) deleted_log_indices: Vec<u32>,
    pub(super) deleted_slab_offsets: Vec<u32>,
    pub(super) sorted_slab_deletes: bool,
}

impl<'a, E, M> LogBackedDescIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    fn decode_slab_slot(&mut self, slot_idx: u32) -> E {
        out_edge_slab_decode_slot(
            self.store,
            self.base_slot_start,
            &mut self.slab_chunk,
            slot_idx,
        )
    }
}

impl<'a, E, M> Iterator for LogBackedDescIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        if self.yield_remaining == 0 {
            return None;
        }
        if self.next_log >= 0 {
            if self.log_table.is_none() {
                let mut buf = Vec::new();
                self.store
                    .log
                    .read_segment_entry_table_into(&self.log_header, self.leaf, &mut buf);
                self.log_table = Some(buf);
            }
            let log_table_sl = self
                .log_table
                .as_ref()
                .and_then(|b| (!b.is_empty()).then_some(b.as_slice()));
            while self.next_log >= 0 {
                if self.remaining_log == 0 {
                    self.next_log = -1;
                    break;
                }
                self.remaining_log -= 1;
                let log_idx = self.next_log as u32;
                let (prev, src, edge) = self.store.read_log_edge_from_table_or_store(
                    &self.log_header,
                    self.leaf,
                    log_idx,
                    log_table_sl,
                );
                self.next_log = prev;
                if let Some(target) = decode_delete_target(src) {
                    match target {
                        DeleteTarget::Slab(offset) => self.deleted_slab_offsets.push(offset),
                        DeleteTarget::Log(index) => self.deleted_log_indices.push(index),
                    }
                    continue;
                }
                if let Some(pos) = self.deleted_log_indices.iter().position(|&d| d == log_idx) {
                    self.deleted_log_indices.swap_remove(pos);
                    continue;
                }
                self.yield_remaining -= 1;
                return Some(edge);
            }
        }
        if !self.sorted_slab_deletes {
            self.sorted_slab_deletes = true;
            self.deleted_slab_offsets.sort_unstable();
        }
        while self.remaining_slab > 0 {
            self.remaining_slab -= 1;
            let slot_idx = self.remaining_slab;
            if self.deleted_slab_offsets.binary_search(&slot_idx).is_ok() {
                continue;
            }
            let edge = self.decode_slab_slot(slot_idx);
            if edge.is_deleted_slot() {
                continue;
            }
            self.yield_remaining -= 1;
            return Some(edge);
        }
        self.yield_remaining = 0;
        None
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = usize::try_from(self.yield_remaining).unwrap_or(usize::MAX);
        (n, Some(n))
    }
}

impl<E, M> ExactSizeIterator for LogBackedDescIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}

impl<E, M> FusedIterator for LogBackedDescIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}

#[inline]
pub(super) fn leaf_segment(vid: VertexId, segment_size: u32) -> u32 {
    u32::from(vid) / segment_size.max(1)
}

/// Contiguous byte window for one prefetch of the out-edge slab (slot indices `[chunk_low, chunk_high]`).
pub(super) struct OutEdgeSlabChunk {
    pub(super) buf: Vec<u8>,
    pub(super) chunk_low: u32,
    pub(super) chunk_high: u32,
}

#[inline]
pub(super) fn out_edge_slab_prefetch_chunk<E: CsrEdge, M: Memory>(
    cache: &mut OutEdgeSlabChunk,
    store: &EdgeStore<E, M>,
    base: u64,
    slot_idx: u32,
) {
    let high = slot_idx;
    let span = OUT_EDGE_SLAB_CHUNK_SLOTS.min(high.saturating_add(1));
    let low = high.saturating_sub(span - 1);
    let nbytes = span as usize * E::BYTES;
    cache.buf.resize(nbytes, 0);
    cache.chunk_low = low;
    cache.chunk_high = high;
    let start = base.saturating_add(u64::from(low));
    store.read_slots_contiguous(start, &mut cache.buf);
}

pub(super) fn out_edge_slab_prefetch_chunk_asc<E: CsrEdge, M: Memory>(
    cache: &mut OutEdgeSlabChunk,
    store: &EdgeStore<E, M>,
    base: u64,
    slot_idx: u32,
    total_slots: u32,
) {
    let low = slot_idx;
    let remaining = total_slots.saturating_sub(slot_idx);
    let span = OUT_EDGE_SLAB_CHUNK_SLOTS.min(remaining);
    let high = low.saturating_add(span.saturating_sub(1));
    let nbytes = span as usize * E::BYTES;
    cache.buf.resize(nbytes, 0);
    cache.chunk_low = low;
    cache.chunk_high = high;
    let start = base.saturating_add(u64::from(low));
    store.read_slots_contiguous(start, &mut cache.buf);
}

#[inline]
pub(super) fn out_edge_slab_decode_slot<E: CsrEdge, M: Memory>(
    store: &EdgeStore<E, M>,
    base_slot_start: u64,
    slab_chunk: &mut Option<OutEdgeSlabChunk>,
    slot_idx: u32,
) -> E {
    if let Some(cache) = slab_chunk {
        if cache.buf.is_empty() || slot_idx < cache.chunk_low || slot_idx > cache.chunk_high {
            out_edge_slab_prefetch_chunk(cache, store, base_slot_start, slot_idx);
        }
        let off = (slot_idx - cache.chunk_low) as usize * E::BYTES;
        debug_assert!(off + E::BYTES <= cache.buf.len());
        E::read_from(&cache.buf[off..off + E::BYTES]).with_slot_index(slot_idx)
    } else {
        store
            .read_slot(base_slot_start + u64::from(slot_idx))
            .with_slot_index(slot_idx)
    }
}

#[inline]
pub(super) fn out_edge_slab_decode_slot_asc<E: CsrEdge, M: Memory>(
    store: &EdgeStore<E, M>,
    base_slot_start: u64,
    slab_chunk: &mut Option<OutEdgeSlabChunk>,
    slot_idx: u32,
    total_slots: u32,
) -> E {
    if let Some(cache) = slab_chunk {
        if cache.buf.is_empty() || slot_idx < cache.chunk_low || slot_idx > cache.chunk_high {
            out_edge_slab_prefetch_chunk_asc(cache, store, base_slot_start, slot_idx, total_slots);
        }
        let off = (slot_idx - cache.chunk_low) as usize * E::BYTES;
        debug_assert!(off + E::BYTES <= cache.buf.len());
        E::read_from(&cache.buf[off..off + E::BYTES]).with_slot_index(slot_idx)
    } else {
        store
            .read_slot(base_slot_start + u64::from(slot_idx))
            .with_slot_index(slot_idx)
    }
}

/// Iterator over **slab-resident** outgoing edges in [`EdgeStore`]'s default **descending** slot
/// order (high index → low, skipping tombstoned slots). For rows with no overflow log
/// (`log_head < 0`) and no overflow-log delete markers; same sequence as the slab phase of
/// [`OutEdgesIter`].
pub(crate) struct OutEdgeSlabIter<'a, E: CsrEdge, M: Memory> {
    store: &'a EdgeStore<E, M>,
    base_slot_start: u64,
    remaining_slab: u32,
    yield_remaining: u32,
    slab_chunk: Option<OutEdgeSlabChunk>,
}

impl<'a, E: CsrEdge, M: Memory> OutEdgeSlabIter<'a, E, M> {
    pub(crate) fn try_new(
        store: &'a EdgeStore<E, M>,
        base_slot_start: u64,
        stored: u32,
        live: u32,
    ) -> Result<Self, LaraOperationError> {
        let nbytes = (stored as usize)
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let slab_chunk = if nbytes >= OUT_EDGE_SLAB_PREFETCH_MIN_BYTES {
            Some(OutEdgeSlabChunk {
                buf: Vec::new(),
                chunk_low: 0,
                chunk_high: 0,
            })
        } else {
            None
        };
        Ok(Self {
            store,
            base_slot_start,
            remaining_slab: stored,
            yield_remaining: live,
            slab_chunk,
        })
    }

    /// Descending slab scan (same order as [`Iterator::next`]). When `raw_matches` is `Some`,
    /// it is applied to each slot's encoded bytes **before** [`CsrEdge::read_from`]; a `false`
    /// result skips the slot without decoding (same contract as [`EdgeStore::visit_out_edges`]).
    pub(crate) fn next_live_edge_filtered(
        &mut self,
        raw_matches: &mut Option<&mut dyn FnMut(&[u8]) -> bool>,
    ) -> Option<E> {
        if self.yield_remaining == 0 {
            return None;
        }
        let mut single = [0u8; INLINE_EDGE_BYTES];
        while self.remaining_slab > 0 {
            self.remaining_slab -= 1;
            let slot_idx = self.remaining_slab;
            let bytes: &[u8] = if let Some(ref mut cache) = self.slab_chunk {
                if cache.buf.is_empty() || slot_idx < cache.chunk_low || slot_idx > cache.chunk_high
                {
                    out_edge_slab_prefetch_chunk(cache, self.store, self.base_slot_start, slot_idx);
                }
                let off = (slot_idx - cache.chunk_low) as usize * E::BYTES;
                debug_assert!(off + E::BYTES <= cache.buf.len());
                &cache.buf[off..off + E::BYTES]
            } else {
                debug_assert!(
                    E::BYTES <= INLINE_EDGE_BYTES,
                    "slab_chunk=None only when stored span fits in prefetch threshold"
                );
                let start = self
                    .base_slot_start
                    .checked_add(u64::from(slot_idx))
                    .unwrap();
                self.store
                    .read_slots_contiguous(start, &mut single[..E::BYTES]);
                &single[..E::BYTES]
            };
            if let Some(raw_m) = raw_matches.as_mut() {
                if !raw_m(bytes) {
                    continue;
                }
            }
            let edge = E::read_from(bytes).with_slot_index(slot_idx);
            if edge.is_deleted_slot() {
                continue;
            }
            self.yield_remaining -= 1;
            return Some(edge);
        }
        debug_assert_eq!(
            self.yield_remaining, 0,
            "slab scan ended before yielding all logical edges"
        );
        self.yield_remaining = 0;
        None
    }

    pub(crate) fn next_live_edge_with_slot(&mut self) -> Option<(u32, E)> {
        if self.yield_remaining == 0 {
            return None;
        }
        while self.remaining_slab > 0 {
            self.remaining_slab -= 1;
            let slot_idx = self.remaining_slab;
            let edge = out_edge_slab_decode_slot(
                self.store,
                self.base_slot_start,
                &mut self.slab_chunk,
                slot_idx,
            );
            if edge.is_deleted_slot() {
                continue;
            }
            self.yield_remaining -= 1;
            return Some((slot_idx, edge));
        }
        debug_assert_eq!(
            self.yield_remaining, 0,
            "slab iterator exhausted before yielding expected live edge count"
        );
        self.yield_remaining = 0;
        None
    }
}

impl<E: CsrEdge, M: Memory> Iterator for OutEdgeSlabIter<'_, E, M> {
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        let mut none = None;
        self.next_live_edge_filtered(&mut none)
    }

    fn advance_by(&mut self, n: usize) -> Result<(), NonZero<usize>> {
        let mut remaining = n;
        while remaining > 0 {
            if self.next().is_none() {
                return Err(NonZero::new(remaining).expect("remaining > 0"));
            }
            remaining -= 1;
        }
        Ok(())
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.advance_by(n).ok()?;
        self.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = usize::try_from(self.yield_remaining).unwrap_or(usize::MAX);
        (n, Some(n))
    }
}

impl<E: CsrEdge, M: Memory> ExactSizeIterator for OutEdgeSlabIter<'_, E, M> {}

impl<E: CsrEdge, M: Memory> FusedIterator for OutEdgeSlabIter<'_, E, M> {}

/// Iterator over outgoing edges in [`EdgeStore`]'s **default descending scan order**:
/// overflow log from the chain head first (each step follows the `prev` link), then live slab
/// slots **high index to low** (skipping tombstoned slots).
///
/// Log-backed rows **prefetch** the overflow chain at construction (same classification as the
/// historical lazy walk): live log edges are buffered in head-first order, and log delete entries
/// populate a sorted slab-offset list so the slab phase can skip masked slots without
/// decoding them.
///
/// This is **not** the same order as [`EdgeStore::asc_out_edges`] (slot /
/// materialization order). Prefer this iterator for hot contiguous reads; use `asc_out_edges`
/// or reverse the collected vector when you need ascending slot layout (e.g. rebalance packing).
///
/// For slab-only rows (`log_head < 0`), only the descending slab phase runs.
///
/// For clean slab-only rows whose stored slab is at least `OUT_EDGE_SLAB_PREFETCH_MIN_BYTES`,
/// `slab_chunk` prefetches a backward window of up to `OUT_EDGE_SLAB_CHUNK_SLOTS` consecutive slab
/// slots so [`Iterator::next`] can issue one stable read per chunk instead of per edge. Decode
/// logic is shared with [`OutEdgeSlabIter`].
pub struct OutEdgesIter<'a, E: CsrEdge, M: Memory> {
    pub(super) store: &'a EdgeStore<E, M>,
    pub(super) base_slot_start: u64,
    /// Count of slab prefix slots still to scan; slots are visited `remaining_slab - 1` down to `0`.
    pub(super) remaining_slab: u32,
    /// Live edges not yet yielded (matches [`ExactSizeIterator`] contract).
    pub(super) yield_remaining: u32,
    /// Live overflow-log edges in descending-scan order (newest chain head first).
    pub(super) log_edges: Vec<E>,
    pub(super) log_pos: usize,
    pub(super) slab_chunk: Option<OutEdgeSlabChunk>,
    /// Slab slot indices (within this row's slab prefix) targeted by overflow-log delete entries, sorted for
    /// binary search. Slots in this set are skipped during the slab phase without decoding.
    pub(super) deleted_slab_offsets: Vec<u32>,
}

impl<'a, E, M> OutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Iterator that yields no edges (descending scan order contract preserved).
    pub(crate) fn empty(store: &'a EdgeStore<E, M>) -> Self {
        Self {
            store,
            base_slot_start: 0,
            remaining_slab: 0,
            yield_remaining: 0,
            log_edges: Vec::new(),
            log_pos: 0,
            slab_chunk: None,
            deleted_slab_offsets: Vec::new(),
        }
    }

    #[inline]
    fn slab_slot_deleted(&self, slot_idx: u32) -> bool {
        self.deleted_slab_offsets.binary_search(&slot_idx).is_ok()
    }

    fn decode_slab_slot(&mut self, slot_idx: u32) -> E {
        out_edge_slab_decode_slot(
            self.store,
            self.base_slot_start,
            &mut self.slab_chunk,
            slot_idx,
        )
    }
}

impl<E, M> Iterator for OutEdgesIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        if self.yield_remaining == 0 {
            return None;
        }
        if self.log_pos < self.log_edges.len() {
            let edge = self.log_edges[self.log_pos];
            self.log_pos += 1;
            self.yield_remaining -= 1;
            return Some(edge);
        }

        while self.remaining_slab > 0 {
            self.remaining_slab -= 1;
            let slot_idx = self.remaining_slab;
            if self.slab_slot_deleted(slot_idx) {
                continue;
            }
            let edge = self.decode_slab_slot(slot_idx);
            if edge.is_deleted_slot() {
                continue;
            }
            self.yield_remaining -= 1;
            return Some(edge);
        }
        debug_assert_eq!(
            self.yield_remaining, 0,
            "slab scan ended before yielding all logical edges"
        );
        self.yield_remaining = 0;
        None
    }

    fn advance_by(&mut self, mut n: usize) -> Result<(), NonZero<usize>> {
        if n == 0 {
            return Ok(());
        }
        let log_rem = self.log_edges.len().saturating_sub(self.log_pos);
        let take = n.min(log_rem);
        self.log_pos += take;
        let take_u32 = u32::try_from(take).unwrap_or(u32::MAX);
        self.yield_remaining = self.yield_remaining.saturating_sub(take_u32);
        n -= take;
        if n == 0 {
            return Ok(());
        }

        while n > 0 {
            if self.yield_remaining == 0 {
                return Err(NonZero::new(n).expect("remaining > 0"));
            }
            if self.remaining_slab == 0 {
                return Err(NonZero::new(n).expect("remaining > 0"));
            }
            self.remaining_slab -= 1;
            let slot_idx = self.remaining_slab;
            if self.slab_slot_deleted(slot_idx) {
                continue;
            }
            let edge = self.decode_slab_slot(slot_idx);
            if edge.is_deleted_slot() {
                continue;
            }
            self.yield_remaining -= 1;
            n -= 1;
        }
        Ok(())
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.advance_by(n).ok()?;
        self.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = usize::try_from(self.yield_remaining).unwrap_or(usize::MAX);
        (n, Some(n))
    }
}

impl<E, M> ExactSizeIterator for OutEdgesIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}

impl<E, M> FusedIterator for OutEdgesIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}

/// Iterator over outgoing edges in **ascending** CSR slot / materialization order (matches
/// [`EdgeStore::asc_out_edges`]).
///
/// Slab slots scan low→high with fixed-size forward prefetch chunks. When a row has an overflow
/// log, the constructor folds log entries old→new into insertion/deletion caches; iteration then
/// streams live slab slots first and cached inserted log edges last.
pub struct AscOutEdgesIter<'a, E: CsrEdge, M: Memory> {
    pub(super) store: &'a EdgeStore<E, M>,
    pub(super) base_slot_start: u64,
    next_slot: u32,
    slab_slots: u32,
    remaining: u32,
    pub(super) slab_chunk: Option<OutEdgeSlabChunk>,
    deleted_slab_offsets: Vec<u32>,
    inserted_log_edges: std::vec::IntoIter<E>,
}

impl<'a, E, M> AscOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn empty(store: &'a EdgeStore<E, M>) -> Self {
        Self::with_log_replay(store, 0, 0, 0, Vec::new(), Vec::new())
    }

    pub(super) fn slab_only(
        store: &'a EdgeStore<E, M>,
        base_slot_start: u64,
        stored_degree: u32,
        remaining: u32,
    ) -> Self {
        Self::with_log_replay(
            store,
            base_slot_start,
            stored_degree,
            remaining,
            Vec::new(),
            Vec::new(),
        )
    }

    pub(super) fn with_log_replay(
        store: &'a EdgeStore<E, M>,
        base_slot_start: u64,
        slab_slots: u32,
        remaining: u32,
        deleted_slab_offsets: Vec<u32>,
        inserted_log_edges: Vec<E>,
    ) -> Self {
        let nbytes = (slab_slots as usize).saturating_mul(E::BYTES);
        let slab_chunk = if nbytes >= OUT_EDGE_SLAB_PREFETCH_MIN_BYTES {
            Some(OutEdgeSlabChunk {
                buf: Vec::new(),
                chunk_low: 0,
                chunk_high: 0,
            })
        } else {
            None
        };
        Self {
            store,
            base_slot_start,
            next_slot: 0,
            slab_slots,
            remaining,
            slab_chunk,
            deleted_slab_offsets,
            inserted_log_edges: inserted_log_edges.into_iter(),
        }
    }

    fn consume_deleted_slab_offset(&mut self, offset: u32) -> bool {
        if let Some(index) = self
            .deleted_slab_offsets
            .iter()
            .position(|deleted| *deleted == offset)
        {
            self.deleted_slab_offsets.swap_remove(index);
            true
        } else {
            false
        }
    }

    fn decode_slab_slot(&mut self, slot_idx: u32) -> E {
        out_edge_slab_decode_slot_asc(
            self.store,
            self.base_slot_start,
            &mut self.slab_chunk,
            slot_idx,
            self.slab_slots,
        )
    }
}

impl<'a, E, M> Iterator for AscOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining == 0 {
            return None;
        }
        while self.next_slot < self.slab_slots {
            let slot_idx = self.next_slot;
            self.next_slot = self.next_slot.checked_add(1)?;
            if self.consume_deleted_slab_offset(slot_idx) {
                continue;
            }
            let edge = self.decode_slab_slot(slot_idx);
            if edge.is_deleted_slot() {
                continue;
            }
            self.remaining = self.remaining.checked_sub(1)?;
            return Some(edge);
        }
        if let Some(edge) = self.inserted_log_edges.next() {
            self.remaining = self.remaining.checked_sub(1)?;
            return Some(edge);
        }
        debug_assert_eq!(
            self.remaining, 0,
            "asc scan ended before yielding all logical edges"
        );
        self.remaining = 0;
        None
    }

    fn advance_by(&mut self, n: usize) -> Result<(), NonZero<usize>> {
        let mut remaining = n;
        while remaining > 0 {
            if self.next().is_none() {
                return Err(NonZero::new(remaining).expect("remaining > 0"));
            }
            remaining -= 1;
        }
        Ok(())
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        self.advance_by(n).ok()?;
        self.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let n = usize::try_from(self.remaining).unwrap_or(usize::MAX);
        (n, Some(n))
    }
}

impl<'a, E, M> ExactSizeIterator for AscOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}

impl<'a, E, M> FusedIterator for AscOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
}
