//! Core LARA graph orchestration.
//!
//! [`LaraGraph`] owns the vertex column, edge slab, per-segment overflow logs,
//! segment counts, segment span metadata, and free span manager. The graph is
//! deliberately split into two contracts:
//!
//! - **Scan contract:** read one vertex row, then enumerate **logical** out-edges.
//!   For [`crate::lara::vertex::Vertex`] rows this means [`crate::traits::CsrVertex::degree`]
//!   (live count); the slab may reserve [`crate::traits::CsrVertex::stored_degree`] cells with
//!   [`crate::VertexId::EDGE_TOMBSTONE_SENTINEL`] tombstones until a rebalance packs the row. Iteration APIs
//!   skip tombstoned cells. Scan code must not branch on segment span metadata or read relocation /
//!   free-span metadata beyond the vertex row fields used for visibility.
//! - **Update contract:** insert, resize, rebalance, relocate, and maintenance
//!   code may read and rewrite `base_slot_start`, live/stored widths, and log metadata. Owned slab span
//!   for writes is derived from CSR successor bases, PMA counts, and slab
//!   `elem_capacity`.
//!
//! Segment relocation moves a physical segment span to a target span, rewrites
//! all affected vertex bases, updates segment counts and span metadata, clears
//! folded logs, and only then releases the old physical span to the free span
//! manager. This order keeps queries pointed at either the old committed layout
//! or the new committed layout, never at reusable free space.
//!
//! Segment identity for a vertex follows the PMA leaf layout (power-of-two leaf
//! count, grown only when `push_vertex` crosses a boundary). Production paths
//! update per-leaf and ancestor counts incrementally; full recounts exist only
//! under `cfg(test)`.

#[cfg(feature = "canbench")]
mod bench;
#[expect(
    dead_code,
    reason = "edge store includes maintenance helpers used by feature-specific paths"
)]
pub mod edge;
#[expect(
    dead_code,
    reason = "payload log helpers are used by targeted edge-value maintenance paths"
)]
pub mod edge_payload;
pub mod maintenance;
pub mod operation_error;
pub mod vertex;

use crate::lara::edge::span_meta::SPAN_PHYSICAL_UNASSIGNED;
use crate::{
    GrowFailed, SegmentId, VertexId,
    lara::{
        edge::{
            AscOutEdgesIter, EdgeHeaderV1, EdgeStore, OutEdgesIter, counts::SegmentEdgeCounts,
            segment_tree_leaf_count,
        },
        operation_error::{LaraOperationError, VertexAccess, VertexAccessError},
        vertex::VertexStore,
    },
    traits::{CsrEdge, CsrEdgeTombstone, CsrVertex, CsrVertexTombstoneScan},
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
    fn len(&self) -> u32 {
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
    /// The graph-owned memories are partially initialized (some regions are empty
    /// while others are populated), so the graph must not be reopened or recreated.
    PartialLayout,
}

impl fmt::Display for InitError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Vertices(e) => write!(f, "vertex init failed: {e}"),
            Self::Edges(e) => write!(f, "edge init failed: {e}"),
            Self::PartialLayout => {
                write!(
                    f,
                    "graph memories are partially initialized; refusing to reopen"
                )
            }
        }
    }
}

impl std::error::Error for InitError {}

