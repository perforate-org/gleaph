# 0075. Conditional resource policies: grant-attached predicates with router-resolved constant pushdown

Date: 2026-08-24
Status: accepted
Last revised: 2026-08-24

## Context

Phase 1 of [ADR 0074] landed label/direction/property-granular grants, but grants are
**unconditional**: whoever holds `READ` on `Post` sees every post. Multi-principal shared
graphs need row-level visibility — "callers see posts they own or that are public" — without
partitioning data into per-user graphs.

`MSG_CALLER()` evaluates today as a runtime extension function inside graph shards
(`crates/graph/src/gql_execution_context.rs`), usable in user-written filters and prepared
access patterns. Conditional policies themselves were explicitly deferred by [ADR 0074]
("separate follow-up ADR"), whose internal model already reserved `predicate?` on privileges;
the original external proposal sketched both standalone `POLICY` objects and `FOR (…) WHERE …`
conditional selectors. This ADR decides between those shapes and fixes the evaluation
architecture.

Two hard architectural facts constrain any design:

1. Row property values live on **graph shards**; the Router orchestrates but holds no row
   data. Anything that requires the Router to fetch-and-filter values post hoc drags
   unauthorized values into Router memory and wrecks performance.
2. The trust model is fixed ([ADR 0028], Plan 0287 review): shards trust the Router plus
   registered internal callers; index and vector canisters never evaluate policies. Caller
   identity does not propagate below the Router.

## Decision

### 1. Grant-attached predicates, not policy objects

A grant row gains an optional compiled predicate:

```text
GrantRow = { subject, graph, privileges… , predicate?: CompiledPredicate, expires_at? }
```

Standalone `POLICY` objects are rejected: they would create a second identity/lifecycle for
what is fundamentally one authorization rule (grant × condition), splitting rule identity,
introspection, and revoke semantics across two stores. One rule = one row; introspection
prints the condition inline; `REVOKE` removes rule and condition together.

### 2. Minimal deterministic predicate DSL

```text
Predicate  := Comparison (AND Comparison)*        // no OR, no NOT, no arithmetic in 2a
Comparison := PropertyRef Op ValueExpr
PropertyRef:= <selector variable>.<property name>
Op         := = | ≠ | < | ≤ | > | ≥
ValueExpr  := literal | MSG_CALLER()
```

Deterministic, side-effect-free, bounded (conjunction depth capped), catalog-checked at
GRANT time (property existence, scalar type compatibility with the literal/principal type).
Disjunction and `EXISTS` patterns belong to later phases (OR-union machinery exists for
indexes; ReBAC remains its own future ADR).

### 3. Grammar

The conditional selector extends the slice-2a surface behind the same `gleaph` feature gate:

```gql
GRANT READ ON GRAPH social
  FOR (p:Post) WHERE p.visibility = 'public' OR p.owner = MSG_CALLER()   -- see §2: AND-only in 2a
  TO PRINCIPAL …;
GRANT TRAVERSE OUTGOING ON GRAPH work EDGES MEMBER_OF TO PUBLIC;          -- unconditional form unchanged
```

(If OR is desired on day one, the bounded same-property disjunction lowering from ADR 0034's
search slices is reusable; default plan ships AND-only.)

### 4. Semantics: always AND, subsets are not errors

Policy predicates are additional conjuncts over the granted resource, independent of user
`WHERE` content. Filtered-out rows are absent results — the authorized-subset contract of
[ADR 0074] §4.6 — never errors. Planner-level implication analysis (eliding a user predicate
provably implied by a policy, or vice versa) is out of scope: both remain in force.

### 5. Evaluation: compile to plan ops with resolved constants — Router-side, policy-blind below

At execution, after plan build and requirement coverage, the Router lowers each attached
policy predicate into ordinary plan machinery with `MSG_CALLER()` substituted by the caller
principal **as a literal constant**:

- Equality on an indexed property → the existing index-scan seeding path receives the
  resolved value (the index canister executes a lookup it cannot distinguish from any other).
