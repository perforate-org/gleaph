//! Labeled graph `bypass` implementation.

use crate::{
    VertexId,
    labeled::{
        bucket_label_key::{BUCKET_LABEL_INDEX_MASK, BucketLabelKey},
        record::{LabelBucket, LabeledVertex},
    },
    lara::operation_error::LaraOperationError,
    traits::{CsrEdge, CsrVertex},
};
use ic_stable_structures::Memory;

use super::error::LabeledOperationError;
use super::{DEFAULT_SEGMENT_SIZE, LabeledLaraGraph};

impl<E, M> LabeledLaraGraph<E, M>
where
    E: CsrEdge,
    M: Memory,
{
    pub(super) fn is_homogeneous_bypass_label(&self, label_id: BucketLabelKey) -> bool {
        let raw = label_id.raw();
        let default = self.default_label.raw();
        raw == default || raw == (default & BUCKET_LABEL_INDEX_MASK)
    }
    pub(super) fn may_use_homogeneous_bypass(&self, src: VertexId) -> bool {
        match u32::from(src).checked_add(1) {
            Some(next) => next >= self.vertices.len(),
            None => false,
        }
    }
    pub(super) fn bypass_storage_label_for(&self, vertex: &LabeledVertex) -> BucketLabelKey {
        vertex.bypass_storage_label(self.default_label)
    }
    pub(super) fn bump_successor_origins_after_bypass_end(
        &self,
        src: VertexId,
        region_end: u64,
    ) -> Result<(), LabeledOperationError> {
        let first = u32::from(src)
            .checked_add(1)
            .ok_or(LaraOperationError::CollectAllocationOverflow)?;
        for idx in first..self.vertices.len() {
            let vid = VertexId::from(idx);
            let successor = self.vertices.get(vid);
            if successor.is_default_edge_labeled() && successor.base_slot_start() < region_end {
                self.set_labeled_vertex(vid, successor.with_base_slot_start(region_end))?;
            }
        }
        Ok(())
    }
    pub(super) fn insert_homogeneous_bypass_edge(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        debug_assert!(vertex.is_default_edge_labeled());
        debug_assert_eq!(label_id, self.bypass_storage_label_for(&vertex));
        self.ensure_bypass_edge_origin(src)?;
        self.edges
            .insert_edge(&self.vertices, src, edge)
            .map_err(LabeledOperationError::from)?;
        let region_end = self.bypass_region_end(src)?;
        self.bump_successor_origins_after_bypass_end(src, region_end)
    }
    pub(super) fn bypass_region_end(&self, src: VertexId) -> Result<u64, LabeledOperationError> {
        let vertex = self.vertices.get(src);
        debug_assert!(vertex.is_default_edge_labeled());
        crate::labeled::slot_index::checked_add_slot_index(
            vertex.base_slot_start(),
            u64::from(vertex.stored_degree()),
        )
        .ok_or(LaraOperationError::CollectAllocationOverflow.into())
    }
    pub(super) fn ensure_bypass_edge_origin(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        if vertex.stored_degree() > 0 {
            return Ok(());
        }
        let edge_base = if u32::from(src) == 0 {
            0
        } else {
            let pred_idx = u32::from(src) - 1;
            self.vertex_prefix_end(VertexId::from(pred_idx))?
        };
        if edge_base != vertex.base_slot_start() {
            self.set_labeled_vertex(src, vertex.with_base_slot_start(edge_base))?;
        }
        Ok(())
    }
    pub(super) fn insert_homogeneous_bypass(
        &self,
        src: VertexId,
        label_id: BucketLabelKey,
        edge: E,
    ) -> Result<(), LabeledOperationError> {
        self.ensure_bypass_edge_origin(src)?;
        let vertex = self.vertices.get(src);
        self.set_labeled_vertex(src, vertex.with_homogeneous_bypass_label(label_id))?;
        self.insert_homogeneous_bypass_edge(src, label_id, edge)
    }
    pub(super) fn promote_bypass_to_bucket_mode(
        &self,
        src: VertexId,
    ) -> Result<(), LabeledOperationError> {
        let vertex = self.vertices.get(src);
        if !vertex.is_default_edge_labeled() {
            return Ok(());
        }
        let bypass_label = self.bypass_storage_label_for(&vertex);
        let edge_start = vertex.base_slot_start();
        let stored_slots = vertex.stored_slots;
        let logical_degree = vertex.degree;
        if logical_degree == 0 {
            // Clearing default-label bypass must also reset locator fields so the row is a
            // coherent empty *normal* bucket row (`base_slot_start` is LabelBucket slab space).
            let cleared = vertex
                .with_default_edge_labeled(false)
                .with_bucket_row_and_slack(0, 0, 0)
                .with_stored_slots(0);
            self.set_labeled_vertex(src, cleared)?;
            return Ok(());
        }

        // Bucket collection must not read edge slots while bypass is still active.
        self.set_labeled_vertex(src, LabeledVertex::default())?;

        let new_alloc = DEFAULT_SEGMENT_SIZE.max(stored_slots);
        let (_, rewrote_bucket_segment) = self.buckets.insert_label_bucket_at(
            &self.vertices,
            src,
            LabelBucket::from_parts(bypass_label, edge_start, logical_degree, stored_slots, -1),
            0,
        )?;
        if rewrote_bucket_segment {
            self.invalidate_bucket_lookup_caches_for_bucket_segment(src)?;
        }
        let updated = self
            .vertices
            .get(src)
            .with_degree(1)
            .with_stored_slots(new_alloc);
        self.set_labeled_vertex(src, updated)?;
        self.edges
            .bump_vertex_segment_counts(src, 0, i64::from(new_alloc))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::*;
    use super::*;
    use crate::VertexId;

    #[test]
    fn homogeneous_bypass_append_extends_edge_capacity() {
        let default = BucketLabelKey::from_raw(7);
        let graph = LabeledLaraGraph::new(
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
            1,
            default,
        )
        .unwrap();
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();

        for target in 0..4 {
            graph
                .insert_edge(hub, default, TestEdge { target })
                .unwrap_or_else(|e| panic!("insert target={target}: {e:?}"));
        }

        assert_eq!(graph.vertices().get(hub).degree(), 4);
        assert!(graph.edges().header().elem_capacity >= 4);
        assert_eq!(graph.iter_edges_for_label(hub, default).unwrap().len(), 4);
    }
    #[test]
    fn out_edges_by_directedness_bypass_empty_when_directedness_mismatches() {
        use crate::labeled::BucketDirectedness;

        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(
                VertexId::from(1),
                graph.default_label(),
                TestEdge { target: 9 },
            )
            .unwrap();
        assert!(
            graph
                .iter_out_edges_by_directedness(
                    VertexId::from(1),
                    BucketDirectedness::Undirected,
                    OutEdgeOrder::Descending,
                )
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            graph
                .iter_out_edges_by_directedness(
                    VertexId::from(1),
                    BucketDirectedness::Directed,
                    OutEdgeOrder::Descending,
                )
                .unwrap(),
            vec![TestEdge { target: 9 }]
        );
    }
    #[test]
    fn mixed_default_bypass_and_normal_labeled_pma_counts_stay_consistent() {
        let graph = test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::from_raw(2),
                TestEdge { target: 1 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(1),
                graph.default_label(),
                TestEdge { target: 2 },
            )
            .unwrap();
        graph.enable_default_edge_bypass(VertexId::from(1)).unwrap();
        graph
            .insert_edge(
                VertexId::from(1),
                graph.default_label(),
                TestEdge { target: 3 },
            )
            .unwrap();
        crate::labeled::invariants::assert_labeled_edge_store_pma_counts(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }
    #[test]
    fn default_bypass_points_directly_into_edge_csr() {
        let graph = test_graph();
        graph.enable_default_edge_bypass(VertexId::from(0)).unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                graph.default_label(),
                TestEdge { target: 7 },
            )
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 1);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), graph.default_label())
                .unwrap(),
            vec![TestEdge { target: 7 }]
        );
    }
    #[test]
    fn bypass_grow_does_not_repoint_bucket_mode_successor_bucket_base() {
        let graph = LabeledLaraGraph::new(
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
            1 << 16,
            BucketLabelKey::from_raw(1),
        )
        .unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let road = BucketLabelKey::from_raw(42);
        let hub = graph.push_vertex(LabeledVertex::default()).unwrap();
        let dst = graph.push_vertex(LabeledVertex::default()).unwrap();
        let mut prefixes = Vec::new();
        for _ in 0..8 {
            prefixes.push(graph.push_vertex(LabeledVertex::default()).unwrap());
        }
        for &prefix in &prefixes {
            graph
                .insert_edge(
                    prefix,
                    road,
                    TestEdge {
                        target: u32::from(hub),
                    },
                )
                .unwrap();
        }
        let src = VertexId::from(0);
        for (i, &prefix) in prefixes.iter().enumerate() {
            graph
                .insert_edge(
                    src,
                    road,
                    TestEdge {
                        target: u32::from(prefix),
                    },
                )
                .unwrap();
            let bucket_base = graph.vertices().get(prefix).base_slot_start();
            graph
                .buckets()
                .read_label_bucket_slot(bucket_base)
                .expect("prefix bucket still readable after src bypass growth");
            assert_eq!(
                graph.vertices().get(prefix).degree(),
                1,
                "prefix {i} still has one label bucket"
            );
        }
        graph
            .insert_edge(
                hub,
                road,
                TestEdge {
                    target: u32::from(dst),
                },
            )
            .unwrap();
        assert!(!graph.vertices().get(hub).is_default_edge_labeled());
        assert_eq!(graph.vertices().get(hub).degree(), 1);
        assert_eq!(
            graph.iter_edges_for_label(hub, road).unwrap(),
            vec![TestEdge {
                target: u32::from(dst)
            }]
        );
    }
    #[test]
    fn first_homogeneous_insert_enters_bypass_without_enable() {
        let graph = test_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(
                VertexId::from(1),
                graph.default_label(),
                TestEdge { target: 9 },
            )
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(1));
        assert!(vertex.is_default_edge_labeled());
        assert!(!vertex.is_bypass_undirected());
        assert_eq!(vertex.degree(), 1);
        assert_eq!(
            graph.out_edge_label_ids(VertexId::from(1)).unwrap(),
            vec![graph.default_label()]
        );
        let earlier = graph.vertices().get(VertexId::from(0));
        assert!(!earlier.is_default_edge_labeled());
    }
    #[test]
    fn non_tail_single_label_insert_does_not_rebase_successor_bypass_edges() {
        let graph = test_graph();
        let successor = graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(successor, graph.default_label(), TestEdge { target: 900 })
            .unwrap();

        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 10 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 11 })
            .unwrap();

        assert!(
            !graph
                .vertices()
                .get(VertexId::from(0))
                .is_default_edge_labeled()
        );
        assert_eq!(
            graph
                .iter_edges_for_label(successor, graph.default_label())
                .unwrap(),
            vec![TestEdge { target: 900 }]
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 11 }, TestEdge { target: 10 }]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }
    #[test]
    fn homogeneous_undirected_bypass_and_promotion_on_named_label() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        let undirected = BucketLabelKey::UNLABELED_UNDIRECTED;
        graph
            .insert_edge(VertexId::from(0), undirected, TestEdge { target: 1 })
            .unwrap();
        graph
            .insert_edge(VertexId::from(0), undirected, TestEdge { target: 2 })
            .unwrap();
        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(vertex.is_default_edge_labeled());
        assert!(vertex.is_bypass_undirected());
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), undirected)
                .unwrap(),
            vec![TestEdge { target: 2 }, TestEdge { target: 1 }]
        );

        let road = BucketLabelKey::from_raw(2);
        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 99 })
            .unwrap();
        let after = graph.vertices().get(VertexId::from(0));
        assert!(!after.is_default_edge_labeled());
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), undirected)
                .unwrap(),
            vec![TestEdge { target: 2 }, TestEdge { target: 1 }]
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 99 }]
        );
        crate::labeled::invariants::assert_labeled_layout_invariants(
            graph.vertices(),
            graph.buckets(),
            graph.edges(),
        );
    }
    #[test]
    fn bypass_accumulates_many_slab_tombstones_without_promotion() {
        let graph = test_graph();
        let default = graph.default_label();
        let total = 202u32;
        for target in 1..=total {
            graph
                .insert_edge(VertexId::from(0), default, TestEdge { target })
                .unwrap();
        }

        for target in 1..=200 {
            assert!(
                graph
                    .remove_edge_matching(VertexId::from(0), default, |edge| edge.target == target)
                    .unwrap()
                    .is_some()
            );
        }

        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(vertex.stored_slots.saturating_sub(vertex.degree), 200);
        assert_eq!(vertex.degree(), 2);
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), default)
                .unwrap(),
            vec![TestEdge { target: 202 }, TestEdge { target: 201 }]
        );
    }
    #[test]
    fn empty_bypass_promotes_as_empty_when_next_insert_uses_different_label() {
        let graph = test_graph();
        let default = graph.default_label();
        let road = BucketLabelKey::from_raw(42);

        graph
            .insert_edge(VertexId::from(0), default, TestEdge { target: 10 })
            .unwrap();
        assert!(
            graph
                .remove_edge_matching(VertexId::from(0), default, |edge| edge.target == 10)
                .unwrap()
                .is_some()
        );

        let vertex = graph.vertices().get(VertexId::from(0));
        assert!(vertex.is_default_edge_labeled());
        assert_eq!(vertex.degree(), 0);
        assert_eq!(vertex.stored_degree(), 1);

        graph
            .insert_edge(VertexId::from(0), road, TestEdge { target: 20 })
            .unwrap();

        assert_eq!(
            graph.out_edge_label_ids(VertexId::from(0)).unwrap(),
            vec![road]
        );
        assert_eq!(
            graph.iter_edges_for_label(VertexId::from(0), road).unwrap(),
            vec![TestEdge { target: 20 }]
        );
        assert!(
            graph
                .iter_edges_for_label(VertexId::from(0), default)
                .unwrap()
                .is_empty()
        );
    }
}
