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
//! Clean neighbor scans read only the vertex row's `base_slot_start` and
//! `degree`, then walk the edge slab. The log, counts, span metadata, and free
//! span index are update-side structures. They may be read while inserting,
//! folding logs, resizing, or relocating, but they are not part of the clean
//! scan contract.
//!
//! Insertions first try to append at `base_slot_start + degree`. The append is
//! allowed only when it stays before this row’s CSR slab boundary (the next
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
//! (`iter_out_edges`, `collect_out_edges_slot_order`) treats **tombstone + zero
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
    traits::{CsrEdge, CsrVertex, CsrVertexTombstoneScan},
};
use counts::{SegmentEdgeCounts, SegmentEdgeCountsStore};
pub(crate) use edges::EdgeSlabStore;
use edges::tree_height_for_segment_count;
pub use edges::{HeaderV1 as EdgeHeaderV1, InitError as SlabInitError, segment_tree_leaf_count};
use free_span::{FreeSpan, FreeSpanStore};
use ic_stable_structures::Memory;
pub use log::HeaderV1 as LogHeaderV1;
use log::LogStore;
use span_meta::{SPAN_PHYSICAL_UNASSIGNED, SegmentSpanMeta, SegmentSpanMetaStore};
use std::{cell::Cell, fmt, iter::FusedIterator};

