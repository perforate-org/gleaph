# Physical plan format

Last updated: 2026-08-24
Anchor timestamp: 2026-08-24 09:55:00 UTC +0000

## Purpose

Define the **contract** between `gleaph-gql-planner` and `gleaph-graph` executor: what a `PhysicalPlan` contains and what the executor may assume.

## Non-goals

- Wire encoding byte layout (see `gql-planner/src/wire/`).
- Cost model formulas (`crates/gql-planner/src/cost.rs`).

## Core types

| Type              | Crate          | Role                             |
| ----------------- | -------------- | -------------------------------- |
| `PhysicalPlan`    | `gql-planner`  | `ops: Vec<PlanOp>` + metadata    |
| `PlanOp`          | `gql-planner`  | One execution step               |
| `PlanAnnotations` | `gql-planner`  | Hints (CSE, live vars, etc.)     |
| `BindingLayout`   | `gql-planner`  | Dense column order for variables |
| `ExecutePlanArgs` | `graph-kernel` | Router → graph transport         |

Planner builds plans; graph decodes plan blobs and runs `execute_ops` (`crates/graph/src/plan/query/executor.rs`).

## Binding layout

Indexed plans attach `BindingLayout` to `PlanRow`:

- Variables map to slot indices for fast fork/merge.
- Spill map holds names outside the layout (rare paths).

Executor optimizations (`PlanRow::try_merge_skip_one`, `QueryArena` slot pool) assume compatible layouts between join inputs.

## PlanOp families

See [execution/operators.md](../execution/operators.md) for the full list. Grouped by role:

| Family        | Examples                                                         | Executor module                    |
| ------------- | ---------------------------------------------------------------- | ---------------------------------- |
| Scan          | `NodeScan`, `IndexScan`, `EdgeIndexScan`, `ConditionalIndexScan` | `execute_*scan*`                   |
| Filter        | `PropertyFilter`, `Filter`                                       | expr evaluator                     |
| Traverse      | `Expand`, `ExpandFilter`, `ShortestPath`                         | `execute_expand`, path search      |
| Join          | `HashJoin`, `CartesianProduct`, `WorstCaseOptimalJoin`           | join helpers                       |
| GQL control   | `Let`, `For`, `OptionalMatch`, `UseGraph`                        | control flow                       |
| Vector search | `Search`                                                         | `execute_ops` resolved-search join |
| Output        | `Project`, `Sort`, `Limit`, `TopK`, `Aggregate`, `Materialize`   | projection / agg                   |
| DML           | `InsertVertex`, `SetProperties`, `DeleteVertex`, …               | mutation executor                  |

### `ScanValue` bounds (implemented)

`IndexScan` / `EdgeIndexScan` bounds are [`ScanValue`](../../crates/gql-planner/src/plan.rs):
`Literal` and `Parameter` probe one encoded bound; `InList` is a **union of point probes** over one
indexed property (`WHERE v.prop IN (…)` vertex anchor, symmetrically `-[e:R WHERE e.prop IN (…)]->`
edge anchors). Elements resolve independently, so a missing parameter fails the scan closed instead
of narrowing the union; Null elements contribute no probe. The planner emits an `InList` bound only
for non-negated predicates whose every element is a scannable literal/parameter, and never rewrites
an AND of lists into the union. Residual retention follows each side's anchor convention: the vertex
scan keeps the original predicate in the residual `PropertyFilter`, while an edge fusion removes the
fused bound exactly like fused edge equality (per-element equality probes are index-exact). A
single-value equality bound always wins over an IN fusion on both sides. `cmp != Eq` with an `InList`
bound is fail-closed. The Router seeds the same union as `IndexAnchor::EqualUnion` with global
`(shard, vertex)` deduplication for vertices and `IndexAnchor::EdgeEqualUnion` with global edge
identity `(shard, owner, wire label, slot)` deduplication for edges; complete-prefix seed validation
and edge inline predicate contexts still reject `InList` bounds fail-closed.

### `ScanValue` text prefix bounds (implemented)

