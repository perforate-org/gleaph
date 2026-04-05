//! IC-style canbench targets: PMA graph backed by real stable memory on wasm32 (`Memory` API),
//! or `VecMemory` on the host for `cargo check`.
//!
//! Executor benches **warm** the graph (wipe + bootstrap + kernel seed) **once** before
//! `canbench_rs::bench_fn`; the measured closure runs **only** parse/plan/execute for the query
//! (or block), so instruction counts are not dominated by PMA cold start.
//!
//! With the `canbench-rs` feature (see `canbench.yml`), block execution and PMA dirty flush record
//! [`canbench_rs::bench_scope`] regions for granular instruction splits — see
//! [canbench_rs](https://docs.rs/canbench-rs/latest/canbench_rs/) (“Granular Benchmarking”).
//! Names include `gql_block_parse`, `gql_block_plan`, `gql_block_execute`; single-query execution via
//! [`crate::execute_query_str`] adds `gql_query_parse`, `gql_query_plan`, `gql_query_execute`.
//! Executor operator dispatch is under `gql_exec_dispatch_ops`. `PlanOp::NodeScan` /
//! `IndexScan` / `EdgeIndexScan` / `EdgeBindEndpoints` use `gql_exec_node_scan`, `gql_exec_index_scan`,
//! `gql_exec_edge_index_scan`, `gql_exec_edge_bind` on the **match arms** (same pattern as
//! `gql_exec_expand`). Leading-edge plans often skip `NodeScan`, so some workloads show
//! `gql_exec_edge_index_scan` / `gql_exec_edge_bind` instead of `gql_exec_node_scan`. On typical
//! ring expand benches (`bench_gql_execute_expand_*`), **`gql_exec_dispatch_ops` ≈ `gql_exec_node_scan`
//! + `gql_exec_expand` + remaining ops** (Project / other `PlanOp`s); use that identity if a nested
//! scope is missing in a given wasm build. `gql_exec_materialize` runs only when the plan does not emit projected rows early
//! (e.g. `RETURN n.uid` often skips it; `RETURN n` does not).
//! Terminal [`GraphWrite::flush`] is
//! `gql_exec_plan_flush` (despite the name, not query planning). PMA flush scopes:
//! `pma_graph_refresh_write`, `pma_node_property_store_flush`, `pma_edge_property_store_flush`,
//! `pma_property_index_paged_flush`, `pma_maint_queue_persist`, finer PIDX scopes inside flush
//! (`pma_pidx_*`), and (when enabled) `gql_exec_set_property_item`.
//!
//! ## Reading mutation baseline (`canbench_results.yml`)
//!
//! For each `bench_gql_execute_block_*` entry, compare `total.instructions` to
//! `scopes` (e.g. `pma_property_index_paged_flush.instructions`) and note each scope’s `calls`
//! (how often that region was entered). Multiple properties or flushes can increment the same
//! label more than once per bench sample.
//!
//! Prefer refreshing results with **`cd crates/graph && canbench --persist` without a bench
//! filter** so `canbench_results.yml` keeps the full benchmark list instead of shrinking to one
//! entry.
//!
//! ### Baseline regression axes (instruction scale is build-dependent)
//!
//! - **`pma_pidx_write_region`**: largest slice of PIDX work (full property-index region writeback).
//! - **`pma_property_index_paged_flush`**: outer PIDX flush; should track closely with `pma_pidx_*`.
//! - **`pma_graph_refresh_write`**: graph surface writeback. Compare `calls` across benchmarks:
//!   a single terminal [`GraphWrite::flush`] should incur **one** entry per flush round-trip; values
//!   above **1** often mean **`INSERT`** (`bootstrap_*_and_write` plus executor `flush`) or more than
//!   one flush in the measured path. **`PlanOp::SetOperation`** RHS defers its terminal flush to the
//!   outer plan (`execute_plan_with_context_maybe_flush` in `gleaph-gql-executor`). IC numbers track
//!   whichever wasm is **installed** on the benchmark canister—rebuild/redeploy before trusting deltas.
//! - **`gql_exec_plan_flush`**: terminal `graph.flush()` only; overlaps nested PMA scopes in totals;
//!   use **`pma_*`** for attribution. The scope is only a few kilo-instructions; large **percentage**
//!   deltas versus an older baseline often reflect **wasm build / canbench attribution drift** rather
//!   than a regression in flush work—compare **`pma_graph_refresh_write`** and related `pma_*`
//!   scopes on the **same** rebuilt wasm before treating `%` on `gql_exec_plan_flush` as meaningful.
//! - **`gql_exec_dispatch_ops`**: gql-executor operator dispatch (`execute_ops`); Expand/scan body.
//! - **`gql_exec_expand`**: `PlanOp::Expand` only (inside dispatch); narrows graph expand vs other ops.
//! - **`gql_exec_node_scan`**: `PlanOp::NodeScan` only (may be absent when the plan leads with index).
//! - **`gql_exec_index_scan` / `gql_exec_edge_index_scan` / `gql_exec_edge_bind`**: matching scan/bind ops.
//! - **`gql_exec_materialize`**: post-pipeline `materialize_row` loop (`projected` was `None`).
//! - **`gql_query_parse` / `gql_query_plan` / `gql_query_execute`**: split for
//!   [`crate::execute_query_str`] (overlay query benches).
//!
//! Multi-`SET` / **`NEXT`**: expect **`pma_property_index_paged_flush.calls` == 1** per block sample and
//! **`gql_exec_set_property_item.calls`** equal to the number of property `SET` items.
//!
//! ## `pma_graph_refresh_write.calls` on mutation benches (instrumentation vs code paths)
//!
//! The `pma_graph_refresh_write` [`canbench_rs::bench_scope`] is installed only on
//! [`gleaph_graph_pma::facade::GraphPma::refresh_and_write_dirty_to_stable_memory`] (facade
//! flush: graph refresh + optional property stores + PIDX paged flush + maintenance queue). It is
//! **not** wrapped around low-level
//! [`GraphRuntime::refresh_and_write_dirty_to_stable_memory`](gleaph_graph_pma::low_level::GraphRuntime::refresh_and_write_dirty_to_stable_memory),
//! which is what internal `*_graph.*_and_write` helpers call **inside** some mutations (e.g.
//! `tombstone_edge_pair_and_write` after `tombstone_edge_pair`).
//!
//! - **`bench_gql_execute_block_insert_person` (+ multi_prop / bare):** seed uses `INSERT` →
//!   `bootstrap_vertex_refs_and_edges_and_write` → **facade** `try_refresh_and_write_dirty_to_stable_memory`
//!   (**1** scoped entry), then executor terminal `GraphWrite::flush` → facade again (**2** scoped
//!   entries in `canbench_results.yml`). Matching `pma_maint_queue_persist.calls` often **2** when the
//!   first flush had no dirty property-store pipeline (early-return path persists the queue only).
//! - **`bench_gql_execute_block_delete_edge`:** each `delete_edge` → `tombstone_edge_pair_and_write` →
//!   **runtime** graph refresh (unscoped) + executor **`flush`** → facade (**`calls` == 1** is
//!   expected, not a single end-to-end write).
//! - **`bench_gql_execute_block_delete_vertex` / `bench_gql_execute_block_detach_delete_vertex`:**
//!   structural deletes may perform multiple **unscoped** runtime refreshes (e.g. per tombstoned edge
//!   on `DETACH DELETE`); **`pma_graph_refresh_write`** still counts **terminal facade flush(es)** only
//!   (typically **1** per block when the executor runs one plan flush). Compare PIDX cost via
//!   `pma_pidx_*` / `pma_property_index_paged_flush`, not only this label.
//! - **Consolidation idea (future):** route tombstone/replace internal writeback through the facade
//!   flush or add a matching scope on the runtime path if Profiles should attribute “every stable
//!   graph refresh” under one label.
//!
//! ## Planned workload-shaped benches (design only; not implemented yet)
//!
//! Approximate real app update patterns on top of existing `overlay_block` / `person_ring` seeds:
//!
//! 1. **`bench_gql_execute_block_bulk_set_person_props`** — `seed_person_ring_spec(32)` (or 128),
//!    block: `MATCH (n:Person) WHERE n.region = 'east' SET n.score = n.score + 1, n.bucket = 'hot'
//!    RETURN count(n)` (tune `WHERE` / property count). **Read in canbench:** `gql_exec_set_property_item`,
//!    `pma_property_index_paged_flush`, `pma_pidx_write_region`, `pma_node_property_store_flush`.
//! 2. **`bench_gql_execute_block_follow_toggle_next`** — star or ring with a single `FOLLOWS` edge
//!    pattern; block uses **`NEXT`**: first statement `INSERT` or `MATCH … CREATE` the edge, second
//!    `MATCH … DELETE` it (alternate rows or fixed endpoints so toggling stays deterministic). **Read:**
//!    mix of `pma_graph_refresh_write`, `pma_node_property_store_flush` / edge store, and tombstone
//!    latency vs insert-heavy benches.
//! 3. **`bench_gql_execute_block_bulk_detach_delete`** — larger fixed `Person` chain; one `DETACH DELETE`
//!    of a high-degree or middle node per sample (optionally `LIMIT 1`). **Read:** `pma_pidx_*`
//!    scaling with incident edges and label-catalog / GC persistence (`persist_vertex_label_index` is
//!    outside `pma_graph_refresh_write` — use totals if investigating).
//!
//! Implementation notes when adding these: extend [`KernelBootstrapGraphSpec`] or local `seed_*`
//! helpers; keep **`bench_overlay_block!`** so warmup/bootstrap stays outside the measured closure;
//! document expected `calls` next to the new `#[bench(raw)]` like the inventory above.

