# 0083. Expired elevation-row retention and GC in the grant store

Date: 2026-08-25
Status: implemented (2026-08-26)
Last revised: 2026-08-26

## Context

[ADR 0080] made the elevation grant row the record of privileged metadata
access: the row carries requester, approver, justification, scope, window, and
emergency flag, and introspection (`list_elevations`, `list_graph_grants`) lists
active and recently-expired elevations. The **review surface already exists** —
the grant store is the record and the review audience can query it.

What is missing is **boundedness**: [ADR 0080] says "expired rows stay stored
until GC," but expired-grant GC is not implemented. Expired elevation rows
accumulate in the grant store — bounded in practice by low elevation frequency,
but unbounded in principle. [ADR 0074] §1b explicitly deferred "expired-grant
GC designed together with the audit-log retention contract" to this ADR.

This ADR formalizes that retention contract and the GC mechanism **in the grant
store**. It does not add a separate audit-log store, grant/revoke/caps history,
or a unified time-ordered view — those are deferred until DAO governance is
actually designed (speculative today, not a demonstrated need).

## Existing architecture assessment

Preserved as-is:

- **The grant row is the record** ([ADR 0080] §4): elevation rows carry the full
  evidence payload; no separate audit store exists or is needed for review.
- **Review surface exists**: `list_elevations` (`MANAGE_AUTHORIZATION`) and
  `list_graph_grants` list active and recently-expired elevations with evidence.
- **Expiry is automatic**: expired rows read as absent via expiry-aware
  evaluation (`GrantState::holds`); reversion needs no human action.
- **Autonomous timer precedent**: `recovery::arm_if_needed()` arms a router-side
  timer (`ic_cdk_timers::set_timer`, re-armed on init, does not survive upgrade)
  — the pattern for a maintenance GC driver.

Missing machinery (the demonstrated gap):

- No GC removes expired elevation rows; the grant store's elevation portion
  grows without bound in principle.
- No retention contract defines how long expired rows are kept for post-use
  review before GC.

## Decision

### 1. No separate audit store; formalize grant-store retention

The review surface is the grant store itself. This ADR adds **no** new stable
region, no second write path, and no grant/revoke/caps history. It formalizes
the retention window and the GC mechanism for expired rows in the existing
grant store (MemoryId 55).

### 2. Retention contract

All rows whose `expires_at` has passed are retained for a **bounded review
window**, then GC'd. Elevation rows are the only such rows today, but the rule
is generic over `expires_at`: a time-boxed row (including any future
grammar-written expiring grant) is time-boxed precisely so it stops counting,
and GC generalizes over that property rather than special-casing evidence rows.
The window defaults to a **constant 90 days after expiry** in v1 — not
configurable; configurability waits for a demonstrated need. Elevation
frequency is low, so the grant store's expired-row portion stays small
(single-digit MB) within the window.

### 3. GC mechanism

- An **autonomous timer** modeled on `recovery::arm_if_needed()`
  (`ic_cdk_timers::set_timer`, re-armed on init) removes expired rows past the
  review window, at a low frequency (daily-scale).
- The driver is **idempotent and bounded per tick** (a bounded slice of rows per
  tick, resuming on the next tick), so a large backlog is drained over multiple
  ticks without a single long call.
- GC is **best-effort**: the grant store remains bounded by elevation frequency
  even if the timer is delayed; the review window is the target, not a hard
  bound.
- **Enumeration**: `expires_at` lives in the row value, not in `GrantKey`
  (`op ‖ resource ‖ subject`), so finding expired rows means scanning the store
  rather than range-seeking on expiry. The driver walks keys in canonical order
  behind a resumable cursor, removing rows whose expiry passed the window; the
  cursor is heap-resident and restarts from the beginning after an upgrade,
  which is safe because a pass is idempotent. One pass costs O(grant-store)
  reads at daily scale — acceptable because the store holds standing grants
  plus a small expired tail, and the timer runs off the query hot path.
- **Why not piggyback on existing maintenance:** there is no always-on
  maintenance tick to ride. The recovery driver arms only when saga work exists
  (work-driven, not a heartbeat), and `GLEAPH.DRAIN_DEFERRED_MAINTENANCE()` is
  an operator-called procedure — hosting GC there would make boundedness depend
  on saga activity or operator diligence. If a general maintenance heartbeat is
  introduced later, migrating the driver onto it is a reasonable simplification.

### 4. Read surface unchanged

