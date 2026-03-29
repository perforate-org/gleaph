# Future Roadmap

> Remaining work items for Gleaph's GQL engine and storage layer.
> All implementation waves (0–5), conformance tiers (T0–T6), and parameter alignment (P1–P5) are complete.
> This document captures only **unfinished** or **deferred** items.

---

## GQL Conformance Gaps

### §planner — Cost Model Calibration ✅
Benchmark C_scan, C_expand, C_filter on IC via canbench. Update `stats.rs`/`planner.rs`.
- Status: **Done.** Core benchmarks running (31 pass), constants re-calibrated (baseline 5,014 IC/row), dynamic filter selectivity from `TableStats`.

### §16.7 — Parenthesized Path Patterns ✅
`MATCH (a)((x)-[:E]->(y)){2,4}(b)` with quantified subpath repetition.
- Status: **Done (Phases 1–3).** AST `PatternElement::SubPath`, parser `((…)){n,m}`, executor `extend_subpath()` with frontier-based expansion. Trailing node pattern `(b)` for endpoint binding. 6 tests (3 parser + 3 executor).
- Phase 4 (path mode): **Done.** TRAIL/SIMPLE/ACYCLIC enforced across subpath repetition boundaries via internal path accumulation. 2 tests.
- Phase 5 (selectors): **Done.** `ANY SHORTEST` (→ `ShortestMode::One`) and `ALL PATHS` (no shortest filter) parser support. 6 tests (3 parser + 3 executor).
- **WARNING**: O(fan_out^(hops×max)) complexity. Keep quantifier ranges small on IC.

### §7 — Session Management
IC model incompatible. SESSION SET/RESET/CLOSE require stateful connections.
- Status: Not planned. Limited session-like semantics possible via continuation checkpoints.

### §8 — Transaction Management
IC model incompatible. START TRANSACTION/COMMIT/ROLLBACK require cross-message atomicity.
- Status: Not planned. Append-only mutation journaling is theoretically feasible but deferred.

---

## GQL Engine Improvements

### §18.9 Phase 3 — Error-Mode Type Checking ✅
Opt-in strict mode that rejects queries with provable type mismatches.
- Status: **Done.** `SET TYPE CHECK STRICT|WARNING` toggles mode. `SHOW SETTINGS` displays current mode. Strict mode returns errors instead of warnings for schema-aware type mismatches. Persisted across upgrades. 13 new tests (10 unit + 3 bridge).

### §20.8 — Edge Property Hints → Planner PropertyFilter
Parser/executor done. Edge-index pre-filter wired into `extend_hop()` — when edge property index exists, matching targets/sources are precomputed from the BTreeSet index and non-matching edges are skipped before loading edge records.
- Status: **Done.** Pre-filter via `edge_index_targets_for_src`/`edge_index_sources_for_dst` (outgoing + incoming). 2 tests.

---

## Schema & Introspection

### DESCRIBE GRAPH TYPE ✅
Introspection query for stored graph types, node types, edge types.
- Status: **Done.** `resolve_describe_graph_type()` returns 7-column table (kind, name, label, labels, from_types, to_types, properties). Node/edge labels, node types with property defs, edge types with endpoint constraints. 3 tests.

### GraphType Schema Extension ✅
Property/edge type validation, edge endpoint constraints, DESCRIBE introspection — all done.
- CONSTRAINT enforcement: **Done.** `CREATE CONSTRAINT name ON (:Label) ASSERT prop IS UNIQUE|NOT NULL`, `DROP CONSTRAINT name`, `SHOW CONSTRAINTS`. Validates existing data on creation. Enforces on INSERT/MERGE. 7 tests.
- See `design/category-d-d3-graph-type-schema.md` for details.

