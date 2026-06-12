use super::helpers::{edge_storage_label, lara_label};
use super::*;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{
    EdgeDirectedness, EdgeLabelId, EdgeSlotIndex, EdgeWeightProfile, VertexRef, WeightEncoding,
};
use ic_stable_lara::{
    MaintenanceBudget, OutEdgeOrder, VertexId,
    labeled::{BucketLabelKey as LaraLabelId, LabeledEdgePayloadBatchScratch, LabeledOrientation},
    traits::CsrEdge,
};
use std::collections::BTreeMap;

fn install_w2_weight_profile(_store: &GraphStore, label_id: EdgeLabelId) {
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );
}

#[test]
fn install_edge_label_weight_profile_stores_payload_and_derives_weight_view() {
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};

    let store = GraphStore::new();
    let label_id = crate::test_labels::edge_label_id_for_name("WeightCompatView");
    let weight = EdgeWeightProfile {
        encoding: WeightEncoding::Linear {
            min: 0.0,
            max: 10.0,
        },
    };
    let expected_payload = EdgePayloadProfile::from(weight.clone());
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(weight.clone()),
    );

    assert_eq!(store.edge_label_weight_profile(label_id), Some(weight));
    assert_eq!(
        store.edge_label_payload_profile(label_id),
        Some(expected_payload)
    );
    assert!(matches!(
        store
            .edge_label_payload_profile(label_id)
            .expect("payload")
            .encoding,
        EdgePayloadEncoding::WeightLinearU16 { .. }
    ));
}

#[test]
fn insert_rejects_payload_bytes_when_label_profile_expects_zero_width() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("ZeroWidthOnly");

    let err = store
        .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &[1, 0])
        .expect_err("new label defaults to zero-byte values");
    assert!(
        matches!(
            err,
            GraphStoreError::EdgePayloadWidthMismatch {
                expected: 0,
                actual: 2,
                ..
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn insert_rejects_payload_bytes_when_profile_width_differs() {
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};

    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("ProfileWidthMismatch");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::WeightRawU16,
        },
    );

    let err = store
        .insert_directed_edge_with_payload_bytes(
            source,
            target,
            Some(label_id),
            &42i32.to_le_bytes(),
        )
        .expect_err("four-byte payload on W2 label");
    assert!(
        matches!(
            err,
            GraphStoreError::EdgePayloadWidthMismatch {
                expected: 2,
                actual: 4,
                ..
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn insert_rejects_invalid_edge_payload_byte_width() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("InvalidValueWidth");

    let err = store
        .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &[1, 2, 3])
        .expect_err("three-byte payload without a matching profile");
    assert!(
        matches!(
            err,
            GraphStoreError::EdgePayloadWidthMismatch {
                expected: 0,
                actual: 3,
                ..
            }
        ),
        "unexpected error: {err:?}"
    );
}

#[test]
fn i32_edge_payload_profile_round_trip() {
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};

    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("I32CostRoad");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: 4,
            encoding: EdgePayloadEncoding::RawI32,
        },
    );
    store
        .insert_directed_edge_with_payload_bytes(
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
    assert_eq!(edge.payload_bytes(), &100i32.to_le_bytes());
}

#[test]
fn graph_store_visits_fixed_label_edge_payload_batches() {
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};

    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let label_id = crate::test_labels::edge_label_id_for_name("BatchValues");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::RawU16,
        },
    );
    store
        .insert_directed_edge_with_payload_bytes(source, first, Some(label_id), &[1, 0])
        .expect("first edge");
    store
        .insert_directed_edge_with_payload_bytes(source, second, Some(label_id), &[2, 0])
        .expect("second edge");

    let mut scratch = LabeledEdgePayloadBatchScratch::default();
    let mut values = Vec::new();
    store
        .visit_out_edge_payload_batches_for_label(
            source,
            lara_label(label_id.pack(EdgeDirectedness::Directed)),
            OutEdgeOrder::Ascending,
            &mut scratch,
            |batch| {
                assert!(batch.dense);
                values.extend(
                    batch
                        .payload_bytes
                        .chunks_exact(2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]])),
                );
            },
        )
        .expect("batch traversal");
    assert_eq!(values, vec![1, 2]);
}

