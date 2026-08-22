use super::helpers::{edge_storage_label, lara_label};
use super::*;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{
    EdgeDirectedness, EdgeLabelId, EdgeSlotIndex, PropertyId, Vertex, VertexRef,
};
use ic_stable_lara::{
    MaintenanceBudget, OutEdgeOrder, VertexId,
    labeled::{
        BucketLabelKey as LaraLabelId, LabeledEdgeInlinePropertyBatchScratch, LabeledOrientation,
    },
    traits::CsrEdge,
};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

fn install_w2_inline_property_profile(_store: &GraphStore, label_id: EdgeLabelId) {
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );
}

#[test]
fn install_edge_label_inline_property_profile_stores_and_returns_profile() {
    use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};

    let store = GraphStore::new();
    let label_id = crate::test_labels::edge_label_id_for_name("InlinePropView");
    let profile = EdgeInlinePropertyProfile {
        byte_width: 4,
        encoding: EdgeInlinePropertyEncoding::F32,
    };
    crate::test_labels::install_test_edge_inline_property_profile(label_id, profile.clone());

    assert_eq!(
        store.edge_label_inline_property_profile(label_id),
        Some(profile)
    );
    assert!(matches!(
        store
            .edge_label_inline_property_profile(label_id)
            .expect("inline property bytes")
            .encoding,
        EdgeInlinePropertyEncoding::F32
    ));
}

#[test]
fn bulk_vertex_rows_return_request_ordered_ids_and_preserve_rows() {
    let store = GraphStore::new();
    let rows = vec![Vertex::default(), Vertex::default(), Vertex::default()];

    let ids = store
        .insert_vertex_rows_bulk(rows)
        .expect("bulk vertex allocation");

    assert_eq!(
        ids,
        vec![VertexId::from(0), VertexId::from(1), VertexId::from(2)]
    );
    assert_eq!(u32::from(store.vertex_count()), 3);
    for id in ids {
        assert!(store.vertex(id).is_some(), "bulk vertex {id:?} is readable");
    }
}

#[test]
fn bulk_vertex_rows_empty_input_is_a_noop() {
    let store = GraphStore::new();

    assert!(
        store
            .insert_vertex_rows_bulk(Vec::new())
            .unwrap()
            .is_empty()
    );
    assert_eq!(u32::from(store.vertex_count()), 0);
}

#[test]
fn bulk_vertex_insert_applies_request_order_without_exposing_allocation_layout() {
    let store = GraphStore::new();

    let ids = crate::facade::mutation_executor::insert_vertices_with(
        &store,
        vec![(Vec::new(), Vec::new()), (Vec::new(), Vec::new())],
        0,
    )
    .expect("bulk vertex insert");

    assert_eq!(ids, vec![VertexId::from(0), VertexId::from(1)]);
    assert!(store.vertex(ids[0]).is_some());
    assert!(store.vertex(ids[1]).is_some());
}

#[test]
fn bulk_vertex_insert_co_writes_labels_and_properties_through_canonical_stores() {
    let store = GraphStore::new();
    let label = crate::test_labels::vertex_label_id_for_name("BulkVertex");
    let property = crate::test_labels::property_id_for_name("name");

    let ids = crate::facade::mutation_executor::insert_vertices_with(
        &store,
        vec![
            (vec![label], vec![(property, Value::Text("a".into()))]),
            (vec![label], vec![(property, Value::Text("b".into()))]),
        ],
        0,
    )
    .expect("bulk vertex labels and properties");

    for (id, expected) in ids.into_iter().zip(["a", "b"]) {
        let vertex = store.vertex(id).expect("bulk vertex row");
        assert!(store.vertex_has_label(id, vertex, label));
        assert_eq!(
            store.vertex_property(id, property),
            Some(Value::Text(expected.into()))
        );
    }
}

#[test]
fn bulk_vertex_insert_preflights_before_allocating_rows() {
    let store = GraphStore::new();
    let before = store.vertex_count();
    let err = crate::facade::mutation_executor::insert_vertices_with(
        &store,
        vec![(Vec::new(), vec![(PropertyId::from_raw(0), Value::Int64(1))])],
        0,
    )
    .expect_err("reserved property id must fail before row allocation");
    assert!(matches!(err, GraphStoreError::PropertyValue(_)));
    assert_eq!(store.vertex_count(), before);
}

#[test]
fn bulk_vertex_insert_rejects_duplicate_properties_before_allocating_rows() {
    let store = GraphStore::new();
    let before = store.vertex_count();
    let property_id = PropertyId::from_raw(7);
    let err = crate::facade::mutation_executor::insert_vertices_with(
        &store,
        vec![(
            Vec::new(),
            vec![
                (property_id, Value::Int64(1)),
                (property_id, Value::Int64(2)),
            ],
        )],
        0,
    )
    .expect_err("duplicate property ids must fail closed");
    assert!(matches!(
        err,
        GraphStoreError::DuplicateBulkVertexProperty { .. }
    ));
    assert_eq!(store.vertex_count(), before);
}

#[test]
fn bulk_vertex_insert_post_write_failure_panics_instead_of_returning_error() {
    let store = GraphStore::new();
    let before = u32::from(store.vertex_count());
    let failure_guard = crate::facade::mutation_executor::test_fail_bulk_vertex_after_row_write();

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        crate::facade::mutation_executor::insert_vertices_with(
            &store,
            vec![(Vec::new(), Vec::new())],
            0,
        )
    }));

    assert!(
        outcome.is_err(),
        "an unexpected error after the first canonical write must panic, not return Err"
    );
    assert_eq!(
        u32::from(store.vertex_count()),
        before + 1,
        "the host-only failpoint must run after the first write; IC message rollback is not emulated by catch_unwind"
    );
    drop(failure_guard);
}

#[test]
fn social_style_leaf_sharing_keeps_alices_third_post_writable() {
    let store = GraphStore::new();
    let initial: Vec<_> = (0..15)
        .map(|_| store.insert_vertex().expect("initial vertex"))
        .collect();
    let follows = crate::test_labels::edge_label_id_for_name("SocialFollow");
    let member_of = crate::test_labels::edge_label_id_for_name("SocialMemberOf");
    let posted = crate::test_labels::edge_label_id_for_name("SocialPosted");
    let insert = |src: usize, dst: usize, label| {
        store
            .insert_directed_edge(initial[src], initial[dst], Some(label))
            .expect("ordinary social edge");
    };

    for (src, dst, label) in [
        (0, 1, follows),
        (0, 2, follows),
        (0, 5, follows),
        (0, 6, follows),
        (0, 7, follows),
        (0, 9, follows),
        (0, 11, member_of),
        (1, 0, follows),
        (1, 7, follows),
        (1, 11, member_of),
        (2, 1, follows),
        (2, 5, follows),
        (2, 11, member_of),
        (3, 4, follows),
        (4, 0, follows),
        (4, 9, follows),
        (4, 11, member_of),
        (5, 0, follows),
        (5, 6, follows),
        (5, 11, member_of),
        (6, 1, follows),
        (6, 5, follows),
        (7, 0, follows),
        (7, 2, follows),
        (7, 11, member_of),
        (8, 3, follows),
        (9, 2, follows),
        (9, 4, follows),
        (10, 7, follows),
    ] {
        insert(src, dst, label);
    }

    let post_sources = [0, 6, 1, 7, 3, 10, 5, 4, 9, 7, 8, 0, 2, 9, 6, 0];
    for (post_index, source) in post_sources.into_iter().enumerate() {
        let post = store.insert_vertex().expect("post vertex");
        store
            .insert_directed_edge(initial[source], post, Some(posted))
            .unwrap_or_else(|err| panic!("ordinary post edge {post_index}: {err:?}"));
    }
}

#[test]
fn insert_rejects_inline_property_bytes_when_label_profile_expects_zero_width() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("ZeroWidthOnly");

    let err = store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[1, 0])
        .expect_err("new label defaults to zero-byte values");
    assert!(
        matches!(
            err,
            GraphStoreError::EdgeInlinePropertyBytesWidthMismatch {
                expected: 0,
                actual: 2,
                ..
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn insert_rejects_inline_property_bytes_when_profile_width_differs() {
    use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};

    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("ProfileWidthMismatch");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: EdgeInlinePropertyEncoding::RawU16,
        },
    );

    let err = store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            target,
            Some(label_id),
            &42i32.to_le_bytes(),
        )
        .expect_err("four-byte payload on W2 label");
    assert!(
        matches!(
            err,
            GraphStoreError::EdgeInlinePropertyBytesWidthMismatch {
                expected: 2,
                actual: 4,
                ..
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn insert_rejects_invalid_edge_inline_property_byte_width() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("InvalidValueWidth");

    let err = store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[1, 2, 3])
        .expect_err("three-byte payload without a matching profile");
    assert!(
        matches!(
            err,
            GraphStoreError::EdgeInlinePropertyBytesWidthMismatch {
                expected: 0,
                actual: 3,
                ..
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn i32_edge_inline_property_profile_round_trip() {
    use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};

    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("I32CostRoad");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        EdgeInlinePropertyProfile {
            byte_width: 4,
            encoding: EdgeInlinePropertyEncoding::RawI32,
        },
    );
    store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            target,
            Some(label_id),
            &100i32.to_le_bytes(),
        )
        .expect("edge");

    let edge = store
        .directed_out_edges(source)
        .expect("out edges")
        .into_iter()
        .find(|edge| edge.neighbor_vid() == target)
        .expect("inserted edge");
    assert_eq!(edge.edge_inline_property_bytes(), &100i32.to_le_bytes());
}

#[test]
fn graph_store_visits_fixed_label_edge_inline_property_batches() {
    use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};

    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let label_id = crate::test_labels::edge_label_id_for_name("BatchValues");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: EdgeInlinePropertyEncoding::RawU16,
        },
    );
    store
        .insert_directed_edge_with_inline_property_bytes(source, first, Some(label_id), &[1, 0])
        .expect("first edge");
    store
        .insert_directed_edge_with_inline_property_bytes(source, second, Some(label_id), &[2, 0])
        .expect("second edge");

    let mut scratch = LabeledEdgeInlinePropertyBatchScratch::default();
    let mut values = Vec::new();
    store
        .visit_out_edge_inline_property_batches_for_label(
            source,
            lara_label(label_id.pack(EdgeDirectedness::Directed)),
            OutEdgeOrder::Ascending,
            &mut scratch,
            |batch| {
                values.extend(
                    batch
                        .inline_property_bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| u16::from_le_bytes([b[0], b[1]])),
                );
            },
        )
        .expect("batch traversal");
    assert_eq!(values, vec![1, 2]);
}

