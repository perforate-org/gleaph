# 0106. A `SELECT ... FROM` query-specification source is an uncorrelated scoped sub-plan

Date: 2026-11-11
Status: accepted (decided and implemented — plan 0399; the two combination rules stay fail-closed)
Last revised: 2026-11-11
Anchor timestamp: 2026-11-11 18:40:00 UTC +0900

Related: [0105](0105-linear-statements-one-container-derived-kind.md),
[GQL syntax support status](../gql/syntax-support.md), `crates/gql/src/ast/query.rs`,
`crates/gql-planner/src/planner/match_plan/result.rs`,
`crates/gql-planner/src/plan.rs`, `crates/graph/src/plan/query/executor/ops.rs`.

## Context

ISO/IEC 39075 lets a `SELECT` take its rows from a query specification. The rules in the checked-in
reference grammar (`crates/gql/reference/GQL.g4`) are:

```
selectStatement          : SELECT setQuantifier? (ASTERISK | selectItemList)
                           (selectStatementBody whereClause? groupByClause? havingClause?
                            orderByClause? offsetClause? limitClause?)?
selectStatementBody      : FROM (selectGraphMatchList | selectQuerySpecification)
selectGraphMatchList     : selectGraphMatch (COMMA selectGraphMatch)*
selectGraphMatch         : graphExpression matchStatement
selectQuerySpecification : nestedQuerySpecification | graphExpression nestedQuerySpecification
nestedQuerySpecification : LEFT_BRACE procedureBody RIGHT_BRACE
```

Two modelling deviations matter here and are recorded rather than fixed by this ADR:

1. `nestedQuerySpecification` wraps a `procedureBody`, but the AST stores a `CompositeQueryExpr`; the
   parsed subset (`{ MATCH (n) RETURN n }`) is what this ADR covers, and a full `procedureBody` source
   (with `NEXT` chains) would need an AST change first.
2. The select-level `WHERE` sits **after** the `FROM` body in the grammar, but `SelectBody` carries only
   `group_by`/`having`/`order_by`/`limit`/`offset`. A source clause therefore cannot currently be
   followed by a select-level `WHERE`; that is a separate AST gap (see the syntax-support reference),
   not part of this lowering decision.
3. `selectGraphMatch` is parsed by a reduced routine: it accepts `[OPTIONAL] MATCH <one path pattern>
   [WHERE ...]` and therefore rejects a match mode or a `YIELD` inside `FROM`. Those stay parser gaps;
   this ADR only decides how the resulting AST lowers.

The AST already models both source forms:

| Fact | Where |
| --- | --- |
| `SelectSource::GraphMatchList(Vec<SelectGraphMatch>)` and `SelectSource::QuerySpecification(SelectQuerySpecification)` | `crates/gql/src/ast/query.rs` |
| `SelectQuerySpecification::{Nested(CompositeQueryExpr), GraphNested { graph, query }}` | same |
| The parser accepts `SELECT n.x FROM g MATCH (n)`, `SELECT * FROM { MATCH (n) RETURN n }` and `SELECT * FROM g { MATCH (n) RETURN n }` | `crates/gql/src/parser/statement.rs` (`parse_select_source`) |
| The planner lowers a **single** graph match and both query specifications (uncorrelated scoped sub-plan, decision 2); more than one graph match and a source after preceding parts still fail closed | `crates/gql-planner/src/planner/match_plan/result.rs` |

The graph-match form was decided and implemented in plan 0398 by reusing the `USE <graph>` lowering.
What remains is the query-specification form, and the question it raises is not "how do we run a
subquery" but "how does a composite query used as a table become bindings for the outer projection,
and what is the boundary between the two scopes".

## Problem

Define the lowering contract for a query-specification source so that:

1. the source is evaluated once and its rows feed the outer `SELECT`;
2. the source's columns are addressable by the outer projection, aggregation, ordering and limiting;
3. nothing that the inner query bound but did not return leaks into the outer scope;
4. the source cannot silently depend on the outer statement's bindings;
5. no second execution mechanism is introduced for "sub-plan that exports its columns".

## Existing Architecture Assessment

The architecture already owns this concept. `PlanOp::InlineProcedureCall { sub_plan, scope,
optional }` means "execute a sub-plan per outer row and merge its result into the row", and the
executor implements exactly the required boundary:

| Mechanism | Evidence |
| --- | --- |
| Seed the sub-plan with only the variables the scope allows, run it, merge result rows | `execute_inline_procedure_call` in `crates/graph/src/plan/query/executor/ops.rs` |
| A returned query leaves a **fresh row of only its projected columns** | `project_row` builds a new `PlanRow` from the columns; inner working bindings do not survive it |
| Exported names are the inner query's output schema | `inline_output_vars` falls back to `sub_plan.output.columns`; names come from `RETURN` aliases, else `expression_name`, and `RETURN *` exports the bound variables |
| The plan shape is already exercised end to end | planner: `CALL { RETURN 1 AS x } RETURN x` (`planner_tests.rs`); executor: `MATCH (n:InlineBasic) CALL { RETURN n AS x } RETURN x` (`executor/ops.rs`) |

