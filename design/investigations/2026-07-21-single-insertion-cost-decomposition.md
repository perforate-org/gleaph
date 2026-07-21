# Single-insertion cost decomposition

Date: 2026-07-21 12:30 UTC
Author: Gleaph agent
Status: draft / measurement plan
Plan: 0100-single-insertion-cost-decomposition

## Goal

Decompose the end-to-end cost of one social-demo `User -[:POSTED]-> Post` edge
insertion into an additive cost model

```
cost(batch_size) = fixed_per_batch + batch_size × marginal_per_item
```

and use the largest residual singleton cost to pick the next implementation
slice.

## Measurement constraints

- Warm cache only: run the full seed at least once, then discard the first run.
- Exclude the first bootstrap batch and any partial last batch.
- Measure scalar (1 item) and bulk sizes 2, 8, 32, and 128.
- Repeat each size enough times to obtain a stable mean; report variance if the
  sample allows.
- Do not change canister code for this slice; only `batch-instr-log` is enabled.

## Additive parent intervals

These numbers can be added to reconstruct the end-to-end Router ingress cost for
a single batch. Child intervals listed later are **nested inside** these parents
and must not be added to them.

| # | Interval | Where measured | Definition |
| --- | --- | --- | --- |
| 1 | Router ingress total | Router `gql_execute_idempotent_batch` | Wall-to-wall instruction count for one batch ingress call. |
| 2 | Router prepare / preflight | Router, between ingress entry and Graph dispatch | Plan decode, candidate-domain validation, preflight index lookups, seed row resolution, bulk group construction. |
| 3 | Router → Graph call | Router dispatch | Cost of the inter-canister call serialization, Candid encoding, and cross-canister invocation overhead. |
| 4 | Graph decode | Graph entry | Decoding the bulk request, plan blob, parameters, and catalog state. |
| 5 | `run_wire_plans` | Graph | Planning and executing the complete-row seed insertion against the graph engine. |
| 6 | canonical mutation | Graph, inside `run_wire_plans` | Low-level vertex/edge/property writes and LARA placement (currently inside the same interval). |
| 7 | outbox persistence | Graph | Persisting derived-index outbox entries to stable memory. |
| 8 | mutation journal + GC | Graph | Appending the group journal entry and any expiry / GC work. |
| 9 | final synchronous drain | Graph | Flushing the outbox to the property/vector index canisters before returning. |
| 10 | result encode / bookkeeping | Graph / Router | Encoding the result, cursor, status, and Router-side response bookkeeping. |

For the linear fit, only **interval 1** (Router ingress total) is used as the
dependent variable. Intervals 2–10 are explanatory diagnostics and must be
reported separately.

## Nested child diagnostics (non-additive)

If an implementation exposes finer markers, report them as children of the
parent interval above. Examples:

- `decode phase` inside interval 4.
- `completed journal plus GC` inside interval 8.
- `outbox persistence` already listed as interval 7; do not add it again to
  interval 9.

## Cost model

For each measured batch size `n`, record `cost_1(n)` = Router ingress total.
Fit

```
cost_1(n) = F + n × M
```

using ordinary least squares over the warm-cache mid-batch samples.

Derived metrics:

- `fixed_per_batch` = `F`
- `marginal_per_item` = `M`
- `singleton_equivalent` = `F + M` (extrapolated cost of one item if it were
  the only item in a batch)
- `fixed_share` = `F / (F + M)` (share of a singleton that is fixed per-call
  work)

## Decision threshold

A candidate direction is worth pursuing only if it explains **≥10% of the
end-to-end singleton cost** or **≥~500k instructions per POST**.

## Candidate mapping

| Dominant residual | Likely interval | Next slice |
| --- | --- | --- |
| Journal append / GC | 8 | Plan 0101: expiry-aware journal GC and frequency tuning |
| Synchronous drain / outbox | 7, 9 | Plan 0101: posting batching and lazy drain policy |
| Decode / duplicated payload | 4 | Plan 0101: shared typed Graph bulk envelope |
| Canonical edge insertion / LARA | 5, 6 | Plan 0101: ADR 0045 LARA placement minimum slice |

## Required commands

Enable instrumentation by temporarily adding `features: [batch-instr-log]` to
the rust canisters in `icp.yaml`. Run a fresh local deployment, then execute the
social-demo seed workload twice. Discard the first run.

```bash
icp network stop
icp network start local -d
SOCIAL_DEMO_USER_SCALE=5 SOCIAL_DEMO_POST_SCALE=20 GLEAPH_DEMO_FORCE_VITE_IC_HOST=1 ./scripts/deploy-social-demo-local.sh 2>&1 | tee /tmp/social-demo-warm-1.log
SOCIAL_DEMO_USER_SCALE=5 SOCIAL_DEMO_POST_SCALE=20 GLEAPH_DEMO_FORCE_VITE_IC_HOST=1 ./scripts/deploy-social-demo-local.sh 2>&1 | tee /tmp/social-demo-warm-2.log
```

After the second run, collect Router and Graph instruction markers from the
replica output or the canister logs. Aggregate POSTED-only batches by size and
compute the fit.

## Acceptance

- [ ] Interval contract documented before measurement.
- [ ] Warm-cache scalar and bulk size series measured.
- [ ] Linear fit and fixed/marginal share reported.
- [ ] Dominant residual identified with threshold evidence.
- [ ] Next slice named and linked from this brief.