#[test]
fn graph_store_visits_fixed_label_in_edge_inline_property_batches() {
    use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};

    let store = GraphStore::new();
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("BatchInValues");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: EdgeInlinePropertyEncoding::RawU16,
        },
    );
    store
        .insert_directed_edge_with_inline_property_bytes(first, target, Some(label_id), &[1, 0])
        .expect("first edge");
    store
        .insert_directed_edge_with_inline_property_bytes(second, target, Some(label_id), &[2, 0])
        .expect("second edge");

    let mut scratch = LabeledEdgeInlinePropertyBatchScratch::default();
    let mut values = Vec::new();
    store
        .visit_in_edge_inline_property_batches_for_label(
            target,
            lara_label(label_id.pack(EdgeDirectedness::Directed)),
            OutEdgeOrder::Ascending,
            &mut scratch,
            |batch| {
                values.extend(
                    batch
                        .inline_property_bytes
                        .as_chunks::<2>()
                        .0
                        .iter()
                        .map(|b| u16::from_le_bytes([b[0], b[1]])),
                );
            },
        )
        .expect("batch traversal");
    values.sort_unstable();
    assert_eq!(values, vec![1, 2]);
}

#[test]
fn updating_directed_edge_inline_property_updates_forward_and_reverse_rows() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("UpdateDirectedValueBothRows");
    install_w2_inline_property_profile(&store, label_id);

    let forward = store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[1, 0])
        .expect("edge");
    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
    let reverse = store
        .find_first_reverse_handle(target, wire_label, |edge| edge.neighbor_vid() == source)
        .expect("reverse lookup")
        .expect("reverse edge");

    store
        .update_edge_inline_property_at_handle(forward, &[9, 0])
        .expect("forward update");
    assert_eq!(
        store
            .find_outgoing_edge_record(forward)
            .expect("forward lookup")
            .expect("forward edge")
            .edge_inline_property_bytes(),
        &[9, 0]
    );
    assert_eq!(
        store
            .directed_in_edges(target)
            .expect("in edges")
            .into_iter()
            .find(|edge| edge.neighbor_vid() == source)
            .expect("reverse row")
            .edge_inline_property_bytes(),
        &[9, 0]
    );

    store
        .update_edge_inline_property_at_occurrence(
            reverse.occurrence(LabeledOrientation::Reverse),
            &[5, 0],
        )
        .expect("reverse update");
    assert_eq!(
        store
            .find_outgoing_edge_record(forward)
            .expect("forward lookup after reverse update")
            .expect("forward edge after reverse update")
            .edge_inline_property_bytes(),
        &[5, 0]
    );
    assert_eq!(
        store
            .directed_in_edges(target)
            .expect("in edges after reverse update")
            .into_iter()
            .find(|edge| edge.neighbor_vid() == source)
            .expect("reverse row after reverse update")
            .edge_inline_property_bytes(),
        &[5, 0]
    );
}

#[test]
fn updating_undirected_edge_inline_property_updates_both_storage_rows() {
    let store = GraphStore::new();
    let low = store.insert_vertex().expect("low");
    let high = store.insert_vertex().expect("high");
    let label_id = crate::test_labels::edge_label_id_for_name("UpdateUndirectedValueBothRows");
    install_w2_inline_property_profile(&store, label_id);

    let handle = store
        .insert_undirected_edge_with_inline_property_bytes(low, high, Some(label_id), &[1, 0])
        .expect("edge");
    store
        .update_edge_inline_property_at_handle(handle, &[8, 0])
        .expect("update");

    let low_edge = store
        .undirected_edges(low)
        .expect("low edges")
        .into_iter()
        .find(|edge| edge.neighbor_vid() == high)
        .expect("low row");
    let high_edge = store
        .undirected_edges(high)
        .expect("high edges")
        .into_iter()
        .find(|edge| edge.neighbor_vid() == low)
        .expect("high row");
    assert_eq!(low_edge.edge_inline_property_bytes(), &[8, 0]);
    assert_eq!(high_edge.edge_inline_property_bytes(), &[8, 0]);
}

#[test]
fn updating_directed_self_loop_inline_property_updates_both_orientations() {
    let store = GraphStore::new();
    let vertex = store.insert_vertex().expect("vertex");
    let label_id = crate::test_labels::edge_label_id_for_name("UpdateDirectedSelfLoopInline");
    install_w2_inline_property_profile(&store, label_id);
    let handle = store
        .insert_directed_edge_with_inline_property_bytes(vertex, vertex, Some(label_id), &[1, 0])
        .expect("self-loop");

    store
        .update_edge_inline_property_at_handle(handle, &[7, 0])
        .expect("update self-loop");

    let outgoing = store
        .directed_out_edges(vertex)
        .expect("outgoing self-loop");
    let incoming = store.directed_in_edges(vertex).expect("incoming self-loop");
    assert_eq!(outgoing.len(), 1);
    assert_eq!(incoming.len(), 1);
    assert_eq!(outgoing[0].edge_inline_property_bytes(), &[7, 0]);
    assert_eq!(incoming[0].edge_inline_property_bytes(), &[7, 0]);
}

#[test]
fn updating_undirected_self_loop_inline_property_updates_one_row() {
    let store = GraphStore::new();
    let vertex = store.insert_vertex().expect("vertex");
    let label_id = crate::test_labels::edge_label_id_for_name("UpdateUndirectedSelfLoopInline");
    install_w2_inline_property_profile(&store, label_id);
    let handle = store
        .insert_undirected_edge_with_inline_property_bytes(vertex, vertex, Some(label_id), &[1, 0])
        .expect("self-loop");

    store
        .update_edge_inline_property_at_handle(handle, &[8, 0])
        .expect("update self-loop");

    let edges = store
        .undirected_edges(vertex)
        .expect("undirected self-loop");
    assert_eq!(edges.len(), 1);
    assert_eq!(edges[0].edge_inline_property_bytes(), &[8, 0]);
}

#[test]
fn updating_parallel_directed_edge_uses_pair_ordinal() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("UpdateParallelInline");
    install_w2_inline_property_profile(&store, label_id);
    let first = store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[1, 0])
        .expect("first edge");
    let second = store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[2, 0])
        .expect("second edge");

    store
        .update_edge_inline_property_at_handle(second, &[9, 0])
        .expect("update second parallel edge");

    assert_eq!(
        store
            .find_outgoing_edge_record(first)
            .expect("first lookup")
            .expect("first edge")
            .edge_inline_property_bytes(),
        &[1, 0]
    );
    assert_eq!(
        store
            .find_outgoing_edge_record(second)
            .expect("second lookup")
            .expect("second edge")
            .edge_inline_property_bytes(),
        &[9, 0]
    );
    let mut reverse_values = store
        .directed_in_edges(target)
        .expect("reverse edges")
        .into_iter()
        .map(|edge| edge.edge_inline_property_bytes().to_vec())
        .collect::<Vec<_>>();
    reverse_values.sort();
    assert_eq!(reverse_values, vec![vec![1, 0], vec![9, 0]]);
}

#[test]
fn inline_property_update_fails_closed_on_missing_occurrence() {
    let store = GraphStore::new();
    let vertex = store.insert_vertex().expect("vertex");
    let label_id = crate::test_labels::edge_label_id_for_name("UpdateMissingInline");
    install_w2_inline_property_profile(&store, label_id);
    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
    let bogus = EdgeHandle::at_slot(vertex, wire_label, 99);

    let err = store
        .update_edge_inline_property_at_occurrence(
            bogus.occurrence(LabeledOrientation::Forward),
            &[4, 0],
        )
        .expect_err("missing occurrence must fail closed");
    assert!(
        format!("{err:?}").contains("SourceNotFound"),
        "expected SourceNotFound, got {err:?}"
    );
}

#[test]
fn forward_edge_compaction_preserves_inline_propertys() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let third = store.insert_vertex().expect("third");
    let label = crate::test_labels::edge_label_id_for_name("CompactionPreservesValues");
    crate::test_labels::install_test_edge_inline_property_profile(
        label,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );

    let doomed = store
        .insert_directed_edge_with_inline_property_bytes(source, first, Some(label), &[1, 0])
        .expect("first edge");
    store
        .insert_directed_edge_with_inline_property_bytes(source, second, Some(label), &[2, 0])
        .expect("second edge");
    store
        .insert_directed_edge_with_inline_property_bytes(source, third, Some(label), &[33, 0])
        .expect("third edge");

    store.delete_edge_by_handle(doomed).expect("delete first");
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &super::GraphStore::maintenance_policy_for_label,
            )
            .expect("mark compaction");
    });
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("maintenance");

    let third_edge = store
        .directed_out_edges(source)
        .expect("out edges")
        .into_iter()
        .find(|edge| edge.neighbor_vid() == third)
        .expect("third edge after compaction");
    assert_eq!(third_edge.edge_inline_property_bytes(), &[33, 0]);
}

