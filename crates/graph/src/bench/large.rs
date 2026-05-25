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
            edge_value_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
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
    let root = store
        .insert_vertex_named(["BenchLargeRootUser"], Vec::<(&str, Value)>::new())
        .expect("root");

    let mut candidates = Vec::with_capacity((FOF_FIRST_HOP * FOF_SECOND_HOP) as usize);
    for i in 0..FOF_FIRST_HOP * FOF_SECOND_HOP {
        candidates.push(
            store
                .insert_vertex_named(
                    ["BenchLargeCandidateUser"],
                    [("rank", Value::Int64(i as i64))],
                )
                .expect("candidate"),
        );
    }

    for friend_idx in 0..FOF_FIRST_HOP {
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

        for second_idx in 0..FOF_SECOND_HOP {
            let candidate_idx = (friend_idx * FOF_SECOND_HOP + second_idx) as usize;
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
            edge_value_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
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
            edge_value_predicate: None,
            edge_vector_predicate: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: false,
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
    let label_id = store
        .get_or_insert_edge_label_id("BenchLargeRoad")
        .expect("road label");
    store
        .install_edge_label_weight_profile_at_init(
            label_id,
            EdgeWeightProfile {
                encoding: WeightEncoding::RawU16,
            },
        )
        .expect("weight profile");
    let road = catalog_edge_label(store, "BenchLargeRoad");

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
                    .insert_directed_edge_with_inline_value(
                        from,
                        vertices[idx(x + 1, y)],
                        Some(road),
                        1,
                    )
                    .expect("east road");
            }
            if y + 1 < ROAD_GRID_SIDE {
                store
                    .insert_directed_edge_with_inline_value(
                        from,
                        vertices[idx(x, y + 1)],
                        Some(road),
                        1,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn large_feed_page_setup_and_execute() {
        let store = GraphStore::new();
        setup_large_feed_graph(&store);
        let result = execute_expand_plan(&store, &large_feed_page_plan());
        assert_eq!(result.rows.len(), FEED_LIMIT as usize);
    }

    #[test]
    fn large_friends_of_friends_setup_and_execute() {
        let store = GraphStore::new();
        setup_large_friends_of_friends_graph(&store);
        let result = execute_expand_plan(&store, &large_friends_of_friends_plan());
        assert_eq!(result.rows.len(), (FOF_FIRST_HOP * FOF_SECOND_HOP) as usize);
    }

    #[test]
    fn large_road_grid_setup_and_execute() {
        let store = GraphStore::new();
        setup_large_road_grid_graph(&store);
        let result = execute_shortest_plan(&store, &large_road_grid_weighted_shortest_plan());
        assert_eq!(result.rows.len(), 1);
    }
}
