//! Larger query-call `canbench` targets.
//!
//! These are intentionally behind `canbench_large` because graph construction is heavier than the
//! default micro/signal benches. The measured closure still executes only the read plan, matching a
//! query call after data has already been written.

use super::*;
use canbench_rs::bench;

const FEED_POSTS: u32 = 120_000;
const FEED_OFFSET: i64 = 10_000;
const FEED_LIMIT: i64 = 100;

const FOF_FIRST_HOP: u32 = 256;
const FOF_SECOND_HOP: u32 = 64;

const ROAD_GRID_SIDE: u32 = 64;
const ROAD_GRID_MAX_HOPS: u64 = (ROAD_GRID_SIDE as u64 - 1) * 2;

const VECTOR_SCAN_MEDIUM: u32 = 128;
const VECTOR_PASS_MEDIUM: u32 = 16;
const VECTOR_SCAN_LARGE: u32 = 512;
const VECTOR_PASS_LARGE: u32 = 64;
const VECTOR_SCAN_XLARGE: u32 = 4_096;
const VECTOR_PASS_XLARGE: u32 = 512;
const VECTOR_SCAN_XXLARGE: u32 = 16_384;
const VECTOR_PASS_XXLARGE: u32 = 2_048;
const VECTOR_EDGES_PER_HUB: u32 = 32;

fn lit_i64(value: i64) -> Expr {
    Expr::new(ExprKind::Literal(Value::Int64(value)))
}

fn setup_large_feed_graph(store: &GraphStore) {
    let account = store
        .insert_vertex_named(["BenchLargeFeedAccount"], Vec::<(&str, Value)>::new())
        .expect("account");

    for i in 0..FEED_POSTS {
        let post = store
            .insert_vertex_named(
                ["BenchLargeFeedPost"],
                [
                    ("bucket", Value::Int64((i % 128) as i64)),
                    ("created_at", Value::Int64(i as i64)),
                ],
            )
            .expect("post");
        store
            .insert_directed_edge_named(
                account,
                post,
                Some("BenchLargeFeedEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("feed edge");
    }
}

fn large_feed_page_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "account".into(),
            label: Some("BenchLargeFeedAccount".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "account".into(),
            edge: "e".into(),
            dst: "post".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchLargeFeedEdge".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(var("post"), "post")],
            distinct: false,
        },
        PlanOp::Limit {
            count: Some(lit_i64(FEED_LIMIT)),
            offset: Some(lit_i64(FEED_OFFSET)),
        },
    ])
}

/// Activity-feed style page over a 120k-edge fanout. The important signal is whether a realistic
/// page request stays far below the 5B instruction query-call budget by stopping after offset+limit.
#[bench(raw)]
fn bench_graph_large_feed_page_120k_offset10k_limit100() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_large_feed_graph(&store);
    let plan = large_feed_page_plan();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("large_feed_page_120k_offset10k_limit100");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), FEED_LIMIT as usize);
        black_box(result.rows.len())
    })
}

fn setup_large_friends_of_friends_graph(store: &GraphStore) {
    build_friends_of_friends_graph(store, FOF_FIRST_HOP, FOF_SECOND_HOP);
}

