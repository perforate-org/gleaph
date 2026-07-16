//! Capacity-slope benchmarks for the LARA-backed graph layout.
//!
//! These benchmarks build a fresh fixture inside the measured call.  The
//! `stable_memory_increase` result is therefore the physical allocation for
//! the fixture, including extent bucket slack and LARA metadata.  They are
//! intentionally small enough for a local canbench loop; capacity estimates
//! extrapolate the measured slope rather than pretending that a 500 GiB
//! fixture is practical to construct in one call.

use super::insert_bench_vertex_named;
use crate::facade::GraphStore;
use canbench_rs::bench;
use gleaph_gql::Value;
use gleaph_graph_kernel::entry::Vertex;
use std::hint::black_box;

const VERTEX_COUNT: u32 = 32_768;
const EDGE_VERTEX_COUNT: u32 = 1_024;
const EDGE_COUNT: u32 = 2_048;
const HUB_VERTEX_COUNT: u32 = 1_025;
const HUB_EDGE_COUNT: u32 = 512;
const CHURN_VERTEX_COUNT: u32 = 257;
const CHURN_EDGE_COUNT: u32 = 256;
const PROPERTY_VERTEX_COUNT: u32 = 1_024;
const PROPERTY_COUNT: u32 = 4;
const TEXT_PROPERTY_VERTEX_COUNT: u32 = 256;
const TEXT_PROPERTY_COUNT: u32 = 4;

fn build_vertices(store: &GraphStore, count: u32) -> u32 {
    for _ in 0..count {
        store
            .push_unplaced_vertex_row(Vertex::default())
            .expect("vertex row");
    }
    count
}

fn build_edge_vertices(store: &GraphStore, vertex_count: u32) -> Vec<ic_stable_lara::VertexId> {
    (0..vertex_count)
        .map(|_| insert_bench_vertex_named(store, &[]))
        .collect()
}

fn build_edges(store: &GraphStore, vertices: &[ic_stable_lara::VertexId], edge_count: u32) -> u32 {
    for edge in 0..edge_count {
        let source = vertices[(edge as usize) % vertices.len()];
        let target = vertices[((edge as usize) + 1) % vertices.len()];
        store
            .insert_directed_edge_named(
                source,
                target,
                Some("BenchCapacityEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("edge");
    }
    edge_count
}

fn build_hub_edges(
    store: &GraphStore,
    vertices: &[ic_stable_lara::VertexId],
    edge_count: u32,
) -> u32 {
    let hub = vertices[0];
    for target in vertices.iter().copied().skip(1).take(edge_count as usize) {
        store
            .insert_directed_edge_named(
                hub,
                target,
                Some("BenchCapacityHubEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("hub edge");
    }
    edge_count
}

fn build_delete_reinsert_churn(
    store: &GraphStore,
    vertices: &[ic_stable_lara::VertexId],
    edge_count: u32,
) -> u32 {
    let mut handles = Vec::with_capacity(edge_count as usize);
    for edge in 0..edge_count {
        let handle = store
            .insert_directed_edge_named(
                vertices[edge as usize],
                vertices[(edge as usize) + 1],
                Some("BenchCapacityChurnEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("churn edge");
        handles.push(handle);
    }

    for handle in handles.iter().step_by(2).copied() {
        store
            .delete_edge_by_handle(handle)
            .expect("delete churn edge");
    }

    for edge in (0..edge_count).step_by(2) {
        store
            .insert_directed_edge_named(
                vertices[edge as usize],
                vertices[(edge as usize) + 1],
                Some("BenchCapacityChurnEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("reinsert churn edge");
    }
    edge_count * 2
}

fn build_property_vertices(store: &GraphStore, vertex_count: u32) -> Vec<ic_stable_lara::VertexId> {
    (0..vertex_count)
        .map(|_| insert_bench_vertex_named(store, &[]))
        .collect()
}

fn build_properties(
    store: &GraphStore,
    vertices: &[ic_stable_lara::VertexId],
    properties_per_vertex: u32,
) -> u32 {
    build_properties_with_value(
        store,
        vertices,
        properties_per_vertex,
        |vertex, property| Value::Int64((vertex + property) as i64),
    )
}

fn build_properties_with_value(
    store: &GraphStore,
    vertices: &[ic_stable_lara::VertexId],
    properties_per_vertex: u32,
    value_for: impl Fn(usize, usize) -> Value,
) -> u32 {
    let property_ids = (0..properties_per_vertex)
        .map(|property_index| {
            crate::test_labels::property_id_for_name(&format!(
                "BenchCapacityProperty{property_index}"
            ))
        })
        .collect::<Vec<_>>();

    for (vertex_index, vertex) in vertices.iter().copied().enumerate() {
        for (property_index, property_id) in property_ids.iter().copied().enumerate() {
            store
                .set_vertex_property_without_index_pending(
                    vertex,
                    property_id,
                    value_for(vertex_index, property_index),
                )
                .expect("vertex property");
        }
    }
    vertices.len() as u32 * properties_per_vertex
}

#[bench(raw)]
fn bench_capacity_vertices_32768() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("capacity_vertices_32768");
        black_box(build_vertices(&GraphStore::new(), VERTEX_COUNT));
    })
}

#[bench(raw)]
fn bench_capacity_edges_1024_vertices_2048_edges() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let vertices = {
            let _scope = canbench_rs::bench_scope("capacity_edges_vertices_1024");
            build_edge_vertices(&store, EDGE_VERTEX_COUNT)
        };
        let _scope = canbench_rs::bench_scope("capacity_edges_2048");
        black_box(build_edges(&store, &vertices, EDGE_COUNT));
    })
}

#[bench(raw)]
fn bench_capacity_edges_hub_512() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let vertices = {
            let _scope = canbench_rs::bench_scope("capacity_hub_vertices_1025");
            build_edge_vertices(&store, HUB_VERTEX_COUNT)
        };
        let _scope = canbench_rs::bench_scope("capacity_hub_edges_512");
        black_box(build_hub_edges(&store, &vertices, HUB_EDGE_COUNT));
    })
}