const INLINE_EDGE_BYTES: usize = 64;
/// When a clean slab row is at least this many bytes, [`OutEdgesIter`] reads the
/// slab in fixed-size slot chunks instead of one stable read per edge.
const SLAB_ITER_PREFETCH_MIN_BYTES: usize = 64;
/// Number of consecutive slab slots loaded per chunk for [`OutEdgesIter`] when
/// chunking is enabled.
const SLAB_ITER_CHUNK_SLOTS: u32 = 32;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertLocation {
    Slab,
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
                return next_base;
            }
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            let cap = span_rec
                .physical_start
                .saturating_add(c.total.max(0) as u64);
            return next_base.min(cap);
        }

        let w = edge_layout.initial_vertex_edge_slots;
        if w > 0 && v_ord.saturating_add(1) < leaf_logical_end_exclusive {
            let tail = base.saturating_add(u64::from(w));
            let span_rec = self.span_meta_store().get(u64::from(leaf));
            if span_rec.physical_start == SPAN_PHYSICAL_UNASSIGNED {
                return tail;
            }
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            let cap = span_rec
                .physical_start
                .saturating_add(c.total.max(0) as u64);
            return tail.min(cap);
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
                    return next_base.min(cap);
                }
                if leaf < edge_layout.segment_count {
                    let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
                    let total_u = c.total.max(0) as u64;
                    if total_u > 0 {
                        let head = vertices.get(VertexId::from(leaf_start)).base_slot_start();
                        let cap = head.saturating_add(total_u);
                        return next_base.min(cap);
                    }
                }
                return next_base;
            }
        }

        let span_rec = self.span_meta_store().get(u64::from(leaf));
        if span_rec.physical_start != SPAN_PHYSICAL_UNASSIGNED {
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            return span_rec
                .physical_start
                .saturating_add(c.total.max(0) as u64);
        }

        if leaf < edge_layout.segment_count {
            let c = self.counts.get(u64::from(leaf + edge_layout.segment_count));
            base.saturating_add(c.total.max(0) as u64)
        } else {
            edge_layout.elem_capacity
        }
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
        self.set_elem_capacity(start.saturating_add(len))?;
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

    pub(crate) fn collect_out_edges_slot_order<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<Vec<E>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        let v_ord = u32::from(vid);
        let log_owner = vertices.log_leaf_vertex(vid);
        // Tombstone rows may still hold slab/log material while incremental
        // `DeleteVertex` maintenance runs; only fully evacuated rows reject reads.
        if V::record_is_vertex_tombstone(&v) && v.degree() == 0 && v.log_head() < 0 {
            return Ok(Vec::new());
        }
        if v.log_head() < 0 {
            let degree = v.degree() as usize;
            let base = v.base_slot_start();
            if degree == 0 {
                return Ok(Vec::new());
            }
            let nbytes = degree
                .checked_mul(E::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut raw = vec![0u8; nbytes];
            self.edges.read_slots_contiguous(base, &mut raw);
            let mut out = Vec::with_capacity(degree);
            for chunk in raw.chunks_exact(E::BYTES) {
                out.push(E::read_from(chunk));
            }
            return Ok(out);
        }

        let edge_layout = self.edge_layout();
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, v_ord, &v);
        let degree = v.degree() as usize;
        let slab_count = on_slab.min(v.degree()) as usize;
        let mut out = Vec::with_capacity(degree);
        for i in 0..slab_count {
            out.push(self.read_slot(v.base_slot_start() + i as u64));
        }
        if slab_count == degree {
            return Ok(out);
        }

        let log_count = degree - slab_count;
        let leaf = leaf_segment(log_owner, edge_layout.segment_size);
        let log_h = self.log.header();

        let mut log_table_buf = Vec::new();
        self.log
            .read_segment_entry_table_into(&log_h, leaf, &mut log_table_buf);
        let log_table = (!log_table_buf.is_empty()).then_some(log_table_buf.as_slice());

        let mut log_i = v.log_head();
        let filler = if slab_count > 0 {
            out[0]
        } else {
            if log_i < 0 {
                return Err(LaraOperationError::LogChainShort);
            }
            let (prev, edge) =
                self.read_log_edge_from_table_or_store(&log_h, leaf, log_i as u32, log_table);
            log_i = prev;
            edge
        };
        out.resize(degree, filler);

        if slab_count == 0 {
            for offset in (0..log_count.saturating_sub(1)).rev() {
                if log_i < 0 {
                    return Err(LaraOperationError::LogChainShort);
                }
                let (prev, edge) =
                    self.read_log_edge_from_table_or_store(&log_h, leaf, log_i as u32, log_table);
                out[slab_count + offset] = edge;
                log_i = prev;
            }
        } else {
            for offset in (0..log_count).rev() {
                if log_i < 0 {
                    return Err(LaraOperationError::LogChainShort);
                }
                let (prev, edge) =
                    self.read_log_edge_from_table_or_store(&log_h, leaf, log_i as u32, log_table);
                out[slab_count + offset] = edge;
                log_i = prev;
            }
        }
        Ok(out)
    }

    /// Walks outgoing edges without materializing the full row vector.
    ///
    /// Invokes `visit` for each edge that satisfies `matches` (same contract as
    /// [`LaraGraph::remove_edge_matching`](super::LaraGraph::remove_edge_matching)).
    ///
    /// On slab-only rows, when `raw_matches` is `Some`, it is consulted on each
    /// encoded record **before** decoding; a `false` result skips the slot with no
    /// [`CsrEdge::read_from`]. Log-backed rows still use `matches` only.
    pub(crate) fn for_each_out_edge_matching<V, A, Match, Visit>(
        &self,
        vertices: &A,
        vid: VertexId,
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
        let v = vertices.get_in_range(vid)?;
        if V::record_is_vertex_tombstone(&v) && v.degree() == 0 && v.log_head() < 0 {
            return Ok(());
        }
        if v.log_head() < 0 {
            let degree = v.degree() as usize;
            if degree == 0 {
                return Ok(());
            }
            let nbytes = degree
                .checked_mul(E::BYTES)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let mut raw = vec![0u8; nbytes];
            self.read_slots_contiguous(v.base_slot_start(), &mut raw);
            for chunk in raw.chunks_exact(E::BYTES) {
                if let Some(raw_m) = raw_matches.as_mut() {
                    if !raw_m(chunk) {
                        continue;
                    }
                    visit(E::read_from(chunk));
                } else {
                    let edge = E::read_from(chunk);
                    if matches(&edge) {
                        visit(edge);
                    }
                }
            }
            return Ok(());
        }

        let mut iter = self.iter_out_edges(vertices, vid)?;
        while let Some(edge) = iter.next() {
            if matches(&edge) {
                visit(edge);
            }
        }
        Ok(())
    }

    /// Returns `true` when [`Self::collect_out_edges_slot_order`] would yield a non-empty vector.
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

    pub(crate) fn iter_out_edges<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<OutEdgesIter<'_, E, M>, LaraOperationError>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let v = vertices.get_in_range(vid)?;
        let v_ord = u32::from(vid);
        let log_owner = vertices.log_leaf_vertex(vid);
        // See `collect_out_edges_slot_order`: allow enumeration for tombstones that
        // still have pending edge material (rebalance during vertex delete).
        if V::record_is_vertex_tombstone(&v) && v.degree() == 0 && v.log_head() < 0 {
            return Ok(OutEdgesIter {
                store: self,
                leaf: 0,
                next_log: -1,
                remaining_log: 0,
                base_slot_start: v.base_slot_start(),
                remaining_slab: 0,
                log_header: None,
                log_table: None,
                slab_chunk: None,
            });
        }
        // Clean rows: the full neighborhood is on the slab, so the iterator never
        // walks the overflow log. Skip `edge_layout()` (full slab header read) and
        // log metadata; `leaf` is only read while `remaining_log > 0`.
        if v.log_head() < 0 {
            let degree = v.degree();
            let nbytes = (degree as usize).saturating_mul(E::BYTES);
            let slab_chunk = if nbytes >= SLAB_ITER_PREFETCH_MIN_BYTES {
                Some(SlabChunkCache {
                    buf: Vec::new(),
                    chunk_low: 0,
                    chunk_high: 0,
                })
            } else {
                None
            };
            return Ok(OutEdgesIter {
                store: self,
                leaf: 0,
                next_log: -1,
                remaining_log: 0,
                base_slot_start: v.base_slot_start(),
                remaining_slab: degree,
                log_header: None,
                log_table: None,
                slab_chunk,
            });
        }

        let edge_layout = self.edge_layout();
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, v_ord, &v);
        let degree = v.degree();
        let slab_count = on_slab.min(degree);
        let log_count = degree.saturating_sub(slab_count);

        Ok(OutEdgesIter {
            store: self,
            leaf: leaf_segment(log_owner, edge_layout.segment_size),
            next_log: v.log_head(),
            remaining_log: log_count,
            base_slot_start: v.base_slot_start(),
            remaining_slab: slab_count,
            log_header: (log_count > 0).then(|| self.log.header()),
            log_table: None,
            slab_chunk: None,
        })
    }

    fn read_log_edge_from_table_or_store(
        &self,
        log_h: &LogHeaderV1,
        leaf: u32,
        log_idx: u32,
        table: Option<&[u8]>,
    ) -> (i32, E) {
        if let Some(buf) = table {
            let stride = log_h.stride as usize;
            if stride > 0 {
                let off = log_idx as usize * stride;
                if off + stride <= buf.len() && off + 8 + E::BYTES <= buf.len() {
                    let prev = i32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
                    let _src = i32::from_le_bytes(buf[off + 4..off + 8].try_into().unwrap());
                    let edge = E::read_from(&buf[off + 8..off + 8 + E::BYTES]);
                    return (prev, edge);
                }
            }
        }
        if E::BYTES <= 8 {
            let mut buf = [0u8; 8];
            let (prev, _src) =
                self.log
                    .read_entry_with_header(log_h, leaf, log_idx, &mut buf[..E::BYTES]);
            (prev, E::read_from(&buf[..E::BYTES]))
        } else if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            let (prev, _src) =
                self.log
                    .read_entry_with_header(log_h, leaf, log_idx, &mut buf[..E::BYTES]);
            (prev, E::read_from(&buf[..E::BYTES]))
        } else {
            let mut buf = vec![0u8; E::BYTES];
            let (prev, _src) = self
                .log
                .read_entry_with_header(log_h, leaf, log_idx, &mut buf);
            (prev, E::read_from(&buf))
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
        let loc = v.base_slot_start().saturating_add(u64::from(v.degree()));
        let location = if self.have_space_on_slab(vertices, v_ord, &v, loc, &edge_layout) {
            self.write_slot(loc, edge)
                .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
            vertices.set(vid, &v.with_degree(v.degree() + 1));
            InsertLocation::Slab
        } else {
            self.insert_into_log_with_layout(&edge_layout, vertices, vid, log_owner, v, edge)?;
            InsertLocation::Log
        };
        self.set_num_edges(edge_layout.num_edges.saturating_add(1));
        self.bump_counts_leaf_with_layout(&edge_layout, log_owner, 1, 0)?;
        Ok(location)
    }

    pub(crate) fn remove_edge_unordered_matching<V, A, F>(
        &self,
        vertices: &A,
        vid: VertexId,
        mut matches: F,
    ) -> Result<Option<E>, LaraOperationError>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
        F: FnMut(&E) -> bool,
    {
        let edge_layout = self.edge_layout();
        let v = vertices.get_in_range(vid)?;
        if v.log_head() >= 0 {
            return Err(LaraOperationError::RemoveRequiresSlabOnlyRow);
        }
        let degree = v.degree();
        if degree == 0 {
            return Ok(None);
        }

        let base = v.base_slot_start();
        let mut found = None;
        for i in 0..degree {
            let edge = self.read_slot(base.saturating_add(u64::from(i)));
            if matches(&edge) {
                found = Some(i);
                break;
            }
        }
        let Some(local_index) = found else {
            return Ok(None);
        };

        let removed = self.read_slot(base.saturating_add(u64::from(local_index)));
        let last_index = degree - 1;
        if local_index != last_index {
            let last = self.read_slot(base.saturating_add(u64::from(last_index)));
            self.write_slot(base.saturating_add(u64::from(local_index)), last)
                .map_err(LaraOperationError::WriteEdgeSlotFailed)?;
        }
        vertices.set(vid, &v.with_degree(last_index));
        self.set_num_edges(edge_layout.num_edges.saturating_sub(1));
        self.bump_counts_leaf_with_layout(&edge_layout, vid, -1, 0)?;
        Ok(Some(removed))
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
        Ok(Some(self.read_slot(
            v.base_slot_start().saturating_add(u64::from(offset)),
        )))
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
        if v.log_head() >= 0 {
            return Err(LaraOperationError::ClearRowRequiresSlabOnlyRow);
        }
        let removed = v.degree();
        if removed == 0 {
            return Ok(0);
        }
        vertices.set(vid, &v.with_degree(0).with_log_head(-1));
        self.set_num_edges(edge_layout.num_edges.saturating_sub(u64::from(removed)));
        self.bump_counts_leaf_with_layout(&edge_layout, vid, -i64::from(removed), 0)?;
        Ok(removed)
    }

    fn insert_into_log_with_layout<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        vid: VertexId,
        log_owner: VertexId,
        v: V,
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
        vertices.set(vid, &v.with_log_head(idx).with_degree(v.degree() + 1));
        Ok(())
    }

    fn on_slab_edges_with_layout<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        v_ord: u32,
        v: &V,
    ) -> u32
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        if v.log_head() < 0 {
            return v.degree();
        }
        let next_exclusive = self.slab_window_exclusive_end(edge_layout, vertices, v_ord, v);
        let span_slots = next_exclusive.saturating_sub(v.base_slot_start());
        let span_u32 = span_slots.min(u64::from(u32::MAX)) as u32;
        // Once the overflow log is active, the slab prefix is at most the CSR window
        // width; additional live edges are chained through `log_head`.
        if v.degree() > span_u32 {
            span_u32
        } else {
            v.degree()
        }
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

