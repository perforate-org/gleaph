# Group variables (variable-length edge groups)

**Status:** Phase 1 implemented (edge group on `{min,max}` expand)

## Purpose

Align variable-length expand semantics with GQL **group variables**: outside the quantified segment, an edge variable bound by `{min,max}` expand is a **list of edges** in hop order, not a single edge.

## Binding model

| Context | `e` binding |
|---------|-------------|
| Single-hop `Expand` | `PlanBinding::Edge` |
| `{min,max}` `Expand` with `emit_edge_binding` | `PlanBinding::EdgeGroup(Arc<[EdgeBinding]>)` |
| Materialized `RETURN e` | `Value::List` of edge records |

`depth = 0` paths (when `min = 0`) bind `EdgeGroup([])`.

## Expression rules (Phase 1)

| Expression | Status |
|------------|--------|
| `RETURN e` | OK → list of edge records |
| `CARDINALITY(e)` | OK |
| `GLEAPH.WEIGHT(e[-1])`, `GLEAPH.WEIGHT(e[0])` | OK (Cypher list index; requires `cypher` feature on `gleaph-gql`) |
| `LET x = SUM(GLEAPH.WEIGHT(e))` | OK (horizontal sum over group in one row) |
| `RETURN SUM(GLEAPH.WEIGHT(e))` (implicit `PlanOp::Aggregate`) | OK (same horizontal fold per input row) |
| `GLEAPH.WEIGHT(e)` | **Error** at evaluation (group, not singleton) |
| `e.prop` | **Error** (use indexed element first) |
| `WHERE GLEAPH.WEIGHT(e) = …` on var_len | Planner may still **fuse** to per-hop `edge_payload_predicate` (search semantics unchanged) |

`SHORTEST … GLEAPH.COST(GLEAPH.WEIGHT(e))` uses singleton `e` inside the cost expression during path search; unrelated to post-match `EdgeGroup` binding.

## Non-goals (later phases)

- Node group variables from parenthesized quantified subpaths
- `path_var` on var_len expand
- `hop_aux_binding` executor (per-hop opaque bytes group)

## Related

- [operators.md](./operators.md)
- `crates/graph/src/plan/query/executor/expand/var_len.rs`
- `crates/graph/src/plan/query/executor/eval.rs`