mod memory;

use std::collections::BTreeMap;

use memory::{BenchMemory, wipe_for_bench_iteration};

use crate::{
    execute_block_str, execute_query_str, parse_block, parse_query, plan_block,
    standard_procedure_registry,
};
use canbench_rs::bench;
use gleaph_gql::Value;
use gleaph_gql_executor::ExecutionContext;
use gleaph_gql_planner::build_plan_output;
use gleaph_graph_kernel::PropertyMap;
use gleaph_graph_pma::integration::{
    KernelBootstrapEdgeSpec, KernelBootstrapGraphSpec, KernelBootstrapNodeSpec,
    GraphPmaKernelHarness,
};

fn execution_context() -> ExecutionContext {
    ExecutionContext {
        procedure_registry: Some(standard_procedure_registry()),
        ..ExecutionContext::default()
    }
}

fn execution_context_with_params(params: BTreeMap<String, Value>) -> ExecutionContext {
    ExecutionContext {
        params,
        procedure_registry: Some(standard_procedure_registry()),
        ..ExecutionContext::default()
    }
}

/// Ring of `Person` nodes with KNOWS edges (single label, varied properties).
fn seed_person_ring_spec(n: usize) -> KernelBootstrapGraphSpec {
    let mut spec = KernelBootstrapGraphSpec::empty();
    for i in 0..n {
        let mut props = PropertyMap::new();
        props.insert("uid".into(), Value::Text(format!("u{i}")));
        props.insert("age".into(), Value::Int64(20 + (i as i64 % 50)));
        props.insert(
            "region".into(),
            Value::Text((if i % 2 == 0 { "east" } else { "west" }).to_owned()),
        );
        props.insert("score".into(), Value::Int64((i * 7) as i64));
        spec = spec.with_node(KernelBootstrapNodeSpec::from_parts(&["Person"], &props));
    }
    for i in 0..n {
        let j = (i + 1) % n;
        spec = spec.with_edge(KernelBootstrapEdgeSpec::from_parts(
            i,
            j,
            Some("KNOWS"),
            &PropertyMap::new(),
        ));
    }
    spec
}

