# Single-insertion cost decomposition

Date: 2026-07-21 12:30 UTC  
Updated: 2026-07-22 04:15 UTC
Author: Gleaph agent
Status: draft / Plan 0101 proxy implemented; default-size POSTED breakdown collected
Plan: 0100-single-insertion-cost-decomposition

## Goal

Decompose the Router and Graph instruction costs of one social-demo
`User -[:POSTED]-> Post` edge insertion, respecting the ICP performance-counter
layer boundary. Fit

```
cost(batch_size) = fixed_per_batch + batch_size × marginal_per_item
```

once Graph-side diagnostics are available.

## Measurement-layer contract

- `call_context_instruction_counter` counts only instructions executed **inside
  the current canister**. It does **not** include cross-canister call execution.
- Router ingress cost ≠ Graph execution cost.
- `dispatch_total` and `graph_calls` reported by the Router are child intervals
  of the Router ingress interval, not Graph measurements.
- Router, Graph, and graph-index counters must be reported separately. They can
  only be compared when they measure the same canister and same call boundary.

## Method

- Deployment: `SOCIAL_DEMO_USER_SCALE=5 SOCIAL_DEMO_POST_SCALE=20`,
  `batch-instr-log` enabled on Router and Graph shard.
- Network restarted; Router installed with deployer principal as initial admin
  so the new `admin_graph_batch_instr_log` composite query can be exercised.
- Logs collected via:
  - `admin_take_batch_instr_log` on Router
  - `admin_graph_batch_instr_log("gleaph.pocket_ic", 0, 10000)` on Router,
    which proxies to `gleaph-graph-shard-0`.

## Router ingress parent (POSTED-only, mid batches)

- 14 batches, 6,515 items, excluding first bootstrap and partial last batches.
- **Router ingress: 2,837,264 instructions per POST**.
- Nested child intervals:
  - prepare total: ~6.3M per call
  - dispatch total: ~1.1G per call
  - graph_calls: ~292M per call (Router-side call cost, **not** Graph execution)

## Router PREPARE sub-phase (per call, all keys)

| Sub-phase | Mean per call |
| --- | ---: |
| envelope | 8,346,583 |
| replay | 6,511,435 |
| reserve | 6,346,160 |
| labels | 2,541,742 |
| claims | 1,649,491 |
| routing | 884,648 |
| props | 120,168 |
| classify | 855 |

Envelope and replay dominate per-call Router preparation cost.

## Graph-side parent (POSTED bulk batch estimates)

Because the Graph log does not include a batch identifier, POSTED batches were
identified as the contiguous block of large `execute_plan_impl` calls following
the bootstrap/wave-1/wave-2 calls. Each Router POSTED batch maps to one Graph
bulk call (the dynamic chunk sizing in Plan 0099 keeps each Router batch to
one Graph call). With ~450 POSTED items per Graph bulk call:

| Graph phase | Mean per POSTED item |
| --- | ---: |
| run_wire_plans | ~6,900 |
| decode | ~1,250 |
| drain | ~5 |
| result_build | ~5 |
| **Graph total per POSTED item** | **~8,200** |

This is an order-of-magnitude estimate: the mapping between Router POSTED
batches and Graph log calls is inferred from ordering and cost size, not a
stable key.

## Key finding

**Graph canonical execution is not the dominant cost.** The Router ingress
per-item cost (~2.84M) is roughly **340× larger** than the estimated Graph
per-item cost (~8.2K). The bulk path is already efficient at the Graph layer.

The remaining cost is Router-side fixed per-call work, especially:

1. **dispatch total** (~1.1G per call) — inter-canister call serialization,
   Candid encoding/decoding, and Router-side dispatch bookkeeping.
2. **prepare envelope** (~8.3M per call) — constructing the bulk request
   envelope, including repeated plan/catalog payload materialization.
3. **prepare replay** (~6.5M per call) — replay/idempotency bookkeeping.

## Implication for next slice

ADR 0045 (LARA placement) and Graph-side journal/outbox optimizations are
unlikely to explain a meaningful share of the end-to-end singleton cost because
the Graph layer itself is already cheap. The next implementation slice should
attack Router-side per-call overhead instead.

Candidate directions:

1. **Increase effective batch size up to the message-size limit.**
   - Current dynamic page sizing targets ~500 KiB Candid text.
   - 2 MiB inter-canister limit leaves headroom; larger batches would amortize
     the 1.1G dispatch and 8.3M envelope fixed costs over more items.
   - Requires measuring actual message bytes and instruction budget headroom.

2. **Shared typed Graph bulk envelope.**
   - Send plan blob, catalog, and resolved labels once per batch instead of
     per item or duplicated per call.
   - Targets the 8.3M envelope and 1.1G dispatch overhead directly.

3. **Optimize Router idempotency/replay bookkeeping.**
   - The 6.5M replay cost suggests redundant journal lookups or cache misses.

Decision threshold: a candidate must explain ≥10% of the end-to-end singleton
cost (≥~280K instructions/POST) or ≥500K instructions/POST. All three
Router-side candidates above potentially meet this threshold.

## Remaining uncertainty

- The Graph→Router POSTED batch mapping is inferred, not keyed. Adding a batch
  identifier to Graph instrumentation would remove this uncertainty.
- Batch-size series (128, 32) was deferred; the default-size breakdown alone
  already points clearly to Router-side overhead.
- A true scalar (1-item) measurement is still missing, but the default-size
  per-item Router cost is sufficient to rule out Graph-side LARA work as the
  primary target.

## Acceptance

- [x] Additive interval contract documented before measurement.
- [x] Warm-cache default-size POSTED batches measured at Router and Graph layers.
- [x] Router PREPARE/DISPATCH sub-phases reported.
- [x] Estimated Graph per-item cost and dominant Router overhead identified.
- [x] Next-slice direction named with threshold reasoning.

## Later Slices

- Resume Plan 0100 only if batch-size series is needed to choose between the
  Router-side candidates above.
- Implement Plan 0102: message-size-aware batch sizing or shared typed Graph
  bulk envelope, depending on which candidate the next brief prioritizes.
