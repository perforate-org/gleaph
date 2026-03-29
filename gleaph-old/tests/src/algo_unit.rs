use gleaph_algo::{
    bfs::{BfsConfig, bfs},
    budget::{CountingBudget, UnlimitedBudget},
    pagerank::{PageRankConfig, pagerank},
    recommend::{RecommendConfig, recommend},
    sssp::{SsspConfig, dijkstra},
};
use gleaph_pma::{PmaGraph, VecMemory};
use gleaph_types::{GleaphError, TimestampRange};

fn mk_graph() -> PmaGraph<VecMemory> {
    PmaGraph::new(VecMemory::default(), 0).unwrap()
}

#[test]
fn bfs_and_sssp_work_on_small_chain() {
    let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
    let a = g.create_vertex(vec![], vec![]).unwrap();
    let b = g.create_vertex(vec![], vec![]).unwrap();
    let c = g.create_vertex(vec![], vec![]).unwrap();
    g.create_edge(a, b, Some("X".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(b, c, Some("X".into()), vec![], 2.0, 20)
        .unwrap();

    let mut ub = UnlimitedBudget;
    let bfs_res = bfs(
        &g,
        a,
        &BfsConfig {
            target: Some(c),
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    assert_eq!(bfs_res.path, Some(vec![a, b, c]));

    let mut ub = UnlimitedBudget;
    let sssp = dijkstra(&g, a, &SsspConfig::default(), &mut ub).unwrap();
    assert!(
        sssp.distances
            .iter()
            .any(|(v, d)| *v == c && (*d - 3.0).abs() < 1e-9)
    );
}

#[test]
fn recommend_and_pagerank_work() {
    let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
    let u1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u2 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let i1 = g.create_vertex(vec!["Item".into()], vec![]).unwrap();
    let i2 = g.create_vertex(vec!["Item".into()], vec![]).unwrap();
    g.create_edge(u1, i1, Some("FOLLOW".into()), vec![], 1.0, 1)
        .unwrap();
    g.create_edge(u2, i1, Some("FOLLOW".into()), vec![], 1.0, 1)
        .unwrap();
    g.create_edge(u2, i2, Some("FOLLOW".into()), vec![], 1.0, 1)
        .unwrap();

    let mut ub = UnlimitedBudget;
    let recs = recommend(&g, u1, &RecommendConfig::default(), &mut ub).unwrap();
    assert_eq!(recs.first().map(|r| r.vertex_id), Some(i2));

    let mut ub = UnlimitedBudget;
    let pr = pagerank(
        &g,
        &PageRankConfig {
            max_iterations: 5,
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    assert!(!pr.scores.is_empty());
}

#[test]
fn budget_exhaustion_surfaces() {
    let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
    let a = g.create_vertex(vec![], vec![]).unwrap();
    let b = g.create_vertex(vec![], vec![]).unwrap();
    g.create_edge(a, b, Some("X".into()), vec![], 1.0, 1)
        .unwrap();
    let mut budget = CountingBudget::new(0);
    let err = bfs(&g, a, &BfsConfig::default(), &mut budget).unwrap_err();
    assert!(matches!(err, gleaph_types::GleaphError::BudgetExhausted));
}

#[test]
fn bfs_respects_max_depth_label_temporal_and_cycle() {
    let mut g = mk_graph();
    let a = g.create_vertex(vec![], vec![]).unwrap();
    let b = g.create_vertex(vec![], vec![]).unwrap();
    let c = g.create_vertex(vec![], vec![]).unwrap();
    let d = g.create_vertex(vec![], vec![]).unwrap();
    g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(b, c, Some("KNOWS".into()), vec![], 1.0, 20)
        .unwrap();
    g.create_edge(c, a, Some("KNOWS".into()), vec![], 1.0, 30)
        .unwrap();
    g.create_edge(a, d, Some("LIKES".into()), vec![], 1.0, 40)
        .unwrap();

    let mut ub = UnlimitedBudget;
    let res = bfs(
        &g,
        a,
        &BfsConfig {
            max_depth: Some(1),
            edge_label: Some("KNOWS".into()),
            ts_range: Some(TimestampRange {
                start: Some(5),
                end: Some(15),
            }),
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    assert!(res.visited.contains(&a));
    assert!(res.visited.contains(&b));
    assert!(
        !res.visited.contains(&c),
        "max_depth should stop expansion at b"
    );
    assert!(
        !res.visited.contains(&d),
        "label filter should exclude LIKES edge"
    );
}

#[test]
fn bfs_single_vertex_target_found_and_max_visited() {
    let mut g = mk_graph();
    let a = g.create_vertex(vec![], vec![]).unwrap();
    let b = g.create_vertex(vec![], vec![]).unwrap();
    let c = g.create_vertex(vec![], vec![]).unwrap();
    g.create_edge(a, b, Some("X".into()), vec![], 1.0, 1)
        .unwrap();
    g.create_edge(a, c, Some("X".into()), vec![], 1.0, 1)
        .unwrap();

    let mut ub = UnlimitedBudget;
    let self_target = bfs(
        &g,
        a,
        &BfsConfig {
            target: Some(a),
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    assert_eq!(self_target.path, Some(vec![a]));

    let mut ub = UnlimitedBudget;
    let limited = bfs(
        &g,
        a,
        &BfsConfig {
            max_visited: Some(2),
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    assert_eq!(limited.visited.len(), 2, "max_visited should cap traversal");
}

#[test]
fn recommend_covers_empty_exclude_known_temporal_and_budget() {
    let mut g = mk_graph();
    let u1 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u2 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let u3 = g.create_vertex(vec!["User".into()], vec![]).unwrap();
    let i1 = g.create_vertex(vec!["Item".into()], vec![]).unwrap();
    let i2 = g.create_vertex(vec!["Item".into()], vec![]).unwrap();
    let i3 = g.create_vertex(vec!["Item".into()], vec![]).unwrap();
    let i4 = g.create_vertex(vec!["Item".into()], vec![]).unwrap();
    g.create_edge(u1, i1, Some("FOLLOW".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(u1, i2, Some("FOLLOW".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(u2, i1, Some("FOLLOW".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(u2, i2, Some("FOLLOW".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(u3, i2, Some("FOLLOW".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(u3, i3, Some("FOLLOW".into()), vec![], 1.0, 100)
        .unwrap();
    g.create_edge(u3, i4, Some("FOLLOW".into()), vec![], 1.0, 10)
        .unwrap();

    let mut ub = UnlimitedBudget;
    let empty = recommend(&g, 999_999, &RecommendConfig::default(), &mut ub).unwrap_err();
    assert!(matches!(empty, GleaphError::VertexNotFound(_)));

    let mut ub = UnlimitedBudget;
    let recs = recommend(
        &g,
        u1,
        &RecommendConfig {
            max_hops: 4,
            ts_range: Some(TimestampRange {
                start: Some(1),
                end: Some(50),
            }),
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    assert!(
        recs.iter().any(|r| r.vertex_id == i4),
        "deeper-hop recommendation expected"
    );
    assert!(
        recs.iter().all(|r| r.vertex_id != i1),
        "exclude_known should exclude already-owned item"
    );
    assert!(
        recs.iter().all(|r| r.vertex_id != i3),
        "temporal window should exclude newer purchase edge"
    );

    let mut ub = UnlimitedBudget;
    let recs_include_known = recommend(
        &g,
        u1,
        &RecommendConfig {
            exclude_known: false,
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    assert!(recs_include_known.iter().any(|r| r.vertex_id == i2));

    let mut budget = CountingBudget::new(0);
    let err = recommend(&g, u1, &RecommendConfig::default(), &mut budget).unwrap_err();
    assert!(matches!(err, GleaphError::BudgetExhausted));
}

#[test]
fn pagerank_triangle_star_temporal_and_budget() {
    let mut triangle = mk_graph();
    let a = triangle.create_vertex(vec![], vec![]).unwrap();
    let b = triangle.create_vertex(vec![], vec![]).unwrap();
    let c = triangle.create_vertex(vec![], vec![]).unwrap();
    triangle
        .create_edge(a, b, Some("X".into()), vec![], 1.0, 1)
        .unwrap();
    triangle
        .create_edge(b, c, Some("X".into()), vec![], 1.0, 1)
        .unwrap();
    triangle
        .create_edge(c, a, Some("X".into()), vec![], 1.0, 1)
        .unwrap();

    let mut ub = UnlimitedBudget;
    let tri = pagerank(
        &triangle,
        &PageRankConfig {
            max_iterations: 50,
            convergence_threshold: 1e-12,
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    assert!(tri.converged);
    let vals: Vec<f64> = tri.scores.iter().map(|(_, s)| *s).collect();
    assert!(
        vals.windows(2).all(|w| (w[0] - w[1]).abs() < 1e-6),
        "triangle ranks should be equal"
    );

    let mut star = mk_graph();
    let center = star.create_vertex(vec![], vec![]).unwrap();
    let l1 = star.create_vertex(vec![], vec![]).unwrap();
    let l2 = star.create_vertex(vec![], vec![]).unwrap();
    let l3 = star.create_vertex(vec![], vec![]).unwrap();
    star.create_edge(l1, center, Some("X".into()), vec![], 1.0, 10)
        .unwrap();
    star.create_edge(l2, center, Some("X".into()), vec![], 1.0, 10)
        .unwrap();
    star.create_edge(l3, center, Some("X".into()), vec![], 1.0, 10)
        .unwrap();
    star.create_edge(center, l1, Some("X".into()), vec![], 1.0, 10)
        .unwrap();
    star.create_edge(center, l2, Some("X".into()), vec![], 1.0, 100)
        .unwrap();
    star.create_edge(center, l3, Some("X".into()), vec![], 1.0, 100)
        .unwrap();

    let mut ub = UnlimitedBudget;
    let temporal = pagerank(
        &star,
        &PageRankConfig {
            ts_range: Some(TimestampRange {
                start: Some(0),
                end: Some(20),
            }),
            max_iterations: 20,
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    let center_rank_temporal = temporal
        .scores
        .iter()
        .find(|(v, _)| *v == center)
        .unwrap()
        .1;
    // In the temporal subgraph (ts ≤ 20) center has 3 in-edges but only 1 out-edge,
    // while each leaf has 1 in-edge and 1 out-edge, so center must outrank every leaf.
    for leaf in [l1, l2, l3] {
        let leaf_rank = temporal.scores.iter().find(|(v, _)| *v == leaf).unwrap().1;
        assert!(
            center_rank_temporal > leaf_rank,
            "center should outrank leaf in temporal subgraph"
        );
    }

    let mut budget = CountingBudget::new(0);
    let partial = pagerank(&star, &PageRankConfig::default(), &mut budget).unwrap();
    assert!(!partial.converged);
}

#[test]
fn sssp_shortest_path_negative_unreachable_target_temporal_and_budget() {
    let mut g = mk_graph();
    let a = g.create_vertex(vec![], vec![]).unwrap();
    let b = g.create_vertex(vec![], vec![]).unwrap();
    let c = g.create_vertex(vec![], vec![]).unwrap();
    let d = g.create_vertex(vec![], vec![]).unwrap();
    let e = g.create_vertex(vec![], vec![]).unwrap();
    g.create_edge(a, b, Some("ROAD".into()), vec![], 5.0, 10)
        .unwrap();
    g.create_edge(a, c, Some("ROAD".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(c, b, Some("ROAD".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(b, d, Some("ROAD".into()), vec![], 1.0, 100)
        .unwrap();
    let mut ub = UnlimitedBudget;
    let res = dijkstra(
        &g,
        a,
        &SsspConfig {
            target: Some(b),
            edge_label: Some("ROAD".into()),
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    let db = res.distances.iter().find(|(v, _)| *v == b).unwrap().1;
    assert!((db - 2.0).abs() < 1e-9);
    assert!(
        !res.distances.iter().any(|(v, _)| *v == e),
        "unreachable vertex should not appear"
    );

    let mut ub = UnlimitedBudget;
    let temporal = dijkstra(
        &g,
        a,
        &SsspConfig {
            edge_label: Some("ROAD".into()),
            ts_range: Some(TimestampRange {
                start: Some(0),
                end: Some(50),
            }),
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap();
    assert!(
        !temporal.distances.iter().any(|(v, _)| *v == d),
        "temporal filter should exclude late edge to d"
    );

    g.create_edge(a, e, Some("NEG".into()), vec![], -1.0, 1)
        .unwrap();
    let mut ub = UnlimitedBudget;
    let err = dijkstra(
        &g,
        a,
        &SsspConfig {
            edge_label: Some("NEG".into()),
            ..Default::default()
        },
        &mut ub,
    )
    .unwrap_err();
    assert!(matches!(err, GleaphError::AlgorithmError(_)));

    let mut budget = CountingBudget::new(0);
    let err = dijkstra(&g, a, &SsspConfig::default(), &mut budget).unwrap_err();
    assert!(matches!(err, GleaphError::BudgetExhausted));
}
