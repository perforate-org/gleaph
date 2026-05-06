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

pub mod counts;
mod edges;
pub mod free_span;
mod log;
pub mod span_meta;

use crate::{
    GrowFailed, SegmentId, VertexId,
    traits::{CsrEdge, CsrVertex, LaraVertex},
};
use counts::{EdgePmaCountsStride, SegmentEdgeCounts, SegmentEdgeCountsStore};
use edges::EdgeSlabStore;
pub use edges::HeaderV1 as EdgeHeaderV1;
use free_span::{FreeSpan, FreeSpanStore};
use ic_stable_structures::Memory;
pub use log::HeaderV1 as LogHeaderV1;
use log::LogStore;
use span_meta::{SegmentSpanMeta, SegmentSpanMetaStore};
use std::fmt;

const INLINE_EDGE_BYTES: usize = 64;

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

#[derive(Clone, Copy, Debug)]
struct LogLayout {
    max_log_entries: u32,
}

impl From<LogHeaderV1> for LogLayout {
    fn from(header: LogHeaderV1) -> Self {
        Self {
            max_log_entries: header.max_log_entries,
        }
    }
}

#[derive(Debug)]
pub enum InitError {
    Counts(counts::InitError),
    Edges(edges::InitError),
    Log(log::InitError),
    SpanMeta(span_meta::InitError),
    LogLayoutMismatch,
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
    fn get(&self, index: u64) -> V;
    fn set(&self, index: u64, item: &V);
}

pub struct EdgeStore<E: CsrEdge, M: Memory> {
    counts: SegmentEdgeCountsStore<E, M>,
    edges: EdgeSlabStore<E, M>,
    log: LogStore<E, M>,
    span_meta: SegmentSpanMetaStore<M>,
    free_spans: FreeSpanStore<M>,
}