fn seed_shortest_path_spec() -> KernelBootstrapGraphSpec {
    let mut a = PropertyMap::new();
    a.insert("name".into(), Value::Text("a".into()));
    let mut b = PropertyMap::new();
    b.insert("name".into(), Value::Text("b".into()));
    let mut c = PropertyMap::new();
    c.insert("name".into(), Value::Text("c".into()));
    KernelBootstrapGraphSpec::empty()
        .with_node(KernelBootstrapNodeSpec::from_parts(&["U"], &a))
        .with_node(KernelBootstrapNodeSpec::from_parts(&["U"], &b))
        .with_node(KernelBootstrapNodeSpec::from_parts(&["U"], &c))
        .with_edge(KernelBootstrapEdgeSpec::from_parts(
            0,
            1,
            Some("KNOWS"),
            &PropertyMap::new(),
        ))
        .with_edge(KernelBootstrapEdgeSpec::from_parts(
            1,
            2,
            Some("KNOWS"),
            &PropertyMap::new(),
        ))
}

fn seed_avg_nodes_spec(n: usize) -> KernelBootstrapGraphSpec {
    let mut spec = KernelBootstrapGraphSpec::empty();
    for i in 0..n {
        let mut props = PropertyMap::new();
        props.insert("x".into(), Value::Int64(i as i64 * 3));
        spec = spec.with_node(KernelBootstrapNodeSpec::from_parts(&["Stat"], &props));
    }
    spec
}

