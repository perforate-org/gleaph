//! Edge storage for LARA.
//!
//! The edge subsystem combines five stable-memory structures:
//!
//! - segment edge counts, used by update/maintenance code to decide when a
//!   segment is dense;
//! - the contiguous edge slab containing clean adjacency prefixes;
//! - per-segment overflow logs for inserts that cannot fit immediately on the
//!   slab;
//! - segment span metadata for locally relocated physical spans;
//! - free span metadata for retired physical ranges.
//!
//! [`EdgeStore::asc_out_edges`] materializes the row in **slot order**
//! (ascending slab indices, skipping tombstoned slots,
//! then overflow-log edges oldest-to-newest). Use this when you need the exact CSR
//! / insertion layout.
//!
//! **Default contiguous read contract:** [`EdgeStore::out_edges_iter`] (see [`OutEdgesIter`])
//! walks the overflow log from the chain head first, then live slab slots **high index to low**
//! (still skipping tombstoned slots). Log-backed rows prefetch the log chain at iterator
//! construction so slab scans can skip slots masked by core-LARA overflow-log delete entries without
//! decoding them. Labeled compact rows normally avoid this log-backed path by rewriting rows into
//! slab tombstones before deletion. The descending scan is the preferred hot path (cache- and
//! prefetch-friendly, newest log entries first). Callers that need slot order should use
//! `asc_out_edges` instead, or reverse the vector produced by `out_edges_iter` when
//! packing rows contiguously (e.g. segment rebalance snapshots).
//! The log, counts, span metadata, and free span index are update-side structures.
//! They may be read while inserting, folding logs, resizing, or relocating, but they
//! are not part of the clean scan contract.
//!
//! Insertions first try to append at `base_slot_start + stored_degree`, or reuse
//! the first tail tombstone at `base_slot_start + degree` when `stored_degree > degree`.
//! Appending is allowed only when it stays before this row’s CSR slab boundary (the next
//! vertex's `base_slot_start`, PMA leaf total, or `elem_capacity`);
//! otherwise the edge is written to the segment log and later folded by
//! maintenance or relocation.
//!
//! ## Layout assumptions (update paths)
//!
//! Slab span geometry uses the successor vertex row’s `base_slot_start` inside a
//! PMA leaf, plus (when slabs are monotone across leaves) caps from later
//! leaves. A materialized segment also clamps the slab window using
//! `span_meta.physical_start + counts.total`. When monotone ordering breaks due
//! to local relocation packing a leaf into earlier slab slots, successors with
//! lower bases are ignored and PMA span metadata determines the slab tail instead.
//! If that invariant is violated, behavior is undefined; **debug builds** assert it on
//! the hot paths below. Prefer [`crate::LaraGraph`] orchestration over ad-hoc
//! [`EdgeStore`] mutation so geometry and PMA counts stay aligned.
//!
//! ## Vertex tombstones and read paths
//!
//! When [`crate::traits::CsrVertexTombstoneScan::record_is_vertex_tombstone`]
//! is true, mutating APIs still reject the row. Read-only enumeration
//! (`out_edges_iter`, `asc_out_edges`) treats **tombstone + zero
//! degree + no log** (`log_head < 0`) as fully evacuated and returns an empty
//! neighborhood; otherwise enumeration proceeds so incremental `DeleteVertex`
//! maintenance and leaf rebalance can snapshot pending slab/log material until
//! rows clear.

#[cfg(feature = "canbench")]
mod bench;
pub mod counts;
mod edges;
pub mod free_span;
mod log;
pub mod span_meta;

use super::operation_error::{LaraOperationError, VertexAccess};
use crate::{
    GrowFailed, SegmentId, VertexCount, VertexId,
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex, CsrVertexTombstoneScan},
};
use counts::{SegmentEdgeCounts, SegmentEdgeCountsStore};
pub(crate) use edges::EdgeSlabStore;
use edges::tree_height_for_segment_count;
pub use edges::{HeaderV1 as EdgeHeaderV1, InitError as SlabInitError, segment_tree_leaf_count};
use free_span::{FreeSpan, FreeSpanStore};
use ic_stable_structures::Memory;
use log::LogStore;
pub use log::{DEFAULT_MAX_LOG_ENTRIES, HeaderV1 as LogHeaderV1};
use span_meta::{SPAN_PHYSICAL_UNASSIGNED, SegmentSpanMeta, SegmentSpanMetaStore};
use std::{cell::Cell, fmt, iter::FusedIterator, num::NonZero};

const INLINE_EDGE_BYTES: usize = 64;
/// When a clean slab row is at least this many bytes, [`OutEdgesIter`] and [`OutEdgeSlabIter`]
/// read the slab in fixed-size **descending** slot chunks instead of one stable read per edge.
const OUT_EDGE_SLAB_PREFETCH_MIN_BYTES: usize = 64;
/// Number of consecutive slab slots loaded per chunk when prefetch chunking is enabled.
const OUT_EDGE_SLAB_CHUNK_SLOTS: u32 = 32;

/// Applies `offset` / `limit` to a logical stream of outgoing edges (after raw / match filters).
pub(crate) struct OutEdgeVisitWindow {
    skip: usize,
    take: Option<usize>,
}

impl OutEdgeVisitWindow {
    pub(crate) fn new(offset: Option<usize>, limit: Option<usize>) -> Self {
        Self {
            skip: offset.unwrap_or(0),
            take: limit,
        }
    }

