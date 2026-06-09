# Federation query semantics

## Status

**Partially Implemented** — immature executor paths (`RemoteVertex` from index hits, graph-side index lookup) coexist with partial router seed routing. **Target semantics** are documented in [../sharding/federation-target.md](../sharding/federation-target.md). Standalone default: [../sharding/standalone-mode.md](../sharding/standalone-mode.md).

## Purpose

Describe how **federated state appears in query execution**: bindings, expand routing, and known limitations. This complements the physical plan docs in [gql/plan-format.md](../gql/plan-format.md).

## Non-goals

- Complete `PlanOp` reference ([execution/operators.md](../execution/operators.md)).
- USE GRAPH remote-graph pushdown (planner feature, not shard placement).
- Index intersection algorithm (see [index/lookup-intersection.md](../index/lookup-intersection.md)).

---

## Target architecture (planned)

See [../sharding/federation-target.md](../sharding/federation-target.md) for the full flow. Summary:

1. **Router** calls graph-index (`lookup_equal` / `lookup_intersection`).
2. Router **slices** `PostingHit` by `shard_id` and builds **per-shard seeds**.
3. Each **graph shard** executes the plan locally with seeds; skips leading index anchor ops.
4. Cross-shard **traverse** uses graph ↔ graph `federated_expand` when needed.
   One-hop `Expand` and var_len / `ShortestPath` entry use
   `resolve_traversal_expand_source` (local CSR when this shard is authoritative;
   peer-expand sources remain unsupported for multi-hop BFS).
5. **Router merges** partial results.

Graph shards do **not** receive other shards' index hits for anchor resolution. Index intersection is index-local; slicing is router-local.

---

## Current implementation (legacy / partial)

The following describes **today's code**, which diverges from the target and is scheduled for defer/refactor.

### Binding model (executor)

**Source:** `crates/graph/src/plan/query/executor.rs`, `PlanBinding` enum.

| Binding | When used (current) |
|---------|---------------------|
| `Vertex(VertexId)` | Local CSR vertex on this shard |
| `RemoteVertex(LogicalVertexId)` | Index hit on another shard (legacy index bind path) |
| `Edge` / `Path` / `Value` | Same as non-federated execution |

`RemoteVertex` is reserved for expand/peer paths when needed; index anchor binds use seeds → local `Vertex` only.

### Index scan → execution (current)

Two overlapping paths:

| Path | Behavior | Target disposition |
|------|----------|-------------------|
| Router `IndexAnchor` + seeds | Router lookup, per-shard seeds, graph skips anchor op | **Implemented** |
| Graph executor `IndexScan` / `IndexIntersection` (library) | Graph calls index via mock/native client; binds local hits via `FederationPort` | Federated wire path uses router seeds only |

**Constraint (current):** Multi-shard plans without an index anchor are rejected at router (`no index anchor: single-shard graph required`).

### Expand behavior (current)

#### Federated expand path

Expand calls `resolve_traversal_expand_source` (`graph/federation/expand.rs`) on the source binding:

- **`PlanBinding::Vertex`** with authoritative placement on another shard → **peer expand** (router seeds path).
- **`PlanBinding::RemoteVertex`** with authoritative placement on another shard → **peer expand** (legacy expand/peer path).
- Authoritative on local shard → local CSR via returned `LocalCsr(VertexId)`.

Peer expand invokes `federated_expand_coordinator` via `StandaloneFederation::peer_expand`.

#### Local CSR expand (limitations)

### Property projection

`property projection on remote vertex binding` → `InvalidExpressionValue` (remote endpoints cannot hydrate arbitrary property maps locally).

### Placement resolution (current)

`resolve_federated_traversal_vertex` / placement client (`crates/graph/src/index/placement.rs`) map logical → physical for expand direction checks.

Failures surface as `FederatedIndexCall { op: "resolve_logical_at" | "federated_expand", ... }`.

**Target:** placement authority stays on Router for writes; graph placement IC reads deferred or narrowed to expand-time peer routing.

---

## Planner vs runtime

| Layer | Federation awareness |
|-------|----------------------|
| `gleaph-gql-planner` | Shard-agnostic plans; emits `IndexScan` / `IndexIntersection` |
| Router | Shard dispatch + seeds (partial) |
| Graph executor | Legacy index bind + federated expand (partial) |

**Target implication:** Correctness for anchors depends on **Router index slice + seeds**, not graph calling index or binding foreign hits.

---

## Unsupported / partial matrix (representative)

| Scenario | Current | Target |
|----------|---------|--------|
| Multi-shard plan without index anchor | **Rejected** at router | Same |
| `IndexIntersection` router seed | **Implemented** | Router `lookup_intersection` + slice |
| Graph executor index intersection | **Partial** (library/mock index only) | Router seeds + skip op on graph |
| `RemoteVertex` from index hits | **Removed** | Peer expand from traverse only |
| `RemoteVertex` + federated expand | **Partial** (wasm IC) | Peer expand from placement on traverse source |
| Local `Vertex` expand, authoritative on other shard | **Partial** — peer expand via `resolve_traversal_expand_source` | Same |
| Remote vertex property projection | **Unsupported** | **Unsupported** |
| Router merge of cross-shard rows | **Partial** (count sum + row-batch union via `rows_blob`) | Join/aggregate merge planned |
| `federated_expand` on native test host | **Unsupported** | **Unsupported** |

Update this table when implementing [../sharding/federation-target.md](../sharding/federation-target.md).

---

## Related documents

- [../sharding/README.md](../sharding/README.md)
- [../sharding/federation-target.md](../sharding/federation-target.md)
- [../index/lookup-intersection.md](../index/lookup-intersection.md)
- [model.md](model.md)
- [operations.md](operations.md)
- [execution/pipeline.md](../execution/pipeline.md)
- [index/property-index.md](../index/property-index.md)