/// Ring with an extra `bucket` property (8 values) for `GROUP BY` benches.
fn seed_person_ring_with_bucket_spec(n: usize) -> KernelBootstrapGraphSpec {
    let mut spec = KernelBootstrapGraphSpec::empty();
    for i in 0..n {
        let mut props = PropertyMap::new();
        props.insert("uid".into(), Value::Text(format!("u{i}")));
        props.insert("age".into(), Value::Int64(20 + (i as i64 % 50)));
        props.insert(
            "region".into(),
            Value::Text((if i % 2 == 0 { "east" } else { "west" }).to_owned()),
        );
        props.insert("score".into(), Value::Int64((i * 7) as i64));
        props.insert("bucket".into(), Value::Text(format!("b{}", i % 8)));
        spec = spec.with_node(KernelBootstrapNodeSpec::from_parts(&["Person"], &props));
    }
    for i in 0..n {
        let j = (i + 1) % n;
        spec = spec.with_edge(KernelBootstrapEdgeSpec::from_parts(
            i,
            j,
            Some("KNOWS"),
            &PropertyMap::new(),
        ));
    }
    spec
}

/// Person ↔ Post small bipartite fan-out (article-style): each of `persons` persons links to `fanout` posts.
fn seed_person_post_star_spec(persons: usize, fanout: usize) -> KernelBootstrapGraphSpec {
    let mut spec = KernelBootstrapGraphSpec::empty();
    for p in 0..persons {
        let mut props = PropertyMap::new();
        props.insert("uid".into(), Value::Text(format!("p{p}")));
        props.insert("kind".into(), Value::Text("author".into()));
        spec = spec.with_node(KernelBootstrapNodeSpec::from_parts(&["Person"], &props));
    }
    let base = persons;
    for i in 0..(persons * fanout) {
        let mut props = PropertyMap::new();
        props.insert("title".into(), Value::Text(format!("post-{i}")));
        props.insert("score".into(), Value::Int64((i as i64 * 11) % 100));
        spec = spec.with_node(KernelBootstrapNodeSpec::from_parts(&["Post"], &props));
    }
    for p in 0..persons {
        for k in 0..fanout {
            let post_idx = base + p * fanout + k;
            spec = spec.with_edge(KernelBootstrapEdgeSpec::from_parts(
                p,
                post_idx,
                Some("WROTE"),
                &PropertyMap::new(),
            ));
        }
    }
    spec
}

fn seed_one_user_uid(uid: &str) -> KernelBootstrapGraphSpec {
    let mut props = PropertyMap::new();
    props.insert("uid".into(), Value::Text(uid.into()));
    KernelBootstrapGraphSpec::empty()
        .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &props))
}

/// Two `User` nodes and one `KNOWS` edge `u0 → u1` (for edge-property / `DELETE` edge benches).
fn seed_two_users_knows_edge() -> KernelBootstrapGraphSpec {
    let mut u0 = PropertyMap::new();
    u0.insert("uid".into(), Value::Text("u0".into()));
    let mut u1 = PropertyMap::new();
    u1.insert("uid".into(), Value::Text("u1".into()));
    KernelBootstrapGraphSpec::empty()
        .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &u0))
        .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &u1))
        .with_edge(KernelBootstrapEdgeSpec::from_parts(
            0,
            1,
            Some("KNOWS"),
            &PropertyMap::new(),
        ))
}

