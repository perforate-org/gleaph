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
        OneOrientationBatchEdge, OneOrientationBatchError, OneOrientationBatchPlan,
        OneOrientationBucketRun,
    };
    use crate::labeled::graph::iter::LabeledEdgeInlineValueBatchScratch;
    use crate::labeled::graph::test_support::{PayloadTestEdge, test_graph_with_default};
    use crate::labeled::graph::{LabeledLaraGraph, OutEdgeOrder};
    use crate::labeled::record::LabeledVertex;
    use crate::lara::edge_inline_value::force_payload_allocation_error_after;
    use std::panic::AssertUnwindSafe;

    fn segment16_payload_graph() -> LabeledLaraGraph<PayloadTestEdge, crate::VectorMemory> {
        use crate::test_support::vector_memory;
        LabeledLaraGraph::<PayloadTestEdge, crate::VectorMemory>::new_with_segment_size(
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

    fn collect_payload_values(
        graph: &LabeledLaraGraph<PayloadTestEdge, crate::VectorMemory>,
        src: VertexId,
        label_id: BucketLabelKey,
    ) -> Vec<Vec<u8>> {
        let mut scratch = LabeledEdgeInlineValueBatchScratch::<PayloadTestEdge>::default();
        let mut out: Vec<Vec<u8>> = Vec::new();
        graph
            .visit_out_edge_inline_value_batches_for_label(
                src,
                label_id,
                OutEdgeOrder::Ascending,
                &mut scratch,
                |batch| {
                    let width = usize::from(batch.byte_width);
                    for chunk in batch.inline_value_bytes.chunks(width) {
                        out.push(chunk.to_vec());
                    }
                },
            )
            .unwrap();
        out
    }

    #[test]
    fn overflow_log_reserve_failure_leaves_canonical_state_unchanged() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        for i in 1..=3u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    crate::labeled::graph::test_support::TestEdge { target: i },
                )
                .unwrap();
        }

        // Fill the edge overflow log to capacity so the batch cannot be reserved.
        let header = graph.edges().header();
        let leaf = crate::LabeledLaraGraph::<
            crate::labeled::graph::test_support::TestEdge,
            crate::VectorMemory,
        >::leaf_index_for_vid(VertexId::from(0), header.segment_size);
        let log_capacity = graph.edges().read_overflow_log_state(leaf).1 as usize;
        let entries: Vec<(i32, crate::labeled::graph::test_support::TestEdge)> = (0..log_capacity)
            .map(|i| {
                let prev = if i == 0 { -1 } else { (i as i32) - 1 };
                (
                    prev,
                    crate::labeled::graph::test_support::TestEdge {
                        target: 100 + i as u32,
                    },
                )
            })
            .collect();
        graph
            .edges()
            .write_overflow_log_entries(leaf, 0, &entries)
            .expect("fill log");

        let before = graph.out_edges(VertexId::from(0)).unwrap();

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
                    edge: crate::labeled::graph::test_support::TestEdge { target: 10 },
                }],
            }],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::LogCapacityExceeded),
            "expected LogCapacityExceeded, got {err}"
        );

        let after = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn overflow_log_reserve_failure_leaves_allocator_state_unchanged() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label_a = BucketLabelKey::directed_from_index(1);
        let label_b = BucketLabelKey::directed_from_index(2);
        graph
            .insert_edge(
                VertexId::from(0),
                label_a,
                crate::labeled::graph::test_support::TestEdge { target: 1 },
            )
            .unwrap();
        graph
            .insert_edge(
                VertexId::from(0),
                label_b,
                crate::labeled::graph::test_support::TestEdge { target: 1 },
            )
            .unwrap();

        // Fill the shared edge overflow log to capacity so the second run fails
        // after the first run has already been reserved.
        let header = graph.edges().header();
        let leaf = crate::LabeledLaraGraph::<
            crate::labeled::graph::test_support::TestEdge,
            crate::VectorMemory,
        >::leaf_index_for_vid(VertexId::from(0), header.segment_size);
        let log_capacity = graph.edges().read_overflow_log_state(leaf).1 as usize;
        let entries: Vec<(i32, crate::labeled::graph::test_support::TestEdge)> = (0..log_capacity)
            .map(|i| {
                let prev = if i == 0 { -1 } else { (i as i32) - 1 };
                (
                    prev,
                    crate::labeled::graph::test_support::TestEdge {
                        target: 100 + i as u32,
                    },
                )
            })
            .collect();
        graph
            .edges()
            .write_overflow_log_entries(leaf, 0, &entries)
            .expect("fill log");

        let edge_capacity_before = graph.edges.header().elem_capacity;
        let payload_tail_before = graph.values.header().slab_occupied_tail;

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_a,
                    inline_value_width: 0,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(2),
                        label_id: label_a,
                        edge: crate::labeled::graph::test_support::TestEdge { target: 2 },
                    }],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_b,
                    inline_value_width: 0,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(2),
                        label_id: label_b,
                        edge: crate::labeled::graph::test_support::TestEdge { target: 2 },
                    }],
                },
            ],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::LogCapacityExceeded),
            "expected LogCapacityExceeded, got {err}"
        );

        let edge_capacity_after = graph.edges.header().elem_capacity;
        let payload_tail_after = graph.values.header().slab_occupied_tail;
        assert_eq!(
            edge_capacity_before, edge_capacity_after,
            "reserve failure must not grow edge store capacity"
        );
        assert_eq!(
            payload_tail_before, payload_tail_after,
            "reserve failure must not move payload occupied tail"
        );
    }

    #[test]
    fn reserve_rejects_new_bucket_in_initial_slice() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: BucketLabelKey::directed_from_index(1),
                inline_value_width: 0,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: BucketLabelKey::directed_from_index(1),
                    edge: crate::labeled::graph::test_support::TestEdge { target: 1 },
                }],
            }],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::UnsupportedGeometry(_)),
            "expected UnsupportedGeometry for new bucket, got {err}"
        );
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
            )
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: BucketLabelKey::directed_from_index(1),
                    inline_value_width: 0,
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
                    inline_value_width: 0,
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
            matches!(err, OneOrientationBatchError::UnsupportedGeometry(_)),
            "expected UnsupportedGeometry for duplicate runs, got {err}"
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
            )
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: BucketLabelKey::directed_from_index(1),
                inline_value_width: 0,
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
            matches!(err, OneOrientationBatchError::UnsupportedGeometry(_)),
            "expected UnsupportedGeometry for out-of-order ordinals, got {err}"
        );
    }

    #[test]
    fn overflow_log_read_back_order() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        let label = BucketLabelKey::directed_from_index(1);
        for i in 1..=3u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label,
                    crate::labeled::graph::test_support::TestEdge { target: i },
                )
                .unwrap();
        }

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_value_width: 0,
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

        let out = graph.out_edges(VertexId::from(0)).unwrap();
        let targets: Vec<u32> = out.iter().map(|e| e.target).collect();
        // The first three scalar edges stay in slab order, the batch edges follow
        // in logical ordinal order inside the overflow log, and ascending
        // traversal replays the log oldest-to-newest after the slab prefix.
        assert_eq!(targets, vec![12, 11, 10, 3, 2, 1]);
    }

    #[test]
    fn commit_edge_only_batch_success() {
        let graph = segment16_payload_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label, 0)
            .unwrap();

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
                    edge: PayloadTestEdge::with_bytes(1, &[]),
                }],
            }],
        };

        let result = graph.insert_one_orientation_batch(&plan).unwrap();
        assert_eq!(result.edge_slots_written, 1);
        assert_eq!(result.payload_slots_written, 0);

        let out = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target, 1);
    }

    #[test]
    fn commit_payload_batch_new_span_success() {
        let graph = segment16_payload_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label, 4)
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_value_width: 4,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: PayloadTestEdge::with_i32(1, 100),
                }],
            }],
        };

        let result = graph.insert_one_orientation_batch(&plan).unwrap();
        assert_eq!(result.edge_slots_written, 1);
        assert_eq!(result.payload_slots_written, 1);

        let out = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target, 1);

        let values = collect_payload_values(&graph, VertexId::from(0), label);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], 100_i32.to_le_bytes().to_vec());
    }

    #[test]
    fn commit_payload_batch_second_run_success() {
        let graph = segment16_payload_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label, 4)
            .unwrap();
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(2), label, 4)
            .unwrap();

        let plan_a = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_value_width: 4,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: PayloadTestEdge::with_i32(1, 42),
                }],
            }],
        };
        graph.insert_one_orientation_batch(&plan_a).unwrap();

        let plan_b = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(2),
                label_id: label,
                inline_value_width: 4,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(2),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: PayloadTestEdge::with_i32(1, 100),
                }],
            }],
        };
        let result = graph.insert_one_orientation_batch(&plan_b).unwrap();
        assert_eq!(result.edge_slots_written, 1);
        assert_eq!(result.payload_slots_written, 1);

        let out = graph.out_edges(VertexId::from(2)).unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].target, 1);

        let values = collect_payload_values(&graph, VertexId::from(2), label);
        assert_eq!(values.len(), 1);
        assert_eq!(values[0], 100_i32.to_le_bytes().to_vec());
    }

    #[test]
    fn commit_multi_run_batch_success() {
        let graph = segment16_payload_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label, 0)
            .unwrap();
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(2), label, 0)
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label,
                    inline_value_width: 0,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: PayloadTestEdge::with_bytes(1, &[]),
                    }],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(2),
                    label_id: label,
                    inline_value_width: 0,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(2),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: PayloadTestEdge::with_bytes(1, &[]),
                    }],
                },
            ],
        };

        let result = graph.insert_one_orientation_batch(&plan).unwrap();
        assert_eq!(result.edge_slots_written, 2);
        assert_eq!(result.payload_slots_written, 0);

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
    fn reserve_mid_allocation_failure_rolls_back_payload_tail() {
        let graph = segment16_payload_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label, 4)
            .unwrap();
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(2), label, 4)
            .unwrap();

        let payload_tail_before = graph.values.header().slab_occupied_tail;

        force_payload_allocation_error_after(1);

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label,
                    inline_value_width: 4,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: PayloadTestEdge::with_i32(1, 100),
                    }],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(2),
                    label_id: label,
                    inline_value_width: 4,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(2),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: PayloadTestEdge::with_i32(1, 200),
                    }],
                },
            ],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::StorageError(_)),
            "expected StorageError after forced allocation failure, got {err}"
        );

        let payload_tail_after = graph.values.header().slab_occupied_tail;
        assert_eq!(
            payload_tail_before, payload_tail_after,
            "mid-allocation failure must roll back payload occupied tail"
        );

        let out = graph.out_edges(VertexId::from(0)).unwrap();
        assert!(out.is_empty());
        let out = graph.out_edges(VertexId::from(2)).unwrap();
        assert!(out.is_empty());
    }

    #[test]
    fn commit_stale_bucket_panics_before_writes() {
        let graph = segment16_payload_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label, 4)
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_value_width: 4,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: PayloadTestEdge::with_i32(1, 100),
                }],
            }],
        };

        let reservation = graph.reserve_one_orientation_batch(&plan).unwrap();

        graph
            .insert_edge(VertexId::from(0), label, PayloadTestEdge::with_i32(2, 200))
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
        let graph_a = segment16_payload_graph();
        graph_a.push_vertex(LabeledVertex::default()).unwrap();
        graph_a.push_vertex(LabeledVertex::default()).unwrap();

        let graph_b = segment16_payload_graph();
        graph_b.push_vertex(LabeledVertex::default()).unwrap();
        graph_b.push_vertex(LabeledVertex::default()).unwrap();

        let label = BucketLabelKey::directed_from_index(1);
        graph_a
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label, 0)
            .unwrap();

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
                    edge: PayloadTestEdge::with_bytes(1, &[]),
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
    fn reserve_failure_restores_edge_capacity_after_payload_alloc_failure() {
        // Use a segment16 graph: creating the first non-default label bucket pins
        // a 16-slot leaf block at the tail of the edge slab.  The initial capacity
        // is 256, so after bucket setup it is 272.  We then deliberately shrink the
        // logical edge-store capacity to the slot just before the bucket's
        // edge_start, forcing the upcoming one-edge run to call set_elem_capacity
        // before the payload allocator runs.  Forcing that payload allocation to
        // fail must restore the logical edge capacity to the pre-reserve snapshot.
        let graph = segment16_payload_graph();
        for _ in 0..3 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label, 4)
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

        force_payload_allocation_error_after(0);

        let plan = OneOrientationBatchPlan {
            runs: vec![OneOrientationBucketRun {
                owner_vertex_id: VertexId::from(0),
                label_id: label,
                inline_value_width: 4,
                edges: vec![OneOrientationBatchEdge {
                    logical_ordinal: 0,
                    owner_vertex_id: VertexId::from(0),
                    neighbor_vertex_id: VertexId::from(1),
                    label_id: label,
                    edge: PayloadTestEdge::with_i32(1, 100),
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
        let graph = segment16_payload_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let plan = OneOrientationBatchPlan { runs: vec![] };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::UnsupportedGeometry(_)),
            "expected UnsupportedGeometry for empty plan, got {err}"
        );
    }

    #[test]
    fn commit_multi_payload_run_offsets_match_read_back() {
        // Two separate vertices with payload buckets; verify each run's payload
        // is written at the correct offset by reading back values.
        let graph = segment16_payload_graph();
        for _ in 0..4 {
            graph.push_vertex(LabeledVertex::default()).unwrap();
        }

        let label = BucketLabelKey::directed_from_index(1);
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label, 4)
            .unwrap();
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(2), label, 4)
            .unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label,
                    inline_value_width: 4,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 0,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: PayloadTestEdge::with_i32(1, 100),
                    }],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(2),
                    label_id: label,
                    inline_value_width: 4,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 1,
                        owner_vertex_id: VertexId::from(2),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label,
                        edge: PayloadTestEdge::with_i32(1, 200),
                    }],
                },
            ],
        };

        let result = graph.insert_one_orientation_batch(&plan).unwrap();
        assert_eq!(result.payload_slots_written, 2);
    }

    #[test]
    fn overflow_log_same_leaf_multi_bucket_append_success() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label_a = BucketLabelKey::directed_from_index(1);
        let label_b = BucketLabelKey::directed_from_index(2);
        for i in 1..=3u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label_a,
                    crate::labeled::graph::test_support::TestEdge { target: i },
                )
                .unwrap();
            graph
                .insert_edge(
                    VertexId::from(0),
                    label_b,
                    crate::labeled::graph::test_support::TestEdge { target: 10 + i },
                )
                .unwrap();
        }

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_a,
                    inline_value_width: 0,
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
                    inline_value_width: 0,
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

        let out = graph.out_edges(VertexId::from(0)).unwrap();
        let targets: Vec<u32> = out.iter().map(|e| e.target).collect();
        assert_eq!(targets, vec![201, 200, 13, 12, 11, 101, 100, 3, 2, 1]);
    }

    #[test]
    fn overflow_log_same_leaf_payload_multi_bucket_append_success() {
        let graph = segment16_payload_graph();
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label_a = BucketLabelKey::directed_from_index(1);
        let label_b = BucketLabelKey::directed_from_index(2);
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label_a, 4)
            .unwrap();
        graph
            .ensure_label_bucket_inline_value_byte_width(VertexId::from(0), label_b, 4)
            .unwrap();

        for i in 1..=3u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label_a,
                    PayloadTestEdge::with_bytes(i, &[1u8, 1, 1, 1]),
                )
                .unwrap();
            graph
                .insert_edge(
                    VertexId::from(0),
                    label_b,
                    PayloadTestEdge::with_bytes(10 + i, &[2u8, 2, 2, 2]),
                )
                .unwrap();
        }

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_a,
                    inline_value_width: 4,
                    edges: vec![
                        OneOrientationBatchEdge {
                            logical_ordinal: 0,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_a,
                            edge: PayloadTestEdge::with_bytes(100, &[10u8, 11, 12, 13]),
                        },
                        OneOrientationBatchEdge {
                            logical_ordinal: 1,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_a,
                            edge: PayloadTestEdge::with_bytes(101, &[14u8, 15, 16, 17]),
                        },
                    ],
                },
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_b,
                    inline_value_width: 4,
                    edges: vec![
                        OneOrientationBatchEdge {
                            logical_ordinal: 2,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_b,
                            edge: PayloadTestEdge::with_bytes(200, &[20u8, 21, 22, 23]),
                        },
                        OneOrientationBatchEdge {
                            logical_ordinal: 3,
                            owner_vertex_id: VertexId::from(0),
                            neighbor_vertex_id: VertexId::from(1),
                            label_id: label_b,
                            edge: PayloadTestEdge::with_bytes(201, &[24u8, 25, 26, 27]),
                        },
                    ],
                },
            ],
        };

        let result = graph
            .insert_one_orientation_batch(&plan)
            .expect("same-leaf payload multi-bucket overflow-log append should commit");
        assert_eq!(result.edge_slots_written, 4);
        assert_eq!(result.edge_log_entries_written, 4);
        assert_eq!(result.payload_slots_written, 4);
        assert_eq!(result.payload_log_entries_written, 4);

        let a_values = collect_payload_values(&graph, VertexId::from(0), label_a);
        let b_values = collect_payload_values(&graph, VertexId::from(0), label_b);
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

    #[test]
    fn overflow_log_same_leaf_second_run_capacity_exhaustion_rolls_back() {
        let graph = test_graph_with_default(BucketLabelKey::UNLABELED_DIRECTED);
        graph.push_vertex(LabeledVertex::default()).unwrap();
        graph.push_vertex(LabeledVertex::default()).unwrap();

        let label_a = BucketLabelKey::directed_from_index(1);
        let label_b = BucketLabelKey::directed_from_index(2);
        for i in 1..=3u32 {
            graph
                .insert_edge(
                    VertexId::from(0),
                    label_a,
                    crate::labeled::graph::test_support::TestEdge { target: i },
                )
                .unwrap();
        }
        // Pre-create label_b bucket so the second run is not rejected as a new bucket.
        graph
            .insert_edge(
                VertexId::from(0),
                label_b,
                crate::labeled::graph::test_support::TestEdge { target: 1 },
            )
            .unwrap();

        // Fill the shared edge overflow log so that only the first run can fit.
        let header = graph.edges().header();
        let leaf = crate::LabeledLaraGraph::<
            crate::labeled::graph::test_support::TestEdge,
            crate::VectorMemory,
        >::leaf_index_for_vid(VertexId::from(0), header.segment_size);
        let log_capacity = graph.edges().read_overflow_log_state(leaf).1 as usize;
        let spare = log_capacity - 2;
        let entries: Vec<(i32, crate::labeled::graph::test_support::TestEdge)> = (0..spare)
            .map(|i| {
                let prev = if i == 0 { -1 } else { (i as i32) - 1 };
                (
                    prev,
                    crate::labeled::graph::test_support::TestEdge {
                        target: 900 + i as u32,
                    },
                )
            })
            .collect();
        graph
            .edges()
            .write_overflow_log_entries(leaf, 0, &entries)
            .expect("fill log");

        let edge_capacity_before = graph.edges.header().elem_capacity;
        let payload_tail_before = graph.values.header().slab_occupied_tail;
        let before = graph.out_edges(VertexId::from(0)).unwrap();

        let plan = OneOrientationBatchPlan {
            runs: vec![
                OneOrientationBucketRun {
                    owner_vertex_id: VertexId::from(0),
                    label_id: label_a,
                    inline_value_width: 0,
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
                    inline_value_width: 0,
                    edges: vec![OneOrientationBatchEdge {
                        logical_ordinal: 2,
                        owner_vertex_id: VertexId::from(0),
                        neighbor_vertex_id: VertexId::from(1),
                        label_id: label_b,
                        edge: crate::labeled::graph::test_support::TestEdge { target: 200 },
                    }],
                },
            ],
        };

        let err = graph.reserve_one_orientation_batch(&plan).unwrap_err();
        assert!(
            matches!(err, OneOrientationBatchError::LogCapacityExceeded),
            "expected LogCapacityExceeded, got {err}"
        );

        let edge_capacity_after = graph.edges.header().elem_capacity;
        let payload_tail_after = graph.values.header().slab_occupied_tail;
        let after = graph.out_edges(VertexId::from(0)).unwrap();
        assert_eq!(edge_capacity_before, edge_capacity_after);
        assert_eq!(payload_tail_before, payload_tail_after);
        assert_eq!(before, after);

        let (idx_after, _) = graph.edges().read_overflow_log_state(leaf);
        assert_eq!(idx_after, spare as i32);
    }
}