fn build_friends_of_friends_graph(store: &GraphStore, first_hop: u32, second_hop: u32) {
    let root = store
        .insert_vertex_named(["BenchLargeRootUser"], Vec::<(&str, Value)>::new())
        .expect("root");

    let mut candidates = Vec::with_capacity((first_hop * second_hop) as usize);
    for i in 0..first_hop * second_hop {
        candidates.push(
            store
                .insert_vertex_named(
                    ["BenchLargeCandidateUser"],
                    [("rank", Value::Int64(i as i64))],
                )
                .expect("candidate"),
        );
    }

    for friend_idx in 0..first_hop {
        let friend = store
            .insert_vertex_named(["BenchLargeFriendUser"], Vec::<(&str, Value)>::new())
            .expect("friend");
        store
            .insert_directed_edge_named(
                root,
                friend,
                Some("BenchLargeFollows"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("root follows friend");

        for second_idx in 0..second_hop {
            let candidate_idx = (friend_idx * second_hop + second_idx) as usize;
            store
                .insert_directed_edge_named(
                    friend,
                    candidates[candidate_idx],
                    Some("BenchLargeFollows"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("friend follows candidate");
        }
    }
}

fn large_friends_of_friends_plan() -> PhysicalPlan {
    plan(vec![
        PlanOp::NodeScan {
            variable: "root".into(),
            label: Some("BenchLargeRootUser".into()),
            property_projection: None,
        },
        PlanOp::Expand {
            src: "root".into(),
            edge: "e1".into(),
            dst: "friend".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchLargeFollows".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Expand {
            src: "friend".into(),
            edge: "e2".into(),
            dst: "candidate".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("BenchLargeFollows".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_payload_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        },
        PlanOp::Project {
            columns: vec![project(var("candidate"), "candidate")],
            distinct: false,
        },
    ])
}

/// Friends-of-friends recommendation shape: 256 first-hop users, each with 64 second-hop candidates.
/// This keeps the result cardinality large enough to expose row-growth costs without measuring setup.
#[bench(raw)]
fn bench_graph_large_friends_of_friends_256x64() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_large_friends_of_friends_graph(&store);
    let plan = large_friends_of_friends_plan();
    let expected = (FOF_FIRST_HOP * FOF_SECOND_HOP) as usize;

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("large_friends_of_friends_256x64");
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), expected);
        black_box(result.rows.len())
    })
}

fn setup_large_road_grid_graph(store: &GraphStore) {
    let label_id = crate::test_labels::edge_label_id_for_name("BenchLargeRoad");
    crate::test_labels::install_test_edge_payload_profile(
        label_id,
        gleaph_graph_kernel::entry::EdgePayloadProfile::from(EdgeWeightProfile {
            encoding: WeightEncoding::RawU16,
        }),
    );
    let road = catalog_edge_label("BenchLargeRoad");

    let mut vertices = Vec::with_capacity((ROAD_GRID_SIDE * ROAD_GRID_SIDE) as usize);
    for y in 0..ROAD_GRID_SIDE {
        for x in 0..ROAD_GRID_SIDE {
            let labels = match (x, y) {
                (0, 0) => vec!["BenchLargeRoadSource"],
                (x, y) if x == ROAD_GRID_SIDE - 1 && y == ROAD_GRID_SIDE - 1 => {
                    vec!["BenchLargeRoadTarget"]
                }
                _ => vec!["BenchLargeRoadJunction"],
            };
            vertices.push(
                store
                    .insert_vertex_named(labels, Vec::<(&str, Value)>::new())
                    .expect("road junction"),
            );
        }
    }

    let idx = |x: u32, y: u32| -> usize { (y * ROAD_GRID_SIDE + x) as usize };
    for y in 0..ROAD_GRID_SIDE {
        for x in 0..ROAD_GRID_SIDE {
            let from = vertices[idx(x, y)];
            if x + 1 < ROAD_GRID_SIDE {
                store
                    .insert_directed_edge_with_payload_bytes(
                        from,
                        vertices[idx(x + 1, y)],
                        Some(road),
                        &1u16.to_le_bytes(),
                    )
                    .expect("east road");
            }
            if y + 1 < ROAD_GRID_SIDE {
                store
                    .insert_directed_edge_with_payload_bytes(
                        from,
                        vertices[idx(x, y + 1)],
                        Some(road),
                        &1u16.to_le_bytes(),
                    )
                    .expect("south road");
            }
        }
    }
}

fn large_road_grid_weighted_shortest_plan() -> PhysicalPlan {
    weighted_shortest_plan(
        "BenchLargeRoadSource",
        "BenchLargeRoadTarget",
        "BenchLargeRoad",
        gleaph_weight_call("e"),
        ROAD_GRID_MAX_HOPS,
    )
}

