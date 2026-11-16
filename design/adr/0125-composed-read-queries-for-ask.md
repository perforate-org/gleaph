# 0125. Composed read queries for `ask`, bounded by the caller's grants

Date: 2026-11-16
Status: proposed (2026-11-16). **Nothing in this ADR is implemented.** No composed form, lowering,
candidate enumeration, profile field or wire change exists, and no scorer has been measured on
composed candidates. Until acceptance, `ask` answers from the prepared catalog only (ADR 0114,
ADR 0118).
Last revised: 2026-11-16
Anchor timestamp: 2026-11-16 01:25:11 UTC

Evidence: [2026-11-16-hybrid-neural-decision-routing.md](../investigations/2026-11-16-hybrid-neural-decision-routing.md)
(slice S2), [2026-11-16-routing-gate-question-sources.md](../investigations/2026-11-16-routing-gate-question-sources.md).
Related: ADR [0074](0074-data-plane-authorization-core.md), [0075](0075-conditional-policies-constant-pushdown.md), [0078](0078-authz-aware-vector-search.md) (data-plane grants and
plan-time enforcement, as summarized in `design/security/rbac-and-prepared.md`),
[0114](0114-general-schema-conditioned-routing.md),
[0115](0115-graph-owner-routing-authority.md), [0116](0116-query-only-general-routing.md),
[0117](0117-parameter-value-domains.md), [0118](0118-public-ask-api.md),
[0124](0124-neural-similarity-feature-for-general-routing.md). This ADR does not depend on
ADR 0124.

## Context

`ask` selects one prepared operation and fills its parameters, or reports `Unsupported`
(ADR 0118). A question the graph could answer is unsupported whenever nobody registered a prepared
query of that shape.

Three facts about the current system:

- **Ad hoc GQL is already admitted under grants.** `gql_query` is a public composite query; it
  passes `authorize_adhoc_gql` and then plan-time enforcement, which walks every `PlanOp` of the
  built plan, extracts the exact requirement set and fails the whole query with a non-disclosing
  `Forbidden` if any demand is uncovered (`crates/router/src/rbac.rs`, `crates/router/src/authz.rs`,
  `design/security/rbac-and-prepared.md`).
- **Grants are fine-grained.** Rows are `MATCH` per label, `TRAVERSE` per edge label and
  direction, `READ` / `READ_PROPERTY` per property, with optional AND-only row predicates that are
  pushed into the plan (ADR 0075). Effective privilege is caller ∪ `PUBLIC` ∪ ownership root.
- **On-chain text generation is not available.** The reference llama.cpp canister produced 12
  tokens per update call at 0.5B parameters
  (`2026-11-10-onchain-inference-comparison.md`).

So a caller who can write GQL may already run any read its grants cover. A caller who can only ask
in natural language may run only what an administrator prepared.

## Problem

The reach of `ask` is set by catalog authoring, not by what the caller is authorized to read.
Public question sets show why a catalog cannot close that gap by enumeration: Spider has 5,693
distinct queries for 10,181 questions.

What prepared operations provide besides authorization, and what a composed query must therefore
replace or give up:

1. a bound on work ("no arbitrary parse/plan on hot path for untrusted callers",
   `rbac-and-prepared.md`);
2. a human-reviewed meaning for each operation;
3. a stable, auditable plan.

## Existing architecture assessment

- **Authorization** needs nothing new. Plan-time enforcement is exhaustive over `PlanOp`, runs
  under SECURITY INVOKER and is the single owner of data-plane coverage. A composed query that is
  lowered to GQL and sent down the `gql_query` path is checked by it like any other.
- **Routing context** is already built per caller by the Router and already carries the graph
  schema (labels, properties, edges) beside the operation cards (`GeneralGraphSchema` in
  `gleaph-routing-wire`).
- **Per-graph policy** has an owner and an authority: `GraphRuntimeConfig.general_routing`
  (MemoryId 5) written by the controller or graph tenant (ADR 0115).
- **`gleaph-gql` and `gleaph-gql-planner`** must stay general-purpose (AGENTS.md). Nothing here may
  add a composed-query concept to them.

Not representable today: a typed composed form, a deterministic lowering of it, an enumeration of
the vocabulary a caller may use, and a public answer that is not a prepared operation.

## Alternatives

- **A. Register more prepared queries (minimum change).** Keeps all three properties above.
  Rejected as the only answer because it is the limitation itself; it remains the right tool for
  frequent, expensive or sensitive shapes.
- **B. The client turns the question into GQL with its own model and calls `gql_query`.** Works
  today with no change and stays available. Rejected as the `ask` contract: it requires an external
  model, is not deterministic, and moves interpretation outside the Router.
