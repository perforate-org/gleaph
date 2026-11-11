# GQL syntax support status

## Purpose

Record which ISO/IEC 39075 surfaces the Gleaph stack implements, which it rejects, and **who rejects
them**. The parser accepting a construct is not a promise that the planner or executor implements its
semantics; this document is the status contract for that gap, and each status is backed by a test.

Grammar reference: [`crates/gql/reference/GQL.g4`](../../crates/gql/reference/GQL.g4).

## Status classes

| Class | Meaning | What the caller sees | Machine check |
| --- | --- | --- | --- |
| **Supported** | Parser accepts, planner lowers, executor runs | normal result | `crates/gql/tests/syntax_conformance.rs` (parse axis) + planner/executor tests |
| **Format-unsupported** | Parses and executes; the AST formatter refuses it | `FormatError::Unsupported(<reason>)`, never invented text | `syntax_conformance.rs` (format axis) |
| **Plan-rejected** | Parses, but the planner refuses the clause because its semantics are not lowered | `PlannerError` with a stable message | `planner_tests::plan_time_rejections_match_the_ledger` |
| **Catalog-rejected** | Parses, planner returns an empty plan, the Router catalog refuses the definition | `RouterError::InvalidArgument` | Router catalog tests |
| **Exec-rejected** | Parses and plans; the operator/evaluator refuses it | `PlanQueryError::UnsupportedOp` / `UnsupportedExpression` | executor tests |
| **Parse error** | The parser does not accept the construct yet | parse error at the offending token | `syntax_conformance.rs` (parse axis, `Parse::Err`) |

Silent semantic degradation is not an allowed class: a parsed clause either changes the plan or fails
the build.

## Machine-checked ledgers

Two ledgers carry this document, and they are the source of truth — the document does not assert
anything they do not check. Code and tests never read `design/` (see `AGENTS.md`).

- **Parse and format axes**: `crates/gql/tests/syntax_conformance.rs`. One table entry per construct
  family with `id`, grammar section, input, expected parse outcome, expected format outcome, and a
  feature gate. `ledger_covers_every_required_section` makes a missing §6–§21 family a failure, so
  the inventory cannot silently lose a row.
- **Plan axis**: `plan_time_rejections_match_the_ledger` in
  `crates/gql-planner/tests/planner_tests.rs`. It reuses the same construct ids and asserts the error
  variant plus a stable message fragment.

Construct ids are stable and shared: `s<grammar-section>-<construct>`.

## Implemented spellings

Grammar-valid ISO spellings that each needed parser work; all are accepted and pinned by the ledger
and section tests. The first block was plans 0388/0390.

| Id | Spelling |
| --- | --- |
| `s07.1-session-set-schema-*` | `SESSION SET SCHEMA` accepts `/a/b`, `/`, `HOME_SCHEMA`, `CURRENT_SCHEMA`, `.`, `..`, `../a/b`, and `$$p` (AST: `SchemaReference`) |
| `s12.4-create-graph-property-graph-inline` | `CREATE GRAPH g [PROPERTY] GRAPH { ... }` |
| `s12.6-graph-type-like` | `CREATE GRAPH TYPE t LIKE <graph>` |
| `s12.6-graph-type-as-nested` | `CREATE GRAPH TYPE t AS { ... }` |
| `s18.3-edge-type-to-endpoints` | `CONNECTING (A TO B)` (and `(A ~ B)`) beside `(A -> B)` |
| `s18.4-pattern-label-set-ampersand` | `CREATE GRAPH TYPE t { (:A&B) }` |
| `s18.6-property-type-typed` | `{ age :: INT64 }` beside `{ age INT64 }` |
| `s14.7-let-value` | `LET VALUE x = 1` |
| `s19.6-value-type-predicate-double-colon` | `n.x IS :: INT64` / `IS NOT ::` |
| `s20.9-aggregate-all-quantifier` | `COUNT(ALL x)` and the other set functions |
| `s21.2-leading-dot-number` | `RETURN .5` |
| `s21.3-digit-parameter-name` | `$1`, `$$1` |
| `s16.6-shortest-k-parameter` | `SHORTEST $k` / `SHORTEST $k GROUP` (path count as a parameter) |
| `s16.6-any-paths-parameter` | `ANY $k [PATHS]` path-count parameter |
| `s16.8-label-expression-nested-negation` | `MATCH (n:!!A)` (negation operand is a label expression, not only a primary) |