#[test]
fn undirected_canonical_owner_carries_inline_property_bytes() {
    let store = GraphStore::new();
    let low = store.insert_vertex().expect("low");
    let high = store.insert_vertex().expect("high");
    let label_id = crate::test_labels::edge_label_id_for_name("UndirectedValueOwner");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );

    let handle = store
        .insert_undirected_edge_with_inline_property_bytes(low, high, Some(label_id), &[7, 0])
        .expect("undirected edge");
    let owner = store.canonical_edge_handle(handle).owner_vertex_id;
    let edge = store
        .find_outgoing_edge_record(handle)
        .expect("lookup")
        .expect("edge record");
    assert_eq!(edge.edge_inline_property_bytes(), &[7, 0]);
    assert_eq!(owner, high, "higher vid owns undirected forward CSR row");

    let alias = store
        .undirected_edges(low)
        .expect("alias view")
        .into_iter()
        .find(|edge| edge.neighbor_vid() == high)
        .expect("counterpart half");
    assert_eq!(alias.edge_inline_property_bytes(), &[7, 0]);
}

#[test]
fn inline_edge_inline_propertys_round_trip_on_parallel_out_edges() {
    let store = GraphStore::new();
    let s = store.insert_vertex().expect("s");
    let a = store.insert_vertex().expect("a");
    let mid = store.insert_vertex().expect("mid");
    let dst = store.insert_vertex().expect("dst");
    let label_id = crate::test_labels::edge_label_id_for_name("WgtRoad");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );
    store
        .insert_directed_edge_with_inline_property_bytes(
            s,
            mid,
            Some(label_id),
            &10u16.to_le_bytes(),
        )
        .expect("s->mid");
    store
        .insert_directed_edge_with_inline_property_bytes(s, a, Some(label_id), &5u16.to_le_bytes())
        .expect("s->a");
    store
        .insert_directed_edge_with_inline_property_bytes(
            a,
            mid,
            Some(label_id),
            &1u16.to_le_bytes(),
        )
        .expect("a->mid");
    store
        .insert_directed_edge_with_inline_property_bytes(
            mid,
            dst,
            Some(label_id),
            &0u16.to_le_bytes(),
        )
        .expect("mid->dst");
    let _ = dst;
    let mut weights = Vec::new();
    store
        .for_each_directed_out_edges_for_label_unchecked(s, label_id, |edge| {
            weights.push(u16::from_le_bytes(
                edge.edge_inline_property_bytes().try_into().unwrap(),
            ));
        })
        .expect("out edges");
    weights.sort_unstable();
    assert_eq!(weights, vec![5, 10]);
}

#[test]
fn weighted_road_parallel_out_edges_from_a_round_trip() {
    let store = GraphStore::new();
    let a = store.insert_vertex().expect("a");
    let b = store.insert_vertex().expect("b");
    let c = store.insert_vertex().expect("c");
    let label_id = crate::test_labels::edge_label_id_for_name("WgtRoad");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );
    store
        .insert_directed_edge_with_inline_property_bytes(a, b, Some(label_id), &1u16.to_le_bytes())
        .expect("a->b");
    store
        .insert_directed_edge_with_inline_property_bytes(b, c, Some(label_id), &1u16.to_le_bytes())
        .expect("b->c");
    store
        .insert_directed_edge_with_inline_property_bytes(
            a,
            c,
            Some(label_id),
            &100u16.to_le_bytes(),
        )
        .expect("a->c");
    let mut weights = Vec::new();
    store
        .for_each_directed_out_edges_for_label_unchecked(a, label_id, |edge| {
            weights.push(u16::from_le_bytes(
                edge.edge_inline_property_bytes().try_into().unwrap(),
            ));
        })
        .expect("out edges from a");
    weights.sort_unstable();
    assert_eq!(weights, vec![1, 100]);
}

#[test]
fn directed_out_edges_visit_attaches_inline_propertys() {
    let store = GraphStore::new();
    let a = store.insert_vertex().expect("a");
    let label_id = crate::test_labels::edge_label_id_for_name("VisitWgtRoad");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );
    for weight in 1..=8u16 {
        let t = store.insert_vertex().expect("target");
        store
            .insert_directed_edge_with_inline_property_bytes(
                a,
                t,
                Some(label_id),
                &weight.to_le_bytes(),
            )
            .expect("a->t");
    }
    let mut weights = Vec::new();
    store
        .for_each_directed_out_edges(a, OutEdgeOrder::Ascending, |edge| {
            weights.push(u16::from_le_bytes(
                edge.edge_inline_property_bytes().try_into().unwrap(),
            ));
        })
        .expect("out edges");
    weights.sort_unstable();
    assert_eq!(weights, vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn delete_valued_directed_edge_by_handle_removes_reverse_counterpart() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("DeleteValuedDirected");
    install_w2_inline_property_profile(&store, label_id);

    let first = store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[1, 0])
        .expect("first edge");
    store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[2, 0])
        .expect("second edge");

    assert_eq!(store.directed_in_edges(target).expect("in before").len(), 2);
    store.delete_edge_by_handle(first).expect("delete first");

    let in_edges = store.directed_in_edges(target).expect("in after");
    assert_eq!(in_edges.len(), 1);
    assert!(in_edges.iter().all(|edge| edge.neighbor_vid() == source));

    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
    let reverse = store
        .find_first_reverse_handle(target, wire_label, |edge| edge.neighbor_vid() == source)
        .expect("reverse lookup")
        .expect("remaining reverse edge");
    let canonical = store.canonical_reverse_in_edge_handle(reverse);
    let remaining = store
        .find_outgoing_edge_record(canonical)
        .expect("remaining forward lookup")
        .expect("remaining forward edge");
    assert_eq!(remaining.edge_inline_property_bytes(), &[2, 0]);
}

#[test]
fn directed_reverse_counterpart_does_not_require_matching_slot_index() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let other_source = store.insert_vertex().expect("other source");
    let label_id = crate::test_labels::edge_label_id_for_name("DirectedCounterpartSlotSkew");
    install_w2_inline_property_profile(&store, label_id);

    store
        .insert_directed_edge_with_inline_property_bytes(
            other_source,
            target,
            Some(label_id),
            &[7, 0],
        )
        .expect("preexisting edge");
    let canonical = store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[42, 0])
        .expect("skewed edge");

    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
    let reverse = store
        .find_first_reverse_handle(target, wire_label, |edge| edge.neighbor_vid() == source)
        .expect("reverse lookup")
        .expect("reverse edge");
    assert_ne!(
        reverse.slot_index.raw(),
        canonical.slot_index.raw(),
        "test setup should force forward/reverse slot skew"
    );
    assert_eq!(store.canonical_reverse_in_edge_handle(reverse), canonical);

    let edge = store
        .find_outgoing_edge_record(reverse)
        .expect("edge lookup")
        .expect("canonicalized edge");
    assert_eq!(edge.edge_inline_property_bytes(), &[42, 0]);
}

#[test]
fn delete_valued_undirected_edge_by_handle_removes_counterpart_slot() {
    let store = GraphStore::new();
    let low = store.insert_vertex().expect("low");
    let high = store.insert_vertex().expect("high");
    let label_id = crate::test_labels::edge_label_id_for_name("DeleteValuedUndirected");
    install_w2_inline_property_profile(&store, label_id);

    let first = store
        .insert_undirected_edge_with_inline_property_bytes(low, high, Some(label_id), &[1, 0])
        .expect("first edge");
    store
        .insert_undirected_edge_with_inline_property_bytes(low, high, Some(label_id), &[2, 0])
        .expect("second edge");

    store.delete_edge_by_handle(first).expect("delete first");

    let weights_from = |vertex| {
        let mut weights: Vec<u16> = store
            .undirected_edges(vertex)
            .expect("undirected edges")
            .into_iter()
            .map(|edge| u16::from_le_bytes(edge.edge_inline_property_bytes().try_into().unwrap()))
            .collect();
        weights.sort_unstable();
        weights
    };
    assert_eq!(weights_from(low), vec![2]);
    assert_eq!(weights_from(high), vec![2]);

    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Undirected));
    let counterpart = store
        .find_first_forward_handle(low, wire_label, |edge| edge.neighbor_vid() == high)
        .expect("counterpart lookup")
        .expect("remaining counterpart half");
    let canonical = store.canonical_edge_handle(counterpart);
    let remaining = store
        .find_outgoing_edge_record(canonical)
        .expect("remaining canonical lookup")
        .expect("remaining canonical edge");
    assert_eq!(remaining.edge_inline_property_bytes(), &[2, 0]);
}

#[test]
fn unvalued_parallel_directed_inserts_align_reverse_counterpart_slot() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("UnvaluedParallelDirected");

    let first = store
        .insert_directed_edge(source, target, Some(label_id))
        .expect("first edge");
    let second = store
        .insert_directed_edge(source, target, Some(label_id))
        .expect("second edge");
    assert_ne!(first.slot_index.raw(), second.slot_index.raw());
    assert_eq!(store.directed_in_edges(target).expect("in before").len(), 2);

    store.delete_edge_by_handle(first).expect("delete first");

    let in_edges = store.directed_in_edges(target).expect("in after");
    assert_eq!(in_edges.len(), 1);
    let remaining_out = store
        .directed_out_edges(source)
        .expect("out after")
        .into_iter()
        .next()
        .expect("remaining out edge");
    assert_eq!(in_edges[0].edge_slot_index, remaining_out.edge_slot_index);
}

