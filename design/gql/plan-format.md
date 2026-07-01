# Physical plan format

Last updated: 2026-07-01
Anchor timestamp: 2026-07-01 07:07:02 UTC +0000

## Purpose

Define the **contract** between `gleaph-gql-planner` and `gleaph-graph` executor: what a `PhysicalPlan` contains and what the executor may assume.

## Non-goals

- Wire encoding byte layout (see `gql-planner/src/wire/`).
- Cost model formulas (`crates/gql-planner/src/cost.rs`).

## Core types

| Type | Crate | Role |
|------|-------|------|
| `PhysicalPlan` | `gql-planner` | `ops: Vec<PlanOp>` + metadata |
| `PlanOp` | `gql-planner` | One execution step |
| `PlanAnnotations` | `gql-planner` | Hints (CSE, live vars, etc.) |
| `BindingLayout` | `gql-planner` | Dense column order for variables |
| `ExecutePlanArgs` | `graph-kernel` | Router → graph transport |

Planner builds plans; graph decodes plan blobs and runs `execute_ops` (`crates/graph/src/plan/query/executor.rs`).

## Binding layout

Indexed plans attach `BindingLayout` to `PlanRow`:

- Variables map to slot indices for fast fork/merge.
- Spill map holds names outside the layout (rare paths).

Executor optimizations (`PlanRow::try_merge_skip_one`, `QueryArena` slot pool) assume compatible layouts between join inputs.

## PlanOp families

See [execution/operators.md](../execution/operators.md) for the full list. Grouped by role:

| Family | Examples | Executor module |
|--------|----------|-----------------|
| Scan | `NodeScan`, `IndexScan`, `EdgeIndexScan`, `ConditionalIndexScan` | `execute_*scan*` |
| Filter | `PropertyFilter`, `Filter` | expr evaluator |
| Traverse | `Expand`, `ExpandFilter`, `ShortestPath` | `execute_expand`, path search |
| Join | `HashJoin`, `CartesianProduct`, `WorstCaseOptimalJoin` | join helpers |
| GQL control | `Let`, `For`, `OptionalMatch`, `UseGraph` | control flow |
| Vector search | `Search` | `execute_ops` resolved-search join |
| Output | `Project`, `Sort`, `Limit`, `TopK`, `Aggregate`, `Materialize` | projection / agg |
| DML | `InsertVertex`, `SetProperties`, `DeleteVertex`, … | mutation executor |

## Router seed contract

When router supplies `seed_bindings_blob`:

- Graph **skips** the first anchor `IndexScan` for that variable.
- Binds listed **local** `VertexId`s on the target shard only.

Plan must be written so remaining ops are valid given seeded rows (planner + router `SeedProbe` agree on property/value).

## Router resolved-search contract

For a non-leading `PlanOp::Search` the Router supplies `ExecutePlanArgs.resolved_search_blob`:

- The blob is a per-shard `ResolvedSearchWire` produced by the Router from a single global vector top-k call (or from an invocation-local empty result when the filtered candidate set is empty).
- The wire carries the bound vertex variable name, the scalar output alias, and the finite, de-duplicated shard-local vertex hits with their user-visible scalar values.
- Shards with no local hit receive an explicit empty wire, not an absent field.
- The Graph executor decodes the wire, validates it against the `PlanOp::Search` binding/alias, and executes the operator as an inner join: input rows whose bound vertex matches a hit survive and get the scalar alias bound.
- A `PlanOp::Search` that reaches the Graph executor without a matching resolved relation fails closed with `PlanQueryError::UnsupportedOp`.
- The full non-leading plan is preserved even when the resolved relation is empty, so global aggregates and other downstream operators still run.

For a leading `NodeScan + Search` prefix the Router strips the prefix and dispatches the tail plan with row-shaped `seed_bindings_blob`; the Graph executor never sees a raw `PlanOp::Search`.


### Filtered search contract

For a leading `NodeScan + Search` or a non-leading `SEARCH` after a bound vertex with an accepted
`WHERE` predicate:

- The planner carries the filter expression in `PlanOp::Search` after structural validation: either
  exactly one equality comparison, one to eight `AND`-connected equality comparisons on distinct
  properties of the searched binding and a literal or parameter, exactly one range comparison
  (`<`, `<=`, `>`, `>=`) between a property of the searched binding and a literal or parameter,
  exactly two range comparisons on the same property of the searched binding where one arm is a
  lower bound (`>` or `>=`) and the other is an upper bound (`<` or `<=`), one to eight
  equality comparisons on distinct properties of the searched binding together with one or two
  range comparisons on the same property where one range arm is a lower bound and the other is an
  upper bound, with the range property distinct from every equality property, any number of
  `OR`-connected equality comparisons on the same property of the searched binding, any number of
  `OR`-connected same-binding equality comparisons where property names may repeat or differ, any
  number of `OR`-connected range comparisons on the same binding where each arm is a pure numeric
  range comparison (`<`, `<=`, `>`, `>=`) and no equality or nested logical operator appears, **or
  any number of `OR`-connected same-binding comparison predicates where each leaf is independently
  either an equality comparison or a one-sided numeric range comparison (`<`, `<=`, `>`, `>=`) and no
  nested logical operator or two-sided range disjunct appears. The arms may reference the same
  property or different properties; the property names may repeat or differ across arms and across
  comparison kinds**. Either operand order and any conjunct or disjunct order is accepted. The planner does not verify label,
  index coverage, or numeric-domain semantics, and does not enforce the Router's eight-arm
  disjunction execution bound.