**State representability (step 2a):** the required state already exists — one plan op, one scope enum
(`InlineProcedureScope::{ImplicitAll, Explicit}`), one output-schema type. No new field, table,
variant or migration is needed, and no executor change is needed to represent the decision. What is
missing is only the planner branch.

## Alternatives

| Alternative | Benefit | Cost / reason rejected |
| --- | --- | --- |
| Flatten the inner query's ops into the outer plan | smallest plan, no wrapper | erases the boundary: late-project, limit-pushdown and topk-fusion passes would gain license to move outer clauses across the inner `ORDER BY`/`LIMIT`; the inner projection would have to be pruned by hand to avoid leaking working variables; "an inner query is one unit" would have no owner |
| **Reuse `InlineProcedureCall` with an explicit empty scope** | one execution path, no new op, no `crates/graph` edit | the op's name says "procedure call" for a non-call source (recorded debt with a trigger below) |
| New `PlanOp::Source { sub_plan }` delegating to the same executor function | vocabulary matches the concept | touches the executor crate, which is in flight and dirty; the only behavioural difference is a rename |
| Correlated source (`ImplicitAll`, importing outer bindings) | would allow future correlated forms | ISO's `fromClause` is uncorrelated; importing outer bindings changes meaning silently and makes the source depend on statement order |

## Decision

(a) A query-specification source is planned as a **scoped sub-plan**:
`PlanOp::InlineProcedureCall { sub_plan, scope: Explicit(vec![]), optional: false }`.
(b) `GraphNested { graph, query }` wraps the sub-plan in
`PlanOp::UseGraph { graph_name, sub_plan: Some(..) }` first; `Nested(query)` has no wrapper. The graph
expression is parsed as a bare object name (`FROM g { ... }`), which is the subset this decision covers.
(c) The source table's column names are the inner query's output-schema names — the same rule
`CALL { }` already uses. The outer statement addresses them as ordinary variables, so projection,
aggregation, `ORDER BY`, `LIMIT` and `DISTINCT` need no change.
(d) The source is **uncorrelated**: `Explicit(vec![])` states that in the plan instead of relying on
"the source happens to be first". Correlation, if ever wanted, is a separate decision.
(e) The existing restrictions stay: the source must be the first statement (no preceding parts) and a
graph-match list must have exactly one entry. Each keeps its own fail-closed message.

## Consequences

- One execution path serves `CALL { }`, `USE { }` and `FROM { }`; a future fix to scoped sub-plan
  execution fixes all three.
- The planner test can assert op equality between `SELECT x FROM { MATCH (n) RETURN n AS x }` and
  `CALL { MATCH (n) RETURN n AS x } SELECT x`-shaped plans, which is what keeps the lowering from
  growing a second implementation.
- No persistence, wire or routing impact.

## Trade-offs

- `InlineProcedureCall` now covers a source that is not a procedure call. This is vocabulary debt,
  not behaviour debt, and it is bounded by the trigger below.
- A `RETURN *` source exports the inner bound variables by name. That is what the grammar means; the
  outer statement can reference them afterwards.
- The sub-plan runs per outer row. With the source restricted to the first statement the outer row
  set is a single empty row, so the cost is one sub-execution, not a per-row loop.

## Trigger to revisit the vocabulary

Introduce `PlanOp::Source` (and move both arms onto one executor function) when any of these holds:

1. a correlated source form is decided;
2. an optimizer pass must treat a source as reorderable or mergeable, which requires distinguishing
   it from a `CALL`;
3. `crates/graph` can take the rename in one patch (the in-flight workstream has landed).

## Migration

None: no persisted state, no plan wire change. Implementation is staged:

1. planner: extend the source branch in `result.rs` to `QuerySpecification` per the decision, reusing
   `plan_in_graph_scope` for the graph nesting;
2. tests: op equality with the `CALL { }` form, plus reconstruction checks for both source forms;
3. executor: expected unchanged; the existing `CALL { }` executor tests are the evidence, and if the
   in-flight crate cannot be built locally, record that as a verification gap rather than editing it.

## Design Documentation Impact

- [GQL syntax support status](../gql/syntax-support.md): the plan-rejected row and the "out of scope"
  item name this ADR. The query-specification forms are implemented (plan 0399); the two combination
  rules above remain the open part.
- [ADR index](README.md): new row.
- No layer or boundary change, so [GQL stack layers](../gql/layers.md) is unchanged.
