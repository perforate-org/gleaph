# 0107. `SELECT ... FROM` as the standard's `CALL` rewrite

Date: 2026-11-11
Status: accepted (decided and implemented — plan 0401; two limits stay fail-closed and are named below)
Last revised: 2026-11-11
Anchor timestamp: 2026-11-11 20:40:00 UTC +0900

Related: [0106](0106-select-from-query-specification.md), [0105](0105-linear-statements-one-container-derived-kind.md),
[GQL syntax support status](../gql/syntax-support.md),
`crates/gql-planner/src/planner/match_plan/result.rs`,
`crates/gql-planner/src/planner.rs`, `crates/graph/src/plan/query/executor/ops.rs`.

## Context

ISO/IEC 39075:2024 Clause 14.12 gives the `<select statement>` **no General Rules** ("General Rules
None"). Its meaning lives in the Syntax Rules, which define the statement by rewriting it. The
relevant rules for the `FROM` source:

- **Rule 25 a–d**: `VSC` is the list of **all names of the incoming working record and working
  table**, and `SSBVARSET` is the set of column names of the outgoing tables of all match statements
  in the source **minus** the incoming names (rule 25 d i; for a query specification it is the nested
  query's outgoing column names).
- **Rule 25 g**: `SSBRIL` is `SSBBVR1 AS SSBBVR1, …` for those names.
- **Rule 26 a**: for a graph-match list the body is
  `CALL VSC { USE GREXP1 MATCHSTM1 ... USE GREXPn MATCHSTMn RETURN SSBRIL }`.
- **Rule 26 b**: for a query specification the body is `CALL VSC { USECL CALL NQS RETURN SSBRIL }`.
- **Rule 27 b**: the select statement is effectively replaced by that body plus the filter, the
  aggregate return chain, and a final `RETURN` carrying the select items.

Two facts follow and supersede earlier readings in this repository:

1. A `FROM` source **appends** to the incoming rows (a left lateral join, matching the definition of
   linear composition in 3.6.12), and returns only its own names so the incoming bindings survive.
2. The source's statements run with the incoming bindings in scope, so a source is **correlated**
   (amended into [0106](0106-select-from-query-specification.md)).

This repository's planner models a `CALL` body as `InlineProcedureCall { sub_plan, scope, optional }`,
whose executor seeds the sub-plan from the scope, runs it, and merges the output row into the outer
row (`crates/graph/src/plan/query/executor/ops.rs`). That is the same shape the rewrite names.

## Decision

### 1. The source is the rewrite, planned as a linear query (implemented)

The lowering builds the rewrite as an actual `LinearQueryStatement` — one
`Focused { graph, body: Match }` part per graph-match item (or one `InlineProcedureCall` part for a
query specification), and a `RETURN` of the source's own names — and plans it with the normal
builder, then wraps the result in `InlineProcedureCall` with `scope = Explicit(incoming names)`.

Consequences that the tests pin:

- a graph-match list is **one** scope holding one `USE <graph> MATCH` per item, not one scope per
  item;
- the returned names are the items' pattern variables minus the incoming names, so incoming bindings
  are not returned twice;
- because the body is planned by the normal pipeline, its plan is `assert_eq!`-identical (spans and
  the binding-layout index aside) to the plan of the hand-written rewrite, e.g.
  `SELECT n.x FROM g MATCH (n)` vs `CALL () { USE g MATCH (n) RETURN n } SELECT n.x`, and
  `MATCH (a) SELECT a.x FROM g MATCH (b)` vs `MATCH (a) CALL (a) { USE g MATCH (b) RETURN b } SELECT a.x`;
- a query-specification source keeps the write-out's two layers, `CALL (…) { CALL { … } RETURN … }`.

### 2. A source after other query parts is legal (implemented)

Removing the earlier rejection follows from rules 25–27: the incoming bindings are the scope and the
rows the source appends to. The previous "put the source first" rule had no basis in the standard.

### 3. Two limits stay fail-closed (named, with the work each needs)

Both are `PlannerError::InvalidStatement` — the statement is well-formed GQL, but this planner cannot
represent it without changing its meaning:

| Limit | Why it cannot be planned today | What unblocks it |
| --- | --- | --- |
| a source match **re-uses an incoming binding** (`MATCH (a) SELECT a.x FROM g MATCH (a)`) | the planner does not seed a source's matching with the incoming bindings, so it would plan a fresh scan for `a` and then collide on merge | seed the rewrite's planning with the incoming binding kinds and bound-variable set |
| **two items declare the same name** (`FROM g1 MATCH (a), g2 MATCH (a)`) | the rewrite makes it one variable, but this planner's `USE` lowering opens a fresh binding scope per part, so the second item would re-bind `a` as a new scan (the executor's merge then accepts equal values and fails on different ones, i.e. data-dependent acceptance) | propagate binding sets through `Focused` parts so consecutive parts compose linearly |

The duplicate-name rule is a deliberate deviation in *acceptance* only: the query stays rejected until
the second row's work is done, rather than being accepted with data-dependent behaviour.

## Alternatives rejected

- **One independent scope per item (the first implementation of this slice).** Cross products of
  independent scopes coincide with the rewrite only when no name repeats and no item refers to an
  earlier item; rule 26 a puts the items in one body, so the plan would diverge exactly in the cases
  that matter.
- **Keep rejecting a source after other parts.** Refuted by rules 25–27; the standard's source is
  built precisely to run over the incoming bindings.
- **Accept repeating names and rely on the executor's merge.** `merge_inline_rows` accepts the same
  name when the values are equal and errors when they differ, so acceptance would depend on data.
- **Synthesize a `CALL` plan by hand instead of planning the rewrite.** The first attempt assembled
  ops directly; it produced a plan without anchor selection and optimizer annotations, i.e. a
  different plan from the written-out rewrite. Planning the rewrite removes that whole class of drift.

## Consequences

- `PlannerError::InvalidStatement(String)` states the two limits; the planner ledger's `Variant` enum
  gained an `Invalid` case so the ledger can pin them (`s14.12-select-from-repeated-name`,
  `s14.12-select-from-correlated-source`).
- The source body is planned twice for a query-specification source (once to learn the nested query's
  column names, once inside the rewrite). The alternative — hand-computing output columns — duplicates
  the output-schema derivation; planning twice is small and deterministic.
- The plan shapes are pinned by equality tests against written-out rewrites, so a future change to the
  lowering that also changes the plan fails a test instead of drifting quietly.

## Trigger to revisit the limits

Do the "seed the source with incoming bindings" and "propagate bindings through `USE` parts" work when
either (a) a query needs the correlated form, or (b) `USE` parts are already being reworked for another
reason (bindings crossing `USE` currently also affect `USE g MATCH (a) USE h MATCH (a)` written directly).