A non-negated `WHERE v.prop STARTS WITH <Text literal | $param>` over a range-indexed vertex
property fuses into one `IndexScan { value: TextPrefix(pattern), cmp: Eq }` anchor
(`AnchorSource::PropertyPrefix`). ENDS WITH / CONTAINS / ILIKE, negation, unindexed or non-range
properties never fuse. The executor lowers the resolved pattern through
`gleaph_gql::text_prefix_range_bounds` into a half-open encoded-key interval `[low, high)`:
TEXT keys are `[tag=6] + escaped(text) + [0, 0]`, so `low = stripped pattern key` and
`high = low + [0xFF]`. UTF-8 payloads contain neither `0x00` nor `0xFF`, so no stored TEXT key can
continue a prefix with `0xFF`; a plain byte-successor of the last pattern byte is rejected because
multi-byte characters can terminate a prefix. The empty pattern spans the whole TEXT domain
(`[6], [7]`), matching `STARTS WITH ''` being true for every non-Null text. BYTES-domain postings
never enter the interval (tag ordering). The interval is deduplicated on `(shard, vertex)`.
A missing parameter fails closed; a Null parameter binds no rows (`STARTS WITH NULL` is Unknown);
a non-TEXT resolved pattern falls back to the node scan + residual filter path like unsupported
range domains. The original predicate always stays in the residual `PropertyFilter`, so results
never depend on pushdown. The Router seeds the same anchor as `IndexAnchor::Prefix` carrying the
encoded pattern key; raw `TextPrefix` bounds reaching `resolve_scan_value` are malformed-plan
rejections.

**Edge symmetry (implemented):** the same contract applies to edges. A non-negated
`e.prop STARTS WITH` over a range-indexed edge property fuses after equality → IN → range in
`EdgeFilterFusion.indexed_prefix`, lowers through the leading-edge bound chain into
`PlanOp::EdgeIndexScan { value: TextPrefix, cmp: Eq }`, and keeps its predicate residual. The
executor intercepts TextPrefix before equality dispatch and reuses `lookup_edge_range_local`
(interval identical to the vertex derivation, no `(shard, vertex)` dedup needed — edge range
postings are already identity-deduplicated per physical namespace); a non-TEXT resolved pattern
falls back to the canonical EDGE_PROPERTIES superset instead of a node scan. The Router extracts
`IndexAnchor::EdgePrefix(EdgePrefixSeedProbe)` (pattern key + ADR 0012 wire-label subset via the
shared edge seed context) and executes it as one Between request per wire label. Leading-hop
eligibility (`has_indexed_edge_bound`) recognizes prefix bounds so first-hop deferral fires
symmetrically with equality/IN/range.

## Router seed contract

`ExecutePlanArgs.seed_bindings_blob` is an opaque Router-to-Graph relation transport. Three
contracts must be distinguished.

**Current implemented contract:**

- grouped `entries` bind local vertex/edge hits for one anchor variable;
- complete `rows` bind row-shaped relations used by supported leading `SEARCH` lowering;
- Graph hydrates existence/tombstone state and required labels, skips supported leading anchor
  operators, and executes the remaining read prefix; and
- `SeedAnchorSet` does not emit a partial seed when the supported leading prefix binds more than one
  variable. Such a plan executes without that Router seed or uses the current sequential fallback.

**ADR 0046 Phase 1/2 contract (implemented):**

- `SeedBindingsWire.complete_prefix_rows: bool` marks the `rows` as complete for the entire read
  prefix. When set, Graph executes the read prefix from the supplied rows while skipping the leading
  index/label scan operators; the skipped operators are re-validated against current canonical Graph
  state. Residual `PropertyFilter`s, joins, and Cartesian products run normally.
- The bulk path detects a multi-variable leading prefix, resolves per-item per-shard candidate domains,
  retains the candidate domains and materializes only the request-sized Cartesian-product chunk into
  complete `SeedRowWire` rows,
  and sets `complete_prefix_rows: true`.
  Chunk size is selected from the actual encoded request and response bounds; there is no fixed
  row-count ceiling.
- Multi-variable seeding requires every anchored variable to have at least one non-label equality
  anchor; otherwise the prefix falls back to Graph-local execution.
- Empty domains produce a zero-row complete-prefix relation, so the item reports zero matches without
  a separate Router short-circuit.

**Historical ADR 0046 full contract:**

- a versioned V2 seed relation carries one bounded candidate domain per independently anchored
  variable and is attached per bulk item, not copied from the first item's parameters;
- Router previously deduplicated identical lookup keys across a homogeneous GQL bulk group and
  persisted the exact per-item relation for deterministic replay;
