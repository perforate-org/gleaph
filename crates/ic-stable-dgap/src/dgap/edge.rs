pub mod counts;
mod edges;
mod log;

use crate::{
    GrowFailed, SegmentId, VertexId,
    traits::{CsrEdge, CsrVertex},
};
use counts::{EdgePmaCountsStride, SegmentEdgeCounts, SegmentEdgeCountsStore};
use edges::EdgeSlabStore;
pub use edges::HeaderV1 as EdgeHeaderV1;
use ic_stable_structures::Memory;
pub use log::HeaderV1 as LogHeaderV1;
use log::LogStore;
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
    LogLayoutMismatch,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Counts(e) => write!(f, "counts init failed: {e}"),
            Self::Edges(e) => write!(f, "edge slab init failed: {e}"),
            Self::Log(e) => write!(f, "log init failed: {e}"),
            Self::LogLayoutMismatch => write!(f, "log layout does not match edge store layout"),
        }
    }
}

impl std::error::Error for InitError {}

pub(crate) trait VertexAccess<V: CsrVertex> {
    fn len(&self) -> u64;
    fn get(&self, index: u64) -> V;
    fn set(&self, index: u64, item: &V);
}

#[derive(Clone, Debug)]
pub struct EdgeStore<E: CsrEdge, MC: Memory, ME: Memory, ML: Memory> {
    counts: SegmentEdgeCountsStore<E, MC>,
    edges: EdgeSlabStore<E, ME>,
    log: LogStore<E, ML>,
}

impl<E: CsrEdge + EdgePmaCountsStride, MC: Memory, ME: Memory, ML: Memory>
    EdgeStore<E, MC, ME, ML>
{
    pub fn new(
        counts: MC,
        edges: ME,
        log: ML,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
    ) -> Result<Self, GrowFailed> {
        let header = EdgeHeaderV1::new(
            elem_capacity,
            segment_count,
            segment_size,
            E::EDGE_BYTES as u32,
        );
        let counts = SegmentEdgeCountsStore::new(counts)?;
        for _ in 0..u64::from(header.segment_count).saturating_mul(2) {
            counts.push(SegmentEdgeCounts {
                actual: 0,
                total: 0,
                tombstone: 0,
            })?;
        }
        let log_header = LogHeaderV1::new(header.segment_count, header.edge_stride);
        let edges = EdgeSlabStore::new(edges, header)?;
        let log = LogStore::new(log, log_header)?;
        Ok(Self { counts, edges, log })
    }

    pub fn init(counts: MC, edges: ME, log: ML) -> Result<Self, InitError> {
        let counts = SegmentEdgeCountsStore::init(counts).map_err(InitError::Counts)?;
        let edges = EdgeSlabStore::init(edges).map_err(InitError::Edges)?;
        let header = edges.header().map_err(InitError::Edges)?;
        let log = LogStore::init(log).map_err(InitError::Log)?;
        let log_header = log.header();
        if log_header.segment_count != header.segment_count {
            return Err(InitError::LogLayoutMismatch);
        }
        Ok(Self { counts, edges, log })
    }

    pub fn header(&self) -> EdgeHeaderV1 {
        self.edges
            .header()
            .expect("edge header is valid after init")
    }

    pub fn counts_store(&self) -> &SegmentEdgeCountsStore<E, MC> {
        &self.counts
    }

    fn edge_layout(&self) -> EdgeLayout {
        self.header().into()
    }

    fn log_layout(&self) -> LogLayout {
        self.log.header().into()
    }

    pub fn into_memories(self) -> (MC, ME, ML) {
        (
            self.counts.into_memory(),
            self.edges.into_memory(),
            self.log.into_memory(),
        )
    }

    pub fn read_slot(&self, slot: u64) -> E {
        if E::EDGE_BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            self.edges.read_slot(slot, &mut buf[..E::EDGE_BYTES]);
            E::read_from(&buf[..E::EDGE_BYTES])
        } else {
            let mut buf = vec![0u8; E::EDGE_BYTES];
            self.edges.read_slot(slot, &mut buf);
            E::read_from(&buf)
        }
    }

    pub fn write_slot(&self, slot: u64, edge: E) -> Result<(), GrowFailed> {
        if E::EDGE_BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut buf[..E::EDGE_BYTES]);
            self.edges.write_slot(slot, &buf[..E::EDGE_BYTES])
        } else {
            let mut buf = vec![0u8; E::EDGE_BYTES];
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
        V: CsrVertex,
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
        if E::EDGE_BYTES <= INLINE_EDGE_BYTES {
            let mut buf = [0u8; INLINE_EDGE_BYTES];
            let (prev, _src) = self
                .log
                .read_entry(leaf, log_idx, &mut buf[..E::EDGE_BYTES]);
            (prev, E::read_from(&buf[..E::EDGE_BYTES]))
        } else {
            let mut buf = vec![0u8; E::EDGE_BYTES];
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
        V: CsrVertex,
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
        let location = if self.have_space_on_slab(vertices, vidx, loc, edge_layout.elem_capacity) {
            self.write_slot(loc, edge)
                .map_err(|_| "write edge slot failed")?;
            vertices.set(vidx as u64, &v.with_degree(v.degree() + 1));
            InsertLocation::Slab
        } else {
            self.insert_into_log_with_layout(&edge_layout, &log_layout, vertices, vid, v, edge)?;
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
        if E::EDGE_BYTES <= INLINE_EDGE_BYTES {
            let mut payload = [0u8; INLINE_EDGE_BYTES];
            edge.write_to(&mut payload[..E::EDGE_BYTES]);
            self.log
                .write_entry(
                    leaf,
                    idx as u32,
                    v.log_head(),
                    src,
                    &payload[..E::EDGE_BYTES],
                )
                .map_err(|_| "write log failed")?;
        } else {
            let mut payload = vec![0u8; E::EDGE_BYTES];
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
        let next = if vidx + 1 < vertices.len() as usize {
            vertices.get((vidx + 1) as u64).base_slot_start()
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
        loc: u64,
        elem_capacity: u64,
    ) -> bool
    where
        V: CsrVertex,
        A: VertexAccess<V>,
    {
        if vidx + 1 >= vertices.len() as usize {
            loc < elem_capacity
        } else {
            vertices.get((vidx + 1) as u64).base_slot_start() > loc
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