#[test]
fn graph_store_visits_fixed_label_in_edge_payload_batches() {
    use gleaph_graph_kernel::entry::{EdgePayloadEncoding, EdgePayloadProfile};

    let store = GraphStore::new();
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("BatchInValues");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        EdgePayloadProfile {
            byte_width: 2,
            encoding: EdgePayloadEncoding::RawU16,
        },
    );
    store
        .insert_directed_edge_with_payload_bytes(first, target, Some(label_id), &[1, 0])
        .expect("first edge");
    store
        .insert_directed_edge_with_payload_bytes(second, target, Some(label_id), &[2, 0])
        .expect("second edge");

    let mut scratch = LabeledEdgePayloadBatchScratch::default();
    let mut values = Vec::new();
    store
        .visit_in_edge_payload_batches_for_label(
            target,
            lara_label(label_id.pack(EdgeDirectedness::Directed)),
            OutEdgeOrder::Ascending,
            &mut scratch,
            |batch| {
                assert!(batch.dense);
                values.extend(
                    batch
                        .payload_bytes
                        .chunks_exact(2)
                        .map(|b| u16::from_le_bytes([b[0], b[1]])),
                );
            },
        )
        .expect("batch traversal");
    values.sort_unstable();
    assert_eq!(values, vec![1, 2]);
}

#[test]
fn updating_directed_edge_payload_updates_forward_and_reverse_rows() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("UpdateDirectedValueBothRows");
    install_w2_weight_profile(&store, label_id);

    let forward = store
        .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &[1, 0])
        .expect("edge");
    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
    let reverse = store
        .find_first_reverse_handle_descending(target, wire_label, |edge| {
            edge.neighbor_vid() == source
        })
        .expect("reverse lookup")
        .expect("reverse edge");

    store
        .update_edge_payload_at_handle(forward, &[9, 0])
        .expect("forward update");
    assert_eq!(
        store
            .find_outgoing_edge_record(forward)
            .expect("forward lookup")
            .expect("forward edge")
            .payload_bytes(),
        &[9, 0]
    );
    assert_eq!(
        store
            .directed_in_edges(target)
            .expect("in edges")
            .into_iter()
            .find(|edge| edge.neighbor_vid() == source)
            .expect("reverse row")
            .payload_bytes(),
        &[9, 0]
    );

    store
        .update_edge_payload_at_handle(reverse, &[5, 0])
        .expect("reverse update");
    assert_eq!(
        store
            .find_outgoing_edge_record(forward)
            .expect("forward lookup after reverse update")
            .expect("forward edge after reverse update")
            .payload_bytes(),
        &[5, 0]
    );
    assert_eq!(
        store
            .directed_in_edges(target)
            .expect("in edges after reverse update")
            .into_iter()
            .find(|edge| edge.neighbor_vid() == source)
            .expect("reverse row after reverse update")
            .payload_bytes(),
        &[5, 0]
    );
}

#[test]
fn updating_undirected_edge_payload_updates_both_storage_rows() {
    let store = GraphStore::new();
    let low = store.insert_vertex().expect("low");
    let high = store.insert_vertex().expect("high");
    let label_id = crate::test_labels::edge_label_id_for_name("UpdateUndirectedValueBothRows");
    install_w2_weight_profile(&store, label_id);

    let handle = store
        .insert_undirected_edge_with_payload_bytes(low, high, Some(label_id), &[1, 0])
        .expect("edge");
    store
        .update_edge_payload_at_handle(handle, &[8, 0])
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
    assert_eq!(low_edge.payload_bytes(), &[8, 0]);
    assert_eq!(high_edge.payload_bytes(), &[8, 0]);
}

