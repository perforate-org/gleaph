# 0072. `!=` predicates on indexed properties stay residual filters; complement pushdown is deferred with named triggers

Date: 2026-08-22
Status: proposed
Last revised: 2026-08-22
Anchor timestamp: 2026-08-22 15:55:55 UTC +0000

## Context

Facts verified against current main `3c75d2fb5` (2026-08-22); all citations re-checked on this
commit after the survey originally recorded them at `8e392c127`:

- **Anchor rejection is total and deliberate.** Every seed-anchor family refuses `CmpOp::Ne`:
  one-sided range bounds (`SeedRangeBound::try_from_cmp` maps `Eq | Ne => None`,
  `crates/router/src/seed.rs:216`), vertex equality anchors (the `PlanOp::IndexScan` arm guards
  `if *cmp == CmpOp::Eq`, `crates/router/src/seed.rs:1171`), vertex intersection arms
  (`scan.cmp != CmpOp::Eq => Ok(None)`, `crates/router/src/seed.rs:1130`), and edge prefix arms
  (`CmpOp::Ne => Ok(None)` at `crates/router/src/seed.rs:340`, `CmpOp::Ne => None` at
  `crates/router/src/seed.rs:786`). No complement anchor kind exists anywhere in the seed surface.
- **Planner statistics already model inverse selectivity**:
  `CmpOp::Ne => 1.0 - self.equality_selectivity()`
  (`crates/gql-planner/src/stats.rs:132`). Nothing consumes that estimate for pushdown; it only
  prices plans among non-index alternatives.
- **Evaluation today:** an `!=` predicate rides as a residual filter over whatever scan produced
  candidate rows (label scan, another index anchor, or seeded rows revalidated by the graph shard).
- **No difference primitive exists.** graph-index offers paginated equality, domain-clamped ordered
  intervals (`PostingRangeRequest::Between`, plan 0270), intersection lookups, and counts; there is
  no difference/complement endpoint in `crates/graph-index/src/`.
- **Domain bounds exist for any future complement.** `gql::range_bounds`
  (`crates/gql/src/value_index_key.rs:336`) supplies exact comparison-domain floors and ceilings and
  rejects unsupported domains with `ValueIndexKeyError::UnsupportedRangeDomain`; its contract comment
  makes it canonical ("Router and Property Index must not duplicate tag or ordering knowledge").
- **Semantics constraint (GQL three-valued logic):** `n.p != v` evaluates UNKNOWN when `p` is absent
  or NULL, excluding the row. Postings exist only for present, indexable values — nulls are already
  excluded from postings (the "Index-only miss" rule,
  [property-index.md](../index/property-index.md)). A complement computed over postings inside one
  comparison domain therefore matches GQL semantics without extra NULL handling.
- **Interaction surface:** ADR 0034 SEARCH disjunction machinery accepts equality and range leaves
  of two to eight arms (`crates/router/src/gql_search.rs:298`); there is no complement leaf kind.

## Problem

Every slice through plans/0278 treated `CmpOp::Ne` as never-anchorable without a recorded decision,
so reviewers cannot distinguish policy from omission. The cost consequence is real but bounded:
an `!=`-leading MATCH on a federated multi-shard graph takes the no-index-anchor error path, and on
a single shard it degrades to a label scan even when the negated value dominates the property's
distribution. Correctness is never at stake today — residual filtering is spec-conformant.

## Existing Architecture Assessment

Residual filtering already produces correct results, so the demonstrated gap is cost on unmeasured
workloads plus documentation debt, not a functional hole. The planner's inverse-selectivity estimate
shows cost-based reasoning exists without a pushdown consumer. Extending the seed surface with a
complement anchor or adding a storage-side difference endpoint would each introduce new machinery
(new anchor kind + collector subtraction, or new Candid surface) with no benchmark evidence that
`!=`-leading queries matter. Under the architecture-preservation bias, the burden of proof sits with
the change, and that proof does not exist yet.

## Alternatives

### A. Status quo — filter-only

`!=` never anchors; predicates stay residual filters. Zero new machinery; worst case remains full
label scans when `!=` leads. Cost cliff is real but unmeasured.

### B. Client-side complement pushdown

