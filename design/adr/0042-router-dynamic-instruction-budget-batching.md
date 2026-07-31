# ADR 0042: Router instruction-budget mutation batching API

Status: Implemented

## Decision

Make `gql_execute_idempotent_batch` a cursor-based continuation API. This is a
breaking replacement for the former separate dynamic endpoint. The caller
supplies a mutation list, `start_index`, an optional instruction budget, and an
optional maximum item count. There is no caller-visible item-count cap when
`max_items` is omitted, and the Router consumes all remaining input that fits
its instruction and encoded-payload budgets. The Router executes the selected
mutations in dynamically sized waves, reusing ADR 0041's Graph-canister batch
boundary. Graph transport chunks each target's operations by encoded request
size. Between waves it reads the current Router call-context instruction
counter and stops before starting another wave when the requested budget or
encoded-payload boundary requires a continuation cursor.

The default and maximum budget is 35B instructions, leaving 5B headroom below
the 40B update-call limit for the final wave and response construction. A
caller-supplied budget cannot exceed that safe maximum. `next_index` is returned
when more mutations remain; retrying from the same cursor is safe because each
mutation keeps its existing client mutation key and Graph mutation id.

The encoded-message sizing policy is owned by `gleaph-message-sizing`:
`MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES` is the current portable platform
ceiling, and `INTER_CANISTER_TARGET_PAYLOAD_BYTES` is derived by subtracting the
fixed `INTER_CANISTER_MESSAGE_HEADROOM_BYTES` (500 KiB). Router and Graph measure
the complete Candid payload and choose a target-sized prefix; the hard ceiling remains
authoritative if a shape estimate overshoots. A future platform-limit increase therefore
changes one sizing constant without changing every caller's formula. The 40B
update ceiling, 5B query ceiling, and 35B dynamic update budget remain owned by
`gleaph-graph-kernel`.

The social-demo seeder uses the generated WASM adapter from the same
`gleaph-message-sizing` crate. It measures the complete binary Candid batch
envelope with `@icp-sdk/core/candid`; it does not estimate from source-text
length or maintain a second sizing algorithm in JavaScript.

Within a dynamic Graph call, Graph executes until its own 35B safety budget and
returns the first unattempted operation. If the Router call context is still
below its budget, Router sends the remaining operations to that Graph again.
This repeats until the Graph operations complete or the Router budget requires a
continuation cursor.

The Graph journal preflight read uses the per-message instruction counter
(`ic_cdk::api::instruction_counter()`) rather than the call-context counter so
that the Router's earlier work in the same ingress call is not counted against
the read's local budget.

This API does not attempt to interrupt a mutation or a Graph batch wave. The
atomicity unit remains one mutation, and the wave remains a transport grouping
only. The instruction counter is the current Router canister call context; it
is not a cross-canister aggregate. Graph execution retains its own per-call
limit and item-local journal boundary.

## Consequences

- Large seed workloads can continue across ingress calls without guessing a
  fixed page size; an omitted `max_items` consumes all remaining input within
  budget.
- A conservative default leaves headroom for Router response construction and
  continuation bookkeeping.
- The caller must retain the original mutation list and feed back `next_index`.
- A budget exhausted before the first wave is a validation error, not an empty
  successful page, preventing a cursor that cannot advance.
- Payload and instruction-budget probes prevent call-context resource exhaustion
  on very large pages while still allowing continuation from the returned cursor.

## Follow-up

Future work may add measured per-wave cost hints, but must not replace the
instruction counter with a cross-canister sum or create a second idempotency
mechanism.

## Related

The Router batch entrypoint uses a planning-phase concurrency semaphore and
per-wave lookup coalescing. The ADR 0042 cursor/budget paging remains responsible
for limiting the total ingress batch size; the semaphore and caches limit parallel
planning and duplicate inter-canister calls inside one wave.