#[test]
fn forward_edge_compaction_preserves_inline_payloads() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let third = store.insert_vertex().expect("third");
    let label = crate::test_labels::edge_label_id_for_name("CompactionPreservesValues");
    crate::test_labels::install_test_edge_payload_profile(
        label,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );

    let doomed = store
        .insert_directed_edge_with_payload_bytes(source, first, Some(label), &[1, 0])
        .expect("first edge");
    store
        .insert_directed_edge_with_payload_bytes(source, second, Some(label), &[2, 0])
        .expect("second edge");
    store
        .insert_directed_edge_with_payload_bytes(source, third, Some(label), &[33, 0])
        .expect("third edge");

    store.delete_edge_by_handle(doomed).expect("delete first");
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(LabeledOrientation::Forward, source, 0)
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
    assert_eq!(third_edge.payload_bytes(), &[33, 0]);
}

#[test]
fn undirected_canonical_owner_carries_payload_bytes() {
    let store = GraphStore::new();
    let low = store.insert_vertex().expect("low");
    let high = store.insert_vertex().expect("high");
    let label_id = crate::test_labels::edge_label_id_for_name("UndirectedValueOwner");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );

    let handle = store
        .insert_undirected_edge_with_payload_bytes(low, high, Some(label_id), &[7, 0])
        .expect("undirected edge");
    let owner = store.canonical_edge_handle(handle).owner_vertex_id;
    let edge = store
        .find_outgoing_edge_record(handle)
        .expect("lookup")
        .expect("edge record");
    assert_eq!(edge.payload_bytes(), &[7, 0]);
    assert_eq!(owner, high, "higher vid owns undirected forward CSR row");

    let alias = store
        .undirected_edges(low)
        .expect("alias view")
        .into_iter()
        .find(|edge| edge.neighbor_vid() == high)
        .expect("alias half");
    assert_eq!(alias.payload_bytes(), &[7, 0]);
}

#[test]
fn inline_edge_payloads_round_trip_on_parallel_out_edges() {
    let store = GraphStore::new();
    let s = store.insert_vertex().expect("s");
    let a = store.insert_vertex().expect("a");
    let mid = store.insert_vertex().expect("mid");
    let dst = store.insert_vertex().expect("dst");
    let label_id = crate::test_labels::edge_label_id_for_name("WgtRoad");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );
    store
        .insert_directed_edge_with_payload_bytes(s, mid, Some(label_id), &10u16.to_le_bytes())
        .expect("s->mid");
    store
        .insert_directed_edge_with_payload_bytes(s, a, Some(label_id), &5u16.to_le_bytes())
        .expect("s->a");
    store
        .insert_directed_edge_with_payload_bytes(a, mid, Some(label_id), &1u16.to_le_bytes())
        .expect("a->mid");
    store
        .insert_directed_edge_with_payload_bytes(mid, dst, Some(label_id), &0u16.to_le_bytes())
        .expect("mid->dst");
    let _ = dst;
    let mut weights = Vec::new();
    store
        .for_each_directed_out_edges_for_label_unchecked(s, label_id, |edge| {
            weights.push(u16::from_le_bytes(edge.payload_bytes().try_into().unwrap()));
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
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );
    store
        .insert_directed_edge_with_payload_bytes(a, b, Some(label_id), &1u16.to_le_bytes())
        .expect("a->b");
    store
        .insert_directed_edge_with_payload_bytes(b, c, Some(label_id), &1u16.to_le_bytes())
        .expect("b->c");
    store
        .insert_directed_edge_with_payload_bytes(a, c, Some(label_id), &100u16.to_le_bytes())
        .expect("a->c");
    let mut weights = Vec::new();
    store
        .for_each_directed_out_edges_for_label_unchecked(a, label_id, |edge| {
            weights.push(u16::from_le_bytes(edge.payload_bytes().try_into().unwrap()));
        })
        .expect("out edges from a");
    weights.sort_unstable();
    assert_eq!(weights, vec![1, 100]);
}