/// Road-network style weighted shortest path over a 64x64 directed grid. This stresses Dijkstra
/// frontier work with inline edge weights while still representing a bounded point-to-point query.
#[bench(raw)]
fn bench_graph_large_weighted_road_grid_64x64() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_large_road_grid_graph(&store);
    let plan = large_road_grid_weighted_shortest_plan();

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("large_weighted_road_grid_64x64");
        let result = execute_shortest_plan(black_box(&store), black_box(&plan));
        assert_eq!(
            result.rows.len(),
            1,
            "large road-grid shortest path should find one route"
        );
        black_box(result.rows.len())
    })
}

fn large_expand_vector_bench(
    hub_label: &str,
    edge_label: &str,
    total: u32,
    pass: u32,
    metric: EdgeVectorMetric,
    op: CmpOp,
    threshold: f32,
    query: &[f32],
    scope: &'static str,
) -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_vector_graph_with_scale(
        &store,
        ExpandVectorGraphScale {
            hub_label,
            edge_label,
            total,
            pass,
            dims: EXPAND_VECTOR_DIMS,
            edges_per_hub: VECTOR_EDGES_PER_HUB,
        },
    );
    let plan = expand_vector_plan(hub_label, edge_label, metric, op, threshold, query);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let result = execute_expand_plan(black_box(&store), black_box(&plan));
        assert_eq!(result.rows.len(), pass as usize);
        black_box(result.rows.len())
    })
}

fn large_expand_vector_bindings_bench(
    hub_label: &str,
    edge_label: &str,
    total: u32,
    pass: u32,
    metric: EdgeVectorMetric,
    op: CmpOp,
    threshold: f32,
    query: &[f32],
    scope: &'static str,
) -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    setup_expand_vector_graph_with_scale(
        &store,
        ExpandVectorGraphScale {
            hub_label,
            edge_label,
            total,
            pass,
            dims: EXPAND_VECTOR_DIMS,
            edges_per_hub: VECTOR_EDGES_PER_HUB,
        },
    );
    let plan = expand_vector_bindings_plan(hub_label, edge_label, metric, op, threshold, query);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope(scope);
        let row_count = execute_expand_bindings(black_box(&store), black_box(&plan));
        assert_eq!(row_count, pass as usize);
        black_box(row_count)
    })
}

/// Fixed-label vector edge payloads over a medium fanout; L2 threshold selects 16 rows.
#[bench(raw)]
fn bench_graph_large_expand_vector_l2_128scan_16match() -> canbench_rs::BenchResult {
    let query = vec![1.0; EXPAND_VECTOR_DIMS];
    large_expand_vector_bench(
        "BenchLargeVectorHubL2Medium",
        "BenchLargeVectorEdgeL2Medium",
        VECTOR_SCAN_MEDIUM,
        VECTOR_PASS_MEDIUM,
        EdgeVectorMetric::L2Squared,
        CmpOp::Le,
        4.0,
        &query,
        "large_expand_vector_l2_128scan_16match",
    )
}

/// Fixed-label vector edge payloads over a medium fanout; DOT threshold selects 16 rows.
#[bench(raw)]
fn bench_graph_large_expand_vector_dot_128scan_16match() -> canbench_rs::BenchResult {
    let query = vec![-1.0; EXPAND_VECTOR_DIMS];
    let threshold = -(EXPAND_VECTOR_DIMS as f32) - 4.0;
    large_expand_vector_bench(
        "BenchLargeVectorHubDotMedium",
        "BenchLargeVectorEdgeDotMedium",
        VECTOR_SCAN_MEDIUM,
        VECTOR_PASS_MEDIUM,
        EdgeVectorMetric::Dot,
        CmpOp::Ge,
        threshold,
        &query,
        "large_expand_vector_dot_128scan_16match",
    )
}

