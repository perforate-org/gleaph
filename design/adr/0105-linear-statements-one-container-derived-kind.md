# 0105. Linear statements are one container with a derived kind; variants live at the part level

Date: 2026-11-11
Status: accepted (container and derived kind implemented; catalog parts and the `Statement`
re-organization are the next slice)
Last revised: 2026-11-11
Anchor timestamp: 2026-11-11 16:20:00 UTC +0900

Related: [GQL syntax support status](../gql/syntax-support.md),
[GQL stack layers](../gql/layers.md), `crates/gql/src/ast/query.rs`,
`crates/gql/src/program_modification.rs`, `crates/gql/src/parser/statement.rs`.

## Context

ISO/IEC 39075 separates three linear statement forms, and they differ **only** in which simple
statements they admit and whether a result statement may follow:

```
statement            : compositeQueryStatement | linearCatalogModifyingStatement | linearDataModifyingStatement
linearQueryStatement : simpleQueryStatement+
linearCatalogModifyingStatement : simpleCatalogModifyingStatement+
linearDataModifyingStatement     : <focused|ambient body>
body                 : simpleDataAccessingStatement+ primitiveResultStatement?
primitiveResultStatement : returnStatement orderByAndPageStatement? | FINISH
```

The AST in `crates/gql` does not follow that shape today:

| Fact | Where |
| --- | --- |
| The linear container is `LinearQueryStatement { at_schema, prefix_bindings, parts: Vec<SimpleQueryStatement>, result }` | `crates/gql/src/ast/query.rs` |
| `SimpleQueryStatement` already mixes query parts with DML (`Insert`, `Set`, `Remove`, `Delete`), authorization (`Grant`, `Revoke`), procedures (`CallProcedure`, `InlineProcedureCall`) and `Focused` | same |
| Catalog DDL lives only at `Statement::{CreateSchema, …, DropGraphType}`, so it cannot appear in `parts` | `crates/gql/src/ast/catalog.rs` |
| `SELECT` is modeled as `ResultStatement::Select`, although ISO puts `selectStatement` among the simple query statements and keeps `primitiveResultStatement` to RETURN/FINISH | `crates/gql/src/ast/query.rs` |

Two consequences are observable now:

1. **No owner for the linear kind.** Nothing names "this linear statement modifies data" from the
   AST. `program_modification` computes booleans for host policy, and the planner derives DML from
   emitted ops; neither answers the question, so no rule can depend on the answer.
2. **A grammar-invalid combination parses.** `MATCH (n) INSERT (:A) SELECT n` and
   `MATCH (n) SET n.x = 1 SELECT n` are accepted (probe 2026-11-11, `gleaph_gql::parser::parse`),
   although no ISO production admits a `SELECT` result in a data-modifying body.

Catalog juxtaposition (`CREATE SCHEMA /a CREATE GRAPH g ANY`) also fails, but making it
representable requires a `Statement` re-organization that touches `crates/router`,
`crates/graph-catalog` and `crates/cli`; `crates/cli` is in an in-flight workstream and cannot even
be compiled in the current environment (its new dependency graph needs an uncached crate). The
migration is therefore staged rather than done in one step.

## Decision

1. **One container struct, not one struct per linear kind.** The linear statement stays a single
   type (today `LinearQueryStatement`, renamed to `LinearStatement` when the `Statement`
   re-organization lands) with `at_schema`, `prefix_bindings`, `parts`, `result`. The three ISO
   forms differ only in their parts, so three structs would duplicate the container walk three
   times.
2. **The kind is derived, never stored.** `LinearKind { Query, DataModifying }` is computed from
   the parts. `CatalogModifying` joins the enum when `LinearPart::Catalog` exists. A stored `kind`
   field would duplicate a derivable value and admit invalid states.
3. **Variants live at the part level, as an exhaustive enum.**
   `LinearPart::{ Query(SimpleQueryStatement), Catalog(CatalogStatement) }` is the target; every
   consumer matches exhaustively and the compiler must see a new variant. No trait, no `dyn`.
4. **Result eligibility is a rule owned by the kind.** Query kind admits RETURN/SELECT/FINISH;
   data-modifying admits RETURN/FINISH, because ISO's `primitiveResultStatement` is RETURN or
   FINISH. The parser is the sole constructor, so it enforces the rule once for every construction
   site.