#[test]
fn directed_out_edges_visit_attaches_inline_payloads() {
    let store = GraphStore::new();
    let a = store.insert_vertex().expect("a");
    let label_id = crate::test_labels::edge_label_id_for_name("VisitWgtRoad");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );
    for weight in 1..=8u16 {
        let t = store.insert_vertex().expect("target");
        store
            .insert_directed_edge_with_payload_bytes(a, t, Some(label_id), &weight.to_le_bytes())
            .expect("a->t");
    }
    let mut weights = Vec::new();
    store
        .for_each_directed_out_edges(a, OutEdgeOrder::Ascending, |edge| {
            weights.push(u16::from_le_bytes(edge.payload_bytes().try_into().unwrap()));
        })
        .expect("out edges");
    weights.sort_unstable();
    assert_eq!(weights, vec![1, 2, 3, 4, 5, 6, 7, 8]);
}

#[test]
fn delete_valued_directed_edge_by_handle_removes_reverse_alias_slot() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("DeleteValuedDirected");
    install_w2_weight_profile(&store, label_id);

    let first = store
        .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &[1, 0])
        .expect("first edge");
    let second = store
        .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &[2, 0])
        .expect("second edge");

    assert_eq!(store.directed_in_edges(target).expect("in before").len(), 2);
    store.delete_edge_by_handle(first).expect("delete first");

    let in_edges = store.directed_in_edges(target).expect("in after");
    assert_eq!(in_edges.len(), 1);
    assert!(in_edges.iter().all(|edge| edge.neighbor_vid() == source));

    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
    let reverse = store
        .find_first_reverse_handle_descending(target, wire_label, |edge| {
            edge.neighbor_vid() == source
        })
        .expect("reverse lookup")
        .expect("remaining reverse edge");
    assert_eq!(store.canonical_reverse_in_edge_handle(reverse), second);
}

#[test]
fn directed_reverse_alias_does_not_require_matching_slot_index() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let other_source = store.insert_vertex().expect("other source");
    let label_id = crate::test_labels::edge_label_id_for_name("DirectedAliasSlotSkew");
    install_w2_weight_profile(&store, label_id);

    store
        .insert_directed_edge_with_payload_bytes(other_source, target, Some(label_id), &[7, 0])
        .expect("preexisting edge");
    let canonical = store
        .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &[42, 0])
        .expect("skewed edge");

    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Directed));
    let reverse = store
        .find_first_reverse_handle_descending(target, wire_label, |edge| {
            edge.neighbor_vid() == source
        })
        .expect("reverse lookup")
        .expect("reverse edge");
    assert_ne!(
        reverse.slot_index, canonical.slot_index,
        "test setup should force forward/reverse slot skew"
    );
    assert_eq!(store.canonical_reverse_in_edge_handle(reverse), canonical);

    let edge = store
        .find_outgoing_edge_record(reverse)
        .expect("edge lookup")
        .expect("canonicalized edge");
    assert_eq!(edge.payload_bytes(), &[42, 0]);
}

#[test]
fn delete_valued_undirected_edge_by_handle_removes_alias_slot() {
    let store = GraphStore::new();
    let low = store.insert_vertex().expect("low");
    let high = store.insert_vertex().expect("high");
    let label_id = crate::test_labels::edge_label_id_for_name("DeleteValuedUndirected");
    install_w2_weight_profile(&store, label_id);

    let first = store
        .insert_undirected_edge_with_payload_bytes(low, high, Some(label_id), &[1, 0])
        .expect("first edge");
    let second = store
        .insert_undirected_edge_with_payload_bytes(low, high, Some(label_id), &[2, 0])
        .expect("second edge");

    store.delete_edge_by_handle(first).expect("delete first");

    let weights_from = |vertex| {
        let mut weights: Vec<u16> = store
            .undirected_edges(vertex)
            .expect("undirected edges")
            .into_iter()
            .map(|edge| u16::from_le_bytes(edge.payload_bytes().try_into().unwrap()))
            .collect();
        weights.sort_unstable();
        weights
    };
    assert_eq!(weights_from(low), vec![2]);
    assert_eq!(weights_from(high), vec![2]);

    let wire_label = lara_label(label_id.pack(EdgeDirectedness::Undirected));
    let alias = store
        .find_first_forward_handle_descending(low, wire_label, |edge| edge.neighbor_vid() == high)
        .expect("alias lookup")
        .expect("remaining alias half");
    assert_eq!(store.canonical_edge_handle(alias), second);
}

