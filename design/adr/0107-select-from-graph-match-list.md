# ADR 0107 — `SELECT ... FROM` with several graph matches, and a `FROM` source after other parts

Status: accepted (decided 2026-11-11; the multi-graph rule is implemented, the after-parts rule is
deferred pending the standard's semantics for a result statement's source)

## Context

[ADR 0106](0106-select-from-query-specification.md) decided how a *query-specification* source lowers.
Two `SELECT ... FROM` shapes were left fail-closed there:

1. a graph-match **list** — `selectGraphMatchList : selectGraphMatch (COMMA selectGraphMatch)*`, each
   `selectGraphMatch : graphExpression matchStatement`;
2. a source that follows other query parts — `MATCH (a) SELECT a.x FROM g MATCH (n)`.

This record decides (1) and explains why (2) stays open.

Facts established while deciding:

- `selectStatement` is a *result* statement (ISO/IEC 39075:2024 puts it in 14.12 beside the return
  statement in 14.11), which this AST also models as `ResultStatement::Select`.
- A `UseGraph` op executes its sub-plan seeded with the rows produced so far
  (`crates/graph/src/plan/query/executor/ops.rs`, the `UseGraph { sub_plan: Some(..) }` arm), so two
  consecutive graph scopes with independent matches multiply rows. The executor already covers the
  row-expansion behaviour (`inline_call_expands_row_cardinality`).
- Merging a sub-plan row into an outer row **fails only when the same name holds a different value**
  (`merge_inline_rows` → `inline merge: conflicting bindings on merge`), so a name bound by two `FROM`
  items would be accepted or rejected depending on the data.
- ISO/IEC 39075:2024, 3.6.12 defines linear composition: "The linear composition of operations over
  binding tables corresponds to a left lateral join between those tables."

## Decision

### 1. A graph-match list is a sequence of independent graph scopes (implemented)

Each item is planned as its own graph scope, in source order, and the scopes are appended: the rows
of item *n+1* are produced for every row of items 1..*n*, which is the cross product. Items are
non-correlated (`Explicit([])` scope is available to each, as in ADR 0106), because the grammar gives
a `selectGraphMatch` no way to reference another item's variables except through a correlation that
the standard's source clause does not provide.

The names the items export must be **distinct**. Two items binding the same name is rejected at plan
time with `PlannerError::InvalidStatement` (a new variant, distinct from the `Unsupported*` family:
this is a statement the planner deliberately refuses to define behaviour for, not work scheduled for
a later slice). Planning the item first and inspecting its binding layout is what makes the rejection
deterministic instead of data-dependent.

### 2. A source after other query parts stays fail-closed (deferred)

The standard's Clause 14.12 General Rules are not available to this repository (ISO text is licensed;
the public mirror reachable from this environment truncates before Clause 14), so two readings remain
open and they differ observably:

| Reading | `MATCH (a) SELECT * FROM g MATCH (n)` returns | Prior parts |
| --- | --- | --- |
| append (lateral join with the source) | `a × n` rows | still bound; a dead-binding check would apply |
| replace (the source is the row source) | `n` rows | bindings unreachable from the result |

3.6.12's "left lateral join" describes how consecutive *statements* compose their binding tables; it
does not say whether a result statement's `FROM` clause replaces the statement's input table. The
current rejection (`SELECT ... FROM after preceding query parts is not implemented`) keeps both
readings possible and costs nothing: a query that needs a source can put it first.

## Alternatives rejected

- **Fuse the list into one sub-plan.** A `UseGraph` wraps exactly one graph scope, so fusing would need
  a new op that carries several scopes — the vocabulary ADR 0106 explicitly deferred (its three
  triggers do not fire: no correlation decision, no reordering requirement, and the executor crate is
  not available for renames).
- **Join the items' rows with `HashJoin`/`CartesianProduct` ops.** Both produce a new row from two
  sub-plans; the list is a *source*, whose items have no join condition, so this would add an op pair
  for behaviour the sequential scopes already give.
- **Keep the runtime behaviour for duplicate names.** Accepting equal values and failing on different
  ones makes acceptance data-dependent, which the repository forbids elsewhere; plan-time rejection is
  deterministic and states the rule in one place.
- **Decide the after-parts rule from 3.6.12 alone.** It describes statement composition, not the
  semantics of a result statement's own source clause; deciding from it alone would be a guess.

## Consequences

- `PlannerError::InvalidStatement(String)` with `invalid statement: {msg}`, used for the duplicate-name
  rule and the unreachable empty-list shape. External matches are all non-exhaustive (`matches!` on
  other variants), so no consumer change is needed.
- The plan for `SELECT ... FROM g1 MATCH (a), g2 MATCH (b)` is one `UseGraph` per item followed by the
  outer projection; `binding_layout` carries both items' names.
- Ledger: `s14.12-select-from-two-graphs` leaves the plan-rejected table; `s14.12-select-from-after-parts`
  stays with the "after preceding query parts" fragment.

## Open question (reopen when evidence appears)

Does ISO/IEC 39075:2024 Clause 14.12 establish the `FROM`-source table as the select statement's input
table (replace) or join it with the ambient one (append)? Reopen this decision when either (a) the
General Rules of 14.12 become available, or (b) an authoritative reference implementation documents
its `MATCH ... SELECT ... FROM` behaviour. Until then the shape stays rejected.

## Implementation status

| Part | Where |
| --- | --- |
| multi-graph lowering, one scope per item | `crates/gql-planner/src/planner/match_plan/result.rs` (`SelectSource::GraphMatchList`) |
| duplicate-name rejection | same file (`graph_match_exported_names`, `PlannerError::InvalidStatement`) |
| after-parts rejection | same file (`plan_select_source`'s leading check) |
| executor | unchanged; the relied-on row-expansion path is covered by existing executor tests |
