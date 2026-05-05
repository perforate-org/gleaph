pub mod edge;
pub mod maintenance;
pub mod vertex;

use crate::{
    GrowFailed, SegmentId, VertexId,
    dgap::{
        edge::{
            EdgeHeaderV1, EdgeStore, InsertLocation, VertexAccess,
            counts::{EdgePmaCountsStride, SegmentEdgeCounts},
        },
        vertex::VertexStore,
    },
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;
use std::{fmt, marker::PhantomData};

const ROOT_UPPER_DENSITY: f64 = 0.75;
const LEAF_UPPER_DENSITY: f64 = 1.0;

#[derive(Debug)]
struct RebalanceCache<E> {
    edges: Vec<E>,
    offsets: Vec<usize>,
}

#[derive(Clone, Copy, Debug)]
struct DgapLayout {
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
    tree_height: u32,
}

impl From<EdgeHeaderV1> for DgapLayout {
    fn from(header: EdgeHeaderV1) -> Self {
        Self {
            elem_capacity: header.elem_capacity,
            segment_count: header.segment_count,
            segment_size: header.segment_size,
            tree_height: header.tree_height,
        }
    }
}

pub(super) struct InsertOutcome {
    pub segment: SegmentId,
    pub inserted_into_log: bool,
}

impl<E> RebalanceCache<E> {
    fn vertex_edges(&self, offset: usize) -> &[E] {
        &self.edges[self.offsets[offset]..self.offsets[offset + 1]]
    }

    fn total_edges(&self) -> u64 {
        self.edges.len() as u64
    }
}

impl<V: CsrVertex, M: Memory> VertexAccess<V> for VertexStore<V, M> {
    fn len(&self) -> u64 {
        self.len()
    }

    fn get(&self, index: u64) -> V {
        self.get(index)
    }

    fn set(&self, index: u64, item: &V) {
        self.set(index, item);
    }
}

#[derive(Debug)]
pub enum InitError {
    Vertices(vertex::InitError),
    Edges(edge::InitError),
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vertices(e) => write!(f, "vertex init failed: {e}"),
            Self::Edges(e) => write!(f, "edge init failed: {e}"),
        }
    }
}

impl std::error::Error for InitError {}

#[derive(Clone, Debug)]
pub struct Dgap<E, V, MV, MC, ME, ML, MS, MF>
where
    E: CsrEdge + EdgePmaCountsStride,
    V: CsrVertex,
    MV: Memory,
    MC: Memory,
    ME: Memory,
    ML: Memory,
    MS: Memory,
    MF: Memory,
{
    pub(super) vertices: VertexStore<V, MV>,
    pub(super) edges: EdgeStore<E, MC, ME, ML, MS, MF>,
    _marker: PhantomData<(E, V)>,
}