#[test]
fn unvalued_parallel_directed_inserts_align_reverse_alias_slot() {
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
    assert_ne!(first.slot_index, second.slot_index);
    assert_eq!(store.directed_in_edges(target).expect("in before").len(), 2);

    store.delete_edge_by_handle(first).expect("delete first");

    let in_edges = store.directed_in_edges(target).expect("in after");
    assert_eq!(in_edges.len(), 1);
    assert_eq!(in_edges[0].edge_slot_index.raw(), second.slot_index);
}

#[test]
fn valued_parallel_insert_returns_handles_for_each_value() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("ParallelValuedHandles");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );

    let first = store
        .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &[1, 0])
        .expect("first edge");
    let second = store
        .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &[2, 0])
        .expect("second edge");

    assert_ne!(first.slot_index, second.slot_index);
    let mut values_by_slot = BTreeMap::new();
    store
        .for_each_directed_out_edges_for_label_unchecked(source, label_id, |edge| {
            values_by_slot.insert(edge.edge_slot_index.raw(), edge.payload_bytes().to_vec());
        })
        .expect("out edges");
    assert_eq!(values_by_slot[&first.slot_index], vec![1, 0]);
    assert_eq!(values_by_slot[&second.slot_index], vec![2, 0]);
}

#[test]
fn lookup_edge_record_at_handle_includes_stored_payload_bytes() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target = store.insert_vertex().expect("target");
    let label_id = crate::test_labels::edge_label_id_for_name("LookupEdgeRecordValue");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );
    let handle = store
        .insert_directed_edge_with_payload_bytes(source, target, Some(label_id), &[4, 0])
        .expect("edge");
    let edge = store
        .find_outgoing_edge_record(handle)
        .expect("lookup")
        .expect("edge record");
    assert_eq!(edge.payload_bytes(), &[4, 0]);
}

/// Regression: vertex `a` is target of `s->a` (reverse-IN alias) and source of `a->mid`
/// (forward-OUT). Shared slot index `0` in both CSR stores must not alias across stores.
#[test]
fn forward_out_lookup_ignores_reverse_in_alias_when_slots_collide() {
    let store = GraphStore::new();
    let s = store.insert_vertex().expect("s");
    let a = store.insert_vertex().expect("a");
    let mid = store.insert_vertex().expect("mid");
    let label_id = crate::test_labels::edge_label_id_for_name("ForwardOutReverseInSlotCollision");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );
    store
        .insert_directed_edge_with_payload_bytes(s, a, Some(label_id), &[5, 0])
        .expect("s->a");
    let a_to_mid = store
        .insert_directed_edge_with_payload_bytes(a, mid, Some(label_id), &[1, 0])
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
    assert_eq!(edge.payload_bytes(), &[1, 0]);
}

#[test]
fn valued_insert_after_delete_returns_handle_for_new_edge() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let target_a = store.insert_vertex().expect("target a");
    let target_b = store.insert_vertex().expect("target b");
    let label_id = crate::test_labels::edge_label_id_for_name("TombstoneHandleLookup");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );

    let doomed = store
        .insert_directed_edge_with_payload_bytes(source, target_a, Some(label_id), &[1, 0])
        .expect("doomed edge");
    store
        .insert_directed_edge_with_payload_bytes(source, target_b, Some(label_id), &[2, 0])
        .expect("survivor edge");
    store.delete_edge_by_handle(doomed).expect("delete doomed");

    let replacement = store
        .insert_directed_edge_with_payload_bytes(source, target_a, Some(label_id), &[9, 0])
        .expect("replacement edge");
    let edge = store
        .directed_out_edges(source)
        .expect("out edges")
        .into_iter()
        .find(|edge| edge.edge_slot_index.raw() == replacement.slot_index)
        .expect("replacement edge record");
    assert_eq!(edge.payload_bytes(), &[9, 0]);
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
        .find(|edge| edge.edge_slot_index.raw() == undirected.slot_index)
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
        EdgeSlotIndex::from_raw(directed.slot_index),
        EdgeSlotIndex::from_raw(0)
    );

    let out_edges = store.directed_out_edges(source).expect("read out edges");
    assert!(out_edges.iter().any(|edge| {
        edge.target == VertexRef::local(target)
            && edge.edge_slot_index.raw() == directed.slot_index
            && !store.edge_is_undirected(source, edge).unwrap()
    }));

    let undirected = store
        .insert_undirected_edge(target, source, None)
        .expect("insert undirected edge");

    assert_eq!(undirected.owner_vertex_id, target);
    assert_eq!(
        EdgeSlotIndex::from_raw(undirected.slot_index),
        EdgeSlotIndex::from_raw(0)
    );

    let target_out_edges = store
        .undirected_edges(target)
        .expect("read target out edges");
    assert!(target_out_edges.iter().any(|edge| {
        edge.target == VertexRef::local(source)
            && edge.edge_slot_index.raw() == undirected.slot_index
            && store.edge_is_undirected(target, edge).unwrap()
    }));
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
        .set_edge_property(old_third, property, Value::Int64(33))
        .expect("set property");
    store
        .delete_edge_by_handle(first_edge)
        .expect("delete first");
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_vertex_edge_span(LabeledOrientation::Forward, source, 0)
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
        store.edge_property(new_third, property),
        Some(Value::Int64(33))
    );
    assert_eq!(store.edge_property(old_third, property), None);
}