The Router collector drains the property's comparison-domain interval through the existing paginated
range endpoint, fetches the negated value's equality bucket through `lookup_equal_page`, and
subtracts it before seeding shards. No wire changes; costs one extra equality call per predicate.
Correct by construction for absent/NULL (postings only cover present values). Costs: a full-domain
drain per `!=` predicate regardless of selectivity, subtraction state in the Router collector, and
derived-posting lag between drain and subtract (the same bounded lag equality anchors already
accept).

### C. Storage-side difference endpoint

A new Candid surface on graph-index (for example a range walk excluding one encoded value). Fastest
execution — the index walks ordered postings once and skips the excluded value inline — but adds a
new read primitive, its own pagination/resume contract, and wire surface.

## Decision

**Option A is accepted as standing policy: `CmpOp::Ne` does not form index anchors and is served by
residual filtering. The rejection sites cited above are intentional contract, not gaps.**

If this decision is ever reopened, **Option B is the designated path**: complement composition is
owned by the Router collector over existing paginated endpoints, preserving the current boundary —
graph-index remains equality/range/intersection-only and gains no complement Candid surface. Option
C is rejected now and stays rejected unless Option B proves measurably insufficient (see triggers).

## Invariant: NULL and three-valued-logic semantics

Any future implementation under this policy MUST preserve:

1. `n.p != v` excludes rows where `p` is absent or NULL (UNKNOWN), matching GQL three-valued logic.
2. Complements are computed **within one comparison domain interval** derived via
   `gql::range_bounds`; cross-domain postings must never leak into a complement result.
3. Domains that reject with `UnsupportedRangeDomain` (Bool, List, Record, Path, Extension, Null,
   Duration) keep their `!=` predicates on the residual-filter path even if complement pushdown
   exists for supported domains.

These hold today because postings exist only for present values; they are recorded here because they
are the correctness argument any implementer needs and existed nowhere else.

## Consequences

### Positive

- The ambiguity is closed: `!=` non-anchoring is a recorded policy with cited rejection sites, so
  future slices and reviews stop relitigating it.
- No Candid, wire, storage, planner, or router changes are made without demonstrated demand,
  honoring the pre-production simplicity policy.
- The NULL/three-valued-logic invariant and the domain-clamped complement design are preserved for
  whoever implements Option B, with the owner boundary already named (Router collector).
- The planner's inverse-selectivity estimate keeps its documented role: cost pricing among
  non-index alternatives.

### Accepted costs

- `!=`-leading MATCH on federated multi-shard graphs still takes the no-index-anchor error path, and
  single-shard execution may degrade to a full label scan when the negated value is dominant. This
  is accepted until a trigger below fires.
- The feasibility work (domain-clamped complement over existing paginated endpoints) remains
  unimplemented; the knowledge lives only in this ADR.

## Revisit triggers

Reopen this decision when **any** of the following holds:

1. **Measured workload:** a benchmark or canbench suite shows `!=`-leading MATCH on an indexed
   property dominating wall time versus the same query expressed through range/equality anchors,
   AND planner histograms show the negated value holding high equality selectivity (so a
   domain-clamped complement scan would be substantially smaller than the label scan it replaces).
2. **SEARCH extension demand:** ADR 0034 disjunction machinery needs complement arms as leaves;
   today's equality/range leaf kinds cannot express them, which would force per-arm residual
   filtering inside SEARCH evaluation.
3. **Cost-model change:** the planner begins consuming inverse selectivity for anchor selection, at
   which point a filter-only policy would make those estimates dead weight unless a complement path
   exists to consume.

## Migration

None. This ADR records policy only; no code, wire format, or storage changes accompany it, and no
follow-up implementation plan is spawned by this document.

## Design Documentation Impact

- [property-index.md](../index/property-index.md) — pointer from the seed-routing section to this
  ADR (done in this slice).
- [README.md](README.md) — index row for 0072 (done in this slice).

## Required Axes Impact

- Encapsulation: graph-index keeps its equality/range/intersection API surface; any future
  complement composition stays behind the Router collector boundary.
- Separation of concerns: value-domain ordering knowledge stays in `gql::range_bounds`; neither the
  Router nor graph-index duplicates it.
- Invariants: the NULL/three-valued-logic rule above becomes the named contract for future pushdown.
- Consistency: unchanged; residual filters revalidate against canonical state exactly as before.
- Fitness for purpose: documents the cheapest correct behavior and names measurable conditions under
  which more machinery would be justified.