/// `u0 → u1` plus isolated `u2` (no incident edges) so plain `DELETE` on `u2` is valid.
fn seed_two_users_knows_plus_isolated_leaf() -> KernelBootstrapGraphSpec {
    let mut u0 = PropertyMap::new();
    u0.insert("uid".into(), Value::Text("u0".into()));
    let mut u1 = PropertyMap::new();
    u1.insert("uid".into(), Value::Text("u1".into()));
    let mut u2 = PropertyMap::new();
    u2.insert("uid".into(), Value::Text("u2".into()));
    KernelBootstrapGraphSpec::empty()
        .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &u0))
        .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &u1))
        .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &u2))
        .with_edge(KernelBootstrapEdgeSpec::from_parts(
            0,
            1,
            Some("KNOWS"),
            &PropertyMap::new(),
        ))
}

/// Linear chain `u0 → u1 → u2` for `DETACH DELETE` on the middle vertex.
fn seed_three_users_path() -> KernelBootstrapGraphSpec {
    let mut u0 = PropertyMap::new();
    u0.insert("uid".into(), Value::Text("u0".into()));
    let mut u1 = PropertyMap::new();
    u1.insert("uid".into(), Value::Text("u1".into()));
    let mut u2 = PropertyMap::new();
    u2.insert("uid".into(), Value::Text("u2".into()));
    KernelBootstrapGraphSpec::empty()
        .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &u0))
        .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &u1))
        .with_node(KernelBootstrapNodeSpec::from_parts(&["User"], &u2))
        .with_edge(KernelBootstrapEdgeSpec::from_parts(
            0,
            1,
            Some("KNOWS"),
            &PropertyMap::new(),
        ))
        .with_edge(KernelBootstrapEdgeSpec::from_parts(
            1,
            2,
            Some("KNOWS"),
            &PropertyMap::new(),
        ))
}

// --- Macros: warm PMA once, measure query/body only ---

macro_rules! bench_overlay_query {
    ($spec:expr, $q:expr) => {{
        let spec = $spec;
        wipe_for_bench_iteration();
        let mut harness = GraphPmaKernelHarness::bootstrap_empty(BenchMemory::default())
            .expect("bootstrap_empty");
        let (mut graph, _) = harness.bind_overlay_with_graph(&spec).expect("seed");
        let ctx = execution_context();
        canbench_rs::bench_fn(|| {
            let _ = execute_query_str(&mut *graph, $q, None, &ctx).expect("execute");
        })
    }};
}

macro_rules! bench_overlay_query_ctx {
    ($spec:expr, $ctx:expr, $q:expr) => {{
        let spec = $spec;
        wipe_for_bench_iteration();
        let mut harness = GraphPmaKernelHarness::bootstrap_empty(BenchMemory::default())
            .expect("bootstrap_empty");
        let (mut graph, _) = harness.bind_overlay_with_graph(&spec).expect("seed");
        let ctx = $ctx;
        canbench_rs::bench_fn(|| {
            let _ = execute_query_str(&mut *graph, $q, None, &ctx).expect("execute");
        })
    }};
}

/// Mutation benches intentionally warm the graph once before `bench_fn` and
/// then measure repeated block execution on that same bound graph.
///
/// `canbench_rs::bench_fn` does not offer a non-measured setup hook per sample,
/// so trying to recreate/seed the graph inside the measured closure ends up
/// benchmarking bootstrap + writeback rather than the mutation itself.
macro_rules! bench_overlay_block {
    ($spec:expr, $block:expr) => {{
        let spec = $spec;
        wipe_for_bench_iteration();
        let mut harness = GraphPmaKernelHarness::bootstrap_empty(BenchMemory::default())
            .expect("bootstrap_empty");
        let (mut graph, _) = harness.bind_overlay_with_graph(&spec).expect("seed");
        let ctx = execution_context();
        canbench_rs::bench_fn(|| {
            let _ = execute_block_str(&mut *graph, $block, None, &ctx).expect("execute");
        })
    }};
}

// --- Planner-only ---

#[bench(raw)]
fn bench_gql_planner_match_join_where_order_limit() -> canbench_rs::BenchResult {
    let q = parse_query(
        "MATCH (a:Person)-[:KNOWS]->(b:Person) \
         WHERE a.age >= 21 AND b.region = 'east' AND a.score > b.score \
         RETURN a.name, b.name, a.score ORDER BY a.score DESC LIMIT 25",
    )
    .expect("parse query");
    canbench_rs::bench_fn(|| {
        let _out = build_plan_output(&q, None).expect("plan");
    })
}