#[bench(raw)]
fn bench_capacity_edges_delete_reinsert_256() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let vertices = {
            let _scope = canbench_rs::bench_scope("capacity_churn_vertices_257");
            build_edge_vertices(&store, CHURN_VERTEX_COUNT)
        };
        let _scope = canbench_rs::bench_scope("capacity_churn_insert_delete_reinsert_256");
        black_box(build_delete_reinsert_churn(
            &store,
            &vertices,
            CHURN_EDGE_COUNT,
        ));
    })
}

#[bench(raw)]
fn bench_capacity_properties_1024_vertices_4_each() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let vertices = {
            let _scope = canbench_rs::bench_scope("capacity_properties_vertices_1024");
            build_property_vertices(&store, PROPERTY_VERTEX_COUNT)
        };
        let _scope = canbench_rs::bench_scope("capacity_properties_4096");
        black_box(build_properties(&store, &vertices, PROPERTY_COUNT));
    })
}

#[bench(raw)]
fn bench_capacity_properties_text_256_vertices_4_each() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let vertices = {
            let _scope = canbench_rs::bench_scope("capacity_text_properties_vertices_256");
            build_property_vertices(&store, TEXT_PROPERTY_VERTEX_COUNT)
        };
        let _scope = canbench_rs::bench_scope("capacity_text_properties_1024");
        black_box(build_properties_with_value(
            &store,
            &vertices,
            TEXT_PROPERTY_COUNT,
            |vertex, property| {
                Value::Text(format!("value-{vertex:04}-{property:02}-{}", "x".repeat(8)))
            },
        ));
    })
}

#[bench(raw)]
fn bench_capacity_properties_large_text_256_vertices_4_each() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let store = GraphStore::new();
        let vertices = {
            let _scope = canbench_rs::bench_scope("capacity_large_text_vertices_256");
            build_property_vertices(&store, TEXT_PROPERTY_VERTEX_COUNT)
        };
        let _scope = canbench_rs::bench_scope("capacity_large_text_properties_1024");
        black_box(build_properties_with_value(
            &store,
            &vertices,
            TEXT_PROPERTY_COUNT,
            |vertex, property| {
                Value::Text(format!(
                    "value-{vertex:04}-{property:02}-{}",
                    "x".repeat(256)
                ))
            },
        ));
    })
}