#[test]
fn valued_parallel_insert_returns_handles_for_each_value() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("ParallelValuedHandles");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );

    let first = store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[1, 0])
        .expect("first edge");
    let second = store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[2, 0])
        .expect("second edge");

    assert_ne!(first.slot_index.raw(), second.slot_index.raw());
    let mut values_by_slot = BTreeMap::new();
    store
        .for_each_directed_out_edges_for_label_unchecked(source, label_id, |edge| {
            values_by_slot.insert(
                edge.edge_slot_index.raw(),
                edge.edge_inline_property_bytes().to_vec(),
            );
        })
        .expect("out edges");
    assert_eq!(values_by_slot[&first.slot_index.raw()], vec![1, 0]);
    assert_eq!(values_by_slot[&second.slot_index.raw()], vec![2, 0]);
}

#[test]
fn lookup_edge_record_at_handle_includes_stored_inline_property_bytes() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("LookupEdgeRecordValue");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );
    let handle = store
        .insert_directed_edge_with_inline_property_bytes(source, target, Some(label_id), &[4, 0])
        .expect("edge");
    let edge = store
        .find_outgoing_edge_record(handle)
        .expect("lookup")
        .expect("edge record");
    assert_eq!(edge.edge_inline_property_bytes(), &[4, 0]);
}

/// Regression: vertex `a` is target of `s->a` (reverse-IN alias) and source of `a->mid`
/// (forward-OUT). Shared slot index `0` in both CSR stores must not alias across stores.
#[test]
fn forward_out_lookup_ignores_reverse_in_counterpart_when_slots_collide() {
    let store = GraphStore::new();
    let s = store.insert_vertex().expect("s");
    let a = store.insert_vertex().expect("a");
    let mid = store.insert_vertex().expect("mid");
    let label_id = crate::test_labels::edge_label_id_for_name("ForwardOutReverseInSlotCollision");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );
    store
        .insert_directed_edge_with_inline_property_bytes(s, a, Some(label_id), &[5, 0])
        .expect("s->a");
    let a_to_mid = store
        .insert_directed_edge_with_inline_property_bytes(a, mid, Some(label_id), &[1, 0])
        .expect("a->mid");

    assert_eq!(
        store.canonical_edge_handle(a_to_mid),
        a_to_mid,
        "forward OUT handle must not resolve through reverse-IN alias"
    );
    let edge = store
        .find_outgoing_edge_record(a_to_mid)
        .expect("lookup")
        .expect("edge");
    assert_eq!(edge.edge_inline_property_bytes(), &[1, 0]);
}

#[test]
fn valued_insert_after_delete_returns_handle_for_new_edge() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target_a = store.insert_vertex().expect("target a");
    let target_b = store.insert_vertex().expect("target b");
    let label_id = crate::test_labels::edge_label_id_for_name("TombstoneHandleLookup");
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );

    let doomed = store
        .insert_directed_edge_with_inline_property_bytes(source, target_a, Some(label_id), &[1, 0])
        .expect("doomed edge");
    store
        .insert_directed_edge_with_inline_property_bytes(source, target_b, Some(label_id), &[2, 0])
        .expect("survivor edge");
    store.delete_edge_by_handle(doomed).expect("delete doomed");

    let replacement = store
        .insert_directed_edge_with_inline_property_bytes(source, target_a, Some(label_id), &[9, 0])
        .expect("replacement edge");
    let edge = store
        .directed_out_edges(source)
        .expect("out edges")
        .into_iter()
        .find(|edge| edge.edge_slot_index.raw() == replacement.slot_index.raw())
        .expect("replacement edge record");
    assert_eq!(edge.edge_inline_property_bytes(), &[9, 0]);
    assert_eq!(edge.neighbor_vid(), target_a);
    assert_eq!(
        store.directed_in_edges(target_a).expect("in edges").len(),
        1
    );
}

#[test]
fn insert_edge_handle_lookup_is_scoped_to_expected_label() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let low_label = crate::test_labels::edge_label_id_for_name("LookupLow");
    let high_label = crate::test_labels::edge_label_id_for_name("LookupHigh");

    store
        .insert_directed_edge(source, target, Some(high_label))
        .expect("high edge");
    let low = store
        .insert_directed_edge(source, target, Some(low_label))
        .expect("low edge");

    assert_eq!(
        low.label_id,
        lara_label(edge_storage_label(Some(low_label), false))
    );
}

#[test]
fn edge_label_lookup_uses_edge_label_annotation() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let directed_label = crate::test_labels::edge_label_id_for_name("LookupDirected");
    let undirected_label = crate::test_labels::edge_label_id_for_name("LookupUndirected");
    store
        .insert_directed_edge(source, target, Some(directed_label))
        .expect("directed edge");
    let undirected = store
        .insert_undirected_edge(source, target, Some(undirected_label))
        .expect("undirected edge");

    let edge = store
        .undirected_edges(source)
        .expect("undirected edges")
        .into_iter()
        .find(|edge| edge.edge_slot_index.raw() == undirected.slot_index.raw())
        .expect("inserted undirected edge");

    assert_eq!(
        store
            .find_forward_edge_bucket_label(source, &edge)
            .expect("find label"),
        Some(lara_label(edge_storage_label(Some(undirected_label), true)))
    );
    assert!(store.edge_is_undirected(source, &edge).unwrap());
}

#[test]
fn inserts_vertices_and_edges_through_facade() {
    let store = GraphStore::new();
    let start: u32 = store.vertex_count().into();
    let source = store.insert_vertex().expect("insert source vertex");
    let target = store.insert_vertex().expect("insert target vertex");

    assert_eq!(source, VertexId::from(start));
    assert_eq!(target, VertexId::from(start + 1));

    let directed = store
        .insert_directed_edge(source, target, None)
        .expect("insert directed edge");

    assert_eq!(directed.owner_vertex_id, source);
    assert_eq!(
        EdgeSlotIndex::from_raw(directed.slot_index.raw()),
        EdgeSlotIndex::from_raw(0)
    );

    let out_edges = store.directed_out_edges(source).expect("read out edges");
    assert!(out_edges.iter().any(|edge| {
        edge.target == VertexRef::local(target)
            && edge.edge_slot_index.raw() == directed.slot_index.raw()
            && !store.edge_is_undirected(source, edge).unwrap()
    }));

    let undirected = store
        .insert_undirected_edge(target, source, None)
        .expect("insert undirected edge");

    assert_eq!(undirected.owner_vertex_id, target);
    assert_eq!(
        EdgeSlotIndex::from_raw(undirected.slot_index.raw()),
        EdgeSlotIndex::from_raw(0)
    );

    let target_out_edges = store
        .undirected_edges(target)
        .expect("read target out edges");
    assert!(target_out_edges.iter().any(|edge| {
        edge.target == VertexRef::local(source)
            && edge.edge_slot_index.raw() == undirected.slot_index.raw()
            && store.edge_is_undirected(target, edge).unwrap()
    }));
}

#[test]
fn scan_only_canonical_lookup_uses_lara_counterpart_resolution() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("ScanOnlyCanonicalBoundary");
    let handle = store
        .insert_directed_edge(source, target, Some(label_id))
        .expect("directed edge");
    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
    let reverse = store
        .find_first_reverse_handle(target, wire_label, |edge| edge.neighbor_vid() == source)
        .expect("reverse scan")
        .expect("reverse half");
    let scan_from_forward =
        store.scan_only_canonical_edge_handle(handle, LabeledOrientation::Forward);
    let scan_from_reverse =
        store.scan_only_canonical_edge_handle(reverse, LabeledOrientation::Reverse);
    assert_eq!(scan_from_forward.expect("forward ScanOnly lookup"), handle);
    assert_eq!(scan_from_reverse.expect("reverse ScanOnly lookup"), handle);
}

#[test]
fn timer_maintenance_tick_runs_on_empty_graph() {
    let store = GraphStore::new();
    let report = store.run_timer_maintenance_tick().expect("tick");
    assert_eq!(report.remaining_queue_len(), 0);
}

#[test]
fn detach_delete_homogeneous_directed_edge() {
    let store = GraphStore::new();
    let a = store.insert_vertex().expect("a");
    let b = store.insert_vertex().expect("b");
    store.insert_directed_edge(a, b, None).expect("edge");
    store.detach_delete_vertex(a).expect("detach delete");
    assert!(store.directed_in_edges(b).expect("in").is_empty());
}

#[test]
fn forward_edge_compaction_moves_property_sidecars() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let third = store.insert_vertex().expect("third");
    let label = crate::test_labels::edge_label_id_for_name("CompactionMovesForward");
    let property = store
        .get_or_insert_property_id("move_marker")
        .expect("property");

    let first_edge = store
        .insert_directed_edge(source, first, Some(label))
        .expect("first edge");
    store
        .insert_directed_edge(source, second, Some(label))
        .expect("second edge");
    store
        .insert_directed_edge(source, third, Some(label))
        .expect("third edge");

    let old_third = EdgeHandle::at_slot(
        source,
        lara_label(label.pack(EdgeDirectedness::Directed)),
        2,
    );
    store
        .set_edge_property(
            old_third.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(33),
        )
        .expect("set property");
    store
        .delete_edge_by_handle(first_edge)
        .expect("delete first");
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &super::GraphStore::maintenance_policy_for_label,
            )
            .expect("mark compaction");
    });
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("maintenance");

    let moved = store
        .directed_out_edges(source)
        .expect("out edges")
        .into_iter()
        .find(|edge| edge.neighbor_vid() == third)
        .expect("third edge after compaction");
    assert_eq!(moved.edge_slot_index, EdgeSlotIndex::from_raw(1));
    let new_third = EdgeHandle::at_slot(
        source,
        LaraLabelId::from_raw(moved.label_id),
        moved.edge_slot_index.raw(),
    );
    assert_eq!(
        store
            .edge_property(new_third.occurrence(LabeledOrientation::Forward), property)
            .unwrap(),
        Some(Value::Int64(33))
    );
    assert!(
        store
            .edge_property(old_third.occurrence(LabeledOrientation::Forward), property)
            .is_err(),
        "stale handle after compaction must fail closed"
    );
}