#[bench(raw)]
fn bench_gql_planner_deep_chain_where_order_limit() -> canbench_rs::BenchResult {
    let q = parse_query(
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person)-[:KNOWS]->(d:Person) \
         WHERE a.age >= 18 AND b.score > 0 AND c.region = d.region AND d.score < a.score \
         RETURN a.uid, b.uid, c.uid, d.uid ORDER BY a.score DESC LIMIT 40",
    )
    .expect("parse query");
    canbench_rs::bench_fn(|| {
        let _out = build_plan_output(&q, None).expect("plan");
    })
}

#[bench(raw)]
fn bench_gql_planner_aggregates_group_by_multi() -> canbench_rs::BenchResult {
    let q = parse_query(
        "MATCH (n:Person) \
         RETURN n.bucket, COUNT(*) AS c, AVG(n.score) AS a \
         GROUP BY n.bucket \
         ORDER BY c DESC",
    )
    .expect("parse query");
    canbench_rs::bench_fn(|| {
        let _out = build_plan_output(&q, None).expect("plan");
    })
}

// --- Scale: ring sizes ---

#[bench(raw)]
fn bench_gql_execute_label_scan_ring_64() -> canbench_rs::BenchResult {
    bench_overlay_query!(seed_person_ring_spec(64), "MATCH (n:Person) RETURN n.uid")
}

#[bench(raw)]
fn bench_gql_execute_label_scan_ring_256() -> canbench_rs::BenchResult {
    bench_overlay_query!(seed_person_ring_spec(256), "MATCH (n:Person) RETURN n.uid")
}

#[bench(raw)]
fn bench_gql_execute_label_scan_ring_128() -> canbench_rs::BenchResult {
    bench_overlay_query!(seed_person_ring_spec(128), "MATCH (n:Person) RETURN n.uid")
}

#[bench(raw)]
fn bench_gql_execute_property_filter_ring_64() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(64),
        "MATCH (n:Person) WHERE n.age > 35 RETURN n.uid"
    )
}

#[bench(raw)]
fn bench_gql_execute_property_filter_ring_256() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(256),
        "MATCH (n:Person) WHERE n.age > 35 RETURN n.uid"
    )
}

#[bench(raw)]
fn bench_gql_execute_expand_one_hop_ring_64() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(64),
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.uid, b.uid"
    )
}

#[bench(raw)]
fn bench_gql_execute_expand_one_hop_ring_256() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(256),
        "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.uid, b.uid"
    )
}

/// Two-hop expand on a medium ring (many paths).
#[bench(raw)]
fn bench_gql_execute_expand_two_hop_ring_128() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(128),
        "MATCH (a:Person)-[:KNOWS]->(b:Person)-[:KNOWS]->(c:Person) RETURN a.uid, b.uid, c.uid"
    )
}

#[bench(raw)]
fn bench_gql_execute_count_star_ring_64() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(64),
        "MATCH (n:Person) RETURN COUNT(*)"
    )
}

#[bench(raw)]
fn bench_gql_execute_count_star_ring_256() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(256),
        "MATCH (n:Person) RETURN COUNT(*)"
    )
}

#[bench(raw)]
fn bench_gql_execute_count_star_ring_128() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(128),
        "MATCH (n:Person) RETURN COUNT(*)"
    )
}

#[bench(raw)]
fn bench_gql_execute_order_by_score_limit_ring_256() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(256),
        "MATCH (n:Person) RETURN n.uid, n.score ORDER BY n.score DESC LIMIT 25"
    )
}

#[bench(raw)]
fn bench_gql_execute_distinct_region_ring_256() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(256),
        "MATCH (n:Person) RETURN DISTINCT n.region"
    )
}

#[bench(raw)]
fn bench_gql_execute_param_filter_ring_256() -> canbench_rs::BenchResult {
    let mut p = BTreeMap::new();
    p.insert("min_age".into(), Value::Int64(38));
    bench_overlay_query_ctx!(
        seed_person_ring_spec(256),
        execution_context_with_params(p),
        "MATCH (n:Person) WHERE n.age > $min_age RETURN n.uid"
    )
}

#[bench(raw)]
fn bench_gql_execute_group_by_bucket_counts_ring_256() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_with_bucket_spec(256),
        "MATCH (n:Person) RETURN n.bucket, COUNT(*) AS c GROUP BY n.bucket ORDER BY c DESC"
    )
}