`list_elevations` / `list_graph_grants` are unchanged. No new endpoints, no
public read path.

### 5. Invariants

1. **The grant row remains the record**: no second store, no duplicated
   history.
2. **Bounded expired rows**: expired rows are GC'd after the review window; the
   grant store's expired-row portion is bounded.
3. **Review window preserved absent reissuance**: rows are retained for the
   full review window before GC, so post-use review ([ADR 0080] §3 stage 5) is
   honored **unless the same (subject, resource) row is reissued first** —
   `GrantKey` carries no issuance time, so a reissued elevation overwrites the
   prior row's evidence (see Trade-offs).
4. **Idempotent, bounded GC**: the driver is safe to run repeatedly and bounded
   per tick.

## Consequences

Positive:

- Bounds the grant store's expired-row portion without a new store or write
  path.
- Preserves the post-use review window ([ADR 0080] §3 stage 5) with a defined
  retention contract.
- Closes [ADR 0074] §1b's deferred "expired-grant GC designed together with the
  audit-log retention contract."
- Minimal: no new stable region, no new endpoints, no second write path.

Trade-offs accepted:

- Review history is bounded by the review window: rows are GC'd after it, so
  older elevation history is not queryable.
- No grant/revoke/caps history and no unified time-ordered view — deferred until
  DAO governance is actually designed.
- GC is best-effort (timer-driven); the grant store stays bounded by elevation
  frequency even if GC is delayed.
- **Reissue supersedes**: re-elevating the same requester for the same scope
  writes the same `GrantKey` and destroys the prior evidence even if it was
  still within its review window. This is pre-existing [ADR 0080] behavior,
  accepted as v1 semantics; durable history needs issuance-time keys (a layout
  change) and rides with the deferred audit-history question.

## Alternatives considered

- **Separate audit-log store** (elevation + grant/revoke/caps history, unified
  time-ordered view): rejected — the DAO-governance need is speculative today;
  the grant store already serves the committed review contract. Revisit when
  DAO governance is designed.
- **Elevation-only separate store**: rejected — the grant store already holds
  the rows; a second copy is duplication with no added review value.
- **No GC (keep expired rows forever)**: rejected — unbounded growth in
  principle, though low in practice; the retention contract is the point of
  this ADR.
- **Lazy GC on access** (delete expired-past-window rows opportunistically when
  the mutation path touches them): rejected — authorization checks also run in
  query context, which cannot mutate stable memory, so coverage would be
  partial and boundedness would depend on access patterns.
- **Secondary expiry index** (an `(expires_at, …)`-keyed collection for
  O(log n) enumeration): rejected — a new stable region plus a second write
  path on every grant write, to accelerate a daily background pass over a small
  store; premature.

## Migration

- Add the GC driver and retention window; **no layout change** (GC only removes
  rows, so fresh state is not required). Implemented as `GrantState::sweep_expired_rows`
  in `crates/auth` plus the autonomous driver in `crates/router/src/retention.rs`.
- New PocketIC suite (`adr0083_elevation_retention`): expired loop-issued elevation
  rows are GC'd only after the review window; grammar-written standing grants (data-plane
  and `READ_METADATA`) are never touched — `GRANT` writes standing rows only ([ADR 0080]
  §5), so the sweep rule's genericity over an expiring evidence-free metadata row is
  pinned at unit level instead; **re-issuing the same (requester, scope) elevation
  replaces the prior row's evidence (supersession semantics)**; the review
  surface is preserved within the window; GC drains a backlog over multiple ticks behind
  its resumable cursor; `list_elevations` /
  `list_graph_grants` are unchanged; adversarial walk asserts no public mutation path.
- `design/security/rbac-and-prepared.md` updated in the same patch.

## Design documentation impact

- Closes [ADR 0074] §1b's "expired-grant GC designed together with the
  audit-log retention contract."
- Fulfills [ADR 0080] §3 stage 5's post-use review contract with a defined
  retention window.
- Deferred (until DAO governance is designed): separate audit-log store,
  grant/revoke/caps history, unified time-ordered view, tamper-evidence,
  policy-decision logging.
- Carried to the same revisit: elevation records are keyed by
  `(op, resource, subject)` without issuance time, so successive elevations of
  the same pair cannot coexist; durable history requires issuance-time keys (a
  grant-key layout change).

[ADR 0074]: 0074-data-plane-authorization-core.md
[ADR 0080]: 0080-jit-metadata-elevation.md
