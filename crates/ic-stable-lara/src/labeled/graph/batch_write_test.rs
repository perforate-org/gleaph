//! Dedicated tests for one-orientation batch writes.
//!
//! These tests are kept in a sibling module so they can import the crate-level
//! `GraphTestEdge` fixture without colliding with the `graph::test_support::GraphTestEdge`
//! used elsewhere in the labeled graph tests.

#[cfg(test)]
mod tests {
    use crate::VertexId;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::graph::batch_write::{
        OneOrientationBatchEdge, OneOrientationBatchPlan, OneOrientationBucketRun,
    };
    use crate::labeled::graph::test_support::TestEdge as GraphTestEdge;
    use crate::labeled::graph::test_support::test_graph_with_default;
    use crate::labeled::record::LabeledVertex;

    #[test]
    fn reserve_failure_leaves_canonical_state_unchanged_for_existing_bucket() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .insert_edge(VertexId::from(0), label, GraphTestEdge { target: 1 })
            .unwrap();

        let before = graph.out_edges(VertexId::from(0)).unwrap();

        use crate::labeled::graph::batch_write::OneOrientationBatchError;
        // A single-edge run that needs a window larger than the existing bucket's
        // stored_slots reservation is rejected before any canonical write.
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_value_width: 0,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(2),
                    label_id: label,
                    edge: GraphTestEdge { target: 2 },
                }],
            }],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::SlabCapacityExceeded),
            "expected SlabCapacityExceeded, got {err}"
        );

        let after = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn reserve_failure_leaves_canonical_state_unchanged() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        for i in 1..=3u32 {
            graph
                .insert_edge(VertexId::from(0), label, GraphTestEdge { target: i })
                .unwrap();
        }

        let before = graph.out_edges(VertexId::from(0)).unwrap();

        use crate::labeled::graph::batch_write::OneOrientationBatchError;
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_value_width: 0,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: GraphTestEdge { target: 10 },
                }],
            }],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::SlabCapacityExceeded),
            "expected SlabCapacityExceeded, got {err}"
        );

        let after = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(before, after);
    }
}
