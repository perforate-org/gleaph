# Planner Extensions — Design Document

## Status

- Phase 4 (Direct Range IndexScan): **Implemented**
- Phase 5 (Parameter-Based IndexScan): **Implemented**
- Phase 6 (Compound Range Intersection): **Implemented**
- Phase 7 (Multi-Predicate Anchor Selection): **Implemented**
- Phase 8 (IS NOT NULL Pattern): **Subsumed** by Phase 5
- Phase 9 (LIMIT Pushdown for ConditionalIndexScan): **Implemented**
- Phase 10 (Join Order Execution): **Implemented** (label-based mid-chain anchor)
- Phase 11 (Cost-Based Candidate Ordering): **Implemented**

### Running Benchmarks

All benchmark functions are in `crates/graph/src/bench/mod.rs` (core benchmarks section).
Run with: `make bench` (all) or `cd bench/core && canbench '<pattern>'` (single).
Benchmark names use the prefix `bench_gql_` followed by the phase-specific pattern.
Full suite takes several hours due to IC instruction counting overhead.

## Validation Methodology

Each phase MUST be validated with canbench measurements before and after implementation.

### Process

1. **Before**: Add benchmark(s) exercising the target query pattern, run `make bench-persist` to capture baseline IC instruction counts
2. **Implement**: Make the planner/executor changes
3. **After**: Re-run `make bench-persist`, compare instruction counts
4. **Record**: Document before/after numbers and speedup ratio in the phase's section
5. **Calibrate**: If the improvement reveals new cost-model constants, update `stats.rs` accordingly

### Benchmark Design Rules

- Each benchmark uses `#[bench(raw)]` + `canbench_rs::bench_fn(|| { ... })`
- Setup (vertex/edge creation, index creation) happens **outside** `bench_fn` to isolate query cost
- Use realistic data volumes (100–500 vertices) matching existing core benchmarks
- Include both the "before" query (full scan + filter) and the "after" query (index scan) as separate benchmarks for A/B comparison
- For parameter-based queries, use `crate::gql_bridge::query_with_params()` (or `execute_with_params()`)
- Name benchmarks descriptively: `bench_gql_range_scan_filter_300` (before) vs `bench_gql_range_index_scan_300` (after)

### Example Benchmark Pair (Phase 4)

```rust
// BEFORE: full scan + filter (baseline)
#[bench(raw)]
fn bench_gql_range_filter_no_index_300() -> canbench_rs::BenchResult {
    init_state(512, 0).unwrap();
    setup_labeled_vertices(300, "Item");
    // No range index created
    canbench_rs::bench_fn(|| {
        let _ = std::hint::black_box(crate::gql_bridge::query(
            "MATCH (n:Item) WHERE n.score >= 200 RETURN n.id",
        ));
    })
}

// AFTER: range index scan
#[bench(raw)]
fn bench_gql_range_index_scan_300() -> canbench_rs::BenchResult {
    init_state(512, 0).unwrap();
    setup_labeled_vertices(300, "Item");
    crate::api::create_index(
        gleaph_types::EntityType::Vertex,
        "score".into(),
        gleaph_types::IndexType::Range,
    ).unwrap();
    canbench_rs::bench_fn(|| {
        let _ = std::hint::black_box(crate::gql_bridge::query(
            "MATCH (n:Item) WHERE n.score >= 200 RETURN n.id",
        ));
    })
}
```

### Results Template

Each phase section should include a results table after implementation:

```
| Benchmark | Before (IC instructions) | After (IC instructions) | Speedup |
|---|---|---|---|
| bench_gql_range_filter_300 | 1,234,567 | 345,678 | 3.6× |
```

---

## Overview

Current planner capabilities:
- **Anchor selection**: property-equality → label-cardinality → label-only → full-scan
- **Scan types**: IndexScan (equality, literal only), EdgeIndexScan (equality, literal only), ConditionalIndexScan (equality + range, parameter-based), NodeScan
- **Optimizations**: filter pushdown, LIMIT pushdown, greedy join ordering (annotation only), SHORTEST reverse-anchor

This document proposes extensions in priority order based on IC instruction budget impact.

## Query-Family Roadmap

The implemented phases below focus mainly on anchor selection and index-assisted scans. That remains useful, but it is not broad enough for the next class of performance problems.

Recent benchmark work showed that larger wins come from recognizing query families and selecting a different execution mode rather than only improving the scan anchor. In particular:

- grouped traversals should aggregate during traversal instead of materializing rows first
- ranked queries should treat `ORDER BY ... LIMIT k` as a planning property, not just a post-processing step
- path/tree queries need stronger pruning before expansion
- cyclic many-to-many patterns will eventually need a different join family entirely

The broader research background is summarized in [query-evaluation-research-notes.md](/Users/yota/dev/gleaph/design/query-evaluation-research-notes.md).

### New Optimization Families

| Family | Problem Shape | Primary Technique | Status |
|---|---|---|---|
| Factorized endpoint aggregate | `MATCH (a)-[]->(b) RETURN endpoint.prop, AGG(...)` | early aggregation / factorized execution | Partial prototype implemented for `COUNT(*)` |
| Semijoin-pruned path execution | acyclic/path `MATCH` with selective predicates | Yannakakis-style pruning | Not started |
| Top-k aware execution | `ORDER BY ... LIMIT k` over joins/aggregates | top-k pruning / ranked enumeration | Not started |
| Cyclic pattern execution | triangle / diamond / many-to-many motifs | worst-case optimal join family | Not started |
| Maintained analytic summaries | repeated degree / ranking queries | incremental maintenance | Not started |

### Near-Term Implementation Order

1. Expand factorized aggregate execution beyond the current `endpoint.prop, COUNT(*)` family
2. Add semijoin-style pruning for path/tree queries before broadening join machinery
3. Add top-k aware execution for ranked aggregate families
4. Introduce a cyclic-pattern operator family for many-to-many motifs
5. Add optional maintained summaries for repeated analytics

### Benchmarking Rule for New Families

Each new family should follow the same methodology as the existing phases:

1. Add one or more benchmarks that isolate the query family
2. Establish a persisted baseline before implementation
3. Implement only the narrowest operator family needed
4. Re-run canbench and record before/after instruction counts
5. Keep the generic executor as fallback and gate the new path with explicit shape checks

### Recommended First Family After Current Phases

The next family worth expanding is:

```gql
MATCH (a)-[e?]->(b)
RETURN endpoint.prop, AGG(...)
ORDER BY AGG(...) DESC
LIMIT k
```

Recommended sequence:

- `endpoint.prop, COUNT(*)` generalization across more safe shapes
- `endpoint.prop, SUM(gleaph_weight(e))`
- selected 2-hop partial aggregates without row materialization

This direction is a continuation of the recent benchmark wins and has a better risk/reward profile than jumping directly to a full worst-case-optimal join implementation.

---

## Phase 4: Direct Range IndexScan

### Problem

`WHERE u.age > 30` with a literal value never uses a range index. `should_use_index_scan()` only calls `equality_property_predicate()`, which requires `CmpOp::Eq`. Range indexes exist but are only reachable through ConditionalIndexScan (`$p IS NULL OR ...` pattern).

### Approach

Add `range_property_predicate()` parallel to `equality_property_predicate()`:

```rust
fn range_property_predicate(where_clause: &Expr) -> Option<(String, String, CmpOp, Value)> {
    // Returns (variable, property, op, literal_value) for >=, >, <=, <
}
```

Extend `should_use_index_scan()`:
1. Try equality predicate first (existing)
2. If no equality match, try range predicate against `stats.range_indexed_vertex_properties`
3. Cost comparison: `range_scan_cost` vs `scan_filter_cost`

New plan variant or extend `IndexScan`:

```rust
pub struct IndexScanInfo {
    pub variable: String,
    pub property: String,
    pub value: Value,
    pub cmp_op: IndexCmpOp,  // Eq | Ge | Gt | Le | Lt
}
```

### Executor Changes

Route `IndexCmpOp::Ge/Gt/Le/Lt` to `scan_vertices_by_property_range_auto()`.

### Benchmarks

| Benchmark | Query | Setup |
|---|---|---|
| `bench_gql_range_filter_no_index_300` | `MATCH (n:Item) WHERE n.score >= 200 RETURN n.id` | 300 vertices, no range index |
| `bench_gql_range_index_scan_300` | Same query | 300 vertices, range index on `score` |
| `bench_gql_range_filter_lt_no_index_300` | `MATCH (n:Item) WHERE n.score < 50 RETURN n.id` | 300 vertices, no range index |
| `bench_gql_range_index_scan_lt_300` | Same query | 300 vertices, range index on `score` |

Expected: index scan should show ~3–5× reduction when matching ~30% of rows (selectivity-dependent).

### Results

_To be filled after implementation._