- All remaining comparisons → ordinary `PropertyFilter` ops inside the dispatched plan.
- Shards execute filters they cannot distinguish from user-authored ones. No caller identity,
  policy object, or policy engine crosses the Router boundary. Wire change: none beyond what
  plan args already carry.

`MSG_CALLER()` therefore gains a second resolution site: policy compilation at the Router
(execution-time resolution inside shards remains for user predicates). Prepared queries
re-resolve constants per invoking caller at each execution; their stored static requirement
sets are unaffected (policies constrain outputs, they add no requirements).

Instruction cost rides normal plan ops under existing budgets; no new accounting surface.

### 6. Authority and governance

Attaching, altering, or removing a predicate follows the existing grant authority (registry
owner in Phase 1 terms) through `GRANT`/`REVOKE`. There is no separate policy-administration
capability in this slice; break-glass/JIT elevation of metadata remains governed by [ADR 0074]
§1b Phase 2 (separate work item, not this ADR).

### 7. Vector search and GraphRAG contract (mechanics deferred)

Conditional policies bind vector search and GraphRAG: candidate generation and context
assembly must observe policy-filtered visibility. The mechanics — oversample-and-filter
versus constrained seeds — are deferred to the vector-authorization ADR, which inherits this
contract plus the boundary rule that the vector canister stays policy-blind.

## Consequences

Positive:

- Row-level visibility on shared graphs without partitioning; the demonstrated product need.
- Indexes accelerate policy enforcement through their ordinary lookups; no policy logic leaks
  into index/vector domains.
- Introspection, revoke, and explainability stay single-store (grant rows carry conditions).
- Shards and wire protocols remain unchanged in trust terms; the shrink attack surface of the
  current model is preserved.

Trade-offs accepted:

- AND-only composition initially: overlapping policies can only narrow, never offer
  alternatives (OR arrives with union lowering later).
- Predicate evaluation consumes shard cycles like any filter; heavy predicates are the
  graph owner's choice, priced by existing budgets.
- Constant substitution means prepared plans are caller-shaped per invocation; cached-plan
  reuse across callers must key on resolved constants (cache key gains caller-derived
  components where plans embed them).
- Catalog renames/removals interact with predicates exactly as with grants: monotonic IDs and
  the Plan 0291 sweep discipline apply to predicate references.

## Alternatives considered

- **Standalone `POLICY` objects referenced by grants** (original proposal §19): rejected —
  duplicated rule identity/lifecycle/introspection versus grant rows (SSOT violation).
- **Shard-side policy evaluation with propagated caller identity** (§29-style signed
  contexts): rejected — invades every wire protocol against the settled trust model.
- **Router post-fetch filtering**: rejected — pulls unauthorized values into Router memory,
  destroys selectivity, contradicts "authorization before fetch".
- **Planner-level predicate implication/elision**: deferred — correctness-sensitive
  optimization with no demonstrated need yet.
- **Per-caller materialized visibility structures**: premature — no measured need.

## Migration

Pre-production destructive evolution consistent with prior slices:

- Grant row encoding gains the optional predicate field; fresh state required (no decode shims).
- Grammar/parser/formatter/flags extended behind the `gleaph` feature gate following the
  established precedent.
- New PocketIC suite: owner-policy visibility matrix (own/public visible, others' hidden rows
  absent), index-seeded equality pushdown returning identical results to residual filtering,
  prepared re-resolution across two callers, non-owner grant attempts failing closed.
- `design/gql/extension-syntax.md` and `design/security/rbac-and-prepared.md` updated in the
  same patch.

## Design documentation impact

- Extends [ADR 0034]'s dialect contract with the conditional selector surface.
- Feeds the vector-authorization ADR (next in Phase 2) with the binding contract from §7.
- Follow-ups opened: OR/disjunction lowering; ReBAC `EXISTS` policies (bounded traversal);
  JIT metadata elevation ([ADR 0074] §1b Phase 2 item, independent of this ADR).

[ADR 0028]: 0028-per-graph-tenancy-metadata-reads.md
[ADR 0034]: 0034-gleaph-gql-extension-syntax.md
[ADR 0074]: 0074-data-plane-authorization-core.md