#[test]
fn unordered_compaction_swap_publishes_move_and_moves_sidecars() {
    // A profile'd label resolves Unordered in test builds, so maintenance swap-compacts:
    // [T, second, third] -> the last live edge (third) moves into the first interior
    // tombstone (slot 0), reordering the bucket (ADR 0052 §7) while the inline property
    // bytes and the edge sidecar follow the exact `EdgeSlotMove` (ADR §8/§9).
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let third = store.insert_vertex().expect("third");
    let label = crate::test_labels::edge_label_id_for_name("SwapSidecarFollows");
    install_w2_inline_property_profile(&store, label);
    let property = store
        .get_or_insert_property_id("swap_marker")
        .expect("property");

    let _first_edge = store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            first,
            Some(label),
            &1u16.to_le_bytes(),
        )
        .expect("first edge");
    store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            second,
            Some(label),
            &2u16.to_le_bytes(),
        )
        .expect("second edge");
    let third_edge = store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            third,
            Some(label),
            &33u16.to_le_bytes(),
        )
        .expect("third edge");
    store
        .set_edge_property(
            third_edge.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(33),
        )
        .expect("set property");

    // Fold the deferred-insert overflow log into the slab first so the bucket is
    // edge-slab-only (the swap gate requires it); the facade's deferred insert path
    // leaves fresh edges log-backed until maintenance folds them.
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &super::GraphStore::maintenance_policy_for_label,
            )
            .expect("mark pre-fold");
    });
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("pre-fold drain");

    // The pre-fold span rewrite may relocate the bucket, so re-resolve the first edge's
    // current handle before deleting it.
    let wire_label = lara_label(label.pack(EdgeDirectedness::Directed));
    let first_edge = store
        .find_first_forward_handle(source, wire_label, |edge| edge.neighbor_vid() == first)
        .expect("first lookup")
        .expect("first edge after fold");
    store
        .delete_edge_by_handle(first_edge)
        .expect("delete first");
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &super::GraphStore::maintenance_policy_for_label,
            )
            .expect("mark compaction");
    });
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("maintenance");

    let moved = store
        .directed_out_edges(source)
        .expect("out edges")
        .into_iter()
        .find(|edge| edge.neighbor_vid() == third)
        .expect("third edge after compaction");
    assert_eq!(
        moved.edge_slot_index,
        EdgeSlotIndex::from_raw(0),
        "unordered swap must place the last live edge into the first interior tombstone"
    );
    assert_eq!(
        store
            .directed_out_edges(source)
            .expect("out edges")
            .iter()
            .map(|edge| edge.neighbor_vid())
            .collect::<Vec<_>>(),
        vec![third, second],
        "swap-compaction reorders the bucket (third before second)"
    );
    assert_eq!(
        moved.edge_inline_property_bytes(),
        &33u16.to_le_bytes(),
        "inline property bytes must follow the swapped edge to its new live ordinal"
    );
    let new_third = EdgeHandle::at_slot(source, wire_label, moved.edge_slot_index.raw());
    assert_eq!(
        store
            .edge_property(new_third.occurrence(LabeledOrientation::Forward), property)
            .unwrap(),
        Some(Value::Int64(33)),
        "sidecar must follow the swap move to the new slot"
    );
    let old_third = EdgeHandle::at_slot(source, wire_label, 2);
    assert!(
        store
            .edge_property(old_third.occurrence(LabeledOrientation::Forward), property)
            .is_err(),
        "stale handle after the swap must fail closed"
    );
}

#[test]
fn insertion_policy_maintenance_preserves_left_pack_order() {
    // Same tombstone geometry with an Insertion resolved table: maintenance must keep the
    // order-preserving left-pack (third at slot 1), never swap-reorder.
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let third = store.insert_vertex().expect("third");
    let label = crate::test_labels::edge_label_id_for_name("InsertionMaintenanceOrder");
    install_w2_inline_property_profile(&store, label);

    let _first_edge = store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            first,
            Some(label),
            &1u16.to_le_bytes(),
        )
        .expect("first edge");
    store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            second,
            Some(label),
            &2u16.to_le_bytes(),
        )
        .expect("second edge");
    store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            third,
            Some(label),
            &3u16.to_le_bytes(),
        )
        .expect("third edge");
    // Fold the deferred-insert overflow log so the comparison starts from the same
    // slab-backed bucket as the swap test.
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &super::GraphStore::maintenance_policy_for_label,
            )
            .expect("mark pre-fold");
    });
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("pre-fold drain");
    // The pre-fold span rewrite may relocate the bucket; re-resolve the first edge's handle.
    let first_edge = store
        .find_first_forward_handle(
            source,
            lara_label(label.pack(EdgeDirectedness::Directed)),
            |edge| edge.neighbor_vid() == first,
        )
        .expect("first lookup")
        .expect("first edge after fold");
    store
        .delete_edge_by_handle(first_edge)
        .expect("delete first");
    // The compaction policy is captured at enqueue time (ADR 0052 slice 6), so the
    // ambient Insertion table must be active when `mark_compact_vertex_edge_span` runs.
    // The drain below then runs with NO ambient table (the timer scenario) and still
    // left-packs because the work item carries the captured policy.
    use gleaph_graph_kernel::plan_exec::{
        EdgeOrderingPolicy, ResolvedEdgeLabel, ResolvedLabelTable,
    };
    let resolved = ResolvedLabelTable {
        edge: vec![
            ResolvedEdgeLabel::new(
                "InsertionMaintenanceOrder".to_string(),
                label,
                gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
                    byte_width: 2,
                    encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
                },
            )
            .with_ordering(EdgeOrderingPolicy::Insertion),
        ],
        ..Default::default()
    };
    crate::edge_inline_property_schema::set_execution_resolved_labels(Some(resolved));
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &super::GraphStore::maintenance_policy_for_label,
            )
            .expect("mark compaction");
    });
    crate::edge_inline_property_schema::set_execution_resolved_labels(None);
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("maintenance");

    let edges = store.directed_out_edges(source).expect("out edges");
    let third_edge = edges
        .iter()
        .find(|edge| edge.neighbor_vid() == third)
        .expect("third edge");
    assert_eq!(
        third_edge.edge_slot_index,
        EdgeSlotIndex::from_raw(1),
        "Insertion maintenance must keep the order-preserving left-pack"
    );
    assert_eq!(
        edges
            .iter()
            .map(|edge| edge.neighbor_vid())
            .collect::<Vec<_>>(),
        vec![second, third],
        "Insertion maintenance preserves bucket-local live order"
    );
}

#[test]
fn unordered_captured_at_enqueue_drains_without_ambient_table() {
    // The timer scenario at the facade: the label resolves to Unordered when the span
    // compaction is enqueued (ambient resolved table active), and the drain then runs
    // with NO ambient table yet still swap-compacts because the work item carries the
    // captured policy.
    use gleaph_graph_kernel::plan_exec::{
        EdgeOrderingPolicy, ResolvedEdgeLabel, ResolvedLabelTable,
    };
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let third = store.insert_vertex().expect("third");
    let label = crate::test_labels::edge_label_id_for_name("TimerScenarioSwap");
    install_w2_inline_property_profile(&store, label);
    for (dst, bytes) in [(first, 1u16), (second, 2), (third, 3)] {
        store
            .insert_directed_edge_with_inline_property_bytes(
                source,
                dst,
                Some(label),
                &bytes.to_le_bytes(),
            )
            .expect("insert");
    }
    // Fold the deferred-insert overflow log into the slab so the delete leaves an
    // in-slab tombstone (the swap gate requires edge-slab-only buckets).
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &GraphStore::maintenance_policy_for_label,
            )
            .expect("mark pre-fold");
    });
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("pre-fold drain");
    let first_edge = store
        .find_first_forward_handle(
            source,
            lara_label(label.pack(EdgeDirectedness::Directed)),
            |edge| edge.neighbor_vid() == first,
        )
        .expect("first lookup")
        .expect("first edge");
    store
        .delete_edge_by_handle(first_edge)
        .expect("delete first");
    let resolved = ResolvedLabelTable {
        edge: vec![
            ResolvedEdgeLabel::new(
                "TimerScenarioSwap".to_string(),
                label,
                gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
                    byte_width: 2,
                    encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
                },
            )
            .with_ordering(EdgeOrderingPolicy::Unordered),
        ],
        ..Default::default()
    };
    crate::edge_inline_property_schema::set_execution_resolved_labels(Some(resolved));
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &GraphStore::maintenance_policy_for_label,
            )
            .expect("enqueue under Unordered table");
    });
    crate::edge_inline_property_schema::set_execution_resolved_labels(None);
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("timer drain");
    let edges = store.directed_out_edges(source).expect("out edges");
    assert_eq!(
        edges
            .iter()
            .map(|edge| edge.neighbor_vid())
            .collect::<Vec<_>>(),
        vec![third, second],
        "a drain with no ambient table must still swap-compact the captured Unordered label"
    );
    assert_eq!(
        edges
            .iter()
            .find(|edge| edge.neighbor_vid() == third)
            .expect("third")
            .edge_slot_index,
        EdgeSlotIndex::from_raw(0),
        "the swapped edge lands in the first interior tombstone"
    );
}

