# Gleaph Redesign: Query Execution Kernel

## Status

- Draft
- Companion to [gleaph-redesign-principles.md](/Users/yota/dev/gleaph/design/gleaph-redesign-principles.md)

## Purpose

This document describes the execution-side rules that should sit on top of the
redesigned storage kernel.

The primary goal is to make the generic path structurally efficient before
adding family-specific fast paths.

## Core Execution Principle

Query execution should consume graph structure in the same form the storage
kernel provides it:

- vertex first
- neighborhood next
- label filtering early
- properties only when needed

Execution should not routinely reconstruct graph structure from side channels.

## Kernel operations

The execution layer should be built from a small set of primitive operations.

## 1. Seed selection

```rust
fn seed_vertices(predicate: NodePredicate) -> VertexStream;
```

Responsibilities:

- evaluate node label and property constraints
- return local vertex ids

## 2. Forward expansion

```rust
fn expand_out(
    input: VertexStream,
    label: Option<LabelId>,
    edge_pred: Option<EdgePredicate>,
    node_pred: Option<NodePredicate>,
) -> VertexStream;
```

This must push exact label filtering into the neighborhood iterator.

## 3. Reverse expansion

```rust
fn expand_in(
    input: VertexStream,
    label: Option<LabelId>,
    edge_pred: Option<EdgePredicate>,
    node_pred: Option<NodePredicate>,
) -> VertexStream;
```

Incoming label filtering must also be early and structural.

## 4. Variable-length expansion

```rust
fn expand_var_len(
    input: VertexStream,
    dir: Direction,
    label_set: LabelSet,
    min_hops: u32,
    max_hops: u32,
) -> VertexStream;
```

This kernel must be designed for:

- small state objects
- early pruning
- cycle checks that do not dominate runtime
- minimal row cloning

## 5. Grouped aggregation

```rust
fn grouped_aggregate<K, A>(
    input: MatchStream,
    key_fn: impl Fn(&MatchRecord) -> K,
    accum: A,
) -> AggregateStream;
```

The design should encourage early aggregation instead of row explosion.

## Match representation

The generic execution layer should avoid allocating large row objects too early.

Suggested layers:

- `VertexStream`
- `EdgeMatchStream`
- `MatchRecord`

Only queries that genuinely need multi-binding row materialization should build
full records.

## Preferred executor collections

The executor should treat specialized integer collections as the default:

- use `roaring::RoaringBitmap` for `u32` membership sets such as visited,
  distinct-vertex, and seed sets
- use `rapidhash::fast::RapidHashMap` for temporary maps where collision
  resistance is not a requirement

Examples:

- grouped aggregate buckets
- planner/executor caches
- vertex-to-accumulator maps
- label-to-range maps

## Property access

Property access should be explicit and demand-driven.

Examples:

- node property read
- edge weight read
- edge timestamp read

A hop that only needs label filtering must not pull edge properties.

## Aggregation guidance

Grouped traversal queries should aggregate as close to the hop kernel as
possible.

Good shapes:

- `COUNT(*)`
- `COUNT(DISTINCT x)`
- `SUM(expr)`
- `AVG(expr)`

The important rule is:

- do not generate full rows if the query fundamentally only needs grouped
  counts or simple accumulators

## ORDER BY / LIMIT guidance

Top-k queries are common enough that the generic kernel should handle them
well.

Rules:

- push down limit where possible
- avoid full sort when only a small top-k is needed
- preserve neighborhood recency order if the storage layout already gives it

This matters especially for:

- feed queries
- trending queries
- top influencers

## WITH / continuation guidance

Continuation execution after `WITH` should preserve compact representations.

If `WITH` reduces the seed set to `k` vertices, the continuation should operate
on that compact seed set directly, not on heavyweight rows derived from it.

## Var-len guidance

Variable-length traversal should be treated as a first-class kernel problem,
not patched over at the row executor layer.

Key requirements:

- exact label filtering at each step
- early dead-end pruning
- compact path state
- configurable cycle policy

## Generic path success criteria

The generic kernel is good enough when it can handle:

- feed-like 2-hop traversals
- reverse-heavy neighborhood queries
- FoF-style aggregations
- label-restricted var-len traversals

without relying on special-case query families.

## Fast paths

Fast paths are still allowed, but they should come after the kernel is already
healthy.

A fast path is justified only when:

- the query family is common
- the kernel still leaves clear constant-factor wins on the table
- the fast path does not compensate for a broken low-level abstraction

## Planner contract

The planner should select from a small family of execution kernels instead of
assembling large row-oriented pipelines by default.

Preferred high-level plan families:

- seed + one-hop expansion
- seed + grouped one-hop aggregate
- seed + two-hop grouped aggregate
- seed + var-len traversal
- seed + top-k grouped aggregate

This is more stable than planning everything as generic rows first.

## Metrics to instrument from day one

The redesigned executor should record cheap internal counters for:

- scanned outgoing edges
- scanned incoming edges
- label rejects
- node predicate rejects
- property predicate rejects
- rows materialized
- groups created
- aggregate updates

These counters were essential in the current regression investigation and
should be built in from the beginning.

## Open questions

- Whether the first kernel should expose records or only vertex streams
- How soon `COUNT(DISTINCT)` needs special accumulator support
- Whether top-k should be part of the generic grouped aggregate kernel or a
  separate execution family