/// Single-orientation LARA adjacency graph.
///
/// This graph stores one CSR orientation: `insert_edge` appends an edge record
/// to the row identified by `src`. [`LaraGraph::out_edges_iter`] scans that row in
/// [`EdgeStore`]'s default **descending** contiguous order (log head first, then slab high→low);
/// use [`LaraGraph::asc_out_edges`] for slot/materialization order.
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
        crate::slab_index::validate_elem_capacity_grow_failed(elem_capacity, edges.size())?;
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

    /// Opens a graph from stable memories, creating it when the edge slab is empty.
    ///
    /// See [`EdgeStore::init`] for how `elem_capacity` is interpreted on reopen.
    #[allow(clippy::too_many_arguments)]
    pub fn init(
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
    ) -> Result<Self, InitError> {
        // The vertex column and the whole edge subsystem are one graph-owned
        // composite: they must be created together or reopened together. A mix
        // (for example empty vertices wired to a populated edge subsystem) would
        // pair an empty `VertexStore` with live edge state, so reject it instead
        // of silently combining them.
        match crate::classify_composite_init([
            vertices.size(),
            counts.size(),
            edges.size(),
            log.size(),
            span_meta.size(),
            free_spans.size(),
            free_span_by_start.size(),
        ]) {
            crate::CompositeInit::Partial => return Err(InitError::PartialLayout),
            crate::CompositeInit::Fresh | crate::CompositeInit::Reopen => {}
        }
        Ok(Self {
            vertices: VertexStore::init(vertices).map_err(InitError::Vertices)?,
            edges: EdgeStore::init(
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
        let id = VertexId::from(self.vertices.len());
        self.vertices.push(vertex)?;
        let layout = self.layout();
        let target = segment_tree_leaf_count(self.vertices.len().into(), layout.segment_size);
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
    pub fn insert_edge(&self, src: VertexId, edge: E) -> Result<(), LaraOperationError> {
        let _ = self.insert_edge_raw(src, edge)?;
        self.rebalance_after_insert(src)
    }

    /// Returns `true` if `src` has at least one outgoing edge visible to clean scans.
    ///
    /// Equivalent to `!asc_out_edges(src)?.is_empty()` on success, without reading
    /// the edge slab or overflow log when `degree` is zero.
    pub fn has_out_edges(&self, src: VertexId) -> Result<bool, LaraOperationError> {
        self.edges.has_out_edges(&self.vertices, src)
    }

    /// Collects outgoing edges for `src` in slab slot order (ascending materialization order).
    ///
    /// Use this when the caller needs the legacy slot-order vector, such as
    /// resize and rebalancing code that rewrites contiguous CSR rows.
    pub fn asc_out_edges(&self, src: VertexId) -> Result<Vec<E>, LaraOperationError> {
        self.edges.asc_out_edges(&self.vertices, src)
    }

    /// All outgoing edges in descending scan order (see [`Self::visit_out_edges`]).
    pub fn out_edges(&self, src: VertexId) -> Result<Vec<E>, LaraOperationError> {
        let mut out = Vec::new();
        self.visit_out_edges(
            src,
            None,
            None,
            None::<&mut dyn FnMut(&[u8]) -> bool>,
            |_| true,
            |e| out.push(e),
        )?;
        Ok(out)
    }

    /// Walks outgoing edges without building a full adjacency vector.
    ///
    /// `offset` / `limit` apply to the stream of edges **accepted** after filters (see
    /// [`EdgeStore::visit_out_edges`]).
    pub fn visit_out_edges<Match, Visit>(
        &self,
        src: VertexId,
        offset: Option<usize>,
        limit: Option<usize>,
        raw_matches: Option<&mut dyn FnMut(&[u8]) -> bool>,
        matches: Match,
        visit: Visit,
    ) -> Result<(), LaraOperationError>
    where
        Match: FnMut(&E) -> bool,
        Visit: FnMut(E),
    {
        self.edges.visit_out_edges(
            &self.vertices,
            src,
            offset,
            limit,
            raw_matches,
            matches,
            visit,
        )
    }

    /// Iterates outgoing edges in [`EdgeStore`]'s default **descending** contiguous order (overflow
    /// log from the chain head, then slab slots high→low). Prefer this on hot paths; use
    /// [`Self::asc_out_edges`] when you need ascending slot / materialization order.
    pub fn out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<OutEdgesIter<'_, E, M>, LaraOperationError> {
        self.edges.out_edges_iter(&self.vertices, src)
    }

    /// Descending-scan iterator (same contract as [`Self::out_edges_iter`]).
    pub fn desc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<OutEdgesIter<'_, E, M>, LaraOperationError> {
        self.edges.desc_out_edges_iter(&self.vertices, src)
    }

    /// Ascending CSR slot / materialization order (same sequence as [`Self::asc_out_edges`]).
    ///
    /// Slab-only rows stream slot-by-slot; log-backed rows materialize once then iterate.
    pub fn asc_out_edges_iter(
        &self,
        src: VertexId,
    ) -> Result<AscOutEdgesIter<'_, E, M>, LaraOperationError> {
        self.edges.asc_out_edges_iter(&self.vertices, src)
    }

    /// Removes one outgoing edge whose full edge record matches `edge`.
    ///
    /// On slab-only rows (`log_head < 0`), the matching cell becomes a
    /// [`CsrEdgeTombstone::tombstone_edge`] tombstone and the vertex's logical
    /// degree drops; physical slab width may stay larger until rebalance compacts the row.
    /// Use this API only where adjacency order is not part of the caller's contract.
    /// When several edges connect the same vertices, fields such as labels or properties
    /// in `E` decide which single record is removed.
    pub fn remove_edge(&self, src: VertexId, edge: E) -> Result<bool, LaraOperationError>
    where
        E: PartialEq + CsrEdgeTombstone,
    {
        Ok(self
            .remove_edge_matching(src, |candidate| *candidate == edge)?
            .is_some())
    }

    /// Removes the first outgoing edge accepted by `matches`.
    ///
    /// On slab-only rows, removal writes an edge tombstone ([`CsrEdgeTombstone`]) rather than
    /// swap-remove. Returns the removed edge record when one was present. This is useful for
    /// matching on only part of the payload, such as a label or one property.
    pub fn remove_edge_matching<F>(
        &self,
        src: VertexId,
        matches: F,
    ) -> Result<Option<E>, LaraOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        if u32::from(src) >= self.vertices.len() {
            return Err(LaraOperationError::VertexAccess(
                VertexAccessError::OutOfRange,
            ));
        }
        let v = self.vertices.get(src);
        if V::record_is_vertex_tombstone(&v) {
            return Err(LaraOperationError::VertexDeleted);
        }
        self.edges
            .remove_edge_slab_tombstone_matching(&self.vertices, src, matches)
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
            .max(total_edges.saturating_add(u64::from(vertex_len)));
        self.edges.set_elem_capacity(new_capacity)?;

        let positions =
            self.calculate_positions(0, vertex_len, new_capacity.saturating_sub(total_edges));
        for vidx in 0..vertex_len {
            let neighborhood = cache.vertex_edges(vidx as usize);
            let start = positions[vidx as usize];
            for (i, edge) in neighborhood.iter().cloned().enumerate() {
                self.edges.write_slot(start + i as u64, edge)?;
            }
            let vid = VertexId::from(vidx);
            let v = self.vertices.get(vid);
            self.vertices.set(
                vid,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
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
    ) -> Result<InsertOutcome, LaraOperationError> {
        let mut layout = self.layout();
        let mut segment = self.segment_for_vertex_id_with_layout(&layout, src);
        if self
            .edges
            .log_is_full_with_segment_size(src, layout.segment_size)
        {
            self.rebalance_leaf_segment_with_layout(&layout, segment)
                .map_err(LaraOperationError::RebalanceFailed)?;
            layout = self.layout();
            segment = self.segment_for_vertex_id_with_layout(&layout, src);
        }
        let location = match self.edges.insert_edge(&self.vertices, src, edge.clone()) {
            Ok(location) => location,
            Err(LaraOperationError::SegmentLogFull) => {
                self.rebalance_leaf_segment_with_layout(&layout, segment)
                    .map_err(LaraOperationError::RebalanceFailed)?;
                self.edges.insert_edge(&self.vertices, src, edge)?
            }
            Err(e) => return Err(e),
        };
        Ok(InsertOutcome {
            segment,
            inserted_into_log: location.inserted_into_log(),
        })
    }

    pub(crate) fn vertex_is_deleted(&self, vid: VertexId) -> Result<bool, LaraOperationError> {
        if u32::from(vid) >= self.vertices.len() {
            return Err(LaraOperationError::VertexAccess(
                VertexAccessError::OutOfRange,
            ));
        }
        Ok(V::record_is_vertex_tombstone(&self.vertices.get(vid)))
    }

    pub(crate) fn set_vertex_deleted(
        &self,
        vid: VertexId,
        deleted: bool,
    ) -> Result<(), LaraOperationError> {
        if u32::from(vid) >= self.vertices.len() {
            return Err(LaraOperationError::VertexAccess(
                VertexAccessError::OutOfRange,
            ));
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
    ) -> Result<Option<E>, LaraOperationError> {
        if u32::from(vid) >= self.vertices.len() {
            return Err(LaraOperationError::VertexAccess(
                VertexAccessError::OutOfRange,
            ));
        }
        if self.vertices.get(vid).log_head() >= 0 {
            self.rebalance_leaf_for(vid)
                .map_err(LaraOperationError::RebalanceFailed)?;
        }
        self.edges.row_edge_at_slab(&self.vertices, vid, offset)
    }

    pub(crate) fn clear_row_after_rebalance(
        &self,
        vid: VertexId,
    ) -> Result<u32, LaraOperationError> {
        if u32::from(vid) >= self.vertices.len() {
            return Err(LaraOperationError::VertexAccess(
                VertexAccessError::OutOfRange,
            ));
        }
        if self.vertices.get(vid).log_head() >= 0 {
            self.rebalance_leaf_for(vid)
                .map_err(LaraOperationError::RebalanceFailed)?;
        }
        self.edges.clear_row_slab(&self.vertices, vid)
    }

    pub(crate) fn remove_edge_matching_idempotent<F>(
        &self,
        src: VertexId,
        matches: F,
    ) -> Result<Option<E>, LaraOperationError>
    where
        E: CsrEdgeTombstone,
        F: FnMut(&E) -> bool,
    {
        if u32::from(src) >= self.vertices.len() {
            return Ok(None);
        }
        if self.vertices.get(src).log_head() >= 0 {
            self.rebalance_leaf_for(src)
                .map_err(LaraOperationError::RebalanceFailed)?;
        }
        self.edges
            .remove_edge_slab_tombstone_matching(&self.vertices, src, matches)
    }

    fn rebalance_after_insert(&self, src: VertexId) -> Result<(), LaraOperationError> {
        let layout = self.layout();
        let current_leaf = self.leaf_for_vertex_with_layout(&layout, src);
        let leaf_counts = self
            .edge_counts_for_leaves_with_layout(&layout, current_leaf, current_leaf + 1)
            .ok_or(LaraOperationError::SegmentCountsOutOfRange)?;
        if density(leaf_counts) < LEAF_UPPER_DENSITY {
            return Ok(());
        }

        let mut window = u64::from(current_leaf) + u64::from(layout.segment_count);
        let mut height = 0u32;
        let mut chosen: Option<(u32, u32, SegmentEdgeCounts)> = None;
        while window > 0 {
            window /= 2;
            height += 1;
            let window_size = u64::from(layout.segment_size).saturating_mul(1u64 << height);
            let src_u = u64::from(src);
            let left_u = (src_u / window_size) * window_size;
            let right_u = left_u
                .saturating_add(window_size)
                .min(u64::from(self.vertices.len()));
            let left_vertex = u32::try_from(left_u).expect("window left fits VertexId");
            let right_vertex = u32::try_from(right_u).expect("window right fits VertexId");
            let left_leaf = self.leaf_for_vertex_with_layout(&layout, VertexId::from(left_vertex));
            let right_leaf = self
                .leaf_end_for_vertex_with_layout(&layout, right_vertex)
                .max(left_leaf + 1);
            let counts = self
                .edge_counts_for_leaves_with_layout(&layout, left_leaf, right_leaf)
                .ok_or(LaraOperationError::SegmentCountsOutOfRange)?;
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
                    .ok_or(LaraOperationError::SegmentCountsOutOfRange)?,
            );
            if leaf_density >= LEAF_UPPER_DENSITY {
                self.rebalance_weighted_with_layout(&layout, left_vertex, right_vertex, counts)
                    .map_err(LaraOperationError::RebalanceFailed)?;
            }
        } else if density(
            self.edge_counts_for_leaves_with_layout(&layout, current_leaf, current_leaf + 1)
                .ok_or(LaraOperationError::SegmentCountsOutOfRange)?,
        ) >= LEAF_UPPER_DENSITY
        {
            self.local_resize_segment_with_layout(&layout, current_leaf)
                .map_err(LaraOperationError::ResizeFailed)?;
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
        let left_vertex = u32::from(segment).saturating_mul(layout.segment_size.max(1));
        let right_vertex = left_vertex
            .saturating_add(layout.segment_size.max(1))
            .min(self.vertices.len());
        let left_leaf = self.leaf_for_vertex_with_layout(layout, VertexId::from(left_vertex));
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
        let current_leaf = u32::from(segment);
        let leaf_counts = self
            .edge_counts_for_leaves_with_layout(
                &layout,
                current_leaf,
                current_leaf.saturating_add(1),
            )
            .unwrap_or(SegmentEdgeCounts {
                actual: 0,
                total: 0,
            });
        if density(leaf_counts) < LEAF_UPPER_DENSITY {
            return self.rebalance_leaf_segment_with_layout(&layout, segment);
        }

        let src_vertex = u32::from(segment).saturating_mul(layout.segment_size.max(1));
        let mut window = u64::from(current_leaf) + u64::from(layout.segment_count);
        let mut height = 0u32;
        let mut chosen: Option<(u32, u32, SegmentEdgeCounts)> = None;
        while window > 0 {
            window /= 2;
            height += 1;
            let window_size = u64::from(layout.segment_size).saturating_mul(1u64 << height);
            let left_u = (u64::from(src_vertex) / window_size) * window_size;
            let right_u = left_u
                .saturating_add(window_size)
                .min(u64::from(self.vertices.len()));
            let left_vertex = u32::try_from(left_u).expect("window left fits VertexId");
            let right_vertex = u32::try_from(right_u).expect("window right fits VertexId");
            let left_leaf = self.leaf_for_vertex_with_layout(&layout, VertexId::from(left_vertex));
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
                    u32::from(segment),
                    u32::from(segment).saturating_add(1),
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
                u32::from(segment),
                u32::from(segment).saturating_add(1),
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
        let seg = layout.segment_size.max(1);
        let start_v = u32::from(segment).saturating_mul(seg);
        let end_v = start_v.saturating_add(seg).min(self.vertices.len());
        (start_v..end_v).any(|vid| self.vertices.get(VertexId::from(vid)).log_head() >= 0)
    }

    fn rebalance_weighted_with_layout(
        &self,
        layout: &LaraLayout,
        start_vertex: u32,
        end_vertex: u32,
        counts: SegmentEdgeCounts,
    ) -> Result<(), GrowFailed> {
        if start_vertex >= end_vertex {
            return Ok(());
        }
        let from = self
            .vertices
            .get(VertexId::from(start_vertex))
            .base_slot_start();
        let to = if end_vertex >= self.vertices.len() {
            layout.elem_capacity
        } else {
            self.vertices
                .get(VertexId::from(end_vertex))
                .base_slot_start()
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
        for (offset, &start) in positions.iter().enumerate() {
            let neighborhood = cache.vertex_edges(offset);
            let vid_u32 = start_vertex + offset as u32;
            for (i, edge) in neighborhood.iter().cloned().enumerate() {
                self.edges.write_slot(start + i as u64, edge)?;
            }
            let id = VertexId::from(vid_u32);
            let v = self.vertices.get(id);
            self.vertices.set(
                id,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
                    .with_log_head(-1),
            );
        }

        let start_leaf = self.leaf_for_vertex_with_layout(layout, VertexId::from(start_vertex));
        let end_leaf = self
            .leaf_end_for_vertex_with_layout(layout, end_vertex)
            .max(start_leaf + 1);
        for leaf in start_leaf..end_leaf.min(layout.segment_count) {
            self.edges.release_log_segment(SegmentId::from(leaf))?;
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

        let start_vertex = segment.saturating_mul(layout.segment_size.max(1));
        let end_vertex = start_vertex
            .saturating_add(layout.segment_size.max(1))
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
        let vertex_count = u64::from(end_vertex.saturating_sub(start_vertex));
        let new_span = old_span
            .saturating_mul(2)
            .max(used_space.saturating_add(vertex_count))
            .max(1);
        let old_start = self
            .vertices
            .get(VertexId::from(start_vertex))
            .base_slot_start();
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
        for (offset, &start) in positions.iter().enumerate() {
            let neighborhood = cache.vertex_edges(offset);
            let vid_u32 = start_vertex.saturating_add(offset as u32);
            for (i, edge) in neighborhood.iter().cloned().enumerate() {
                self.edges.write_slot(start + i as u64, edge)?;
            }
            let id = VertexId::from(vid_u32);
            let v = self.vertices.get(id);
            self.vertices.set(
                id,
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
        start_vertex: u32,
        end_vertex: u32,
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
        for (offset, &start) in positions.iter().enumerate() {
            let neighborhood = cache.vertex_edges(offset);
            let vid_u32 = start_vertex.saturating_add(offset as u32);
            for (i, edge) in neighborhood.iter().cloned().enumerate() {
                self.edges.write_slot(start + i as u64, edge)?;
            }
            let id = VertexId::from(vid_u32);
            let v = self.vertices.get(id);
            self.vertices.set(
                id,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
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

            let segment_start = self
                .vertices
                .get(VertexId::from(start_vertex))
                .base_slot_start();
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
    ) -> Option<(u32, u32)> {
        if segment >= layout.segment_count {
            return None;
        }
        let seg = layout.segment_size.max(1);
        let start_vertex = segment.saturating_mul(seg);
        if start_vertex >= self.vertices.len() {
            return None;
        }
        let end_vertex = start_vertex.saturating_add(seg).min(self.vertices.len());
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
            let start = self
                .vertices
                .get(VertexId::from(start_vertex))
                .base_slot_start();
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
        start_vertex: u32,
        end_vertex: u32,
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
        for (offset, &start) in positions.iter().enumerate() {
            let neighborhood = cache.vertex_edges(offset);
            let vid_u32 = start_vertex.saturating_add(offset as u32);
            for (i, edge) in neighborhood.iter().cloned().enumerate() {
                self.edges.write_slot(start + i as u64, edge)?;
            }
            let id = VertexId::from(vid_u32);
            let v = self.vertices.get(id);
            self.vertices.set(
                id,
                &v.with_base_slot_start(start)
                    .with_degree(neighborhood.len() as u32)
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

    fn collect_rebalance_cache(&self, start_vertex: u32, end_vertex: u32) -> RebalanceCache<E> {
        let vertex_count = (end_vertex - start_vertex) as usize;
        let mut total_edges = 0usize;
        for vid in start_vertex..end_vertex {
            total_edges = total_edges
                .saturating_add(self.vertices.get(VertexId::from(vid)).degree() as usize);
        }

        let mut edges = Vec::with_capacity(total_edges);
        let mut offsets = Vec::with_capacity(vertex_count + 1);
        offsets.push(0);
        for vid in start_vertex..end_vertex {
            edges.extend(
                self.edges
                    .asc_out_edges(&self.vertices, VertexId::from(vid))
                    .expect("LARA log chains are valid before rebalance"),
            );
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
        let seg = layout.segment_size.max(1);
        let start_vid = leaf.saturating_mul(seg);
        if start_vid >= self.vertices.len() {
            return Ok(());
        }
        let row = self.vertices.get(vid);
        if row.base_slot_start() != 0 || row.degree() != 0 {
            return Ok(());
        }

        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        let span_rec = self.edges.span_meta_store().get(u64::from(leaf));
        let mut leaf_base = span_rec.physical_start;
        let has_recorded_base = span_rec.physical_start != SPAN_PHYSICAL_UNASSIGNED;
        let mut has_materialized_row = false;
        for v in start_vid..end_vid {
            let row = self.vertices.get(VertexId::from(v));
            if row.base_slot_start() != 0 {
                has_materialized_row = true;
                if !has_recorded_base {
                    let offset = u64::from(v.saturating_sub(start_vid).saturating_mul(w));
                    leaf_base = row.base_slot_start().saturating_sub(offset);
                }
                break;
            }
            if row.degree() != 0 || row.log_head() >= 0 {
                return Ok(());
            }
        }

        if !has_recorded_base && !has_materialized_row {
            let span_len = u64::from(seg.saturating_mul(w));
            leaf_base = self.edges.allocate_span(span_len)?;
            self.edges
                .set_segment_physical_start(SegmentId::from(leaf), leaf_base)?;
        } else if !has_recorded_base {
            self.edges
                .set_segment_physical_start(SegmentId::from(leaf), leaf_base)?;
        }

        let offset =
            (u64::from(vid) % u64::from(layout.segment_size.max(1))).saturating_mul(u64::from(w));
        self.vertices.set(
            vid,
            &row.with_base_slot_start(leaf_base.saturating_add(offset))
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

    fn leaf_for_vertex_with_layout(&self, layout: &LaraLayout, vertex: VertexId) -> u32 {
        u32::from(vertex) / layout.segment_size.max(1)
    }

    /// `vertex` is a vertex ordinal; use `vertices.len()` for the exclusive end of the segment range.
    fn leaf_end_for_vertex_with_layout(&self, layout: &LaraLayout, vertex: u32) -> u32 {
        if vertex >= self.vertices.len() {
            layout.segment_count
        } else {
            vertex / layout.segment_size.max(1)
        }
    }

    fn edge_counts_for_leaves_with_layout(
        &self,
        layout: &LaraLayout,
        start_leaf: u32,
        end_leaf: u32,
    ) -> Option<SegmentEdgeCounts> {
        if start_leaf >= end_leaf || end_leaf > layout.segment_count {
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
                .get(u64::from(leaf) + u64::from(layout.segment_count));
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
        start_leaf: u32,
        end_leaf: u32,
        elem_capacity: u64,
    ) {
        if layout.segment_count == 0 {
            return;
        }
        let start = start_leaf.min(layout.segment_count);
        let end = end_leaf.min(layout.segment_count);
        if start >= end {
            return;
        }

        for leaf in start..end {
            let count = self.segment_leaf_count_with_layout(layout, leaf, elem_capacity);
            self.update_leaf_count_and_ancestors(layout, leaf, count);
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
        if start_vid >= self.vertices.len() {
            return SegmentEdgeCounts {
                actual: 0,
                total: 0,
            };
        }
        let end_vid = ((leaf + 1).saturating_mul(layout.segment_size)).min(self.vertices.len());
        let mut actual = 0i64;
        for vid in start_vid..end_vid {
            actual += i64::from(self.vertices.get(VertexId::from(vid)).degree());
        }
        let start_slot = if start_vid < self.vertices.len() {
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
                ((leaf + 1).saturating_mul(layout.segment_size)).min(self.vertices.len());
            if next_vid < self.vertices.len() {
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

    fn calculate_positions(&self, start_vertex: u32, end_vertex: u32, gaps: u64) -> Vec<u64> {
        let start_slot = self
            .vertices
            .get(VertexId::from(start_vertex))
            .base_slot_start();
        self.calculate_positions_from(start_vertex, end_vertex, start_slot, gaps)
    }

    fn calculate_positions_from(
        &self,
        start_vertex: u32,
        end_vertex: u32,
        start_slot: u64,
        gaps: u64,
    ) -> Vec<u64> {
        let size = u64::from(end_vertex.saturating_sub(start_vertex));
        let mut total_degree = size;
        for vid in start_vertex..end_vertex {
            total_degree = total_degree
                .saturating_add(u64::from(self.vertices.get(VertexId::from(vid)).degree()));
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
            let degree = f64::from(self.vertices.get(VertexId::from(vid)).degree());
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

pub(crate) enum MarkPriority {
    Clean,
    Dirty(SegmentId),
    Urgent(SegmentId),
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::VertexId;
    use crate::lara::edge::EdgeLayout;
    use crate::lara::vertex::Vertex;
    use crate::{
        slab_index::MAX_SLOT_EXCLUSIVE_END,
        test_support::{
            LabelledTestEdge, TestEdge, assert_vertex_capacity_invariants, lara_test_graph,
            test_graph, vector_memory,
        },
    };
    use ic_stable_structures::{Storable, storable::Bound};
    use std::borrow::Cow;

    #[test]
    fn init_rejects_partial_layout_when_vertices_wiped() {
        let graph = LaraGraph::<TestEdge, Vertex, _>::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            8,
            1,
            0,
        )
        .unwrap();
        let (_vertices, counts, edges, log, span_meta, free_spans, free_span_by_start) =
            graph.into_memories();
        // Edge subsystem populated, vertex column wiped (e.g. a miswired MemoryId).
        let result = LaraGraph::<TestEdge, Vertex, _>::init(
            vector_memory(),
            counts,
            edges,
            log,
            span_meta,
            free_spans,
            free_span_by_start,
            8,
            1,
            0,
        );
        assert!(matches!(result, Err(InitError::PartialLayout)));
    }

    #[test]
    fn init_reopens_fully_populated_layout() {
        let graph = LaraGraph::<TestEdge, Vertex, _>::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            8,
            1,
            0,
        )
        .unwrap();
        let (vertices, counts, edges, log, span_meta, free_spans, free_span_by_start) =
            graph.into_memories();
        let reopened = LaraGraph::<TestEdge, Vertex, _>::init(
            vertices,
            counts,
            edges,
            log,
            span_meta,
            free_spans,
            free_span_by_start,
            8,
            1,
            0,
        );
        assert!(reopened.is_ok());
    }

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

        fn write_to(&self, bytes: &mut [u8]) {
            bytes[..80].copy_from_slice(&self.0);
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(u32::from(self.0[0]))
        }

        fn with_neighbor_vid(&self, vid: VertexId) -> Self {
            let mut edge = *self;
            edge.0[0] = u32::from(vid) as u8;
            edge
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

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct SlotAwareTestEdge {
        target: u32,
        slot: u32,
    }

    impl SlotAwareTestEdge {
        fn new(target: u32) -> Self {
            Self {
                target,
                slot: u32::MAX,
            }
        }
    }

    impl CsrEdge for SlotAwareTestEdge {
        const BYTES: usize = 4;

        fn read_from(bytes: &[u8]) -> Self {
            Self::new(u32::from_le_bytes(bytes[0..4].try_into().unwrap()))
        }

        fn write_to(&self, bytes: &mut [u8]) {
            bytes[0..4].copy_from_slice(&self.target.to_le_bytes());
        }

        fn neighbor_vid(&self) -> VertexId {
            VertexId::from(self.target)
        }

        fn with_neighbor_vid(&self, vid: VertexId) -> Self {
            Self {
                target: u32::from(vid),
                ..*self
            }
        }

        fn with_slot_index(self, slot_index: u32) -> Self {
            Self {
                slot: slot_index,
                ..self
            }
        }

        fn edge_slot_index_raw(&self) -> u32 {
            self.slot
        }
    }

    impl CsrEdgeTombstone for SlotAwareTestEdge {
        fn tombstone_edge() -> Self {
            Self::new(u32::from(VertexId::EDGE_TOMBSTONE_SENTINEL))
        }
    }

    #[test]
    fn lara_graph_new_rejects_elem_capacity_above_index_space() {
        assert!(
            LaraGraph::<TestEdge, Vertex, _>::new(
                vector_memory(),
                vector_memory(),
                vector_memory(),
                vector_memory(),
                vector_memory(),
                vector_memory(),
                vector_memory(),
                MAX_SLOT_EXCLUSIVE_END.saturating_add(1),
                4,
                0,
            )
            .is_err(),
            "elem_capacity above 36-bit index space must be rejected at open"
        );
    }

    #[test]
    fn lara_init_creates_empty_graph_when_memory_is_empty() {
        let graph = LaraGraph::<TestEdge, Vertex, _>::init(
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            crate::test_support::vector_memory(),
            8,
            2,
            0,
        )
        .unwrap();

        assert_eq!(graph.vertices().len(), 0);
        assert_eq!(graph.edges().header().elem_capacity, 8);
        assert_eq!(graph.edges().header().segment_size, 2);
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
            .push_vertex(Vertex::from_parts(0, 0, 0, -1, false))
            .unwrap();
        graph
            .push_vertex(Vertex::from_parts(0, 0, 0, -1, false))
            .unwrap();

        assert_eq!(graph.vertices().get(VertexId::from(0)).base_slot_start(), 0);
        let layout: EdgeLayout = graph.edges().header().into();
        let v0 = graph.vertices().get(VertexId::from(0));
        let end0 = graph
            .edges()
            .slab_window_exclusive_end(&layout, graph.vertices(), 0, &v0);
        assert_eq!(end0.saturating_sub(v0.base_slot_start()), 2);
        assert_eq!(graph.vertices().get(VertexId::from(1)).base_slot_start(), 2);
        let v1 = graph.vertices().get(VertexId::from(1));
        let end1 = graph
            .edges()
            .slab_window_exclusive_end(&layout, graph.vertices(), 1, &v1);
        assert_eq!(end1.saturating_sub(v1.base_slot_start()), 2);

        graph.insert_edge(VertexId::from(1), TestEdge(10)).unwrap();
        graph.insert_edge(VertexId::from(1), TestEdge(11)).unwrap();

        assert_eq!(graph.vertices().get(VertexId::from(1)).degree(), 2);
        assert_eq!(graph.vertices().get(VertexId::from(1)).log_head(), -1);
        assert_eq!(
            graph.asc_out_edges(VertexId::from(1)).unwrap(),
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
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11)]
        );
        assert_eq!(graph.vertices().get(VertexId::from(0)).degree(), 2);
        assert_eq!(graph.vertices().get(VertexId::from(0)).log_head(), -1);
        assert!(graph.edges().header().elem_capacity >= 4);
    }

    #[test]
    fn lara_out_edges_iter_is_descending_scan() {
        let graph = test_graph(2, 2, &[0, 1]);

        graph.insert_edge(VertexId::from(0), TestEdge(10)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(11)).unwrap();

        let slot_order = graph.asc_out_edges(VertexId::from(0)).unwrap();
        let actual = graph
            .out_edges_iter(VertexId::from(0))
            .unwrap()
            .collect::<Vec<_>>();

        let mut expected = slot_order.clone();
        expected.reverse();
        assert_eq!(actual, expected);
        assert_eq!(slot_order, vec![TestEdge(10), TestEdge(11)]);
    }

    #[test]
    fn lara_desc_and_asc_out_edges_iters_match_out_edges_and_asc_vec() {
        let graph = test_graph(2, 2, &[0, 1]);
        graph.insert_edge(VertexId::from(0), TestEdge(10)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(11)).unwrap();

        let expected_desc = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(
            graph
                .desc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            expected_desc
        );
        assert_eq!(
            graph
                .asc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
        );
    }

    #[test]
    fn lara_remove_edge_uses_slab_tombstone_delete() {
        let graph = test_graph(8, 2, &[0, 4]);

        graph.insert_edge(VertexId::from(0), TestEdge(10)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(11)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(12)).unwrap();

        assert!(graph.remove_edge(VertexId::from(0), TestEdge(11)).unwrap());

        assert_eq!(
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(12)]
        );
        assert_eq!(
            graph
                .out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(12), TestEdge(10)]
        );
        assert_eq!(graph.vertices().get(VertexId::from(0)).degree(), 2);
        assert_eq!(graph.edges().header().num_edges, 2);
        assert_eq!(
            graph
                .edges()
                .counts_store()
                .get(u64::from(graph.layout().segment_count))
                .actual,
            2
        );

        graph.insert_edge(VertexId::from(0), TestEdge(13)).unwrap();

        assert_eq!(
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(12), TestEdge(13)]
        );
        assert_eq!(graph.vertices().get(VertexId::from(0)).degree(), 3);
        assert_eq!(graph.vertices().get(VertexId::from(0)).stored_degree(), 4);
        assert_eq!(graph.edges().header().num_edges, 3);
    }

    #[test]
    fn lara_clear_row_after_tombstone_delete_counts_only_live_edges() {
        let graph = test_graph(8, 2, &[0, 4]);

        graph.insert_edge(VertexId::from(0), TestEdge(10)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(11)).unwrap();
        graph.insert_edge(VertexId::from(0), TestEdge(12)).unwrap();
        assert!(graph.remove_edge(VertexId::from(0), TestEdge(11)).unwrap());

        assert_eq!(graph.vertices().get(VertexId::from(0)).degree(), 2);
        assert_eq!(graph.vertices().get(VertexId::from(0)).stored_degree(), 3);
        assert_eq!(graph.edges().header().num_edges, 2);

        let removed = graph.clear_row_after_rebalance(VertexId::from(0)).unwrap();

        assert_eq!(removed, 2);
        assert_eq!(graph.vertices().get(VertexId::from(0)).degree(), 0);
        assert_eq!(graph.vertices().get(VertexId::from(0)).stored_degree(), 0);
        assert_eq!(graph.edges().header().num_edges, 0);
        assert_eq!(
            graph
                .edges()
                .counts_store()
                .get(u64::from(graph.layout().segment_count))
                .actual,
            0
        );
    }

    #[test]
    fn lara_remove_edge_rewrites_overflow_log_tombstone_without_reordering() {
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
        let log_head_before = graph.vertices().get(VertexId::from(0)).log_head();
        assert!(log_head_before >= 0);
        let leaf = 0u32;
        let chain_before = graph
            .edges()
            .overflow_log_chain_asc_indices(leaf, log_head_before);

        assert!(graph.remove_edge(VertexId::from(0), TestEdge(11)).unwrap());

        assert_eq!(
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(12)]
        );
        assert_eq!(
            graph
                .asc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(10), TestEdge(12)]
        );
        assert_eq!(graph.vertices().get(VertexId::from(0)).degree(), 2);
        assert_eq!(
            graph.vertices().get(VertexId::from(0)).log_head(),
            log_head_before
        );
        let chain_after = graph
            .edges()
            .overflow_log_chain_asc_indices(leaf, log_head_before);
        assert_eq!(chain_before, chain_after);
        assert_eq!(graph.edges().header().num_edges, 2);
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_overflow_log_delete_targets_exact_duplicate_occurrence() {
        let graph = test_graph(2, 2, &[0, 1]);

        graph
            .insert_edge_raw(VertexId::from(0), TestEdge(10))
            .unwrap();
        graph
            .insert_edge_raw(VertexId::from(0), TestEdge(11))
            .unwrap();
        graph
            .insert_edge_raw(VertexId::from(0), TestEdge(10))
            .unwrap();
        graph
            .insert_edge_raw(VertexId::from(0), TestEdge(12))
            .unwrap();
        assert!(graph.vertices().get(VertexId::from(0)).log_head() >= 0);

        let mut seen_tens = 0u32;
        let removed = graph
            .remove_edge_matching(VertexId::from(0), |edge| {
                if *edge != TestEdge(10) {
                    return false;
                }
                seen_tens += 1;
                seen_tens == 2
            })
            .unwrap();

        assert_eq!(removed, Some(TestEdge(10)));
        assert_eq!(
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12)]
        );
        assert_eq!(
            graph
                .asc_out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12)]
        );
        assert_eq!(
            graph
                .out_edges_iter(VertexId::from(0))
                .unwrap()
                .collect::<Vec<_>>(),
            vec![TestEdge(12), TestEdge(11), TestEdge(10)]
        );
    }

    #[test]
    fn lara_collect_refs_preserves_log_slot_ordinals_after_tombstone() {
        let graph = LaraGraph::<SlotAwareTestEdge, Vertex, _>::new(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            2,
            2,
            0,
        )
        .unwrap();
        graph.push_vertex(Vertex::default()).unwrap();

        graph
            .insert_edge_raw(VertexId::from(0), SlotAwareTestEdge::new(10))
            .unwrap();
        graph
            .insert_edge_raw(VertexId::from(0), SlotAwareTestEdge::new(11))
            .unwrap();
        graph
            .insert_edge_raw(VertexId::from(0), SlotAwareTestEdge::new(12))
            .unwrap();
        assert!(graph.vertices().get(VertexId::from(0)).log_head() >= 0);

        assert!(
            graph
                .remove_edge_matching(VertexId::from(0), |edge| edge.target == 11)
                .unwrap()
                .is_some()
        );

        let removed = graph
            .remove_edge_matching(VertexId::from(0), |edge| {
                edge.target == 12 && edge.edge_slot_index_raw() == 2
            })
            .unwrap();
        assert_eq!(
            removed.map(|edge| (edge.edge_slot_index_raw(), edge.target)),
            Some((2, 12))
        );
    }

    #[test]
    fn lara_remove_edge_returns_false_when_missing() {
        let graph = test_graph(8, 2, &[0, 4]);

        graph.insert_edge(VertexId::from(0), TestEdge(10)).unwrap();

        assert!(!graph.remove_edge(VertexId::from(0), TestEdge(99)).unwrap());
        assert_eq!(
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
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
        assert_eq!(graph.asc_out_edges(VertexId::from(0)).unwrap(), vec![red]);
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
        assert_eq!(graph.asc_out_edges(VertexId::from(0)).unwrap(), vec![blue]);
    }

    #[test]
    fn lara_insert_rebalances_parent_window_before_resizing() {
        let graph = test_graph(8, 2, &[0, 2, 4, 6]);

        for dst in 10..14 {
            graph.insert_edge(VertexId::from(0), TestEdge(dst)).unwrap();
        }

        assert_eq!(graph.edges().header().elem_capacity, 8);
        assert_eq!(graph.vertices().get(VertexId::from(0)).log_head(), -1);
        assert_eq!(
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert!(
            graph.vertices().get(VertexId::from(1)).base_slot_start()
                > graph.vertices().get(VertexId::from(0)).base_slot_start() + 3
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
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert_eq!(graph.edges().span_meta_store().get(0).physical_start, 4);
        assert_eq!(graph.edges().free_span_store().len(), 1);
        let released = graph.edges().free_span_store().peek_best_fit(1).unwrap();
        assert_eq!(released.start_slot, 0);
        assert!(released.len > 0);
        assert_eq!(graph.vertices().get(VertexId::from(0)).base_slot_start(), 4);
        assert_eq!(graph.vertices().get(VertexId::from(0)).degree(), 4);
        let layout: EdgeLayout = graph.edges().header().into();
        let v = graph.vertices().get(VertexId::from(0));
        let end = graph
            .edges()
            .slab_window_exclusive_end(&layout, graph.vertices(), 0, &v);
        assert!(
            v.base_slot_start().saturating_add(u64::from(v.degree())) <= end,
            "slab neighborhood must fit csr window",
        );
        assert_eq!(graph.vertices().get(VertexId::from(0)).log_head(), -1);
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
        assert_eq!(
            graph.vertices().get(VertexId::from(0)).base_slot_start(),
            12
        );
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
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
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
            graph.asc_out_edges(VertexId::from(1)).unwrap(),
            vec![
                TestEdge(20),
                TestEdge(21),
                TestEdge(22),
                TestEdge(23),
                TestEdge(24)
            ]
        );
        assert_eq!(graph.vertices().get(VertexId::from(1)).base_slot_start(), 0);
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
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5, memories.6, 8,
            2, 0,
        )
        .unwrap();

        assert_eq!(reopened.edges().span_meta_store().get(0).physical_start, 4);
        assert_eq!(reopened.edges().free_span_store().len(), 1);
        let released = reopened.edges().free_span_store().peek_best_fit(1).unwrap();
        assert_eq!(released.start_slot, 0);
        assert!(released.len > 0);
        assert_eq!(
            reopened.asc_out_edges(VertexId::from(0)).unwrap(),
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
        graph
            .vertices()
            .set(VertexId::from(0), &Vertex::from_parts(0, 1, 1, -1, false));
        graph
            .vertices()
            .set(VertexId::from(1), &Vertex::from_parts(2, 1, 1, -1, false));
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
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(101)]
        );
        assert_eq!(
            graph.asc_out_edges(VertexId::from(1)).unwrap(),
            vec![TestEdge(201)]
        );
        assert_vertex_capacity_invariants(&graph);
    }

    #[test]
    fn lara_local_resize_relocates_when_right_free_span_is_too_small() {
        let graph = test_graph(40, 2, &[0, 2, 20, 22]);
        graph.edges().write_slot(0, TestEdge(301)).unwrap();
        graph.edges().write_slot(2, TestEdge(401)).unwrap();
        graph
            .vertices()
            .set(VertexId::from(0), &Vertex::from_parts(0, 1, 1, -1, false));
        graph
            .vertices()
            .set(VertexId::from(1), &Vertex::from_parts(2, 1, 1, -1, false));
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
            graph.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(301)]
        );
        assert_eq!(
            graph.asc_out_edges(VertexId::from(1)).unwrap(),
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
        graph
            .vertices()
            .set(VertexId::from(2), &Vertex::from_parts(10, 1, 1, -1, false));
        graph
            .vertices()
            .set(VertexId::from(3), &Vertex::from_parts(13, 2, 2, -1, false));
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

        assert_eq!(graph.vertices().get(VertexId::from(2)).base_slot_start(), 4);
        assert_eq!(
            graph.asc_out_edges(VertexId::from(2)).unwrap(),
            vec![TestEdge(101)]
        );
        assert_eq!(
            graph.asc_out_edges(VertexId::from(3)).unwrap(),
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
        graph
            .vertices()
            .set(VertexId::from(2), &Vertex::from_parts(10, 1, 1, -1, false));
        graph
            .vertices()
            .set(VertexId::from(3), &Vertex::from_parts(12, 1, 1, -1, false));
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

        assert_eq!(
            graph.vertices().get(VertexId::from(2)).base_slot_start(),
            15
        );
        assert_eq!(
            graph.asc_out_edges(VertexId::from(2)).unwrap(),
            vec![TestEdge(301)]
        );
        assert_eq!(
            graph.asc_out_edges(VertexId::from(3)).unwrap(),
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
        graph
            .vertices()
            .set(VertexId::from(2), &Vertex::from_parts(10, 1, 1, -1, false));
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
        assert_eq!(
            graph.vertices().get(VertexId::from(2)).base_slot_start(),
            10
        );
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
        graph
            .vertices()
            .set(VertexId::from(2), &Vertex::from_parts(10, 1, 1, -1, false));
        graph
            .vertices()
            .set(VertexId::from(3), &Vertex::from_parts(13, 1, 1, -1, false));
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
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5, memories.6, 24,
            2, 0,
        )
        .unwrap();

        assert_eq!(
            reopened.vertices().get(VertexId::from(2)).base_slot_start(),
            4
        );
        assert_eq!(
            reopened.asc_out_edges(VertexId::from(2)).unwrap(),
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
                .push_vertex(Vertex::from_parts(base_slot_start, 0, 0, -1, false))
                .unwrap();
        }
        graph.edges().write_slot(10, LargeTestEdge::new(7)).unwrap();
        graph
            .edges()
            .write_slot(13, LargeTestEdge::new(11))
            .unwrap();
        graph
            .vertices()
            .set(VertexId::from(2), &Vertex::from_parts(10, 1, 1, -1, false));
        graph
            .vertices()
            .set(VertexId::from(3), &Vertex::from_parts(13, 1, 1, -1, false));
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

        assert_eq!(graph.vertices().get(VertexId::from(2)).base_slot_start(), 4);
        assert_eq!(
            graph.asc_out_edges(VertexId::from(2)).unwrap(),
            vec![LargeTestEdge::new(7)]
        );
        assert_eq!(
            graph.asc_out_edges(VertexId::from(3)).unwrap(),
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
            memories.0, memories.1, memories.2, memories.3, memories.4, memories.5, memories.6, 8,
            2, 0,
        )
        .unwrap();

        assert_eq!(reopened.edges().header().elem_capacity, 8);
        assert_eq!(reopened.edges().span_meta_store().len(), 2);
        assert_eq!(reopened.vertices().get(VertexId::from(0)).degree(), 4);
        let layout: EdgeLayout = reopened.edges().header().into();
        let v = reopened.vertices().get(VertexId::from(0));
        let end = reopened
            .edges()
            .slab_window_exclusive_end(&layout, reopened.vertices(), 0, &v);
        assert!(
            v.base_slot_start().saturating_add(u64::from(v.degree())) <= end,
            "slab neighborhood must fit csr window",
        );
        assert_eq!(reopened.vertices().get(VertexId::from(0)).log_head(), -1);
        assert_eq!(
            reopened.asc_out_edges(VertexId::from(0)).unwrap(),
            vec![TestEdge(10), TestEdge(11), TestEdge(12), TestEdge(13)]
        );
        assert_eq!(reopened.edges().counts_store().get(2).total, 6);
        assert_vertex_capacity_invariants(&reopened);
    }

    /// Upgrade-boundary corruption guard: drive heavy relocation/rebalance/resize
    /// churn, reopen the same stable memories (as a canister upgrade does), and
    /// require every adjacency to be byte-identical with all invariants intact —
    /// then keep mutating to prove the reopened graph is fully operational.
    #[test]
    fn lara_heavy_relocation_survives_reopen_without_corruption() {
        const VERTICES: u32 = 4;
        const PER_VERTEX: u32 = 24;
        const SEGMENT_SIZE: u32 = 16;

        let starts: Vec<u64> = (0..u64::from(VERTICES))
            .map(|i| i * u64::from(SEGMENT_SIZE))
            .collect();
        let graph = test_graph(
            u64::from(VERTICES) * u64::from(SEGMENT_SIZE),
            SEGMENT_SIZE,
            &starts,
        );

        // Deterministic model: vertex v owns ascending, globally-unique targets.
        let mut expected: Vec<Vec<TestEdge>> = vec![Vec::new(); VERTICES as usize];
        for round in 0..PER_VERTEX {
            for v in 0..VERTICES {
                let target = v * 1000 + round;
                graph
                    .insert_edge(VertexId::from(v), TestEdge(target))
                    .unwrap();
                expected[v as usize].push(TestEdge(target));
            }
        }

        let check =
            |g: &LaraGraph<TestEdge, Vertex, _>, expected: &[Vec<TestEdge>], phase: &str| {
                assert_vertex_capacity_invariants(g);
                for v in 0..VERTICES {
                    assert_eq!(
                        g.asc_out_edges(VertexId::from(v)).unwrap(),
                        expected[v as usize],
                        "{phase}: vertex {v} adjacency diverged"
                    );
                }
            };
        check(&graph, &expected, "pre-reopen");

        // Simulate the upgrade boundary: tear down the in-memory graph and reopen
        // the persisted stable memories with the grown header geometry.
        let elem_capacity = graph.edges().header().elem_capacity;
        let segment_size = graph.edges().header().segment_size;
        let m = graph.into_memories();
        let reopened = LaraGraph::<TestEdge, Vertex, _>::init(
            m.0,
            m.1,
            m.2,
            m.3,
            m.4,
            m.5,
            m.6,
            elem_capacity,
            segment_size,
            0,
        )
        .unwrap();
        check(&reopened, &expected, "post-reopen");

        // Continued mutation after reopen must trigger fresh relocation without
        // corrupting any restored adjacency.
        for v in 0..VERTICES {
            let target = v * 1000 + PER_VERTEX;
            reopened
                .insert_edge(VertexId::from(v), TestEdge(target))
                .unwrap();
            expected[v as usize].push(TestEdge(target));
        }
        check(&reopened, &expected, "post-reopen-mutation");
    }
}