#[inline]
fn leaf_segment(vid: VertexId, segment_size: u32) -> u32 {
    u32::from(vid) / segment_size.max(1)
}

/// Iterator over outgoing edges in the store's standard scan order.
///
/// The order is deterministic for the committed store state, but it is not
/// guaranteed to be insertion order or slab slot order. It may change after
/// unordered removals, rebalancing, or future layout changes.
///
/// `leaf` is only consulted while draining the overflow log (`remaining_log >
/// 0`). For purely slab-backed rows, it is uninitialized and must not be read.
///
/// For clean slab-only rows whose encoded row is at least 64 bytes, `slab_chunk`
/// caches a window of consecutive slab slots so [`Iterator::next`] issues one
/// stable read per chunk (32 slots) instead of per edge.
pub struct OutEdgesIter<'a, E: CsrEdge, M: Memory> {
    store: &'a EdgeStore<E, M>,
    leaf: u32,
    next_log: i32,
    remaining_log: u32,
    base_slot_start: u64,
    remaining_slab: u32,
    log_header: Option<LogHeaderV1>,
    /// Prefetched [`LogStore::read_segment_entry_table_into`] bytes; filled on first log step.
    log_table: Option<Vec<u8>>,
    slab_chunk: Option<SlabChunkCache>,
}

