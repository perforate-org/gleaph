use gleaph_gql::{
    executor::execute_plan, parse_statement, planner::build_plan, validate_statement,
};
use gleaph_pma::{PmaGraph, VecMemory};
use gleaph_types::Value;

#[test]
fn parser_accepts_temporal_predicates_and_var_len() {
    let stmt = parse_statement(
        "MATCH (a)-[e:KNOWS*1..3]->(b) WHERE gleaph_timestamp(e) > 10 AND gleaph_timestamp(e) < 30 RETURN b",
    )
    .unwrap();
    validate_statement(&stmt).unwrap();
}

#[test]
fn executor_filters_by_edge_timestamp() {
    let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
    let a = g.create_vertex(vec![], vec![]).unwrap();
    let b = g.create_vertex(vec![], vec![]).unwrap();
    let c = g.create_vertex(vec![], vec![]).unwrap();
    g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(a, c, Some("KNOWS".into()), vec![], 1.0, 50)
        .unwrap();

    let stmt =
        parse_statement("MATCH (a)-[e:KNOWS]->(b) WHERE gleaph_timestamp(e) <= 10 RETURN id(b)")
            .unwrap();
    validate_statement(&stmt).unwrap();
    let plan = build_plan(&stmt).unwrap();
    let res = execute_plan(&plan, &g).unwrap();
    assert_eq!(res.rows, vec![vec![Value::Int64(i64::from(b))]]);
}

#[test]
fn var_len_range_returns_reachable_vertices() {
    let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
    let a = g.create_vertex(vec![], vec![]).unwrap();
    let b = g.create_vertex(vec![], vec![]).unwrap();
    let c = g.create_vertex(vec![], vec![]).unwrap();
    g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
        .unwrap();
    g.create_edge(b, c, Some("KNOWS".into()), vec![], 1.0, 2)
        .unwrap();

    let stmt = parse_statement("MATCH (a)-[:KNOWS*1..2]->(b) RETURN id(b) ORDER BY id(b)").unwrap();
    validate_statement(&stmt).unwrap();
    let plan = build_plan(&stmt).unwrap();
    let res = execute_plan(&plan, &g).unwrap();
    assert!(
        res.rows
            .iter()
            .any(|r| r == &vec![Value::Int64(i64::from(b))])
    );
    assert!(
        res.rows
            .iter()
            .any(|r| r == &vec![Value::Int64(i64::from(c))])
    );
}

#[test]
fn executor_combines_temporal_and_label_filters() {
    let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
    let a = g.create_vertex(vec![], vec![]).unwrap();
    let b = g.create_vertex(vec![], vec![]).unwrap();
    let c = g.create_vertex(vec![], vec![]).unwrap();
    let d = g.create_vertex(vec![], vec![]).unwrap();
    g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(a, c, Some("LIKES".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(a, d, Some("KNOWS".into()), vec![], 1.0, 100)
        .unwrap();

    let stmt = parse_statement(
        "MATCH (a)-[e:KNOWS]->(b) WHERE gleaph_timestamp(e) >= 5 AND gleaph_timestamp(e) <= 20 RETURN id(b)",
    )
    .unwrap();
    validate_statement(&stmt).unwrap();
    let plan = build_plan(&stmt).unwrap();
    let res = execute_plan(&plan, &g).unwrap();
    assert_eq!(res.rows, vec![vec![Value::Int64(i64::from(b))]]);
}

#[test]
fn var_len_path_respects_temporal_window() {
    let mut g = PmaGraph::new(VecMemory::default(), 0).unwrap();
    let a = g.create_vertex(vec![], vec![]).unwrap();
    let b = g.create_vertex(vec![], vec![]).unwrap();
    let c = g.create_vertex(vec![], vec![]).unwrap();
    let d = g.create_vertex(vec![], vec![]).unwrap();
    g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 5)
        .unwrap();
    g.create_edge(b, c, Some("KNOWS".into()), vec![], 1.0, 10)
        .unwrap();
    g.create_edge(c, d, Some("KNOWS".into()), vec![], 1.0, 100)
        .unwrap();

    let stmt = parse_statement(
        "MATCH (a)-[e:KNOWS*1..3]->(b) WHERE gleaph_timestamp(e) <= 20 RETURN id(b) ORDER BY id(b)",
    )
    .unwrap();
    validate_statement(&stmt).unwrap();
    let plan = build_plan(&stmt).unwrap();
    let res = execute_plan(&plan, &g).unwrap();
    assert!(
        res.rows
            .iter()
            .any(|r| r == &vec![Value::Int64(i64::from(b))])
    );
    assert!(
        res.rows
            .iter()
            .any(|r| r == &vec![Value::Int64(i64::from(c))])
    );
    assert!(
        !res.rows
            .iter()
            .any(|r| r == &vec![Value::Int64(i64::from(d))]),
        "late edge should be filtered in var-len expansion"
    );
}
