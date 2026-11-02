//! Shared native/canbench oracle for one ordinary paired inline update.
//! Setup and every assertion stay outside the measured API call.

use super::helpers::lara_label;
use super::{EdgeHandle, GraphStore};
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{EdgeDirectedness, EdgeLabelId};
use ic_stable_lara::{VertexId, labeled::LabeledOrientation, traits::CsrEdge};
use std::{collections::BTreeMap, hint::black_box};

#[cfg(all(feature = "canbench", target_family = "wasm"))]
type Measurement = canbench_rs::BenchResult;
#[cfg(not(all(feature = "canbench", target_family = "wasm")))]
type Measurement = ();

pub(super) fn install_w2_inline_property_profile(label_id: EdgeLabelId) {
    crate::test_labels::install_test_edge_inline_property_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgeInlinePropertyProfile {
            byte_width: 2,
            encoding: gleaph_graph_kernel::entry::EdgeInlinePropertyEncoding::RawU16,
        },
    );
}

/// Third step: the real fixture's insert sequence, read back for neighbour/value agreement only.
#[test]
fn undirected_valued_insert_sequence_keeps_neighbours() {
    let store = GraphStore::new();
    let low = store.insert_vertex().expect("low");
    let high = store.insert_vertex().expect("high");
    let guard = store.insert_vertex().expect("guard");
    let label = crate::test_labels::edge_label_id_for_name("UndirectedSequenceNeighbour");
    install_w2_inline_property_profile(label);

    let insert = |src: VertexId, dst: VertexId, value: u16| {
        store
            .insert_undirected_edge_with_inline_property_bytes(
                src,
                dst,
                Some(label),
                &value.to_le_bytes(),
            )
            .expect("insert");
    };
    for value in 1000..1032u16 {
        insert(high, high, value);
    }
    for value in 2000..2008u16 {
        insert(low, low, value);
    }
    insert(high, low, 5000);
    insert(guard, guard, 6000);
    for value in 3000..3008u16 {
        insert(high, high, value);
    }
    for value in 4000..4008u16 {
        insert(low, low, value);
    }

    let read = |vertex: VertexId| {
        let mut rows: Vec<(u32, VertexId, u16)> = store
            .undirected_edges(vertex)
            .expect("rows")
            .into_iter()
            .map(|edge| {
                (
                    edge.edge_slot_index.raw(),
                    edge.neighbor_vid(),
                    u16::from_le_bytes(
                        <[u8; 2]>::try_from(edge.edge_inline_property_bytes()).expect("two bytes"),
                    ),
                )
            })
            .collect();
        rows.sort_unstable_by_key(|row| row.0);
        rows
    };

    for (vertex, other) in [(low, high), (high, low)] {
        for (slot, neighbour, value) in read(vertex) {
            if value == 5000 {
                assert_eq!(neighbour, other, "cross row at slot {slot}");
            } else if value == 6000 {
                assert_eq!(neighbour, guard, "guard row at slot {slot}");
            } else {
                assert_eq!(
                    neighbour, vertex,
                    "self-loop row at slot {slot} (value {value}) points at its owner"
                );
            }
        }
    }
}

/// Second step of the reproduction: two vertices that each hold valued self-loops, joined by one
/// undirected edge - the row that lands in both buckets.
#[test]
fn undirected_cross_edge_between_valued_buckets_keeps_neighbours() {
    let store = GraphStore::new();
    let low = store.insert_vertex().expect("low");
    let high = store.insert_vertex().expect("high");
    let label = crate::test_labels::edge_label_id_for_name("UndirectedCrossNeighbour");
    install_w2_inline_property_profile(label);

    for value in 1000..1004u16 {
        store
            .insert_undirected_edge_with_inline_property_bytes(
                high,
                high,
                Some(label),
                &value.to_le_bytes(),
            )
            .expect("high self loop");
    }
    for value in 2000..2004u16 {
        store
            .insert_undirected_edge_with_inline_property_bytes(
                low,
                low,
                Some(label),
                &value.to_le_bytes(),
            )
            .expect("low self loop");
    }
    store
        .insert_undirected_edge_with_inline_property_bytes(
            high,
            low,
            Some(label),
            &5000u16.to_le_bytes(),
        )
        .expect("cross edge");

    let read = |vertex: VertexId| {
        let mut rows: Vec<(u32, VertexId, u16)> = store
            .undirected_edges(vertex)
            .expect("rows")
            .into_iter()
            .map(|edge| {
                (
                    edge.edge_slot_index.raw(),
                    edge.neighbor_vid(),
                    u16::from_le_bytes(
                        <[u8; 2]>::try_from(edge.edge_inline_property_bytes()).expect("two bytes"),
                    ),
                )
            })
            .collect();
        rows.sort_unstable_by_key(|row| row.0);
        rows
    };

    for (vertex, self_values, other, cross_value) in [
        (low, 2000u16..2004, high, 5000u16),
        (high, 1000u16..1004, low, 5000u16),
    ] {
        let rows = read(vertex);
        let mut seen_self = 0u16;
        let mut seen_cross = false;
        for (slot, neighbour, value) in rows {
            if value == cross_value {
                assert_eq!(
                    neighbour, other,
                    "cross row at slot {slot} points at the peer"
                );
                seen_cross = true;
            } else {
                assert_eq!(
                    neighbour, vertex,
                    "self-loop row at slot {slot} (value {value}) points at its owner"
                );
                assert_eq!(value, self_values.start + seen_self);
                seen_self += 1;
            }
        }
        assert!(seen_cross, "the cross row is present in both buckets");
    }
}