### Static Type System ✅
Pattern type annotations + schema-aware inference + error-mode + type union syntax + NOT NULL constraint propagation — all done.
- Type union syntax: `$x :: INT | TEXT | FLOAT` parameter annotations. Union-aware type checking (operations on union-typed values check all variants). 8 new tests.
- NOT NULL propagation: `Type::NonNull` wrapper, `PropertySchema` returns `(name, type, required)` tuples, IS NULL/IS NOT NULL on NOT NULL properties warns (`NullCheckOnNonNull`). 5 new tests.
- See `design/category-d-v4-static-type-system.md` for details.

---

## Optimizer & Index

### Physical Join Reordering
- Status: **Done.** Multi-clause reordering by selectivity (OPTIONAL MATCH ordering), `match_clause_order` annotation, executor wired to use reordered clause order.

### Extended LIMIT Pushdown ✅
- Status: **Done.** Top-k cost awareness in `estimate_cost()` (O(n log k) when ORDER BY + LIMIT). DISTINCT + LIMIT early termination in executor. Aggregation + LIMIT early group emission termination in both `project_aggregated_rows` and `project_aggregated_rows_fast` (when LIMIT present without ORDER BY or HAVING).

### Persisted Index Metadata
- Status: **Done.** Cost model calibrated with correct selectivity semantics:
  - `property_selectivity` stores cardinality ratio (`distinct/total`); `query_selectivity()` converts to matching-row fraction (`1/distinct`).
  - `should_use_index_scan()` uses calibrated constants (COST_INDEX_SEEK_FRACTION=2.18, COST_EXPAND_MULTIPLIER=6.19) with expansion cost for non-start anchor predicates.
  - `estimate_cost()` IndexScan arm uses calibrated per-row index seek cost.
  - `filter_selectivity_from_stats()` converts cardinality ratio to query selectivity.
  - CREATE INDEX immediately computes selectivity (no separate ANALYZE needed).
  - 3 cost model tests + 1 selectivity-on-create test.

### Stable Index Storage (Equality + Range)
- Status: **Done.** ABP B+ tree secondary index is production-wired for both equality and range indexes:
  - `build_abp_secondary_index()` writes both equality and range entries using key-prefix namespacing.
  - Always rebuilt from in-memory state on `persist_state_metadata()` (removed stale live-handle guard).
  - Auto-allocates region during persist if vertex indexes exist but no region allocated.
  - DROP INDEX triggers ABP rebuild to remove stale entries.
  - In-memory indexes (vertex + edge) backfilled during `restore_overlay_snapshot()`.
  - Live ABP handle reattached on restore for incremental mutation updates.
  - Range index survives upgrade: ABP `scan_vertices_range` provides data even when in-memory BTreeMap is empty.
  - Lifecycle tests: equality create → mutate → snapshot → restore → verify (1 test), range snapshot → restore → ABP scan (1 test).

### Extended IndexScan ✅
- Status: **Done.** Non-start anchor vertex equality works (WHERE-based IndexScan + reverse traversal). Edge property index pre-filter wired into `extend_hop()`. Edge-index-seeded queries implemented: `PlanOp::EdgeIndexScan` seeds from `scan_edges_by_property_eq` instead of vertex scan for `MATCH ()-[e]-() WHERE e.prop = val` or inline `{prop: val}`. Supports multi-chain forward extension. 5 tests.

---

## Continuation Execution — Remaining

### SET/REMOVE Continuation ✅
DELETE, SET, and REMOVE continuation all implemented. Uses flatten-then-apply pattern: Phase 1 pre-evaluates all operations into flat `MutationOp` lists (avoiding `Binding` serialization), Phase 2 applies with budget checking. `MutationCheckpoint` enum dispatches by variant.

### Inter-Canister Cycling for Heavy Mutations
Self-call cycling to reset instruction counter for very large mutations.
- Status: **Deferred.** IC DTS (Deterministic Time Slicing) provides 40B instructions per update call (8× the 5B assumed during initial design). The existing client-driven continuation pattern (`mutate_resumable` → `mutate_continue`) handles budget exhaustion gracefully. Self-call cycling adds complexity (concurrency guards, cycle costs, trap recovery) for marginal UX gain. Revisit only if users report single mutations exceeding 40B instructions.