Quoted parameter names (`$"a b"`) stay out of scope.

## Parse-time rules owned by the linear statement kind

[ADR 0105](../adr/0105-linear-statements-one-container-derived-kind.md) gives the linear statement one
derived modification class (`LinearKind`: query or data-modifying) and moves result eligibility onto
it. The parser is the sole constructor, so it enforces the rule once:

| Rule | Rejected example | Id |
| --- | --- | --- |
| A `SELECT` result cannot terminate a data-modifying statement: ISO's `primitiveResultStatement` is `RETURN` or `FINISH` | `MATCH (n) INSERT (:A) SELECT n`, `USE g SET n.x = 1 SELECT n` | `s13.1-select-result-after-data-modification` |

`RETURN` and `FINISH` remain legal after data modification, and a query-kind statement still accepts
a `SELECT` result (`s14.12-select-after-match`). The kind is derived from the parts, never stored, so
it cannot disagree with them.

One modeling deviation remains recorded here: ISO places `selectStatement` among the simple query
statements and keeps `primitiveResultStatement` to `RETURN`/`FINISH`, while this AST stores `SELECT`
as a result statement. Moving it is bundled with `SELECT ... FROM` execution (ADR 0105 §Migration 4).

## Remaining parse gaps

Valid ISO GQL that the parser still rejects. Each row is a tracked `Parse::Err` ledger case; the
"owner" column names the module that would change.

| Construct | Example | Owner |
| --- | --- | --- |
| Statement composition (§9.1, §12.1, §13.1) | bare `{ MATCH (n) RETURN n }`; `INSERT (:A) INSERT (:B)`; `INSERT (:A) RETURN 1`; juxtaposed `CREATE SCHEMA /a CREATE GRAPH g ANY` | `parser/statement.rs` (statement model) |
| `OPTIONAL` block operands (§14.4) | `OPTIONAL { MATCH ... }`, `OPTIONAL ( MATCH ... )` | `parser/statement.rs` |
| `RETURN *` trailing clauses (§14.10–14.11) | `MATCH (n) RETURN * ORDER BY n.name` (also `GROUP BY`, `LIMIT`) | `parser/clause.rs` + `ast/query.rs` |
| `SELECT` trailing clauses (§14.12) | `SELECT * FROM { ... } WHERE true`; `selectGraphMatchList`; `matchMode`/`YIELD` inside `FROM` | `parser/statement.rs` |
| Procedure-body chains (§9.1, §15.2, §16.2) | `CALL { MATCH (n) NEXT RETURN n }`; `USE g { ... NEXT ... }` | `parser/statement.rs` |
| Graph type element alternatives (§18.2–18.4) | `NODE TYPE Person`, `NODE :Person`, phrase/pattern prefix forms, `IMPLIES` (`=>`) key label sets, multi-label edge endpoints `(:A)-[:R]->(:B&C)` | `parser/ddl.rs`, `ast/graph_type.rs` |
| Graph type reference types (§18.8–18.9) | `:: TABLE { f INT64 }`, `:: (:Person)` closed reference, `PROPERTY VALUE` | `parser/types.rs` |
| Relative catalog references (§17.x) | `USE ../s/g`, `CALL ../s/p()` | `parser/statement.rs` |
| `VARIABLE` object expression (§11.3) | `SESSION SET GRAPH VARIABLE $x` | `parser/statement.rs` |
| Value expressions (§20.1) | `RETURN GRAPH $g`, `RETURN TABLE { ... }` | `parser/expr.rs` + `ast/expr.rs` |
| `CASE` comparison operands (§20.7) | `CASE n.x WHEN = 1 THEN ...` | `parser/expr.rs` |
| Double-quoted string literals (§21.2) | `RETURN DATE "2024-01-01"` (the lexer always reads `"..."` as a delimited identifier) | `lexer.rs` (deliberate dialect decision) |
| Non-ASCII whitespace (§21.4) | `MATCH` + U+00A0 + `(n)` | `lexer.rs` |

