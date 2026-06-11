# Federation query semantics

Last updated: 2026-06-11  
Anchor timestamp: 2026-06-11 23:23:04 UTC +0000

## Status

**Partially Implemented** — router seed routing and graph skip of leading index anchors are **Implemented** on the federated wire path. Cross-shard expand, peer ACL, and `RemoteVertex` expand are **not implemented**. Standalone default: [../sharding/standalone-mode.md](../sharding/standalone-mode.md). Target: [../sharding/federation-target.md](../sharding/federation-target.md).

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
4. Cross-shard **traverse** is **not implemented** (`UnsupportedOp`); target uses graph ↔ graph expand when added.
5. **Router merges** partial results.

Graph shards do **not** receive other shards' index hits for anchor resolution. Index intersection is index-local; slicing is router-local.

---

## Current implementation

### Binding model (executor)

**Source:** `crates/graph/src/plan/query/executor.rs`, `PlanBinding` enum.

| Binding | When used (current) |
|---------|---------------------|
| `Vertex(VertexId)` | Local CSR vertex on this shard (router seeds, local scans) |
| `RemoteVertex(GlobalVertexId)` | Wire variant only; expand when foreign placement authority is **not implemented** |
| `Edge` / `Path` / `Value` | Same as non-federated execution |

Index anchor binds use seeds → local `Vertex` only. `FederationPort::bind_index_hits` filters to the local shard.

### Index scan → execution (current)

| Path | Behavior | Status |
|------|----------|--------|
| Router `IndexAnchor` + seeds | Router lookup, per-shard seeds, graph skips anchor op | **Implemented** |
| Graph executor `IndexScan` / `IndexIntersection` (library) | Graph calls index via mock/native client when wired | Standalone dev only; **not** federated wasm wire path |
| Federated wasm without seeds | `plan_wire_guard` rejects `IndexScan` / `IndexIntersection` | **Implemented** |

**Constraint:** Multi-shard plans without an index anchor are rejected at router (`no index anchor: single-shard graph required`).

### Expand behavior (current)

Expand calls `resolve_traversal_expand_source` (`graph/federation/expand.rs`):

| Source binding | Placement | Result |
|----------------|-------------|--------|
| `Vertex` / `RemoteVertex` | Authoritative on **this** shard | `LocalCsr(VertexId)` |
| `Vertex` | Authoritative on **another** shard | `UnsupportedOp` (cross-shard expand) |
| `RemoteVertex` | Placement lookup fails (foreign home) | `UnsupportedOp` (cross-shard expand) |

`StandaloneFederation::peer_expand` returns `UnsupportedOp`. `EdgeTarget::Remote` endpoints return `UnsupportedOp` during local CSR expand.

### Property projection

`property projection on remote vertex binding` → `InvalidExpressionValue` (remote endpoints cannot hydrate arbitrary property maps locally).

### Placement resolution (current)

`placement::resolve_placement` maps `GlobalVertexId` → `VertexPlacement` for expand source checks (native test stubs + wasm IC).

Failures surface as `PlanQueryError::UnsupportedOp` for cross-shard expand, or placement errors when routing is misconfigured.

**Target:** placement authority stays on router for writes; graph placement IC reads narrowed to expand-time routing when peer expand returns.

---

## Planner vs runtime

| Layer | Federation awareness |
|-------|----------------------|
| `gleaph-gql-planner` | Shard-agnostic plans; emits `IndexScan` / `IndexIntersection` |
| Router | Shard dispatch + seeds (**Implemented** for equality and intersection anchors) |
| Graph executor | Local CSR + seed skip; no index client on federated wasm wire path |

**Implication:** Anchor correctness depends on **router index slice + seeds**, not graph calling index on the federated wire path.

---

## Unsupported / partial matrix (representative)

| Scenario | Current | Target |
|----------|---------|--------|
| Multi-shard plan without index anchor | **Rejected** at router | Same |
| `IndexIntersection` router seed | **Implemented** | Router `lookup_intersection` + slice |
| Graph executor index intersection (library) | Mock/native client only | Router seeds + skip op on graph |
| `RemoteVertex` from index hits | **Removed** | Peer expand from traverse only |
| Cross-shard expand (any binding) | **Unsupported** | Peer expand from placement |
| Remote vertex property projection | **Unsupported** | **Unsupported** |
| Router merge of cross-shard rows | **Partial** (count sum + row-batch union via `rows_blob`) | Join/aggregate merge planned |
| `federated_expand` canister API | **Removed** | Restore with follow-up ADR |

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
