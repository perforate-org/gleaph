# 0081. Index-ordered ORDER BY delivery for existence-guaranteed single-key vertex sorts

Date: 2026-08-25
Status: accepted (Slice A; DESC and declared-sort-key extensions deferred)
Last revised: 2026-08-25

## Context

Every `ORDER BY` today materializes its full input and re-sorts it. The planner already owns
`Sort`, `Limit`, and a heap-fused `TopK` op (`gql-planner/src/plan.rs:609-654`; emission sites
`planner/match_plan/result.rs:133,235`, `planner/match_plan.rs:540`), so memory is bounded for
top-k shapes — but scan work is not: all matching rows are produced before ordering.

Meanwhile the property posting index already stores each indexed property as an encoded-key
ordered map. Range scans walk it ascending (`PostingRangeRequest::Between`,
`PostingHitPage` cursor pagination), equality/prefix anchors read contiguous or point regions of
the same order, and `execute_index_scan` binds rows in hit order (`scan/index.rs`). The order the
queries want already falls out of the access path; no plan op currently claims it.

The property index is sparse by invariant: `Value::Null` produces no key bytes
(`value_index_key.rs:59`), so rows whose sort property is absent never appear in postings.
This is the same model as Neo4j ("null is absence; indexes do not store nulls"), MongoDB
sparse indexes, and Elasticsearch unmapped/null fields.

Industry practice for ordered delivery:

- PostgreSQL delivers B-tree output in key order (backward scan = DESC) and stops reading once
  `LIMIT n` rows are produced; MySQL prefers an ordered index for `ORDER BY ... LIMIT`
  (`prefer_ordering_index`).
- Neo4j permits range-index-backed ORDER BY only when the planner can rule out nulls — via a
  predicate such as `IS NOT NULL` or a declared existence constraint. Index-backed ORDER BY
  without any presence guarantee has stayed unimplemented there since 2022 (issue #12812)
  precisely because the index cannot return rows it does not contain.
- Elasticsearch/Atlas Search expose explicit missing-value placement (`missing: _first/_last`,
  `noData`) instead of pretending absent rows participate in index order.

## Problem

`MATCH (v:X) WHERE v.p >= 10 RETURN v ORDER BY v.p LIMIT 20` pays full-input production plus a
sort even though the driving index scan yields exactly those rows already ordered by `p`. The
planner neither detects that the anchor property equals the sort key nor records ordered delivery
anywhere downstream (wire, execution, seeding), so nothing can skip the redundant work.

Any solution must respect the sparse-posting invariant: if rows lacking `v.p` can reach the
result set, an index scan cannot deliver them, so index-ordered delivery is only correct when
some predicate guarantees every result row carries `v.p`.

## Existing Architecture Assessment

No new subsystem is required:

- The order source exists (posting range walk over the encoded domain).
- Cursor pagination exists (`PostingHitPage.next`), giving bounded-memory streaming.
- Anchor selection already computes the driving scan and knows its variable+property
  (`anchor.rs`, `filters.rs`, vertex fusion paths landed with ADR 0073 slices and the IN-list /
  STARTS WITH anchors).
- `TopK` already fuses Sort+Limit; what changes is that its input contract becomes "sorted by
  the same key" and it may stop at the first tie-group boundary past k survivors.
- Shard fan-out already paginates per shard with cursors; a small R-way merge over per-shard
  cursors is an extension of existing federation binding code, not a new layer.

Therefore the change is confined to: planner eligibility detection + one wire-visible intent on
the scan op + executor consumption (merge/elide) + explain/cost visibility. No storage, catalog,
or DDL surface changes in this slice.

## Alternatives

1. **Minimum-change (chosen): ordered-delivery intent on the existing anchor scan.**
   When the single sort item is a bare `PropertyAccess(var, p)` with ASC direction and the
   leading scan's anchored property equals `p` (equality, IN, range, or prefix anchor), record
   ordered-by-sort intent on that scan op. Execution consumes the posting stream through an
   R-way merge over per-shard cursors; `Sort` is elided, `TopK` early-terminates at the first
   tie-group boundary after k survivors.
   Benefits: no new storage, no new query syntax, correctness argument local to one op pair.
   Drawbacks: eligibility proof lives in the planner; tie-boundary rule must be tested.
2. **Declared sort keys / existence constraints (deferred).** Developer-declared canonical sort
   keys with null placement (Neo4j existence constraint / Atlas `noData` generalization) would
   extend ordered delivery to queries *without* a presence-guaranteeing predicate by merging
   absent-property rows from a separate source. Rejected for now: new DDL surface, new merge
   semantics, and no demonstrated demand yet beyond the general feature ask.
3. **Physical clustering (shard-local sorted column).** Store vertices in sort-key order so
   scans come out sorted without the posting map. Rejected: rewrite cost on every write, fights
   Lara placement contracts, and duplicates ordering knowledge the posting map already owns.
4. **DESC now via backward iteration API.** Requires new graph-kernel traversal plus a verified
   cross-shard descending merge. Deferred to Slice B; until then DESC queries keep `Sort`/`TopK`.

## Decision

Slice A adopts alternative 1 with these pinned rules:

1. **Eligibility.** Exactly one sort item; its expression is a bare `PropertyAccess(var, p)`;
   direction ASC; a driving anchor on the same `var`+`p` exists among equality | IN | range |
   prefix vertex anchors; `var` is the leading scan variable; no `DISTINCT` or other reordering
   op sits between scan and sort site. `null_order` annotations are accepted and vacuous
   (every delivered row has `p`).
2. **Intent on the wire.** The scan op records ordered-by-sort intent (new field on the vertex
   scan op variant; exact shape delegated under the pre-stability wire policy). Wire convert,
   plan_wire_guard, explain, and cost are updated in the same slice.
3. **Execution.** Ordered delivery merges per-shard cursor streams with an R-way heap at the
   federation bind boundary and emits rows globally ascending. With elided `Sort`, rows stream
   directly; with `TopK`, consumption stops at the first value strictly greater than the k-th
   survivor (tie groups are never split mid-boundary); residual filters do not invalidate early
   termination under that boundary rule. OFFSET skips forward within the merged stream.
4. **IN anchors** concatenate per-element blocks sorted by encoded payload bytes (encoded byte
   order equals domain order by construction); equality is trivially ordered; range/prefix are
   contiguous intervals.
5. **Out of scope:** DESC (Slice B), multi-key sorts (future incremental-sort-style pass),
   edge-side sorts, existence-constraint/declared-sort-key surface (Slice C, future ADR).

## Consequences

Positive: top-k and ranked-read patterns lose O(n log n) sorting and full materialization;
bounded memory holds for arbitrary input sizes; the sparse-posting invariant is enforced by an
eligibility proof instead of a runtime check; explain output makes ordered delivery auditable.

Trade-offs: planner gains an eligibility proof that must track residual/reorder ops between scan
and sort site; the executor gains an R-way merge and a tie-boundary termination rule that need
dedicated tests; the scan op grows one field (wire/guard/explain churn absorbed by the
pre-stability policy). DESC and multi-key users see no change yet.

## Migration

None. No stored layout, catalog, or DDL change; the wire addition follows the pre-stability
no-bump policy. Feature is planner-opt-in: ineligible plans compile exactly as before.

## Design Documentation Impact

- `design/gql/plan-format.md`: ordered-delivery field on the scan op + ordering contract.
- `design/execution/operators.md`: Sort/TopK elision note.
- `design/implementation-gaps.md`: priority-table row tracking this slice.