#[test]
fn maintenance_without_resolved_table_compacts_order_preserving() {
    // No resolved table and a no-profile label: the maintenance resolver maps the absent
    // policy to order-preserving (never swap), so the left-pack layout survives.
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let third = store.insert_vertex().expect("third");
    let label = crate::test_labels::edge_label_id_for_name("SafeFallbackCompaction");
    let property = store
        .get_or_insert_property_id("safe_fallback_marker")
        .expect("property");

    let first_edge = store
        .insert_directed_edge(source, first, Some(label))
        .expect("first edge");
    store
        .insert_directed_edge(source, second, Some(label))
        .expect("second edge");
    let third_edge = store
        .insert_directed_edge(source, third, Some(label))
        .expect("third edge");
    store
        .set_edge_property(
            third_edge.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(33),
        )
        .expect("set property");
    store
        .delete_edge_by_handle(first_edge)
        .expect("delete first");
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &super::GraphStore::maintenance_policy_for_label,
            )
            .expect("mark compaction");
    });
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("maintenance");

    let third_edge = store
        .directed_out_edges(source)
        .expect("out edges")
        .into_iter()
        .find(|edge| edge.neighbor_vid() == third)
        .expect("third edge after compaction");
    assert_eq!(
        third_edge.edge_slot_index,
        EdgeSlotIndex::from_raw(1),
        "absent policy must compact order-preservingly (left-pack), not swap"
    );
    let new_third = EdgeHandle::at_slot(
        source,
        lara_label(label.pack(EdgeDirectedness::Directed)),
        third_edge.edge_slot_index.raw(),
    );
    assert_eq!(
        store
            .edge_property(new_third.occurrence(LabeledOrientation::Forward), property)
            .unwrap(),
        Some(Value::Int64(33)),
        "sidecar follows the order-preserving move"
    );
}

#[test]
fn swap_compaction_rekeys_inline_scalar_index_to_new_slot() {
    // The swap move flows through `GraphSidecarMoveObserver` ->
    // `relocate_edge_properties_for_move`, rekeying the property-index entry keyed on the
    // edge's inline value from the old slot to the new slot (ADR §8).
    use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::index::{IndexedEdgeMembership, IndexedPropertyCatalog};

    let store = GraphStore::new();
    store
        .set_federation_routing(Some(crate::facade::FederationRouting {
            router_canister: candid::Principal::management_canister(),
            index_canister: candid::Principal::management_canister(),
            shard_id: ShardId::new(0),
            vector_canister: None,
        }))
        .expect("configure index routing");
    crate::index::edge_pending::clear_pending();

    let label = EdgeLabelId::from_raw(11);
    let property = PropertyId::from_raw(911);
    crate::test_labels::install_test_edge_inline_property_profile(
        label,
        EdgeInlinePropertyProfile {
            byte_width: 4,
            encoding: EdgeInlinePropertyEncoding::F32,
        },
    );
    crate::test_labels::install_test_edge_inline_property(label, property);
    let _catalog = crate::index::catalog_context::enter(IndexedPropertyCatalog {
        edge_indexes: vec![IndexedEdgeMembership {
            physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId::new(106)
                .expect("test physical id"),
            catalog_epoch: 1,
            phase: gleaph_graph_kernel::index::IndexMaintenancePhase::Active,
            label_id: label.raw(),
            property_id: property.raw(),
            direction: gleaph_graph_kernel::index::EdgeIndexDirection::Outgoing,
            field_path: String::new(),
        }],
        ..Default::default()
    });

    let source = store.insert_vertex().expect("source");
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let third = store.insert_vertex().expect("third");
    let _first_edge = store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            first,
            Some(label),
            &1.5f32.to_le_bytes(),
        )
        .expect("first edge");
    store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            second,
            Some(label),
            &2.5f32.to_le_bytes(),
        )
        .expect("second edge");
    store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            third,
            Some(label),
            &3.5f32.to_le_bytes(),
        )
        .expect("third edge");
    crate::index::edge_pending::take_pending(); // swallow insert postings

    // Fold the deferred-insert overflow log into the slab so the swap gate applies.
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &super::GraphStore::maintenance_policy_for_label,
            )
            .expect("mark pre-fold");
    });
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("pre-fold drain");
    crate::index::edge_pending::take_pending(); // swallow fold move postings

    // The pre-fold span rewrite may relocate the bucket; re-resolve the first edge's handle.
    let first_edge = store
        .find_first_forward_handle(
            source,
            lara_label(label.pack(EdgeDirectedness::Directed)),
            |edge| edge.neighbor_vid() == first,
        )
        .expect("first lookup")
        .expect("first edge after fold");
    store
        .delete_edge_by_handle(first_edge)
        .expect("delete first");
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(
                LabeledOrientation::Forward,
                source,
                0,
                &super::GraphStore::maintenance_policy_for_label,
            )
            .expect("mark compaction");
    });
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("maintenance");

    // The drain ran the swap (3.5 edge moved slot 2 -> 0); the pending postings hold the
    // delete's removal plus the rekey pair: Remove(old slot 2) + Insert(new slot 0).
    let pending = crate::index::edge_pending::take_pending();
    let removed_at_2 = pending.iter().any(|op| {
        matches!(op, crate::index::edge_pending::PendingEdgePostingOp::Remove {
            property_id, slot_index, label_id, ..
        } if *property_id == property.raw()
            && *slot_index == 2
            && *label_id == label.pack(EdgeDirectedness::Directed).raw())
    });
    let inserted_at_0 = pending.iter().any(|op| {
        matches!(op, crate::index::edge_pending::PendingEdgePostingOp::Insert {
            property_id, slot_index, label_id, ..
        } if *property_id == property.raw()
            && *slot_index == 0
            && *label_id == label.pack(EdgeDirectedness::Directed).raw())
    });
    assert!(
        removed_at_2,
        "rekey must remove the inline-scalar posting at the old slot: {pending:?}"
    );
    assert!(
        inserted_at_0,
        "rekey must insert the inline-scalar posting at the new slot: {pending:?}"
    );
    store.set_federation_routing(None).expect("clear routing");
}

#[test]
fn reverse_edge_compaction_preserves_canonical_sidecars() {
    let store = GraphStore::new();
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let third = store.insert_vertex().expect("third");
    let target = store.insert_vertex().expect("target");
    let label = crate::test_labels::edge_label_id_for_name("CompactionMovesReverseCounterpart");
    let other_label =
        crate::test_labels::edge_label_id_for_name("CompactionMovesReverseCounterpartOther");
    let property = store
        .get_or_insert_property_id("reverse_move_marker")
        .expect("property");

    let first_edge = store
        .insert_directed_edge(first, target, Some(label))
        .expect("first edge");
    store
        .insert_directed_edge(second, target, Some(label))
        .expect("second edge");
    let third_edge = store
        .insert_directed_edge(third, target, Some(label))
        .expect("third edge");
    store
        .insert_directed_edge(second, target, Some(other_label))
        .expect("other label edge");
    store
        .set_edge_property(
            third_edge.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(44),
        )
        .expect("set property");
    let wire_label = lara_label(label.pack(EdgeDirectedness::Directed));

    store
        .delete_edge_by_handle(first_edge)
        .expect("delete first");
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_dense_labeled_vertex_maintenance(
                LabeledOrientation::Reverse,
                target,
                &super::GraphStore::maintenance_policy_for_label,
            )
            .expect("mark reverse compaction");
    });
    store
        .run_maintenance_best_effort(MaintenanceBudget {
            max_instructions: 0,
            reserve_instructions: 0,
            checkpoint_every: 1,
            max_work_items: None,
            max_segments: None,
            max_delete_edge_steps: None,
        })
        .expect("maintenance");

    assert_eq!(
        store
            .edge_property(third_edge.occurrence(LabeledOrientation::Forward), property)
            .unwrap(),
        Some(Value::Int64(44)),
        "canonical forward handle keeps properties across reverse compaction"
    );

    let reverse_third = store
        .find_first_reverse_handle(target, wire_label, |edge| edge.neighbor_vid() == third)
        .expect("reverse lookup after compaction")
        .expect("third reverse edge after compaction");
    assert_eq!(
        store.canonical_reverse_in_edge_handle(reverse_third),
        third_edge,
        "reverse CSR slot should still resolve to the canonical forward handle"
    );
    assert_eq!(
        store
            .edge_property(
                reverse_third.occurrence(LabeledOrientation::Reverse),
                property
            )
            .unwrap(),
        Some(Value::Int64(44))
    );
}

#[test]
fn post_insert_maintenance_reclaims_parallel_overflow_bucket_for_inline_properties() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let label = crate::test_labels::edge_label_id_for_name("PostInsertOverflowReclaim");
    install_w2_inline_property_profile(&store, label);

    for i in 0..48u16 {
        let target = store.insert_vertex().expect("target");
        store
            .insert_directed_edge_with_inline_property_bytes(
                source,
                target,
                Some(label),
                &i.to_le_bytes(),
            )
            .unwrap_or_else(|e| panic!("edge i={i}: {e:?}"));
    }

    let mut scratch = LabeledEdgeInlinePropertyBatchScratch::default();
    let mut edge_count = 0;
    store
        .visit_directed_out_edge_inline_property_batches_for_label(
            source,
            label,
            OutEdgeOrder::Descending,
            &mut scratch,
            |batch| edge_count += batch.edges.len(),
        )
        .expect("inline property batches");

    assert_eq!(edge_count, 48);
    assert_eq!(
        store.directed_out_edges(source).expect("out").len(),
        48,
        "topology must stay intact after reclaim"
    );
}