/// Minimal reproduction attempt for the one row that reads back with the right value and the wrong
/// neighbour: one vertex, undirected self-loops carrying inline values, read straight back.
#[test]
fn undirected_self_loops_with_inline_values_keep_their_neighbour() {
    let store = GraphStore::new();
    let v = store.insert_vertex().expect("vertex");
    let label_id = crate::test_labels::edge_label_id_for_name("UndirectedSelfLoopNeighbour");
    install_w2_inline_property_profile(label_id);

    for value in 1..=8u16 {
        store
            .insert_undirected_edge_with_inline_property_bytes(
                v,
                v,
                Some(label_id),
                &value.to_le_bytes(),
            )
            .expect("self loop");
    }

    let mut rows: Vec<_> = store
        .undirected_edges(v)
        .expect("undirected rows")
        .into_iter()
        .map(|edge| {
            (
                edge.edge_slot_index.raw(),
                edge.neighbor_vid(),
                <[u8; 2]>::try_from(edge.edge_inline_property_bytes()).expect("two bytes"),
            )
        })
        .collect();
    rows.sort_unstable_by_key(|row| row.0);
    for (index, (_, neighbour, bytes)) in rows.iter().enumerate() {
        assert_eq!(
            *neighbour, v,
            "row {index} is a self-loop and must point at its owner"
        );
        assert_eq!(
            u16::from_le_bytes(*bytes),
            index as u16 + 1,
            "row {index} keeps the value it was inserted with"
        );
    }
    assert_eq!(rows.len(), 8);
}