/// Fixed-label vector edge payloads over a larger fanout; L2 threshold selects 64 rows.
#[bench(raw)]
fn bench_graph_large_expand_vector_l2_512scan_64match() -> canbench_rs::BenchResult {
    let query = vec![1.0; EXPAND_VECTOR_DIMS];
    large_expand_vector_bench(
        "BenchLargeVectorHubL2Large",
        "BenchLargeVectorEdgeL2Large",
        VECTOR_SCAN_LARGE,
        VECTOR_PASS_LARGE,
        EdgeVectorMetric::L2Squared,
        CmpOp::Le,
        4.0,
        &query,
        "large_expand_vector_l2_512scan_64match",
    )
}

/// Fixed-label vector edge payloads over a larger fanout; DOT threshold selects 64 rows.
#[bench(raw)]
fn bench_graph_large_expand_vector_dot_512scan_64match() -> canbench_rs::BenchResult {
    let query = vec![-1.0; EXPAND_VECTOR_DIMS];
    let threshold = -(EXPAND_VECTOR_DIMS as f32) - 4.0;
    large_expand_vector_bench(
        "BenchLargeVectorHubDotLarge",
        "BenchLargeVectorEdgeDotLarge",
        VECTOR_SCAN_LARGE,
        VECTOR_PASS_LARGE,
        EdgeVectorMetric::Dot,
        CmpOp::Ge,
        threshold,
        &query,
        "large_expand_vector_dot_512scan_64match",
    )
}

/// Fixed-label vector edge payloads over an xlarge logical scan; L2 threshold selects 512 rows.
#[bench(raw)]
fn bench_graph_large_expand_vector_l2_4096scan_512match() -> canbench_rs::BenchResult {
    let query = vec![1.0; EXPAND_VECTOR_DIMS];
    large_expand_vector_bench(
        "BenchLargeVectorHubL2XLarge",
        "BenchLargeVectorEdgeL2XLarge",
        VECTOR_SCAN_XLARGE,
        VECTOR_PASS_XLARGE,
        EdgeVectorMetric::L2Squared,
        CmpOp::Le,
        4.0,
        &query,
        "large_expand_vector_l2_4096scan_512match",
    )
}

/// Fixed-label vector edge payloads over an xlarge logical scan; DOT threshold selects 512 rows.
#[bench(raw)]
fn bench_graph_large_expand_vector_dot_4096scan_512match() -> canbench_rs::BenchResult {
    let query = vec![-1.0; EXPAND_VECTOR_DIMS];
    let threshold = -(EXPAND_VECTOR_DIMS as f32) - 4.0;
    large_expand_vector_bench(
        "BenchLargeVectorHubDotXLarge",
        "BenchLargeVectorEdgeDotXLarge",
        VECTOR_SCAN_XLARGE,
        VECTOR_PASS_XLARGE,
        EdgeVectorMetric::Dot,
        CmpOp::Ge,
        threshold,
        &query,
        "large_expand_vector_dot_4096scan_512match",
    )
}

/// Fixed-label vector edge payloads over an xxlarge logical scan; L2 threshold selects 2,048 rows.
#[bench(raw)]
fn bench_graph_large_expand_vector_l2_16384scan_2048match() -> canbench_rs::BenchResult {
    let query = vec![1.0; EXPAND_VECTOR_DIMS];
    large_expand_vector_bench(
        "BenchLargeVectorHubL2XXLarge",
        "BenchLargeVectorEdgeL2XXLarge",
        VECTOR_SCAN_XXLARGE,
        VECTOR_PASS_XXLARGE,
        EdgeVectorMetric::L2Squared,
        CmpOp::Le,
        4.0,
        &query,
        "large_expand_vector_l2_16384scan_2048match",
    )
}