### Complexity

Low — mirrors existing equality IndexScan path.

---

## Phase 5: Parameter-Based IndexScan

### Problem

`WHERE u.name = $name` (no IS NULL guard) always falls back to NodeScan + filter because `equality_property_predicate()` only matches `Expr::Literal`. This is the most common prepared-statement pattern but gets no index benefit.

### Approach

Extend `equality_property_predicate()` to also match `Expr::Parameter`:

```rust
// Currently returns Option<(var, prop, literal_value)>
// Extend to return Option<(var, prop, ValueSource)>
enum ValueSource {
    Literal(Value),
    Parameter(String),
}
```

At execution time, resolve the parameter from `QUERY_PARAMS`. If the parameter is NULL, the predicate evaluates to NULL (three-valued logic) — no rows match, so returning an empty result is correct.

This is simpler than ConditionalIndexScan because there's no fallback needed — NULL parameter = empty result.

### Interaction with ConditionalIndexScan

- `WHERE u.name = $name` → Parameter-based IndexScan (always uses index)
- `WHERE ($name IS NULL OR u.name = $name)` → ConditionalIndexScan (index or full scan)

Parameter-based IndexScan should take priority over ConditionalIndexScan since it's unconditional.

### Benchmarks

| Benchmark | Query | Setup |
|---|---|---|
| `bench_gql_param_eq_no_index_300` | `MATCH (n:Person) WHERE n.id = $id RETURN n.score` with `$id=42` | 300 vertices, no index |
| `bench_gql_param_eq_full_scan_300` | Same query | 300 vertices, equality index on `id` (but planner can't use it — baseline) |
| `bench_gql_param_eq_index_scan_300` | Same query after Phase 5 | 300 vertices, equality index on `id` (planner uses it) |
| `bench_gql_param_range_index_300` | `MATCH (n:Item) WHERE n.score >= $min RETURN n.id` with `$min=200` | 300 vertices, range index on `score` |

Expected: parameter-based IndexScan should match literal IndexScan performance (~`bench_gql_index_seek_1_of_100` levels).

### Results

_To be filled after implementation._

### Complexity

Low — extends existing predicate detection.

---

## Phase 6: Compound Range Intersection

### Problem

```gql
MATCH (u:User)
WHERE ($min IS NULL OR u.age >= $min)
  AND ($max IS NULL OR u.age <= $max)
RETURN u
```

Currently each candidate scans independently. If both `$min` and `$max` are non-NULL, only the first candidate is used. The second becomes a post-scan filter.

### Approach

Group ConditionalIndexScan candidates by `(variable, property)`. When two range candidates on the same property have complementary operators (one lower-bound, one upper-bound), merge them into a `CompoundRangeScan`:

```rust
pub struct CompoundRangeScanCandidate {
    pub variable: String,
    pub property: String,
    pub lower: Option<(ConditionalCmpOp, String)>,  // (Ge|Gt, param_name)
    pub upper: Option<(ConditionalCmpOp, String)>,  // (Le|Lt, param_name)
}
```

Executor: resolve both bounds at runtime, call a new `scan_vertices_by_property_range_between()` that does a single ABP B+ tree traversal with both bounds.

### PMA Changes

Add `scan_vertices_by_property_range_between(property, lower_bound, lower_op, upper_bound, upper_op)` that walks the B+ tree once with start/end keys derived from both bounds.

### Benchmarks

| Benchmark | Query | Setup |
|---|---|---|
| `bench_gql_compound_range_single_bound_300` | `WHERE ($min IS NULL OR n.score >= $min)` with `$min=100, $max=200` | 300 vertices, range index, only first bound used |
| `bench_gql_compound_range_both_bounds_300` | `WHERE ($min IS NULL OR n.score >= $min) AND ($max IS NULL OR n.score <= $max)` with both non-NULL | Same setup, both bounds used → single B+ tree traversal |
| `bench_gql_compound_range_wide_vs_narrow` | Same query, `$min=0, $max=300` (wide) vs `$min=100, $max=110` (narrow) | Validates that narrow ranges benefit proportionally more |

Expected: compound scan should show 1.5–3× improvement over single-bound scan when the intersection is significantly smaller than either bound alone.

### Results

_To be filled after implementation._

### Complexity

Medium — requires grouping logic in planner and a new scan method in PMA.

---

## Phase 7: Multi-Predicate Anchor Selection

### Problem

`equality_predicate_anchor()` returns the **first** equality predicate found in the WHERE clause. It doesn't compare multiple candidates by selectivity.

```gql
MATCH (u:User)
WHERE u.country = "JP" AND u.email = "alice@example.com"
```

If `country` appears before `email` in the AST, the planner picks `country` (low selectivity) over `email` (high selectivity).

### Approach

Collect **all** equality predicates from the WHERE clause. For each, compute estimated selectivity from `TableStats.property_selectivity`. Pick the one with lowest estimated matching rows:

```rust
fn best_equality_predicate(
    where_clause: &Expr,
    stats: &TableStats,
    pattern_vars: &HashSet<String>,
) -> Option<(String, String, Value, f64)> {
    // Returns (var, prop, value, selectivity) with lowest selectivity
}
```

Fallback: if no selectivity data available, prefer indexed properties over non-indexed, then first-found.

### Benchmarks

| Benchmark | Query | Setup |
|---|---|---|
| `bench_gql_multi_pred_low_sel_first_300` | `WHERE n.country = "JP" AND n.email = "u42@test.com"` | 300 vertices, both indexed. `country` first in AST (low selectivity ~30%), `email` second (unique) |
| `bench_gql_multi_pred_high_sel_first_300` | `WHERE n.email = "u42@test.com" AND n.country = "JP"` | Same setup but `email` first (baseline — already optimal) |

Expected: after Phase 7, both queries should pick `email` as anchor regardless of AST order, matching the performance of the optimal case.

### Results

_To be filled after implementation._

### Complexity

Low — refactors existing `walk_pair()` to collect all matches.

---

## Phase 8: IS NOT NULL Pattern Detection

### Problem

`$p IS NOT NULL AND v.prop = $p` is semantically equivalent to the IS NULL OR pattern when the parameter is non-NULL. This is a natural way to write "only filter when parameter is provided" in some coding styles.

### Approach

Extend `detect_optional_filters()` to also detect AND nodes where one branch is `$param IS NOT NULL` and the other is `var.prop <op> $param`:

```
AND
├── $param IS NOT NULL
└── var.prop <op> $param
```

Semantics: when `$param IS NULL`, the AND short-circuits to FALSE — no rows match (not a full scan fallback). This is different from the IS NULL OR pattern. Two options:

1. **Treat as unconditional IndexScan** (Phase 5 covers this — `WHERE v.prop = $param` already handles NULL correctly)
2. **Skip** — Phase 5's parameter-based IndexScan subsumes this pattern

**Recommendation**: Phase 5 makes this unnecessary. If the user writes `$p IS NOT NULL AND v.prop = $p`, the `v.prop = $p` part alone is sufficient for Phase 5's parameter-based IndexScan. The IS NOT NULL check is redundant.

### Complexity

N/A — subsumed by Phase 5.

---

## Phase 9: LIMIT Pushdown for ConditionalIndexScan

### Problem

`execute_conditional_index_plan_query()` passes `None` as `reverse_max_rows` to `reverse_traverse_to_start()`, while `execute_index_plan_query()` passes the actual LIMIT value. This means ConditionalIndexScan doesn't benefit from early termination during reverse traversal.

### Approach

Mirror the LIMIT pushdown logic from `execute_index_plan_query()`:

```rust
let reverse_max_rows = if plan.annotations.limit_pushdown_applied {
    None // already handled by Limit op
} else {
    with_limit // from WITH clause
};
```

### Benchmarks

| Benchmark | Query | Setup |
|---|---|---|
| `bench_gql_conditional_limit_reverse_200` | `MATCH (a)-[]->(b:User) WHERE ($name IS NULL OR b.name = $name) RETURN a.id LIMIT 5` with `$name="user_50"` | 200 users, 500 edges, equality index on `name`. ConditionalIndexScan on non-start anchor with LIMIT |

Expected: LIMIT pushdown should reduce IC instructions proportionally to `total_rows / limit` when most rows are filtered by reverse traversal.

### Results

_To be filled after implementation._

### Complexity

Trivial — one-line change.

---

## Phase 10: Join Order Execution (Label-Based Mid-Chain Anchor)

### Problem

`choose_anchor()` already picks the lowest-cardinality label node (including mid-chain nodes) via `lowest_label_cardinality_anchor`. However, only the IndexScan fast path used non-start anchors. The general NodeScan path always fell through to `execute_query` which starts from the pattern's start node, ignoring the anchor.

### Approach

Added `execute_label_anchor_plan_query()` — a label-anchor fast path in `execute_plan_with_limits_and_hasher` that handles `NodeScan` with a non-start `chosen_anchor`:

1. Checks if `chosen_anchor` is a non-start node in the first MATCH clause
2. Uses `initial_candidates` on the anchor node (label scan) for anchor vertices
3. Calls `reverse_traverse_to_start` to walk backward from anchor to start
4. Forward-expands via `execute_query_match_entries_from_seed_rows` for remaining hops/clauses
5. Reuses the full projection/sort/limit/distinct pipeline from the IndexScan fast path

Guards: requires all hops to be `Fixed(1)` length, non-optional, non-shortest.

### Benchmarks

| Benchmark | Query | Setup |
|---|---|---|
| `bench_gql_join_order_label_anchor_300` | `MATCH (a:Common)-[:E]->(b:Rare)-[:E]->(c:Common)` | 200 Common, 20 Rare, edges forming chains. Planner picks b:Rare as anchor (card=20 vs 200) |
| `bench_gql_join_order_manual_rare_start_300` | `MATCH (b:Rare)<-[:E]-(a:Common) MATCH (b)-[:E]->(c:Common)` | Same graph, manually rewritten to start from Rare. Baseline. |

### Results

| Benchmark | Instructions | Notes |
|---|---|---|
| `bench_gql_join_order_label_anchor_300` | 292.62K | Automatic: planner picks b:Rare, reverse-traverses to a:Common |
| `bench_gql_join_order_manual_rare_start_300` | 148.81K | Manual: query rewritten to start from Rare (baseline) |

The label-anchor path is ~2× the manual rewrite due to reverse-traverse overhead, but both are far better than starting from 200 Common vertices.

### Tests

- `label_anchor_mid_chain_reverse_traverse`: 3-hop chain (Common→Rare→Common), verifies correct results via reverse-traverse
- `label_anchor_end_chain_covers_all`: 2-hop chain (Common→Rare), anchor covers all chains

---

## Phase 11: Cost-Based Candidate Ordering

### Problem

ConditionalIndexScan candidates are ordered by detection order (left-to-right in WHERE clause). When multiple candidates are non-NULL at runtime, the first one wins regardless of selectivity.

### Approach

After detecting candidates, sort by estimated selectivity from `TableStats.property_selectivity`:

```rust
candidates.sort_by(|a, b| {
    let sel_a = stats.property_selectivity
        .get(&format!("vertex:{}", a.property))
        .unwrap_or(&1.0);
    let sel_b = stats.property_selectivity
        .get(&format!("vertex:{}", b.property))
        .unwrap_or(&1.0);
    sel_a.partial_cmp(sel_b).unwrap()
});
```

Range candidates should be penalized slightly vs equality (range typically matches more rows).

### Benchmarks

| Benchmark | Query | Setup |
|---|---|---|
| `bench_gql_candidate_order_suboptimal_300` | `WHERE ($country IS NULL OR n.country = $country) AND ($email IS NULL OR n.email = $email)` with both non-NULL | 300 vertices, both indexed. `country` first (low sel ~30%), `email` second (unique). Only first candidate used |
| `bench_gql_candidate_order_optimal_300` | Same query, reversed order in WHERE clause | `email` first (baseline) |

Expected: after Phase 11, both should pick `email` candidate first regardless of WHERE order.

### Results

_To be filled after implementation._

### Complexity

Low — sorting step after candidate collection.

---

## Priority Summary

| Phase | Feature | Impact | Complexity | Depends On |
|---|---|---|---|---|
| 4 | Direct Range IndexScan | High | Low | — |
| 5 | Parameter-Based IndexScan | High | Low | — |
| 6 | Compound Range Intersection | Medium | Medium | Phase 3 |
| 7 | Multi-Predicate Anchor Selection | Medium | Low | — |
| 8 | IS NOT NULL Pattern | — | — | Subsumed by Phase 5 |
| 9 | LIMIT Pushdown (Conditional) | Low | Trivial | — |
| 10 | Join Order Execution | Medium | High | — |
| 11 | Cost-Based Candidate Ordering | Low | Low | — |

### Recommended Implementation Order

1. **Phase 9** (trivial fix, immediate benefit)
2. **Phase 4 + 5** (highest impact, low complexity, independent)
3. **Phase 7** (anchor quality improvement)
4. **Phase 6** (compound ranges)
5. **Phase 11** (candidate ordering)
6. **Phase 10** (join order — deferred, high complexity)
