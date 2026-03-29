# Gleaph Internal Ordered Source Optimization Plan

## Summary

This plan adds `ORDER BY ... LIMIT/OFFSET` optimization to Gleaph without changing the public GQL surface.

The design keeps ordered optimization as an internal feature. Version 1 only accelerates query shapes that are clearly tractable and safe:

- `gleaph_timestamp(edge)` ordering, using the existing PMA timestamp order
- Single-key ordering on a required vertex property defined by the active graph type

All other `ORDER BY` queries continue to use the current `Sort` or `top_k_rows()` fallback path.

The primary design goal is to reduce full scans and full sorts where possible while keeping stable-memory growth tightly controlled.

## Public Interface

No public GQL syntax, DDL, `SHOW INDEXES`, or canister API changes are introduced.

The optimization is implemented entirely through internal planner, executor, graph, and PMA changes:

- add an internal ordered-source execution path
- add an internal vertex ordered index substrate
- keep activation behind a private registry, not a user-facing API

## Implementation Plan

### 1. Add a common ordered-source planning path

Introduce a single planner/executor entry point that determines whether a query can be served by an ordered source instead of the current generic sort path.

Version 1 eligibility is intentionally narrow:

- `LIMIT` is required
- `OFFSET` is allowed
- `ORDER BY` must contain exactly one item
- the sort expression must be exactly `var.prop`, its alias, or `gleaph_timestamp(edge)`
- no `DISTINCT`
- no `GROUP BY`, `HAVING`, or aggregates
- no set operations
- no `OPTIONAL MATCH`
- no negation
- no variable-length path
- no `SHORTEST`
- no multiple `MATCH` clauses
- the ordered key must belong to the anchor variable

When the ordered path is used, the internal ordered key must include a stable entity-id tie-breaker so result ordering remains deterministic and consistent with current behavior.

### 2. Promote the existing edge-timestamp fast path

Use the existing PMA timestamp-ordered adjacency traversal as the first ordered source.

This path handles:

- `ORDER BY gleaph_timestamp(edge) DESC LIMIT k`

It requires no new stable-memory structure and should become the default internal ordered-source implementation for timeline-style queries.

### 3. Add an internal vertex ordered index

Add a label-scoped, key-only ordered index for selected vertex properties.

Each internal ordered index is scoped by:

- label
- property name
- declared `ValueType`

Supported scalar types in v1:

- `Bool`
- `Int`
- `Float`
- `Text`
- `Timestamp`
- `Bytes`
- `Date`
- `Time`
- `DateTime`
- `Duration`

The index key must use an order-preserving encoding based on the declared schema type.

To keep correctness simple in v1:

- only properties marked `required` in the active graph type are eligible
- only properties listed in a private internal registry are built
- if a write violates the declared property type, the mutation is rejected
- no null bucket is added in v1

### 4. Connect the generic vertex ordered path

Planner and executor should use the internal vertex ordered index only for eligible queries.

Execution model:

- read candidates from the ordered source in index order
- apply the normal residual filters, expansions, and projection
- stop once `OFFSET + LIMIT` qualifying rows have been produced

If any eligibility rule fails, execution must fall back to the current implementation.

### 5. Roll out conservatively

The repository default should ship with an empty private registry for generic vertex ordered indexes.

That means:

- edge timestamp ordering is always available
- generic vertex-property ordering is present in code but disabled by default
- benchmarks and tests can enable selected properties explicitly

Add internal metrics for:

- fast-path attempted
- fast-path used
- fallback reason
- ordered-index stable bytes
- rebuild/backfill time

## Test Plan

The implementation should include tests for:

- eligible queries using the ordered path instead of `Sort`
- exact result equivalence with the current implementation
- `LIMIT + OFFSET`
- ascending and descending order
- deterministic tie handling
- guaranteed fallback for unsupported shapes:
  - negation
  - aggregates
  - `OPTIONAL MATCH`
  - multiple `MATCH`
  - variable-length paths
- generic vertex ordered path disabled when no active graph type exists
- ordered-index build, restore, upgrade rebuild, and incremental maintenance
- mutation rejection on property type drift
- timeline benchmarks confirming no additional stable-memory growth
- synthetic vertex-property ranking benchmarks confirming full sort removal

## Assumptions

- Ordered optimization remains internal-only.
- Version 1 supports only single-key exact property ordering.
- Generic ordered optimization in v1 applies only to vertex properties.
- Generic ordered optimization in v1 requires an active graph type and `required` property definitions.
- Open-schema mode does not use the generic vertex ordered path.
- The private registry is empty by default, so initial rollout enables only the edge timestamp fast path.