/// Fixed-label vector edge payloads over an xxlarge logical scan; DOT threshold selects 2,048 rows.
#[bench(raw)]
fn bench_graph_large_expand_vector_dot_16384scan_2048match() -> canbench_rs::BenchResult {
    let query = vec![-1.0; EXPAND_VECTOR_DIMS];
    let threshold = -(EXPAND_VECTOR_DIMS as f32) - 4.0;
    large_expand_vector_bench(
        "BenchLargeVectorHubDotXXLarge",
        "BenchLargeVectorEdgeDotXXLarge",
        VECTOR_SCAN_XXLARGE,
        VECTOR_PASS_XXLARGE,
        EdgeVectorMetric::Dot,
        CmpOp::Ge,
        threshold,
        &query,
        "large_expand_vector_dot_16384scan_2048match",
    )
}

/// Scan/predicate-only fixed-label vector L2 over 16,384 rows; no edge passes.
#[bench(raw)]
fn bench_graph_large_expand_vector_bindings_l2_16384scan_0match() -> canbench_rs::BenchResult {
    let query = vec![1.0; EXPAND_VECTOR_DIMS];
    large_expand_vector_bindings_bench(
        "BenchLargeVectorHubL2BindingsZero",
        "BenchLargeVectorEdgeL2BindingsZero",
        VECTOR_SCAN_XXLARGE,
        0,
        EdgeVectorMetric::L2Squared,
        CmpOp::Le,
        4.0,
        &query,
        "large_expand_vector_bindings_l2_16384scan_0match",
    )
}

/// Scan/predicate-only fixed-label vector L2 over 16,384 rows; 128 rows pass.
#[bench(raw)]
fn bench_graph_large_expand_vector_bindings_l2_16384scan_128match() -> canbench_rs::BenchResult {
    let query = vec![1.0; EXPAND_VECTOR_DIMS];
    large_expand_vector_bindings_bench(
        "BenchLargeVectorHubL2BindingsSparse",
        "BenchLargeVectorEdgeL2BindingsSparse",
        VECTOR_SCAN_XXLARGE,
        128,
        EdgeVectorMetric::L2Squared,
        CmpOp::Le,
        4.0,
        &query,
        "large_expand_vector_bindings_l2_16384scan_128match",
    )
}

/// Scan/predicate-only fixed-label vector L2 over 16,384 rows; 2,048 rows pass.
#[bench(raw)]
fn bench_graph_large_expand_vector_bindings_l2_16384scan_2048match() -> canbench_rs::BenchResult {
    let query = vec![1.0; EXPAND_VECTOR_DIMS];
    large_expand_vector_bindings_bench(
        "BenchLargeVectorHubL2BindingsCurrent",
        "BenchLargeVectorEdgeL2BindingsCurrent",
        VECTOR_SCAN_XXLARGE,
        VECTOR_PASS_XXLARGE,
        EdgeVectorMetric::L2Squared,
        CmpOp::Le,
        4.0,
        &query,
        "large_expand_vector_bindings_l2_16384scan_2048match",
    )
}

/// 9_500 noise + 500 matching payload edges; edge payload `Eq` predicate expand.
#[bench(raw)]
fn bench_graph_large_expand_payload_skewed_10k_a_500b() -> canbench_rs::BenchResult {
    bench_expand_payload_skewed(
        EXPAND_SKEW_NOISE_L,
        EXPAND_HUB_OUT_L,
        "large_expand_payload_skewed_10k_a_500b",
    )
}

/// 49_000 noise + 1_000 matching payload edges; edge payload `Eq` predicate expand.
#[bench(raw)]
fn bench_graph_large_expand_payload_skewed_50k_a_1k_b() -> canbench_rs::BenchResult {
    bench_expand_payload_skewed(
        EXPAND_SKEW_NOISE_XL,
        EXPAND_HUB_OUT_XL,
        "large_expand_payload_skewed_50k_a_1k_b",
    )
}

const DELETE_SUPERNODE_IN_L: u32 = 500;
const DELETE_SUPERNODE_IN_XL: u32 = 1_000;