#[test]
fn edge_property_counterpart_scan_reads_forward_directed_edge() -> Result<(), GraphStoreError> {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label = crate::test_labels::edge_label_id_for_name("CounterpartForwardDirected");
    let property = store.get_or_insert_property_id("weight").expect("property");
    let handle = store
        .insert_directed_edge(source, target, Some(label))
        .expect("edge");

    store
        .set_edge_property(
            handle.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(7),
        )
        .expect("set");

    assert_eq!(
        store.edge_property(handle.occurrence(LabeledOrientation::Forward), property)?,
        Some(Value::Int64(7))
    );
    Ok(())
}

#[test]
fn edge_property_counterpart_scan_reads_reverse_directed_edge() -> Result<(), GraphStoreError> {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label = crate::test_labels::edge_label_id_for_name("CounterpartReverseDirected");
    let wire_label = lara_label(label.pack(EdgeDirectedness::Directed));
    let property = store.get_or_insert_property_id("weight").expect("property");
    let forward = store
        .insert_directed_edge(source, target, Some(label))
        .expect("edge");
    store
        .set_edge_property(
            forward.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(9),
        )
        .expect("set");

    let reverse = store
        .find_first_reverse_handle(target, wire_label, |edge| edge.neighbor_vid() == source)
        .expect("reverse scan")
        .expect("reverse half");

    assert_eq!(
        store.edge_property(reverse.occurrence(LabeledOrientation::Reverse), property)?,
        Some(Value::Int64(9))
    );
    Ok(())
}

#[test]
fn edge_property_counterpart_scan_reads_directed_self_loop_from_both_orientations()
-> Result<(), GraphStoreError> {
    let store = GraphStore::new();
    let vertex = store.insert_vertex().expect("vertex");
    let label = crate::test_labels::edge_label_id_for_name("CounterpartDirectedSelfLoop");
    let wire_label = lara_label(label.pack(EdgeDirectedness::Directed));
    let property = store.get_or_insert_property_id("tag").expect("property");
    let forward = store
        .insert_directed_edge(vertex, vertex, Some(label))
        .expect("self loop");
    store
        .set_edge_property(
            forward.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(1),
        )
        .expect("set");

    let reverse = store
        .find_first_reverse_handle(vertex, wire_label, |edge| edge.neighbor_vid() == vertex)
        .expect("reverse scan")
        .expect("reverse half");

    assert_eq!(
        store.edge_property(forward.occurrence(LabeledOrientation::Forward), property)?,
        Some(Value::Int64(1))
    );
    assert_eq!(
        store.edge_property(reverse.occurrence(LabeledOrientation::Reverse), property)?,
        Some(Value::Int64(1))
    );
    Ok(())
}

#[test]
fn edge_property_counterpart_scan_reads_undirected_max_owner_edge() -> Result<(), GraphStoreError> {
    let store = GraphStore::new();
    let small = store.insert_vertex().expect("small");
    let large = store.insert_vertex().expect("large");
    let label = crate::test_labels::edge_label_id_for_name("CounterpartUndirectedMaxOwner");
    let wire_label = lara_label(label.pack(EdgeDirectedness::Undirected));
    let property = store.get_or_insert_property_id("shared").expect("property");
    let owner = std::cmp::max(small, large);
    let _handle = store
        .insert_undirected_edge(small, large, Some(label))
        .expect("edge");

    store
        .set_edge_property(
            EdgeHandle::at_slot(owner, wire_label, 0).occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(42),
        )
        .expect("set at max-owner");

    assert_eq!(
        store.edge_property(
            EdgeHandle::at_slot(owner, wire_label, 0).occurrence(LabeledOrientation::Forward),
            property,
        )?,
        Some(Value::Int64(42))
    );
    Ok(())
}

#[test]
fn edge_property_counterpart_scan_reads_undirected_self_loop() -> Result<(), GraphStoreError> {
    let store = GraphStore::new();
    let vertex = store.insert_vertex().expect("vertex");
    let label = crate::test_labels::edge_label_id_for_name("CounterpartUndirectedSelfLoop");
    let wire_label = lara_label(label.pack(EdgeDirectedness::Undirected));
    let property = store.get_or_insert_property_id("loop").expect("property");
    let handle = store
        .insert_undirected_edge(vertex, vertex, Some(label))
        .expect("self loop");

    store
        .set_edge_property(
            handle.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(3),
        )
        .expect("set");

    assert_eq!(
        store.edge_property(
            EdgeHandle::at_slot(vertex, wire_label, 0).occurrence(LabeledOrientation::Forward),
            property,
        )?,
        Some(Value::Int64(3))
    );
    Ok(())
}

#[test]
fn edge_property_counterpart_scan_distinguishes_parallel_edges() -> Result<(), GraphStoreError> {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label = crate::test_labels::edge_label_id_for_name("CounterpartParallelEdges");
    let property = store.get_or_insert_property_id("order").expect("property");
    let first = store
        .insert_directed_edge(source, target, Some(label))
        .expect("first");
    let second = store
        .insert_directed_edge(source, target, Some(label))
        .expect("second");

    store
        .set_edge_property(
            first.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(1),
        )
        .expect("set first");
    store
        .set_edge_property(
            second.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(2),
        )
        .expect("set second");

    assert_eq!(
        store.edge_property(first.occurrence(LabeledOrientation::Forward), property)?,
        Some(Value::Int64(1))
    );
    assert_eq!(
        store.edge_property(second.occurrence(LabeledOrientation::Forward), property)?,
        Some(Value::Int64(2))
    );
    Ok(())
}

#[test]
fn edge_property_counterpart_scan_fails_closed_on_missing_source() -> Result<(), GraphStoreError> {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label = crate::test_labels::edge_label_id_for_name("CounterpartMissingSource");
    let property = store.get_or_insert_property_id("weight").expect("property");
    let forward = store
        .insert_directed_edge(source, target, Some(label))
        .expect("edge");
    store
        .set_edge_property(
            forward.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(5),
        )
        .expect("set");

    store.with_graph_mut(|graph| {
        graph
            .remove_forward_edge_at_slot(
                forward.owner_vertex_id,
                forward.label_id,
                forward.slot_index.raw(),
            )
            .expect("remove edge")
            .expect("edge existed");
    });

    let err = store
        .edge_property(forward.occurrence(LabeledOrientation::Forward), property)
        .expect_err("missing source must fail closed");
    assert!(
        format!("{err:?}").contains("SourceNotFound"),
        "expected SourceNotFound, got {err:?}"
    );
    Ok(())
}
#[test]
fn edge_property_write_fails_before_mutation_on_missing_source() -> Result<(), GraphStoreError> {
    let store = GraphStore::new();
    let vertex = store.insert_vertex().expect("vertex");
    let label = crate::test_labels::edge_label_id_for_name("CounterpartMissingWrite");
    let wire_label = lara_label(label.pack(EdgeDirectedness::Directed));
    let property = store.get_or_insert_property_id("weight").expect("property");
    let bogus = EdgeHandle::at_slot(vertex, wire_label, 99);

    let err = store
        .set_edge_property(
            bogus.occurrence(LabeledOrientation::Forward),
            property,
            Value::Int64(1),
        )
        .expect_err("missing source must fail closed");
    assert!(
        format!("{err:?}").contains("SourceNotFound"),
        "expected SourceNotFound, got {err:?}"
    );

    // Index postings and property store must remain unchanged.
    let repeated = store.edge_property(bogus.occurrence(LabeledOrientation::Forward), property);
    assert!(repeated.is_err(), "repeated lookup must remain failed");
    assert_eq!(format!("{:?}", repeated.unwrap_err()), format!("{:?}", err));
    Ok(())
}

#[test]
fn unordered_default_reuses_tombstoned_slot() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    // No execution-resolved labels: undeclared labels resolve to the Unordered
    // default (ADR 0052 §1), so a delete-then-insert reuses the tombstone slot.
    let first = store
        .insert_directed_edge(source, target, None)
        .expect("first");
    let second = store
        .insert_directed_edge(source, target, None)
        .expect("second");
    let third = store
        .insert_directed_edge(source, target, None)
        .expect("third");
    assert_eq!(first.slot_index.raw(), 0);
    assert_eq!(second.slot_index.raw(), 1);
    assert_eq!(third.slot_index.raw(), 2);
    // Fold both orientations onto the slab (bulk-ingest finalize) so a delete
    // leaves an in-slab tombstone that the Unordered default can reuse.
    store
        .finalize_bulk_ingest(&BulkIngestFinalizeSpec {
            forward_vertices: vec![source],
            reverse_vertices: vec![target],
        })
        .expect("fold log to slab");
    store.delete_edge_by_handle(second).expect("delete middle");
    let reused = store
        .insert_directed_edge(source, target, None)
        .expect("reused");
    assert_eq!(
        reused.slot_index.raw(),
        1,
        "Unordered default must reuse the in-slab tombstone before appending"
    );
    assert_eq!(store.directed_out_edges(source).expect("out").len(), 3);
}