impl<E: CsrEdge + EdgePmaCountsStride, M: Memory> EdgeStore<E, M> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
    ) -> Result<Self, GrowFailed> {
        let header = EdgeHeaderV1::new(elem_capacity, segment_count, segment_size, E::BYTES as u32);
        let counts = SegmentEdgeCountsStore::new(counts)?;
        for _ in 0..u64::from(header.segment_count).saturating_mul(2) {
            counts.push(SegmentEdgeCounts {
                actual: 0,
                total: 0,
                tombstone: 0,
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
            log,
            span_meta,
            free_spans,
        })
    }

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
        Ok(Self {
            counts,
            edges,
            log,
            span_meta,
            free_spans,
        })
    }

    pub fn header(&self) -> EdgeHeaderV1 {
        self.edges
            .header()
            .expect("edge header is valid after init")
    }

    pub fn counts_store(&self) -> &SegmentEdgeCountsStore<E, M> {
        &self.counts
    }

    pub fn span_meta_store(&self) -> &SegmentSpanMetaStore<M> {
        &self.span_meta
    }

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

    fn log_layout(&self) -> LogLayout {
        self.log.header().into()
    }

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
        if len == 0 {
            return Ok(self.header().elem_capacity);
        }
        if let Some(span) = self.free_spans.take_best_fit(len).map_err(|_| GrowFailed {
            current_size: 0,
            delta: 0,
        })? {
            return Ok(span.start_slot);
        }

        let start = self.header().elem_capacity;
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

    pub fn read_slot(&self, slot: u64) -> E {
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            self.edges.read_slot(slot, &mut buf[..E::BYTES]);
            E::read_from(&buf[..E::BYTES])
        } else {
            let mut buf = vec![0u8; E::BYTES];
            self.edges.read_slot(slot, &mut buf);
            E::read_from(&buf)
        }
    }

    pub fn write_slot(&self, slot: u64, edge: E) -> Result<(), GrowFailed> {
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut buf[..E::BYTES]);
            self.edges.write_slot(slot, &buf[..E::BYTES])
        } else {
            let mut buf = vec![0u8; E::BYTES];
            edge.write_to(&mut buf);
            self.edges.write_slot(slot, &buf)
        }
    }

    pub(crate) fn collect_out_edges<V, A>(
        &self,
        vertices: &A,
        vid: VertexId,
    ) -> Result<Vec<E>, &'static str>
    where
        V: LaraVertex,
        A: VertexAccess<V>,
    {
        let edge_layout = self.edge_layout();
        let vidx = vertex_index(vid);
        if vidx >= vertices.len() as usize {
            return Err("vertex out of range");
        }
        let v = vertices.get(vidx as u64);
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
        let filler = if slab_count > 0 {
            out[0]
        } else {
            self.read_log_head_edge(leaf_segment(vid, edge_layout.segment_size), v.log_head())?
        };
        out.resize(degree, filler);

        let leaf = leaf_segment(vid, edge_layout.segment_size);
        let mut log_i = v.log_head();
        for offset in (0..log_count).rev() {
            if log_i < 0 {
                return Err("log chain short");
            }
            let (prev, edge) = self.read_log_edge(leaf, log_i as u32);
            out[slab_count + offset] = edge;
            log_i = prev;
        }
        Ok(out)
    }

    fn read_log_head_edge(&self, leaf: u32, log_head: i32) -> Result<E, &'static str> {
        if log_head < 0 {
            return Err("log chain short");
        }
        let (_prev, edge) = self.read_log_edge(leaf, log_head as u32);
        Ok(edge)
    }

    fn read_log_edge(&self, leaf: u32, log_idx: u32) -> (i32, E) {
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            let (prev, _src) = self.log.read_entry(leaf, log_idx, &mut buf[..E::BYTES]);
            (prev, E::read_from(&buf[..E::BYTES]))
        } else {
            let mut buf = vec![0u8; E::BYTES];
            let (prev, _src) = self.log.read_entry(leaf, log_idx, &mut buf);
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
        V: LaraVertex,
        A: VertexAccess<V>,
    {
        let edge_layout = self.edge_layout();
        let log_layout = self.log_layout();
        let vidx = vertex_index(vid);
        if vidx >= vertices.len() as usize {
            return Err("vertex out of range");
        }
        let v = vertices.get(vidx as u64);
        let loc = v.base_slot_start().saturating_add(u64::from(v.degree()));
        let location =
            if self.have_space_on_slab(vertices, vidx, &v, loc, edge_layout.elem_capacity) {
                self.write_slot(loc, edge)
                    .map_err(|_| "write edge slot failed")?;
                vertices.set(vidx as u64, &v.with_degree(v.degree() + 1));
                InsertLocation::Slab
            } else {
                self.insert_into_log_with_layout(
                    &edge_layout,
                    &log_layout,
                    vertices,
                    vid,
                    v,
                    edge,
                )?;
                InsertLocation::Log
            };
        self.edges
            .set_num_edges(edge_layout.num_edges.saturating_add(1));
        self.bump_counts_leaf_with_layout(&edge_layout, vid, 1, 0, 0)?;
        Ok(location)
    }

    fn insert_into_log_with_layout<V, A>(
        &self,
        edge_layout: &EdgeLayout,
        log_layout: &LogLayout,
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
        let idx = self.log.read_idx(leaf);
        if idx < 0 || idx >= log_layout.max_log_entries as i32 {
            return Err("segment log full");
        }
        let src = i32::try_from(u32::from(vid)).map_err(|_| "vertex id exceeds i32")?;
        if E::BYTES <= INLINE_EDGE_BYTES {
            let mut payload = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut payload[..E::BYTES]);
            self.log
                .write_entry(leaf, idx as u32, v.log_head(), src, &payload[..E::BYTES])
                .map_err(|_| "write log failed")?;
        } else {
            let mut payload = vec![0u8; E::BYTES];
            edge.write_to(&mut payload);
            self.log
                .write_entry(leaf, idx as u32, v.log_head(), src, &payload)
                .map_err(|_| "write log failed")?;
        }
        self.log.write_idx(leaf, idx + 1);
        vertices.set(
            u32::from(vid) as u64,
            &v.with_log_head(idx).with_degree(v.degree() + 1),
        );
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
        let current_leaf = (vidx as u32) / edge_layout.segment_size.max(1);
        let next = if vidx + 1 < vertices.len() as usize
            && ((vidx + 1) as u32) / edge_layout.segment_size.max(1) == current_leaf
        {
            vertices.get((vidx + 1) as u64).base_slot_start()
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

    pub(crate) fn have_space_on_slab<V, A>(
        &self,
        vertices: &A,
        vidx: usize,
        v: &V,
        loc: u64,
        elem_capacity: u64,
    ) -> bool
    where
        V: LaraVertex,
        A: VertexAccess<V>,
    {
        if v.span_capacity() > 0 {
            return loc
                < v.base_slot_start()
                    .saturating_add(u64::from(v.span_capacity()));
        }
        let current_leaf = (vidx as u32) / self.header().segment_size.max(1);
        if vidx + 1 < vertices.len() as usize
            && ((vidx + 1) as u32) / self.header().segment_size.max(1) == current_leaf
        {
            vertices.get((vidx + 1) as u64).base_slot_start() > loc
        } else if current_leaf < self.header().segment_count {
            let c = self
                .counts
                .get(u64::from(current_leaf + self.header().segment_count));
            loc < vertices
                .get(vidx as u64)
                .base_slot_start()
                .saturating_add(c.total.max(0) as u64)
        } else {
            loc < elem_capacity
        }
    }

    fn bump_counts_leaf_with_layout(
        &self,
        edge_layout: &EdgeLayout,
        vid: VertexId,
        d_actual: i64,
        d_total: i64,
        d_tombstone: i64,
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
                c.tombstone += d_tombstone;
            } else {
                let left = self.counts.get((idx * 2) as u64);
                let right = self.counts.get((idx * 2 + 1) as u64);
                c = SegmentEdgeCounts {
                    actual: left.actual + right.actual,
                    total: left.total + right.total,
                    tombstone: left.tombstone + right.tombstone,
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

    pub(crate) fn log_is_full(&self, vid: VertexId) -> bool {
        let edge_layout = self.edge_layout();
        let log_layout = self.log_layout();
        let leaf = leaf_segment(vid, edge_layout.segment_size);
        self.log.read_idx(leaf) >= log_layout.max_log_entries as i32
    }

    pub(crate) fn log_fill_ratio(&self, segment: SegmentId) -> f64 {
        let log_layout = self.log_layout();
        let idx = self.log.read_idx(u32::from(segment)).max(0) as f64;
        let capacity = log_layout.max_log_entries.max(1) as f64;
        idx / capacity
    }

    pub fn release_log_segment(&self, leaf_segment: SegmentId) -> Result<(), GrowFailed> {
        self.log.release_segment(u32::from(leaf_segment))
    }

    pub(crate) fn set_num_edges(&self, n: u64) {
        self.edges.set_num_edges(n);
    }

    pub(crate) fn set_elem_capacity(&self, n: u64) -> Result<(), GrowFailed> {
        self.edges.set_elem_capacity(n)
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
fn leaf_segment(vid: VertexId, segment_size: u32) -> u32 {
    u32::from(vid) / segment_size.max(1)
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
            })
            .unwrap();
        vertices
            .push(Vertex {
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
            2,
            1,
            2,
        )
        .unwrap();
        assert_eq!(edges.span_meta_store().len(), 1);

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert_eq!(
            edges
                .collect_out_edges(&vertices, VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
        assert_eq!(vertices.get(0).degree, 2);
        assert!(vertices.get(0).log_head >= 0);
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
            })
            .unwrap();
        vertices
            .push(Vertex {
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
            2,
        )
        .unwrap();

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert_eq!(vertices.get(0).degree, 2);
        assert_eq!(vertices.get(0).log_head, -1);
        assert_eq!(
            edges
                .collect_out_edges(&vertices, VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
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
            2,
        )
        .unwrap();

        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(10))
            .unwrap();
        edges
            .insert_edge(&vertices, VertexId::from(0), TestEdge(11))
            .unwrap();

        assert!(CAPACITY_READS.load(Ordering::SeqCst) >= 2);
        assert_eq!(vertices.get(0).degree, 2);
        assert_eq!(vertices.get(0).log_head, -1);
        assert_eq!(
            edges
                .collect_out_edges(&vertices, VertexId::from(0))
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
            1,
        )
        .unwrap();
        edges.write_slot(0, TestEdge(10)).unwrap();
        edges.write_slot(1, TestEdge(11)).unwrap();

        assert_eq!(
            edges
                .collect_out_edges(&vertices, VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
    }
}