/// Super-node detach-delete: hub fed by 500 payload-free in-edges. Measures the
/// full synchronous reverse-adjacency purge under one delete call (ADR 0021).
#[bench(raw)]
fn bench_graph_large_detach_delete_supernode_500in() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let hub = setup_delete_hub_in_edges(&store, DELETE_SUPERNODE_IN_L);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("large_detach_delete_supernode_500in");
        store
            .detach_delete_vertex(black_box(hub))
            .expect("detach delete supernode");
        assert!(!store.is_vertex_live(hub), "hub tombstoned after detach");
    })
}

/// Super-node detach-delete: hub fed by 1,000 payload-free in-edges (ADR 0021).
#[bench(raw)]
fn bench_graph_large_detach_delete_supernode_1k_in() -> canbench_rs::BenchResult {
    let store = GraphStore::new();
    let hub = setup_delete_hub_in_edges(&store, DELETE_SUPERNODE_IN_XL);

    canbench_rs::bench_fn(|| {
        let _scope = canbench_rs::bench_scope("large_detach_delete_supernode_1k_in");
        store
            .detach_delete_vertex(black_box(hub))
            .expect("detach delete supernode");
        assert!(!store.is_vertex_live(hub), "hub tombstoned after detach");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detach_delete_supernode_in_edges_setup_and_purge() {
        let store = GraphStore::new();
        let hub = setup_delete_hub_in_edges(&store, 32);
        store.detach_delete_vertex(hub).expect("detach delete");
        assert!(!store.is_vertex_live(hub));
        assert!(!store.vertex_is_pending_purge(hub));
    }

    #[test]
    fn large_feed_page_setup_and_execute() {
        let store = GraphStore::new();
        setup_large_feed_graph(&store);
        let result = execute_expand_plan(&store, &large_feed_page_plan());
        assert_eq!(result.rows.len(), FEED_LIMIT as usize);
    }

    // Validates the friends-of-friends setup + two-hop expand at a reduced scale.
    // The full bench scale (256x64) is exercised by the canbench bench; running it
    // here wedges the native test-only unlimited post-insert compaction
    // (`CollectAllocationOverflow` from tightly packed PMA leaves). Production wasm
    // uses the bounded maintenance budget and is unaffected.
    const FOF_TEST_FIRST_HOP: u32 = 16;
    const FOF_TEST_SECOND_HOP: u32 = 8;

    #[test]
    fn large_friends_of_friends_setup_and_execute() {
        let store = GraphStore::new();
        build_friends_of_friends_graph(&store, FOF_TEST_FIRST_HOP, FOF_TEST_SECOND_HOP);
        let result = execute_expand_plan(&store, &large_friends_of_friends_plan());
        assert_eq!(
            result.rows.len(),
            (FOF_TEST_FIRST_HOP * FOF_TEST_SECOND_HOP) as usize
        );
    }

    #[test]
    fn large_road_grid_setup_and_execute() {
        let store = GraphStore::new();
        setup_large_road_grid_graph(&store);
        let result = execute_shortest_plan(&store, &large_road_grid_weighted_shortest_plan());
        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn large_vector_setup_and_execute() {
        let store = GraphStore::new();
        setup_expand_vector_graph_with_scale(
            &store,
            ExpandVectorGraphScale {
                hub_label: "BenchLargeVectorHubTest",
                edge_label: "BenchLargeVectorEdgeTest",
                total: VECTOR_SCAN_MEDIUM,
                pass: VECTOR_PASS_MEDIUM,
                dims: EXPAND_VECTOR_DIMS,
                edges_per_hub: VECTOR_EDGES_PER_HUB,
            },
        );
        let query = vec![1.0; EXPAND_VECTOR_DIMS];
        let plan = expand_vector_plan(
            "BenchLargeVectorHubTest",
            "BenchLargeVectorEdgeTest",
            EdgeVectorMetric::L2Squared,
            CmpOp::Le,
            4.0,
            &query,
        );
        let result = execute_expand_plan(&store, &plan);
        assert_eq!(result.rows.len(), VECTOR_PASS_MEDIUM as usize);
    }
}