Known over-acceptance (parser accepts input the grammar does not generate): `CREATE GRAPH <name>`
without a graph type source, `CREATE GRAPH TYPE <name>` without a source, and
`CREATE GRAPH TYPE t COPY OF x { ... }`. These are recorded here rather than pinned as failures.

## Catalog-rejected

Parsed and planned (empty plan) but the Router catalog refuses the definition, per
[ADR 0013](../adr/0013-gql-graph-type-catalog-on-router.md):

- `CREATE GRAPH TYPE t LIKE <graph>` and `AS COPY OF` sources → `CatalogError::Unsupported`.
- `CREATE SCHEMA` / `DROP SCHEMA` are **not** part of the catalog path; they are plan-rejected (below).

## Plan-rejected

| Id | Construct | Message fragment |
| --- | --- | --- |
| `s16.3-graph-pattern-yield` | `MATCH ... YIELD` | `MATCH ... YIELD` |
| `s16.4-match-mode-*` | `REPEATABLE ELEMENTS` / `DIFFERENT EDGES` | `match modes` |
| `s16.4-keep-clause` | `KEEP <prefix>` | `KEEP` |
| `s16.6-path-mode-walk` | `WALK`/`TRAIL`/`SIMPLE`/`ACYCLIC`, including a mode on a shortest prefix | `path mode prefixes` |
| `s16.6-all-paths`, `s16.6-any-paths` | `ALL` / `ANY` path search prefixes | `non-shortest path search prefixes` |
| `s16.6-shortest-k-parameter` | `SHORTEST $k` (path count as a runtime parameter) | `parameterized path counts` |
| `s14.12-select-from` | `SELECT ... FROM ...` | `SELECT ... FROM` |
| `s13.3-set-ambient`, `s13.4-remove-ambient`, `s13.5-delete-ambient` | ambient `SET`/`REMOVE`/`DELETE` | `ambient SET/REMOVE/DELETE` |
| `s12.2-create-schema`, `s12.3-drop-schema` | `CREATE`/`DROP SCHEMA` | `CREATE/DROP SCHEMA` |

Intentional empty-plan paths: `CREATE`/`DROP GRAPH` and `CREATE`/`DROP GRAPH TYPE` are applied by the
Router catalog before planning (`crates/router/src/facade/stable/graph_type_catalog.rs`).

## Exec-rejected

The evaluator refuses expression and operator kinds it does not implement instead of returning a
partial value (`crates/graph/src/plan/query/executor/eval.rs` catch-all arm):

- Predicates: `IS TYPED`, `IS NORMALIZED`, `IS DIRECTED`, `ALL_DIFFERENT`, `SAME`, `PROPERTY_EXISTS`,
  `IS SOURCE OF` / `IS DESTINATION OF`, `EXISTS { ... }`.
- Functions/expressions: `UPPER`, `LOWER`, `TRIM`, `NORMALIZE`, `CHAR_LENGTH`, `BYTE_LENGTH`,
  `PATH_LENGTH`, `ELEMENTS`, `LET ... END`, `VALUE { ... }`, `PATH [ ... ]`.
- Named `CALL <procedure>` is planned but rejected by the graph executor; inline `CALL { ... }` runs.
- Group edge-property aggregates (`SUM(e.distance)` over a variable-length edge group) — see
  [group-variables.md](../execution/group-variables.md).

## Formatter deferrals

`gleaph-gql --features format` formats a subset and refuses the rest with
`FormatError::Unsupported(<reason>)`. The format axis of the ledger pins every reason, so a new
formatter gap cannot appear silently.

