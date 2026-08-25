# 0078. Authorization-aware vector search: oversampled visibility filtering with iterative deepening

Date: 2026-08-24
Status: implemented
Last revised: 2026-08-24

## Context

Phase 1 of [ADR 0074] left vector search explicitly ungated ("vector-search lookups stay
ungated until Phase 2"): `SEARCH … IN (VECTOR INDEX …)` candidates are fetched by score order
and only whatever the downstream plan happens to enforce is applied afterward. Two problems
follow:

1. **Correctness**: the naive flow — ANN top-k, then filter — returns the top-k of *all*
   vertices, filtered. When unauthorized or policy-hidden vertices rank highly, the caller
   receives fewer than k rows even though the authorized subset has more, and the returned
   set is not the true top-k of anything.
2. **Coverage**: vertex-embedding subjects and edge-inline byte-vector subjects
   (`GLEAPH.VECTOR.*`) had no defined authorization rule at all; GraphRAG context assembly
   could ingest unfiltered retrieval output.

[ADR 0075] §7 fixed the contract this ADR implements: candidate generation and context
assembly must observe policy-filtered visibility, while the **vector canister stays
policy-blind** and no caller identity propagates below the Router ([ADR 0028] trust model).
The vector canister also cannot evaluate policies structurally — predicate property values do
not exist in its subject space.

Execution surface today (verified): `crates/router/src/gql_search.rs` lowers leading and
non-leading `SEARCH` shapes into `VectorSearchRequest` dispatch (`VectorSearchHit`,
`VectorSubject`, `MAX_VECTOR_SEARCH_TOP_K`, `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` from
`gleaph-graph-kernel::vector_index`); leading-search hits become row-shaped seeds for the
remaining graph-tail plan, which is dispatched through normal `ExecutePlanArgs`. Since slice
2b, requirement coverage runs post-plan-build on that tail; since [ADR 0075], policy
predicates lower into ordinary plan ops.

## Decision

### 1. Three authorization layers

| Layer | Rule | Site |
| --- | --- | --- |
| Query admission | Vector search over graph `g` requires the same effective privileges as an equivalent `MATCH` over the source label(s), plus `READ_PROPERTY` on any projected embedding source property. Implemented by extending the slice-2b requirement walker so `PlanOp::Search` contributes to the `RequirementSet` like a scan. | Router, pre-dispatch |
| Per-candidate visibility | Every returned subject must satisfy caller ∪ PUBLIC grants **and** all attached policy predicates exactly as ordinary rows. | Tail-plan execution |
| Context assembly (GraphRAG) | Nothing enters LLM context that did not pass layer 2 through the same query machinery. No separate evaluation path exists to bypass. | Retrieval pipelines |

Layer 1 is a cheap catalog-only check that rejects unauthorized vector queries before any ANN
work; layers 2–3 share one implementation because filtering rides ordinary plan ops.

### 2. Correctness contract

Returned rows are the true top-k (or true top-m < k) of the **authorized subset**, monotone in
k. "Fetch k then truncate" is forbidden anywhere on this path.

### 3. Mechanism: the tail plan is the visibility filter — reuse, do not duplicate

Per-candidate visibility is **not** evaluated by new Router logic. Candidates seed the normal
graph-tail plan; grant coverage and lowered policy predicates execute as ordinary plan ops on
the owning shard ([ADR 0075] §5 machinery, unchanged). The Router's only added responsibility
is **iterative deepening**:

```text
round r (r = 0,1,2,…): request ceil(k · c^r) candidates (c ≈ 2) from the vector index
    → dispatch the tail plan with those seeds (visibility filters apply inside)
    → if ≥ k authorized rows: return exactly k
      elif candidates exhausted: return m < k with truncated marker (§4)
      elif instruction budget exhausted: return m < k with truncated marker
      else: r ← r+1
```

Rationale for reusing plan execution instead of a batch-probe endpoint: probe-based designs
require a second predicate evaluator (Router-side, duplicating op semantics — an SSOT
violation against [ADR 0075] §5) plus new internal wire surface. The tail plan already
evaluates everything correctly; deepening only changes how many candidates feed it.

### 4. Exhaustion semantics: partial results with an explicit truncated marker

GQL `LIMIT` is an upper-bound contract, so returning m < k authorized rows is legal; what is
forbidden is silence. Result metadata carries an explicit truncated indicator whenever rounds
stopped early (candidate exhaustion or budget exhaustion). Ordering remains deterministic:
candidates arrive score-ordered, so the authorized prefix is stable across identical
query/data states. This mirrors the partial-batch behavior of the dynamic instruction-budget
batching precedent ([ADR 0042]).

### 5. Candidate-cap integration: one limit, one owner

Deepening must not introduce a second knob beside
`MAX_VECTOR_SEARCH_FILTER_CANDIDATES`. Implementation rule: per-round request size ≤ the
existing constant; cumulative candidates across rounds are bounded by the query instruction
budget, not by a new constant. If implementation shows the constant currently serves a
different role, the relationship is documented at the constant rather than papered over with
a derived value. General principle recorded here: **new limits derive from existing limits,
or document their relationship — never coexist silently**.

### 6. Edge-inline byte vectors

Subjects may be edges (`GLEAPH.VECTOR.*` surfaces). Visibility rules are identical with the
edge translation: coverage requires the matching edge privilege including direction, and
policy predicates reference edge properties. The Phase 1 note "edge-inline bytes ride the
traversal row" becomes a precise rule: **bytes of visible edges may be read; bytes of
invisible edges never appear as candidates.**

### 7. Rejected alternatives, with re-evaluation triggers

| Alternative | Why rejected | Revisit trigger |
| --- | --- | --- |
| Pre-filtering inside the vector canister | Policy-blind boundary violation; property values absent from its subject space | Only if the canister later hosts generic, policy-agnostic constraint metadata |
| Per-subject candidate bitmaps / partitioned indexes | per-caller state; premature without measured need | Oversampling cost measured above threshold (record the threshold when first measured) |
| Router-side batch-probe evaluator | duplicates op semantics; new internal wire | If `EXPLAIN AUTHORIZATION` later needs Router-visible evaluations, reuse it for probes then |

## Consequences

Positive:

- Correct top-k over the authorized subset with zero changes to the vector canister, shard
  trust model, or wire protocols; identity still never propagates below the Router.
- Unauthorized callers are rejected before any ANN spend (layer 1), so admission gating also
  removes the current free-ANN hole.
- GraphRAG inherits enforcement by construction — context assembly goes through plans.
- Edge-inline vectors gain a defined rule, closing the documented Phase 1 limitation.

Trade-offs accepted:

- Deepening multiplies ANN + dispatch cost for restricted callers (up to the budget cap);
  hidden-heavy graphs pay more than open ones. The cost is visible via truncated markers and
  budgets, not silent.
- Fewer-than-k results are legitimate outcomes; applications relying on "always k" must read
  the truncated marker.
- Layer-1 admission means newly created embeddings are unreachable until the caller holds
  label privileges — intended, but a behavior change from today's ungated reads.

## Migration

Pre-production destructive evolution consistent with prior slices:

- Requirement walker gains `PlanOp::Search` handling (exhaustive match extended — compile-
  enforced).
- Deepening loop implemented at the `gql_search.rs` dispatch sites (leading and non-leading);
  truncated marker added to result metadata and wire response fields (additive wire evolution;
  router/shard version skew unaffected because the field is Router→caller only).
- New PocketIC suite: k-preserved-when-enough-authorized, hidden-heavy exhaustion returning
  partial+truncated, policy predicates applied to candidates (owner/public matrix reused from
  [ADR 0075]'s fixtures), edge-subject visibility, upgrade persistence not applicable (no
  storage change) — state expected counts before running.
- `design/security/rbac-and-prepared.md` vector section rewritten in the same patch.

## Design documentation impact

- Implements the binding contract of [ADR 0075] §7; cross-linked both directions.
- Opens follow-ups: GraphRAG context-assembly checklist referencing layer 3; OR-lowering
  interplay when [ADR 0075] grows disjunctions (policy OR-composition unions compose with IN
  union anchors — combined correctness test required then).

[ADR 0028]: 0028-per-graph-tenancy-metadata-reads.md
[ADR 0042]: 0042-router-dynamic-instruction-budget-batching.md
[ADR 0074]: 0074-data-plane-authorization-core.md
[ADR 0075]: 0075-conditional-policies-constant-pushdown.md
