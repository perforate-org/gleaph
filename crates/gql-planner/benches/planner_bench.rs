//! Planner performance benchmarks.
//!
//! Measures plan generation time across various query patterns and scales.

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use gleaph_gql::parser;
use gleaph_gql_planner::stats::TableStats;
use gleaph_gql_planner::{build_plan, explain_plan};


// ════════════════════════════════════════════════════════════════════════════════
// Helpers
// ════════════════════════════════════════════════════════════════════════════════

fn parse_and_plan(input: &str, stats: Option<&TableStats>) {
    let program = parser::parse(input).expect("parse");
    let tx = program.transaction_activity.expect("tx");
    let block = tx.body.expect("block");
    if let gleaph_gql::ast::Statement::Query(composite) = &block.first {
        let plan = build_plan(
            &composite.left,
            stats.map(|s| s as &dyn gleaph_gql_planner::stats::GraphStats),
        )
        .expect("plan");
        // Force explain to exercise all formatting paths.
        let _ = explain_plan(&plan);
    }
}

fn make_stats() -> TableStats {
    let mut stats = TableStats::default();
    stats.avg_degree = 10.0;
    stats
        .label_cardinality
        .insert("Person".to_string(), 100_000);
    stats
        .label_cardinality
        .insert("Company".to_string(), 10_000);
    stats.label_cardinality.insert("Post".to_string(), 500_000);
    stats.label_cardinality.insert("Tag".to_string(), 1_000);
    stats
        .indexed_vertex_properties
        .insert("uid".to_string());
    stats
        .indexed_vertex_properties
        .insert("name".to_string());
    stats
        .property_selectivity
        .insert("uid".to_string(), 0.001);
    stats
        .property_selectivity
        .insert("name".to_string(), 0.01);
    stats
        .property_selectivity
        .insert("age".to_string(), 0.05);
    stats.edge_endpoint_labels.insert(
        "WORKS_AT".to_string(),
        (vec!["Person".to_string()], vec!["Company".to_string()]),
    );
    stats.edge_endpoint_labels.insert(
        "KNOWS".to_string(),
        (vec!["Person".to_string()], vec!["Person".to_string()]),
    );
    stats
}

// ════════════════════════════════════════════════════════════════════════════════
// Query generators
// ════════════════════════════════════════════════════════════════════════════════

/// Simple: MATCH (n:Label) WHERE n.prop = val RETURN n
fn gen_simple_filter(n_predicates: usize) -> String {
    let mut where_parts = Vec::new();
    for i in 0..n_predicates {
        where_parts.push(format!("n.prop{} = {}", i, i));
    }
    let where_clause = if where_parts.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_parts.join(" AND "))
    };
    format!("MATCH (n:Person){} RETURN n", where_clause)
}

/// Chain: (a)-[]->(b)-[]->(c)-[]->... of length `hops`
fn gen_chain(hops: usize) -> String {
    let mut pattern = "(n0:Person)".to_string();
    for i in 0..hops {
        pattern.push_str(&format!("-[e{}:KNOWS]->(n{}:Person)", i, i + 1));
    }
    let returns: Vec<String> = (0..=hops).map(|i| format!("n{}", i)).collect();
    format!("MATCH {} RETURN {}", pattern, returns.join(", "))
}

/// Star: central node with `branches` outgoing edges
fn gen_star(branches: usize) -> String {
    let mut patterns = Vec::new();
    let mut returns = vec!["center".to_string()];
    for i in 0..branches {
        patterns.push(format!(
            "(center:Person)-[e{0}:KNOWS]->(leaf{0}:Person)",
            i
        ));
        returns.push(format!("leaf{}", i));
    }
    format!(
        "MATCH {} RETURN {}",
        patterns.join(", "),
        returns.join(", ")
    )
}

/// Chain with inline filters on every node
fn gen_chain_filtered(hops: usize) -> String {
    let mut pattern = "(n0:Person {active: TRUE})".to_string();
    for i in 0..hops {
        pattern.push_str(&format!(
            "-[e{}:KNOWS]->(n{}:Person {{score: {}}})",
            i,
            i + 1,
            i * 10
        ));
    }
    let returns: Vec<String> = (0..=hops).map(|i| format!("n{}", i)).collect();
    format!("MATCH {} RETURN {}", pattern, returns.join(", "))
}

/// MATCH with WHERE having many predicates + ORDER BY + LIMIT (TopK candidate)
fn gen_topk(n_predicates: usize) -> String {
    let mut where_parts = Vec::new();
    for i in 0..n_predicates {
        where_parts.push(format!("n.prop{} > {}", i, i * 10));
    }
    format!(
        "MATCH (n:Person) WHERE {} RETURN n ORDER BY n.score DESC LIMIT 10",
        where_parts.join(" AND ")
    )
}

/// Triangle pattern: (a)->(b)->(c)->(a)
fn gen_triangle() -> String {
    "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(a) \
     RETURN a, b, c"
        .to_string()
}