5. **`NEXT` stays the block separator.** Juxtaposition inside one linear statement is expressed by
   `parts`, never by `next`; a `NEXT` chain is a statement block, and each statement in it gets its
   own kind.
6. **A bare `{ ... }` procedure body is a statement, not a part.** `nestedProcedureSpecification`
   is a body form, so it becomes a `Statement` variant wrapping a statement block; it is not one
   of the `simple+` items.
7. **`ExplainAuthorization` and `Session` stay outside the linear model**: the first is a host
   diagnostic, the second is a session command.

## Consequences

- The linear statement has one classification owner (`LinearQueryStatement::kind`), so a rule that
  depends on the kind cannot drift between the parser, the validator and the formatter.
- The first rule that needed the owner now fails closed: a `SELECT` result after a data-modifying
  part is a parse error instead of a silently accepted statement the planner would have to
  interpret.
- DDL juxtaposition becomes representable as soon as `LinearPart::Catalog` lands, with no second
  container.
- The conformance ledger gains a row for the rejected combination, so the rule cannot regress.

## Trade-offs

- **The name stays wrong for one slice.** `LinearQueryStatement` is retained until the
  `Statement` re-organization, because a rename alone would force edits in `crates/cli`, which is
  unverifiable in this environment. The trigger for the rename is the catalog-parts slice, which
  touches that crate anyway.
- **`LinearKind` has one consumer today.** Accepted because the enum is this ADR's vocabulary, the
  catalog slice extends it with a reachable variant, and the alternative (a private boolean) would
  hide the concept the catalog migration needs.
- **Kind derivation scans the parts.** Linear statements have few parts; no caching is warranted,
  and caching would reintroduce the stored-state problem.
- **`SELECT` stays a result statement** even though ISO models it as a simple statement. Moving it
  is deferred until `SELECT ... FROM` execution is scoped, because the move changes the planner and
  formatter contracts; the deviation is recorded here and in the status document.

## Alternatives considered

**A. Separate `Statement` variants per linear kind plus a trait unifying processing (the
suggestion that prompted this ADR).** Rejected. The trait would unify the *container* (two accessor
methods), while every consumer — planner dispatch, validator, `program_modification`, formatter —
branches on the *part* kind. It therefore removes no match arm, adds a dispatch layer on top of
matches that must still distinguish kinds, and, if `dyn`, loses the exhaustiveness check that
already caught a real defect in this codebase (the formatter's missing `AllVertexLabels` arm).
Trait abstraction becomes justified only if a second representation of the same body appears (for
example a cached plan form) or a fourth linear kind arrives; both are recorded as triggers.

**B. Keep the current per-kind `Statement` split and add results ad hoc.** Rejected: it cannot
represent juxtaposition, has no owner for the kind, and each new kind re-implements the container
walk.

**C. Store `kind` as a field.** Rejected: duplicates derivable state and admits `kind`/`parts`
disagreement.

**D. Wait for the `crates/cli` workstream before doing anything.** Rejected: the missing result
rule is a live conformance defect that does not depend on the catalog migration.

## Migration

1. **This slice** (implemented 2026-11-11): `LinearKind` + derived `kind()`, one parser-owned
   result-eligibility check, tests, ledger rows, status-document rows.
2. **Catalog parts**: `LinearPart::Catalog(CatalogStatement)`, `Statement::Linear(LinearStatement)`;
   migrate `crates/router/src/facade/stable/graph_type_catalog.rs`, `crates/graph-catalog`,
   `crates/cli`, `crates/gql-planner`, `crates/gql/src/{validate,type_check,format}`, and rename
   `LinearQueryStatement` → `LinearStatement`. Prerequisite: the `crates/cli` workstream lands so
   the rename can be compiled and tested.
3. **Statement acceptance**: catalog juxtaposition, bare `{ ... }` bodies, DML-first ambient bodies
   (`Statement::Nested`), flipping the corresponding `Parse::Err` ledger rows.
4. **`SELECT`-as-simple-statement** decision, bundled with `SELECT ... FROM` support.

## Design Documentation Impact

- [GQL syntax support status](../gql/syntax-support.md): new rule row, and the remaining gaps stay
  with their owners.
- [GQL stack layers](../gql/layers.md): statement-model note pointing here.
- [ADR index](README.md): add this row.
- `crates/gql/src/ast/query.rs`: type and method documentation.
