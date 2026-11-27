use super::helpers::{edge_storage_label, lara_label};
use super::inline_update_fixture::{
    check_undirected_inline_update_reuse, install_w2_inline_property_profile,
};
use super::*;
use crate::facade::topology::EdgeInlinePropertyBatchScratch;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{
    EdgeDirectedness, EdgeLabelId, EdgeSlotIndex, PropertyId, VertexRef,
};
use gleaph_graph_kernel::entry::{EdgeOrder, EdgeOrientation, VertexId};
use std::collections::BTreeMap;
use std::panic::{AssertUnwindSafe, catch_unwind};

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
    let ids = store.insert_vertices(3).expect("bulk vertex allocation");

    assert_eq!(
        ids,
        vec![VertexId::from(0), VertexId::from(1), VertexId::from(2)]
    );
    assert_eq!(store.vertex_count(), 3);
    for id in ids {
        assert!(store.vertex_exists(id), "bulk vertex {id:?} is readable");
    }
}

#[test]
fn bulk_vertex_rows_empty_input_is_a_noop() {
    let store = GraphStore::new();

    assert!(store.insert_vertices(0).unwrap().is_empty());
    assert_eq!(store.vertex_count(), 0);
}

#[test]
fn bulk_vertex_insert_applies_request_order_without_exposing_allocation_layout() {
    let store = GraphStore::new();

    let ids = crate::facade::mutation_executor::insert_vertices_with(
        &store,
        vec![(Vec::new(), Vec::new()), (Vec::new(), Vec::new())],
        0.into(),
    )
    .expect("bulk vertex insert");

    assert_eq!(ids, vec![VertexId::from(0), VertexId::from(1)]);
    assert!(store.vertex_exists(ids[0]));
    assert!(store.vertex_exists(ids[1]));
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
        0.into(),
    )
    .expect("bulk vertex labels and properties");

    for (id, expected) in ids.into_iter().zip(["a", "b"]) {
        assert!(store.vertex_has_label(id, label));
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
        0.into(),
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
        0.into(),
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
    let before = store.vertex_count();
    let failure_guard = crate::facade::mutation_executor::test_fail_bulk_vertex_after_row_write();

    let outcome = catch_unwind(AssertUnwindSafe(|| {
        crate::facade::mutation_executor::insert_vertices_with(
            &store,
            vec![(Vec::new(), Vec::new())],
            0.into(),
        )
    }));

    assert!(
        outcome.is_err(),
        "an unexpected error after the first canonical write must panic, not return Err"
    );
    assert_eq!(
        store.vertex_count(),
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

    let mut scratch = EdgeInlinePropertyBatchScratch::default();
    let mut values = Vec::new();
    store
        .visit_out_edge_inline_property_batches_for_label(
            source,
            lara_label(label_id.pack(EdgeDirectedness::Directed)),
            EdgeOrder::Ascending,
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

    let mut scratch = EdgeInlinePropertyBatchScratch::default();
    let mut values = Vec::new();
    store
        .visit_in_edge_inline_property_batches_for_label(
            target,
            lara_label(label_id.pack(EdgeDirectedness::Directed)),
            EdgeOrder::Ascending,
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
    install_w2_inline_property_profile(label_id);

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
            reverse.occurrence(EdgeOrientation::Reverse),
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

/// The boundary the promotion still handles: 2 048 valueless parallel edges on one bucket.
///
/// Companion to `valueless_parallel_edges_promote_past_two_thousand_rows`, which fails and is ignored.
/// Together they pin the exact edge of the slice-ownership defect (ADR 0098, 2026-11-04 amendment):
/// promotion works up to here and breaks on the next doubling, so a fix that moves the boundary instead
/// of removing it is visible.
/// A large bucket must not stop a second label from taking a descriptor row on the same vertex.
///
/// Reproduces `GAP-GRAPH-TWO-LABELS-ON-A-BIG-VERTEX`: after 10 000 edges on one label, the fifth edge of a
/// second label fails with `CollectAllocationOverflow`, and the recorded construction sites show the
/// tiny-insert branch adding one to a `degree` of `u32::MAX` - a vacated bucket slot read as `0xFF`, i.e.
/// the second label's descriptor row names a slot that was never written.
#[test]
fn a_large_bucket_does_not_block_a_second_label() {
    let store = GraphStore::new();
    let hub = store
        .insert_vertex_named(["SecondLabelHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    let a = store
        .insert_vertex_named(["SecondLabelA"], Vec::<(&str, Value)>::new())
        .expect("a");
    let b = store
        .insert_vertex_named(["SecondLabelB"], Vec::<(&str, Value)>::new())
        .expect("b");
    for i in 0..10_000u32 {
        store
            .insert_directed_edge_named(hub, a, Some("BigLabel"), Vec::<(&str, Value)>::new())
            .unwrap_or_else(|error| panic!("big label insert {i} failed: {error:?}"));
    }
    for i in 0..500u32 {
        store
            .insert_directed_edge_named(hub, b, Some("SmallLabel"), Vec::<(&str, Value)>::new())
            .unwrap_or_else(|error| panic!("small label insert {i} failed: {error:?}"));
    }
    assert_eq!(
        store.directed_out_edges(hub).expect("out edges").len(),
        10_500
    );
}

#[test]
fn valueless_parallel_edges_promote_at_two_thousand_rows() {
    let store = GraphStore::new();
    let hub = store
        .insert_vertex_named(["PromoteAt2kHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    let dst = store
        .insert_vertex_named(["PromoteAt2kDst"], Vec::<(&str, Value)>::new())
        .expect("dst");
    for i in 0..2048u32 {
        store
            .insert_directed_edge_named(hub, dst, Some("PromoteAt2k"), Vec::<(&str, Value)>::new())
            .unwrap_or_else(|error| panic!("insert {i} failed: {error:?}"));
    }
    assert_eq!(
        store.directed_out_edges(hub).expect("out edges").len(),
        2048
    );
    assert_eq!(store.directed_in_edges(dst).expect("in edges").len(), 2048);
}

/// A bucket with ~2 048 valueless parallel edges must survive promotion.
///
/// This is the reproduction for the slice-ownership defect recorded in ADR 0098's 2026-11-04 amendment:
/// `LeafArena::slices` derives a vertex's slice as the gap to its neighbour, and `place` handed that gap
/// out without taking it from the free-span store, so the promotion later released a tail inside a free
/// span the store still owned (`OverlapPrevious`). Claims are best-effort now: whatever the store owns is
/// taken, the vertex's own tile slack is left alone.
#[test]
fn valueless_parallel_edges_promote_past_two_thousand_rows() {
    let store = GraphStore::new();
    let hub = store
        .insert_vertex_named(["PromotePast2kHub"], Vec::<(&str, Value)>::new())
        .expect("hub");
    let dst = store
        .insert_vertex_named(["PromotePast2kDst"], Vec::<(&str, Value)>::new())
        .expect("dst");
    for i in 0..4096u32 {
        store
            .insert_directed_edge_named(
                hub,
                dst,
                Some("PromotePast2k"),
                Vec::<(&str, Value)>::new(),
            )
            .unwrap_or_else(|error| panic!("insert {i} failed: {error:?}"));
    }
    assert_eq!(
        store.directed_out_edges(hub).expect("out edges").len(),
        4096
    );
}

#[test]
fn updating_undirected_edge_inline_property_updates_both_storage_rows() {
    check_undirected_inline_update_reuse(32);
}

#[test]
fn updating_undirected_edge_inline_property_grows_when_retired_span_too_small() {
    check_undirected_inline_update_reuse(8);
}

#[test]
fn updating_directed_self_loop_inline_property_updates_both_orientations() {
    let store = GraphStore::new();
    let vertex = store.insert_vertex().expect("vertex");
    let label_id = crate::test_labels::edge_label_id_for_name("UpdateDirectedSelfLoopInline");
    install_w2_inline_property_profile(label_id);
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
    install_w2_inline_property_profile(label_id);
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
    install_w2_inline_property_profile(label_id);
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
    install_w2_inline_property_profile(label_id);
    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
    let bogus = EdgeHandle::at_slot(vertex, wire_label, 99);

    let err = store
        .update_edge_inline_property_at_occurrence(
            bogus.occurrence(EdgeOrientation::Forward),
            &[4, 0],
        )
        .expect_err("missing occurrence must fail closed");
    assert!(
        format!("{err:?}").contains("SourceNotFound"),
        "expected SourceNotFound, got {err:?}"
    );
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
        .for_each_directed_out_edges_for_label(
            s,
            label_id,
            gleaph_graph_kernel::entry::EdgeOrder::Ascending,
            |edge| {
                weights.push(u16::from_le_bytes(
                    edge.edge_inline_property_bytes().try_into().unwrap(),
                ));
            },
        )
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
        .for_each_directed_out_edges_for_label(
            a,
            label_id,
            gleaph_graph_kernel::entry::EdgeOrder::Ascending,
            |edge| {
                weights.push(u16::from_le_bytes(
                    edge.edge_inline_property_bytes().try_into().unwrap(),
                ));
            },
        )
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
        .for_each_directed_out_edges(a, EdgeOrder::Ascending, |edge| {
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
    install_w2_inline_property_profile(label_id);

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
    install_w2_inline_property_profile(label_id);

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
    install_w2_inline_property_profile(label_id);

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
        .for_each_directed_out_edges_for_label(
            source,
            label_id,
            gleaph_graph_kernel::entry::EdgeOrder::Ascending,
            |edge| {
                values_by_slot.insert(
                    edge.edge_slot_index.raw(),
                    edge.edge_inline_property_bytes().to_vec(),
                );
            },
        )
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
        lara_label(edge_storage_label(
            Some(low_label),
            EdgeDirectedness::Directed
        ))
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
        Some(lara_label(edge_storage_label(
            Some(undirected_label),
            EdgeDirectedness::Undirected
        )))
    );
    assert!(store.edge_is_undirected(source, &edge).unwrap());
}

#[test]
fn inserts_vertices_and_edges_through_facade() {
    let store = GraphStore::new();
    let start = store.vertex_count();
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
    let scan_from_forward = store.scan_only_canonical_edge_handle(handle, EdgeOrientation::Forward);
    let scan_from_reverse =
        store.scan_only_canonical_edge_handle(reverse, EdgeOrientation::Reverse);
    assert_eq!(scan_from_forward.expect("forward ScanOnly lookup"), handle);
    assert_eq!(scan_from_reverse.expect("reverse ScanOnly lookup"), handle);
}

#[test]
fn timer_maintenance_tick_runs_on_empty_graph() {
    let store = GraphStore::new();
    let report = store.run_timer_maintenance_tick().expect("tick");
    assert_eq!(report.remaining_queue_len, 0);
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
fn post_insert_maintenance_reclaims_parallel_overflow_bucket_for_inline_properties() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let label = crate::test_labels::edge_label_id_for_name("PostInsertOverflowReclaim");
    install_w2_inline_property_profile(label);

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

    let mut scratch = EdgeInlinePropertyBatchScratch::default();
    let mut edge_count = 0;
    store
        .visit_directed_out_edge_inline_property_batches_for_label(
            source,
            label,
            EdgeOrder::Descending,
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
            handle.occurrence(EdgeOrientation::Forward),
            property,
            Value::Int64(7),
        )
        .expect("set");

    assert_eq!(
        store.edge_property(handle.occurrence(EdgeOrientation::Forward), property)?,
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
            forward.occurrence(EdgeOrientation::Forward),
            property,
            Value::Int64(9),
        )
        .expect("set");

    let reverse = store
        .find_first_reverse_handle(target, wire_label, |edge| edge.neighbor_vid() == source)
        .expect("reverse scan")
        .expect("reverse half");

    assert_eq!(
        store.edge_property(reverse.occurrence(EdgeOrientation::Reverse), property)?,
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
            forward.occurrence(EdgeOrientation::Forward),
            property,
            Value::Int64(1),
        )
        .expect("set");

    let reverse = store
        .find_first_reverse_handle(vertex, wire_label, |edge| edge.neighbor_vid() == vertex)
        .expect("reverse scan")
        .expect("reverse half");

    assert_eq!(
        store.edge_property(forward.occurrence(EdgeOrientation::Forward), property)?,
        Some(Value::Int64(1))
    );
    assert_eq!(
        store.edge_property(reverse.occurrence(EdgeOrientation::Reverse), property)?,
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
            EdgeHandle::at_slot(owner, wire_label, 0).occurrence(EdgeOrientation::Forward),
            property,
            Value::Int64(42),
        )
        .expect("set at max-owner");

    assert_eq!(
        store.edge_property(
            EdgeHandle::at_slot(owner, wire_label, 0).occurrence(EdgeOrientation::Forward),
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
            handle.occurrence(EdgeOrientation::Forward),
            property,
            Value::Int64(3),
        )
        .expect("set");

    assert_eq!(
        store.edge_property(
            EdgeHandle::at_slot(vertex, wire_label, 0).occurrence(EdgeOrientation::Forward),
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
            first.occurrence(EdgeOrientation::Forward),
            property,
            Value::Int64(1),
        )
        .expect("set first");
    store
        .set_edge_property(
            second.occurrence(EdgeOrientation::Forward),
            property,
            Value::Int64(2),
        )
        .expect("set second");

    assert_eq!(
        store.edge_property(first.occurrence(EdgeOrientation::Forward), property)?,
        Some(Value::Int64(1))
    );
    assert_eq!(
        store.edge_property(second.occurrence(EdgeOrientation::Forward), property)?,
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
            forward.occurrence(EdgeOrientation::Forward),
            property,
            Value::Int64(5),
        )
        .expect("set");

    // Remove the edge in the store alone, leaving its sidecar row behind: a hole.
    assert!(
        crate::facade::topology::change::remove_edge(forward.occurrence(EdgeOrientation::Forward))
            .expect("remove edge"),
        "edge existed"
    );

    let err = store
        .edge_property(forward.occurrence(EdgeOrientation::Forward), property)
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
            bogus.occurrence(EdgeOrientation::Forward),
            property,
            Value::Int64(1),
        )
        .expect_err("missing source must fail closed");
    assert!(
        format!("{err:?}").contains("SourceNotFound"),
        "expected SourceNotFound, got {err:?}"
    );

    // Index postings and property store must remain unchanged.
    let repeated = store.edge_property(bogus.occurrence(EdgeOrientation::Forward), property);
    assert!(repeated.is_err(), "repeated lookup must remain failed");
    assert_eq!(format!("{:?}", repeated.unwrap_err()), format!("{:?}", err));
    Ok(())
}

#[test]
fn unordered_default_reuses_tombstoned_slot() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let targets: Vec<_> = (0..4)
        .map(|_| store.insert_vertex().expect("target"))
        .collect();
    // No execution-resolved labels: undeclared labels resolve to the Unordered default (ADR 0052
    // §1), which places an insert in the first hole after the last live edge to the same
    // neighbour (none here), else after every position (ADR 0136, Decision 5).
    let handles: Vec<_> = targets[..3]
        .iter()
        .map(|&target| {
            store
                .insert_directed_edge(source, target, None)
                .expect("edge")
        })
        .collect();
    assert_eq!(
        handles
            .iter()
            .map(|h| h.slot_index.raw())
            .collect::<Vec<_>>(),
        [0, 1, 2]
    );
    store
        .delete_edge_by_handle(handles[1])
        .expect("delete middle");
    let reused = store
        .insert_directed_edge(source, targets[3], None)
        .expect("reused");
    assert_eq!(
        reused.slot_index.raw(),
        1,
        "Unordered default must reuse the hole before appending"
    );
    assert_eq!(store.directed_out_edges(source).expect("out").len(), 3);
}

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
            text_targets: Default::default(),
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
    let (store, _scalar_label, property, _scalar_edge, _scalar_catalog) =
        inline_scalar_posting_fixture(110, 15, 915, "");
    // The struct projection is another label's profile: two F32 leaves at offsets 0 and 4. A
    // label keeps its inline width once it has one (ADR 0136, Decision 7).
    let label = EdgeLabelId::from_raw(16);
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
    let _catalog = inline_posting_test_catalog(110, 16, property.raw(), "score");
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

#[test]
fn set_index_canister_repoints_the_local_federation_routing() {
    use gleaph_graph_kernel::federation::ShardId;
    // ADR 0054: a provisioned indexless-bootstrap shard is installed with the anonymous index
    // sentinel in its local routing; the Router's retrofit handshake re-points it via
    // `admin_set_index_canister` so index flushes and the maintenance timer dial the real target.
    let store = GraphStore::new();
    store
        .set_federation_routing(Some(crate::facade::FederationRouting {
            router_canister: candid::Principal::from_slice(&[7; 29]),
            index_canister: candid::Principal::anonymous(),
            shard_id: ShardId::new(0),
            vector_canister: None,
            text_targets: Default::default(),
        }))
        .expect("configure indexless routing");

    let target = candid::Principal::from_slice(&[8; 29]);
    store.set_index_canister(target).expect("retrofit attach");
    assert_eq!(
        store.federation_routing().expect("routing").index_canister,
        target,
        "the graph leg must re-point the local index target away from the sentinel"
    );
    store.set_federation_routing(None).expect("clear routing");
}

#[test]
fn set_index_canister_rejects_a_graph_without_federation_routing() {
    let store = GraphStore::new();
    let err = store
        .set_index_canister(candid::Principal::from_slice(&[8; 29]))
        .expect_err("a standalone graph has no routing to re-point");
    assert!(matches!(
        err,
        crate::facade::GraphMetadataError::MissingFederationRouting
    ));
    assert!(store.federation_routing().is_none());
}

#[test]
fn text_targets_attach_retire_and_reattach_one_definition_at_a_time() {
    use crate::facade::stable::metadata::{TextTarget, TextTargetEntry};
    let store = GraphStore::new();
    store
        .set_federation_routing(Some(crate::facade::FederationRouting {
            router_canister: candid::Principal::from_slice(&[7; 29]),
            index_canister: candid::Principal::from_slice(&[3; 29]),
            shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
            vector_canister: None,
            text_targets: Default::default(),
        }))
        .expect("configure routing");
    let first = candid::Principal::from_slice(&[8; 29]);
    let second = candid::Principal::from_slice(&[9; 29]);
    let entry = |label_id, property_id, target| TextTargetEntry {
        label_id,
        property_id,
        target,
    };
    let targets = || store.federation_routing().expect("routing").text_targets;

    store.set_text_target(1, 10, first).expect("attach");
    store
        .set_text_target(1, 11, second)
        .expect("attach another");
    store.retire_text_target(1, 10).expect("retire");
    // A definition this shard was never attached to is still recorded retired, so rows a late
    // mutation persists for it are discarded too.
    store.retire_text_target(2, 20).expect("retire unattached");
    assert_eq!(
        targets(),
        vec![
            entry(1, 11, TextTarget::Attached(second)),
            entry(1, 10, TextTarget::Retired),
            entry(2, 20, TextTarget::Retired),
        ]
    );

    store
        .retire_text_target(1, 10)
        .expect("retire is idempotent");
    store
        .set_text_target(1, 10, second)
        .expect("re-attach replaces the retirement");
    assert_eq!(
        targets(),
        vec![
            entry(1, 11, TextTarget::Attached(second)),
            entry(2, 20, TextTarget::Retired),
            entry(1, 10, TextTarget::Attached(second)),
        ]
    );

    store.set_federation_routing(None).expect("clear routing");
    assert!(matches!(
        store.retire_text_target(1, 10),
        Err(crate::facade::GraphMetadataError::MissingFederationRouting)
    ));
}

#[test]
fn bulk_vertex_insert_derives_text_ops_when_catalog_installed() {
    let store = GraphStore::new();
    let label = crate::test_labels::vertex_label_id_for_name("BulkText");
    let property = crate::test_labels::property_id_for_name("body");
    store
        .set_federation_routing(Some(crate::facade::FederationRouting {
            router_canister: candid::Principal::management_canister(),
            index_canister: candid::Principal::management_canister(),
            shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
            vector_canister: Some(candid::Principal::management_canister()),
            text_targets: vec![crate::facade::stable::metadata::TextTargetEntry {
                label_id: 1,
                property_id: 2,
                target: crate::facade::stable::metadata::TextTarget::Attached(
                    candid::Principal::management_canister(),
                ),
            }],
        }))
        .expect("set routing");
    crate::index::text_pending::clear_pending();
    let _catalog = crate::index::text_catalog_context::enter_indexed(&[
        crate::index::text_catalog_context::IndexedTextSpec {
            property_id: property,
            labels: vec![label],
        },
    ]);
    crate::facade::mutation_executor::insert_vertices_with(
        &store,
        vec![(
            vec![label],
            vec![(property, gleaph_gql::Value::Text("graph traversal".into()))],
        )],
        0.into(),
    )
    .expect("bulk insert");
    assert_eq!(
        crate::index::text_pending::pending_len(),
        1,
        "bulk-loaded text property must enqueue one text upsert"
    );
    crate::index::text_pending::clear_pending();
}

/// The work list's compaction (ADR 0136, Decision 6): a removal that leaves a bucket with as many
/// holes as live edges lists it, and a drain compacts it, moving each surviving canonical half's
/// inline value, sidecar properties and index postings with it.
mod compaction {
    use super::*;
    use crate::facade::store::MaintenanceBudget;

    const UNLIMITED: MaintenanceBudget = MaintenanceBudget {
        max_instructions: 0,
        reserve_instructions: 0,
        max_steps: None,
    };

    fn out_slots(store: &GraphStore, source: VertexId) -> Vec<(u32, VertexId, Vec<u8>)> {
        store
            .directed_out_edges(source)
            .expect("out edges")
            .into_iter()
            .map(|edge| {
                (
                    edge.edge_slot_index.raw(),
                    edge.neighbor_vid(),
                    edge.edge_inline_property_bytes().to_vec(),
                )
            })
            .collect()
    }

    #[test]
    fn a_sparse_bucket_is_listed_and_a_drain_compacts_it() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let targets: Vec<_> = (0..3)
            .map(|_| store.insert_vertex().expect("target"))
            .collect();
        let label = crate::test_labels::edge_label_id_for_name("ListedCompaction");
        install_w2_inline_property_profile(label);
        let handles: Vec<_> = targets
            .iter()
            .enumerate()
            .map(|(value, &target)| {
                store
                    .insert_directed_edge_with_inline_property_bytes(
                        source,
                        target,
                        Some(label),
                        &(value as u16 + 10).to_le_bytes(),
                    )
                    .expect("insert")
            })
            .collect();
        assert_eq!(store.maintenance_queue_len(), 0, "inserts list nothing");

        // One hole beside two live edges does not list the source's out bucket; the first
        // target's in bucket, emptied, is compacted at once, which drops it.
        assert!(
            crate::facade::topology::change::remove_edge(
                handles[0].occurrence(EdgeOrientation::Forward)
            )
            .expect("remove")
        );
        assert_eq!(
            store.maintenance_queue_len(),
            0,
            "an emptied bucket is compacted, not listed"
        );
        // Two holes beside one live edge: the source's out bucket is listed; the second
        // target's emptied in bucket is compacted at once.
        assert!(
            crate::facade::topology::change::remove_edge(
                handles[1].occurrence(EdgeOrientation::Forward)
            )
            .expect("remove")
        );
        assert_eq!(store.maintenance_queue_len(), 1, "sparse out bucket listed");
        assert_eq!(
            out_slots(&store, source),
            vec![(2, targets[2], 12u16.to_le_bytes().to_vec())],
            "a removal moves nothing"
        );

        let report = store.run_maintenance_best_effort(UNLIMITED).expect("drain");
        assert_eq!(report.remaining_queue_len, 0);
        assert!(report.processed_work_items > 0);
        assert_eq!(
            out_slots(&store, source),
            vec![(0, targets[2], 12u16.to_le_bytes().to_vec())],
            "the survivor moved to the first position with its value"
        );
        assert_eq!(
            store.directed_in_edges(targets[2]).expect("in edges").len(),
            1,
            "the survivor's other half is still paired"
        );
    }

    #[test]
    fn compaction_moves_values_and_property_sidecars_with_their_edges() {
        let store = GraphStore::new();
        let source = store.insert_vertex().expect("source");
        let targets: Vec<_> = (0..4)
            .map(|_| store.insert_vertex().expect("target"))
            .collect();
        let label = crate::test_labels::edge_label_id_for_name("CompactionCarriesSidecars");
        install_w2_inline_property_profile(label);
        let property = store.get_or_insert_property_id("marker").expect("property");
        let handles: Vec<_> = targets
            .iter()
            .enumerate()
            .map(|(index, &target)| {
                let handle = store
                    .insert_directed_edge_with_inline_property_bytes(
                        source,
                        target,
                        Some(label),
                        &(index as u16 + 1).to_le_bytes(),
                    )
                    .expect("insert");
                store
                    .set_edge_property(
                        handle.occurrence(EdgeOrientation::Forward),
                        property,
                        Value::Int64(index as i64 + 100),
                    )
                    .expect("set property");
                handle
            })
            .collect();

        // Deleting two of four leaves two holes beside two live edges; the delete message
        // drains the listed compaction (native builds drain fully).
        store.delete_edge_by_handle(handles[0]).expect("delete 0");
        store.delete_edge_by_handle(handles[2]).expect("delete 2");
        assert_eq!(store.maintenance_queue_len(), 0, "drained in the message");

        assert_eq!(
            out_slots(&store, source),
            vec![
                (0, targets[1], 2u16.to_le_bytes().to_vec()),
                (1, targets[3], 4u16.to_le_bytes().to_vec()),
            ],
            "survivors keep their order and values at the packed positions"
        );
        let wire_label = handles[0].label_id;
        for (slot, expected) in [(0u32, 101i64), (1, 103)] {
            assert_eq!(
                store
                    .edge_property(
                        EdgeHandle::at_slot(source, wire_label, slot)
                            .occurrence(EdgeOrientation::Forward),
                        property,
                    )
                    .expect("read property"),
                Some(Value::Int64(expected)),
                "the sidecar property moved with its edge to slot {slot}"
            );
        }
    }

    #[test]
    fn compaction_rekeys_inline_scalar_index_postings_to_the_new_slot() {
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
                text_targets: Default::default(),
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
        let first_edge = store
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
        crate::index::edge_pending::take_pending(); // swallow insert postings

        // One hole beside one live edge: the delete lists the bucket and its drain moves the
        // 2.5 edge from slot 1 to slot 0.
        store
            .delete_edge_by_handle(first_edge)
            .expect("delete first");
        let wire = label.pack(EdgeDirectedness::Directed).raw();
        let pending = crate::index::edge_pending::take_pending();
        let removed_at_1 = pending.iter().any(|op| {
            matches!(op, crate::index::edge_pending::PendingEdgePostingOp::Remove {
                property_id, slot_index, label_id, ..
            } if *property_id == property.raw() && *slot_index == 1 && *label_id == wire)
        });
        let inserted_at_0 = pending.iter().any(|op| {
            matches!(op, crate::index::edge_pending::PendingEdgePostingOp::Insert {
                property_id, slot_index, label_id, ..
            } if *property_id == property.raw() && *slot_index == 0 && *label_id == wire)
        });
        assert!(
            removed_at_1,
            "rekey must remove the inline-scalar posting at the old slot: {pending:?}"
        );
        assert!(
            inserted_at_0,
            "rekey must insert the inline-scalar posting at the new slot: {pending:?}"
        );
        store.set_federation_routing(None).expect("clear routing");
    }

    #[test]
    fn incoming_side_compaction_leaves_canonical_sidecars_in_place() {
        let store = GraphStore::new();
        let first = store.insert_vertex().expect("first");
        let second = store.insert_vertex().expect("second");
        let target = store.insert_vertex().expect("target");
        let label = crate::test_labels::edge_label_id_for_name("IncomingCompaction");
        let property = store.get_or_insert_property_id("marker").expect("property");
        let doomed = store
            .insert_directed_edge(first, target, Some(label))
            .expect("first edge");
        let kept = store
            .insert_directed_edge(second, target, Some(label))
            .expect("second edge");
        store
            .set_edge_property(
                kept.occurrence(EdgeOrientation::Forward),
                property,
                Value::Int64(7),
            )
            .expect("set property");

        // The target's in bucket is left with one hole and one live half and is compacted; the
        // half that moves there is not canonical, so no sidecar is re-keyed.
        store.delete_edge_by_handle(doomed).expect("delete");
        assert_eq!(store.maintenance_queue_len(), 0);
        let in_edges = store.directed_in_edges(target).expect("in edges");
        assert_eq!(in_edges.len(), 1);
        assert_eq!(in_edges[0].neighbor_vid(), second);
        assert_eq!(
            in_edges[0].edge_slot_index.raw(),
            0,
            "in half packed to slot 0"
        );
        assert_eq!(
            store
                .edge_property(kept.occurrence(EdgeOrientation::Forward), property)
                .expect("read"),
            Some(Value::Int64(7)),
            "the canonical half did not move"
        );
        assert_eq!(
            store
                .counterpart_edge_occurrence(kept.occurrence(EdgeOrientation::Forward))
                .expect("counterpart")
                .slot_index
                .raw(),
            0,
            "the counterpart is found at its new position"
        );
    }

    /// A compaction step of an indexed label reaches the index as one move per moved posting:
    /// its value read once at the new slot, sent once.
    #[test]
    fn compacting_an_indexed_label_sends_one_move_per_moved_posting() {
        use gleaph_graph_kernel::entry::{EdgeInlinePropertyEncoding, EdgeInlinePropertyProfile};
        use gleaph_graph_kernel::index::IndexPostingMutation;
        let store = GraphStore::new();
        store
            .set_federation_routing(Some(crate::facade::FederationRouting {
                router_canister: candid::Principal::management_canister(),
                index_canister: candid::Principal::management_canister(),
                shard_id: gleaph_graph_kernel::federation::ShardId::new(0),
                vector_canister: None,
                text_targets: Default::default(),
            }))
            .expect("configure index routing");
        let label = EdgeLabelId::from_raw(41);
        let property = PropertyId::from_raw(941);
        crate::test_labels::install_test_edge_inline_property_profile(
            label,
            EdgeInlinePropertyProfile {
                byte_width: 8,
                encoding: EdgeInlinePropertyEncoding::RawU64,
            },
        );
        crate::test_labels::install_test_edge_inline_property(label, property);
        let _catalog = super::inline_posting_test_catalog(141, label.raw(), property.raw(), "");
        let source = store.insert_vertex().expect("source");
        let handles: Vec<_> = (0..8u64)
            .map(|value| {
                let target = store.insert_vertex().expect("target");
                store
                    .insert_directed_edge_with_inline_property_bytes(
                        source,
                        target,
                        Some(label),
                        &value.to_le_bytes(),
                    )
                    .expect("edge")
            })
            .collect();
        // Each edge's posting as its insert sent it, by slot.
        let inserted: BTreeMap<u32, Vec<u8>> = crate::index::edge_pending::take_pending()
            .into_iter()
            .filter_map(|op| match op {
                crate::index::edge_pending::PendingEdgePostingOp::Insert {
                    slot_index,
                    payload_bytes,
                    ..
                } => Some((slot_index, payload_bytes)),
                crate::index::edge_pending::PendingEdgePostingOp::Remove { .. } => None,
            })
            .collect();
        assert_eq!(inserted.len(), 8, "every insert posts its value");
        // Remove the first half in the store alone: the bucket is listed, nothing moved yet.
        for handle in &handles[..4] {
            assert!(
                crate::facade::topology::change::remove_edge(
                    handle.occurrence(EdgeOrientation::Forward)
                )
                .expect("remove")
            );
        }
        crate::index::edge_pending::clear_pending();

        let report = store.run_maintenance_best_effort(UNLIMITED).expect("drain");
        assert!(report.processed_work_items > 0);
        let pending = crate::index::edge_pending::take_pending();
        let mutations = crate::index::edge_pending::to_index_mutations(&pending).expect("Active");
        let moves: Vec<(u32, u32, Vec<u8>)> = mutations
            .into_iter()
            .map(|(mutation, _)| match mutation {
                IndexPostingMutation::EdgePropertyMove {
                    from_slot,
                    to_slot,
                    value,
                    ..
                } => (from_slot, to_slot, value),
                other => panic!("a moved posting sent as {other:?}"),
            })
            .collect();
        assert_eq!(
            moves
                .iter()
                .map(|(from, to, _)| (*from, *to))
                .collect::<Vec<_>>(),
            [(4, 0), (5, 1), (6, 2), (7, 3)],
            "each surviving edge's posting moves with it"
        );
        for (from, _, value) in &moves {
            assert_eq!(
                Some(value),
                inserted.get(from),
                "slot {from} keeps its value"
            );
        }
    }
}

/// An export window over a bucket reports where it stopped, so the caller charges the positions
/// it read: nothing for a vertex without the bucket, the bucket's positions once it is
/// exhausted, and the window when more positions follow.
#[test]
fn out_edge_window_reports_the_positions_it_read() {
    let store = GraphStore::new();
    let owner = store.insert_vertex().expect("owner");
    let empty = store.insert_vertex().expect("empty");
    let label = crate::test_labels::edge_label_id_for_name("ExportWindow");
    for _ in 0..3 {
        let target = store.insert_vertex().expect("target");
        store
            .insert_directed_edge(owner, target, Some(label))
            .expect("edge");
    }
    let window = |vertex, start, limit| {
        let mut seen = Vec::new();
        let cursor = store
            .visit_out_edge_window_from_slot(
                vertex,
                label,
                EdgeDirectedness::Directed,
                start,
                limit,
                |edge| seen.push(edge.edge_slot_index.raw()),
            )
            .expect("window");
        (cursor, seen)
    };
    assert_eq!(
        window(empty, 0, 16),
        ((0, true), vec![]),
        "no bucket: nothing read"
    );
    assert_eq!(window(owner, 0, 2), ((2, false), vec![0, 1]), "more follow");
    assert_eq!(
        window(owner, 2, 16),
        ((3, true), vec![2]),
        "exhausted at its end"
    );
}