#[test]
fn reverse_edge_compaction_moves_alias_keys() {
    let store = GraphStore::new();
    let first = store.insert_vertex().expect("first");
    let second = store.insert_vertex().expect("second");
    let third = store.insert_vertex().expect("third");
    let target = store.insert_vertex().expect("target");
    let label = crate::test_labels::edge_label_id_for_name("CompactionMovesReverseAlias");
    let other_label =
        crate::test_labels::edge_label_id_for_name("CompactionMovesReverseAliasOther");
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
        .set_edge_property(third_edge, property, Value::Int64(44))
        .expect("set property");
    let wire_label = lara_label(label.pack(EdgeDirectedness::Directed));

    store
        .delete_edge_by_handle(first_edge)
        .expect("delete first");
    store.with_graph_mut(|graph| {
        graph
            .mark_compact_dense_labeled_vertex_maintenance(LabeledOrientation::Reverse, target)
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
        store.edge_property(third_edge, property),
        Some(Value::Int64(44)),
        "canonical forward handle keeps properties across reverse compaction"
    );

    let reverse_third = store
        .find_first_reverse_handle_descending(target, wire_label, |edge| {
            edge.neighbor_vid() == third
        })
        .expect("reverse lookup after compaction")
        .expect("third reverse edge after compaction");
    assert_eq!(
        store.canonical_reverse_in_edge_handle(reverse_third),
        third_edge,
        "reverse CSR slot should still alias the canonical forward handle"
    );
    assert_eq!(
        store.edge_property(reverse_third, property),
        Some(Value::Int64(44))
    );
}

#[test]
fn post_insert_maintenance_reclaims_parallel_overflow_bucket_to_dense() {
    let store = GraphStore::new();
    let source = store.insert_vertex().expect("source");
    let label = crate::test_labels::edge_label_id_for_name("PostInsertOverflowReclaim");
    install_w2_weight_profile(&store, label);

    for i in 0..48u16 {
        let target = store.insert_vertex().expect("target");
        store
            .insert_directed_edge_with_payload_bytes(source, target, Some(label), &i.to_le_bytes())
            .unwrap_or_else(|e| panic!("edge i={i}: {e:?}"));
    }

    let mut scratch = LabeledEdgePayloadBatchScratch::default();
    let mut dense = None;
    store
        .visit_directed_out_edge_payload_batches_for_label(
            source,
            label,
            OutEdgeOrder::Descending,
            &mut scratch,
            |batch| dense = Some(batch.dense),
        )
        .expect("payload batches");

    assert_eq!(
        dense,
        Some(true),
        "post-insert maintenance should fold parallel overflow bucket to dense-eligible"
    );
    assert_eq!(
        store.directed_out_edges(source).expect("out").len(),
        48,
        "topology must stay intact after reclaim"
    );
}
