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
//! allowed only when it fits in the vertex's owned `capacity`; otherwise the
//! edge is written to the segment log and later folded by maintenance or
//! relocation.
//!
//! ## Layout assumptions (update paths)
//!
//! When a vertex row uses `capacity == 0` (no explicit owned span), slab vs
//! log splitting uses the **next vertex row’s** `base_slot_start` as the
//! exclusive end of the current row’s slab window. This matches compact CSR
//! layouts where `VertexId` order and slab bases are non-decreasing. If that
//! invariant is violated, behavior is undefined; **debug builds** assert it on
//! the hot paths below. Prefer [`crate::LaraGraph`] orchestration over ad-hoc
//! [`EdgeStore`] mutation so geometry and PMA counts stay aligned.

#[cfg(feature = "canbench")]
mod bench;
pub mod counts;
mod edges;
pub mod free_span;
mod log;
pub mod span_meta;

use crate::{
    GrowFailed, SegmentId, VertexId,
    traits::{CsrEdge, CsrVertex, CsrVertexTombstoneScan},
};
use counts::{SegmentEdgeCounts, SegmentEdgeCountsStore};
use edges::{EdgeSlabStore, tree_height_for_segment_count};
pub use edges::{HeaderV1 as EdgeHeaderV1, segment_tree_leaf_count};
use free_span::{FreeSpan, FreeSpanStore};
use ic_stable_structures::Memory;
pub use log::HeaderV1 as LogHeaderV1;
use log::LogStore;
use span_meta::{SegmentSpanMeta, SegmentSpanMetaStore};
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
struct EdgeLayout {
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
    num_edges: u64,
}

impl From<EdgeHeaderV1> for EdgeLayout {
    fn from(header: EdgeHeaderV1) -> Self {
        Self {
            elem_capacity: header.elem_capacity,
            segment_count: header.segment_count,
            segment_size: header.segment_size,
            num_edges: header.num_edges,
        }
    }
}

/// Errors returned when reopening the full edge storage subsystem.
#[derive(Debug)]
pub enum InitError {
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

pub(crate) trait VertexAccess<V: CsrVertex> {
    fn len(&self) -> u64;
    fn get(&self, id: VertexId) -> V;
    fn set(&self, id: VertexId, item: &V);
}

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
        let segment_count = segment_tree_leaf_count(0, segment_size);
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
            span_meta.push(SegmentSpanMeta { physical_start: 0 })?;
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

    /// Reopens an edge subsystem from previously initialized stable memories.
    pub fn init(
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
    ) -> Result<Self, InitError> {
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
            self.span_meta.push(SegmentSpanMeta { physical_start: 0 })?;
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
        let idx = u64::from(u32::from(segment));
        if idx < self.span_meta.len() {
            self.span_meta.set(idx, &SegmentSpanMeta { physical_start });
        } else {
            while self.span_meta.len() < idx {
                self.span_meta.push(SegmentSpanMeta { physical_start: 0 })?;
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

    pub(crate) fn allocate_span(&self, len: u64) -> Result<u64, GrowFailed> {
        let cap = self.header().elem_capacity;
        if len == 0 {
            return Ok(cap);
        }
        if let Some(span) = self.free_spans.take_best_fit(len).map_err(|_| GrowFailed {
            current_size: 0,
            delta: 0,
        })? {
            return Ok(span.start_slot);
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
    ) -> Result<Vec<E>, &'static str>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let vidx = vertex_index(vid);
        if vidx >= vertices.len() as usize {
            return Err("vertex out of range");
        }
        let v = vertices.get(vid);
        if V::record_is_vertex_tombstone(&v) {
            return Err("vertex deleted");
        }
        if v.log_head() < 0 {
            let degree = v.degree() as usize;
            let base = v.base_slot_start();
            if degree == 0 {
                return Ok(Vec::new());
            }
            let nbytes = degree.checked_mul(E::BYTES).ok_or("collect overflow")?;
            let mut raw = vec![0u8; nbytes];
            self.edges.read_slots_contiguous(base, &mut raw);
            let mut out = Vec::with_capacity(degree);
            for chunk in raw.chunks_exact(E::BYTES) {
                out.push(E::read_from(chunk));
            }
            return Ok(out);
        }

        let edge_layout = self.edge_layout();
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, vidx, &v);
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
        let leaf = leaf_segment(vid, edge_layout.segment_size);
        let log_h = self.log.header();

        let mut log_i = v.log_head();
        let filler = if slab_count > 0 {
            out[0]
        } else {
            if log_i < 0 {
                return Err("log chain short");
            }
            let (prev, edge) = self.read_log_edge_with_header(&log_h, leaf, log_i as u32);
            log_i = prev;
            edge
        };
        out.resize(degree, filler);

        if slab_count == 0 {
            for offset in (0..log_count.saturating_sub(1)).rev() {
                if log_i < 0 {
                    return Err("log chain short");
                }
                let (prev, edge) = self.read_log_edge_with_header(&log_h, leaf, log_i as u32);
                out[slab_count + offset] = edge;
                log_i = prev;
            }
        } else {
            for offset in (0..log_count).rev() {
                if log_i < 0 {
                    return Err("log chain short");
                }
                let (prev, edge) = self.read_log_edge_with_header(&log_h, leaf, log_i as u32);
                out[slab_count + offset] = edge;
                log_i = prev;
            }
        }
        Ok(out)
    }

    pub(crate) fn iter_out_edges<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<OutEdgesIter<'_, E, M>, &'static str>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let vidx = vertex_index(vid);
        if vidx >= vertices.len() as usize {
            return Err("vertex out of range");
        }
        let v = vertices.get(vid);
        if V::record_is_vertex_tombstone(&v) {
            return Err("vertex deleted");
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
                slab_chunk,
            });
        }