Formatted today: identifiers (quoting when needed), literals, parameters, property access,
arithmetic/concatenation/boolean operators, comparisons, `[NOT] IN`, string predicates
(`LIKE`/`ILIKE`/`STARTS WITH`/`ENDS WITH`/`CONTAINS`, including `ESCAPE`), the `IS [NOT]` predicate
family (`NULL`, `TYPED`, `NFC`-style `NORMALIZED`, `TRUE`/`FALSE`/`UNKNOWN`, `LABELED`, `SOURCE OF`,
`DESTINATION OF`, `DIRECTED`), `ALL_DIFFERENT`/`SAME`/`PROPERTY_EXISTS`, `EXISTS { <pattern> }`,
`EXISTS { <subquery> }` and `VALUE { <subquery> }`, aggregates (including `DISTINCT` and the second argument of
`PERCENTILE_*`), `CASE` in both forms, `COALESCE`/`NULLIF`, `LET ... IN ... END`, list/record/`PATH`
constructors, `CAST`/`IS TYPED` with value types, the dedicated scalar/string/temporal function
variants (`ABS`-family, `UPPER`/`LOWER`/`LEFT`/`RIGHT`, `TRIM`/`BTRIM`/`LTRIM`/`RTRIM`,
`CHAR_LENGTH`/`BYTE_LENGTH`/`CARDINALITY`, `NORMALIZE`, `ELEMENTS`/`PATH_LENGTH`,
`DATE '...'`-style typed literals and the temporal constructor functions, `DURATION_BETWEEN`), and
the nullary `SESSION_USER`/`CURRENT_*`/`LOCAL_*` forms.

Two canonicalizations are deliberate: `IS :: T` renders as `IS TYPED T`, and `COUNT(ALL x)` renders
as `COUNT(x)` because `ALL` is the default quantifier.

Still deferred, with the reason the ledger records:

| Reason | Shapes |
| --- | --- |
| `statement` | every DDL/DML statement, `GRANT`/`REVOKE`, session and transaction commands |
| `session activity`, `transaction wrapper` | programs with session commands or `START`/`COMMIT`/`ROLLBACK` |
| `linear-query prefix` | `AT <schema>` and procedure-body binding definitions |
| `MATCH YIELD`, `NEXT YIELD` | yield clauses |
| `graph pattern modifier`, `path pattern modifier`, `path pattern expression`, `path primary` | match modes, `KEEP`, path prefixes/variables, quantified subpaths |
| `SELECT source` | `SELECT ... FROM ...` |
| `FOR ordinality` | `FOR ... WITH ORDINALITY/OFFSET` |
| `expression` | list index/slice (cypher) and AST shapes the parser cannot produce |
| `path pattern expression`, `path primary` | simplified path patterns and quantified subpath primaries inside a pattern |
| `expression` (aggregate) | an aggregate carrying `ORDER BY`/`FILTER` (the parser does not produce them today) |
| `temporal literal shape` | a typed temporal literal whose argument is not a text literal |
| `value type` | `IS TYPED`/`CAST` targets outside the printer's set (typed reference types with a label, string length bounds) |
| `literal` | bytes, path and extension values |
| `empty program` | a program with no transaction activity |

Two behaviours are canonicalizations rather than deferrals, and the ledger pins them: the `::` typed
spelling renders as `IS TYPED`, and `COUNT(ALL x)` renders as `COUNT(x)` (the `ALL` quantifier is
the default).

## Out of scope (later slices)

1. Statement model: bare block statements, juxtaposition without `NEXT`, ambient DML bodies.
2. AST-shape items: `RETURN *` clauses, `OPTIONAL { ... }` operand.
3. Semantics: match modes, path modes, non-shortest prefixes, `SELECT ... FROM` execution.
4. Executor expression families and the Gleaph dialect roadmap
   ([extension-syntax.md](extension-syntax.md)).

## Related documents

- [layers.md](layers.md)
- [plan-format.md](plan-format.md)
- [execution/operators.md](../execution/operators.md)
- [execution/pipeline.md](../execution/pipeline.md)