/// Contiguous slab bytes for slot indices `[chunk_low, chunk_high]` inclusive.
struct SlabChunkCache {
    buf: Vec<u8>,
    chunk_low: u32,
    chunk_high: u32,
}

impl<'a, E, M> OutEdgesIter<'a, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    fn fill_slab_chunk(
        cache: &mut SlabChunkCache,
        store: &'a EdgeStore<E, M>,
        base: u64,
        slot_idx: u32,
    ) {
        let high = slot_idx;
        let span = SLAB_ITER_CHUNK_SLOTS.min(high.saturating_add(1));
        let low = high.saturating_sub(span - 1);
        let nbytes = span as usize * E::BYTES;
        cache.buf.resize(nbytes, 0);
        cache.chunk_low = low;
        cache.chunk_high = high;
        let start = base.saturating_add(u64::from(low));
        store.read_slots_contiguous(start, &mut cache.buf);
    }

    fn decode_slab_slot(&mut self, slot_idx: u32) -> E {
        if let Some(cache) = &mut self.slab_chunk {
            if cache.buf.is_empty() || slot_idx < cache.chunk_low || slot_idx > cache.chunk_high {
                Self::fill_slab_chunk(cache, self.store, self.base_slot_start, slot_idx);
            }
            let off = (slot_idx - cache.chunk_low) as usize * E::BYTES;
            debug_assert!(off + E::BYTES <= cache.buf.len());
            E::read_from(&cache.buf[off..off + E::BYTES])
        } else {
            self.store
                .read_slot(self.base_slot_start + u64::from(slot_idx))
        }
    }
}

