# 0089. Permission-aware label-bucket restriction for unconstrained vertex scans

Date: 2026-08-29
Status: proposed
Last revised: 2026-08-29

## Context

Gleaph's data-plane authorization (ADR 0074) is enforced at plan time: the Router walks the
physical plan and extracts a static [`RequirementSet`] of privilege rows, then evaluates it
against `caller ∪ PUBLIC`. A vertex read whose bound label is not a single resolvable label
name — an unconstrained `(a)` / `MATCH (n)`, a positive multi-label `(a:A|B)`, a `NOT`/wildcard
label expression — cannot be attributed to exactly one grantable resource, so the walker marks
the demand set **tenancy-only**: owners and admins proceed, everyone else is denied with a
uniform non-disclosing `Forbidden`.

This fail-closed rule is safe but coarse. It means a caller who holds `MATCH` on some labels
cannot run an unconstrained scan at all, even though they are entitled to see the vertices of
the labels they hold. The graph explorer's whole-graph load (`MATCH (a)-[e:L]->(b)` with
unconstrained endpoints) is exactly such a query, and it is denied for non-owners.

The reference graph database (Neo4j) instead treats broad reads as **partial visibility**: an
unconstrained `MATCH (n)` returns only the elements the caller is authorized to see, and a
wildcard grant (`NODES *`) lets an administrator grant "match any vertex" in one row. Reads
never fail the whole query; unauthorized elements are simply invisible.

Gleaph's storage makes the Neo4j model cheap: vertices are grouped and indexed per label
(ADR 0004 label postings), and the executor scans exactly the labels carried in the Graph
request's `resolved_vertex_labels`. The authorization granularity (label) therefore aligns with
the storage granularity (label bucket), so "row-level filtering" degrades to **bucket-level
selection** — restrict the request's label set to the caller's grantable labels, and the
executor needs no change.

## Decision

Adopt a **permission-aware label-bucket restriction** for unconstrained and positive
multi-label vertex scans, plus an optional **wildcard vertex-label grant**. Enforcement stays
plan-time-static on the specialized request; the caller-dependence is confined to request
construction.

### 1. Plan specialization at the router (the core mechanism)

When the Router builds the physical plan for a query, it rewrites an unconstrained or positive
multi-label vertex scan into a set of per-label scans restricted to the caller's effective
`MATCH`-granted vertex labels. The executor then scans only those labels. A caller with `MATCH`
on labels `{A, B}` running `MATCH (n)` sees exactly the `A` and `B` vertices — the Neo4j
"smaller graph" behavior — with no executor change.

- The specialization applies to unconstrained scans and to positive multi-label scans whose
  candidate set is a union of concrete labels.
- `NOT`/wildcard label expressions (a complement, not a bucket union) remain tenancy-only
  (fail closed), unchanged.
- A caller with zero grantable vertex labels scans zero buckets and sees an empty result.

> **Correction (2026-08-29):** the original wording of this ADR said the restriction is applied
> by intersecting the request's `resolved_labels.vertex` with the caller's grantable labels.
> That is wrong: the graph shard uses `resolved_labels` only for name→id resolution, and the
> executor's unconstrained `NodeScan { label: None }` scans every vertex regardless of that
> table (`execute_node_scan`). The restriction must therefore be applied at the **plan** level
> (rewrite the unconstrained scan into per-label scans of the caller's grantable labels), not
> at the request's resolved-label table.

### 2. Walker marker for unconstrained scans

`require_vertex_scan_rows` no longer marks an unconstrained/multi-label scan tenancy-only.
Instead it emits a **marker demand** ("the caller must hold `MATCH` on at least one vertex
label"). This keeps the plan-time walker meaningful (a caller with no vertex grants is
rejected) and provides the durable marker for prepared-query gating. The precise per-bucket
enforcement is the request-build restriction.

### 3. Prepared-query gating

The durable `RequirementSet` embedded at prepared registration records the marker for an
unconstrained scan. Publication gating treats it as "the caller must hold `MATCH` on at least
one vertex label." At execution the request-build restriction applies per executing caller
(the same path as raw queries); the "primary checked set at prepared execution" defers the
marker to that request-level restriction.

### 4. Property reads stay atomic (no NULL substitution)

Partial visibility applies to **topology** (which labels a caller can match), not to protected
property values. A query that projects a property the caller cannot `READ` on a scanned label
is **denied** — the existing ADR 0074 §2 contract ("unauthorized properties are never
substituted with NULL") is preserved. Neo4j's NULL-substitution behavior is a deliberate
non-goal.

### 5. Wildcard vertex-label grant

Add `NODES *` (all vertex labels) as a grant resource, so an administrator can grant "match
any vertex" in one row instead of enumerating every label. A wildcard grant means the caller's
grantable vertex-label set is the full catalog set. This is additive and orthogonal to the
bucket restriction.

## Consequences

- **Positive**: non-owner callers can run unconstrained scans over the labels they hold;
  the graph explorer's whole-graph load works for a caller granted all labels; administrators
  get a convenient wildcard grant. The change is low-cost because label buckets align with the
  grant unit.
- **Negative**: partial visibility lets a caller observe which of their *granted* labels have
  data (a count side-channel). This is inherent to partial visibility and matches Neo4j; it
  does not leak beyond the caller's granted labels. The denied case still returns a uniform
  non-disclosing `Forbidden`.
- **Contract change**: the ADR 0074 Phase-1 rule "unlabeled scans are tenancy-only" is refined:
  unconstrained and positive multi-label scans become marker-gated + bucket-restricted;
  `NOT`/wildcard and ambiguous-variable reads remain tenancy-only.
- **Prepared execution** must handle the marker (defer to request-level restriction). This is a
  bounded change to the prepared-execution check.

## Alternatives considered

- **Coarse conjunctive demand** (unconstrained → require `MATCH` on every graph label): simpler
  (no request-build change) but denies partial callers entirely; does not deliver the Neo4j
  partial-visibility UX. Rejected in favor of bucket restriction.
- **Runtime per-row filtering in the executor**: the Neo4j literal model, but unnecessary
  because label buckets already align with the grant unit; would add per-row overhead and
  executor changes. Rejected.
- **Owner-only whole-graph load** (the explorer connects as the owner): no authz change, but
  keeps broad reads unavailable to non-owners and does not deliver the general capability.
  Rejected as the primary mechanism (still available as a fallback).

[`RequirementSet`]: ../../crates/router/src/authz.rs
