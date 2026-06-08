# Group variables (variable-length edge and node groups)

**Status:** Phase 1–2 implemented (edge group on `{min,max}` expand; node groups from quantified subpaths)

## Purpose

Align variable-length expand semantics with GQL **group variables**: outside the quantified segment, variables bound by `{min,max}` expand are **lists** in hop order, not singletons.

## Binding model

| Context | Binding |
|---------|---------|
| Single-hop `Expand` | `PlanBinding::Edge` / `PlanBinding::Vertex` |
| `{min,max}` `Expand` with `emit_edge_binding` | `PlanBinding::EdgeGroup(Arc<[EdgeBinding]>)` |
| `{min,max}` `Expand` with `near_group_var` / `far_group_var` | `PlanBinding::VertexGroup(Arc<[VertexId]>)` per hop |
| Materialized `RETURN e` / `RETURN u` | `Value::List` of edge / vertex records |

`depth = 0` paths (when `min = 0`) bind empty groups (`EdgeGroup([])`, `VertexGroup([])`).

### Quantified subpath planning

Patterns like `MATCH (a)((u)-[e:L]->(v)){m,n}(c)` lower to one `PlanOp::Expand` from `a` to `c` with:

- `var_len: Some({m,n})`
- `near_group_var: Some(u)`, `far_group_var: Some(v)`
- inner `(u)` / `(v)` are **not** separately scanned

## Expression rules

| Expression | Status |
|------------|--------|
| `RETURN e`, `RETURN u` | OK → list of records |
| `CARDINALITY(e)`, `CARDINALITY(u)` | OK |
| `GLEAPH.WEIGHT(e[-1])`, `u[0]`, `v[-1]` | OK (Cypher list index; requires `cypher` feature on `gleaph-gql`) |
| `LET x = SUM(GLEAPH.WEIGHT(e))` | OK (horizontal sum over edge group in one row) |
| `RETURN SUM(GLEAPH.WEIGHT(e))` (implicit `PlanOp::Aggregate`) | OK (same horizontal fold per input row) |
| `GLEAPH.WEIGHT(e)` | **Error** at evaluation (edge group, not singleton) |
| `e.prop`, `u.prop` | **Error** (use indexed element first) |
| `WHERE GLEAPH.WEIGHT(e) = …` on var_len | Planner may still **fuse** to per-hop `edge_payload_predicate` (search semantics unchanged) |

`SHORTEST … GLEAPH.COST(GLEAPH.WEIGHT(e))` uses singleton `e` inside the cost expression during path search; unrelated to post-match group bindings.

## Wire format

`PlanOp::Expand` / `ExpandFilter` carry optional `near_group_var` / `far_group_var` on the plan wire. `PLAN_WIRE_VERSION` remains **1** during active development (wire layout may change without a version bump until the format stabilizes).

## Non-goals (later phases)

- `path_var` on var_len expand
- `hop_aux_binding` executor (per-hop opaque bytes group)

## Related

- [operators.md](./operators.md)
- `crates/graph/src/plan/query/executor/expand/var_len.rs`
- `crates/graph/src/plan/query/executor/eval.rs`
- `crates/gql-planner/src/planner/match_plan/path/pattern/term.rs`