// --- Aggregates on Stat nodes ---

#[bench(raw)]
fn bench_gql_execute_avg_x_32() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_avg_nodes_spec(32),
        "MATCH (n:Stat) RETURN AVG(n.x) AS a"
    )
}

#[bench(raw)]
fn bench_gql_execute_avg_x_256() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_avg_nodes_spec(256),
        "MATCH (n:Stat) RETURN AVG(n.x) AS a"
    )
}

#[bench(raw)]
fn bench_gql_execute_sum_min_max_stats_128() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_avg_nodes_spec(128),
        "MATCH (n:Stat) RETURN SUM(n.x) AS s, MIN(n.x) AS lo, MAX(n.x) AS hi"
    )
}

#[bench(raw)]
fn bench_gql_execute_count_distinct_x_mod_stats_128() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_avg_nodes_spec(128),
        "MATCH (n:Stat) RETURN COUNT(DISTINCT n.x) AS d"
    )
}

// --- Shortest path ---

#[bench(raw)]
fn bench_gql_execute_any_shortest_path() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_shortest_path_spec(),
        "MATCH ANY SHORTEST (a:U WHERE a.name = 'a')-[:KNOWS]->{1,3}(b:U) RETURN a, b"
    )
}

// --- Procedures ---

#[bench(raw)]
fn bench_gql_execute_call_db_labels_ring_32() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(32),
        "CALL db.labels() YIELD lbl RETURN lbl"
    )
}

#[bench(raw)]
fn bench_gql_execute_call_db_property_keys_ring_128() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(128),
        "CALL db.propertyKeys() YIELD propertyKey RETURN propertyKey"
    )
}

#[bench(raw)]
fn bench_gql_execute_call_db_relationship_types_ring_128() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_ring_spec(128),
        "CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType"
    )
}

// --- Star / fan-out pattern ---

#[bench(raw)]
fn bench_gql_execute_expand_post_fanout_persons_32_fanout_6() -> canbench_rs::BenchResult {
    bench_overlay_query!(
        seed_person_post_star_spec(32, 6),
        "MATCH (u:Person)-[:WROTE]->(p:Post) RETURN u.uid, p.title LIMIT 200"
    )
}

// --- Mutations (same semantics as before) ---

const INSERT_PERSON_MINIMAL: &str = "INSERT (:Person {name: 'canbench'})";

/// Parse-only cost for a typical `INSERT` block (no graph / plan / execute).
#[bench(raw)]
fn bench_gql_block_parse_insert_person() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let _ = parse_block(INSERT_PERSON_MINIMAL).expect("parse");
    })
}

/// Parse + plan for a typical `INSERT` block (no graph / execute).
#[bench(raw)]
fn bench_gql_block_plan_insert_person() -> canbench_rs::BenchResult {
    canbench_rs::bench_fn(|| {
        let block = parse_block(INSERT_PERSON_MINIMAL).expect("parse");
        let _ = plan_block(&block, None).expect("plan");
    })
}

#[bench(raw)]
fn bench_gql_execute_block_set_property() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_one_user_uid("u1"),
        "MATCH (n:User) SET n.name = 'updated' RETURN n"
    )
}

/// One `INSERT` per canbench sample on the **same** bound graph and stable backing (IC-style).
/// Graph and property-index state grow monotonically across samples, so instruction totals are a
/// **stress / growth** signal — not a fixed “single insert on empty DB” cost (contrast query benches).
#[bench(raw)]
fn bench_gql_execute_block_insert_person() -> canbench_rs::BenchResult {
    bench_overlay_block!(KernelBootstrapGraphSpec::empty(), INSERT_PERSON_MINIMAL)
}

/// `INSERT` with several scalar properties (index / property-store pressure vs minimal insert).
#[bench(raw)]
fn bench_gql_execute_block_insert_person_multi_prop() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        KernelBootstrapGraphSpec::empty(),
        "INSERT (:Person {uid: 'u0', name: 'canbench', age: 40, region: 'east'})"
    )
}

/// Node with labels only (no inline property map).
#[bench(raw)]
fn bench_gql_execute_block_insert_person_bare() -> canbench_rs::BenchResult {
    bench_overlay_block!(KernelBootstrapGraphSpec::empty(), "INSERT (:Person)")
}

