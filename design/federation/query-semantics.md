# Federation query semantics

## Purpose

Describe how **federated state appears in query execution**: bindings, expand routing, and known limitations. This complements the physical plan docs in [gql/plan-format.md](../gql/plan-format.md).

## Non-goals

- Complete `PlanOp` reference ([execution/operators.md](../execution/operators.md)).
- USE GRAPH remote-graph pushdown (planner feature, not shard placement).

## Binding model (executor)

**Source:** `crates/graph/src/plan/query/executor.rs`, `PlanBinding` enum.

| Binding | When used |
|---------|-----------|
| `Vertex(VertexId)` | Local CSR vertex on this shard |
| `RemoteVertex(LogicalVertexId)` | Index hit or expand result on another shard / logical-only reference |
| `Edge` / `Path` / `Value` | Same as non-federated execution |

`RemoteVertex` is introduced at runtime (e.g. `materialize_federated_index_hits`), not by `gleaph-gql-planner` output schema alone.

## Index scan → multi-shard execution

1. Router `SeedProbe` finds an equality anchor in the plan.
2. Index returns `PostingHit` list (possibly multiple shards).
3. Router executes the **same plan blob** per shard with `seed_bindings_blob` for local ids on that shard.
4. Foreign-shard hits become `RemoteVertex` where the executor materializes federated index rows.

**Constraint:** Plans without an index anchor cannot run on multi-shard graphs (`no index anchor: single-shard graph required`).

## Expand behavior

### Federated expand path (implemented)

When `Expand` source is `RemoteVertex` and `resolve_federated_traversal_vertex` indicates cross-shard routing:

- Executor calls `federated_expand_coordinator` (`execute_expand` ~2620–2705).
- Results merged via `expand_rows_from_federated_expand_hits`.

### Local CSR expand (limitations)

When traversal uses **local** `VertexId` but placement says authoritative copy is elsewhere:

| Situation | Result |
|-----------|--------|
| Forward/reverse expand, placement on other shard | `UnsupportedOp("Expand.forward/reverse(federated placement on another shard)")` |
| Vertex `Migrating` on this or other shard | `UnsupportedOp("Expand(vertex migrating ...)")` |
| Remote vertex without federation routing | `UnsupportedOp("Expand(remote vertex requires federation routing)")` |

### Property projection

`property projection on remote vertex binding` → `InvalidExpressionValue` (remote endpoints cannot hydrate arbitrary property maps locally).

## Placement resolution

`resolve_federated_traversal_vertex` / placement client (`crates/graph/src/index/placement.rs`) map logical → physical for expand direction checks.

Failures surface as `FederatedIndexCall { op: "resolve_logical_at" | "federated_expand", ... }`.

## Planner vs executor gap

| Layer | Federation awareness |
|-------|----------------------|
| `gleaph-gql-planner` | `OutputBindingKind::RemoteVertex` exists; plans do not encode shard routing |
| Router | Shard dispatch + seeds |
| Graph executor | `RemoteVertex`, federated expand, placement checks |

**Implication:** Correctness depends on router dispatch + executor runtime checks, not on compile-time shard inference in the planner.

## Unsupported / partial matrix (representative)

| Scenario | Status |
|----------|--------|
| Multi-shard plan without index anchor | **Rejected** at router |
| `RemoteVertex` + federated expand | **Implemented** (wasm IC) |
| Local expand on non-authoritative copy | **Unsupported** |
| Expand during migration | **Unsupported** |
| Remote vertex property projection in expand | **Unsupported** |
| Router-driven full migration workflow | **Not implemented** |
| `federated_expand` on native test host | **Unsupported** |

Update this table when adding executor branches or router orchestration.

## Related documents

- [model.md](model.md)
- [operations.md](operations.md)
- [execution/pipeline.md](../execution/pipeline.md)
- [index/property-index.md](../index/property-index.md)
