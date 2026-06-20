//! PMA leaf physical pin for labeled edge bytes (Phase A).
//!
//! Normal labeled rows reserve edge bytes inside the leaf block recorded in
//! [`SegmentSpanMetaStore`], mirroring core LARA `try_materialize_leaf_slab`.

use crate::{
    SegmentId, VertexId,
    labeled::{record::LabelBucket, slot_index::checked_add_slot_index},
    lara::{
        edge::{counts::segment_span_density, span_meta::SPAN_PHYSICAL_UNASSIGNED},
        operation_error::LaraOperationError,
    },
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{DEFAULT_SEGMENT_SIZE, LabeledLaraGraph};

/// Edge-slab slots reserved per vertex within one PMA leaf block.
pub(crate) fn labeled_leaf_vertex_edge_quota() -> u32 {
    DEFAULT_SEGMENT_SIZE
}

/// Total edge-slab slots reserved for one PMA leaf when pinned.
pub(crate) fn labeled_leaf_physical_block_len(segment_size: u32) -> u64 {
    u64::from(
        segment_size
            .max(1)
            .saturating_mul(labeled_leaf_vertex_edge_quota()),
    )
}

fn labeled_vertex_edge_offset_in_leaf(vid: VertexId, segment_size: u32) -> u64 {
    u64::from(u32::from(vid) % segment_size.max(1))
        .saturating_mul(u64::from(labeled_leaf_vertex_edge_quota()))
}

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    /// Returns `(physical_start, block_len)` when the leaf is pinned, else `None`.
    ///
    /// `block_len` tracks the leaf PMA `segment_edges_total` (grows on segment relocate).
    pub(crate) fn labeled_leaf_physical_range(&self, vid: VertexId) -> Option<(u64, u64)> {
        let header = self.edges.header();
        let leaf = Self::leaf_index_for_vid(vid, header.segment_size);
        let span_rec = self.edges.span_meta_store().get(u64::from(leaf));
        if span_rec.physical_start == SPAN_PHYSICAL_UNASSIGNED {
            return None;
        }
        let counts = self.leaf_segment_counts_for_vid(vid);
        let block_len = if counts.total > 0 {
            counts.total as u64
        } else {
            labeled_leaf_physical_block_len(header.segment_size)
        };
        Some((span_rec.physical_start, block_len))
    }

    /// Edge-slab spans `[base, base + stored_slots)` owned by other labeled vertices in
    /// `vid`'s PMA leaf. Used to place a new vertex's edge span without overlapping a
    /// leaf-mate whose weighted span exceeds the fixed per-vertex quota.
    fn labeled_leaf_occupied_spans(&self, vid: VertexId, exclude: VertexId) -> Vec<(u64, u64)> {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(vid, seg);
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        let mut spans = Vec::new();
        for vid_u in start_vid..end_vid {
            let other = VertexId::from(vid_u);
            if other == exclude {
                continue;
            }
            let vertex = self.vertices.get(other);
            if vertex.is_default_edge_labeled() || vertex.stored_slots == 0 {
                continue;
            }
            let Ok(buckets) = self.read_vertex_label_buckets(&vertex) else {
                continue;
            };
            let Some(first) = buckets.first() else {
                continue;
            };
            let base = first.edge_start();
            let Some(end) = checked_add_slot_index(base, u64::from(vertex.stored_slots)) else {
                continue;
            };
            spans.push((base, end));
        }
        spans
    }

    /// `true` when some labeled vertex in `vid`'s PMA leaf reserves more than `quota`
    /// slots, i.e. the fixed-quota leaf layout no longer holds and a new vertex's quota
    /// offset may overlap a leaf-mate. Cheap: reads only vertex records, not buckets.
    fn labeled_leaf_has_oversized_vertex(&self, vid: VertexId, quota: u64) -> bool {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(vid, seg);
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        for vid_u in start_vid..end_vid {
            let vertex = self.vertices.get(VertexId::from(vid_u));
            if vertex.is_default_edge_labeled() {
                continue;
            }
            if u64::from(vertex.stored_slots) > quota {
                return true;
            }
        }
        false
    }

    /// Lowest base offering `need` contiguous free slots in `vid`'s pinned leaf block that
    /// does not overlap any leaf-mate's span. Prefers the fixed per-vertex quota offset.
    pub(super) fn find_free_labeled_leaf_edge_base(
        &self,
        vid: VertexId,
        leaf_start: u64,
        leaf_len: u64,
        need: u64,
    ) -> Option<u64> {
        if need == 0 || need > leaf_len {
            return None;
        }
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf_end = checked_add_slot_index(leaf_start, leaf_len)?;
        let quota = u64::from(labeled_leaf_vertex_edge_quota());
        let preferred =
            checked_add_slot_index(leaf_start, labeled_vertex_edge_offset_in_leaf(vid, seg));

        // Fast path: when no leaf-mate's span exceeds the fixed per-vertex quota, the
        // fixed-quota layout is intact and the requesting vertex's quota offset is free.
        // This avoids reading every leaf-mate's buckets on the common (sparse) insert.
        //
        // Soundness rests on the weighted-slide invariant: a relocate/slide tiles the
        // *entire* leaf block across its active vertices (each `stored_slots` spans up to
        // the next vertex, the last to `leaf_end`). So a leaf with `k < seg` active
        // vertices — i.e. one that still has a free degree-0 slot to place — has an
        // average tile width `leaf_len / k > quota`, forcing at least one oversized
        // vertex and bypassing this fast path. When the fast path *does* fire, the leaf
        // is in the untiled fixed-quota layout where each vertex sits in its own quota
        // slot, so `preferred` is genuinely free. The debug check below pins that down.
        if need <= quota
            && !self.labeled_leaf_has_oversized_vertex(vid, quota)
            && let Some(preferred) = preferred
            && checked_add_slot_index(preferred, need)? <= leaf_end
        {
            #[cfg(debug_assertions)]
            {
                let preferred_end = checked_add_slot_index(preferred, need)?;
                let occupied = self.labeled_leaf_occupied_spans(vid, vid);
                debug_assert!(
                    occupied
                        .iter()
                        .all(|(s, e)| preferred_end <= *s || preferred >= *e),
                    "fixed-quota fast path returned base {preferred} (end {preferred_end}) overlapping a leaf-mate; layout is not fixed-quota intact"
                );
            }
            return Some(preferred);
        }

        let mut spans = self.labeled_leaf_occupied_spans(vid, vid);
        spans.sort_by_key(|(start, _)| *start);

        let fits = |base: u64| -> bool {
            match checked_add_slot_index(base, need) {
                Some(end) if end <= leaf_end => spans.iter().all(|(s, e)| end <= *s || base >= *e),
                _ => false,
            }
        };

        if let Some(preferred) = preferred
            && fits(preferred)
        {
            return Some(preferred);
        }

        let mut cursor = leaf_start;
        for (start, end) in &spans {
            if *start > cursor && start.saturating_sub(cursor) >= need {
                return Some(cursor);
            }
            if *end > cursor {
                cursor = *end;
            }
        }
        if checked_add_slot_index(cursor, need)? <= leaf_end {
            Some(cursor)
        } else {
            None
        }
    }

    /// Ensures the PMA leaf containing `vid` owns a contiguous edge-slab block, returning
    /// `(physical_start, block_len)`. Allocates the block on first use.
    pub(super) fn ensure_labeled_leaf_block_pinned(
        &self,
        vid: VertexId,
    ) -> Result<(u64, u64), LabeledOperationError> {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(vid, seg);
        let block_len = labeled_leaf_physical_block_len(seg);

        let span_rec = self.edges.span_meta_store().get(u64::from(leaf));
        let physical_start = if span_rec.physical_start == SPAN_PHYSICAL_UNASSIGNED {
            let start = self
                .edges
                .allocate_span(block_len)
                .map_err(LabeledOperationError::from)?;
            self.edges
                .set_segment_physical_start(SegmentId::from(leaf), start)
                .map_err(LabeledOperationError::from)?;
            self.edges
                .bump_vertex_segment_counts(vid, 0, i64::try_from(block_len).unwrap_or(i64::MAX))
                .map_err(LabeledOperationError::from)?;
            start
        } else {
            span_rec.physical_start
        };

        Ok(self
            .labeled_leaf_physical_range(vid)
            .unwrap_or((physical_start, block_len)))
    }

    /// Ensures the PMA leaf containing `vid` owns a contiguous edge-slab block and
    /// returns a free, non-overlapping first edge slot for that vertex's quota.
    pub(super) fn ensure_labeled_leaf_edge_physical_pin(
        &self,
        vid: VertexId,
    ) -> Result<u64, LabeledOperationError> {
        let (leaf_start, leaf_len) = self.ensure_labeled_leaf_block_pinned(vid)?;
        if let Some(base) = self.find_free_labeled_leaf_edge_base(
            vid,
            leaf_start,
            leaf_len,
            u64::from(labeled_leaf_vertex_edge_quota()),
        ) {
            return Ok(base);
        }

        // No free, non-overlapping quota-sized region exists: the leaf block is full.
        // Returning the fixed quota offset here would land on a leaf-mate's span (the
        // data-loss bug class). The first-edge placement path never reaches this — it
        // uses `find_free` directly and relocates on `None` — so fail loud instead of
        // handing back an overlapping base. Callers needing room must relocate first.
        Err(LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn labeled_leaf_geometry_stored_slots(&self, leaf: u32) -> u64 {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        let mut total = 0u64;
        for vidx in start_vid..end_vid {
            let vertex = self.vertices.get(VertexId::from(vidx));
            if vertex.is_default_edge_labeled() {
                continue;
            }
            total = total.saturating_add(u64::from(vertex.stored_slots));
        }
        total
    }

    pub(super) fn labeled_leaf_pma_density(&self, vid: VertexId) -> f64 {
        segment_span_density(self.leaf_segment_counts_for_vid(vid))
    }

    /// Interim geometry density (live / sum `stored_slots`); not used for maintenance after Phase B.
    pub(super) fn labeled_leaf_geometry_density(&self, vid: VertexId) -> f64 {
        let header = self.edges.header();
        let leaf = Self::leaf_index_for_vid(vid, header.segment_size);
        let geometry_total = self.labeled_leaf_geometry_stored_slots(leaf);
        if geometry_total == 0 {
            return 0.0;
        }
        let counts = self.leaf_segment_counts_for_vid(vid);
        (counts.actual.max(0) as f64) / (geometry_total as f64)
    }

    pub(super) fn try_labeled_vertex_edge_base_in_pinned_leaf(
        &self,
        vid: VertexId,
        new_alloc: u32,
    ) -> Option<u64> {
        let (leaf_start, leaf_len) = self.labeled_leaf_physical_range(vid)?;
        let leaf_end = checked_add_slot_index(leaf_start, leaf_len)?;
        let vertex = self.vertices.get(vid);
        let base = if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            let header = self.edges.header();
            let vertex_offset = labeled_vertex_edge_offset_in_leaf(vid, header.segment_size.max(1));
            checked_add_slot_index(leaf_start, vertex_offset)?
        } else {
            let buckets = self.read_vertex_label_buckets(&vertex).ok()?;
            buckets.first()?.edge_start()
        };
        let end = checked_add_slot_index(base, u64::from(new_alloc))?;
        if end <= leaf_end { Some(base) } else { None }
    }

    /// Maximum `stored_slots` for `vid` from its first bucket base through leaf block end.
    pub(super) fn labeled_vertex_stored_slots_max_in_leaf(
        &self,
        vid: VertexId,
    ) -> Result<u32, LabeledOperationError> {
        let (leaf_start, leaf_len) = self
            .labeled_leaf_physical_range(vid)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let leaf_end = checked_add_slot_index(leaf_start, leaf_len)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let vertex = self.vertices.get(vid);
        let base = if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            self.ensure_labeled_leaf_edge_physical_pin(vid)?
        } else {
            // `unwrap_or` would eagerly run the (now occupancy-aware) pin on every call;
            // only resolve a pin when the vertex genuinely has no first bucket.
            match self.read_vertex_label_buckets(&vertex)?.first() {
                Some(bucket) => bucket.edge_start(),
                None => self.ensure_labeled_leaf_edge_physical_pin(vid)?,
            }
        };
        let fit = leaf_end.saturating_sub(base);
        u32::try_from(fit).map_err(|_| LaraOperationError::CollectAllocationOverflow.into())
    }

    pub(super) fn try_expand_labeled_leaf_in_place(
        &self,
        old_start: u64,
        old_len: u64,
        new_len: u64,
    ) -> Result<bool, LabeledOperationError> {
        if new_len <= old_len {
            return Ok(false);
        }
        let delta = new_len.saturating_sub(old_len);
        let adjacent_free_start = checked_add_slot_index(old_start, old_len)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
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
        if self
            .edges
            .free_span_store()
            .take_prefix_at(adjacent_free_start, delta)
            .map_err(|_| LaraOperationError::CollectAllocationOverflow)?
            .is_none()
        {
            return Ok(false);
        }
        Ok(true)
    }

    pub(super) fn bump_vertex_edge_span_total_delta(
        &self,
        vid: VertexId,
        d_total: i64,
    ) -> Result<(), LabeledOperationError> {
        if d_total == 0 || self.labeled_leaf_physical_range(vid).is_some() {
            return Ok(());
        }
        self.edges
            .bump_vertex_segment_counts(vid, 0, d_total)
            .map_err(LabeledOperationError::from)
    }

    /// `true` when `[start, start + len)` lies in the vertex's current pinned leaf block.
    pub(super) fn labeled_edge_footprint_in_current_leaf_pin(
        &self,
        vid: VertexId,
        start: u64,
        len: u32,
    ) -> bool {
        let (leaf_start, leaf_len) = match self.labeled_leaf_physical_range(vid) {
            Some(range) => range,
            None => return false,
        };
        let leaf_end = match checked_add_slot_index(leaf_start, leaf_len) {
            Some(end) => end,
            None => return false,
        };
        let end = match checked_add_slot_index(start, u64::from(len)) {
            Some(end) => end,
            None => return false,
        };
        start >= leaf_start && end <= leaf_end
    }

    /// `true` when both the old and new vertex edge spans lie inside the pinned leaf block.
    pub(super) fn vertex_edge_span_relocates_within_pinned_leaf(
        &self,
        vid: VertexId,
        old_base: u64,
        old_alloc: u32,
        new_base: u64,
        new_alloc: u32,
    ) -> bool {
        let (leaf_start, leaf_len) = match self.labeled_leaf_physical_range(vid) {
            Some(range) => range,
            None => return false,
        };
        let leaf_end = match checked_add_slot_index(leaf_start, leaf_len) {
            Some(end) => end,
            None => return false,
        };
        let old_end = match checked_add_slot_index(old_base, u64::from(old_alloc)) {
            Some(end) => end,
            None => return false,
        };
        let new_end = match checked_add_slot_index(new_base, u64::from(new_alloc)) {
            Some(end) => end,
            None => return false,
        };
        old_base >= leaf_start
            && old_end <= leaf_end
            && new_base >= leaf_start
            && new_end <= leaf_end
    }

    pub(crate) fn labeled_bucket_edge_end_exclusive(
        bucket: &LabelBucket,
    ) -> Result<u64, LabeledOperationError> {
        checked_add_slot_index(bucket.edge_start(), u64::from(bucket.stored_slots))
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }

    /// Debug-only: asserts no two labeled vertices sharing `vid`'s PMA leaf reserve
    /// overlapping edge-slab spans.
    ///
    /// Guards the leaf-mate overlap bug class (ADR 0022): fixed-quota placement must
    /// never site a vertex's edge span on top of a leaf-mate's reservation. Each
    /// vertex's reserved span is `[first_bucket.edge_start(), + stored_slots)` — the
    /// same occupancy model [`Self::labeled_leaf_occupied_spans`] uses for placement —
    /// so adjacency (`a.end == b.start`) is allowed but any true overlap is a bug.
    /// Reads only vertex records and first buckets; gated to debug builds.
    #[cfg(debug_assertions)]
    pub(crate) fn assert_no_labeled_leaf_mate_overlap(&self, vid: VertexId) {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(vid, seg);
        let start_vid = leaf.saturating_mul(seg);
        let end_vid = start_vid.saturating_add(seg).min(self.vertices.len());
        let mut spans: Vec<(VertexId, u64, u64)> = Vec::new();
        for vid_u in start_vid..end_vid {
            let other = VertexId::from(vid_u);
            let vertex = self.vertices.get(other);
            if vertex.is_default_edge_labeled() || vertex.stored_slots == 0 {
                continue;
            }
            let Ok(buckets) = self.read_vertex_label_buckets(&vertex) else {
                continue;
            };
            let Some(first) = buckets.first() else {
                continue;
            };
            let base = first.edge_start();
            let Some(end) = checked_add_slot_index(base, u64::from(vertex.stored_slots)) else {
                continue;
            };
            spans.push((other, base, end));
        }
        spans.sort_by_key(|(_, base, _)| *base);
        for win in spans.windows(2) {
            let (a, _a_base, a_end) = win[0];
            let (b, b_base, _b_end) = win[1];
            assert!(
                a_end <= b_base,
                "leaf-mate edge spans overlap in leaf {leaf}: vid {a:?} reserves up to {a_end} but vid {b:?} starts at {b_base}"
            );
        }
    }

    pub(crate) fn assert_labeled_buckets_within_leaf_physical(
        &self,
        vid: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() || vertex.degree() == 0 {
            return Ok(());
        }
        let (leaf_start, leaf_len) = self
            .labeled_leaf_physical_range(vid)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        let leaf_end = checked_add_slot_index(leaf_start, leaf_len)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;

        let base = vertex.base_slot_start();
        for offset in 0..u64::from(vertex.degree()) {
            let slot = checked_add_slot_index(base, offset)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let bucket = self
                .buckets
                .read_label_bucket_slot(slot)
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            let bucket_end = Self::labeled_bucket_edge_end_exclusive(&bucket)?;
            assert!(
                bucket.edge_start() >= leaf_start && bucket_end <= leaf_end,
                "bucket edge range [{}, {bucket_end}) must lie in leaf physical [{leaf_start}, {leaf_end})",
                bucket.edge_start()
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::*;
    use crate::{
        labeled::{bucket_label_key::BucketLabelKey, record::LabeledVertex},
        lara::edge::span_meta::SPAN_PHYSICAL_UNASSIGNED,
    };

    fn hub_graph() -> LabeledLaraGraph<TestEdge, crate::VectorMemory> {
        LabeledLaraGraph::new(
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            mem(),
            1 << 20,
            BucketLabelKey::from_raw(1),
        )
        .unwrap()
    }

    #[test]
    fn labeled_span_meta_assigned_on_first_leaf_edge_write() {
        let graph = hub_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        let leaf = LabeledLaraGraph::<TestEdge, crate::VectorMemory>::leaf_index_for_vid(
            hub,
            graph.edges().header().segment_size,
        );
        assert_eq!(
            graph
                .edges()
                .span_meta_store()
                .get(u64::from(leaf))
                .physical_start,
            SPAN_PHYSICAL_UNASSIGNED
        );
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                BucketLabelKey::from_raw(10),
                TestEdge {
                    target: u32::from(dst),
                },
            )
            .unwrap();
        assert_ne!(
            graph
                .edges()
                .span_meta_store()
                .get(u64::from(leaf))
                .physical_start,
            SPAN_PHYSICAL_UNASSIGNED
        );
    }

    #[test]
    fn labeled_leaf_vertices_share_span_meta_physical_start() {
        let graph = hub_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let neighbor = graph.push_vertex(LabeledVertex::default()).unwrap();
        let label_a = BucketLabelKey::from_raw(10);
        let label_b = BucketLabelKey::from_raw(11);
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                label_a,
                TestEdge {
                    target: u32::from(neighbor),
                },
            )
            .unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                neighbor,
                label_b,
                TestEdge {
                    target: u32::from(hub),
                },
            )
            .unwrap();
        let (leaf_start, leaf_len) = graph
            .labeled_leaf_physical_range(hub)
            .expect("leaf pinned after labeled insert");
        assert_eq!(
            graph.labeled_leaf_physical_range(neighbor),
            Some((leaf_start, leaf_len))
        );
        graph
            .assert_labeled_buckets_within_leaf_physical(hub)
            .unwrap();
        graph
            .assert_labeled_buckets_within_leaf_physical(neighbor)
            .unwrap();
    }

    #[test]
    fn labeled_leaf_physical_block_covers_all_label_buckets() {
        let graph = hub_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        for label_idx in 0..4u16 {
            let label = BucketLabelKey::from_raw(20_000 + label_idx);
            graph
                .insert_edge_skip_leaf_cascade(
                    hub,
                    label,
                    TestEdge {
                        target: u32::from(dst),
                    },
                )
                .unwrap();
        }
        graph
            .assert_labeled_buckets_within_leaf_physical(hub)
            .unwrap();
    }

    #[test]
    fn labeled_reopen_preserves_leaf_physical_pin() {
        let vertices = mem();
        let buckets = mem();
        let bucket_free_spans = mem();
        let bucket_free_span_by_start = mem();
        let edge_counts = mem();
        let edges = mem();
        let edge_log = mem();
        let edge_span_meta = mem();
        let edge_free_spans = mem();
        let edge_free_span_by_start = mem();
        let payload_slab = mem();
        let value_free_spans = mem();
        let value_free_span_by_start = mem();
        let payload_log = mem();
        let value_blobs = mem();
        let default_label = BucketLabelKey::from_raw(1);
        let elem_capacity = 1 << 20;

        let graph = LabeledLaraGraph::new(
            vertices.clone(),
            buckets.clone(),
            bucket_free_spans.clone(),
            bucket_free_span_by_start.clone(),
            edge_counts.clone(),
            edges.clone(),
            edge_log.clone(),
            edge_span_meta.clone(),
            edge_free_spans.clone(),
            edge_free_span_by_start.clone(),
            payload_slab.clone(),
            value_free_spans.clone(),
            value_free_span_by_start.clone(),
            payload_log.clone(),
            value_blobs.clone(),
            elem_capacity,
            default_label,
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                BucketLabelKey::from_raw(42),
                TestEdge {
                    target: u32::from(dst),
                },
            )
            .unwrap();
        let (pin_start, pin_len) = graph.labeled_leaf_physical_range(hub).unwrap();

        let reopened = LabeledLaraGraph::init(
            vertices,
            buckets,
            bucket_free_spans,
            bucket_free_span_by_start,
            edge_counts,
            edges,
            edge_log,
            edge_span_meta,
            edge_free_spans,
            edge_free_span_by_start,
            payload_slab,
            value_free_spans,
            value_free_span_by_start,
            payload_log,
            value_blobs,
            elem_capacity,
            default_label,
        )
        .unwrap();
        assert_eq!(
            reopened.labeled_leaf_physical_range(hub),
            Some((pin_start, pin_len))
        );
        reopened
            .assert_labeled_buckets_within_leaf_physical(hub)
            .unwrap();
        let mut edges_out: Vec<TestEdge> = Vec::new();
        reopened
            .for_each_edges_for_label(hub, BucketLabelKey::from_raw(42), |e| edges_out.push(e))
            .unwrap();
        assert_eq!(edges_out.len(), 1);
        assert_eq!(edges_out[0].target, u32::from(dst));
    }

    #[test]
    fn leaf_mate_overlap_guard_passes_for_normal_inserts() {
        let graph = hub_graph();
        for _ in 0..3 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let label = BucketLabelKey::from_raw(7);
        for vid_u in 0..3u32 {
            for t in 0..40u32 {
                graph
                    .insert_edge_skip_leaf_cascade(
                        VertexId::from(vid_u),
                        label,
                        TestEdge { target: t },
                    )
                    .unwrap();
            }
            // Settled per vertex: leaf-mates must never reserve overlapping spans.
            graph.assert_no_labeled_leaf_mate_overlap(VertexId::from(vid_u));
        }
    }

    #[test]
    fn leaf_mate_overlap_guard_detects_injected_overlap() {
        let graph = hub_graph();
        for _ in 0..2 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }
        let label = BucketLabelKey::from_raw(7);
        for vid_u in 0..2u32 {
            graph
                .insert_edge_skip_leaf_cascade(VertexId::from(vid_u), label, TestEdge { target: 1 })
                .unwrap();
        }
        graph.assert_no_labeled_leaf_mate_overlap(VertexId::from(0));

        // Corrupt vid 1's reservation to start on top of vid 0's span, then confirm the
        // guard fires. This simulates the data-loss bug class the guard exists to catch.
        let v0 = graph.vertices().get(VertexId::from(0));
        let base0 = graph
            .read_vertex_label_buckets(&v0)
            .unwrap()
            .first()
            .unwrap()
            .edge_start();
        let v1 = graph.vertices().get(VertexId::from(1));
        let slot1 = v1.base_slot_start();
        let bucket1 = graph.buckets().read_label_bucket_slot(slot1).unwrap();
        graph
            .buckets()
            .write_label_bucket_slot(slot1, bucket1.with_edge_range(base0, bucket1.degree()))
            .unwrap();

        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            graph.assert_no_labeled_leaf_mate_overlap(VertexId::from(0));
        }));
        assert!(
            result.is_err(),
            "guard must panic on injected leaf-mate overlap"
        );
    }

    /// Upgrade-boundary corruption guard for the labeled engine (highest-risk
    /// relocation surface): drive multi-vertex, multi-label churn that forces
    /// overflow-log folds, leaf rebalances, and leaf/bucket relocations; reopen
    /// the same stable memories (as `post_upgrade` does); and require every
    /// per-label adjacency plus every structural invariant to survive — then
    /// mutate again to prove the reopened graph is fully operational.
    #[test]
    fn labeled_heavy_relocation_survives_reopen_without_corruption() {
        const VERTICES: u32 = 3;
        const LABELS: u16 = 4;
        const PER_LABEL: u32 = 20;
        let default_label = BucketLabelKey::from_raw(1);
        let elem_capacity = 1u64 << 20;

        let mems = labeled_memories();
        let graph = open_labeled_graph(&mems, elem_capacity, default_label);
        let mut ids = Vec::new();
        for _ in 0..VERTICES {
            ids.push(graph.push_vertex(LabeledVertex::default()).unwrap());
        }

        // Model: per vertex, per label, an ascending set of unique targets.
        let labels: Vec<BucketLabelKey> = (0..LABELS)
            .map(|l| BucketLabelKey::from_raw(10_000 + l))
            .collect();
        let mut expected: Vec<Vec<(BucketLabelKey, Vec<u32>)>> =
            vec![Vec::new(); VERTICES as usize];
        for (vi, &vid) in ids.iter().enumerate() {
            for (li, &label) in labels.iter().enumerate() {
                let mut targets = Vec::new();
                for i in 0..PER_LABEL {
                    let target = (vi as u32) * 100_000 + (li as u32) * 1_000 + i;
                    graph
                        .insert_edge(vid, label, TestEdge { target })
                        .unwrap_or_else(|e| panic!("insert v{vi} l{li} i{i}: {e:?}"));
                    targets.push(target);
                }
                expected[vi].push((label, targets));
            }
        }

        let check = |g: &LabeledLaraGraph<TestEdge, crate::VectorMemory>,
                     expected: &[Vec<(BucketLabelKey, Vec<u32>)>],
                     phase: &str| {
            crate::labeled::invariants::assert_labeled_layout_invariants(
                g.vertices(),
                g.buckets(),
                g.edges(),
            );
            crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
                g.vertices(),
                g.buckets(),
                g.edges(),
            );
            for (vi, &vid) in ids.iter().enumerate() {
                g.assert_no_labeled_leaf_mate_overlap(vid);
                g.assert_labeled_buckets_within_leaf_physical(vid).unwrap();
                for (label, want) in &expected[vi] {
                    let mut got: Vec<u32> = g
                        .iter_edges_for_label(vid, *label)
                        .unwrap()
                        .into_iter()
                        .map(|e| e.target)
                        .collect();
                    got.sort_unstable();
                    let mut want = want.clone();
                    want.sort_unstable();
                    assert_eq!(
                        got, want,
                        "{phase}: v{vi} label {label:?} adjacency diverged"
                    );
                }
            }
        };
        check(&graph, &expected, "pre-reopen");

        // Cross the upgrade boundary: drop the in-memory graph, reopen the bytes.
        drop(graph);
        let reopened = reopen_labeled_graph(&mems, elem_capacity, default_label);
        check(&reopened, &expected, "post-reopen");

        // Continued inserts after reopen must relocate without corrupting state.
        for (vi, &vid) in ids.iter().enumerate() {
            for (li, &label) in labels.iter().enumerate() {
                let target = (vi as u32) * 100_000 + (li as u32) * 1_000 + PER_LABEL;
                reopened
                    .insert_edge(vid, label, TestEdge { target })
                    .unwrap();
                expected[vi][li].1.push(target);
            }
        }
        check(&reopened, &expected, "post-reopen-mutation");
    }

    #[test]
    fn labeled_leaf_pma_density_matches_counts_store_when_pinned() {
        let graph = hub_graph();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge_skip_leaf_cascade(
                hub,
                BucketLabelKey::from_raw(10),
                TestEdge {
                    target: u32::from(dst),
                },
            )
            .unwrap();
        let counts = graph.leaf_segment_counts_for_vid(hub);
        let density = graph.labeled_leaf_pma_density(hub);
        assert_eq!(
            density,
            counts.actual.max(0) as f64 / counts.total.max(1) as f64
        );
    }
}
