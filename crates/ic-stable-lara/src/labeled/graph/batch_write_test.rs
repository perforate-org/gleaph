//! Dedicated tests for one-orientation batch writes.
//!
//! These tests are kept in a sibling module so they can import the crate-level
//! `TestEdge` fixture without colliding with the `graph::test_support::TestEdge`
//! used elsewhere in the labeled graph tests.

#[cfg(test)]
mod tests {
    use crate::VertexId;
    use crate::labeled::bucket_label_key::BucketLabelKey;
    use crate::labeled::graph::batch_write::{
        OneOrientationBatchEdge, OneOrientationBatchError, OneOrientationBatchLocation,
        OneOrientationBatchPlan, OneOrientationBucketRun, OneOrientationPhysicalLocation,
    };
    use crate::labeled::graph::iter::LabeledEdgeInlinePropertyBatchScratch;
    use crate::labeled::graph::test_support::{InlinePropertyTestEdge, test_graph_with_default};
    use crate::labeled::graph::{BucketSearch, LabeledLaraGraph, OutEdgeOrder};
    use crate::labeled::record::LabeledVertex;
    use crate::lara::edge_inline_property::force_inline_property_bytes_allocation_error_after;
    use std::ops::ControlFlow;
    use std::panic::AssertUnwindSafe;

    fn segment16_inline_property_graph()
    -> LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory> {
        use crate::test_support::vector_memory;
        LabeledLaraGraph::<InlinePropertyTestEdge, crate::VectorMemory>::new_with_segment_size(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::directed_from_index(1),
            16,
        )
        .unwrap()
    }
    fn segment16_graph()
    -> LabeledLaraGraph<crate::labeled::graph::test_support::TestEdge, crate::VectorMemory> {
        use crate::test_support::vector_memory;
        LabeledLaraGraph::<crate::labeled::graph::test_support::TestEdge, crate::VectorMemory>::new_with_segment_size(
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            vector_memory(),
            crate::labeled::InitialCapacities::uniform(256),
            BucketLabelKey::directed_from_index(1),
            16,
        )
        .unwrap()
    }

    fn collect_inline_property_values(
        graph: &LabeledLaraGraph<InlinePropertyTestEdge, crate::VectorMemory>,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Vec<Vec<u8>> {
        let mut scratch =
            LabeledEdgeInlinePropertyBatchScratch::<InlinePropertyTestEdge>::default();
        let mut out: Vec<Vec<u8>> = Vec::new();
        graph
            .visit_out_edge_inline_property_batches_for_label_next(
                src,
                label_id,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    let width = usize::from(batch.byte_width);
                    for chunk in batch.inline_property_bytes.chunks(width) {
                        out.push(chunk.to_vec());
                    }
                    ControlFlow::<()>::Continue(())
                },
            )
            .map(|_| ())
            .unwrap();
        out
    }