    /// Visit `edge` if it falls inside the window. Returns `false` when the caller should stop
    /// traversing (limit reached).
    pub(crate) fn emit_edge<E, V>(&mut self, edge: E, visit: &mut V) -> bool
    where
        V: FnMut(E),
    {
        if self.skip > 0 {
            self.skip -= 1;
            return true;
        }
        if let Some(0) = self.take {
            return false;
        }
        visit(edge);
        if let Some(t) = self.take.as_mut() {
            *t -= 1;
            if *t == 0 {
                return false;
            }
        }
        true
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeleteTarget {
    Slab(u32),
    Log(u32),
}

fn encode_delete_target(target: DeleteTarget) -> Result<i32, LaraOperationError> {
    let tag = match target {
        DeleteTarget::Slab(offset) => offset
            .checked_mul(2)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?,
        DeleteTarget::Log(index) => index
            .checked_mul(2)
            .and_then(|n| n.checked_add(1))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?,
    };
    let encoded = -1i64 - i64::from(tag);
    i32::try_from(encoded).map_err(|_| LaraOperationError::CollectAllocationOverflow)
}

fn decode_delete_target(src: i32) -> Option<DeleteTarget> {
    if src >= 0 {
        return None;
    }
    let tag = (-1i64 - i64::from(src)) as u32;
    if tag % 2 == 0 {
        Some(DeleteTarget::Slab(tag / 2))
    } else {
        Some(DeleteTarget::Log(tag / 2))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertLocation {
    Slab(u32),
    Log,
}

#[derive(Clone, Copy, Debug)]
pub(crate) struct EdgeLayout {
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
    num_edges: u64,
    initial_vertex_edge_slots: u32,
}

impl From<EdgeHeaderV1> for EdgeLayout {
    fn from(header: EdgeHeaderV1) -> Self {
        Self {
            elem_capacity: header.elem_capacity,
            segment_count: header.segment_count,
            segment_size: header.segment_size,
            num_edges: header.num_edges,
            initial_vertex_edge_slots: header.initial_vertex_edge_slots,
        }
    }
}

/// Errors returned when reopening the full edge storage subsystem.
#[derive(Debug)]
pub enum InitError {
    /// The edge subsystem could not allocate its initial metadata.
    OutOfMemory,
    /// The PMA count tree could not be reopened.
    Counts(counts::InitError),
    /// The edge slab could not be reopened.
    Edges(edges::InitError),
    /// The overflow log could not be reopened.
    Log(log::InitError),
    /// Segment span metadata could not be reopened.
    SpanMeta(span_meta::InitError),
    /// The overflow log was created for a different edge layout.
    LogLayoutMismatch,
    /// Segment span metadata length does not match the edge layout.
    SpanMetaLayoutMismatch,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::OutOfMemory => write!(f, "failed to allocate edge subsystem metadata"),
            Self::Counts(e) => write!(f, "counts init failed: {e}"),
            Self::Edges(e) => write!(f, "edge slab init failed: {e}"),
            Self::Log(e) => write!(f, "log init failed: {e}"),
            Self::SpanMeta(e) => write!(f, "segment span metadata init failed: {e}"),
            Self::LogLayoutMismatch => write!(f, "log layout does not match edge store layout"),
            Self::SpanMetaLayoutMismatch => {
                write!(f, "segment span metadata length does not match edge layout")
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Combined stable edge storage used by [`LaraGraph`](crate::LaraGraph).
pub struct EdgeStore<E: CsrEdge, M: Memory> {
    counts: SegmentEdgeCountsStore<E, M>,
    edges: EdgeSlabStore<E, M>,
    header: Cell<EdgeHeaderV1>,
    log: LogStore<E, M>,
    span_meta: SegmentSpanMetaStore<M>,
    free_spans: FreeSpanStore<M>,
}

/// Descending scan for a **log-backed** row without prefetching every live log edge into a `Vec`.
///
/// Same logical order as [`OutEdgesIter`]: scan the core-LARA overflow log chain (newest first),
/// then walk the slab prefix in descending slot order, skipping slab slots targeted by overflow-log
/// delete entries.
pub(crate) struct LogBackedDescIter<'a, E: CsrEdge, M: Memory> {
    store: &'a EdgeStore<E, M>,
    leaf: u32,
    next_log: i32,
    remaining_log: u32,
    base_slot_start: u64,
    remaining_slab: u32,
    yield_remaining: u32,
    log_header: LogHeaderV1,
    log_table: Option<Vec<u8>>,
    slab_chunk: Option<OutEdgeSlabChunk>,
    deleted_log_indices: Vec<u32>,
    deleted_slab_offsets: Vec<u32>,
    sorted_slab_deletes: bool,
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

impl<E: CsrEdge, M: Memory> EdgeStore<E, M> {
    /// Exclusive slab slot boundary for vertex ordinal `v_ord`.
    ///
    /// Within one PMA leaf, the successor vertex row defines the CSR prefix end.
    /// When the next [`VertexId`] lives in another leaf, its `base_slot_start`
    /// still caps the slab window only if it is **monotone** (`>=` this row's
    /// base); otherwise local relocation may have packed a later leaf below the
    /// previous one and the slab tail must come from PMA span metadata.
    ///
    /// When [`SegmentSpanMeta::physical_start`] is set, PMA tail boundaries from
    /// counts apply both within a leaf (clipping the CSR stripe to the relocated
    /// physical span) and across leaves. Without materialized span rows, PMA width
    /// from counts is anchored at this leaf's first vertex ordinal (`head +
    /// total`) and is consulted only once a vertex row has no same-leaf CSR
    /// successor (cross-leaf or sparse tail)—not between adjacent vertices in one
    /// leaf, since that count may reflect slab-wide bookkeeping rather than a
    /// per-neighbor stripe.
    ///
    /// When [`EdgeLayout::initial_vertex_edge_slots`] is non-empty and ids remain
    /// empty past `v_ord` inside the logical leaf range, the implicit stripe width
    /// still follows `initial_vertex_edge_slots`.
    #[inline]
    fn max_slab_window_for_vertex<V: CsrVertex>(v: &V, base: u64, end: u64) -> u64 {
        v.slab_append_exclusive_end(base)
            .map(|bypass_end| end.max(bypass_end))
            .unwrap_or(end)
    }

    pub(crate) fn slab_window_exclusive_end<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        v_ord: u32,
        v: &V,
    ) -> u64
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let len = vertices.len();
        let base = v.base_slot_start();
        let seg = edge_layout.segment_size.max(1);
        let leaf = v_ord / seg;
        let leaf_start = leaf.saturating_mul(seg);
        let leaf_logical_end_exclusive = leaf_start.saturating_add(seg);
        let occupied_leaf_end_exclusive = leaf_logical_end_exclusive.min(len);

        // Hot path for inserts: successive vertex ids inside one PMA leaf. Only touch
        // span metadata (+ counts when the leaf is PMA-pinned) after the CSR neighbor read.
        if v_ord.saturating_add(1) < occupied_leaf_end_exclusive {
            let next_base = vertices
                .get(VertexId::from(v_ord.saturating_add(1)))
                .base_slot_start();
            debug_assert!(
                next_base >= base,
                "LARA CSR invariant: base_slot_start must be non-decreasing in VertexId order"
            );
            let span_rec = self.span_meta_store().get(u64::from(leaf));
            if span_rec.physical_start == SPAN_PHYSICAL_UNASSIGNED {
                return Self::max_slab_window_for_vertex(v, base, next_base);
            }
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            let cap = span_rec
                .physical_start
                .saturating_add(c.total.max(0) as u64);
            return Self::max_slab_window_for_vertex(v, base, next_base.min(cap));
        }

        let w = edge_layout.initial_vertex_edge_slots;
        if w > 0 && v_ord.saturating_add(1) < leaf_logical_end_exclusive {
            let tail = base.saturating_add(u64::from(w));
            let span_rec = self.span_meta_store().get(u64::from(leaf));
            if span_rec.physical_start == SPAN_PHYSICAL_UNASSIGNED {
                return Self::max_slab_window_for_vertex(v, base, tail);
            }
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            let cap = span_rec
                .physical_start
                .saturating_add(c.total.max(0) as u64);
            return Self::max_slab_window_for_vertex(v, base, tail.min(cap));
        }

        if v_ord.saturating_add(1) < len {
            let next_base = vertices
                .get(VertexId::from(v_ord.saturating_add(1)))
                .base_slot_start();
            if next_base >= base {
                let span_rec = self.span_meta_store().get(u64::from(leaf));
                if span_rec.physical_start != SPAN_PHYSICAL_UNASSIGNED {
                    let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
                    let cap = span_rec
                        .physical_start
                        .saturating_add(c.total.max(0) as u64);
                    return Self::max_slab_window_for_vertex(v, base, next_base.min(cap));
                }
                if leaf < edge_layout.segment_count {
                    let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
                    let total_u = c.total.max(0) as u64;
                    if total_u > 0 {
                        let head = vertices.get(VertexId::from(leaf_start)).base_slot_start();
                        let cap = head.saturating_add(total_u);
                        return Self::max_slab_window_for_vertex(v, base, next_base.min(cap));
                    }
                }
                return Self::max_slab_window_for_vertex(v, base, next_base);
            }
        }

        let span_rec = self.span_meta_store().get(u64::from(leaf));
        if span_rec.physical_start != SPAN_PHYSICAL_UNASSIGNED {
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            let end = span_rec
                .physical_start
                .saturating_add(c.total.max(0) as u64);
            return Self::max_slab_window_for_vertex(v, base, end);
        }

        let end = if leaf < edge_layout.segment_count {
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            base.saturating_add(c.total.max(0) as u64)
        } else {
            edge_layout.elem_capacity
        };
        Self::max_slab_window_for_vertex(v, base, end)
    }

    /// Creates a fresh edge subsystem over the supplied stable memories.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
        elem_capacity: u64,
        segment_size: u32,
        initial_vertex_edge_slots: u32,
    ) -> Result<Self, GrowFailed> {
        crate::slab_index::validate_elem_capacity_grow_failed(elem_capacity, edges.size())?;
        let segment_count = segment_tree_leaf_count(VertexCount::default(), segment_size);
        let header = EdgeHeaderV1::new(
            elem_capacity,
            segment_count,
            segment_size,
            E::BYTES as u32,
            initial_vertex_edge_slots,
        );
        let counts = SegmentEdgeCountsStore::new(counts)?;
        for _ in 0..u64::from(header.segment_count).saturating_mul(2) {
            counts.push(SegmentEdgeCounts {
                actual: 0,
                total: 0,
            })?;
        }
        let log_header = LogHeaderV1::new(header.segment_count, header.stride);
        let span_meta = SegmentSpanMetaStore::new(span_meta)?;
        for _ in 0..u64::from(header.segment_count) {
            span_meta.push(SegmentSpanMeta::default())?;
        }
        let edges = EdgeSlabStore::new(edges, header)?;
        let log = LogStore::new(log, log_header)?;
        let free_spans =
            FreeSpanStore::new(free_spans, free_span_by_start).map_err(|_| GrowFailed {
                current_size: 0,
                delta: 0,
            })?;
        Ok(Self {
            counts,
            edges,
            header: Cell::new(header),
            log,
            span_meta,
            free_spans,
        })
    }

    /// Opens an edge subsystem from stable memories, creating it when the edge slab is empty.
    ///
    /// When subgraph memories already exist, `elem_capacity` is only used for the empty-slab
    /// creation path. On reopen the persisted [`EdgeHeaderV1::elem_capacity`] is authoritative
    /// (validated in [`EdgeSlabStore::init`]); pass the same value you used at first open if
    /// the slab has not been grown since.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
        elem_capacity: u64,
        segment_size: u32,
        initial_vertex_edge_slots: u32,
    ) -> Result<Self, InitError> {
        if edges.size() == 0 {
            return Self::new(
                counts,
                edges,
                log,
                span_meta,
                free_spans,
                free_span_by_start,
                elem_capacity,
                segment_size,
                initial_vertex_edge_slots,
            )
            .map_err(|_| InitError::OutOfMemory);
        }
        let counts = SegmentEdgeCountsStore::init(counts).map_err(InitError::Counts)?;
        let edges = EdgeSlabStore::init(edges).map_err(InitError::Edges)?;
        let header = edges.header().map_err(InitError::Edges)?;
        let _ = elem_capacity;
        let log = LogStore::init(log).map_err(InitError::Log)?;
        let span_meta = SegmentSpanMetaStore::init(span_meta).map_err(InitError::SpanMeta)?;
        let free_spans = FreeSpanStore::init(free_spans, free_span_by_start)
            .map_err(|_| InitError::SpanMetaLayoutMismatch)?;
        let log_header = log.header();
        if log_header.segment_count != header.segment_count {
            return Err(InitError::LogLayoutMismatch);
        }
        if span_meta.len() != u64::from(header.segment_count) {
            return Err(InitError::SpanMetaLayoutMismatch);
        }
        if counts.len() != u64::from(header.segment_count).saturating_mul(2) {
            return Err(InitError::SpanMetaLayoutMismatch);
        }
        Ok(Self {
            counts,
            edges,
            header: Cell::new(header),
            log,
            span_meta,
            free_spans,
        })
    }

    /// Grows the PMA/log/span metadata to `new_segment_count` (power-of-two leaves, ≥ current).
    pub(crate) fn grow_segment_tree_to(&self, new_segment_count: u32) -> Result<(), GrowFailed> {
        let h = self.header();
        let old = h.segment_count;
        if new_segment_count <= old {
            return Ok(());
        }
        self.migrate_counts_for_segment_grow(old, new_segment_count)?;
        for _ in old..new_segment_count {
            self.span_meta.push(SegmentSpanMeta::default())?;
        }
        self.log.grow_segment_count_to(new_segment_count)?;
        let mut nh = h;
        nh.segment_count = new_segment_count;
        nh.tree_height = tree_height_for_segment_count(new_segment_count);
        self.write_header(&nh);
        Ok(())
    }

    fn migrate_counts_for_segment_grow(&self, old_l: u32, new_l: u32) -> Result<(), GrowFailed> {
        let mut leaf_vals: Vec<SegmentEdgeCounts> = Vec::with_capacity(old_l as usize);
        for leaf in 0..old_l {
            let idx = u64::from(old_l + leaf);
            leaf_vals.push(self.counts.get(idx));
        }
        let target_len = u64::from(new_l).saturating_mul(2);
        while self.counts.len() < target_len {
            self.counts.push(SegmentEdgeCounts {
                actual: 0,
                total: 0,
            })?;
        }
        for leaf in 0..old_l {
            self.counts
                .set(u64::from(new_l + leaf), &leaf_vals[leaf as usize]);
        }
        for leaf in old_l..new_l {
            self.counts.set(
                u64::from(new_l + leaf),
                &SegmentEdgeCounts {
                    actual: 0,
                    total: 0,
                },
            );
        }
        for idx in (1..new_l).rev() {
            let left = self.counts.get(u64::from(idx * 2));
            let right = self.counts.get(u64::from(idx * 2 + 1));
            self.counts.set(
                u64::from(idx),
                &SegmentEdgeCounts {
                    actual: left.actual + right.actual,
                    total: left.total + right.total,
                },
            );
        }
        self.counts.set(
            0,
            &SegmentEdgeCounts {
                actual: 0,
                total: 0,
            },
        );
        Ok(())
    }

    /// Returns the current edge slab header.
    pub fn header(&self) -> EdgeHeaderV1 {
        self.header.get()
    }

    fn write_header(&self, header: &EdgeHeaderV1) {
        self.edges.write_header(header);
        self.header.set(*header);
    }

    /// Returns the PMA segment-count store.
    pub fn counts_store(&self) -> &SegmentEdgeCountsStore<E, M> {
        &self.counts
    }

    /// Returns the segment physical-span metadata store.
    pub fn span_meta_store(&self) -> &SegmentSpanMetaStore<M> {
        &self.span_meta
    }

    /// Returns the free-span manager.
    pub fn free_span_store(&self) -> &FreeSpanStore<M> {
        &self.free_spans
    }

    pub(crate) fn set_segment_physical_start(
        &self,
        segment: SegmentId,
        physical_start: u64,
    ) -> Result<(), GrowFailed> {
        let idx = u64::from(segment);
        if idx < self.span_meta.len() {
            self.span_meta.set(idx, &SegmentSpanMeta { physical_start });
        } else {
            while self.span_meta.len() < idx {
                self.span_meta.push(SegmentSpanMeta::default())?;
            }
            self.span_meta.push(SegmentSpanMeta { physical_start })?;
        }
        Ok(())
    }

    fn edge_layout(&self) -> EdgeLayout {
        self.header().into()
    }

    /// Consumes the edge subsystem and returns its stable memories in constructor order.
    pub fn into_memories(self) -> (M, M, M, M, M, M) {
        let (free_spans, free_span_by_start) = self.free_spans.into_memories();
        (
            self.counts.into_memory(),
            self.edges.into_memory(),
            self.log.into_memory(),
            self.span_meta.into_memory(),
            free_spans,
            free_span_by_start,
        )
    }

    fn spans_overlap(a_start: u64, a_len: u64, b_start: u64, b_len: u64) -> bool {
        let a_end = a_start.saturating_add(a_len);
        let b_end = b_start.saturating_add(b_len);
        a_start < b_end && b_start < a_end
    }

    pub(crate) fn allocate_span(&self, len: u64) -> Result<u64, GrowFailed> {
        self.allocate_span_avoiding(len, None)
    }

    /// Allocates `len` contiguous slots, optionally refusing spans that overlap `avoid`.
    pub(crate) fn allocate_span_avoiding(
        &self,
        len: u64,
        avoid: Option<(u64, u64)>,
    ) -> Result<u64, GrowFailed> {
        let cap = self.header().elem_capacity;
        if len == 0 {
            return Ok(cap);
        }
        let map_err = |_| GrowFailed {
            current_size: 0,
            delta: 0,
        };
        if let Some(span) = self.free_spans.take_best_fit(len).map_err(map_err)? {
            crate::slab_index::checked_add_slot_exclusive_end(span.start_slot, len).ok_or(
                GrowFailed {
                    current_size: 0,
                    delta: 0,
                },
            )?;
            if let Some((avoid_start, avoid_len)) = avoid {
                if Self::spans_overlap(span.start_slot, len, avoid_start, avoid_len) {
                    self.free_spans
                        .release(FreeSpan {
                            start_slot: span.start_slot,
                            len,
                        })
                        .map_err(map_err)?;
                } else {
                    return Ok(span.start_slot);
                }
            } else {
                return Ok(span.start_slot);
            }
        }

        let start = cap;
        let new_cap =
            crate::slab_index::checked_add_slot_exclusive_end(start, len).ok_or(GrowFailed {
                current_size: 0,
                delta: 0,
            })?;
        self.set_elem_capacity(new_cap)?;
        Ok(start)
    }

    pub(crate) fn release_span(&self, start_slot: u64, len: u64) -> Result<(), GrowFailed> {
        if len > 0 {
            self.free_spans
                .release(FreeSpan { start_slot, len })
                .map_err(|_| GrowFailed {
                    current_size: 0,
                    delta: 0,
                })?;
        }
        Ok(())
    }

    /// Decodes and returns the edge record stored at `slot`.
    pub fn read_slot(&self, slot: u64) -> E {
        if E::BYTES <= 8 {
            let mut buf = [0u8; 8];
            self.edges.read_slot(slot, &mut buf[..E::BYTES]);
            E::read_from(&buf[..E::BYTES])
        } else if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            self.edges.read_slot(slot, &mut buf[..E::BYTES]);
            E::read_from(&buf[..E::BYTES])
        } else {
            let mut buf = vec![0u8; E::BYTES];
            self.edges.read_slot(slot, &mut buf);
            E::read_from(&buf)
        }
    }

    /// Reads contiguous edge-slot bytes starting at `start_slot` into `out`.
    ///
    /// `out.len()` must be a multiple of `E::BYTES`.
    pub(crate) fn read_slots_contiguous(&self, start_slot: u64, out: &mut [u8]) {
        self.edges.read_slots_contiguous(start_slot, out);
    }

    /// Writes contiguous edge-slot bytes starting at `start_slot`.
    ///
    /// `bytes.len()` must be a multiple of `E::BYTES`.
    pub(crate) fn write_slots_contiguous(
        &self,
        start_slot: u64,
        bytes: &[u8],
    ) -> Result<(), GrowFailed> {
        self.edges.write_slots_contiguous(start_slot, bytes)
    }

    /// Encodes and writes `edge` to `slot`.
    pub fn write_slot(&self, slot: u64, edge: E) -> Result<(), GrowFailed> {
        if E::BYTES <= 8 {
            let mut buf = [0u8; 8];
            edge.write_to(&mut buf[..E::BYTES]);
            self.edges.write_slot(slot, &buf[..E::BYTES])
        } else if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut buf[..E::BYTES]);
            self.edges.write_slot(slot, &buf[..E::BYTES])
        } else {
            let mut buf = vec![0u8; E::BYTES];
            edge.write_to(&mut buf);
            self.edges.write_slot(slot, &buf)
        }
    }

    fn collect_out_edge_refs_slot_order<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<Vec<(DeleteTarget, E)>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        let v_ord = u32::from(vid);
        let log_owner = vertices.log_leaf_vertex(vid);
        // Tombstone rows may still hold slab/log material while incremental
        // `DeleteVertex` maintenance runs; only fully evacuated rows reject reads.
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(Vec::new());
        }
        if v.log_head() < 0 {
            let stored = v.stored_degree() as usize;
            let live = v.degree() as usize;
            let base = v.base_slot_start();
            if live == 0 {
                return Ok(Vec::new());
            }
            let nbytes = stored
                .checked_mul(E::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut raw = vec![0u8; nbytes];
            self.edges.read_slots_contiguous(base, &mut raw);
            let mut out = Vec::with_capacity(live);
            for (offset, chunk) in raw.chunks_exact(E::BYTES).enumerate() {
                let edge = E::read_from(chunk);
                if edge.is_deleted_slot() {
                    continue;
                }
                out.push((DeleteTarget::Slab(offset as u32), edge));
            }
            debug_assert_eq!(
                out.len(),
                live,
                "slab row must have exactly `degree` live edges among stored slots"
            );
            return Ok(out);
        }

        let edge_layout = self.edge_layout();
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, v_ord, &v)?;
        let slab_count = on_slab.min(v.stored_degree()) as usize;
        let mut out = Vec::with_capacity(v.degree() as usize);
        for i in 0..slab_count {
            let slot = v
                .base_slot_start()
                .checked_add(i as u64)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.read_slot(slot);
            if !edge.is_deleted_slot() {
                out.push((DeleteTarget::Slab(i as u32), edge));
            }
        }
        if v.log_head() < 0 {
            return Ok(out);
        }

        let leaf = leaf_segment(log_owner, edge_layout.segment_size);
        let log_h = self.log.header();

        let mut log_table_buf = Vec::new();
        self.log
            .read_segment_entry_table_into(&log_h, leaf, &mut log_table_buf);
        let log_table = (!log_table_buf.is_empty()).then_some(log_table_buf.as_slice());

        let mut entries = Vec::new();
        let mut log_i = v.log_head();
        let mut steps = 0u32;
        while log_i >= 0 {
            if steps >= log_h.max_log_entries {
                return Err(LaraOperationError::LogChainShort);
            }
            let (prev, src, edge) =
                self.read_log_edge_from_table_or_store(&log_h, leaf, log_i as u32, log_table);
            entries.push((log_i as u32, src, edge));
            log_i = prev;
            steps = steps
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        entries.reverse();

        for (log_idx, src, edge) in entries {
            if let Some(target) = decode_delete_target(src) {
                if let Some(index) = out.iter().position(|(candidate, _)| *candidate == target) {
                    out.remove(index);
                }
            } else {
                out.push((DeleteTarget::Log(log_idx), edge));
            }
        }
        debug_assert_eq!(
            out.len(),
            v.degree() as usize,
            "logical log replay must yield exactly `degree` live edges"
        );
        if out.len() != v.degree() as usize {
            // The log chain may be truncated/corrupt; preserve the old error shape rather than
            // silently returning a count that violates the vertex row.
            return Err(LaraOperationError::LogChainShort);
        }
        Ok(out)
    }

    pub(crate) fn asc_out_edges<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<Vec<E>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        Ok(self
            .collect_out_edge_refs_slot_order(vertices, vid)?
            .into_iter()
            .map(|(_, edge)| edge)
            .collect())
    }

    /// Walks outgoing edges without materializing the full row vector.
    ///
    /// Invokes `visit` for each edge that satisfies `matches` (same contract as
    /// [`LaraGraph::remove_edge_matching`](super::LaraGraph::remove_edge_matching)).
    ///
    /// `offset` / `limit` apply to the stream of edges **accepted** after filters.
    ///
    /// On slab-only rows (`log_head < 0`), when `raw_matches` is `Some`, it is consulted on each
    /// encoded record **before** decoding; a `false` result skips the slot with no
    /// [`CsrEdge::read_from`]. Edges that pass the raw filter are still gated by `matches` after
    /// decode.
    /// Enumeration uses the same **descending** slab walk as
    /// [`Self::out_edges_iter`] (including chunked slab reads when enabled), not a full-row
    /// materialization. Log-backed rows use `matches` only.
    pub(crate) fn visit_out_edges<V, A, Match, Visit>(
        &self,
        vertices: &A,
        vid: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        mut raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        mut matches: Match,
        mut visit: Visit,
    ) -> Result<(), LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        let mut window = OutEdgeVisitWindow::new(offset, limit);
        let v = vertices.get_in_range(vid)?;
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(());
        }
        if v.log_head() < 0 {
            let mut it =
                OutEdgeSlabIter::try_new(self, v.base_slot_start(), v.stored_degree(), v.degree())?;
            let has_raw = raw_matches.is_some();
            while let Some(edge) = it.next_live_edge_filtered(&mut raw_matches) {
                if has_raw {
                    if matches(&edge) && !window.emit_edge(edge, &mut visit) {
                        return Ok(());
                    }
                } else if matches(&edge) && !window.emit_edge(edge, &mut visit) {
                    return Ok(());
                }
            }
            return Ok(());
        }

        let mut walk = self.log_backed_desc_edges_iter(vertices, vid)?;
        while let Some(edge) = walk.next() {
            if matches(&edge) && !window.emit_edge(edge, &mut visit) {
                return Ok(());
            }
        }
        Ok(())
    }
    ///
    /// For purely slab-backed rows (`log_head < 0`), this uses the same descending slab scan as
    /// [`Self::out_edges_iter`], including optional raw-byte prefilter before decode (no full-row
    /// allocation).
    pub(crate) fn find_first_out_edge_matching<V, A, Match>(
        &self,
        vertices: &A,
        vid: VertexId,
        mut raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: &mut Match,
    ) -> Result<Option<E>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
        Match: FnMut(&E) -> bool,
    {
        let v = vertices.get_in_range(vid)?;
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(None);
        }
        if v.log_head() < 0 {
            let mut it =
                OutEdgeSlabIter::try_new(self, v.base_slot_start(), v.stored_degree(), v.degree())?;
            while let Some(edge) = it.next_live_edge_filtered(&mut raw_matches) {
                if matches(&edge) {
                    return Ok(Some(edge));
                }
            }
            return Ok(None);
        }

        let mut walk = self.log_backed_desc_edges_iter(vertices, vid)?;
        while let Some(edge) = walk.next() {
            if matches(&edge) {
                return Ok(Some(edge));
            }
        }
        Ok(None)
    }
    ///
    /// For in-range vertices this is exactly [`CsrVertex::degree`] `> 0`: a zero-degree row has no
    /// material in the slab or overflow log that clean enumeration would surface (including fully
    /// evacuated tombstone rows).
    pub(crate) fn has_out_edges<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<bool, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        Ok(v.degree() > 0)
    }

    /// Walks the overflow log once (newest→oldest along `prev`) and returns live log edges in that
    /// visitation order plus slab slot indices targeted by log delete records.
    ///
    /// Matches the historical lazy [`OutEdgesIter`] log phase: delete entries update
    /// `deleted_log_indices` / `deleted_slab_offsets` before inserts at the same log index are
    /// skipped.
    fn prefetch_descending_log_edges(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_head: i32,
    ) -> Result<(Vec<E>, Vec<u32>), LaraOperationError> {
        let mut log_table_buf = Vec::new();
        self.log
            .read_segment_entry_table_into(log_h, leaf, &mut log_table_buf);
        let log_table = (!log_table_buf.is_empty()).then_some(log_table_buf.as_slice());

        let mut deleted_log_indices: Vec<u32> = Vec::new();
        let mut deleted_slab_offsets: Vec<u32> = Vec::new();
        let mut log_edges: Vec<E> = Vec::new();
        let mut log_i = log_head;
        let mut budget = log_h.max_log_entries;
        while budget > 0 {
            budget -= 1;
            if log_i < 0 {
                return Ok((log_edges, deleted_slab_offsets));
            }
            let log_idx = log_i as u32;
            let (prev, src, edge) =
                self.read_log_edge_from_table_or_store(log_h, leaf, log_idx, log_table);
            log_i = prev;
            if let Some(target) = decode_delete_target(src) {
                match target {
                    DeleteTarget::Slab(offset) => deleted_slab_offsets.push(offset),
                    DeleteTarget::Log(index) => deleted_log_indices.push(index),
                }
                continue;
            }
            if let Some(pos) = deleted_log_indices.iter().position(|&d| d == log_idx) {
                deleted_log_indices.swap_remove(pos);
                continue;
            }
            log_edges.push(edge);
        }
        if log_i >= 0 {
            return Err(LaraOperationError::LogChainShort);
        }
        Ok((log_edges, deleted_slab_offsets))
    }

    /// Descending scan over a **log-backed** row without prefetching live log edges into a buffer.
    ///
    /// Used by [`Self::visit_out_edges`] and [`Self::find_first_out_edge_matching`] so full
    /// traversals do not allocate a `Vec` of every live log edge. Public iteration
    /// ([`Self::out_edges_iter`]) still prefetches for cheaper [`Iterator::advance_by`] / `skip`.
    pub(crate) fn log_backed_desc_edges_iter<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<LogBackedDescIter<'_, E, M>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        let v_ord = u32::from(vid);
        let log_owner = vertices.log_leaf_vertex(vid);
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, v_ord, &v)?;
        let stored = v.stored_degree();
        let slab_count = on_slab.min(stored);
        let nbytes_slab = (slab_count as usize)
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let slab_chunk = if nbytes_slab >= OUT_EDGE_SLAB_PREFETCH_MIN_BYTES {
            Some(OutEdgeSlabChunk {
                buf: Vec::new(),
                chunk_low: 0,
                chunk_high: 0,
            })
        } else {
            None
        };
        let log_header = self.log.header();
        let leaf = leaf_segment(log_owner, edge_layout.segment_size);
        Ok(LogBackedDescIter {
            store: self,
            leaf,
            next_log: v.log_head(),
            remaining_log: log_header.max_log_entries,
            base_slot_start: v.base_slot_start(),
            remaining_slab: slab_count,
            yield_remaining: v.degree(),
            log_header,
            log_table: None,
            slab_chunk,
            deleted_log_indices: Vec::new(),
            deleted_slab_offsets: Vec::new(),
            sorted_slab_deletes: false,
        })
    }

    /// Default **descending** contiguous scan over one vertex row (see [`OutEdgesIter`]).
    pub(crate) fn out_edges_iter<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<OutEdgesIter<'_, E, M>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        // See `asc_out_edges`: allow enumeration for tombstones that
        // still have pending edge material (rebalance during vertex delete).
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(OutEdgesIter {
                store: self,
                base_slot_start: v.base_slot_start(),
                remaining_slab: 0,
                yield_remaining: 0,
                log_edges: Vec::new(),
                log_pos: 0,
                slab_chunk: None,
                deleted_slab_offsets: Vec::new(),
            });
        }
        // Clean rows: the full neighborhood is on the slab, so the iterator never
        // walks the overflow log. Skip `edge_layout()` (full slab header read) and
        // log metadata.
        if v.log_head() < 0 {
            let stored = v.stored_degree();
            let live = v.degree();
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
            return Ok(OutEdgesIter {
                store: self,
                base_slot_start: v.base_slot_start(),
                remaining_slab: stored,
                yield_remaining: live,
                log_edges: Vec::new(),
                log_pos: 0,
                slab_chunk,
                deleted_slab_offsets: Vec::new(),
            });
        }

        let edge_layout = self.edge_layout();
        let v_ord = u32::from(vid);
        let log_owner = vertices.log_leaf_vertex(vid);
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, v_ord, &v)?;
        let stored = v.stored_degree();
        let slab_count = on_slab.min(stored);
        let nbytes_slab = (slab_count as usize)
            .checked_mul(E::BYTES)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let slab_chunk = if nbytes_slab >= OUT_EDGE_SLAB_PREFETCH_MIN_BYTES {
            Some(OutEdgeSlabChunk {
                buf: Vec::new(),
                chunk_low: 0,
                chunk_high: 0,
            })
        } else {
            None
        };

        let log_header = self.log.header();
        let leaf = leaf_segment(log_owner, edge_layout.segment_size);
        let (log_edges, mut deleted_slab_offsets) =
            self.prefetch_descending_log_edges(&log_header, leaf, v.log_head())?;
        deleted_slab_offsets.sort_unstable();
        Ok(OutEdgesIter {
            store: self,
            base_slot_start: v.base_slot_start(),
            remaining_slab: slab_count,
            yield_remaining: v.degree(),
            log_edges,
            log_pos: 0,
            slab_chunk,
            deleted_slab_offsets,
        })
    }

    /// Descending contiguous scan iterator (alias of [`Self::out_edges_iter`]).
    pub(crate) fn desc_out_edges_iter<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<OutEdgesIter<'_, E, M>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        self.out_edges_iter(vertices, vid)
    }

    /// Ascending CSR slot / materialization order (same sequence as [`Self::asc_out_edges`]).
    ///
    /// Slab scans use the same fixed-size chunk cache as descending scans, but read forward
    /// low→high. Log-backed rows replay the log once up front into small insertion/deletion caches,
    /// then stream slab slots followed by live inserted log edges.
    pub(crate) fn asc_out_edges_iter<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<AscOutEdgesIter<'_, E, M>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(AscOutEdgesIter::empty(self));
        }
        if v.log_head() < 0 {
            return Ok(AscOutEdgesIter::slab_only(
                self,
                v.base_slot_start(),
                v.stored_degree(),
                v.degree(),
            ));
        }

        let edge_layout = self.edge_layout();
        let v_ord = u32::from(vid);
        let log_owner = vertices.log_leaf_vertex(vid);
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, v_ord, &v)?;
        let slab_count = on_slab.min(v.stored_degree());
        let leaf = leaf_segment(log_owner, edge_layout.segment_size);
        let log_h = self.log.header();

        let mut log_table_buf = Vec::new();
        self.log
            .read_segment_entry_table_into(&log_h, leaf, &mut log_table_buf);
        let log_table = (!log_table_buf.is_empty()).then_some(log_table_buf.as_slice());

        let mut entries = Vec::new();
        let mut log_i = v.log_head();
        let mut steps = 0u32;
        while log_i >= 0 {
            if steps >= log_h.max_log_entries {
                return Err(LaraOperationError::LogChainShort);
            }
            let (prev, src, edge) =
                self.read_log_edge_from_table_or_store(&log_h, leaf, log_i as u32, log_table);
            entries.push((log_i as u32, src, edge));
            log_i = prev;
            steps = steps
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        entries.reverse();

        let mut inserted = Vec::new();
        let mut deleted_slab_offsets = Vec::new();
        for (log_idx, src, edge) in entries {
            if let Some(target) = decode_delete_target(src) {
                match target {
                    DeleteTarget::Slab(offset) => deleted_slab_offsets.push(offset),
                    DeleteTarget::Log(_) => {
                        if let Some(index) = inserted
                            .iter()
                            .position(|(candidate, _)| *candidate == target)
                        {
                            inserted.remove(index);
                        }
                    }
                }
            } else {
                inserted.push((DeleteTarget::Log(log_idx), edge));
            }
        }

        Ok(AscOutEdgesIter::with_log_replay(
            self,
            v.base_slot_start(),
            slab_count,
            v.degree(),
            deleted_slab_offsets,
            inserted.into_iter().map(|(_, edge)| edge).collect(),
        ))
    }

    fn read_log_edge_from_table_or_store(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_idx: u32,
        table: Option<&[u8]>,
    ) -> (i32, i32, E) {
        if let Some(buf) = table {
            let stride = log_h.stride as usize;
            if stride > 0 {
                let off = log_idx as usize * stride;
                if off + stride <= buf.len() && off + 8 + E::BYTES <= buf.len() {
                    let prev = i32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                    let src = i32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
                    let edge = E::read_from(&buf[off + 8..off + 8 + E::BYTES]);
                    return (prev, src, edge);
                }
            }
        }
        if E::BYTES <= 8 {
            let mut buf = [0u8; 8];
            let (prev, _src) =
                self.log
                    .read_entry_with_header(log_h, leaf, log_idx, &mut buf[..E::BYTES]);
            (prev, _src, E::read_from(&buf[..E::BYTES]))
        } else if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            let (prev, _src) =
                self.log
                    .read_entry_with_header(log_h, leaf, log_idx, &mut buf[..E::BYTES]);
            (prev, _src, E::read_from(&buf[..E::BYTES]))
        } else {
            let mut buf = vec![0u8; E::BYTES];
            let (prev, _src) = self
                .log
                .read_entry_with_header(log_h, leaf, log_idx, &mut buf);
            (prev, _src, E::read_from(&buf))
        }
    }

    pub(crate) fn insert_edge<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
        edge: E,
    ) -> Result<InsertLocation, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        let v_ord = u32::from(vid);
        if V::record_is_vertex_tombstone(&v) {
            return Err(LaraOperationError::VertexDeleted);
        }
        let log_owner = vertices.log_leaf_vertex(vid);

        let _next_degree = v
            .degree()
            .checked_add(1)
            .ok_or(LaraOperationError::RowDegreeOverflow)?;
        let next_num_edges = edge_layout
            .num_edges
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let loc = v
            .base_slot_start()
            .checked_add(u64::from(v.stored_degree()))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let location = if self.have_space_on_slab(vertices, v_ord, &v, loc, &edge_layout) {
            let write_end = loc
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            if write_end > self.header().elem_capacity {
                self.set_elem_capacity(write_end)
                    .map_err(LaraOperationError::ResizeFailed)?;
            }
            self.write_slot(loc, edge)
                .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
            let grown = v
                .try_grow_packed_slab_by_one()
                .map_err(|()| LaraOperationError::RowDegreeOverflow)?;
            vertices.set(vid, &grown);
            InsertLocation::Slab(v.stored_degree())
        } else {
            self.insert_into_log_with_layout(
                &edge_layout,
                vertices,
                vid,
                log_owner,
                v,
                _next_degree,
                edge,
            )?;
            InsertLocation::Log
        };
        self.set_num_edges(next_num_edges);
        self.bump_counts_leaf_with_layout(&edge_layout, log_owner, 1, 0)?;
        Ok(location)
    }

    pub(crate) fn remove_edge_slab_tombstone_matching<V, A, F>(
        &self,
        vertices: &A,
        vid: VertexId,
        mut matches: F,
    ) -> Result<Option<E>, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        let log_owner = vertices.log_leaf_vertex(vid);
        if v.log_head() >= 0 {
            let removed = self
                .collect_out_edge_refs_slot_order(vertices, vid)?
                .into_iter()
                .find(|(_, edge)| matches(edge));
            let Some((target, removed)) = removed else {
                return Ok(None);
            };
            self.insert_delete_into_log_with_layout(
                &edge_layout,
                vertices,
                vid,
                v,
                target,
                removed,
            )?;
            let next_global = edge_layout
                .num_edges
                .checked_sub(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            self.set_num_edges(next_global);
            self.bump_counts_leaf_with_layout(&edge_layout, log_owner, -1, 0)?;
            return Ok(Some(removed));
        }
        let live = v.degree();
        if live == 0 {
            return Ok(None);
        }

        let base = v.base_slot_start();
        let stored = v.stored_degree();
        let mut found_index: Option<u32> = None;
        for i in 0..stored {
            let slot = base
                .checked_add(u64::from(i))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.read_slot(slot);
            if edge.is_tombstone_edge() {
                continue;
            }
            if matches(&edge) {
                found_index = Some(i);
                break;
            }
        }
        let Some(local_index) = found_index else {
            return Ok(None);
        };

        let rm_slot = base
            .checked_add(u64::from(local_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let removed = self.read_slot(rm_slot);
        self.write_slot(rm_slot, E::tombstone_edge())
            .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
        vertices.set(vid, &v.after_slab_tombstone_delete());
        let next_global = edge_layout
            .num_edges
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.set_num_edges(next_global);
        self.bump_counts_leaf_with_layout(&edge_layout, log_owner, -1, 0)?;
        Ok(Some(removed))
    }

    fn insert_delete_into_log_with_layout<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        vid: VertexId,
        v: V,
        target: DeleteTarget,
        edge: E,
    ) -> Result<(), LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let leaf = leaf_segment(vertices.log_leaf_vertex(vid), edge_layout.segment_size);
        let log_h = self.log.header();
        let idx = self.log.read_idx_with_header(&log_h, leaf);
        if idx < 0 || idx >= log_h.max_log_entries as i32 {
            return Err(LaraOperationError::SegmentLogFull);
        }
        let src = encode_delete_target(target)?;
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut payload = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut payload[..E::BYTES]);
            self.log
                .write_entry_with_header(
                    &log_h,
                    leaf,
                    idx as u32,
                    v.log_head(),
                    src,
                    &payload[..E::BYTES],
                )
                .map_err(LaraOperationError::WriteLogFailed)?;
        } else {
            let mut payload = vec![0u8; E::BYTES];
            edge.write_to(&mut payload);
            self.log
                .write_entry_with_header(&log_h, leaf, idx as u32, v.log_head(), src, &payload)
                .map_err(LaraOperationError::WriteLogFailed)?;
        }
        self.log.write_idx_with_header(&log_h, leaf, idx + 1);
        vertices.set(vid, &v.with_log_head(idx).after_slab_tombstone_delete());
        Ok(())
    }

    pub(crate) fn row_edge_at_slab<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
        offset: u32,
    ) -> Result<Option<E>, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        if v.log_head() >= 0 {
            return Err(LaraOperationError::RowEdgeReadRequiresSlabOnlyRow);
        }
        if offset >= v.degree() {
            return Ok(None);
        }
        let mut seen = 0u32;
        for stored_offset in 0..v.stored_degree() {
            let slot = v
                .base_slot_start()
                .checked_add(u64::from(stored_offset))
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let edge = self.read_slot(slot);
            if edge.is_deleted_slot() {
                continue;
            }
            if seen == offset {
                return Ok(Some(edge));
            }
            seen = seen
                .checked_add(1)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        }
        Ok(None)
    }

    pub(crate) fn find_first_out_edge_slot_matching<V, A, F>(
        &self,
        vertices: &A,
        vid: VertexId,
        mut matches: F,
    ) -> Result<Option<(u32, E)>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
        F: FnMut(&E) -> bool,
    {
        let v = vertices.get_in_range(vid)?;
        if V::record_is_vertex_tombstone(&v) && v.stored_degree() == 0 && v.log_head() < 0 {
            return Ok(None);
        }
        if v.log_head() >= 0 {
            return Ok(None);
        }
        let mut it =
            OutEdgeSlabIter::try_new(self, v.base_slot_start(), v.stored_degree(), v.degree())?;
        while let Some((slot, edge)) = it.next_live_edge_with_slot() {
            if matches(&edge) {
                return Ok(Some((slot, edge)));
            }
        }
        Ok(None)
    }

    pub(crate) fn remove_edge_at_slab_slot<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
        slot_index: u32,
    ) -> Result<Option<E>, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
        E: CsrEdgeTombstone,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        if v.log_head() >= 0 || slot_index >= v.stored_degree() {
            return Ok(None);
        }
        let rm_slot = v
            .base_slot_start()
            .checked_add(u64::from(slot_index))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let removed = self.read_slot(rm_slot);
        if removed.is_deleted_slot() {
            return Ok(None);
        }
        self.write_slot(rm_slot, E::tombstone_edge())
            .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
        vertices.set(vid, &v.after_slab_tombstone_delete());
        let next_global = edge_layout
            .num_edges
            .checked_sub(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.set_num_edges(next_global);
        self.bump_counts_leaf_with_layout(&edge_layout, vertices.log_leaf_vertex(vid), -1, 0)?;
        Ok(Some(removed))
    }

    pub(crate) fn clear_row_slab<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<u32, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        let log_owner = vertices.log_leaf_vertex(vid);
        if v.log_head() >= 0 {
            return Err(LaraOperationError::ClearRowRequiresSlabOnlyRow);
        }
        let removed = v.degree();
        if removed == 0 {
            return Ok(0);
        }
        vertices.set(vid, &v.with_degree(0).with_log_head(-1));
        let next_global = edge_layout
            .num_edges
            .checked_sub(u64::from(removed))
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        self.set_num_edges(next_global);
        self.bump_counts_leaf_with_layout(&edge_layout, log_owner, -i64::from(removed), 0)?;
        Ok(removed)
    }

    fn insert_into_log_with_layout<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        vid: VertexId,
        log_owner: VertexId,
        v: V,
        next_degree: u32,
        edge: E,
    ) -> Result<(), LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let leaf = leaf_segment(log_owner, edge_layout.segment_size);
        let log_h = self.log.header();
        let idx = self.log.read_idx_with_header(&log_h, leaf);
        if idx < 0 || idx >= log_h.max_log_entries as i32 {
            return Err(LaraOperationError::SegmentLogFull);
        }
        let src = i32::try_from(u32::from(log_owner))
            .map_err(|_| LaraOperationError::VertexIdExceedsI32)?;
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut payload = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut payload[..E::BYTES]);
            self.log
                .write_entry_with_header(
                    &log_h,
                    leaf,
                    idx as u32,
                    v.log_head(),
                    src,
                    &payload[..E::BYTES],
                )
                .map_err(LaraOperationError::WriteLogFailed)?;
        } else {
            let mut payload = vec![0u8; E::BYTES];
            edge.write_to(&mut payload);
            self.log
                .write_entry_with_header(&log_h, leaf, idx as u32, v.log_head(), src, &payload)
                .map_err(LaraOperationError::WriteLogFailed)?;
        }
        self.log.write_idx_with_header(&log_h, leaf, idx + 1);
        let _ = next_degree;
        let grown = v
            .with_log_head(idx)
            .try_grow_packed_slab_by_one()
            .map_err(|()| LaraOperationError::RowDegreeOverflow)?;
        vertices.set(vid, &grown);
        Ok(())
    }

    fn on_slab_edges_with_layout<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        v_ord: u32,
        v: &V,
    ) -> Result<u32, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        if v.log_head() < 0 {
            return Ok(v.stored_degree());
        }
        let next_exclusive = self.slab_window_exclusive_end(edge_layout, vertices, v_ord, v);
        let span_slots = next_exclusive
            .checked_sub(v.base_slot_start())
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let span_u32 = span_slots.min(u64::from(u32::MAX)) as u32;
        // Once the overflow log is active, the slab prefix is at most the CSR window
        // width; additional live edges are chained through `log_head`.
        Ok(if v.stored_degree() > span_u32 {
            span_u32
        } else {
            v.stored_degree()
        })
    }

    fn have_space_on_slab<V, A>(
        &self,
        vertices: &A,
        v_ord: u32,
        v: &V,
        loc: u64,
        edge_layout: &EdgeLayout,
    ) -> bool
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        loc < self.slab_window_exclusive_end(edge_layout, vertices, v_ord, v)
    }

    /// Incremental update of the PMA leaf row for `vid` (and internal ancestors).
    ///
    /// Core inserts/removes typically adjust only [`SegmentEdgeCounts::actual`] (`d_total = 0`).
    /// Labeled vertex-edge-span growth/shrink may also adjust [`SegmentEdgeCounts::total`] when
    /// physical slab reservation changes.
    pub(crate) fn bump_vertex_segment_counts(
        &self,
        vid: VertexId,
        d_actual: i64,
        d_total: i64,
    ) -> Result<(), LaraOperationError> {
        let edge_layout = self.edge_layout();
        self.bump_counts_leaf_with_layout(&edge_layout, vid, d_actual, d_total)
    }

    fn bump_counts_leaf_with_layout(
        &self,
        edge_layout: &EdgeLayout,
        vid: VertexId,
        d_actual: i64,
        d_total: i64,
    ) -> Result<(), LaraOperationError> {
        let mut idx =
            (leaf_segment(vid, edge_layout.segment_size) + edge_layout.segment_count) as usize;
        if idx as u64 >= self.counts.len() {
            return Err(LaraOperationError::SegmentCountsTreeTooSmall);
        }
        // Inserts/removes only ever adjust `actual` (live edge records). `total` is owned by
        // explicit recount/rebalance paths (`LaraGraph::update_leaf_count_and_ancestors`).
        // Propagate the same delta up the tree with one read + write per level instead of
        // re-summing both children at every internal node (two reads + write per level).
        if d_total == 0 {
            loop {
                let mut c = self.counts.get(idx as u64);
                c.actual += d_actual;
                self.counts.set(idx as u64, &c);
                if idx == 1 {
                    break;
                }
                idx /= 2;
            }
            return Ok(());
        }
        loop {
            let mut c = self.counts.get(idx as u64);
            if idx >= edge_layout.segment_count as usize {
                c.actual += d_actual;
                c.total += d_total;
            } else {
                let left = self.counts.get((idx * 2) as u64);
                let right = self.counts.get((idx * 2 + 1) as u64);
                c = SegmentEdgeCounts {
                    actual: left.actual + right.actual,
                    total: left.total + right.total,
                };
            }
            self.counts.set(idx as u64, &c);
            if idx == 1 {
                break;
            }
            idx /= 2;
        }
        Ok(())
    }

    /// Returns whether the overflow log for `vid`'s leaf segment has no free slots.
    ///
    /// `segment_size` must match the edge slab header's `segment_size` field.
    pub(crate) fn log_is_full_with_segment_size(&self, vid: VertexId, segment_size: u32) -> bool {
        let log_h = self.log.header();
        let leaf = leaf_segment(vid, segment_size);
        self.log.read_idx_with_header(&log_h, leaf) >= log_h.max_log_entries as i32
    }

    pub(crate) fn log_fill_ratio(&self, segment: SegmentId) -> f64 {
        let log_h = self.log.header();
        let idx = self
            .log
            .read_idx_with_header(&log_h, u32::from(segment))
            .max(0) as f64;
        let capacity = log_h.max_log_entries.max(1) as f64;
        idx / capacity
    }

    /// Clears all overflow-log entries for `leaf_segment`.
    pub fn release_log_segment(&self, leaf_segment: SegmentId) -> Result<(), GrowFailed> {
        self.log.release_segment(u32::from(leaf_segment))
    }

    pub(crate) fn set_num_edges(&self, n: u64) {
        self.edges.set_num_edges(n);
        let mut header = self.header();
        header.num_edges = n;
        self.header.set(header);
    }

    pub(crate) fn set_elem_capacity(&self, n: u64) -> Result<(), GrowFailed> {
        self.edges.set_elem_capacity(n)?;
        let mut header = self.header();
        header.elem_capacity = n;
        self.header.set(header);
        Ok(())
    }

    pub(crate) fn set_count(&self, index: u64, count: SegmentEdgeCounts) {
        self.counts.set(index, &count);
    }
}