pub(super) fn check_undirected_inline_update_reuse(canonical_prefix: u16) -> Measurement {
    let store = GraphStore::new();
    let low = store.insert_vertex().expect("low");
    let high = store.insert_vertex().expect("high");
    let guard = store.insert_vertex().expect("guard");
    let label_id = crate::test_labels::edge_label_id_for_name("UpdateUndirectedValueBothRows");
    install_w2_inline_property_profile(label_id);
    let label = lara_label(label_id.pack(EdgeDirectedness::Undirected));
    let stats = || {
        store.with_graph_mut(|graph| {
            (
                graph
                    .forward()
                    .inline_property_bytes_storage_stats()
                    .unwrap(),
                graph
                    .reverse()
                    .inline_property_bytes_storage_stats()
                    .unwrap(),
            )
        })
    };
    let placement = |vertex| {
        store.with_graph_mut(|graph| {
            graph
                .read_forward_bucket_placement_info(vertex, label)
                .unwrap()
                .unwrap()
        })
    };
    assert_eq!(stats().0.slab_occupied_tail, 0);
    let mut expected = BTreeMap::new();
    let mut insert = |src: VertexId, dst: VertexId, value: u16| {
        let handle = store
            .insert_undirected_edge_with_inline_property_bytes(
                src,
                dst,
                Some(label_id),
                &value.to_le_bytes(),
            )
            .expect("fixture edge");
        // The model records logical inputs, not rows read back from storage.
        for (owner, neighbor) in
            [(src, dst), (dst, src)]
                .into_iter()
                .take(if src == dst { 1 } else { 2 })
        {
            let rows = expected.entry(owner).or_insert_with(Vec::new);
            rows.push((rows.len() as u32, neighbor, value.to_le_bytes()));
        }
        handle
    };
    for i in 0..canonical_prefix {
        insert(high, high, 1000 + i);
    }
    let canonical_bytes = u64::from(canonical_prefix) * 2;
    assert_eq!(stats().0.slab_occupied_tail, canonical_bytes);
    for i in 0..8 {
        insert(low, low, 2000 + i);
    }
    let handle = insert(high, low, 5000);
    assert_eq!(handle.owner_vertex_id, high);
    assert_eq!(handle.slot_index.raw(), u32::from(canonical_prefix));
    assert_eq!(stats().0.slab_occupied_tail, canonical_bytes + 18);
    insert(guard, guard, 6000);
    assert_eq!(stats().0.slab_occupied_tail, canonical_bytes + 20);
    for i in 0..8 {
        insert(high, high, 3000 + i);
        insert(low, low, 4000 + i);
    }

    let read_rows = |vertex| {
        let mut rows: Vec<_> = store
            .undirected_edges(vertex)
            .unwrap()
            .into_iter()
            .map(|edge| {
                assert_eq!(edge.label_id, label.raw());
                (
                    edge.edge_slot_index.raw(),
                    edge.neighbor_vid(),
                    <[u8; 2]>::try_from(edge.edge_inline_property_bytes()).unwrap(),
                )
            })
            .collect();
        rows.sort_unstable_by_key(|row| row.0);
        rows
    };
    for (&vertex, rows) in &expected {
        assert_eq!(&read_rows(vertex), rows);
    }
    let before_high = placement(high);
    let before_low = placement(low);
    let before_guard = placement(guard);
    assert_eq!(before_high.degree, u32::from(canonical_prefix) + 9);
    assert_eq!(
        before_high.inline_property_bytes_slab_slots,
        u32::from(canonical_prefix)
    );
    assert_eq!(
        (
            before_low.degree,
            before_low.inline_property_bytes_slab_slots
        ),
        (17, 9)
    );
    assert_eq!(
        (
            before_guard.degree,
            before_guard.inline_property_bytes_slab_slots
        ),
        (1, 1)
    );
    // Exact rows beyond each slab prefix witness nonempty value runs; retired log fields do not.
    assert!(before_high.degree > before_high.inline_property_bytes_slab_slots);
    assert!(before_low.degree > before_low.inline_property_bytes_slab_slots);
    let (before, reverse_before) = stats();
    assert_eq!(
        before.free_bytes, 0,
        "no earlier retired destination may mask reuse"
    );
    assert_eq!(before.slab_occupied_tail, canonical_bytes + 20);
    assert_eq!(before.allocated_bytes, before.slab_occupied_tail);
    let counterpart_bytes = 34;
    let canonical_request = u64::from(before_high.degree) * 2;
    assert_eq!(before.live_bytes, canonical_request + counterpart_bytes + 2);
    let mut prefix = vec![0; canonical_bytes as usize];
    store.with_graph_mut(|graph| graph.forward().values().read_bytes(0, &mut prefix));
    let original_prefix: Vec<_> = (0..canonical_prefix)
        .flat_map(|i| (1000 + i).to_le_bytes())
        .collect();
    assert_eq!(
        prefix, original_prefix,
        "known canonical source starts at byte zero"
    );

    let note = crate::test_labels::property_id_for_name("reuse_note");
    let unrelated = EdgeHandle::at_slot(high, label, 0);
    for (edge, text) in [(handle, "target"), (unrelated, "unrelated")] {
        store
            .set_edge_property(
                edge.occurrence(LabeledOrientation::Forward),
                note,
                Value::Text(text.into()),
            )
            .unwrap();
    }
    assert_eq!(
        stats(),
        (before, reverse_before),
        "sidecars do not change value storage"
    );
    let update_handle = black_box(handle);
    let replacement = black_box([8, 0]);
    let update = || store.update_edge_inline_property_at_handle(update_handle, &replacement);
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    let (measurement, outcome) = {
        let mut outcome = None;
        // Only the API call and retaining its Result are timed, not its success assertion.
        let measurement = canbench_rs::bench_fn(|| outcome = Some(update()));
        (measurement, outcome.expect("one update invocation"))
    };
    #[cfg(not(all(feature = "canbench", target_family = "wasm")))]
    let (measurement, outcome) = ((), update());
    outcome.expect("paired update");

    // Value-only folding must not move topology/slots or alter the guard's descriptor projection.
    let mut expected_high = before_high;
    expected_high.inline_property_bytes_slab_slots = before_high.degree;
    let mut expected_low = before_low;
    expected_low.inline_property_bytes_slab_slots = before_low.degree;
    assert_eq!(placement(high), expected_high, "canonical values must fold");
    assert_eq!(placement(low), expected_low, "counterpart values must fold");
    assert_eq!(placement(guard), before_guard);
    expected.get_mut(&high).unwrap()[canonical_prefix as usize].2 = [8, 0];
    expected.get_mut(&low).unwrap()[8].2 = [8, 0];
    for (&vertex, rows) in &expected {
        assert_eq!(&read_rows(vertex), rows, "all rows at {vertex:?}");
    }
    for (edge, text) in [(handle, "target"), (unrelated, "unrelated")] {
        assert_eq!(
            store
                .edge_property(edge.occurrence(LabeledOrientation::Forward), note)
                .unwrap(),
            Some(Value::Text(text.into()))
        );
    }
    let counterpart = EdgeHandle::at_slot(low, label, 8);
    assert_eq!(
        store
            .edge_property(counterpart.occurrence(LabeledOrientation::Forward), note)
            .unwrap(),
        Some(Value::Text("target".into()))
    );

    let (after, reverse_after) = stats();
    let fits = canonical_bytes >= counterpart_bytes;
    let consumed = if fits { counterpart_bytes } else { 0 };
    assert_eq!(
        after.slab_occupied_tail,
        before.slab_occupied_tail + canonical_request + counterpart_bytes - consumed
    );
    assert_eq!(after.free_bytes, canonical_bytes + 18 - consumed);
    assert_eq!(after.largest_free_span, after.free_bytes);
    assert_eq!(after.free_span_count, 1);
    assert_eq!(after.live_bytes, before.live_bytes);
    assert_eq!(after.allocated_bytes, after.live_bytes);
    assert_eq!(
        after.slab_occupied_tail,
        after.allocated_bytes + after.free_bytes
    );
    assert_eq!(after.byte_capacity, before.byte_capacity);
    assert_eq!(reverse_after, reverse_before);
    // Initial free=0 makes canonical retirement the only reusable destination. Check complete
    // payloads at all three disjoint occupied regions, not just aggregate free accounting.
    let counterpart_start = if fits {
        0
    } else {
        before.slab_occupied_tail + canonical_request
    };
    for (vertex, start) in [
        (high, before.slab_occupied_tail),
        (low, counterpart_start),
        (guard, canonical_bytes + 18),
    ] {
        let wanted: Vec<_> = expected[&vertex].iter().flat_map(|row| row.2).collect();
        let mut actual = vec![0; wanted.len()];
        store.with_graph_mut(|graph| graph.forward().values().read_bytes(start, &mut actual));
        assert_eq!(actual, wanted, "payload at byte {start}");
    }
    let summary = format!(
        "UNDIRECTED_REUSE prefix={canonical_prefix} fit={fits} before_tail={} after_tail={} tail_delta={} before_free={} free={} consumed={} before_allocated={} allocated={} live={} p_degree={} c_degree={}",
        before.slab_occupied_tail,
        after.slab_occupied_tail,
        after.slab_occupied_tail - before.slab_occupied_tail,
        before.free_bytes,
        after.free_bytes,
        canonical_bytes + 18 + before.free_bytes - after.free_bytes,
        before.allocated_bytes,
        after.allocated_bytes,
        after.live_bytes,
        before_high.degree,
        before_low.degree,
    );
    #[cfg(all(feature = "canbench", target_family = "wasm"))]
    {
        assert_eq!(measurement.total.calls, 1);
        ic_cdk::println!(
            "{summary} instructions={} calls={} heap_pages={} stable_pages={}",
            measurement.total.instructions,
            measurement.total.calls,
            measurement.total.heap_increase,
            measurement.total.stable_memory_increase,
        );
    }
    #[cfg(not(all(feature = "canbench", target_family = "wasm")))]
    println!("{summary}");
    measurement
}

#[cfg(all(feature = "canbench", target_family = "wasm"))]
#[canbench_rs::bench(raw)]
fn bench_graph_undirected_inline_update_fit_w2() -> canbench_rs::BenchResult {
    check_undirected_inline_update_reuse(32)
}

#[cfg(all(feature = "canbench", target_family = "wasm"))]
#[canbench_rs::bench(raw)]
fn bench_graph_undirected_inline_update_no_fit_w2() -> canbench_rs::BenchResult {
    check_undirected_inline_update_reuse(8)
}