        let edge_layout = self.edge_layout();
        let on_slab = self.on_slab_edges_with_layout(&edge_layout, vertices, vidx, &v);
        let degree = v.degree();
        let slab_count = on_slab.min(degree);
        let log_count = degree.saturating_sub(slab_count);

        Ok(OutEdgesIter {
            store: self,
            leaf: leaf_segment(vid, edge_layout.segment_size),
            next_log: v.log_head(),
            remaining_log: log_count,
            base_slot_start: v.base_slot_start(),
            remaining_slab: slab_count,
            log_header: (log_count > 0).then(|| self.log.header()),
            slab_chunk: None,
        })
    }

    fn read_log_edge_with_header(&self, log_h: &LogHeaderV1, leaf: u32, log_idx: u32) -> (i32, E) {
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
    ) -> Result<InsertLocation, &'static str>
    where
        V: CsrVertex + CsrVertexTombstoneScan,
        A: VertexAccess<V>,
    {
        let edge_layout = self.edge_layout();
        let vidx = vertex_index(vid);
        if vidx >= vertices.len() as usize {
            return Err("vertex out of range");
        }
        let v = vertices.get(vid);
        if V::record_is_vertex_tombstone(&v) {
            return Err("vertex deleted");
        }
        let loc = v.base_slot_start().saturating_add(u64::from(v.degree()));
        let location = if self.have_space_on_slab(vertices, vidx, &v, loc, edge_layout) {
            self.write_slot(loc, edge)
                .map_err(|_| "write edge slot failed")?;
            vertices.set(vid, &v.with_degree(v.degree() + 1));
            InsertLocation::Slab
        } else {
            self.insert_into_log_with_layout(&edge_layout, vertices, vid, v, edge)?;
            InsertLocation::Log
        };
        self.set_num_edges(edge_layout.num_edges.saturating_add(1));
        self.bump_counts_leaf_with_layout(&edge_layout, vid, 1, 0)?;
        Ok(location)
    }

    pub(crate) fn remove_edge_unordered_matching<V, A, F>(
        &self,
        vertices: &A,
        vid: VertexId,
        mut matches: F,
    ) -> Result<Option<E>, &'static str>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
        F: FnMut(&E) -> bool,
    {
        let edge_layout = self.edge_layout();
        let vidx = vertex_index(vid);
        if vidx >= vertices.len() as usize {
            return Err("vertex out of range");
        }
        let v = vertices.get(vid);
        if v.log_head() >= 0 {
            return Err("remove requires slab-only row");
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
                .map_err(|_| "write edge slot failed")?;
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
    ) -> Result<Option<E>, &'static str>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let vidx = vertex_index(vid);
        if vidx >= vertices.len() as usize {
            return Err("vertex out of range");
        }
        let v = vertices.get(vid);
        if v.log_head() >= 0 {
            return Err("row edge read requires slab-only row");
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
    ) -> Result<u32, &'static str>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let edge_layout = self.edge_layout();
        let vidx = vertex_index(vid);
        if vidx >= vertices.len() as usize {
            return Err("vertex out of range");
        }
        let v = vertices.get(vid);
        if v.log_head() >= 0 {
            return Err("clear row requires slab-only row");
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
        v: V,
        edge: E,
    ) -> Result<(), &'static str>
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        let leaf = leaf_segment(vid, edge_layout.segment_size);
        let log_h = self.log.header();
        let idx = self.log.read_idx_with_header(&log_h, leaf);
        if idx < 0 || idx >= log_h.max_log_entries as i32 {
            return Err("segment log full");
        }
        let src = i32::try_from(u32::from(vid)).map_err(|_| "vertex id exceeds i32")?;
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
                .map_err(|_| "write log failed")?;
        } else {
            let mut payload = vec![0u8; E::BYTES];
            edge.write_to(&mut payload);
            self.log
                .write_entry_with_header(&log_h, leaf, idx as u32, v.log_head(), src, &payload)
                .map_err(|_| "write log failed")?;
        }
        self.log.write_idx_with_header(&log_h, leaf, idx + 1);
        vertices.set(vid, &v.with_log_head(idx).with_degree(v.degree() + 1));
        Ok(())
    }

    fn on_slab_edges_with_layout<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        vertices: &A,
        vidx: usize,
        v: &V,
    ) -> u32
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        if v.log_head() < 0 {
            return v.degree();
        }
        if v.span_capacity() > 0 {
            return v.degree().min(v.span_capacity());
        }
        let current_leaf = (vidx as u32) / edge_layout.segment_size.max(1);
        let next = if vidx + 1 < vertices.len() as usize {
            let next_base = vertices.get(vertex_id(vidx + 1)).base_slot_start();
            debug_assert!(
                next_base >= v.base_slot_start(),
                "LARA CSR invariant: base_slot_start must be non-decreasing in VertexId order"
            );
            next_base
        } else if current_leaf < edge_layout.segment_count {
            let c = self
                .counts
                .get(u64::from(current_leaf + edge_layout.segment_count));
            v.base_slot_start().saturating_add(c.total.max(0) as u64)
        } else {
            edge_layout.elem_capacity
        };
        next.saturating_sub(v.base_slot_start())
            .min(u64::from(u32::MAX)) as u32
    }

    fn have_space_on_slab<V, A>(
        &self,
        vertices: &A,
        vidx: usize,
        v: &V,
        loc: u64,
        edge_layout: EdgeLayout,
    ) -> bool
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        if v.span_capacity() > 0 {
            return loc
                < v.base_slot_start()
                    .saturating_add(u64::from(v.span_capacity()));
        }
        let seg_sz = edge_layout.segment_size.max(1);
        let current_leaf = (vidx as u32) / seg_sz;
        if vidx + 1 < vertices.len() as usize {
            let next_base = vertices.get(vertex_id(vidx + 1)).base_slot_start();
            debug_assert!(
                next_base >= v.base_slot_start(),
                "LARA CSR invariant: base_slot_start must be non-decreasing in VertexId order"
            );
            return loc < next_base;
        }
        if current_leaf < edge_layout.segment_count {
            let c = self
                .counts
                .get(u64::from(current_leaf + edge_layout.segment_count));
            loc < vertices
                .get(vertex_id(vidx))
                .base_slot_start()
                .saturating_add(c.total.max(0) as u64)
        } else {
            loc < edge_layout.elem_capacity
        }
    }

    fn bump_counts_leaf_with_layout(
        &self,
        edge_layout: &EdgeLayout,
        vid: VertexId,
        d_actual: i64,
        d_total: i64,
    ) -> Result<(), &'static str> {
        let mut idx =
            (leaf_segment(vid, edge_layout.segment_size) + edge_layout.segment_count) as usize;
        if idx as u64 >= self.counts.len() {
            return Err("segment counts tree too small");
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
fn vertex_index(vid: VertexId) -> usize {
    u32::from(vid) as usize
}

#[inline]
fn vertex_id(index: usize) -> VertexId {
    VertexId::from(u32::try_from(index).expect("vertex index exceeds VertexId"))
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
            let (prev, edge) =
                self.store
                    .read_log_edge_with_header(log_h, self.leaf, self.next_log as u32);
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
    use crate::test_support::{
        CAPACITY_READS, CountingCapacityVertex, PoisonCapacityVertex, TestEdge, vector_memory,
    };
    use crate::{VectorMemory, VertexId};
    use std::{cell::RefCell, rc::Rc, sync::atomic::Ordering};

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
                capacity: 1,
                log_head: -1,
                deleted: false,
            })
            .unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 1,
                degree: 0,
                capacity: 1,
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
            .grow_segment_tree_to(segment_tree_leaf_count(2, 1))
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
    fn edge_store_uses_vertex_capacity_for_slab_space() {
        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<Vertex, _>::new(mv).unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 0,
                degree: 0,
                capacity: 2,
                log_head: -1,
                deleted: false,
            })
            .unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 1,
                degree: 0,
                capacity: 1,
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
            .grow_segment_tree_to(segment_tree_leaf_count(2, 1))
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
                capacity: 2,
                log_head: -1,
                deleted: false,
            })
            .unwrap();
        vertices
            .push(Vertex {
                base_slot_start: 2,
                degree: 0,
                capacity: 1,
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
            .grow_segment_tree_to(segment_tree_leaf_count(2, 1))
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
    fn edge_store_insert_reads_capacity_for_update_boundary() {
        CAPACITY_READS.store(0, Ordering::SeqCst);

        let mv: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let mc: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let me: VectorMemory = Rc::new(RefCell::new(Vec::new()));
        let ml: VectorMemory = Rc::new(RefCell::new(Vec::new()));

        let vertices = VertexStore::<CountingCapacityVertex, _>::new(mv).unwrap();
        vertices
            .push(CountingCapacityVertex {
                base_slot_start: 0,
                degree: 0,
                capacity: 2,
                log_head: -1,
            })
            .unwrap();
        vertices
            .push(CountingCapacityVertex {
                base_slot_start: 1,
                degree: 0,
                capacity: 1,
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
            4,
            1,
            0,
        )
        .unwrap();
        edges
            .grow_segment_tree_to(segment_tree_leaf_count(2, 1))
            .unwrap();

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert!(CAPACITY_READS.load(Ordering::SeqCst) >= 2);
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
    fn edge_store_scan_uses_base_and_degree_not_capacity() {
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