impl<E, M> Iterator for OutEdgesIter<'_, E, M>
where
    E: CsrEdge,
    M: Memory,
{
    type Item = E;

    fn next(&mut self) -> Option<Self::Item> {
        if self.remaining_log > 0 {
            if self.next_log < 0 {
                self.remaining_log = 0;
                self.remaining_slab = 0;
                return None;
            }
            let log_h = self.log_header.as_ref()?;
            if self.log_table.is_none() {
                let mut buf = Vec::new();
                self.store
                    .log
                    .read_segment_entry_table_into(log_h, self.leaf, &mut buf);
                self.log_table = Some(buf);
            }
            let table = self.log_table.as_ref().map(|b| b.as_slice());
            let (prev, edge) = self.store.read_log_edge_from_table_or_store(
                log_h,
                self.leaf,
                self.next_log as u32,
                table,
            );
            self.next_log = prev;
            self.remaining_log -= 1;
            return Some(edge);
        }

        if self.remaining_slab == 0 {
            return None;
        }
        self.remaining_slab -= 1;
        Some(self.decode_slab_slot(self.remaining_slab))
    }

    fn nth(&mut self, n: usize) -> Option<Self::Item> {
        if self.remaining_log == 0 {
            if n >= self.remaining_slab as usize {
                self.remaining_slab = 0;
                return None;
            }
            self.remaining_slab -= n as u32;
            return self.next();
        }
        for _ in 0..n {
            self.next()?;
        }
        self.next()
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        let remaining = u64::from(self.remaining_log) + u64::from(self.remaining_slab);
        let n = usize::try_from(remaining).unwrap_or(usize::MAX);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lara::vertex::{Vertex, VertexStore};
    use crate::test_support::{PoisonCapacityVertex, TestEdge, vector_memory};
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
            .push(Vertex {
                base_slot_start: 0,
                degree: 0,
                log_head: -1,
                deleted: false,
            })
            .unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 1,
                degree: 0,
                log_head: -1,
                deleted: false,
            })
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
            edges
                .collect_out_edges_slot_order(&vertices, VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
        assert_eq!(
            edges
                .iter_out_edges(&vertices, VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(11), TestEdge(10)]
        );
        assert_eq!(vertices.get(VertexId::from(0)).degree, 2);
        assert!(vertices.get(VertexId::from(0)).log_head >= 0);
    }

    #[test]
    fn edge_store_uses_csr_neighbor_bases_for_slab_space() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 0,
                degree: 0,
                log_head: -1,
                deleted: false,
            })
            .unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 2,
                degree: 0,
                log_head: -1,
                deleted: false,
            })
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

        assert_eq!(vertices.get(VertexId::from(0)).degree, 2);
        assert_eq!(vertices.get(VertexId::from(0)).log_head, -1);
        assert_eq!(
            edges
                .collect_out_edges_slot_order(&vertices, VertexId::from(0))
                .unwrap(),
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
            .push(Vertex {
                base_slot_start: 0,
                degree: 0,
                log_head: -1,
                deleted: false,
            })
            .unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 2,
                degree: 0,
                log_head: -1,
                deleted: false,
            })
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
            .iter_out_edges(&vertices, VertexId::from(0))
            .unwrap()
            .collect::<Vec<_>>();
        assert_eq!(scan, vec![TestEdge(11), TestEdge(10)]);

        let mut it = edges.iter_out_edges(&vertices, VertexId::from(0)).unwrap();
        assert_eq!(it.next(), Some(TestEdge(11)));
        let mut it = edges.iter_out_edges(&vertices, VertexId::from(0)).unwrap();
        assert_eq!(it.nth(1), Some(TestEdge(10)));
        let mut it = edges.iter_out_edges(&vertices, VertexId::from(0)).unwrap();
        assert_eq!(it.nth(2), None);
    }

    #[test]
    fn edge_store_scan_uses_base_and_degree_only() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<PoisonCapacityVertex, _>::new(mv).unwrap();
        vertices
            .push(PoisonCapacityVertex {
                base_slot_start: 0,
                degree: 2,
                log_head: -1,
            })
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
            edges
                .collect_out_edges_slot_order(&vertices, VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
    }
}
