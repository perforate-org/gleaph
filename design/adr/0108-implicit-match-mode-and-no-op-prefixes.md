# 0108. The implicit match mode is declared, and the no-op prefixes are accepted

Date: 2026-11-11
Status: accepted (decided and implemented — plan 0406; `DIFFERENT EDGES`, the restricting path modes and
path variable declarations stay fail-closed)
Last revised: 2026-11-11
Anchor timestamp: 2026-11-11 23:10:00 UTC +0900

Related: [0107](0107-select-from-graph-match-list.md), [GQL syntax support status](../gql/syntax-support.md),
`crates/gql-planner/src/planner/match_plan/path/pattern.rs`,
`crates/gql-planner/src/planner/match_plan/path/pattern/lower.rs`.

## Context

ISO/IEC 39075:2024 4.11.9 gives two match modes and says: "If a `<match mode>` is not specified, then an
implementation-defined (ID086) `<match mode>` is implicit." The standard also defines:
`DIFFERENT EDGES` — "A matched edge is not permitted to bind to more than one edge variable. There are
no restrictions on matched nodes"; `REPEATABLE ELEMENTS` — "There are no restrictions on matched edges
or matched nodes".

The planner's behaviour has always been the unrestricted one (`REPEATABLE ELEMENTS`), but it rejected
the clause outright, including the spelling of its own default, and it rejected the `WALK` path mode
(also unrestricted) and every `KEEP` clause. 16.4 Syntax Rule 6 makes `KEEP <path-pattern-prefix>` the
default prefix for the path patterns that declare none, so `KEEP` is a distribution rule rather than a
new traversal semantics.

## Decision

1. **The implementation's implicit match mode is REPEATABLE ELEMENTS** (ID086). Written out, every
   spelling of it (`REPEATABLE ELEMENT`, `REPEATABLE ELEMENT BINDINGS`, `REPEATABLE ELEMENTS`) is a
   no-op and plans exactly like the bare pattern.
2. **`WALK` is a no-op path mode prefix**: it asks for no traversal restriction, which is the planner's
   behaviour. `<mode> PATH(S)` also declares a path variable (Feature G004), so those stay rejected.
3. **`KEEP <prefix>` is distributed** to the path patterns that declare no prefix of their own
   (16.4 Syntax Rule 6) before lowering, so `KEEP WALK` and `KEEP ANY SHORTEST` plan as if the prefix
   were written on those paths. A `KEEP` prefix that the path lowering rejects is rejected with that
   rejection's message, which keeps one message per semantics.
4. **`DIFFERENT EDGES` stays rejected** with its ISO definition in the message. It needs a per-match
   check over the matched edge variables' values (pairwise disjoint element sets, including list
   bindings from variable-length expansions), which is a new plan/executor concern rather than a
   prefix-distribution rule.
5. **`TRAIL`/`SIMPLE`/`ACYCLIC` stay rejected**: each restricts repetition *within a path*, so a
   variable-length expansion must carry per-path visited sets. That is executor state, not a planner
   rewrite.

## Alternatives rejected

- **Treat the absent match mode as `DIFFERENT EDGES`.** That would change the meaning of every pattern
  the repository already plans; the unrestricted mode is the behaviour implemented everywhere.
- **Accept `DIFFERENT EDGES` as a no-op.** It is observably different (`(a)-[e]->(b)<-[e]-(c)` binds one
  edge twice), so accepting it would answer a different query.
- **Implement `KEEP` by copying the prefix into the AST during parsing.** Parsing describes the syntax;
  the distribution is a lowering rule, and keeping it in the planner is what lets the prefix's own
  rejection fire unchanged.

## Consequences

- Patterns that spell out the default mode, add `WALK`, or use `KEEP` with a supported prefix now plan;
  their plans are asserted equal to the bare-pattern plan.
- The rejection ledger keeps one row per remaining semantics (`DIFFERENT EDGES`, the restricting path
  modes, path variable declarations) so each stays measured.
- The declared default is now a contract; changing it later is a behaviour change that the tests for
  bare patterns would not catch, so the ADR is the place that records it.

## Trigger to revisit

Implement `DIFFERENT EDGES` when the executor gains element-identity comparison over matched bindings,
and the restricting path modes when a variable-length expansion can carry per-path visited sets.