/// Aggregation: GROUP BY with multiple keys
fn gen_aggregation(group_keys: usize) -> String {
    let keys: Vec<String> = (0..group_keys).map(|i| format!("n.key{}", i)).collect();
    format!(
        "MATCH (n:Person) RETURN {}, COUNT(*) AS cnt",
        keys.join(", ")
    )
}

// ════════════════════════════════════════════════════════════════════════════════
// Benchmarks
// ════════════════════════════════════════════════════════════════════════════════

fn bench_simple_filter(c: &mut Criterion) {
    let stats = make_stats();
    let mut group = c.benchmark_group("simple_filter");

    for n in [0, 1, 5, 10, 20, 50] {
        let query = gen_simple_filter(n);
        group.bench_with_input(BenchmarkId::new("no_stats", n), &n, |b, _| {
            b.iter(|| parse_and_plan(&query, None));
        });
        group.bench_with_input(BenchmarkId::new("with_stats", n), &n, |b, _| {
            b.iter(|| parse_and_plan(&query, Some(&stats)));
        });
    }
    group.finish();
}

fn bench_chain(c: &mut Criterion) {
    let stats = make_stats();
    let mut group = c.benchmark_group("chain");

    for hops in [1, 2, 5, 10, 20, 50] {
        let query = gen_chain(hops);
        group.bench_with_input(BenchmarkId::new("no_stats", hops), &hops, |b, _| {
            b.iter(|| parse_and_plan(&query, None));
        });
        group.bench_with_input(BenchmarkId::new("with_stats", hops), &hops, |b, _| {
            b.iter(|| parse_and_plan(&query, Some(&stats)));
        });
    }
    group.finish();
}

fn bench_chain_filtered(c: &mut Criterion) {
    let stats = make_stats();
    let mut group = c.benchmark_group("chain_filtered");

    for hops in [1, 2, 5, 10, 20] {
        let query = gen_chain_filtered(hops);
        group.bench_with_input(BenchmarkId::new("with_stats", hops), &hops, |b, _| {
            b.iter(|| parse_and_plan(&query, Some(&stats)));
        });
    }
    group.finish();
}

fn bench_star(c: &mut Criterion) {
    let stats = make_stats();
    let mut group = c.benchmark_group("star");

    for branches in [2, 5, 10, 20, 50] {
        let query = gen_star(branches);
        group.bench_with_input(
            BenchmarkId::new("with_stats", branches),
            &branches,
            |b, _| {
                b.iter(|| parse_and_plan(&query, Some(&stats)));
            },
        );
    }
    group.finish();
}

fn bench_topk(c: &mut Criterion) {
    let stats = make_stats();
    let mut group = c.benchmark_group("topk");

    for n in [1, 5, 10, 20] {
        let query = gen_topk(n);
        group.bench_with_input(BenchmarkId::new("with_stats", n), &n, |b, _| {
            b.iter(|| parse_and_plan(&query, Some(&stats)));
        });
    }
    group.finish();
}

fn bench_triangle(c: &mut Criterion) {
    let stats = make_stats();
    let mut group = c.benchmark_group("triangle");

    let query = gen_triangle();
    group.bench_function("no_stats", |b| {
        b.iter(|| parse_and_plan(&query, None));
    });
    group.bench_function("with_stats", |b| {
        b.iter(|| parse_and_plan(&query, Some(&stats)));
    });
    group.finish();
}

fn bench_aggregation(c: &mut Criterion) {
    let stats = make_stats();
    let mut group = c.benchmark_group("aggregation");

    for keys in [1, 3, 5, 10] {
        let query = gen_aggregation(keys);
        group.bench_with_input(BenchmarkId::new("with_stats", keys), &keys, |b, _| {
            b.iter(|| parse_and_plan(&query, Some(&stats)));
        });
    }
    group.finish();
}

/// Parse-only vs full pipeline comparison.
fn bench_parse_vs_plan(c: &mut Criterion) {
    let stats = make_stats();
    let mut group = c.benchmark_group("parse_vs_plan");

    let query = gen_chain(10);

    group.bench_function("parse_only", |b| {
        b.iter(|| {
            let _ = parser::parse(&query).expect("parse");
        });
    });

    group.bench_function("parse_and_plan", |b| {
        b.iter(|| parse_and_plan(&query, Some(&stats)));
    });

    // Pre-parsed: measure planner overhead only.
    let program = parser::parse(&query).expect("parse");
    let tx = program.transaction_activity.as_ref().expect("tx");
    let block = tx.body.as_ref().expect("block");
    let linear = if let gleaph_gql::ast::Statement::Query(composite) = &block.first {
        composite.left.clone()
    } else {
        panic!("expected query");
    };

    group.bench_function("plan_only", |b| {
        b.iter(|| {
            let plan = build_plan(&linear, Some(&stats)).expect("plan");
            let _ = explain_plan(&plan);
        });
    });

    group.finish();
}

criterion_group!(
    benches,
    bench_simple_filter,
    bench_chain,
    bench_chain_filtered,
    bench_star,
    bench_topk,
    bench_triangle,
    bench_aggregation,
    bench_parse_vs_plan,
);
criterion_main!(benches);
