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

    /// Ensures the PMA leaf containing `vid` owns a contiguous edge-slab block and
    /// returns the first edge slot for that vertex's quota inside the block.
    pub(super) fn ensure_labeled_leaf_edge_physical_pin(
        &self,
        vid: VertexId,
    ) -> Result<u64, LabeledOperationError> {
        let header = self.edges.header();
        let seg = header.segment_size.max(1);
        let leaf = Self::leaf_index_for_vid(vid, seg);
        let block_len = labeled_leaf_physical_block_len(seg);
        let vertex_offset = labeled_vertex_edge_offset_in_leaf(vid, seg);

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

        checked_add_slot_index(physical_start, vertex_offset)
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
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