/// Two property assignments in one `SET` clause.
#[bench(raw)]
fn bench_gql_execute_block_set_two_properties() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_one_user_uid("u1"),
        "MATCH (n:User) SET n.name = 'alice', n.role = 'admin' RETURN n"
    )
}

/// Remove one property from a matched node.
#[bench(raw)]
fn bench_gql_execute_block_remove_property() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_one_user_uid("u1"),
        "MATCH (n:User) REMOVE n.uid RETURN n"
    )
}

/// Set a property on a matched relationship (stress on edge property store + edge PIDX).
#[bench(raw)]
fn bench_gql_execute_block_set_edge_property() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_two_users_knows_edge(),
        "MATCH (a:User {uid: 'u0'})-[e:KNOWS]->(b:User {uid: 'u1'}) SET e.weight = 1 RETURN e"
    )
}

/// Two statements in one block (`NEXT`): `SET` then `REMOVE` on the same graph (see PMA `calls`).
#[bench(raw)]
fn bench_gql_execute_block_set_then_remove_next() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_one_user_uid("u1"),
        "MATCH (n:User {uid: 'u1'}) SET n.name = 'alice', n.role = 'admin' RETURN n \
         NEXT MATCH (n:User {uid: 'u1'}) REMOVE n.role RETURN n"
    )
}

/// Remove the only relationship on the seeded graph; later samples match zero rows (cost shifts).
#[bench(raw)]
fn bench_gql_execute_block_delete_edge() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_two_users_knows_edge(),
        "MATCH (a:User {uid: 'u0'})-[e:KNOWS]->(b:User {uid: 'u1'}) DELETE e"
    )
}

/// Delete an isolated `User` (`u2` has no edges); first sample removes it, later samples match nothing.
#[bench(raw)]
fn bench_gql_execute_block_delete_vertex() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_two_users_knows_plus_isolated_leaf(),
        "MATCH (n:User {uid: 'u2'}) DELETE n"
    )
}

/// `DETACH DELETE` the middle vertex of a chain; first sample is structural DML, later samples idle.
#[bench(raw)]
fn bench_gql_execute_block_detach_delete_vertex() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_three_users_path(),
        "MATCH (n:User {uid: 'u1'}) DETACH DELETE n"
    )
}

/// Person/Post star with a high-outdegree `Person` hub for `DETACH DELETE`.
///
/// We intentionally reuse an already-stable bootstrap shape (`seed_person_ring_spec`) so the
/// benchmark consistently executes on canbench wasm targets.
fn seed_person_chain_detach_delete_high_degree() -> KernelBootstrapGraphSpec {
    seed_person_ring_spec(256)
}

/// `DETACH DELETE` the high-degree "middle" node of a `Person` chain; first sample does
/// structural DML, later samples match no rows.
pub(crate) fn bench_gql_execute_block_bulk_detach_delete_impl() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_person_chain_detach_delete_high_degree(),
        "MATCH (n:Person {uid: 'u16'}) DETACH DELETE n"
    )
}

/// `NEXT`: two `SET` clauses (two planner statements / potential flush boundaries; graph growth stress on `role`).
#[bench(raw)]
fn bench_gql_execute_block_set_twice_next() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_one_user_uid("u1"),
        "MATCH (n:User {uid: 'u1'}) SET n.name = 'a' RETURN n \
         NEXT MATCH (n:User {uid: 'u1'}) SET n.role = 'b' RETURN n"
    )
}

/// Three new properties on one `SET` (executor `SetProperties` + single flush shape vs 1-prop / 5-prop).
#[bench(raw)]
fn bench_gql_execute_block_set_three_properties() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_one_user_uid("u1"),
        "MATCH (n:User) SET n.p1 = 1, n.p2 = 2, n.p3 = 3 RETURN n"
    )
}

/// Five new properties on one `SET` (growth stress; compare `gql_exec_set_property_item` `calls`).
#[bench(raw)]
fn bench_gql_execute_block_set_five_properties() -> canbench_rs::BenchResult {
    bench_overlay_block!(
        seed_one_user_uid("u1"),
        "MATCH (n:User) SET n.p1 = 1, n.p2 = 2, n.p3 = 3, n.p4 = 4, n.p5 = 5 RETURN n"
    )
}