- **C. `ask` composes a bounded typed query from candidates the caller's grants admit, lowers it
  deterministically and runs it through the ad hoc path (chosen).**
- **D. Generate GQL text on-chain.** Rejected: not feasible at the measured token rates.
- **E. Compose from the whole graph type and rely on `Forbidden` to stop what is not granted.**
  Rejected: the difference between "unsupported" and "failed authorization" would tell a caller
  that a label or edge it cannot see exists, and the scorer would be conditioned on vocabulary the
  caller has no right to.

## Decision

1. **A per-graph ask mode.** The routing profile gains a mode: `PreparedOnly` (default) or
   `PreparedAndComposed`. It is written under the ADR 0115 authority and requires
   `inference_enabled`. `PreparedOnly` is exactly today's behavior.
2. **Composition is selection, never generation.** The scorer chooses among typed candidates; a
   deterministic function assembles the choices and lowers them to GQL.
3. **Grants define the candidate vocabulary and are not model input.** For one request the Router
   derives the vocabulary from the caller's effective privileges intersected with the graph type:
   labels it may `MATCH`, edge labels and directions it may `TRAVERSE`, properties it may read, and
   the text and vector indexes over those. The derivation uses the same privilege resolution as
   plan-time enforcement — one owner, not a second reader of the grant table. The graph schema in
   the routing context is this caller-visible subset.
4. **The vocabulary is never the authority.** The lowered query runs as the original caller
   through the same planning, plan-time enforcement, policy pushdown and execution as `gql_query`.
   Filtering candidates narrows the search; only the plan walker admits a query.
5. **A fixed first grammar.** One composed query is:
   - one labeled anchor;
   - at most two fixed-length steps;
   - an AND of equality or range predicates on declared properties, with values from the slot
     extractor and ADR 0117 domains;
   - at most one text search, or one vector search in the anchor form of ADR 0121;
   - a projection of named properties;
   - a mandatory `LIMIT` under a fixed cap, and at most one `ORDER BY` key.

   Excluded: mutations, variable-length paths and shortest paths (their policy treatment is
   deferred, `rbac-and-prepared.md` conditional policy pushdown), unlabeled scans and wildcard label expressions (tenancy-only, not
   expressible as grants), `OPTIONAL`, subqueries, `CALL`, `USE GRAPH`, and raw embedding
   parameters. Widening the grammar is an amendment to this ADR.
6. **One decision, one abstain.** Prepared cards and composed candidates are scored by the same
   scorer in one request and produce one result or one abstain. A prepared operation wins a tie. A
   composed result has its own calibrated threshold in the profile, because it is not calibrated by
   the prepared threshold.
7. **The answer says what ran.** `Interpretation` and `Answer` report whether the result came from
   a prepared operation or a composed query, and a composed result carries the lowered GQL text.
   `Unsupported` keeps one meaning for the caller: nothing visible to it answers the question.
8. **The route stays a composite query (ADR 0116) and read-only.** Execution failures map to the
   existing `AskFailureKind` values. Existing execution bounds fail the query rather than truncate
   it (`design/execution/pipeline.md`), and that failure is reported, not hidden.
9. **Ownership.** The typed composed form and its lowering live in a Gleaph-owned layer beside the
   Router; `gleaph-gql` and `gleaph-gql-planner` receive ordinary GQL and are unchanged. The
   inference canister scores candidates and returns selections; it never emits GQL.

### State representability

| Claim | Representable today | Needs |
| --- | --- | --- |
| Ask mode and composed threshold | no: `GeneralRoutingProfile` has two fields | new fields; fresh state for MemoryId 5 |
| Caller-visible vocabulary | partly: enforcement resolves effective privileges, nothing enumerates them as a vocabulary | an enumeration owned by `authz` |
| Composed form and lowering | no | a new type and one lowering function |
| Composed candidates in the context | partly: schema is carried, candidates are not | context and result fields; wire version bump |
| "Answered by a composed query" | no: `Interpretation.operation` names a prepared operation | a source discriminator and the GQL text |
| Execution and authorization | yes: the `gql_query` path | nothing |

No new cross-canister effect is introduced: the route is a query, and nothing is persisted.

## Consequences

- With the mode on, what `ask` can answer follows the caller's grants instead of the catalog.
- Two callers asking the same question can get different queries, as they already can get
  different operations (ADR 0114 trade-offs).
- Row predicates on grants apply without any composer logic, because pushdown happens at lowering
  of the plan.
- Prepared operations keep their role for shapes outside the grammar, for reviewed semantics and
  for anything that must not depend on a scorer.