impl<E, V, MV, MC, ME, ML, MS, MF> Dgap<E, V, MV, MC, ME, ML, MS, MF>
where
    E: CsrEdge + EdgePmaCountsStride,
    V: CsrVertex,
    MV: Memory,
    MC: Memory,
    ME: Memory,
    ML: Memory,
    MS: Memory,
    MF: Memory,
{
    pub fn new(
        vertices: MV,
        counts: MC,
        edges: ME,
        log: ML,
        span_meta: MS,
        free_spans: MF,
        elem_capacity: u64,
        segment_count: u32,
        segment_size: u32,
    ) -> Result<Self, GrowFailed> {
        Ok(Self {
            vertices: VertexStore::new(vertices)?,
            edges: EdgeStore::new(
                counts,
                edges,
                log,
                span_meta,
                free_spans,
                elem_capacity,
                segment_count,
                segment_size,
            )?,
            _marker: PhantomData,
        })
    }

    pub fn init(
        vertices: MV,
        counts: MC,
        edges: ME,
        log: ML,
        span_meta: MS,
        free_spans: MF,
    ) -> Result<Self, InitError> {
        Ok(Self {
            vertices: VertexStore::init(vertices).map_err(InitError::Vertices)?,
            edges: EdgeStore::init(counts, edges, log, span_meta, free_spans)
                .map_err(InitError::Edges)?,
            _marker: PhantomData,
        })
    }

    pub fn vertices(&self) -> &VertexStore<V, MV> {
        &self.vertices
    }

    pub fn edges(&self) -> &EdgeStore<E, MC, ME, ML, MS, MF> {
        &self.edges
    }

    pub fn into_memories(self) -> (MV, MC, ME, ML, MS, MF) {
        let (counts, edges, log, span_meta, free_spans) = self.edges.into_memories();
        (
            self.vertices.into_memory(),
            counts,
            edges,
            log,
            span_meta,
            free_spans,
        )
    }

    pub fn push_vertex(&self, vertex: V) -> Result<VertexId, GrowFailed> {
        let id = VertexId::from(u32::try_from(self.vertices.len()).expect("too many vertices"));
        self.vertices.push(vertex)?;
        let layout = self.layout();
        self.recount_segment_counts_with_layout(&layout, layout.elem_capacity);
        Ok(id)
    }

    pub fn insert_edge(&self, src: VertexId, edge: E) -> Result<(), &'static str> {
        let _ = self.insert_edge_raw(src, edge)?;
        self.rebalance_after_insert(src)
    }

    pub fn collect_out_edges(&self, src: VertexId) -> Result<Vec<E>, &'static str> {
        self.edges.collect_out_edges(&self.vertices, src)
    }

    pub fn resize(&self) -> Result<(), GrowFailed> {
        let layout = self.layout();
        let vertex_len = self.vertices.len();
        if vertex_len == 0 {
            self.edges
                .set_elem_capacity(layout.elem_capacity.saturating_mul(2).max(1))?;
            return Ok(());
        }

        let cache = self.collect_rebalance_cache(0, vertex_len);
        let total_edges = cache.total_edges();

        let new_capacity = layout
            .elem_capacity
            .saturating_mul(2)
            .max(total_edges.saturating_add(vertex_len));
        self.edges.set_elem_capacity(new_capacity)?;

        let positions =
            self.calculate_positions(0, vertex_len, new_capacity.saturating_sub(total_edges));
        for vidx in 0..vertex_len as usize {
            let neighborhood = cache.vertex_edges(vidx);
            let start = positions[vidx];
            for (i, edge) in neighborhood.iter().copied().enumerate() {
                self.edges.write_slot(start + i as u64, edge)?;
            }
            let v = self.vertices.get(vidx as u64);
            self.vertices.set(
                vidx as u64,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
                    .with_log_head(-1),
            );
        }

        self.edges.set_num_edges(total_edges);
        self.recount_segment_counts_with_layout(&layout, new_capacity);
        for leaf in 0..layout.segment_count {
            self.edges.release_log_segment(SegmentId::from(leaf))?;
        }
        Ok(())
    }

    pub(super) fn insert_edge_raw(
        &self,
        src: VertexId,
        edge: E,
    ) -> Result<InsertOutcome, &'static str> {
        if self.edges.log_is_full(src) {
            self.rebalance_leaf_for(src)
                .map_err(|_| "rebalance failed")?;
        }
        let layout = self.layout();
        let segment = self.segment_for_vertex_id_with_layout(&layout, src);
        let location = match self.edges.insert_edge(&self.vertices, src, edge) {
            Ok(location) => location,
            Err("segment log full") => {
                self.rebalance_leaf_for(src)
                    .map_err(|_| "rebalance failed")?;
                self.edges.insert_edge(&self.vertices, src, edge)?
            }
            Err(e) => return Err(e),
        };
        Ok(InsertOutcome {
            segment,
            inserted_into_log: location == InsertLocation::Log,
        })
    }

    fn rebalance_after_insert(&self, src: VertexId) -> Result<(), &'static str> {
        let layout = self.layout();
        let current_leaf = self.leaf_for_vertex_with_layout(&layout, u64::from(u32::from(src)));
        let leaf_counts = self
            .edge_counts_for_leaves_with_layout(&layout, current_leaf, current_leaf + 1)
            .ok_or("segment counts out of range")?;
        if density(leaf_counts) < LEAF_UPPER_DENSITY {
            return Ok(());
        }

        let mut window = current_leaf + u64::from(layout.segment_count);
        let mut height = 0u32;
        let mut chosen: Option<(u64, u64, SegmentEdgeCounts)> = None;
        while window > 0 {
            window /= 2;
            height += 1;
            let window_size = u64::from(layout.segment_size).saturating_mul(1u64 << height);
            let left_vertex = (u64::from(u32::from(src)) / window_size) * window_size;
            let right_vertex = left_vertex
                .saturating_add(window_size)
                .min(self.vertices.len());
            let left_leaf = self.leaf_for_vertex_with_layout(&layout, left_vertex);
            let right_leaf = self
                .leaf_end_for_vertex_with_layout(&layout, right_vertex)
                .max(left_leaf + 1);
            let counts = self
                .edge_counts_for_leaves_with_layout(&layout, left_leaf, right_leaf)
                .ok_or("segment counts out of range")?;
            let up_height =
                LEAF_UPPER_DENSITY - f64::from(height) * self.delta_up_with_layout(&layout);
            if density(counts) < up_height {
                chosen = Some((left_vertex, right_vertex, counts));
                break;
            }
        }

        if let Some((left_vertex, right_vertex, counts)) = chosen {
            let leaf_density = density(
                self.edge_counts_for_leaves_with_layout(&layout, current_leaf, current_leaf + 1)
                    .ok_or("segment counts out of range")?,
            );
            if leaf_density >= LEAF_UPPER_DENSITY {
                self.rebalance_weighted_with_layout(&layout, left_vertex, right_vertex, counts)
                    .map_err(|_| "rebalance failed")?;
            }
        } else if density(
            self.edge_counts_for_leaves_with_layout(&layout, current_leaf, current_leaf + 1)
                .ok_or("segment counts out of range")?,
        ) >= LEAF_UPPER_DENSITY
        {
            self.local_resize_segment_with_layout(&layout, current_leaf as u32)
                .map_err(|_| "resize failed")?;
        }

        Ok(())
    }

    pub(super) fn rebalance_leaf_for(&self, src: VertexId) -> Result<(), GrowFailed> {
        let layout = self.layout();
        self.rebalance_leaf_segment_with_layout(
            &layout,
            self.segment_for_vertex_id_with_layout(&layout, src),
        )
    }

    fn rebalance_leaf_segment_with_layout(
        &self,
        layout: &DgapLayout,
        segment: SegmentId,
    ) -> Result<(), GrowFailed> {
        let left_vertex =
            u64::from(u32::from(segment)).saturating_mul(u64::from(layout.segment_size));
        let right_vertex = left_vertex
            .saturating_add(u64::from(layout.segment_size))
            .min(self.vertices.len());
        let left_leaf = self.leaf_for_vertex_with_layout(layout, left_vertex);
        let right_leaf = self
            .leaf_end_for_vertex_with_layout(layout, right_vertex)
            .max(left_leaf + 1);
        let counts = self
            .edge_counts_for_leaves_with_layout(layout, left_leaf, right_leaf)
            .unwrap_or(SegmentEdgeCounts {
                actual: 0,
                total: 0,
                tombstone: 0,
            });
        self.rebalance_weighted_with_layout(layout, left_vertex, right_vertex, counts)
    }

    pub(super) fn rebalance_dirty_segment(&self, segment: SegmentId) -> Result<(), GrowFailed> {
        let layout = self.layout();
        let current_leaf = u64::from(u32::from(segment));
        let leaf_counts = self
            .edge_counts_for_leaves_with_layout(&layout, current_leaf, current_leaf + 1)
            .unwrap_or(SegmentEdgeCounts {
                actual: 0,
                total: 0,
                tombstone: 0,
            });
        if density(leaf_counts) < LEAF_UPPER_DENSITY {
            return self.rebalance_leaf_segment_with_layout(&layout, segment);
        }

        let src_vertex =
            u64::from(u32::from(segment)).saturating_mul(u64::from(layout.segment_size));
        let mut window = current_leaf + u64::from(layout.segment_count);
        let mut height = 0u32;
        let mut chosen: Option<(u64, u64, SegmentEdgeCounts)> = None;
        while window > 0 {
            window /= 2;
            height += 1;
            let window_size = u64::from(layout.segment_size).saturating_mul(1u64 << height);
            let left_vertex = (src_vertex / window_size) * window_size;
            let right_vertex = left_vertex
                .saturating_add(window_size)
                .min(self.vertices.len());
            let left_leaf = self.leaf_for_vertex_with_layout(&layout, left_vertex);
            let right_leaf = self
                .leaf_end_for_vertex_with_layout(&layout, right_vertex)
                .max(left_leaf + 1);
            let Some(counts) =
                self.edge_counts_for_leaves_with_layout(&layout, left_leaf, right_leaf)
            else {
                continue;
            };
            let up_height =
                LEAF_UPPER_DENSITY - f64::from(height) * self.delta_up_with_layout(&layout);
            if density(counts) < up_height {
                chosen = Some((left_vertex, right_vertex, counts));
                break;
            }
        }

        if let Some((left_vertex, right_vertex, counts)) = chosen {
            self.rebalance_weighted_with_layout(&layout, left_vertex, right_vertex, counts)
        } else {
            self.local_resize_segment_with_layout(&layout, u32::from(segment))
        }
    }

    pub(super) fn rebalance_maintenance_segment(&self, segment: SegmentId) -> bool {
        let layout = self.layout();
        self.segment_has_log_with_layout(&layout, segment)
            || self
                .edge_counts_for_leaves_with_layout(
                    &layout,
                    u64::from(u32::from(segment)),
                    u64::from(u32::from(segment)) + 1,
                )
                .is_some_and(|counts| density(counts) >= LEAF_UPPER_DENSITY)
    }

    pub(super) fn deferred_mark_priority(
        &self,
        segment: SegmentId,
        inserted_into_log: bool,
        leaf_dirty_density: f64,
        log_urgent_ratio: f64,
    ) -> MarkPriority {
        let layout = self.layout();
        let density = self
            .edge_counts_for_leaves_with_layout(
                &layout,
                u64::from(u32::from(segment)),
                u64::from(u32::from(segment)) + 1,
            )
            .map(density)
            .unwrap_or(0.0);
        let log_fill = self.edges.log_fill_ratio(segment);
        if density >= LEAF_UPPER_DENSITY || log_fill >= log_urgent_ratio {
            MarkPriority::Urgent(segment)
        } else if inserted_into_log || density >= leaf_dirty_density {
            MarkPriority::Dirty(segment)
        } else {
            MarkPriority::Clean
        }
    }

    fn segment_has_log_with_layout(&self, layout: &DgapLayout, segment: SegmentId) -> bool {
        let start = u64::from(u32::from(segment)).saturating_mul(u64::from(layout.segment_size));
        let end = start
            .saturating_add(u64::from(layout.segment_size))
            .min(self.vertices.len());
        (start..end).any(|vid| self.vertices.get(vid).log_head() >= 0)
    }

    fn rebalance_weighted_with_layout(
        &self,
        layout: &DgapLayout,
        start_vertex: u64,
        end_vertex: u64,
        counts: SegmentEdgeCounts,
    ) -> Result<(), GrowFailed> {
        if start_vertex >= end_vertex {
            return Ok(());
        }
        let from = self.vertices.get(start_vertex).base_slot_start();
        let to = if end_vertex >= self.vertices.len() {
            layout.elem_capacity
        } else {
            self.vertices.get(end_vertex).base_slot_start()
        };
        let total_space = if counts.total > 0 {
            counts.total as u64
        } else {
            to.saturating_sub(from)
        };
        let used_space = if counts.actual >= 0 {
            counts.actual as u64
        } else {
            0
        };
        let gaps = total_space.saturating_sub(used_space);
        let positions = self.calculate_positions(start_vertex, end_vertex, gaps);

        let cache = self.collect_rebalance_cache(start_vertex, end_vertex);
        for offset in 0..(end_vertex - start_vertex) as usize {
            let neighborhood = cache.vertex_edges(offset);
            let vid = start_vertex + offset as u64;
            let start = positions[offset];
            for (i, edge) in neighborhood.iter().copied().enumerate() {
                self.edges.write_slot(start + i as u64, edge)?;
            }
            let v = self.vertices.get(vid);
            self.vertices.set(
                vid,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
                    .with_log_head(-1),
            );
        }

        let start_leaf = self.leaf_for_vertex_with_layout(layout, start_vertex);
        let end_leaf = self
            .leaf_end_for_vertex_with_layout(layout, end_vertex)
            .max(start_leaf + 1);
        for leaf in start_leaf..end_leaf.min(u64::from(layout.segment_count)) {
            self.edges
                .release_log_segment(SegmentId::from(leaf as u32))?;
        }
        self.recount_segment_counts_range_with_layout(
            layout,
            start_leaf,
            end_leaf,
            layout.elem_capacity,
        );
        Ok(())
    }

    fn local_resize_segment_with_layout(
        &self,
        layout: &DgapLayout,
        segment: u32,
    ) -> Result<(), GrowFailed> {
        if segment >= layout.segment_count {
            return Ok(());
        }

        let start_vertex = u64::from(segment).saturating_mul(u64::from(layout.segment_size.max(1)));
        let end_vertex = start_vertex
            .saturating_add(u64::from(layout.segment_size.max(1)))
            .min(self.vertices.len());
        if start_vertex >= end_vertex {
            return Ok(());
        }

        let cache = self.collect_rebalance_cache(start_vertex, end_vertex);
        let used_space = cache.total_edges();
        let old_leaf = self
            .edges
            .counts_store()
            .get(u64::from(segment + layout.segment_count));
        let old_span = old_leaf.total.max(0) as u64;
        let vertex_count = end_vertex.saturating_sub(start_vertex);
        let new_span = old_span
            .saturating_mul(2)
            .max(used_space.saturating_add(vertex_count))
            .max(1);
        let old_start = self.vertices.get(start_vertex).base_slot_start();
        let new_start = self.edges.allocate_span(new_span)?;

        let gaps = new_span.saturating_sub(used_space);
        let positions = self.calculate_positions_from(start_vertex, end_vertex, new_start, gaps);
        for offset in 0..(end_vertex - start_vertex) as usize {
            let neighborhood = cache.vertex_edges(offset);
            let vid = start_vertex + offset as u64;
            let start = positions[offset];
            for (i, edge) in neighborhood.iter().copied().enumerate() {
                self.edges.write_slot(start + i as u64, edge)?;
            }
            let v = self.vertices.get(vid);
            self.vertices.set(
                vid,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
                    .with_log_head(-1),
            );
        }

        self.edges
            .set_segment_physical_start(SegmentId::from(segment), new_start)?;
        self.edges.release_log_segment(SegmentId::from(segment))?;
        self.edges.release_span(old_start, old_span)?;
        self.update_leaf_count_and_ancestors(
            layout,
            segment,
            SegmentEdgeCounts {
                actual: used_space as i64,
                total: new_span as i64,
                tombstone: 0,
            },
        );
        Ok(())
    }

    fn collect_rebalance_cache(&self, start_vertex: u64, end_vertex: u64) -> RebalanceCache<E> {
        let vertex_count = end_vertex.saturating_sub(start_vertex) as usize;
        let mut total_edges = 0usize;
        for vid in start_vertex..end_vertex {
            total_edges = total_edges.saturating_add(self.vertices.get(vid).degree() as usize);
        }

        let mut edges = Vec::with_capacity(total_edges);
        let mut offsets = Vec::with_capacity(vertex_count + 1);
        offsets.push(0);
        for vid in start_vertex..end_vertex {
            let out = self
                .edges
                .collect_out_edges(&self.vertices, VertexId::from(vid as u32))
                .expect("DGAP log chains are valid before rebalance");
            edges.extend(out);
            offsets.push(edges.len());
        }

        RebalanceCache { edges, offsets }
    }

    fn layout(&self) -> DgapLayout {
        self.edges.header().into()
    }

    fn leaf_for_vertex_with_layout(&self, layout: &DgapLayout, vertex: u64) -> u64 {
        let leaf = vertex / u64::from(layout.segment_size.max(1));
        leaf.min(u64::from(layout.segment_count.saturating_sub(1)))
    }

    fn leaf_end_for_vertex_with_layout(&self, layout: &DgapLayout, vertex: u64) -> u64 {
        if vertex >= self.vertices.len() {
            u64::from(layout.segment_count)
        } else {
            vertex / u64::from(layout.segment_size.max(1))
        }
    }

    fn edge_counts_for_leaves_with_layout(
        &self,
        layout: &DgapLayout,
        start_leaf: u64,
        end_leaf: u64,
    ) -> Option<SegmentEdgeCounts> {
        if start_leaf >= end_leaf || end_leaf > u64::from(layout.segment_count) {
            return None;
        }
        let mut out = SegmentEdgeCounts {
            actual: 0,
            total: 0,
            tombstone: 0,
        };
        for leaf in start_leaf..end_leaf {
            let c = self
                .edges
                .counts_store()
                .get(leaf + u64::from(layout.segment_count));
            out.actual += c.actual;
            out.total += c.total;
            out.tombstone += c.tombstone;
        }
        Some(out)
    }

    fn delta_up_with_layout(&self, layout: &DgapLayout) -> f64 {
        let tree_height = layout.tree_height.max(1);
        (LEAF_UPPER_DENSITY - ROOT_UPPER_DENSITY) / f64::from(tree_height)
    }

    fn recount_segment_counts_with_layout(&self, layout: &DgapLayout, elem_capacity: u64) {
        for i in 0..u64::from(layout.segment_count) * 2 {
            self.edges.set_count(
                i,
                SegmentEdgeCounts {
                    actual: 0,
                    total: 0,
                    tombstone: 0,
                },
            );
        }
        for leaf in 0..layout.segment_count {
            let start_vid = leaf.saturating_mul(layout.segment_size);
            let end_vid =
                ((leaf + 1).saturating_mul(layout.segment_size)).min(self.vertices.len() as u32);
            let mut actual = 0i64;
            for vid in start_vid..end_vid {
                actual += i64::from(self.vertices.get(u64::from(vid)).degree());
            }
            let start_slot = if start_vid < self.vertices.len() as u32 {
                self.vertices.get(u64::from(start_vid)).base_slot_start()
            } else {
                elem_capacity
            };
            let next_slot = if leaf + 1 >= layout.segment_count {
                elem_capacity
            } else {
                let next_vid = ((leaf + 1).saturating_mul(layout.segment_size))
                    .min(self.vertices.len() as u32);
                if next_vid < self.vertices.len() as u32 {
                    self.vertices.get(u64::from(next_vid)).base_slot_start()
                } else {
                    elem_capacity
                }
            };
            self.edges.set_count(
                u64::from(leaf + layout.segment_count),
                SegmentEdgeCounts {
                    actual,
                    total: next_slot.saturating_sub(start_slot) as i64,
                    tombstone: 0,
                },
            );
        }
        for idx in (1..layout.segment_count).rev() {
            let left = self.edges.counts_store().get(u64::from(idx * 2));
            let right = self.edges.counts_store().get(u64::from(idx * 2 + 1));
            self.edges.set_count(
                u64::from(idx),
                SegmentEdgeCounts {
                    actual: left.actual + right.actual,
                    total: left.total + right.total,
                    tombstone: left.tombstone + right.tombstone,
                },
            );
        }
    }

    fn recount_segment_counts_range_with_layout(
        &self,
        layout: &DgapLayout,
        start_leaf: u64,
        end_leaf: u64,
        elem_capacity: u64,
    ) {
        if layout.segment_count == 0 {
            return;
        }
        let start = start_leaf.min(u64::from(layout.segment_count));
        let end = end_leaf.min(u64::from(layout.segment_count));
        if start >= end {
            return;
        }

        for leaf in start..end {
            let count = self.segment_leaf_count_with_layout(layout, leaf as u32, elem_capacity);
            self.edges
                .set_count(leaf + u64::from(layout.segment_count), count);
        }
        for leaf in start..end {
            let mut idx = (leaf + u64::from(layout.segment_count)) / 2;
            while idx >= 1 {
                let left = self.edges.counts_store().get(idx * 2);
                let right = self.edges.counts_store().get(idx * 2 + 1);
                self.edges.set_count(
                    idx,
                    SegmentEdgeCounts {
                        actual: left.actual + right.actual,
                        total: left.total + right.total,
                        tombstone: left.tombstone + right.tombstone,
                    },
                );
                if idx == 1 {
                    break;
                }
                idx /= 2;
            }
        }
    }

    fn update_leaf_count_and_ancestors(
        &self,
        layout: &DgapLayout,
        leaf: u32,
        count: SegmentEdgeCounts,
    ) {
        if leaf >= layout.segment_count {
            return;
        }
        let mut idx = u64::from(leaf + layout.segment_count);
        self.edges.set_count(idx, count);
        idx /= 2;
        while idx >= 1 {
            let left = self.edges.counts_store().get(idx * 2);
            let right = self.edges.counts_store().get(idx * 2 + 1);
            self.edges.set_count(
                idx,
                SegmentEdgeCounts {
                    actual: left.actual + right.actual,
                    total: left.total + right.total,
                    tombstone: left.tombstone + right.tombstone,
                },
            );
            if idx == 1 {
                break;
            }
            idx /= 2;
        }
    }

    fn segment_leaf_count_with_layout(
        &self,
        layout: &DgapLayout,
        leaf: u32,
        elem_capacity: u64,
    ) -> SegmentEdgeCounts {
        let start_vid = leaf.saturating_mul(layout.segment_size);
        let end_vid =
            ((leaf + 1).saturating_mul(layout.segment_size)).min(self.vertices.len() as u32);
        let mut actual = 0i64;
        for vid in start_vid..end_vid {
            actual += i64::from(self.vertices.get(u64::from(vid)).degree());
        }
        let start_slot = if start_vid < self.vertices.len() as u32 {
            self.vertices.get(u64::from(start_vid)).base_slot_start()
        } else {
            elem_capacity
        };
        let next_slot = if leaf + 1 >= layout.segment_count {
            elem_capacity
        } else {
            let next_vid =
                ((leaf + 1).saturating_mul(layout.segment_size)).min(self.vertices.len() as u32);
            if next_vid < self.vertices.len() as u32 {
                self.vertices.get(u64::from(next_vid)).base_slot_start()
            } else {
                elem_capacity
            }
        };
        SegmentEdgeCounts {
            actual,
            total: next_slot.saturating_sub(start_slot) as i64,
            tombstone: 0,
        }
    }

    fn segment_for_vertex_id_with_layout(&self, layout: &DgapLayout, src: VertexId) -> SegmentId {
        let leaf = u32::from(src) / layout.segment_size.max(1);
        SegmentId::from(leaf.min(layout.segment_count.saturating_sub(1)))
    }

    fn calculate_positions(&self, start_vertex: u64, end_vertex: u64, gaps: u64) -> Vec<u64> {
        let start_slot = self.vertices.get(start_vertex).base_slot_start();
        self.calculate_positions_from(start_vertex, end_vertex, start_slot, gaps)
    }

    fn calculate_positions_from(
        &self,
        start_vertex: u64,
        end_vertex: u64,
        start_slot: u64,
        gaps: u64,
    ) -> Vec<u64> {
        let size = end_vertex.saturating_sub(start_vertex);
        let mut total_degree = size;
        for vid in start_vertex..end_vertex {
            total_degree = total_degree.saturating_add(u64::from(self.vertices.get(vid).degree()));
        }

        let mut out = Vec::with_capacity(size as usize);
        if size == 0 {
            return out;
        }

        let precision = 100_000_000.0;
        let mut step = if total_degree == 0 {
            0.0
        } else {
            gaps as f64 / total_degree as f64
        };
        step = (step * precision).floor() / precision;

        let mut index = start_slot as f64;
        for vid in start_vertex..end_vertex {
            let start = index as u64;
            out.push(start);
            let degree = f64::from(self.vertices.get(vid).degree());
            index = start as f64 + degree + step * (degree + 1.0);
        }
        out
    }
}

fn density(counts: SegmentEdgeCounts) -> f64 {
    if counts.total <= 0 {
        0.0
    } else {
        counts.actual as f64 / counts.total as f64
    }
}

pub(super) enum MarkPriority {
    Clean,
    Dirty(SegmentId),
    Urgent(SegmentId),
}
