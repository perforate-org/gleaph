# Single-insertion cost decomposition

Date: 2026-07-21 12:30 UTC
Author: Gleaph agent
Status: draft / paused pending Plan 0101 Graph log proxy
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

## Additive parent intervals (Router ingress)

| # | Interval | Where measured | Definition |
| --- | --- | --- | --- |
| 1 | Router ingress total | Router `gql_execute_idempotent_batch` | Wall-to-wall instruction count inside the Router for one batch ingress call. |
| 2 | Router prepare / preflight | Router, inside ingress | Plan decode, candidate-domain validation, preflight index lookups, seed row resolution, bulk group construction. |
| 3 | Router → Graph call overhead | Router dispatch | Inter-canister call serialization, Candid encoding, and invocation overhead **inside the Router**. |
| 4 | Router result encode / bookkeeping | Router | Encoding the result, cursor, status, and response bookkeeping. |

Intervals 2–4 are nested inside interval 1 and must not be added to it.

## Graph-side diagnostics (pending Plan 0101 proxy)

These require a Router composite query that forwards to the Graph shard because
the Graph `admin_take_batch_instr_log` endpoint is restricted to the Router
principal. Planned intervals:

| # | Interval | Where measured | Definition |
| --- | --- | --- | --- |
| 5 | Graph decode | Graph entry | Decoding the bulk request, plan blob, parameters, and catalog state. |
| 6 | `run_wire_plans` | Graph | Planning and executing the complete-row seed insertion. |
| 7 | canonical mutation | Graph, inside `run_wire_plans` | Low-level vertex/edge/property writes and LARA placement. |
| 8 | outbox persistence | Graph | Persisting derived-index outbox entries to stable memory. |
| 9 | mutation journal + GC | Graph | Appending the group journal entry and any expiry/GC work. |
| 10 | final synchronous drain | Graph | Flushing the outbox to index canisters before returning. |

## Current Router measurement (warm-cache, default page size)

- Deployment: `SOCIAL_DEMO_USER_SCALE=5 SOCIAL_DEMO_POST_SCALE=20`,
  `batch-instr-log` enabled on Router and Graph shard.
- POSTED-only edge batches (User->POSTED->Post), excluding first bootstrap and
  partial last batches: **14 batches, 6,515 items**.
- **Router ingress parent: 2,837,264 instructions per POST**.
- Router `dispatch_total` per POST: 2,767,204 instructions (child of ingress).
- Router `graph_calls` per POST: 865,645 instructions (child of ingress, not
  Graph execution).

The previously reported 5,555,030 instructions/POST and ~21.2% improvement are
**withdrawn** because they mixed parent and child intervals. The old baseline
7,048,339 instructions/POST was a Graph scalar end-to-end measurement; it cannot
be compared with the Router ingress parent until the same layer is measured.

## Blocked work

- Graph-side interval breakdown cannot be collected without a Router proxy to
  Graph's restricted `admin_take_batch_instr_log` endpoint.
- Batch-size series (32, 128) is deferred until Graph diagnostics are available.
- Scalar single-item measurement is deferred until the same proxy exists.

## Next step

Implement Plan 0101: Router composite query proxy to Graph instruction log, then
restart the network and collect the default-size Graph breakdown. If that
breakdown alone is conclusive, skip the 32/128 series. Otherwise, measure 128
(and 32 only if necessary).