- The engine parts — vocabulary enumeration, the composed form, lowering — are deterministic and
  can be built and tested before any scorer change.

## Trade-offs

- **Correctness replaces authorization as the main risk.** A composed query can be authorized and
  still answer a different question. Returning the GQL makes that inspectable; it does not make it
  correct. This is why the mode is opt-in per graph and why the gate below scores executed results.
- **No new cost surface, but a new way to reach it.** Every composed query is one the caller could
  already send to `gql_query`. What changes is that a caller triggers it without writing it. The
  grammar's step and `LIMIT` caps bound the shape; they do not bound a scan over a large label.
- **Plans are not stable.** A scorer or catalog change can change the query composed for the same
  question. Prepared operations remain the tool when stability matters.
- **The vocabulary is derived state.** It must come from the enforcement module's own resolution;
  a separate reimplementation would drift from what the plan walker admits, and the drift would
  show up as composed queries that are then refused.
- **Calibration doubles.** Two thresholds per graph on small labelled sets is weak; ADR 0114 already
  found abstention sensitive to catalog size.
- **Entity values are surface text.** Predicate values need the same resolution as prepared
  parameters (ADR 0117 domains and the Router resolver); an unresolved value must yield
  `NeedsInput` or `Unsupported`, never a guess.

## Acceptance gates

**Engine, before any scorer work:**

| Gate | Requirement |
| --- | --- |
| Lowering | every grammar shape lowers to GQL that parses and plans; one test per shape |
| Vocabulary soundness | for generated composed queries, the requirement set extracted from the plan is covered by the caller's privileges whenever every part came from the caller's vocabulary |
| Non-disclosure | a question about a label, edge or property outside the caller's vocabulary returns the same result as one about a name that does not exist |
| Mode off | `PreparedOnly` results are identical to today's on the existing suites |
| General-purpose crates | no change in `gleaph-gql` or `gleaph-gql-planner` |

**Quality, on data frozen before tuning** (schema-directed sources from the question-source record,
with a second annotator):

| Gate | Requirement |
| --- | --- |
| Execution accuracy on answered items | at or above 90%, scored on result sets against a frozen snapshot; an empty result never counts as correct |
| Out-of-scope abstain | at or above 90% |
| Prepared suites with the mode on | no ADR 0114 gate regresses |
| Reporting | per graph type and per stratum, with paired-bootstrap 95% intervals |

**Cost:** PocketIC, worst grammar shape on the benchmark graph, recorded against the 5B query
budget, with the composed-candidate count per request recorded and bounded.

**Stop rule.** If execution accuracy on answered items cannot reach the bar without abstaining on
most in-scope questions, the mode is not shipped; the engine parts that passed stay only if another
caller uses them.

## Migration

Pre-production fresh install: the profile record and the routing wire change shape, with no
decoder for the old shape. `PreparedOnly` is the default, so an existing deployment behaves the
same after reinstall.

## Design documentation impact

On acceptance, with the implementation:

- ADR 0114 (routing output is no longer only a prepared operation) and ADR 0118 (`Interpretation`
  / `Answer` contract and the meaning of `Unsupported`).
- `design/security/rbac-and-prepared.md`: the vocabulary enumeration and the statement that
  composed queries use the ad hoc path.
- `design/storage/stable-memory-inventory.md`: MemoryId 5 profile fields.
- `design/gql/`: nothing, provided the general-purpose gate holds.

## Review axes

- **Encapsulation:** `authz` owns both the vocabulary and the enforcement; the composer consumes
  the vocabulary and cannot extend it.
- **Separation of concerns:** the scorer selects, a deterministic function lowers, the plan walker
  authorizes, the executor runs. None of these learns another's rule.
- **Invariants:** a composed query is read-only and inside the grammar; it runs as the caller; it
  is admitted only by plan-time enforcement; no candidate names vocabulary the caller cannot see.
- **Consistency:** one scorer and one abstain decision per request; one source of truth for
  effective privileges.
- **Fitness for purpose:** the grammar is the smallest that covers filter, search and one- or
  two-step questions; anything larger waits for evidence from the gate.

## Open questions

- Does the first grammar include `COUNT`? Schema-directed question sets are aggregate-heavy, and
  identity-only uses add no property demand under plan-time enforcement.
- How many composed candidates can one request score inside the context limits
  (`MAX_GENERAL_CONTEXT_BYTES`), and are they enumerated by the Router or by the canister from the
  caller-visible schema?
- Should composed results be offered as a draft prepared query to the graph owner when the same
  shape recurs?
- Is the feature scorer enough for composed candidates, or does this need ADR 0124's encoder? The
  engine gates do not depend on the answer; the quality gate decides it.
