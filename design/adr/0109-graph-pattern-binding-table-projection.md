# ADR 0109 — The graph-pattern yield clause lowers to a binding-table projection

- Status: accepted
- Date: 2026-11-12
- Supersedes: nothing. Related: [0105](0105-linear-statements-one-container-derived-kind.md) (linear
  statements), [0107](0107-select-from-graph-match-list.md) (the `SELECT ... FROM` rewrite).

## Context

ISO/IEC 39075:2024 16.3 makes a graph pattern a `<graph pattern binding table>`: the pattern plus an
optional `<graph pattern yield clause>` (`YIELD ev, ev, pv ...` or `YIELD NO BINDINGS`). The clause
decides which of the pattern's variables the statement keeps; Syntax Rule 12 defines the projected
list (the names written, plus the variables the pattern shares with the incoming row), and rule 20
joins that projected table with the incoming record, so bindings produced by earlier parts are not
dropped.

The planner had no way to express that: a row's bindings are a union across the plan, every operator
except `Project`/`Materialize` adds to it, and nothing removed one. `MATCH ... YIELD` was therefore
rejected fail-closed (`MATCH ... YIELD is not implemented`), while the validate layer already modelled
the clause's *scope* — with one mistake: it assumed the clause hides the incoming bindings too.

## Decision

1. **One operator, `PlanOp::KeepBindings { bindings }`**, keeps exactly the listed row bindings and
   drops the rest. It changes no value, no row count and no order; it only narrows the binding table.
2. **The lowering computes the keep set as `incoming ∪ yielded`.** The incoming names are captured
   before the pattern binds anything, so independent matches plan as a product and each branch
   projects its own table; the union is what rule 20's natural join leaves visible. `NO BINDINGS`
   keeps the incoming set alone.
3. **The shared variable sets are narrowed after the op**, so later parts plan against the projected
   scope rather than against variables the row no longer carries.
4. **The validate layer mirrors the same rule**: after the clause, the visible names are the prior
   scope plus the yielded names. This reverses the previous behaviour for a name bound by an earlier
   part (`MATCH (p) MATCH (a) YIELD a RETURN p` is valid, not an error).
5. **The projection is shard-safe**: a graph shard executes it (row shaping only), so the remote
   `USE GRAPH` whitelist allows it after the root scan.

## Consequences

- The clause now parses, formats, validates and plans; the conformance ledger's plan-rejection rows
  for `MATCH ... YIELD` are gone and planner tests pin the keep set, the incoming bindings and the
  narrowed output schema.
- `MATCH ... YIELD` is not limited to `RETURN`-style use: any later part sees the projected scope, and
  reading a dropped variable is rejected by the validate layer before planning.
- The projection does not re-match: the plan keeps the scans and filters the pattern already needed.
- A plan that reads a projected-away binding by construction (hand-built plans, other embedders) fails
  at execution with `MissingBinding`, which the executor test pins.

## Alternatives rejected

- **Accept only the no-op spellings** (a yield list equal to the pattern's own variables, `NO BINDINGS`
  on a variable-free pattern). That would have closed the ledger rows without implementing 16.3.
- **Keep only the yielded names, dropping the incoming ones.** That is what the validate layer used to
  model; it contradicts rule 20's join and would have made `MATCH (p) ... YIELD a RETURN p` invalid.
- **Model a separate binding-table type in the plan.** The row already *is* the binding table; the
  missing capability was removing a binding, not representing one.