impl InsertLocation {
    pub(crate) fn inserted_into_log(self) -> bool {
        matches!(self, Self::Log)
    }
}

#[inline]
fn leaf_segment(vid: VertexId, segment_size: u32) -> u32 {
    u32::from(vid) / segment_size.max(1)
}

/// Contiguous byte window for one prefetch of the out-edge slab (slot indices `[chunk_low, chunk_high]`).
struct OutEdgeSlabChunk {
    buf: Vec<u8>,
    chunk_low: u32,
    chunk_high: u32,
}

#[inline]
fn out_edge_slab_prefetch_chunk<E: CsrEdge, M: Memory>(
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

fn out_edge_slab_prefetch_chunk_asc<E: CsrEdge, M: Memory>(
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
fn out_edge_slab_decode_slot<E: CsrEdge, M: Memory>(
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
fn out_edge_slab_decode_slot_asc<E: CsrEdge, M: Memory>(
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
    store: &'a EdgeStore<E, M>,
    base_slot_start: u64,
    /// Count of slab prefix slots still to scan; slots are visited `remaining_slab - 1` down to `0`.
    remaining_slab: u32,
    /// Live edges not yet yielded (matches [`ExactSizeIterator`] contract).
    yield_remaining: u32,
    /// Live overflow-log edges in descending-scan order (newest chain head first).
    log_edges: Vec<E>,
    log_pos: usize,
    slab_chunk: Option<OutEdgeSlabChunk>,
    /// Slab slot indices (within this row's slab prefix) targeted by overflow-log delete entries, sorted for
    /// binary search. Slots in this set are skipped during the slab phase without decoding.
    deleted_slab_offsets: Vec<u32>,
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
    store: &'a EdgeStore<E, M>,
    base_slot_start: u64,
    next_slot: u32,
    slab_slots: u32,
    remaining: u32,
    slab_chunk: Option<OutEdgeSlabChunk>,
    deleted_slab_offsets: Vec<u32>,
    inserted_log_edges: std::vec::IntoIter<E>,
}

impl<'a, E, M> AscOutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    fn empty(store: &'a EdgeStore<E, M>) -> Self {
        Self::with_log_replay(store, 0, 0, 0, Vec::new(), Vec::new())
    }

    fn slab_only(
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

    fn with_log_replay(
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lara::vertex::{Vertex, VertexStore};
    use crate::test_support::{TestEdge, vector_memory};
    use crate::{VectorMemory, VertexId};
    use std::{cell::RefCell, rc::Rc};

    #[test]
    fn edge_store_reads_slab_then_log_neighborhood() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex::from_parts(0, 0, 0, -1, false))
            .unwrap();
        vertices
            .push(Vertex::from_parts(1, 0, 0, -1, false))
            .unwrap();

        let edges = EdgeStore::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            vector_memory(),
            8,
            1,
            0,
        )
        .unwrap();
        edges
            .grow_segment_tree_to(segment_tree_leaf_count(VertexCount::from(2u32), 1))
            .unwrap();
        assert_eq!(edges.span_meta_store().len(), 2);

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert_eq!(
            edges.asc_out_edges(&vertices, VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
        assert_eq!(
            edges
                .out_edges_iter(&vertices, VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(11), TestEdge(10)]
        );
        assert_eq!(
            edges
                .desc_out_edges_iter(&vertices, VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(11), TestEdge(10)]
        );
        assert_eq!(
            edges
                .asc_out_edges_iter(&vertices, VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(10), TestEdge(11)]
        );
        assert_eq!(vertices.get(VertexId::from(0)).live_edges, 2);
        assert!(vertices.get(VertexId::from(0)).log_head() >= 0);
    }

    #[test]
    fn edge_store_uses_csr_neighbor_bases_for_slab_space() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex::from_parts(0, 0, 0, -1, false))
            .unwrap();
        vertices
            .push(Vertex::from_parts(2, 0, 0, -1, false))
            .unwrap();

        let edges = EdgeStore::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            vector_memory(),
            4,
            1,
            0,
        )
        .unwrap();
        edges
            .grow_segment_tree_to(segment_tree_leaf_count(VertexCount::from(2u32), 1))
            .unwrap();

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert_eq!(vertices.get(VertexId::from(0)).live_edges, 2);
        assert_eq!(vertices.get(VertexId::from(0)).log_head(), -1);
        assert_eq!(
            edges.asc_out_edges(&vertices, VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
    }

    #[test]
    fn out_edges_iter_nth_pure_slab_matches_scan_order() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex::from_parts(0, 0, 0, -1, false))
            .unwrap();
        vertices
            .push(Vertex::from_parts(2, 0, 0, -1, false))
            .unwrap();

        let edges = EdgeStore::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            vector_memory(),
            4,
            1,
            0,
        )
        .unwrap();
        edges
            .grow_segment_tree_to(segment_tree_leaf_count(VertexCount::from(2u32), 1))
            .unwrap();

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        let scan = edges
            .out_edges_iter(&vertices, VertexId::from(0))
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(scan, vec![TestEdge(11), TestEdge(10)]);

        let mut it = edges.out_edges_iter(&vertices, VertexId::from(0)).unwrap();
        assert_eq!(it.next(), Some(TestEdge(11)));
        let mut it = edges.out_edges_iter(&vertices, VertexId::from(0)).unwrap();
        assert_eq!(it.nth(1), Some(TestEdge(10)));
        let mut it = edges.out_edges_iter(&vertices, VertexId::from(0)).unwrap();
        assert_eq!(it.nth(2), None);
    }

    #[test]
    fn edge_store_scan_uses_base_and_degree_only() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex::from_parts(0, 2, 2, -1, false))
            .unwrap();

        let edges = EdgeStore::new(
            mc,
            me,
            ml,
            vector_memory(),
            vector_memory(),
            vector_memory(),
            2,
            1,
            0,
        )
        .unwrap();
        edges.write_slot(0, TestEdge(10)).unwrap();
        edges.write_slot(1, TestEdge(11)).unwrap();

        assert_eq!(
            edges.asc_out_edges(&vertices, VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
    }
}