- candidate domains are not complete authoritative match rows and do not enumerate an unbounded
  Cartesian product on the wire;
- Graph evaluates bound `NodeScan` / equality `IndexScan` / `IndexIntersection` semantics against
  current local labels and canonical properties instead of simply deleting those predicates;
- residual filters and joins retain ordinary physical-plan semantics; and
- checked candidate, product, encoded-payload, and instruction bounds fail closed without
  truncation.

**Current Graph update transport:**

- `ExecutePlanArgs` remains the scalar physical-plan update boundary.
- The internal simple independent-operation transport remains as `ExecutePlanBatchArgs`,
  `ExecutePlanBatchMode`, `ExecutePlanBatchResult`, and `execute_plan_update_batch` because
  `gql_mutate` uses it to execute already-resolved plan work. It is not a Router public endpoint or
  an application bulk-loading contract.
- The former typed/shared seed-envelope and bulk-plan variants are removed in place. `atomic_insert` and
  `bulk_load` dispatch ordered Graph mutation families owned by their Router lifecycles; they do not
  expose physical-plan aggregation as a client concept.
- Graph still revalidates canonical labels, properties, and edge endpoints at the owning write
  boundary. Router-resolved candidates are never authoritative canonical rows.

The physical plan remains the single source of predicate/join semantics. Gleaph-specific seed
lowering must not add shard, canister, constraint, or Property Index concepts to the generic planner.
Declared constraints may select an equivalent Router/Graph integration fast path, but observed data
uniqueness is not a plan contract.

## NEXT / YIELD binding contract

A `StatementBlock` chains statements with `NEXT`. Two boundary shapes are supported:

- **Explicit `YIELD`**: the prior statement emits only the yielded columns; downstream statements
  receive those bindings under their aliases.
- **Top-level DML**: `INSERT`, `SET`, `REMOVE`, and `DELETE` retain their input binding table.
  A following `NEXT` can therefore operate on the same matched elements, just as inline DML does
  within a linear query. This is required for an update followed by a Graph-owned operational
  `CALL` in one shard-local mutation plan.
- **No `YIELD`**: every typed graph binding (vertex, edge, path) that survived the previous
  statement remains in scope for the next statement. The planner must extend the boundary
  `Project`/`Materialize` with hidden columns for those bindings, and the executor must keep a
  plain variable column as the original typed `PlanBinding` rather than materializing it to
  `Value`. This is what lets a chained `INSERT (a)-[:L]->(b)` reuse matched vertices `a` and
  `b` as edge endpoints instead of creating new ones.

Value bindings and computed projections are not silently retained across a no-YIELD boundary; they
must be explicitly yielded if a downstream statement needs them.

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
   number of `OR`-connected range comparisons on the same binding where each arm is a pure one-sided
   range comparison (`<`, `<=`, `>`, `>=`) and no equality or nested logical operator appears, **or
   any number of `OR`-connected same-binding comparison predicates where each leaf is independently
   either an equality comparison or a one-sided range comparison (`<`, `<=`, `>`, `>=`) and no
   nested logical operator or two-sided range disjunct appears. The arms may reference the same
   property or different properties; the property names may repeat or differ across arms and across
   comparison kinds**. Either operand order and any conjunct or disjunct order is accepted. The planner does not verify label,
   index coverage, or range-domain semantics, and does not enforce the Router's eight-arm
   disjunction execution bound.
- The Router resolves the searched label and every filter property to router-issued ids, proves an
  active vertex property index for the exact `(graph_id, label_id, property_id)` tuple in the
  named-index catalog for every arm, and validates each encoded size against
  `MAX_INDEX_VALUE_KEY_BYTES` before calling the index. For equality arms it encodes each comparison
   value with `gleaph_gql::value_to_index_key_bytes`. For a range arm it derives a finite
   half-open encoded comparison-domain interval with `gleaph_gql::range_bounds`, normalizing
   reversed operands by inverting the operator; literals outside the supported comparison domains
   (NUMERIC, TEXT, BYTES, and TEMPORAL Date..ZonedTime) reject fail-closed.
- The Router collects at most `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` (4096) distinct vertex subjects
  from the Property Index via paginated `lookup_equal_page` for one equality arm, the server-side
  `lookup_intersection_page` for two to eight equality arms, `lookup_range_page` with
   `PostingRangeRequest::Between { low, high }` for a range arm,
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
   error. Intervals are never merged across property ids because encoded keys are
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

