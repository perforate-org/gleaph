# 0112. Router-enforced inference quotas

Date: 2026-11-12
Status: accepted (2026-11-12; implemented and verified)
Last revised: 2026-11-12
Anchor timestamp: 2026-11-12 13:20:00 UTC +0900

## Context

ADR 0111 makes the Router the owner of the deployment inference canister and proxies
`route_and_extract` for every inference-enabled graph. The Router pays the inference update call's
cycles. Any caller that passes graph tenancy and the per-graph flag can make the Router spend those
cycles; a long question costs more model work because the canister processes 32-token windows.

The repository has no request-rate or spend control today. Graph tenancy limits *which* data a
principal may read, not *how often* it may ask the deployment to run a model.

## Problem

- An inference-enabled graph exposes an unmetered, Router-funded compute path to every tenant.
- A single caller can drive the model as fast as ingress permits, consuming Router cycles and
  shard-independent compute.
- Question length changes cost by an order of magnitude (one 32-token window versus many windows),
  so a plain call counter would under-price long questions.

## Existing architecture assessment

The Router already owns authorization, graph tenancy, orchestration and the durable
deployment config (MemoryId 48). A quota is request policy evaluated in the same ingress that
already decides whether the graph may route. No model-side or graph-side state is involved.

Rate-limit state is explicitly **best-effort, not an authority**: it must not become a durable
SSOT that the rest of the system reasons about. It can be heap-only without weakening the
authorization boundary, and a heap reset on upgrade only grants a burst, not access.

## Alternatives

- **A. No quota.** Rejected: unbounded Router-funded compute for any tenant.
- **B. Durable per-caller token bucket in a new stable region.** Rejected for this slice: adds a
  MemoryId, a GC/eviction policy for unique callers, and backup/restore obligations for state that
  is not an authority. Reconsider if quotas must survive upgrades or be auditable.
- **C. Heap-only bounded fixed-window counter per `(caller, graph)` (chosen).** Bounded memory,
  no new region, resets on upgrade; adequate as abuse protection.
- **D. Stable map with timer GC.** Deferred; the correct shape if durable billing is introduced.
- **E. Per-graph limits.** Deferred; the deployment-wide default is one knob. The key already
  carries the graph, so per-graph overrides can be added without changing the stored shape.

## Decision

1. **Scope and unit.** Quota is per `(caller principal, graph)` over a fixed window (default
   60 seconds). A request costs `1 + ceil((tokens - 32) / 10)` units for `tokens > 32`, and 1 unit
   otherwise, mirroring the model's 32-token window / 10-token stride. `token_ids.len() > 256`
   (the client cap) is rejected as `InvalidArgument` before quota and before any inference call.
2. **Default and configuration.** The deployment config (`RouterRuntimeConfig`, MemoryId 48) gains
   `inference_quota { enabled, window_secs, units_per_window }` with defaults enabled / 60 s /
   60 units. A controller-only `admin_set_inference_quota` validates bounds
   (`1..=3600` seconds, `1..=100_000` units) and persists the record; no new MemoryId is created.
3. **Exemption.** Router controllers (the deployment governance) bypass the quota: they fund the
   canister and need it for operation and tests. Graph tenancy remains required for everyone.
4. **State.** A heap-only `BTreeMap<(Principal, GraphId), Window { started_at_ns, units_used }>`
   bounded to 4096 entries. Expired windows reset in place; when the map is full, stale windows are
   dropped first and the oldest remaining window is evicted. State is not durable by design.
5. **Failure.** Exhaustion returns `RouterError::Busy { operation: "inference_quota" }` without
   calling the inference canister. A dedicated error variant and a `Retry-After`-style hint are
   deferred; `Busy` is already retryable and does not disclose quota internals.
6. **Accounting.** Units are consumed before the inference call and are not refunded if the call
   fails. This is conservative and avoids a reservation/refund state machine; the first slice is
   protection, not billing.
7. **Observability.** Deferred. Denials are testable through the ingress result; a durable audit
   log is only warranted together with billing.

## Consequences

- Every tenant's model calls are bounded by a deployment-wide budget while controllers keep full
  operational access.
- The graph tenancy check, the per-graph inference flag and the quota are evaluated in one ingress
  in a fixed order: graph exists → tenancy → inference enabled → token cap → quota → proxy.
- Configuration changes do not allocate a new stable region; only the existing record changes.

## Trade-offs

- Heap-only state resets on upgrade and is per-canister-instance: an upgrade grants a burst. This
  is accepted because quotas are abuse protection, not an authority or billing ledger.
- A fixed window allows a burst at a window boundary. A sliding window or token bucket is deferred.
- The unit model approximates canister windows; it does not measure actual instructions or cycles.
- Controller exemption means a leaked controller key is not limited by this mechanism.

## Migration

Pre-production fresh-install semantics: MemoryId 48 changes shape; no decoder or migration is
added. Existing deployments are reinstalled with the new release.

## Design Documentation Impact

- `design/storage/stable-memory-inventory.md`: MemoryId 48 record gains the quota config (no new
  region).
- ADR 0111: the open follow-up "per-caller quotas/rate limits" is resolved by this ADR.
- SDK/CLI: no surface change; `Busy` is surfaced as a Router error.

## Implementation slices

1. Config: `InferenceQuotaConfig` in `RouterRuntimeConfig`, controller-only setter, init/upgrade
   preservation and tests. **Landed 2026-11-12.**
2. Enforcement: heap fixed-window map, unit calculation, token cap, `Busy` denial, unit tests.
   **Landed 2026-11-12** (`inference_quota` unit tests).
3. PocketIC: set a 1-unit quota, call `route_and_extract` twice as a non-controller graph admin,
   assert the second call is `Busy` and the controller remains exempt. **Landed 2026-11-12** in
   `inference_router_proxy`.
4. Deferred: sliding window, per-graph overrides, durable audited billing.

## Review axes

- **Encapsulation**: the quota is Router ingress policy; Graph and the inference canister stay
  unaware of it.
- **Separation of concerns**: authorization (tenancy), feature enablement (per-graph flag) and
  spend policy (quota) remain separate decisions in one ingress.
- **Invariants**: quota is checked before any inference call; controllers are the only bypass; the
  map is bounded.
- **Consistency**: the configuration has one durable owner; the counter has one writer per key.
- **Fitness for purpose**: bounded protection without a new stable region or a billing subsystem.

## Amendment (2026-11-12, ADR 0113)

Quota unit accounting is unchanged. The text ingress obtains the exact token count from the
inference canister's `tokenizer_token_count` before charging, so `1 + ceil((tokens - 32) / 10)`
still prices the model's 32-token windows exactly; the Router's pre-call 256-token check now uses
that count instead of a client-supplied token id length.