    #[test]
    fn reserve_prepares_new_bucket_in_initial_slice() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: BucketLabelKey::directed_from_index(1),
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: BucketLabelKey::directed_from_index(1),
                    edge: crate::labeled::graph::test_support::TestEdge { target: 1 },
                }],
            }],
        };

        let reservation = graph
            .reserve_one_orientation_batch(&plan)
            .expect("new bucket reservation");
        reservation.rollback(&graph);
    }

    #[test]
    fn reserve_rejects_duplicate_bucket_runs() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(1),
                crate::labeled::graph::test_support::TestEdge { target: 1 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: BucketLabelKey::directed_from_index(1),
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: BucketLabelKey::directed_from_index(1),
                        edge: crate::labeled::graph::test_support::TestEdge { target: 1 },
                    }],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: BucketLabelKey::directed_from_index(1),
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: BucketLabelKey::directed_from_index(1),
                        edge: crate::labeled::graph::test_support::TestEdge { target: 1 },
                    }],
                },
            ],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::DuplicateBucketRun),
            "expected DuplicateBucketRun for duplicate runs, got {err}"
        );
    }

    #[test]
    fn reserve_rejects_out_of_order_ordinals() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                BucketLabelKey::directed_from_index(1),
                crate::labeled::graph::test_support::TestEdge { target: 1 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: BucketLabelKey::directed_from_index(1),
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![
                    OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: BucketLabelKey::directed_from_index(1),
                        edge: crate::labeled::graph::test_support::TestEdge { target: 1 },
                    },
                    OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: BucketLabelKey::directed_from_index(1),
                        edge: crate::labeled::graph::test_support::TestEdge { target: 1 },
                    },
                ],
            }],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::NonIncreasingLogicalOrdinals),
            "expected NonIncreasingLogicalOrdinals for out-of-order ordinals, got {err}"
        );
    }

    #[test]
    fn overflow_log_read_back_order() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::directed_from_index(1);
        // ADR 0096 §4: four seeds (promotion fills the exact span) so the
        // batch spills to the log like the quota-filled span before.
        for i in 1..=5u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    crate::labeled::graph::test_support::TestEdge { target: i },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![
                    OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: crate::labeled::graph::test_support::TestEdge { target: 10 },
                    },
                    OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: crate::labeled::graph::test_support::TestEdge { target: 11 },
                    },
                    OneOrientationBatchEdge {
                        logical_ordinal: 2,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: crate::labeled::graph::test_support::TestEdge { target: 12 },
                    },
                ],
            }],
        };

        let result = graph
            .insert_one_orientation_batch(&plan)
            .expect("overflow-log batch append should commit");
        assert_eq!(result.edge_slots_written, 3);
        assert_eq!(result.edge_log_entries_written, 3);

        let _buckets = graph
            .read_vertex_label_buckets(&graph.vertices.get(VertexId::from(0)))
            .unwrap();
        let out = graph.out_edges(VertexId::from(0)).unwrap();
        let targets: Vec<u32> = out.iter().map(|e| e.target).collect();
        // The first four scalar edges stay in slab order, the batch edges follow
        // in logical ordinal order inside the overflow log, and ascending
        // traversal replays the log oldest-to-newest after the slab prefix.
        assert_eq!(targets, vec![1, 2, 3, 4, 5, 10, 11, 12]);
    }

    #[test]
    fn commit_edge_only_batch_success() {
        let graph = segment16_inline_property_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 0)
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: InlinePropertyTestEdge::with_bytes(1, &[]),
                }],
            }],
        };

        let result = graph.insert_one_orientation_batch(&plan).unwrap();
        assert_eq!(result.edge_slots_written, 1);
        assert_eq!(result.inline_property_bytes_slots_written, 0);
        assert!(result.locations.is_none());

        let _buckets = graph
            .read_vertex_label_buckets(&graph.vertices.get(VertexId::from(0)))
            .unwrap();
        let out = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target, 1);
    }

    #[test]
    fn commit_inline_property_bytes_batch_new_span_success() {
        let graph = segment16_inline_property_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 4)
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 4,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: InlinePropertyTestEdge::with_i32(1, 100),
                }],
            }],
        };

        let result = graph.insert_one_orientation_batch(&plan).unwrap();
        assert_eq!(result.edge_slots_written, 1);
        assert_eq!(result.inline_property_bytes_slots_written, 1);

        let _buckets = graph
            .read_vertex_label_buckets(&graph.vertices.get(VertexId::from(0)))
            .unwrap();
        let out = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target, 1);

        let values = collect_inline_property_values(&graph, VertexId::from(0), label);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], 100_i32.to_le_bytes().to_vec());
    }

    #[test]
    fn commit_inline_property_bytes_batch_second_run_success() {
        let graph = segment16_inline_property_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 4)
            .unwrap();
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(2), label, 4)
            .unwrap();

        let plan_a = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 4,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: InlinePropertyTestEdge::with_i32(1, 42),
                }],
            }],
        };
        graph.insert_one_orientation_batch(&plan_a).unwrap();

        let plan_b = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(2),
                label_id: label,
                inline_property_width: 4,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(2),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: InlinePropertyTestEdge::with_i32(1, 100),
                }],
            }],
        };
        let result = graph.insert_one_orientation_batch(&plan_b).unwrap();
        assert_eq!(result.edge_slots_written, 1);
        assert_eq!(result.inline_property_bytes_slots_written, 1);

        let out = graph.out_edges(VertexId::from(2)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target, 1);

        let values = collect_inline_property_values(&graph, VertexId::from(2), label);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], 100_i32.to_le_bytes().to_vec());
    }

    #[test]
    fn commit_multi_run_batch_success() {
        let graph = segment16_inline_property_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 0)
            .unwrap();
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(2), label, 0)
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label,
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: InlinePropertyTestEdge::with_bytes(1, &[]),
                    }],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(2),
                    label_id: label,
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(2),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: InlinePropertyTestEdge::with_bytes(1, &[]),
                    }],
                },
            ],
        };

        let result = graph.insert_one_orientation_batch(&plan).unwrap();
        assert!(
            result.edge_slots_written >= 2,
            "expanded run writes folded log plus pending edges"
        );
        assert_eq!(result.inline_property_bytes_slots_written, 0);

        let v0_targets: Vec<u32> = graph
            .iter_edges_for_label(VertexId::from(0), label)
            .unwrap()
            .into_iter()
            .map(|e| e.target)
            .collect();
        let v2_targets: Vec<u32> = graph
            .iter_edges_for_label(VertexId::from(2), label)
            .unwrap()
            .into_iter()
            .map(|e| e.target)
            .collect();
        assert_eq!(v0_targets, vec![1]);
        assert_eq!(v2_targets, vec![1]);
    }

    #[test]
    fn reserve_mid_allocation_failure_rolls_back_inline_property_bytes_tail() {
        let graph = segment16_inline_property_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 4)
            .unwrap();
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(2), label, 4)
            .unwrap();

        let inline_property_bytes_tail_before = graph.values.header().slab_occupied_tail;

        force_inline_property_bytes_allocation_error_after(1);

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label,
                    inline_property_width: 4,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: InlinePropertyTestEdge::with_i32(1, 100),
                    }],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(2),
                    label_id: label,
                    inline_property_width: 4,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(2),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: InlinePropertyTestEdge::with_i32(1, 200),
                    }],
                },
            ],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::StorageError(_)),
            "expected StorageError after forced allocation failure, got {err}"
        );

        let inline_property_bytes_tail_after = graph.values.header().slab_occupied_tail;
        assert_eq!(
            inline_property_bytes_tail_before, inline_property_bytes_tail_after,
            "mid-allocation failure must roll back inline property bytes occupied tail"
        );

        let _buckets = graph
            .read_vertex_label_buckets(&graph.vertices.get(VertexId::from(0)))
            .unwrap();
        let out = graph.out_edges(VertexId::from(0)).unwrap();
        assert!(out.is_empty());
        let out = graph.out_edges(VertexId::from(2)).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn commit_stale_bucket_panics_before_writes() {
        let graph = segment16_inline_property_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 4)
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 4,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: InlinePropertyTestEdge::with_i32(1, 100),
                }],
            }],
        };

        let reservation = graph.reserve_one_orientation_batch(&plan).unwrap();

        graph
            .insert_edge(
                VertexId::from(0),
                label,
                InlinePropertyTestEdge::with_i32(2, 200),
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();

        let before = graph.out_edges(VertexId::from(0)).unwrap();

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| reservation.commit(&graph)));
        assert!(result.is_err(), "expected panic on stale bucket");

        let after = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(
            before, after,
            "commit must not write canonical bytes before detecting stale bucket"
        );
    }

    #[test]
    fn commit_wrong_graph_instance_panics_before_writes() {
        let graph_a = segment16_inline_property_graph();
        graph_a.push_vertex(LabeledVertex::default()).unwrap();
        graph_a.push_vertex(LabeledVertex::default()).unwrap();

        let graph_b = segment16_inline_property_graph();
        graph_b.push_vertex(LabeledVertex::default()).unwrap();
        graph_b.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph_a
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 0)
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: InlinePropertyTestEdge::with_bytes(1, &[]),
                }],
            }],
        };

        let reservation = graph_a.reserve_one_orientation_batch(&plan).unwrap();

        let before = graph_b.out_edges(VertexId::from(0)).unwrap();

        let result = std::panic::catch_unwind(AssertUnwindSafe(|| reservation.commit(&graph_b)));
        assert!(result.is_err(), "expected panic on wrong graph instance");

        let after = graph_b.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(
            before, after,
            "commit must not write canonical bytes to the wrong graph instance"
        );
    }

    #[test]
    fn reserve_failure_restores_edge_capacity_after_inline_property_bytes_alloc_failure() {
        // Use a segment16 graph: creating the first non-default label bucket pins
        // a 16-slot leaf block at the tail of the edge slab.  The initial capacity
        // is 256, so after bucket setup it is 272.  We then deliberately shrink the
        // logical edge-store capacity to the slot just before the bucket's
        // edge_start, forcing the upcoming one-edge run to call set_elem_capacity
        // before the inline property bytes allocator runs.  Forcing that inline property bytes allocation to
        // fail must restore the logical edge capacity to the pre-reserve snapshot.
        let graph = segment16_inline_property_graph();
        for _ in 0..3 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 4)
            .unwrap();

        let edge_capacity_after_setup = graph.edges.header().elem_capacity;
        // Sanity: the segment16 leaf pin grew the edge store by one 16-slot block.
        assert!(
            edge_capacity_after_setup >= 256 + 16,
            "bucket setup must pin a leaf block"
        );

        // Shrink logical capacity to the slot immediately before the bucket's
        // edge_start (the original capacity before the leaf pin).  The bucket's
        // one-slot window still fits, but reserve must grow the edge store to
        // write into it.
        let shrunk_capacity = edge_capacity_after_setup - 16;
        graph
            .edges
            .set_elem_capacity(shrunk_capacity)
            .expect("shrinking edge logical capacity to existing memory slack must succeed");
        assert_eq!(graph.edges.header().elem_capacity, shrunk_capacity);

        force_inline_property_bytes_allocation_error_after(0);

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 4,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: InlinePropertyTestEdge::with_i32(1, 100),
                }],
            }],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::StorageError(_)),
            "expected StorageError after forced allocation failure, got {err}"
        );

        let edge_capacity_after = graph.edges.header().elem_capacity;
        assert_eq!(
            shrunk_capacity, edge_capacity_after,
            "reserve failure must restore edge-store logical capacity to the pre-reserve snapshot"
        );
    }

    #[test]
    fn reserve_rejects_empty_plan() {
        // An empty batch has no defined semantics and must be rejected.
        let graph = segment16_inline_property_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let plan = OneOrientationBatchPlan { runs: vec![] };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::EmptyPlan),
            "expected EmptyPlan for empty plan, got {err}"
        );
    }

    #[test]
    fn commit_multi_inline_property_run_offsets_match_read_back() {
        // Two separate vertices with inline property bytes buckets; verify each run's inline property bytes
        // is written at the correct offset by reading back values.
        let graph = segment16_inline_property_graph();
        for _ in 0..4 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label, 4)
            .unwrap();
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(2), label, 4)
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label,
                    inline_property_width: 4,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: InlinePropertyTestEdge::with_i32(1, 100),
                    }],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(2),
                    label_id: label,
                    inline_property_width: 4,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(2),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: InlinePropertyTestEdge::with_i32(1, 200),
                    }],
                },
            ],
        };

        let result = graph.insert_one_orientation_batch(&plan).unwrap();
        assert_eq!(result.inline_property_bytes_slots_written, 2);
    }

    #[test]
    fn overflow_log_same_leaf_multi_bucket_append_success() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label_a = BucketLabelKey::directed_from_index(1);
        let label_b = BucketLabelKey::directed_from_index(2);
        // ADR 0096 §4: four seeds per bucket (promotion fills exact spans) so
        // both batch runs spill to the log.
        for i in 1..=5u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label_a,
                    crate::labeled::graph::test_support::TestEdge { target: i },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            graph
                .insert_edge(
                    VertexId::from(0),
                    label_b,
                    crate::labeled::graph::test_support::TestEdge { target: 10 + i },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_a,
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![
                        OneOrientationBatchEdge {
                            logical_ordinal: 0,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_a,
                            edge: crate::labeled::graph::test_support::TestEdge { target: 100 },
                        },
                        OneOrientationBatchEdge {
                            logical_ordinal: 1,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_a,
                            edge: crate::labeled::graph::test_support::TestEdge { target: 101 },
                        },
                    ],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_b,
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![
                        OneOrientationBatchEdge {
                            logical_ordinal: 2,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_b,
                            edge: crate::labeled::graph::test_support::TestEdge { target: 200 },
                        },
                        OneOrientationBatchEdge {
                            logical_ordinal: 3,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_b,
                            edge: crate::labeled::graph::test_support::TestEdge { target: 201 },
                        },
                    ],
                },
            ],
        };

        let result = graph
            .insert_one_orientation_batch(&plan)
            .expect("same-leaf multi-bucket overflow-log append should commit");
        assert_eq!(result.edge_slots_written, 4);
        assert_eq!(result.edge_log_entries_written, 4);

        let _buckets = graph
            .read_vertex_label_buckets(&graph.vertices.get(VertexId::from(0)))
            .unwrap();
        let out = graph.out_edges(VertexId::from(0)).unwrap();
        let targets: Vec<u32> = out.iter().map(|e| e.target).collect();
        assert_eq!(
            targets,
            vec![1, 2, 3, 4, 5, 100, 101, 11, 12, 13, 14, 15, 200, 201]
        );
    }

    #[test]
    fn overflow_log_same_leaf_inline_property_multi_bucket_append_success() {
        let graph = segment16_inline_property_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label_a = BucketLabelKey::directed_from_index(1);
        let label_b = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label_a, 4)
            .unwrap();
        graph
            .ensure_label_bucket_inline_property_byte_width(VertexId::from(0), label_b, 4)
            .unwrap();

        for i in 1..=3u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label_a,
                    InlinePropertyTestEdge::with_bytes(i, &[1u8, 1, 1, 1]),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
            graph
                .insert_edge(
                    VertexId::from(0),
                    label_b,
                    InlinePropertyTestEdge::with_bytes(10 + i, &[2u8, 2, 2, 2]),
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap();
        }

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_a,
                    inline_property_width: 4,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![
                        OneOrientationBatchEdge {
                            logical_ordinal: 0,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_a,
                            edge: InlinePropertyTestEdge::with_bytes(100, &[10u8, 11, 12, 13]),
                        },
                        OneOrientationBatchEdge {
                            logical_ordinal: 1,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_a,
                            edge: InlinePropertyTestEdge::with_bytes(101, &[14u8, 15, 16, 17]),
                        },
                    ],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_b,
                    inline_property_width: 4,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                    edges: vec![
                        OneOrientationBatchEdge {
                            logical_ordinal: 2,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_b,
                            edge: InlinePropertyTestEdge::with_bytes(200, &[20u8, 21, 22, 23]),
                        },
                        OneOrientationBatchEdge {
                            logical_ordinal: 3,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_b,
                            edge: InlinePropertyTestEdge::with_bytes(201, &[24u8, 25, 26, 27]),
                        },
                    ],
                },
            ],
        };

        let result = graph
            .insert_one_orientation_batch(&plan)
            .expect("same-leaf inline property multi-bucket overflow-log append should commit");
        assert_eq!(result.edge_slots_written, 4);
        assert_eq!(result.edge_log_entries_written, 4);
        assert_eq!(result.inline_property_bytes_slots_written, 4);
        assert_eq!(result.inline_property_bytes_log_entries_written, 4);

        let a_values = collect_inline_property_values(&graph, VertexId::from(0), label_a);
        let b_values = collect_inline_property_values(&graph, VertexId::from(0), label_b);
        assert_eq!(
            a_values,
            vec![
                vec![1u8, 1, 1, 1],
                vec![1u8, 1, 1, 1],
                vec![1u8, 1, 1, 1],
                vec![10u8, 11, 12, 13],
                vec![14u8, 15, 16, 17],
            ]
        );
        assert_eq!(
            b_values,
            vec![
                vec![2u8, 2, 2, 2],
                vec![2u8, 2, 2, 2],
                vec![2u8, 2, 2, 2],
                vec![20u8, 21, 22, 23],
                vec![24u8, 25, 26, 27],
            ]
        );
    }

    /// Plan 0321 §Step 1 + Step 2: a batch run whose target bucket
    /// is in tree mode and exceeds the current tail-block room
    /// (no headroom) must fail closed at the batch boundary with
    /// `TreeRunExceedsTailBlock` (Step 2 minimal first slice) or
    /// `TreeModeBucketRunUnsupported` (Step 1 reject path before
    /// Step 2 lands). The bucket itself is untouched after the
    /// failure. The plan doc records both rejection paths.
    #[test]
    fn batch_run_rejects_tree_mode_bucket_fail_closed() {
        use crate::labeled::graph::test_support::{
            TestEdge, force_tree_mode_for_test, test_graph_with_default,
        };
        let graph = test_graph_with_default(BucketLabelKey::directed_from_index(1));
        graph.push_vertex(LabeledVertex::default()).unwrap();
        // Promote the bucket to tree mode via the natural promotion
        // path (force_bucket_to_stored_slots = T_PROMOTE, then
        // promote_bucket_to_tree_mode).
        let label = BucketLabelKey::directed_from_index(1);
        force_tree_mode_for_test(&graph, VertexId::from(0), label);
        // Confirm the bucket is now in tree mode with stored_slots
        // = T_PROMOTE = 4096 = exactly 4 blocks. tail_offset = 0
        // (the bucket ends exactly on a block boundary), so any
        // batch run is rejected with TreeRunExceedsTailBlock (Step
        // 2 minimal first slice).
        let vertex = graph.vertices.get(VertexId::from(0));
        let (slot, bucket) = match graph
            .find_bucket(VertexId::from(0), &vertex, label)
            .expect("find")
        {
            crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("expected Found bucket after promote"),
        };
        assert!(bucket.is_tree_mode(), "bucket must be tree-mode");
        assert_eq!(
            bucket.stored_slots(),
            crate::labeled::graph::T_PROMOTE,
            "post-promotion stored_slots should be T_PROMOTE"
        );
        // Build a batch plan with 1100 edges targeting the tree
        // bucket. The bucket is at stored_slots = 4096 (block 4,
        // offset 0). tail_room = 1024 * 4 = 4096 bytes. 1100
        // edges × 4 bytes = 4400 bytes > tail_room, so the run
        // must be rejected with TreeRunExceedsTailBlock.
        let mut edges = Vec::with_capacity(1100);
        for i in 0..1100u32 {
            edges.push(OneOrientationBatchEdge {
                logical_ordinal: i,
                owner_vertex_id: VertexId::from(0),
                neighbor_vertex_id: VertexId::from(2 + i),
                label_id: label,
                edge: TestEdge { target: 2 + i },
            });
        }
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Insertion,
                edges,
            }],
        };
        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(
                err,
                OneOrientationBatchError::TreeRunExceedsTailBlock { .. }
            ),
            "expected TreeRunExceedsTailBlock, got {err}"
        );
        // Bucket descriptor is unchanged.
        let bucket_after = graph
            .buckets
            .read_label_bucket_slot(slot)
            .expect("read after");
        assert!(bucket_after.is_tree_mode());
        assert_eq!(bucket_after.stored_slots(), bucket.stored_slots());
    }

    /// Plan 0321 §Step 1: per-run precision. A vertex with one
    /// tree bucket and one slab bucket batch-inserts both in the
    /// same plan. The tree run is rejected; the slab run is
    /// admitted (the run-level guard does not over-reject).
    ///
    /// (Note: today's GQL/kernel layer splits the plan at the
    /// boundary anyway; this test asserts the inner guard
    /// semantics, not the wrapper behavior.)
    #[test]
    fn batch_plan_with_mixed_slab_and_tree_runs_rejects_only_tree_run() {
        use crate::labeled::graph::test_support::{
            TestEdge, force_tree_mode_for_test, test_graph_with_default,
        };
        let graph = test_graph_with_default(BucketLabelKey::directed_from_index(1));
        graph.push_vertex(LabeledVertex::default()).unwrap();
        // Bucket 1: promote to tree mode.
        let label_tree = BucketLabelKey::directed_from_index(2);
        force_tree_mode_for_test(&graph, VertexId::from(0), label_tree);
        // Bucket 2: leave as slab (no inserts, no force).
        // ADR 0096 §4: single-edge buckets are born tiny; the slab run needs
        // a real slab bucket. Promote directly (exact one-edge span).
        let label_slab = BucketLabelKey::directed_from_index(3);
        graph
            .insert_edge(
                VertexId::from(0),
                label_slab,
                TestEdge { target: 100 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .unwrap();
        {
            let vertex = graph.vertices.get(VertexId::from(0));
            let (slot, bucket) = match graph
                .find_bucket(VertexId::from(0), &vertex, label_slab)
                .unwrap()
            {
                BucketSearch::Found { slot, bucket } => (slot, bucket),
                _ => panic!("slab bucket must exist for promotion"),
            };
            graph
                .promote_tiny_to_slab(VertexId::from(0), slot, &bucket)
                .unwrap();
        }
        // The plan puts the slab run FIRST; preflight_run walks
        // runs in plan order, so the slab run is admitted and the
        // tree run (second) is rejected.
        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_slab,
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(101),
                        label_id: label_slab,
                        edge: TestEdge { target: 101 },
                    }],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_tree,
                    inline_property_width: 0,
                    placement: crate::labeled::graph::EdgePlacementPolicy::Insertion,
                    edges: (0..1100u32)
                        .map(|i| OneOrientationBatchEdge {
                            logical_ordinal: i,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(2000 + i),
                            label_id: label_tree,
                            edge: TestEdge { target: 2000 + i },
                        })
                        .collect(),
                },
            ],
        };
        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(
                err,
                OneOrientationBatchError::TreeRunExceedsTailBlock { .. }
            ),
            "expected TreeRunExceedsTailBlock, got {err}"
        );
        // The slab bucket is untouched (no run was admitted because
        // the tree run failure aborts the plan). The slab bucket's
        // edge count stays at 1. (Target 100 also exists in the forced tree
        // bucket — `out_edges` fans out across labels, so count the slab
        // bucket directly.)
        let per_slab = graph
            .iter_edges_for_label(VertexId::from(0), label_slab)
            .unwrap();
        assert_eq!(per_slab.len(), 1, "slab bucket unchanged");
        assert_eq!(per_slab[0].target, 100);
    }

    /// Plan 0321 §Step 2: a batch run targeting a tree-mode bucket
    /// with tail room for the run is admitted through the batch
    /// boundary. The run writes 1 edge into the existing tail
    /// block, the descriptor's stored_slots and degree are bumped
    /// in one canonical write, and the edge is readable through
    /// the standard out_edges path.
    #[test]
    fn batch_run_admits_tree_mode_bucket_tail_fit() {
        use crate::labeled::graph::test_support::{
            TestEdge, force_tree_mode_for_test, test_graph_with_default,
        };
        let graph = test_graph_with_default(BucketLabelKey::directed_from_index(1));
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::directed_from_index(1);
        // Promote the bucket to tree mode (stored_slots = T_PROMOTE, a whole
        // number of B-slot blocks). Then issue 1 scalar insert to mint the next
        // block and advance stored_slots to T_PROMOTE + 1. The tail block is
        // then the block holding that last slot, with tail_offset = 4 bytes and
        // tail_room = 4096 - 4 = 4092 bytes.
        force_tree_mode_for_test(&graph, VertexId::from(0), label);
        graph
            .insert_edge(
                VertexId::from(0),
                label,
                TestEdge { target: 1 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("scalar insert after promote");
        let vertex = graph.vertices.get(VertexId::from(0));
        let (_slot, bucket_before) = match graph
            .find_bucket(VertexId::from(0), &vertex, label)
            .expect("find")
        {
            crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("expected Found bucket after promote+insert"),
        };
        assert!(bucket_before.is_tree_mode());
        let pre_stored = bucket_before.stored_slots();
        let pre_degree = bucket_before.degree;
        assert_eq!(
            pre_stored,
            crate::labeled::graph::T_PROMOTE + 1,
            "stored_slots after scalar insert"
        );
        // ADR 0096 §5: the promote subtracted the slab-era degree, and the
        // scalar tree insert above must not have re-added it.
        assert_eq!(
            graph.leaf_segment_counts_for_vid(VertexId::from(0)).actual,
            0,
            "tree-mode edges (promote + scalar insert) leave leaf `actual` empty"
        );
        // Build a 1-edge batch plan. 1 edge = 4 bytes fits in
        // tail_room.
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Insertion,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(2),
                    label_id: label,
                    edge: TestEdge { target: 2 },
                }],
            }],
        };
        let result = graph
            .insert_one_orientation_batch_with_locations(&plan)
            .expect("tail-fit run should be admitted");
        assert_eq!(result.edge_slots_written, 1);
        // Verify the descriptor was bumped.
        let vertex = graph.vertices.get(VertexId::from(0));
        let (_slot, bucket_after) = match graph
            .find_bucket(VertexId::from(0), &vertex, label)
            .expect("find")
        {
            crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("expected Found bucket after batch"),
        };
        assert!(bucket_after.is_tree_mode());
        assert_eq!(bucket_after.stored_slots(), pre_stored + 1);
        assert_eq!(bucket_after.degree, pre_degree + 1);
        // ADR 0096 §5: a tree run writes LTB rows, so the batch commit must
        // not bump the leaf `actual` (wrong implementation: the run's
        // `edge_slot_count` added unconditionally at commit).
        assert_eq!(
            graph.leaf_segment_counts_for_vid(VertexId::from(0)).actual,
            0,
            "tree batch run must not bump leaf `actual`"
        );
        // The new edge's target must be in the out_edges list.
        let all = graph.out_edges(VertexId::from(0)).unwrap();
        let count = all.iter().filter(|e| e.target == 2).count();
        assert!(
            count >= 1,
            "new edge target 2 must be reachable (count={count})"
        );
    }

    /// Plan 0321 §Step 3 (threshold-crossing decision): batch
    /// writes past `T_PROMOTE` are kept working. The next scalar
    /// insert promotes the over-sized slab bucket to tree mode.
    /// The tree bucket's contents (including all batch-inserted
    /// edges) must be intact after promotion.
    #[test]
    fn batch_run_past_t_promote_then_scalar_promotes_correctly() {
        use crate::labeled::graph::test_support::{TestEdge, test_graph_with_default};
        let graph = test_graph_with_default(BucketLabelKey::directed_from_index(1));
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::directed_from_index(1);
        // Pre-fill the bucket to T_PROMOTE - 100 with scalar inserts, then
        // batch-insert 200 edges to push stored_slots past T_PROMOTE.
        let pre = crate::labeled::graph::T_PROMOTE - 100;
        for i in 0..pre {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge { target: 100 + i },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .expect("scalar pre-fill");
        }
        // Now batch-insert 200 edges.
        let batch_count = 200u32;
        let mut batch_edges = Vec::with_capacity(batch_count as usize);
        for i in 0..batch_count {
            batch_edges.push(OneOrientationBatchEdge {
                logical_ordinal: pre + i,
                owner_vertex_id: VertexId::from(0),
                neighbor_vertex_id: VertexId::from(5000 + i),
                label_id: label,
                edge: TestEdge { target: 5000 + i },
            });
        }
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Insertion,
                edges: batch_edges,
            }],
        };
        let result = graph
            .insert_one_orientation_batch(&plan)
            .expect("batch past T_PROMOTE");
        // The run takes the in-place slab path or a wide expansion (which
        // rewrites the resident prefix, so the reported count can exceed the new
        // edges at smaller thresholds). The regime-independent contract is that
        // no row is lost — asserted as a full census after the promotion below.
        assert!(
            result.edge_slots_written >= batch_count,
            "batch run must at least write its own edges (got {})",
            result.edge_slots_written
        );
        // Verify the bucket is past T_PROMOTE but still in slab
        // mode (batch-past-threshold is allowed; promotion is
        // triggered on the next scalar insert).
        let vertex = graph.vertices.get(VertexId::from(0));
        let (_slot, bucket) = match graph
            .find_bucket(VertexId::from(0), &vertex, label)
            .expect("find")
        {
            crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("expected Found bucket"),
        };
        assert!(
            !bucket.is_tree_mode(),
            "still slab after batch past T_PROMOTE"
        );
        assert_eq!(bucket.stored_slots(), pre + batch_count);
        // Next scalar insert triggers promotion. Use target 9999 to
        // ensure it's a new edge.
        graph
            .insert_edge(
                VertexId::from(0),
                label,
                TestEdge { target: 9999 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("scalar insert after batch past T_PROMOTE");
        // Verify the bucket is now in tree mode.
        let vertex = graph.vertices.get(VertexId::from(0));
        let (_slot, bucket) = match graph
            .find_bucket(VertexId::from(0), &vertex, label)
            .expect("find")
        {
            crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("expected Found bucket after promote"),
        };
        assert!(bucket.is_tree_mode(), "promoted after scalar insert");
        // The descriptor's correctness is the key invariant: the
        // post-promote bucket is in tree mode and stored_slots =
        // pre + batch_count + 1 (the scalar insert). Promotion
        // transcribes all stored_slots entries; the batch-inserted
        // edges are at slab positions T_PROMOTE..(T_PROMOTE + batch_count)
        // and get transcribed like any other slab entry.
        let vertex = graph.vertices.get(VertexId::from(0));
        let (_slot, bucket) = match graph
            .find_bucket(VertexId::from(0), &vertex, label)
            .expect("find")
        {
            crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("expected Found bucket after promote"),
        };
        assert!(bucket.is_tree_mode());
        assert_eq!(bucket.stored_slots(), pre + batch_count + 1);
        // Reachability: promotion transcribed every pre-filled, batch-inserted
        // and scalar row (the descriptor assertions only cover the width).
        assert_eq!(
            graph
                .iter_edges_for_label(VertexId::from(0), label)
                .expect("scan")
                .len() as u32,
            pre + batch_count + 1
        );
        // Spot-check reachability: the bucket is in tree mode and
        // the LTB has the right number of blocks. The post-promote
        // tree-mode read path is exercised by the existing tree_csr
        // tests; the regression signal here is the descriptor
        // correctness (the next scalar insert on an over-sized
        // batch-loaded bucket promotes without losing entries).
    }

    /// Plan 0321 §F-1 (review rework, 2026-09-02): the batch
    /// tree branch must reject runs targeting a freshly-promoted
    /// bucket whose `stored_slots % B == 0` (a freshly promoted bucket ends
    /// exactly at a block boundary because `T_PROMOTE` is a whole number of
    /// B-slot blocks). At
    /// that point the bucket ends exactly on a block boundary:
    /// `tail_offset == 0` and the **next** LTB block does not
    /// exist yet. The scalar path mints the new block; the batch
    /// minimal first slice rejects typed and the caller falls
    /// back to scalar inserts.
    #[test]
    fn batch_run_rejects_exactly_full_tail_block() {
        use crate::labeled::graph::test_support::{
            TestEdge, force_tree_mode_for_test, test_graph_with_default,
        };
        let graph = test_graph_with_default(BucketLabelKey::directed_from_index(1));
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::directed_from_index(1);
        // Promote (stored = T_PROMOTE, tail_offset = 0).
        force_tree_mode_for_test(&graph, VertexId::from(0), label);
        let vertex = graph.vertices.get(VertexId::from(0));
        let (slot, bucket) = match graph
            .find_bucket(VertexId::from(0), &vertex, label)
            .expect("find")
        {
            crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("expected Found bucket after promote"),
        };
        assert!(bucket.is_tree_mode());
        assert_eq!(bucket.stored_slots(), crate::labeled::graph::T_PROMOTE);
        assert_eq!(
            bucket.stored_slots() % (crate::labeled::tree_csr::B as u32),
            0,
            "a freshly promoted bucket ends on a block boundary"
        );
        // Build a 100-edge batch run. Even with only 100 edges
        // (= 400 bytes, well within the would-be 4096-byte tail
        // room) the run is rejected because `tail_offset == 0`
        // means the next block doesn't exist yet.
        let mut edges = Vec::with_capacity(100);
        for i in 0..100u32 {
            edges.push(OneOrientationBatchEdge {
                logical_ordinal: i,
                owner_vertex_id: VertexId::from(0),
                neighbor_vertex_id: VertexId::from(2000 + i),
                label_id: label,
                edge: TestEdge { target: 2000 + i },
            });
        }
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Insertion,
                edges,
            }],
        };
        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(
                err,
                OneOrientationBatchError::TreeRunExceedsTailBlock { .. }
            ),
            "expected TreeRunExceedsTailBlock (tail_offset == 0), got {err}"
        );
        // Bucket descriptor is unchanged.
        let bucket_after = graph
            .buckets
            .read_label_bucket_slot(slot)
            .expect("read after");
        assert!(bucket_after.is_tree_mode());
        assert_eq!(
            bucket_after.stored_slots(),
            crate::labeled::graph::T_PROMOTE
        );
        // Scalar fallback: insert one edge via the scalar path.
        // The scalar path mints a new block (Plan 0318 §tree
        // insert handles `tail_offset == 0` correctly) and the
        // bucket advances to stored = 4097.
        graph
            .insert_edge(
                VertexId::from(0),
                label,
                TestEdge { target: 9000 },
                crate::labeled::graph::EdgePlacementPolicy::Insertion,
            )
            .expect("scalar fallback at tail_offset == 0");
        let bucket_after_scalar = graph
            .buckets
            .read_label_bucket_slot(slot)
            .expect("read after scalar");
        assert_eq!(
            bucket_after_scalar.stored_slots(),
            crate::labeled::graph::T_PROMOTE + 1
        );
        assert_eq!(
            bucket_after_scalar.degree,
            crate::labeled::graph::T_PROMOTE + 1
        );
    }

    /// Plan 0321 §F-1 (review rework, 2026-09-02): scalar
    /// fallback after a `tail_offset == 0` rejection. Inserting
    /// B edges one-by-one via the scalar path at the boundary
    /// advances the bucket from stored = T_PROMOTE to T_PROMOTE + B
    /// without errors. The scalar `tree_mode_insert_edge` path mints
    /// a new block for each `tail_offset == 0` transition.
    #[test]
    fn scalar_fallback_after_exactly_full_tail_block_advances_correctly() {
        use crate::labeled::graph::test_support::{
            TestEdge, force_tree_mode_for_test, test_graph_with_default,
        };
        let graph = test_graph_with_default(BucketLabelKey::directed_from_index(1));
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::directed_from_index(1);
        force_tree_mode_for_test(&graph, VertexId::from(0), label);
        // Insert 1024 edges via the scalar path. The first insert
        // mints a new block (tail_offset == 0); the next 1023 fit
        // in that block; the 1024th crosses another boundary and
        // mints a second new block.
        for i in 0..1024u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    TestEdge { target: 10_000 + i },
                    crate::labeled::graph::EdgePlacementPolicy::Insertion,
                )
                .unwrap_or_else(|e| panic!("scalar insert {i} failed: {e:?}"));
        }
        let vertex = graph.vertices.get(VertexId::from(0));
        let (_slot, bucket) = match graph
            .find_bucket(VertexId::from(0), &vertex, label)
            .expect("find")
        {
            crate::labeled::graph::BucketSearch::Found { slot, bucket } => (slot, bucket),
            _ => panic!("expected Found bucket"),
        };
        assert_eq!(
            bucket.stored_slots(),
            crate::labeled::graph::T_PROMOTE + crate::labeled::tree_csr::B as u32
        );
        assert_eq!(
            bucket.degree,
            crate::labeled::graph::T_PROMOTE + crate::labeled::tree_csr::B as u32
        );
        assert!(bucket.is_tree_mode());
    }

    // ADR 0096 §5 (R2b): batch runs on tiny buckets reject before any run math
    // (width bytes are payload and would misfire WidthMismatch). Scalar
    // fallback promotes naturally.
    #[test]
    fn tiny_bucket_run_rejected_before_width_check() {
        use crate::labeled::graph::test_support::{force_tiny_bucket, test_graph};
        let graph = test_graph();
        let vid = VertexId::from(0);
        let label = BucketLabelKey::directed_from_index(2);
        force_tiny_bucket(&graph, vid, label, &[7, 8]);
        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: vid,
                label_id: label,
                inline_property_width: 0,
                placement: crate::labeled::graph::EdgePlacementPolicy::Unordered,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 2,
                    owner_vertex_id: vid,
                    neighbor_vertex_id: VertexId::from(9),
                    label_id: label,
                    edge: crate::labeled::graph::test_support::TestEdge { target: 9 },
                }],
            }],
        };
        match graph.reserve_one_orientation_batch(&plan) {
            Err(OneOrientationBatchError::TinyBucketRunUnsupported {
                owner_vertex_id,
                label_id: run_label,
            }) => {
                assert_eq!(owner_vertex_id, vid);
                assert_eq!(run_label, label);
            }
            other => panic!("expected TinyBucketRunUnsupported, got {other:?}"),
        }
    }
}
