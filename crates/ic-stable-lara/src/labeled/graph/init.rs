//! Labeled graph `init` implementation.

use crate::{
    VertexCount, VertexId,
    labeled::{
        bucket_label_key::{BucketDirectedness, BucketLabelKey},
        bucket_store::{DirectednessPartitionStrategy, LabelBucketStore},
        record::LabeledVertex,
    },
    lara::{
        edge::{EdgeStore, counts::SegmentEdgeCounts, segment_tree_leaf_count},
        edge_payload::EdgePayloadStore,
        operation_error::LaraOperationError,
        vertex::VertexStore,
    },
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;
use std::{cell::Cell, marker::PhantomData};

use super::error::{InitError, LabeledOperationError};
use super::{DEFAULT_SEGMENT_SIZE, LabeledLaraGraph};

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn edge_matches_label_lookup(candidate: &E, needle: &E) -> bool
    where
        E: PartialEq,
    {
        if candidate.neighbor_vid() != needle.neighbor_vid() {
            return false;
        }
        if let Some(label_id) = needle.edge_label_id_raw() {
            if candidate.edge_label_id_raw() != Some(label_id) {
                return false;
            }
            if candidate.edge_slot_index_raw() != needle.edge_slot_index_raw() {
                return false;
            }
            let width = needle.edge_payload_byte_width();
            if width != 0 {
                return candidate.edge_payload_byte_width() == width
                    && candidate.edge_payload_bytes() == needle.edge_payload_bytes();
            }
            return true;
        }
        let width = needle.edge_payload_byte_width();
        if width != 0 {
            return candidate.edge_payload_byte_width() == width
                && candidate.edge_payload_bytes() == needle.edge_payload_bytes();
        }
        candidate == needle
    }

    /// Creates a new labeled LARA graph over empty stable memories.
    pub fn new(
        vertices: M,
        buckets: M,
        bucket_free_spans: M,
        bucket_free_span_by_start: M,
        edge_counts: M,
        edges: M,
        edge_log: M,
        edge_span_meta: M,
        edge_free_spans: M,
        edge_free_span_by_start: M,
        payload_slab: M,
        value_free_spans: M,
        value_free_span_by_start: M,
        payload_log: M,
        value_blobs: M,
        elem_capacity: u64,
        default_label: BucketLabelKey,
    ) -> Result<Self, crate::GrowFailed> {
        crate::slab_index::validate_elem_capacity_grow_failed(elem_capacity, edges.size())?;
        let segment_count = segment_tree_leaf_count(VertexCount::default(), DEFAULT_SEGMENT_SIZE);
        Ok(Self {
            vertices: VertexStore::new(vertices)?,
            buckets: LabelBucketStore::new(
                buckets,
                bucket_free_spans,
                bucket_free_span_by_start,
                elem_capacity,
                DEFAULT_SEGMENT_SIZE,
            )?,
            edges: EdgeStore::new(
                edge_counts,
                edges,
                edge_log,
                edge_span_meta,
                edge_free_spans,
                edge_free_span_by_start,
                elem_capacity,
                DEFAULT_SEGMENT_SIZE,
                DEFAULT_SEGMENT_SIZE,
            )?,
            values: EdgePayloadStore::new(
                payload_slab,
                payload_log,
                value_blobs,
                value_free_spans,
                value_free_span_by_start,
                elem_capacity,
                segment_count,
            )?,
            default_label,
            last_bucket_lookup: Cell::new(None),
            bucket_lookup_cache: std::array::from_fn(|_| Cell::new(None)),
            _marker: PhantomData,
        })
    }

    /// Reopens a labeled LARA graph from existing stable memories.
    pub fn init(
        vertices: M,
        buckets: M,
        bucket_free_spans: M,
        bucket_free_span_by_start: M,
        edge_counts: M,
        edges: M,
        edge_log: M,
        edge_span_meta: M,
        edge_free_spans: M,
        edge_free_span_by_start: M,
        payload_slab: M,
        value_free_spans: M,
        value_free_span_by_start: M,
        payload_log: M,
        value_blobs: M,
        elem_capacity: u64,
        default_label: BucketLabelKey,
    ) -> Result<Self, InitError> {
        // The vertex column, bucket, edge, and payload subsystems are one
        // graph-owned composite that must be created or reopened together.
        // `value_blobs` is excluded: it may legitimately stay empty on reopen
        // (no wide payloads), and its Fresh-vs-Reopen asymmetry is enforced
        // inside `EdgePayloadStore::init`.
        match crate::classify_composite_init([
            vertices.size(),
            buckets.size(),
            bucket_free_spans.size(),
            bucket_free_span_by_start.size(),
            edge_counts.size(),
            edges.size(),
            edge_log.size(),
            edge_span_meta.size(),
            edge_free_spans.size(),
            edge_free_span_by_start.size(),
            payload_slab.size(),
            value_free_spans.size(),
            value_free_span_by_start.size(),
            payload_log.size(),
        ]) {
            crate::CompositeInit::Partial => return Err(InitError::PartialLayout),
            crate::CompositeInit::Fresh | crate::CompositeInit::Reopen => {}
        }
        let edges = EdgeStore::init(
            edge_counts,
            edges,
            edge_log,
            edge_span_meta,
            edge_free_spans,
            edge_free_span_by_start,
            elem_capacity,
            DEFAULT_SEGMENT_SIZE,
            DEFAULT_SEGMENT_SIZE,
        )
        .map_err(InitError::Edges)?;
        let edge_segment_count = edges.header().segment_count;
        Ok(Self {
            vertices: VertexStore::init(vertices).map_err(InitError::Vertices)?,
            buckets: LabelBucketStore::init(
                buckets,
                bucket_free_spans,
                bucket_free_span_by_start,
                elem_capacity,
                DEFAULT_SEGMENT_SIZE,
            )
            .map_err(InitError::Buckets)?,
            edges,
            values: EdgePayloadStore::init(
                payload_slab,
                payload_log,
                value_blobs,
                value_free_spans,
                value_free_span_by_start,
                elem_capacity,
                edge_segment_count,
            )
            .map_err(InitError::Payloads)?,
            default_label,
            last_bucket_lookup: Cell::new(None),
            bucket_lookup_cache: std::array::from_fn(|_| Cell::new(None)),
            _marker: PhantomData,
        })
    }

    /// Returns the stable vertex store.
    pub fn vertices(&self) -> &VertexStore<LabeledVertex, M> {
        &self.vertices
    }

    pub(crate) fn buckets(&self) -> &LabelBucketStore<M> {
        &self.buckets
    }

    /// Returns the stable edge store.
    pub fn edges(&self) -> &EdgeStore<E, M> {
        &self.edges
    }

    /// Returns the stable edge-payload store.
    pub fn values(&self) -> &EdgePayloadStore<M> {
        &self.values
    }

    /// Returns the label used for unlabeled/default edge storage.
    pub fn default_label(&self) -> BucketLabelKey {
        self.default_label
    }

    pub(super) fn vertex_prefix_end(&self, vid: VertexId) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() {
            crate::labeled::slot_index::checked_add_slot_index(
                vertex.base_slot_start(),
                u64::from(vertex.stored_degree()),
            )
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
        } else if vertex.degree() == 0 {
            Ok(vertex.base_slot_start())
        } else {
            let first = self
                .buckets
                .read_label_bucket_slot(vertex.base_slot_start())
                .ok_or(LaraOperationError::CollectAllocationOverflow)?;
            crate::labeled::slot_index::checked_add_slot_index(
                first.edge_start(),
                u64::from(vertex.stored_slots),
            )
            .ok_or(LaraOperationError::CollectAllocationOverflow.into())
        }
    }

    /// Returns the number of vertex rows in the graph.
    pub fn vertex_count(&self) -> VertexCount {
        VertexCount::from(self.vertices.len())
    }

    /// Total live logical edges stored for `vid` across all of its label buckets.
    ///
    /// O(number of distinct labels on the vertex), not O(edges): the vertex row's
    /// own `degree()` is a bucket count in labeled mode (and the edge count in
    /// default-edge/bypass mode), so the labeled case sums each bucket's logical
    /// degree.
    pub(crate) fn vertex_live_edge_count(
        &self,
        vid: VertexId,
    ) -> Result<u64, LabeledOperationError> {
        self.ensure_vertex(vid)?;
        let vertex = self.vertices.get(vid);
        if vertex.is_default_edge_labeled() {
            return Ok(u64::from(vertex.degree()));
        }
        let bucket_count = vertex.degree();
        let mut total = 0u64;
        for index in 0..bucket_count {
            let slot = Self::labeled_vertex_bucket_slot(&vertex, index)?;
            if let Some(bucket) = self.buckets.read_label_bucket_slot(slot) {
                total = total.saturating_add(u64::from(bucket.degree()));
            }
        }
        Ok(total)
    }

    pub(super) fn set_labeled_vertex(
        &self,
        vid: VertexId,
        vertex: LabeledVertex,
    ) -> Result<(), LabeledOperationError> {
        vertex.ensure_valid_normal_row()?;
        self.vertices.set(vid, &vertex);
        Ok(())
    }

    pub(super) fn ensure_vertex(&self, vid: VertexId) -> Result<(), LabeledOperationError> {
        if u32::from(vid) >= self.vertices.len() {
            return Err(LabeledOperationError::VertexOutOfRange {
                vid,
                len: self.vertex_count(),
            });
        }
        Ok(())
    }

    pub(super) fn leaf_index_for_vid(vid: VertexId, segment_size: u32) -> u32 {
        u32::from(vid) / segment_size.max(1)
    }

    pub(super) fn leaf_segment_counts_for_vid(&self, vid: VertexId) -> SegmentEdgeCounts {
        let header = self.edges.header();
        let leaf = Self::leaf_index_for_vid(vid, header.segment_size);
        let Some(idx) = leaf.checked_add(header.segment_count) else {
            return SegmentEdgeCounts {
                actual: 0,
                total: 0,
            };
        };
        self.edges.counts_store().get(u64::from(idx))
    }

    pub(super) fn directedness_partition_strategy(
        directedness: BucketDirectedness,
        ascending: bool,
    ) -> DirectednessPartitionStrategy {
        match (directedness, ascending) {
            (BucketDirectedness::Directed, false) => DirectednessPartitionStrategy::LinearFromEnd,
            (BucketDirectedness::Directed, true) => DirectednessPartitionStrategy::HybridBinary,
            (BucketDirectedness::Undirected, false) => DirectednessPartitionStrategy::HybridBinary,
            (BucketDirectedness::Undirected, true) => {
                DirectednessPartitionStrategy::LinearFromStart
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;

    #[allow(clippy::type_complexity)]
    fn labeled_memories() -> (
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
        crate::VectorMemory,
    ) {
        (
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
        )
    }

    #[test]
    fn init_rejects_partial_layout_when_vertices_wiped() {
        let label = BucketLabelKey::directed_from_index(1);
        let (v, bk, bfs, bfsbs, ec, e, el, esm, efs, efsbs, ps, vfs, vfsbs, pl, vb) =
            labeled_memories();
        LabeledLaraGraph::<TestEdge, _>::new(
            v.clone(),
            bk.clone(),
            bfs.clone(),
            bfsbs.clone(),
            ec.clone(),
            e.clone(),
            el.clone(),
            esm.clone(),
            efs.clone(),
            efsbs.clone(),
            ps.clone(),
            vfs.clone(),
            vfsbs.clone(),
            pl.clone(),
            vb.clone(),
            256,
            label,
        )
        .unwrap();
        // Every subsystem populated, vertex column wiped (e.g. a miswired MemoryId).
        let result = LabeledLaraGraph::<TestEdge, _>::init(
            mem(),
            bk,
            bfs,
            bfsbs,
            ec,
            e,
            el,
            esm,
            efs,
            efsbs,
            ps,
            vfs,
            vfsbs,
            pl,
            vb,
            256,
            label,
        );
        assert!(matches!(result, Err(InitError::PartialLayout)));
    }

    #[test]
    fn init_reopens_fully_populated_layout() {
        let label = BucketLabelKey::directed_from_index(1);
        let (v, bk, bfs, bfsbs, ec, e, el, esm, efs, efsbs, ps, vfs, vfsbs, pl, vb) =
            labeled_memories();
        LabeledLaraGraph::<TestEdge, _>::new(
            v.clone(),
            bk.clone(),
            bfs.clone(),
            bfsbs.clone(),
            ec.clone(),
            e.clone(),
            el.clone(),
            esm.clone(),
            efs.clone(),
            efsbs.clone(),
            ps.clone(),
            vfs.clone(),
            vfsbs.clone(),
            pl.clone(),
            vb.clone(),
            256,
            label,
        )
        .unwrap();
        let reopened = LabeledLaraGraph::<TestEdge, _>::init(
            v, bk, bfs, bfsbs, ec, e, el, esm, efs, efsbs, ps, vfs, vfsbs, pl, vb, 256, label,
        );
        assert!(reopened.is_ok());
    }

    #[test]
    fn label_edge_span_positioning_rejects_impossible_live_width() {
        let err =
            LabeledLaraGraph::<TestEdge, crate::VectorMemory>::calculate_label_edge_span_positions(
                0,
                1,
                &[LabelBucket::from_parts(
                    BucketLabelKey::from_raw(10),
                    0,
                    2,
                    2,
                    -1,
                )],
                None,
                0,
            )
            .expect_err("live edges wider than span must be rejected");

        assert!(matches!(
            err,
            LabeledOperationError::Store(LaraOperationError::CollectAllocationOverflow)
        ));
    }
}
