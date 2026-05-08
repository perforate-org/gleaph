//! Core LARA graph orchestration.
//!
//! [`LaraGraph`] owns the vertex column, edge slab, per-segment overflow logs,
//! segment counts, segment span metadata, and free span manager. The graph is
//! deliberately split into two contracts:
//!
//! - **Scan contract:** read one vertex row, then read edge slots
//!   `[base_slot_start, base_slot_start + degree)`. Scan code must not branch
//!   on `capacity` or read relocation/free-span metadata.
//! - **Update contract:** insert, resize, rebalance, relocate, and maintenance
//!   code may read and rewrite `base_slot_start`, `degree`, and `capacity`.
//!   `capacity` is authoritative for whether a write fits inside the currently
//!   owned slab span.
//!
//! Segment relocation moves a physical segment span to a target span, rewrites
//! all affected vertex bases/capacities, updates segment counts and span
//! metadata, clears folded logs, and only then releases the old physical span to
//! the free span manager. This order keeps queries pointed at either the old
//! committed layout or the new committed layout, never at reusable free space.
//!
//! Segment identity for a vertex follows the PMA leaf layout (power-of-two leaf
//! count, grown only when `push_vertex` crosses a boundary). Production paths
//! update per-leaf and ancestor counts incrementally; full recounts exist only
//! under `cfg(test)`.

#[cfg(feature = "canbench")]
mod bench;
pub mod edge;
pub mod maintenance;
pub mod vertex;

