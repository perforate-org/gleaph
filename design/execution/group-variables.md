# Group variables (variable-length edge and node groups)

**Status:** Phase 1–4 implemented (edge group, vertex groups, var_len `path_var`, `hop_aux_binding`)

## Purpose

Align variable-length expand semantics with GQL **group variables**: outside the quantified segment, variables bound by `{min,max}` expand are **lists** in hop order, not singletons.

## Binding model

| Context | Binding |
|---------|---------|
| Single-hop `Expand` | `PlanBinding::Edge` / `PlanBinding::Vertex` |
| `{min,max}` `Expand` with `emit_edge_binding` | `PlanBinding::EdgeGroup(Arc<[EdgeBinding]>)` |
| `{min,max}` `Expand` with `near_group_var` / `far_group_var` | `PlanBinding::VertexGroup(Arc<[VertexId]>)` per hop |
| `{min,max}` `Expand` with `emit_path_binding` | `PlanBinding::Path(PathBinding)` (lazy, materializes to `Value::Path`) |
| `SHORTEST k GROUP` with `path_var` | `PlanBinding::PathGroup` (materializes to `Value::List` of `Value::Path`) |
| `SHORTEST k GROUP` with `edge` emitted | `PlanBinding::EdgeGroup` (last-hop edges) |
| `{min,max}` `Expand` with `hop_aux_binding` | `PlanBinding::Value` — `Value::List` of `Value::Bytes` per hop (inline payload) |
| Single-hop `Expand` with `hop_aux_binding` | `PlanBinding::Value(Value::Bytes)` or `Value::Null` when payload empty |

`depth = 0` paths (when `min = 0`) bind empty groups (`EdgeGroup([])`, `VertexGroup([])`, empty hop_aux list).

### Quantified subpath planning

Patterns like `MATCH (a)((u)-[e:L]->(v)){m,n}(c)` lower to one `PlanOp::Expand` from `a` to `c` with:

- `var_len: Some({m,n})`
- `near_group_var: Some(u)`, `far_group_var: Some(v)`
- inner `(u)` / `(v)` are **not** separately scanned

### Path variables on var_len expand

`MATCH p = (a)-[e:L]->{m,n}(b)` (no `SHORTEST` prefix) plans `path_var: Some(p)` on the var_len `Expand`. Binding uses the same `PathBinding` / `Value::Path` materialization as `ShortestPath`. Single-hop `MATCH p = (a)-[e]->(b)` remains unsupported.

### `SHORTEST k GROUP`

`MATCH SHORTEST k GROUP …` plans `ShortestPath` with `mode: ShortestKGroup(k)`. The executor emits **one row** per input row:

- `path_var` → `PlanBinding::PathGroup` (materializes to `Value::List` of `Value::Path`)
- `edge` → `PlanBinding::EdgeGroup` of last-hop edges (when emitted)

`SHORTEST k` without `GROUP` still emits **one row per path** (`ShortestK(k)`).

### WCOJ `hop_aux`

Triangle / cycle patterns fused to [`PlanOp::WorstCaseOptimalJoin`] carry `hop_aux_binding` on each [`WcojEdge`] when `{edge}__hop_aux` is referenced (same naming as `Expand`). Executor binds `PlanBinding::Value(Value::Bytes)` per matched hop.

## Expression rules

| Expression | Status |
|------------|--------|
| `RETURN e`, `RETURN u` | OK → list of records |
| `RETURN e__hop_aux` | OK → `Value::Bytes` (single hop) or `Value::List` of bytes (var_len) |
| `RETURN p` | OK → `Value::Path` (singleton) or `Value::List` of paths (`SHORTEST k GROUP`) |
| `CARDINALITY(p)` | OK on `PathGroup` |
| `CARDINALITY(e)`, `CARDINALITY(u)` | OK |
| `GLEAPH.WEIGHT(e[-1])`, `u[0]`, `v[-1]` | OK (Cypher list index; requires `cypher` feature on `gleaph-gql`) |
| `LET x = SUM(GLEAPH.WEIGHT(e))` | OK (horizontal sum over group in one row) |
| `RETURN SUM(GLEAPH.WEIGHT(e))` (implicit `PlanOp::Aggregate`) | OK (same horizontal fold per input row) |
| `GLEAPH.WEIGHT(e)` | **Error** at evaluation (group, not singleton) |
| `e.prop`, `u.prop` | **Error** (use indexed element first) |
| `WHERE GLEAPH.WEIGHT(e) = …` on var_len | Planner may still **fuse** to per-hop `edge_payload_predicate` (search semantics unchanged) |

`SHORTEST … GLEAPH.COST(GLEAPH.WEIGHT(e))` uses singleton `e` inside the cost expression during path search; unrelated to post-match group bindings.

## Wire format

`PlanOp::Expand` / `ExpandFilter` carry optional `near_group_var`, `far_group_var`, `path_var`, and `emit_path_binding` on the plan wire. `PLAN_WIRE_VERSION` remains **1** during active development (wire layout may change without a version bump until the format stabilizes).

## Related

- [operators.md](./operators.md)
- `crates/graph/src/plan/query/executor/expand/var_len.rs`
- `crates/graph/src/plan/query/executor/eval.rs`
- `crates/gql-planner/src/planner/match_plan/path/pattern/term.rs`