- The Router resolves the searched label and every filter property to router-issued ids, proves an
  active vertex property index for the exact `(graph_id, label_id, property_id)` tuple in the
  named-index catalog for every arm, and validates each encoded size against
  `MAX_INDEX_VALUE_KEY_BYTES` before calling the index. For equality arms it encodes each comparison
  value with `gleaph_gql::value_to_index_key_bytes`. For a numeric range arm it derives a finite
  half-open encoded comparison-domain range with `gleaph_gql::numeric_range_bounds`, normalizing
  reversed operands by inverting the operator.
- The Router collects at most `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` (4096) distinct vertex subjects
  from the Property Index via paginated `lookup_equal_page` for one equality arm, the server-side
  `lookup_intersection_page` for two to eight equality arms, `lookup_range_page` with
  `PostingRangeRequest::Between { low, high }` for a numeric range arm,
  `lookup_range_intersection_page` for one to eight equality arms plus one or two same-property range arms on a
  distinct property (the two range arms are collapsed into one intersected encoded interval in
  Router before a single range-walk/equality-sieve stream), a sequential union of up to eight
  paginated `lookup_equal_page` streams for two to eight `OR`-connected same-property or cross-property
  equality arms (each arm resolves one `(graph_id, label_id, property_id)` tuple, each tuple must
  have an active index, one page in flight per source, per-source cursors starting from `None`,
  global deduplication, label filtering before counting, and the 4096 candidate bound), a
  sequential union of up to eight paginated `lookup_range_page` streams for two to eight `OR`-connected
  same-property or cross-property range arms (each range arm resolves its own `(graph_id, label_id,
  property_id)` tuple and its own finite half-open encoded interval; empty arms are dropped, the
  remaining intervals are grouped by property id, sorted and merged into disjoint encoded intervals
  within each group, and each merged interval is walked per index source through `lookup_range_page`),
  **or a sequential union of up to eight paginated `lookup_equal_page` and/or `lookup_range_page`
  streams for two to eight `OR`-connected same-binding heterogeneous comparison arms. Each arm is
  independently classified as equality or range, resolves its own `(graph_id, label_id, property_id)`
  tuple, and is normalized like the pure equality and pure range paths. Equality sources are
  deduplicated by `(property_id, encoded_value)`, range intervals are grouped by property id and merged
  within each group, and the combined normalized sources are walked through the shared union collector.
  The same 4096 candidate bound, per-page label filtering, and global `(shard_id, vertex_id)`
  deduplication are enforced, and the collector stops at the 4097th distinct subject with an explicit
  error. Intervals are never merged across property ids because encoded numeric keys are
  property-specific, and equality/range sources are never merged with each other because they are
  semantically distinct postings lookups.**
- Deduplicating by
  `(shard_id, vertex_id)`, it stops as soon as a 4097th distinct subject is observed and returns an
  explicit error instead of truncating. Malformed postings are rejected. Nine or more syntactic
  disjunction arms are rejected with `InvalidArgument` before any Property Index call.
- If the candidate set is empty, the Router skips the vector canister. For a leading search it dispatches
  the stripped plan with an empty `SeedBindingsWire` to every live shard, preserving the leading-search
  global aggregate contract. For a non-leading search it keeps the full plan and attaches an explicit
  empty `ResolvedSearchWire` to every live shard, so Graph still executes the prefix and any global
  aggregate returns one zero row.
- Otherwise the Router forwards one `VectorSearchRequest` with `candidate_subjects = Some(allowlist)`
  to the vector canister. `candidate_subjects = None` retains the existing unrestricted search semantics;
  `Some([])` is an empty allowlist that returns no hits.
- For a non-leading filtered search the Router proves exactly one positive simple label for the searched
  binding from the top-level prefix. Accepted proofs are a labeled `NodeScan` for the binding, or a
  `PropertyFilter`/`ExpandFilter` containing `IS LABELED(binding, label, negated = false)` before the
  `PlanOp::Search`. This applies equally to equality and range filter arms. Zero labels, multiple distinct labels, negated labels, dynamic/nested label
  expressions, a label proof that appears after `SEARCH`, or a later prefix operator rebinding the
  searched variable are all rejected fail-closed.
- The vector canister validates the allowlist count, vertex-only subjects, and duplicates at its
  boundary, then ranks exactly over current live vector slots for those subjects.

## Federation interaction

- Planner emits normal `IndexScan` / `Expand`; no `ShardId` in `PlanOp`.
- Router may run the **same** plan on multiple shards.
- Executor introduces `RemoteVertex` at runtime ([federation/query-semantics.md](../federation/query-semantics.md)).

## Versioning

**Current practice:** Plan blobs are encoded with `gql-planner` wire format; router and graph must deploy compatible planner versions.

**Future:** Document explicit plan version field if wire breaking changes become frequent.

## Invariants (executor expectations)

1. Variables referenced in an op are bound by a prior scan, expand, join, or `Let`.
2. `PropertyFilter.stage` matches planner’s pipeline staging.
3. DML ops appear only on update path; router verifies `has_dml()` vs program classification.
4. `Expand.emit_edge_binding` controls whether edge variable is populated.

Violations → `PlanQueryError` at execution time.

## Related documents

- [layers.md](layers.md)
- [execution/pipeline.md](../execution/pipeline.md)
- [execution/operators.md](../execution/operators.md)