use crate::{
    GrowFailed, SegmentId, VertexId,
    lara::{
        edge::{
            EdgeHeaderV1, EdgeStore, InsertLocation, OutEdgesIter, VertexAccess,
            counts::SegmentEdgeCounts, segment_tree_leaf_count,
        },
        vertex::VertexStore,
    },
    traits::{CsrEdge, CsrVertex, CsrVertexTombstoneScan},
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
struct LaraLayout {
    elem_capacity: u64,
    segment_count: u32,
    segment_size: u32,
    tree_height: u32,
    initial_vertex_edge_slots: u32,
}

impl From<EdgeHeaderV1> for LaraLayout {
    fn from(header: EdgeHeaderV1) -> Self {
        Self {
            elem_capacity: header.elem_capacity,
            segment_count: header.segment_count,
            segment_size: header.segment_size,
            tree_height: header.tree_height,
            initial_vertex_edge_slots: header.initial_vertex_edge_slots,
        }
    }
}

pub(crate) struct InsertOutcome {
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

    fn get(&self, id: VertexId) -> V {
        self.get(id)
    }

    fn set(&self, id: VertexId, item: &V) {
        self.set(id, item);
    }
}

/// Errors returned when reopening a persisted [`LaraGraph`].
#[derive(Debug)]
pub enum InitError {
    /// The vertex column could not be reopened.
    Vertices(vertex::InitError),
    /// The edge subsystem could not be reopened.
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

/// Single-orientation LARA adjacency graph.
///
/// This graph stores one CSR orientation: `insert_edge` appends an edge record
/// to the row identified by `src`, and `iter_out_edges` scans that row.
pub struct LaraGraph<E, V, M>
where
    E: CsrEdge,
    V: CsrVertex,
    M: Memory,
{
    pub(super) vertices: VertexStore<V, M>,
    pub(super) edges: EdgeStore<E, M>,
    _marker: PhantomData<(E, V)>,
}

impl<E, V, M> LaraGraph<E, V, M>
where
    E: CsrEdge,
    V: CsrVertex,
    M: Memory,
{
    /// Creates a fresh graph over the supplied stable memories.
    ///
    /// The seven memories are, in order: vertex rows, PMA counts, edge slab,
    /// overflow log, segment span metadata, free span records, and free span
    /// start index.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        vertices: M,
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
        Ok(Self {
            vertices: VertexStore::new(vertices)?,
            edges: EdgeStore::new(
                counts,
                edges,
                log,
                span_meta,
                free_spans,
                free_span_by_start,
                elem_capacity,
                segment_size,
                initial_vertex_edge_slots,
            )?,
            _marker: PhantomData,
        })
    }

    /// Reopens a graph from previously initialized stable memories.
    pub fn init(
        vertices: M,
        counts: M,
        edges: M,
        log: M,
        span_meta: M,
        free_spans: M,
        free_span_by_start: M,
    ) -> Result<Self, InitError> {
        Ok(Self {
            vertices: VertexStore::init(vertices).map_err(InitError::Vertices)?,
            edges: EdgeStore::init(
                counts,
                edges,
                log,
                span_meta,
                free_spans,
                free_span_by_start,
            )
            .map_err(InitError::Edges)?,
            _marker: PhantomData,
        })
    }

    /// Returns the stable vertex column.
    pub fn vertices(&self) -> &VertexStore<V, M> {
        &self.vertices
    }

    /// Returns the edge storage subsystem.
    pub fn edges(&self) -> &EdgeStore<E, M> {
        &self.edges
    }

    /// Consumes the graph and returns its stable memories in constructor order.
    pub fn into_memories(self) -> (M, M, M, M, M, M, M) {
        let (counts, edges, log, span_meta, free_spans, free_span_by_start) =
            self.edges.into_memories();
        (
            self.vertices.into_memory(),
            counts,
            edges,
            log,
            span_meta,
            free_spans,
            free_span_by_start,
        )
    }

    /// Appends a vertex row and returns its assigned [`VertexId`].
    pub fn push_vertex(&self, vertex: V) -> Result<VertexId, GrowFailed> {
        let id = VertexId::from(u32::try_from(self.vertices.len()).expect("too many vertices"));
        self.vertices.push(vertex)?;
        let layout = self.layout();
        let target = segment_tree_leaf_count(self.vertices.len(), layout.segment_size);
        let did_grow = target > layout.segment_count;
        if did_grow {
            self.edges.grow_segment_tree_to(target)?;
            let layout = self.layout();
            self.sync_all_leaf_counts_after_pma_grow(&layout);
        }
        let layout = self.layout();
        if layout.initial_vertex_edge_slots > 0 {
            self.try_materialize_leaf_slab(id)?;
        }
        let layout = self.layout();
        let leaf = u32::from(id) / layout.segment_size.max(1);
        if layout.initial_vertex_edge_slots > 0 || !did_grow {
            self.refresh_leaf_segment_counts(&layout, leaf);
        }
        Ok(id)
    }

    /// Inserts one edge into `src`, running immediate rebalancing when needed.
    ///
    /// Rebalance uses PMA leaf totals from the last geometry sync (vertex growth,
    /// relocation, resize) plus incremental `actual` updates on the insert path.
    /// PMA growth refreshes all leaf totals in one pass and rebuilds internal nodes,
    /// without per-leaf ancestor walks. Avoid mutating `EdgeStore` directly beside
    /// this graph; mismatched `total` fields can skew density until the next
    /// full leaf refresh (e.g. after PMA grow or rebalance).
    pub fn insert_edge(&self, src: VertexId, edge: E) -> Result<(), &'static str> {
        let _ = self.insert_edge_raw(src, edge)?;
        self.rebalance_after_insert(src)
    }

    /// Collects outgoing edges for `src` in slab slot order.
    ///
    /// Use this when the caller needs the legacy slot-order vector, such as
    /// resize and rebalancing code that rewrites contiguous CSR rows.
    pub fn collect_out_edges_slot_order(&self, src: VertexId) -> Result<Vec<E>, &'static str> {
        self.edges.collect_out_edges_slot_order(&self.vertices, src)
    }

    /// Iterates outgoing edges in the store's standard scan order.
    ///
    /// This order is deterministic for the committed store state, but it is not
    /// guaranteed to be insertion order or slab slot order.
    pub fn iter_out_edges(&self, src: VertexId) -> Result<OutEdgesIter<'_, E, M>, &'static str> {
        self.edges.iter_out_edges(&self.vertices, src)
    }

    /// Removes one outgoing edge whose full edge record matches `edge`.
    ///
    /// When the edge is present, the last edge in `src`'s row may be moved into
    /// the removed slot. Use this API only where adjacency order is not part of
    /// the caller's contract. When several edges connect the same vertices,
    /// fields such as labels or properties in `E` decide which single record is
    /// removed.
    pub fn remove_edge(&self, src: VertexId, edge: E) -> Result<bool, &'static str>
    where
        E: PartialEq,
    {
        Ok(self
            .remove_edge_matching(src, |candidate| *candidate == edge)?
            .is_some())
    }

    /// Removes the first outgoing edge accepted by `matches`.
    ///
    /// Returns the removed edge record when one was present. This is useful for
    /// matching on only part of the payload, such as a label or one property.
    pub fn remove_edge_matching<F>(
        &self,
        src: VertexId,
        matches: F,
    ) -> Result<Option<E>, &'static str>
    where
        F: FnMut(&E) -> bool,
    {
        let src_idx = u64::from(src);
        if src_idx >= self.vertices.len() {
            return Err("vertex out of range");
        }
        let v = self.vertices.get(src);
        if V::record_is_vertex_tombstone(&v) {
            return Err("vertex deleted");
        }
        if v.log_head() >= 0 {
            self.rebalance_leaf_for(src)
                .map_err(|_| "rebalance failed")?;
        }
        self.edges
            .remove_edge_unordered_matching(&self.vertices, src, matches)
    }

    /// Doubles or otherwise expands the edge slab and redistributes all rows.
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
            let vid = vertex_id(vidx as u64);
            let v = self.vertices.get(vid);
            self.vertices.set(
                vid,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
                    .with_span_capacity(capacity_from_positions(
                        &positions,
                        vidx,
                        vertex_len as usize,
                        new_capacity,
                    ))
                    .with_log_head(-1),
            );
        }

        self.edges.set_num_edges(total_edges);
        let post_layout = self.layout();
        for leaf in 0..post_layout.segment_count {
            let count = self.segment_leaf_count_with_layout(&post_layout, leaf, new_capacity);
            self.update_leaf_count_and_ancestors(&post_layout, leaf, count);
        }
        for leaf in 0..post_layout.segment_count {
            self.edges.release_log_segment(SegmentId::from(leaf))?;
        }
        Ok(())
    }

    pub(crate) fn insert_edge_raw(
        &self,
        src: VertexId,
        edge: E,
    ) -> Result<InsertOutcome, &'static str> {
        let mut layout = self.layout();
        let mut segment = self.segment_for_vertex_id_with_layout(&layout, src);
        if self
            .edges
            .log_is_full_with_segment_size(src, layout.segment_size)
        {
            self.rebalance_leaf_segment_with_layout(&layout, segment)
                .map_err(|_| "rebalance failed")?;
            layout = self.layout();
            segment = self.segment_for_vertex_id_with_layout(&layout, src);
        }
        let location = match self.edges.insert_edge(&self.vertices, src, edge) {
            Ok(location) => location,
            Err("segment log full") => {
                self.rebalance_leaf_segment_with_layout(&layout, segment)
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

    pub(crate) fn vertex_is_deleted(&self, vid: VertexId) -> Result<bool, &'static str> {
        let vidx = u64::from(vid);
        if vidx >= self.vertices.len() {
            return Err("vertex out of range");
        }
        Ok(V::record_is_vertex_tombstone(&self.vertices.get(vid)))
    }

    pub(crate) fn set_vertex_deleted(
        &self,
        vid: VertexId,
        deleted: bool,
    ) -> Result<(), &'static str> {
        let vidx = u64::from(vid);
        if vidx >= self.vertices.len() {
            return Err("vertex out of range");
        }
        let v = self.vertices.get(vid);
        self.vertices
            .set(vid, &V::record_with_vertex_tombstone(v, deleted));
        Ok(())
    }

    pub(crate) fn row_edge_at_after_rebalance(
        &self,
        vid: VertexId,
        offset: u32,
    ) -> Result<Option<E>, &'static str> {
        let vidx = u64::from(vid);
        if vidx >= self.vertices.len() {
            return Err("vertex out of range");
        }
        if self.vertices.get(vid).log_head() >= 0 {
            self.rebalance_leaf_for(vid)
                .map_err(|_| "rebalance failed")?;
        }
        self.edges.row_edge_at_slab(&self.vertices, vid, offset)
    }

    pub(crate) fn clear_row_after_rebalance(&self, vid: VertexId) -> Result<u32, &'static str> {
        let vidx = u64::from(vid);
        if vidx >= self.vertices.len() {
            return Err("vertex out of range");
        }
        if self.vertices.get(vid).log_head() >= 0 {
            self.rebalance_leaf_for(vid)
                .map_err(|_| "rebalance failed")?;
        }
        self.edges.clear_row_slab(&self.vertices, vid)
    }

    pub(crate) fn remove_edge_matching_idempotent<F>(
        &self,
        src: VertexId,
        matches: F,
    ) -> Result<Option<E>, &'static str>
    where
        F: FnMut(&E) -> bool,
    {
        let src_idx = u64::from(src);
        if src_idx >= self.vertices.len() {
            return Ok(None);
        }
        if self.vertices.get(src).log_head() >= 0 {
            self.rebalance_leaf_for(src)
                .map_err(|_| "rebalance failed")?;
        }
        self.edges
            .remove_edge_unordered_matching(&self.vertices, src, matches)
    }

    fn rebalance_after_insert(&self, src: VertexId) -> Result<(), &'static str> {
        let layout = self.layout();
        let current_leaf = self.leaf_for_vertex_with_layout(&layout, u64::from(src));
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
            let left_vertex = (u64::from(src) / window_size) * window_size;
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
        layout: &LaraLayout,
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
            });
        self.rebalance_weighted_with_layout(layout, left_vertex, right_vertex, counts)
    }

    pub(crate) fn rebalance_dirty_segment(&self, segment: SegmentId) -> Result<(), GrowFailed> {
        let layout = self.layout();
        let current_leaf = u64::from(u32::from(segment));
        let leaf_counts = self
            .edge_counts_for_leaves_with_layout(&layout, current_leaf, current_leaf + 1)
            .unwrap_or(SegmentEdgeCounts {
                actual: 0,
                total: 0,
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

    pub(crate) fn rebalance_maintenance_segment(&self, segment: SegmentId) -> bool {
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

    pub(crate) fn deferred_mark_priority(
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

    fn segment_has_log_with_layout(&self, layout: &LaraLayout, segment: SegmentId) -> bool {
        let start = u64::from(u32::from(segment)).saturating_mul(u64::from(layout.segment_size));
        let end = start
            .saturating_add(u64::from(layout.segment_size))
            .min(self.vertices.len());
        (start..end).any(|vid| self.vertices.get(vertex_id(vid)).log_head() >= 0)
    }

    fn rebalance_weighted_with_layout(
        &self,
        layout: &LaraLayout,
        start_vertex: u64,
        end_vertex: u64,
        counts: SegmentEdgeCounts,
    ) -> Result<(), GrowFailed> {
        if start_vertex >= end_vertex {
            return Ok(());
        }
        let from = self.vertices.get(vertex_id(start_vertex)).base_slot_start();
        let to = if end_vertex >= self.vertices.len() {
            layout.elem_capacity
        } else {
            self.vertices.get(vertex_id(end_vertex)).base_slot_start()
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
            let id = vertex_id(vid);
            let v = self.vertices.get(id);
            self.vertices.set(
                id,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
                    .with_span_capacity(capacity_from_positions(
                        &positions,
                        offset,
                        (end_vertex - start_vertex) as usize,
                        from.saturating_add(total_space),
                    ))
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
        layout: &LaraLayout,
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
        let old_start = self.vertices.get(vertex_id(start_vertex)).base_slot_start();
        if self.try_expand_segment_in_place_with_layout(
            layout,
            segment,
            start_vertex,
            end_vertex,
            &cache,
            old_start,
            old_span,
            new_span,
            used_space,
        )? {
            self.try_slide_segment_for_adjacent_buffer_with_layout(layout)?;
            return Ok(());
        }
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
            let id = vertex_id(vid);
            let v = self.vertices.get(id);
            self.vertices.set(
                id,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
                    .with_span_capacity(capacity_from_positions(
                        &positions,
                        offset,
                        (end_vertex - start_vertex) as usize,
                        new_start.saturating_add(new_span),
                    ))
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
            },
        );
        self.try_slide_segment_for_adjacent_buffer_with_layout(layout)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn try_expand_segment_in_place_with_layout(
        &self,
        layout: &LaraLayout,
        segment: u32,
        start_vertex: u64,
        end_vertex: u64,
        cache: &RebalanceCache<E>,
        old_start: u64,
        old_span: u64,
        new_span: u64,
        used_space: u64,
    ) -> Result<bool, GrowFailed> {
        if old_span == 0 || new_span <= old_span {
            return Ok(false);
        }
        let delta = new_span - old_span;
        let adjacent_free_start = old_start.saturating_add(old_span);
        let Some(right_free) = self
            .edges
            .free_span_store()
            .free_span_starting_at(adjacent_free_start)
        else {
            return Ok(false);
        };
        if right_free.len < delta {
            return Ok(false);
        }
        let Some(_) = self
            .edges
            .free_span_store()
            .take_prefix_at(adjacent_free_start, delta)
            .map_err(|_| GrowFailed {
                current_size: 0,
                delta: 0,
            })?
        else {
            return Ok(false);
        };

        let gaps = new_span.saturating_sub(used_space);
        let positions = self.calculate_positions_from(start_vertex, end_vertex, old_start, gaps);
        for offset in 0..(end_vertex - start_vertex) as usize {
            let neighborhood = cache.vertex_edges(offset);
            let vid = start_vertex + offset as u64;
            let start = positions[offset];
            for (i, edge) in neighborhood.iter().copied().enumerate() {
                self.edges.write_slot(start + i as u64, edge)?;
            }
            let id = vertex_id(vid);
            let v = self.vertices.get(id);
            self.vertices.set(
                id,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
                    .with_span_capacity(capacity_from_positions(
                        &positions,
                        offset,
                        (end_vertex - start_vertex) as usize,
                        old_start.saturating_add(new_span),
                    ))
                    .with_log_head(-1),
            );
        }

        self.edges
            .set_segment_physical_start(SegmentId::from(segment), old_start)?;
        self.edges.release_log_segment(SegmentId::from(segment))?;
        self.update_leaf_count_and_ancestors(
            layout,
            segment,
            SegmentEdgeCounts {
                actual: used_space as i64,
                total: new_span as i64,
            },
        );
        Ok(true)
    }

    fn try_slide_segment_for_adjacent_buffer_with_layout(
        &self,
        layout: &LaraLayout,
    ) -> Result<bool, GrowFailed> {
        for segment in 0..layout.segment_count {
            let Some((start_vertex, end_vertex)) =
                self.segment_vertex_range_with_layout(layout, segment)
            else {
                continue;
            };
            let leaf_count = self
                .edges
                .counts_store()
                .get(u64::from(segment + layout.segment_count));
            let segment_len = leaf_count.total.max(0) as u64;
            if segment_len == 0 || leaf_count.actual < 0 || leaf_count.actual as u64 > segment_len {
                continue;
            }

            let segment_start = self.vertices.get(vertex_id(start_vertex)).base_slot_start();
            let segment_end = segment_start.saturating_add(segment_len);
            let Some(left_free) = self
                .edges
                .free_span_store()
                .free_span_ending_at(segment_start)
            else {
                continue;
            };
            let Some(right_free) = self
                .edges
                .free_span_store()
                .free_span_starting_at(segment_end)
            else {
                continue;
            };
            let Some(previous_segment_len) =
                self.segment_len_ending_at_with_layout(layout, left_free.start_slot, segment)
            else {
                continue;
            };

            let slide_right = previous_segment_len > segment_len;
            let new_start = if slide_right {
                segment_start.saturating_add(right_free.len)
            } else {
                left_free.start_slot
            };
            if !self.slide_segment_to_with_layout(
                layout,
                segment,
                start_vertex,
                end_vertex,
                new_start,
                segment_len,
            )? {
                continue;
            }

            let replacement_start = if slide_right {
                left_free.start_slot
            } else {
                new_start.saturating_add(segment_len)
            };
            self.edges
                .free_span_store()
                .replace_exact_pair_with(
                    left_free,
                    right_free,
                    crate::lara::edge::free_span::FreeSpan {
                        start_slot: replacement_start,
                        len: left_free.len.saturating_add(right_free.len),
                    },
                )
                .map_err(|_| GrowFailed {
                    current_size: 0,
                    delta: 0,
                })?;

            return Ok(true);
        }

        Ok(false)
    }

    fn segment_vertex_range_with_layout(
        &self,
        layout: &LaraLayout,
        segment: u32,
    ) -> Option<(u64, u64)> {
        if segment >= layout.segment_count {
            return None;
        }
        let start_vertex = u64::from(segment).saturating_mul(u64::from(layout.segment_size.max(1)));
        if start_vertex >= self.vertices.len() {
            return None;
        }
        let end_vertex = start_vertex
            .saturating_add(u64::from(layout.segment_size.max(1)))
            .min(self.vertices.len());
        Some((start_vertex, end_vertex))
    }

    fn segment_len_ending_at_with_layout(
        &self,
        layout: &LaraLayout,
        end_slot: u64,
        excluded_segment: u32,
    ) -> Option<u64> {
        for segment in 0..layout.segment_count {
            if segment == excluded_segment {
                continue;
            }
            let Some((start_vertex, _)) = self.segment_vertex_range_with_layout(layout, segment)
            else {
                continue;
            };
            let count = self
                .edges
                .counts_store()
                .get(u64::from(segment + layout.segment_count));
            let len = count.total.max(0) as u64;
            if len == 0 {
                continue;
            }
            let start = self.vertices.get(vertex_id(start_vertex)).base_slot_start();
            if start.saturating_add(len) == end_slot {
                return Some(len);
            }
        }
        None
    }

    fn slide_segment_to_with_layout(
        &self,
        layout: &LaraLayout,
        segment: u32,
        start_vertex: u64,
        end_vertex: u64,
        new_start: u64,
        segment_len: u64,
    ) -> Result<bool, GrowFailed> {
        let cache = self.collect_rebalance_cache(start_vertex, end_vertex);
        let used_space = cache.total_edges();
        if used_space > segment_len {
            return Ok(false);
        }

        let gaps = segment_len.saturating_sub(used_space);
        let positions = self.calculate_positions_from(start_vertex, end_vertex, new_start, gaps);
        for offset in 0..(end_vertex - start_vertex) as usize {
            let neighborhood = cache.vertex_edges(offset);
            let vid = start_vertex + offset as u64;
            let start = positions[offset];
            for (i, edge) in neighborhood.iter().copied().enumerate() {
                self.edges.write_slot(start + i as u64, edge)?;
            }
            let id = vertex_id(vid);
            let v = self.vertices.get(id);
            self.vertices.set(
                id,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
                    .with_span_capacity(capacity_from_positions(
                        &positions,
                        offset,
                        (end_vertex - start_vertex) as usize,
                        new_start.saturating_add(segment_len),
                    ))
                    .with_log_head(-1),
            );
        }

        self.edges
            .set_segment_physical_start(SegmentId::from(segment), new_start)?;
        self.edges.release_log_segment(SegmentId::from(segment))?;
        self.update_leaf_count_and_ancestors(
            layout,
            segment,
            SegmentEdgeCounts {
                actual: used_space as i64,
                total: segment_len as i64,
            },
        );
        Ok(true)
    }

    fn collect_rebalance_cache(&self, start_vertex: u64, end_vertex: u64) -> RebalanceCache<E> {
        let vertex_count = end_vertex.saturating_sub(start_vertex) as usize;
        let mut total_edges = 0usize;
        for vid in start_vertex..end_vertex {
            total_edges =
                total_edges.saturating_add(self.vertices.get(vertex_id(vid)).degree() as usize);
        }

        let mut edges = Vec::with_capacity(total_edges);
        let mut offsets = Vec::with_capacity(vertex_count + 1);
        offsets.push(0);
        for vid in start_vertex..end_vertex {
            let vid32 = VertexId::from(vid as u32);
            let segment_start = edges.len();
            edges.extend(
                self.edges
                    .iter_out_edges(&self.vertices, vid32)
                    .expect("LARA log chains are valid before rebalance"),
            );
            edges[segment_start..].reverse();
            offsets.push(edges.len());
        }

        RebalanceCache { edges, offsets }
    }

    fn layout(&self) -> LaraLayout {
        self.edges.header().into()
    }

    fn try_materialize_leaf_slab(&self, vid: VertexId) -> Result<(), GrowFailed> {
        let layout = self.layout();
        let w = layout.initial_vertex_edge_slots;
        if w == 0 {
            return Ok(());
        }
        let leaf = u32::from(vid) / layout.segment_size.max(1);
        let seg_size = u64::from(layout.segment_size.max(1));
        let start_vid = u64::from(leaf).saturating_mul(seg_size);
        if start_vid >= self.vertices.len() {
            return Ok(());
        }
        let row = self.vertices.get(vid);
        if row.base_slot_start() != 0 || row.span_capacity() != 0 || row.degree() != 0 {
            return Ok(());
        }

        let end_vid = start_vid.saturating_add(seg_size).min(self.vertices.len());
        let mut leaf_base = self
            .edges
            .span_meta_store()
            .get(u64::from(leaf))
            .physical_start;
        let has_recorded_base = leaf_base != 0;
        let mut has_materialized_row = false;
        for v in start_vid..end_vid {
            let row = self.vertices.get(vertex_id(v));
            if row.span_capacity() > 0 {
                has_materialized_row = true;
                if !has_recorded_base {
                    let offset = v.saturating_sub(start_vid).saturating_mul(u64::from(w));
                    leaf_base = row.base_slot_start().saturating_sub(offset);
                }
                break;
            }
            if row.base_slot_start() != 0 || row.degree() != 0 {
                return Ok(());
            }
        }

        if !has_recorded_base && !has_materialized_row {
            let span_len = seg_size.saturating_mul(u64::from(w));
            leaf_base = self.edges.allocate_span(span_len)?;
            self.edges
                .set_segment_physical_start(SegmentId::from(leaf), leaf_base)?;
        } else if !has_recorded_base {
            self.edges
                .set_segment_physical_start(SegmentId::from(leaf), leaf_base)?;
        }

        let offset =
            u64::from(u32::from(vid) % layout.segment_size.max(1)).saturating_mul(u64::from(w));
        self.vertices.set(
            vid,
            &row.with_base_slot_start(leaf_base.saturating_add(offset))
                .with_span_capacity(w)
                .with_log_head(-1),
        );
        Ok(())
    }

    fn refresh_leaf_segment_counts(&self, layout: &LaraLayout, leaf: u32) {
        if leaf >= layout.segment_count {
            return;
        }
        let count = self.segment_leaf_count_with_layout(layout, leaf, layout.elem_capacity);
        self.update_leaf_count_and_ancestors(layout, leaf, count);
    }

    /// Recomputes every PMA leaf from vertex/slab geometry, then rebuilds internal
    /// count nodes in one bottom-up pass (`O(L)` store updates vs `O(L log L)` when
    /// calling [`Self::update_leaf_count_and_ancestors`] per leaf).
    fn sync_all_leaf_counts_after_pma_grow(&self, layout: &LaraLayout) {
        if layout.segment_count == 0 {
            return;
        }
        let cap = layout.elem_capacity;
        for leaf in 0..layout.segment_count {
            let count = self.segment_leaf_count_with_layout(layout, leaf, cap);
            self.edges
                .set_count(u64::from(leaf + layout.segment_count), count);
        }
        self.edges.set_count(
            0,
            SegmentEdgeCounts {
                actual: 0,
                total: 0,
            },
        );
        for idx in (1..layout.segment_count).rev() {
            let left = self.edges.counts_store().get(u64::from(idx * 2));
            let right = self.edges.counts_store().get(u64::from(idx * 2 + 1));
            self.edges.set_count(
                u64::from(idx),
                SegmentEdgeCounts {
                    actual: left.actual + right.actual,
                    total: left.total + right.total,
                },
            );
        }
    }

    fn leaf_for_vertex_with_layout(&self, layout: &LaraLayout, vertex: u64) -> u64 {
        vertex / u64::from(layout.segment_size.max(1))
    }

    fn leaf_end_for_vertex_with_layout(&self, layout: &LaraLayout, vertex: u64) -> u64 {
        if vertex >= self.vertices.len() {
            u64::from(layout.segment_count)
        } else {
            vertex / u64::from(layout.segment_size.max(1))
        }
    }

    fn edge_counts_for_leaves_with_layout(
        &self,
        layout: &LaraLayout,
        start_leaf: u64,
        end_leaf: u64,
    ) -> Option<SegmentEdgeCounts> {
        if start_leaf >= end_leaf || end_leaf > u64::from(layout.segment_count) {
            return None;
        }
        let mut out = SegmentEdgeCounts {
            actual: 0,
            total: 0,
        };
        for leaf in start_leaf..end_leaf {
            let c = self
                .edges
                .counts_store()
                .get(leaf + u64::from(layout.segment_count));
            out.actual += c.actual;
            out.total += c.total;
        }
        Some(out)
    }

    fn delta_up_with_layout(&self, layout: &LaraLayout) -> f64 {
        let tree_height = layout.tree_height.max(1);
        (LEAF_UPPER_DENSITY - ROOT_UPPER_DENSITY) / f64::from(tree_height)
    }

    fn recount_segment_counts_range_with_layout(
        &self,
        layout: &LaraLayout,
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
            self.update_leaf_count_and_ancestors(layout, leaf as u32, count);
        }
    }

    fn update_leaf_count_and_ancestors(
        &self,
        layout: &LaraLayout,
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
        layout: &LaraLayout,
        leaf: u32,
        elem_capacity: u64,
    ) -> SegmentEdgeCounts {
        let start_vid = leaf.saturating_mul(layout.segment_size);
        if u64::from(start_vid) >= self.vertices.len() {
            return SegmentEdgeCounts {
                actual: 0,
                total: 0,
            };
        }
        let end_vid =
            ((leaf + 1).saturating_mul(layout.segment_size)).min(self.vertices.len() as u32);
        let mut actual = 0i64;
        for vid in start_vid..end_vid {
            actual += i64::from(self.vertices.get(VertexId::from(vid)).degree());
        }
        let start_slot = if start_vid < self.vertices.len() as u32 {
            self.vertices
                .get(VertexId::from(start_vid))
                .base_slot_start()
        } else {
            elem_capacity
        };
        let next_slot = if leaf + 1 >= layout.segment_count {
            elem_capacity
        } else {
            let next_vid =
                ((leaf + 1).saturating_mul(layout.segment_size)).min(self.vertices.len() as u32);
            if next_vid < self.vertices.len() as u32 {
                self.vertices
                    .get(VertexId::from(next_vid))
                    .base_slot_start()
            } else {
                elem_capacity
            }
        };
        SegmentEdgeCounts {
            actual,
            total: next_slot.saturating_sub(start_slot) as i64,
        }
    }

    fn segment_for_vertex_id_with_layout(&self, layout: &LaraLayout, src: VertexId) -> SegmentId {
        let leaf = u32::from(src) / layout.segment_size.max(1);
        SegmentId::from(leaf)
    }

    fn calculate_positions(&self, start_vertex: u64, end_vertex: u64, gaps: u64) -> Vec<u64> {
        let start_slot = self.vertices.get(vertex_id(start_vertex)).base_slot_start();
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
            total_degree =
                total_degree.saturating_add(u64::from(self.vertices.get(vertex_id(vid)).degree()));
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
            let degree = f64::from(self.vertices.get(vertex_id(vid)).degree());
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

#[inline]
fn vertex_id(index: u64) -> VertexId {
    VertexId::from(u32::try_from(index).expect("vertex index exceeds VertexId"))
}

fn capacity_from_positions(positions: &[u64], index: usize, len: usize, end_slot: u64) -> u32 {
    let start = positions[index];
    let next = if index + 1 < len {
        positions[index + 1]
    } else {
        end_slot
    };
    next.saturating_sub(start).min(u64::from(u32::MAX)) as u32
}

pub(crate) enum MarkPriority {
    Clean,
    Dirty(SegmentId),
    Urgent(SegmentId),
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::VertexId;
    use crate::lara::vertex::Vertex;
    use crate::test_support::{
        LabelledTestEdge, TestEdge, assert_vertex_capacity_invariants, lara_test_graph, test_graph,
    };
    use ic_stable_structures::{Storable, storable::Bound};
    use std::borrow::Cow;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct LargeTestEdge([u8; 80]);

    impl LargeTestEdge {
        fn new(seed: u8) -> Self {
            let mut bytes = [0u8; 80];
            for (offset, byte) in bytes.iter_mut().enumerate() {
                *byte = seed.wrapping_add(offset as u8);
            }
            Self(bytes)
        }
    }

    impl CsrEdge for LargeTestEdge {
        const BYTES: usize = 80;

        fn read_from(bytes: &[u8]) -> Self {
            let mut out = [0u8; 80];
            out.copy_from_slice(&bytes[..80]);
            Self(out)
        }

        fn write_to(self, bytes: &mut [u8]) {
            bytes[..80].copy_from_slice(&self.0);
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(u32::from(self.0[0]))
        }

        fn with_neighbor_vid(mut self, vid: VertexId) -> Self {
            self.0[0] = u32::from(vid) as u8;
            self
        }
    }

    impl Storable for LargeTestEdge {
        const BOUND: Bound = Bound::Bounded {
            max_size: 80,
            is_fixed_size: true,
        };

        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Owned(Vec::from(self.0))
        }

        fn into_bytes(self) -> Vec<u8> {
            Vec::from(self.0)
        }

        fn from_bytes(bytes: Cow<[u8]>) -> Self {
            Self::read_from(bytes.as_ref())
        }
    }

    #[test]
    fn lara_initial_vertex_edge_slots_apply_to_later_vertices_in_leaf() {
        let graph = LaraGraph::<TestEdge, Vertex, _>::new(
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            0,
            4,
            2,
        )
        .unwrap();

        graph
            .push_vertex(Vertex {
                base_slot_start: 0,
                degree: 0,
                capacity: 0,
                log_head: -1,
                deleted: false,
            })
            .unwrap();
        graph
            .push_vertex(Vertex {
                base_slot_start: 0,
                degree: 0,
                capacity: 0,
                log_head: -1,
                deleted: false,
            })
            .unwrap();

        assert_eq!(graph.vertices().get(VertexId::from(0)).base_slot_start, 0);
        assert_eq!(graph.vertices().get(VertexId::from(0)).capacity, 2);
        assert_eq!(graph.vertices().get(VertexId::from(1)).base_slot_start, 2);
        assert_eq!(graph.vertices().get(VertexId::from(1)).capacity, 2);

        graph.insert_edge(VertexId::from(1), TestEdge(10)).unwrap();
        graph.insert_edge(VertexId::from(1), TestEdge(11)).unwrap();

        assert_eq!(graph.vertices().get(VertexId::from(1)).degree, 2);
        assert_eq!(graph.vertices().get(VertexId::from(1)).log_head, -1);
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(1))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
    }

    #[test]
    fn lara_resize_folds_log_edges_back_into_slab() {
        let graph = test_graph(2, 2, &[0, 1]);

        graph.insert_edge(VertexId::from(0), TestEdge(10)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(11)).unwrap();

        graph.resize().unwrap();

        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
        assert_eq!(graph.vertices().get(VertexId::from(0)).degree, 2);
        assert_eq!(graph.vertices().get(VertexId::from(0)).log_head, -1);
        assert!(graph.edges().header().elem_capacity >= 4);
    }

    #[test]
    fn lara_iter_out_edges_matches_reverse_slot_order() {
        let graph = test_graph(2, 2, &[0, 1]);

        graph.insert_edge(VertexId::from(0), TestEdge(10)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(11)).unwrap();

        let mut expected = graph
            .collect_out_edges_slot_order(VertexId::from(0))
            .unwrap();
        expected.reverse();
        let actual = graph
            .iter_out_edges(VertexId::from(0))
            .unwrap()
            .collect::<Vec<_>>();

        assert_eq!(actual, expected);
    }

    #[test]
    fn lara_remove_edge_uses_unordered_swap_remove() {
        let graph = test_graph(8, 2, &[0, 4]);

        graph.insert_edge(VertexId::from(0), TestEdge(10)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(11)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(12)).unwrap();

        assert!(graph.remove_edge(VertexId::from(0), TestEdge(11)).unwrap());

        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(12)]
        );
        assert_eq!(graph.vertices().get(VertexId::from(0)).degree, 2);
        assert_eq!(graph.edges().header().num_edges, 2);
        assert_eq!(
            graph
                .edges()
                .counts_store()
                .get(u64::from(graph.layout().segment_count))
                .actual,
            2
        );
    }

    #[test]
    fn lara_remove_edge_folds_log_before_removing() {
        let graph = test_graph(2, 2, &[0, 1]);

        graph
            .insert_edge_raw(VertexId::from(0), TestEdge(10))
            .unwrap();
        graph
            .insert_edge_raw(VertexId::from(0), TestEdge(11))
            .unwrap();
        graph
            .insert_edge_raw(VertexId::from(0), TestEdge(12))
            .unwrap();
        assert!(graph.vertices().get(VertexId::from(0)).log_head >= 0);

        assert!(graph.remove_edge(VertexId::from(0), TestEdge(11)).unwrap());

        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(12)]
        );
        assert_eq!(graph.vertices().get(VertexId::from(0)).degree, 2);
        assert_eq!(graph.vertices().get(VertexId::from(0)).log_head, -1);
        assert_eq!(graph.edges().header().num_edges, 2);
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_remove_edge_returns_false_when_missing() {
        let graph = test_graph(8, 2, &[0, 4]);

        graph.insert_edge(VertexId::from(0), TestEdge(10)).unwrap();

        assert!(!graph.remove_edge(VertexId::from(0), TestEdge(99)).unwrap());
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10)]
        );
        assert_eq!(graph.edges().header().num_edges, 1);
    }

    #[test]
    fn lara_parallel_edges_remove_by_full_record() {
        let graph = lara_test_graph::<LabelledTestEdge>(8, 2, &[0, 4]);
        let red = LabelledTestEdge::new(1, 10);
        let blue = LabelledTestEdge::new(1, 20);

        graph.insert_edge(VertexId::from(0), red).unwrap();
        graph.insert_edge(VertexId::from(0), blue).unwrap();

        assert!(graph.remove_edge(VertexId::from(0), blue).unwrap());
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![red]
        );
        assert!(!graph.remove_edge(VertexId::from(0), blue).unwrap());
    }

    #[test]
    fn lara_parallel_edges_remove_by_predicate() {
        let graph = lara_test_graph::<LabelledTestEdge>(8, 2, &[0, 4]);
        let red = LabelledTestEdge::new(1, 10);
        let blue = LabelledTestEdge::new(1, 20);

        graph.insert_edge(VertexId::from(0), red).unwrap();
        graph.insert_edge(VertexId::from(0), blue).unwrap();

        let removed = graph
            .remove_edge_matching(VertexId::from(0), |edge| edge.label == 10)
            .unwrap();
        assert_eq!(removed, Some(red));
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![blue]
        );
    }

    #[test]
    fn lara_insert_rebalances_parent_window_before_resizing() {
        let graph = test_graph(8, 2, &[0, 2, 4, 6]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        assert_eq!(graph.edges().header().elem_capacity, 8);
        assert_eq!(graph.vertices().get(VertexId::from(0)).log_head, -1);
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert!(
            graph.vertices().get(VertexId::from(1)).base_slot_start
                > graph.vertices().get(VertexId::from(0)).base_slot_start + 3
        );
    }

    #[test]
    fn lara_parent_rebalance_recomputes_reference_segment_counts() {
        let graph = test_graph(8, 2, &[0, 2, 4, 6]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        let counts = graph.edges().counts_store();
        assert_eq!(counts.get(1).actual, 4);
        assert_eq!(counts.get(1).total, 8);
        assert_eq!(counts.get(2).actual, 4);
        assert_eq!(counts.get(2).total, 6);
        assert_eq!(counts.get(3).actual, 0);
        assert_eq!(counts.get(3).total, 2);
    }

    #[test]
    fn lara_root_saturation_relocates_hot_segment_to_tail() {
        let graph = test_graph(4, 1, &[0, 2]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        assert_eq!(graph.edges().header().elem_capacity, 10);
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert_eq!(graph.edges().span_meta_store().get(0).physical_start, 4);
        assert_eq!(graph.edges().free_span_store().len(), 1);
        let released = graph.edges().free_span_store().peek_best_fit(1).unwrap();
        assert_eq!(released.start_slot, 0);
        assert!(released.len > 0);
        assert_eq!(graph.vertices().get(VertexId::from(0)).base_slot_start, 4);
        assert_eq!(graph.vertices().get(VertexId::from(0)).degree, 4);
        assert!(
            graph.vertices().get(VertexId::from(0)).capacity
                >= graph.vertices().get(VertexId::from(0)).degree
        );
        assert_eq!(graph.vertices().get(VertexId::from(0)).log_head, -1);
        assert_eq!(graph.edges().counts_store().get(1).actual, 4);
        assert_eq!(graph.edges().counts_store().get(1).total, 7);
        assert_eq!(graph.edges().counts_store().get(2).actual, 4);
        assert_eq!(graph.edges().counts_store().get(2).total, 6);
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_local_relocation_reuses_prior_free_span() {
        let graph = test_graph(12, 1, &[0, 10]);

        for dst in 10..20 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }
        assert_eq!(graph.vertices().get(VertexId::from(0)).base_slot_start, 12);
        assert_eq!(
            graph
                .edges()
                .free_span_store()
                .peek_best_fit(10)
                .unwrap()
                .start_slot,
            0
        );

        for dst in 20..25 {
            graph.insert_edge(VertexId::from(1), TestEdge(dst)).unwrap();
        }

        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![
                TestEdge(10),
                TestEdge(11),
                TestEdge(12),
                TestEdge(13),
                TestEdge(14),
                TestEdge(15),
                TestEdge(16),
                TestEdge(17),
                TestEdge(18),
                TestEdge(19)
            ]
        );
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(1))
                .unwrap(),
            vec![
                TestEdge(20),
                TestEdge(21),
                TestEdge(22),
                TestEdge(23),
                TestEdge(24)
            ]
        );
        assert_eq!(graph.vertices().get(VertexId::from(1)).base_slot_start, 0);
        assert_eq!(graph.edges().span_meta_store().get(0).physical_start, 12);
        assert_eq!(graph.edges().span_meta_store().get(1).physical_start, 0);
        let root = graph.edges().counts_store().get(1);
        let left = graph.edges().counts_store().get(2);
        let right = graph.edges().counts_store().get(3);
        assert_eq!(root.actual, left.actual + right.actual);
        assert_eq!(root.total, left.total + right.total);
        assert_eq!(left.actual, 10);
        assert_eq!(right.actual, 5);
        assert!(left.total >= left.actual);
        assert!(right.total >= right.actual);
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_local_relocation_metadata_survives_reopen() {
        let graph = test_graph(4, 1, &[0, 2]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        let memories = graph.into_memories();
        let reopened = LaraGraph::<TestEdge, Vertex, _>::init(
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5, memories.6,
        )
        .unwrap();

        assert_eq!(reopened.edges().span_meta_store().get(0).physical_start, 4);
        assert_eq!(reopened.edges().free_span_store().len(), 1);
        let released = reopened.edges().free_span_store().peek_best_fit(1).unwrap();
        assert_eq!(released.start_slot, 0);
        assert!(released.len > 0);
        assert_eq!(
            reopened
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert_eq!(reopened.edges().counts_store().get(2).total, 6);
        assert_vertex_capacity_invariants(&reopened);
    }

    #[test]
    fn lara_local_resize_expands_in_place_into_right_free_span() {
        let graph = test_graph(40, 2, &[0, 2, 20, 22]);
        graph.edges().write_slot(0, TestEdge(101)).unwrap();
        graph.edges().write_slot(2, TestEdge(201)).unwrap();
        graph.vertices().set(
            VertexId::from(0),
            &Vertex {
                base_slot_start: 0,
                degree: 1,
                capacity: 2,
                log_head: -1,
                deleted: false,
            },
        );
        graph.vertices().set(
            VertexId::from(1),
            &Vertex {
                base_slot_start: 2,
                degree: 1,
                capacity: 2,
                log_head: -1,
                deleted: false,
            },
        );
        let layout = graph.layout();
        graph.update_leaf_count_and_ancestors(
            &layout,
            0,
            SegmentEdgeCounts {
                actual: 2,
                total: 4,
            },
        );
        graph.edges().release_span(4, 4).unwrap();

        graph.local_resize_segment_with_layout(&layout, 0).unwrap();

        assert_eq!(graph.edges().header().elem_capacity, 40);
        assert_eq!(graph.edges().span_meta_store().get(0).physical_start, 0);
        assert_eq!(graph.edges().counts_store().get(2).total, 8);
        assert!(graph.edges().free_span_store().spans().is_empty());
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(101)]
        );
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(1))
                .unwrap(),
            vec![TestEdge(201)]
        );
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_local_resize_relocates_when_right_free_span_is_too_small() {
        let graph = test_graph(40, 2, &[0, 2, 20, 22]);
        graph.edges().write_slot(0, TestEdge(301)).unwrap();
        graph.edges().write_slot(2, TestEdge(401)).unwrap();
        graph.vertices().set(
            VertexId::from(0),
            &Vertex {
                base_slot_start: 0,
                degree: 1,
                capacity: 2,
                log_head: -1,
                deleted: false,
            },
        );
        graph.vertices().set(
            VertexId::from(1),
            &Vertex {
                base_slot_start: 2,
                degree: 1,
                capacity: 2,
                log_head: -1,
                deleted: false,
            },
        );
        let layout = graph.layout();
        graph.update_leaf_count_and_ancestors(
            &layout,
            0,
            SegmentEdgeCounts {
                actual: 2,
                total: 4,
            },
        );
        graph.edges().release_span(4, 2).unwrap();

        graph.local_resize_segment_with_layout(&layout, 0).unwrap();

        assert_eq!(graph.edges().header().elem_capacity, 48);
        assert_eq!(graph.edges().span_meta_store().get(0).physical_start, 40);
        assert_eq!(
            graph.edges().free_span_store().spans(),
            vec![crate::lara::edge::free_span::FreeSpan {
                start_slot: 0,
                len: 6
            }]
        );
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(301)]
        );
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(1))
                .unwrap(),
            vec![TestEdge(401)]
        );
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_segment_slide_moves_target_left_when_target_segment_is_larger() {
        let graph = test_graph(40, 2, &[0, 2, 10, 13]);
        graph.edges().write_slot(10, TestEdge(101)).unwrap();
        graph.edges().write_slot(13, TestEdge(201)).unwrap();
        graph.edges().write_slot(14, TestEdge(202)).unwrap();
        graph.vertices().set(
            VertexId::from(2),
            &Vertex {
                base_slot_start: 10,
                degree: 1,
                capacity: 3,
                log_head: -1,
                deleted: false,
            },
        );
        graph.vertices().set(
            VertexId::from(3),
            &Vertex {
                base_slot_start: 13,
                degree: 2,
                capacity: 3,
                log_head: -1,
                deleted: false,
            },
        );
        let layout = graph.layout();
        graph.update_leaf_count_and_ancestors(
            &layout,
            0,
            SegmentEdgeCounts {
                actual: 0,
                total: 4,
            },
        );
        graph.update_leaf_count_and_ancestors(
            &layout,
            1,
            SegmentEdgeCounts {
                actual: 3,
                total: 6,
            },
        );
        graph.edges().release_span(4, 6).unwrap();
        graph.edges().release_span(16, 5).unwrap();

        assert!(
            graph
                .try_slide_segment_for_adjacent_buffer_with_layout(&layout)
                .unwrap()
        );

        assert_eq!(graph.vertices().get(VertexId::from(2)).base_slot_start, 4);
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(2))
                .unwrap(),
            vec![TestEdge(101)]
        );
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(3))
                .unwrap(),
            vec![TestEdge(201), TestEdge(202)]
        );
        assert_eq!(
            graph.edges().free_span_store().spans(),
            vec![crate::lara::edge::free_span::FreeSpan {
                start_slot: 10,
                len: 11
            }]
        );
        assert_eq!(graph.edges().span_meta_store().get(1).physical_start, 4);
        assert_eq!(graph.edges().counts_store().get(3).total, 6);
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_segment_slide_moves_target_right_when_previous_segment_is_larger() {
        let graph = test_graph(40, 2, &[0, 3, 10, 12]);
        graph.edges().write_slot(10, TestEdge(301)).unwrap();
        graph.edges().write_slot(12, TestEdge(401)).unwrap();
        graph.vertices().set(
            VertexId::from(2),
            &Vertex {
                base_slot_start: 10,
                degree: 1,
                capacity: 2,
                log_head: -1,
                deleted: false,
            },
        );
        graph.vertices().set(
            VertexId::from(3),
            &Vertex {
                base_slot_start: 12,
                degree: 1,
                capacity: 2,
                log_head: -1,
                deleted: false,
            },
        );
        let layout = graph.layout();
        graph.update_leaf_count_and_ancestors(
            &layout,
            0,
            SegmentEdgeCounts {
                actual: 0,
                total: 8,
            },
        );
        graph.update_leaf_count_and_ancestors(
            &layout,
            1,
            SegmentEdgeCounts {
                actual: 2,
                total: 4,
            },
        );
        graph.edges().release_span(8, 2).unwrap();
        graph.edges().release_span(14, 5).unwrap();

        assert!(
            graph
                .try_slide_segment_for_adjacent_buffer_with_layout(&layout)
                .unwrap()
        );

        assert_eq!(graph.vertices().get(VertexId::from(2)).base_slot_start, 15);
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(2))
                .unwrap(),
            vec![TestEdge(301)]
        );
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(3))
                .unwrap(),
            vec![TestEdge(401)]
        );
        assert_eq!(
            graph.edges().free_span_store().spans(),
            vec![crate::lara::edge::free_span::FreeSpan {
                start_slot: 8,
                len: 7
            }]
        );
        assert_eq!(graph.edges().span_meta_store().get(1).physical_start, 15);
        assert_eq!(graph.edges().counts_store().get(3).total, 4);
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_segment_slide_requires_both_free_neighbors_and_previous_segment() {
        let graph = test_graph(40, 2, &[0, 2, 10, 13]);
        graph.edges().write_slot(10, TestEdge(501)).unwrap();
        graph.vertices().set(
            VertexId::from(2),
            &Vertex {
                base_slot_start: 10,
                degree: 1,
                capacity: 3,
                log_head: -1,
                deleted: false,
            },
        );
        let layout = graph.layout();
        graph.update_leaf_count_and_ancestors(
            &layout,
            1,
            SegmentEdgeCounts {
                actual: 1,
                total: 6,
            },
        );
        graph.edges().release_span(4, 6).unwrap();

        assert!(
            !graph
                .try_slide_segment_for_adjacent_buffer_with_layout(&layout)
                .unwrap()
        );
        assert_eq!(graph.vertices().get(VertexId::from(2)).base_slot_start, 10);
        assert_eq!(
            graph.edges().free_span_store().spans(),
            vec![crate::lara::edge::free_span::FreeSpan {
                start_slot: 4,
                len: 6
            }]
        );
    }

    #[test]
    fn lara_segment_sliding_persists_after_reopen() {
        let graph = test_graph(40, 2, &[0, 2, 10, 13]);
        graph.edges().write_slot(10, TestEdge(601)).unwrap();
        graph.edges().write_slot(13, TestEdge(701)).unwrap();
        graph.vertices().set(
            VertexId::from(2),
            &Vertex {
                base_slot_start: 10,
                degree: 1,
                capacity: 3,
                log_head: -1,
                deleted: false,
            },
        );
        graph.vertices().set(
            VertexId::from(3),
            &Vertex {
                base_slot_start: 13,
                degree: 1,
                capacity: 3,
                log_head: -1,
                deleted: false,
            },
        );
        let layout = graph.layout();
        graph.update_leaf_count_and_ancestors(
            &layout,
            0,
            SegmentEdgeCounts {
                actual: 0,
                total: 4,
            },
        );
        graph.update_leaf_count_and_ancestors(
            &layout,
            1,
            SegmentEdgeCounts {
                actual: 2,
                total: 6,
            },
        );
        graph.edges().release_span(4, 6).unwrap();
        graph.edges().release_span(16, 5).unwrap();
        assert!(
            graph
                .try_slide_segment_for_adjacent_buffer_with_layout(&layout)
                .unwrap()
        );

        let memories = graph.into_memories();
        let reopened = LaraGraph::<TestEdge, Vertex, _>::init(
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5, memories.6,
        )
        .unwrap();

        assert_eq!(
            reopened.vertices().get(VertexId::from(2)).base_slot_start,
            4
        );
        assert_eq!(
            reopened
                .collect_out_edges_slot_order(VertexId::from(2))
                .unwrap(),
            vec![TestEdge(601)]
        );
        assert_eq!(
            reopened.edges().free_span_store().spans(),
            vec![crate::lara::edge::free_span::FreeSpan {
                start_slot: 10,
                len: 11
            }]
        );
        assert_vertex_capacity_invariants(&reopened);
    }

    #[test]
    fn lara_segment_sliding_copies_large_edges() {
        let graph = LaraGraph::new(
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            40,
            2,
            0,
        )
        .unwrap();
        for base_slot_start in [0, 2, 10, 13] {
            graph
                .push_vertex(Vertex {
                    base_slot_start,
                    degree: 0,
                    capacity: 0,
                    log_head: -1,
                    deleted: false,
                })
                .unwrap();
        }
        graph.edges().write_slot(10, LargeTestEdge::new(7)).unwrap();
        graph
            .edges()
            .write_slot(13, LargeTestEdge::new(11))
            .unwrap();
        graph.vertices().set(
            VertexId::from(2),
            &Vertex {
                base_slot_start: 10,
                degree: 1,
                capacity: 3,
                log_head: -1,
                deleted: false,
            },
        );
        graph.vertices().set(
            VertexId::from(3),
            &Vertex {
                base_slot_start: 13,
                degree: 1,
                capacity: 3,
                log_head: -1,
                deleted: false,
            },
        );
        let layout = graph.layout();
        graph.update_leaf_count_and_ancestors(
            &layout,
            0,
            SegmentEdgeCounts {
                actual: 0,
                total: 4,
            },
        );
        graph.update_leaf_count_and_ancestors(
            &layout,
            1,
            SegmentEdgeCounts {
                actual: 2,
                total: 6,
            },
        );
        graph.edges().release_span(4, 6).unwrap();
        graph.edges().release_span(16, 5).unwrap();

        assert!(
            graph
                .try_slide_segment_for_adjacent_buffer_with_layout(&layout)
                .unwrap()
        );

        assert_eq!(graph.vertices().get(VertexId::from(2)).base_slot_start, 4);
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(2))
                .unwrap(),
            vec![LargeTestEdge::new(7)]
        );
        assert_eq!(
            graph
                .collect_out_edges_slot_order(VertexId::from(3))
                .unwrap(),
            vec![LargeTestEdge::new(11)]
        );
    }

    #[test]
    fn lara_reopen_preserves_rebalanced_layout_and_counts() {
        let graph = test_graph(8, 2, &[0, 2, 4, 6]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        let memories = graph.into_memories();
        let reopened = LaraGraph::<TestEdge, Vertex, _>::init(
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5, memories.6,
        )
        .unwrap();

        assert_eq!(reopened.edges().header().elem_capacity, 8);
        assert_eq!(reopened.edges().span_meta_store().len(), 2);
        assert_eq!(reopened.vertices().get(VertexId::from(0)).degree, 4);
        assert!(
            reopened.vertices().get(VertexId::from(0)).capacity
                >= reopened.vertices().get(VertexId::from(0)).degree
        );
        assert_eq!(reopened.vertices().get(VertexId::from(0)).log_head, -1);
        assert_eq!(
            reopened
                .collect_out_edges_slot_order(VertexId::from(0))
                .unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert_eq!(reopened.edges().counts_store().get(2).total, 6);
        assert_vertex_capacity_invariants(&reopened);
    }
}