## Router resolved edge-label contract

`ExecutePlanArgs.label_table` carries `ResolvedEdgeLabel` entries for every edge label the plan may touch. Each entry includes:

- `name`, `id` — the canonical edge label identity.
- `ordering` — Router-derived `Unordered` or `Insertion` policy from the Graph Type edge
  declaration. `Insertion` is required for query `ORDER BY INSERTION(e)` and selects the ordered
  LARA insertion/batch/compaction path; `Unordered` is the default and permits tombstone reuse.
- `inline_property_profile` — physical byte width and encoding from Router stable state.
- `inline_schema: Option<ResolvedInlineSchema>` — Router-derived scalar-or-struct projection for this concrete edge label. `None` for `UnnamedProfile`; `Scalar { property_id }` for `InlineScalar`; `Struct { property_id, fields }` for `InlineStruct`, where each field carries its name, declaration-ordered byte offset, and exact scalar `EdgeInlinePropertyProfile`. Graph validates this projection but never persists or infers it.

Graph must treat `ordering`, `inline_schema`, and the inline profile as read-only Router-resolved
projections. It does not own the schema and must not infer or persist the policy, property identity,
or field layout. Mutation envelopes carry the same resolved label policy before the first canonical
edge write.

For a requested edge property:

- If the concrete label's `inline_schema` is `Scalar { property_id }` and the resolved property id matches, Graph decodes the edge inline property bytes strictly and returns the exact GQL scalar value. Malformed, missing, or unsupported inline property bytes fail closed; the sidecar property store is never consulted as fallback.
- If the concrete label's `inline_schema` is `Struct { property_id, fields }` and the resolved property id matches the top-level struct property, Graph validates the field layout (non-empty, unique names, non-overlapping offsets, field-width sum equals inline property byte width) and decodes the inline property bytes into a declaration-ordered GQL `Value::Record`. Accessing an unknown nested field returns `Value::Null`; a malformed projection or inline property bytes fails closed before sidecar fallback.
- Otherwise Graph falls back to the sidecar property store (`GraphStore::edge_property`), preserving existing non-inline behavior.

This rule is shared by expression evaluation, edge-record projection, shortest-path hop cost evaluation (`COST BY e.property`), and any downstream consumer that reads an edge property.

`PlanOp::ShortestPath` with `cost: ShortestPathCost::EdgeCostExpr { expr, .. }` contributes any properties read by `expr` to the plan's property-use metadata, so Router projection includes them for execution.

## Inline mutation contract

For `InsertEdge` and edge-target `SetProperties` / `SetProperties::AllProperties` / `RemoveProperties`,
Graph classifies evaluated assignments using the `ResolvedEdgeLabel.inline_schema` projection:

- For an `InlineScalar` schema, exactly one assignment for the property id is required. Missing, duplicate, `NULL`, overflowing, signedness-mismatched, non-finite, or malformed fixed-byte values fail closed before any storage write.
- For an `InlineStruct` schema, full and field-level edge mutation paths use the canonical inline property bytes and never write the top-level struct property to sidecar state. Leaf indexes use the Router-interned dotted field property identity and are maintained from the same bytes.
- The inline property is encoded into the exact fixed-width inline property bytes using one Graph-owned scalar
  codec shared with read and predicate paths.
- `InsertEdge` calls the existing inline-property-bytes-aware edge insert commits (`insert_*_edge_with_inline_property_bytes`)
  with the encoded bytes; non-inline assignments are applied as ordinary sidecar properties.
- `SET e.prop = val` for the inline property updates the inline property bytes through the existing mirrored
  inline property bytes update commit (`update_edge_inline_property_at_handle`). Non-matching properties continue to use
  `GraphStore::set_edge_property`.
- `SET e = { ... }` evaluates and resolves the complete record, rejects duplicates and missing
  inline propertys, encodes the inline field, removes all existing sidecar properties, updates the inline property bytes
  exactly once, and writes only the remaining sidecar assignments.
- `REMOVE e.prop` for the inline property is rejected until an absence representation exists. Removing
  non-inline sidecar properties keeps existing behavior.
- The inline property id is never written to the sidecar `EDGE_PROPERTIES` store or an index-maintenance
  queue.

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
- [ADR 0046: multi-variable candidate seed relations](../adr/0046-multi-variable-candidate-seed-relations.md)