#[test]
fn resolved_insertion_policy_preserves_append_order() {
    use gleaph_graph_kernel::plan_exec::{
        EdgeOrderingPolicy, ResolvedEdgeLabel, ResolvedLabelTable,
    };
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("OrderedInsertionBoundary");
    let resolved = ResolvedLabelTable {
        vertex: vec![],
        edge: vec![
            ResolvedEdgeLabel::new(
                "OrderedInsertionBoundary".to_string(),
                label_id,
                gleaph_graph_kernel::entry::EdgeInlinePropertyProfile::no_inline_property(),
            )
            .with_ordering(EdgeOrderingPolicy::Insertion),
        ],
    };
    crate::edge_inline_property_schema::set_execution_resolved_labels(Some(resolved));
    let first = store
        .insert_directed_edge(source, target, Some(label_id))
        .expect("first");
    let second = store
        .insert_directed_edge(source, target, Some(label_id))
        .expect("second");
    let third = store
        .insert_directed_edge(source, target, Some(label_id))
        .expect("third");
    assert_eq!(first.slot_index.raw(), 0);
    assert_eq!(second.slot_index.raw(), 1);
    assert_eq!(third.slot_index.raw(), 2);
    store
        .finalize_bulk_ingest(&BulkIngestFinalizeSpec {
            forward_vertices: vec![source],
            reverse_vertices: vec![target],
        })
        .expect("fold log to slab");
    store.delete_edge_by_handle(second).expect("delete middle");
    // Insertion placement appends past the interior tombstone (ADR 0052 §6).
    let appended = store
        .insert_directed_edge(source, target, Some(label_id))
        .expect("appended");
    assert_eq!(appended.slot_index.raw(), 3);
    crate::edge_inline_property_schema::set_execution_resolved_labels(None);
}

// --- GAP-2026-07-29-004: inline index old-key/new-key posting transitions ---

fn inline_posting_test_catalog(
    physical_raw: u64,
    label_raw: u16,
    property_raw: u32,
    field_path: &str,
) -> crate::index::catalog_context::CatalogGuard {
    use gleaph_graph_kernel::index::{
        EdgeIndexDirection, IndexedEdgeMembership, IndexedPropertyCatalog,
    };
    crate::index::catalog_context::enter(IndexedPropertyCatalog {
        edge_indexes: vec![IndexedEdgeMembership {
            physical_index_id: gleaph_graph_kernel::index::PhysicalIndexId::new(physical_raw)
                .expect("test physical id"),
            catalog_epoch: 1,
            phase: gleaph_graph_kernel::index::IndexMaintenancePhase::Active,
            label_id: label_raw,
            property_id: property_raw,
            direction: EdgeIndexDirection::Outgoing,
            field_path: field_path.to_owned(),
        }],
        ..Default::default()
    })
}

fn posting_ops_for(
    pending: &[crate::index::edge_pending::PendingEdgePostingOp],
    property_raw: u32,
) -> Vec<(&'static str, Vec<u8>)> {
    pending
        .iter()
        .filter(|op| match op {
            crate::index::edge_pending::PendingEdgePostingOp::Insert { property_id, .. }
            | crate::index::edge_pending::PendingEdgePostingOp::Remove { property_id, .. } => {
                *property_id == property_raw
            }
        })
        .map(|op| match op {
            crate::index::edge_pending::PendingEdgePostingOp::Insert { payload_bytes, .. } => {
                ("insert", payload_bytes.clone())
            }
            crate::index::edge_pending::PendingEdgePostingOp::Remove { payload_bytes, .. } => {
                ("remove", payload_bytes.clone())
            }
        })
        .collect()
}

fn inline_scalar_posting_fixture(
    physical_raw: u64,
    label_raw: u16,
    property_raw: u32,
    field_path: &str,
) -> (
    GraphStore,
    EdgeLabelId,
    PropertyId,
    EdgeHandle,
    crate::index::catalog_context::CatalogGuard,
) {
    use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};
    let store = GraphStore::new();
    store
        .set_federation_routing(Some(crate::facade::FederationRouting {
            router_canister: candid::Principal::management_canister(),
            index_canister: candid::Principal::management_canister(),
            shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
            vector_canister: None,
        }))
        .expect("configure index routing");
    crate::index::edge_pending::clear_pending();
    let label = EdgeLabelId::from_raw(label_raw);
    let property = PropertyId::from_raw(property_raw);
    crate::test_labels::install_test_edge_inline_property_profile(
        label,
        EdgeInlinePropertyProfile {
            byte_width: 4,
            encoding: EdgeInlinePropertyEncoding::F32,
        },
    );
    crate::test_labels::install_test_edge_inline_property(label, property);
    let catalog =
        inline_posting_test_catalog(physical_raw, label.raw(), property.raw(), field_path);
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let handle = store
        .insert_directed_edge_with_inline_property_bytes(
            source,
            target,
            Some(label),
            &1.5f32.to_le_bytes(),
        )
        .expect("edge");
    let swallowed = crate::index::edge_pending::take_pending();
    assert!(
        !posting_ops_for(&swallowed, property.raw()).is_empty(),
        "fixture insert must post the initial scalar value"
    );
    (store, label, property, handle, catalog)
}

#[test]
fn updating_indexed_inline_scalar_swaps_posting_old_for_new() {
    let (store, _label, property, handle, _catalog) =
        inline_scalar_posting_fixture(107, 12, 912, "");

    store
        .update_edge_inline_property_at_handle(handle, &2.5f32.to_le_bytes())
        .expect("scalar update");

    let ops = posting_ops_for(&crate::index::edge_pending::take_pending(), property.raw());
    let key = |v: f32| crate::property::sortable_index_key(&Value::Float32(v)).expect("key");
    assert_eq!(
        ops,
        vec![("remove", key(1.5)), ("insert", key(2.5))],
        "replacement must remove exactly the old key and insert exactly the new key"
    );
    store.set_federation_routing(None).expect("clear routing");
}

#[test]
fn updating_indexed_inline_scalar_to_same_value_emits_no_posting() {
    let (store, _label, property, handle, _catalog) =
        inline_scalar_posting_fixture(107, 12, 912, "");

    store
        .update_edge_inline_property_at_handle(handle, &1.5f32.to_le_bytes())
        .expect("no-op update");

    assert!(
        posting_ops_for(&crate::index::edge_pending::take_pending(), property.raw()).is_empty(),
        "an equal-value update must not churn index postings"
    );
    store.set_federation_routing(None).expect("clear routing");
}

#[test]
fn updating_indexed_inline_struct_leaf_replaces_only_that_leaf_posting() {
    use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};
    let (store, label, property, _scalar_edge, _scalar_catalog) =
        inline_scalar_posting_fixture(110, 15, 915, "");
    // The struct projection widens the label profile: two F32 leaves at offsets 0 and 4.
    use gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding as Encoding;
    crate::test_labels::install_test_edge_inline_property_profile(
        label,
        EdgeInlinePropertyProfile {
            byte_width: 8,
            encoding: Encoding::RawBytes,
        },
    );
    crate::test_labels::install_test_edge_inline_struct_property(
        label,
        property,
        vec![
            (
                "score".to_string(),
                0,
                EdgeInlinePropertyProfile {
                    byte_width: 4,
                    encoding: EdgeInlinePropertyEncoding::F32,
                },
            ),
            (
                "confidence".to_string(),
                4,
                EdgeInlinePropertyProfile {
                    byte_width: 4,
                    encoding: EdgeInlinePropertyEncoding::F32,
                },
            ),
        ],
    );
    drop(_scalar_catalog);
    let _catalog = inline_posting_test_catalog(110, 15, property.raw(), "score");
    // Insert with struct bytes: score 1.5, confidence 0.75.
    let source = store.insert_vertex().expect("source");
    let neighbor = store.insert_vertex().expect("neighbor");
    let struct_handle = store
        .insert_directed_edge_with_inline_property_bytes(source, neighbor, Some(label), &{
            let mut bytes = Vec::with_capacity(8);
            bytes.extend_from_slice(&1.5f32.to_le_bytes());
            bytes.extend_from_slice(&0.75f32.to_le_bytes());
            bytes
        })
        .expect("struct edge");
    assert!(
        !posting_ops_for(&crate::index::edge_pending::take_pending(), property.raw()).is_empty(),
        "fixture insert must post initial leaf values"
    );

    let mut updated = Vec::with_capacity(8);
    updated.extend_from_slice(&2.5f32.to_le_bytes());
    updated.extend_from_slice(&0.75f32.to_le_bytes());
    store
        .update_edge_inline_property_at_handle(struct_handle, &updated)
        .expect("struct leaf update");

    let ops = posting_ops_for(&crate::index::edge_pending::take_pending(), property.raw());
    let key = |v: f32| crate::property::sortable_index_key(&Value::Float32(v)).expect("key");
    assert_eq!(
        ops,
        vec![("remove", key(1.5)), ("insert", key(2.5))],
        "only the changed leaf may transition; untouched leaves keep their postings"
    );
    store.set_federation_routing(None).expect("clear routing");
}

#[test]
fn updating_indexed_inline_bytes_with_wrong_width_rejects_before_write() {
    let (store, _label, property, handle, _catalog) =
        inline_scalar_posting_fixture(107, 12, 912, "");

    let err = store
        .update_edge_inline_property_at_handle(handle, &[1, 2, 3])
        .expect_err("width mismatch must reject");

    assert!(matches!(
        err,
        GraphStoreError::EdgeInlinePropertyBytesWidthMismatch { .. }
    ));
    assert!(
        posting_ops_for(&crate::index::edge_pending::take_pending(), property.raw()).is_empty(),
        "a rejected update must not dispatch any index transition"
    );
    assert_eq!(
        store
            .find_outgoing_edge_record(handle)
            .expect("row lookup")
            .expect("row")
            .edge_inline_property_bytes(),
        &1.5f32.to_le_bytes(),
        "the previous value stays canonical"
    );
    store.set_federation_routing(None).expect("clear routing");
}
