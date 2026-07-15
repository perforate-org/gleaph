# ADR 0041: Router-to-Graph batch mutation dispatch

Status: Implemented

## Context

The Router currently accepts a fixed page of idempotent GQL mutations, but
executes each mutation with a separate `execute_plan_update` inter-canister
call to the target Graph canister. This preserves per-mutation idempotency but
repeats the inter-canister call overhead, update-message base cost, Candid
boundary, and callback handling for every seed.

The ICP cycle-cost model charges an inter-canister request/response overhead and
an update-message base cost in addition to the instructions and payload bytes.
The cost applies to each call even when Router and Graph are on the same subnet.
The existing fixed-page API therefore reduces ingress calls but does not reduce
the dominant Router-to-Graph call count.

## Problem

Reduce Router-to-Graph call overhead for fixed-page bulk mutation execution
without weakening the existing per-mutation idempotency, shard-local atomicity,
Router saga, or recovery contracts.

This decision does not attempt to make a whole page atomic. A page may contain
multiple independent mutations, and a later mutation may fail after earlier
mutations have committed.

## Decision

Add a Graph-canister `execute_plan_update_batch` endpoint and make Router group
the mutations in one fixed page by target Graph canister. Router sends one batch
call per target Graph canister, not necessarily one call for the whole page when
the page spans multiple shards.

The batch wire item retains all data currently carried by `ExecutePlanArgs`,
including its independent `mutation_id`, plan and parameter blobs, shard
bindings, catalog projections, and uniqueness/vector dispatch metadata.

Graph executes items sequentially through the existing single-mutation execution
core. Each item remains its own canonical write and idempotency boundary:

- a `mutation_id` is never shared between items;
- an item success is durable before the next item begins;
- replay of an already completed item returns its existing journal outcome;
- an item failure is returned as an item result and must not trap the whole batch;
- earlier successes remain committed when a later item fails;
- the batch result reports item order and per-item success/failure.

Router remains the owner of cross-shard orchestration, mutation-key mapping,
reservation/confirmation, and recovery. Graph remains the owner of Graph-local
canonical writes and Graph-local mutation idempotency. The batch endpoint is a
transport and execution aggregation boundary, not a second journal owner.

The public batch endpoint has no arbitrary item-count limit. An optional
`max_items` value lets a caller impose a cursor page limit; when omitted, the
Router consumes as many remaining items as the instruction and encoded-payload
budgets allow. Router chunks each target Graph canister's operations by encoded
request size, not by a fixed item count. Each Graph batch also has bounded
encoded request and response sizes and rejects an over-sized request before the
first item in that transport batch is executed. The shared conservative request
ceiling is 2 MiB, matching ICP's ingress and cross-subnet request limit; this
keeps the transport valid if canisters move between subnets even though same-
subnet requests permit a larger payload.

## Implementation status

Implemented as the transport layer of the cursor-based
`gql_execute_idempotent_batch` endpoint. The endpoint advances all items
concurrently through Router planning and mutation-journal preparation.
Prepared Graph operations rendezvous at the dispatch boundary and are grouped
by target Graph canister, so a request issues as few size-bounded batch calls per
target canister as the Router instruction budget permits. Early-completed
items register an empty operation set, while
pre-dispatch errors abort the coordinator so remaining items do not wait
indefinitely.

## Alternatives

### Keep one Graph call per mutation

This requires no API change and preserves the current execution path, but keeps
the repeated inter-canister and update-message fixed costs. It is rejected for
fixed-page seed workloads.

### Combine all page mutations into one GQL statement block

This reduces calls but collapses independent idempotency and failure boundaries
into one GQL mutation. It would require a new durable submutation journal and a
new recovery contract. It is rejected for this slice.

### Add Graph batch dispatch first, then dynamic paging

This preserves the existing fixed-page behavior while reducing the dominant
call count. Dynamic paging can later choose how many items to place in the same
Graph batch using the Router call-context instruction counter. This is selected.

## Invariants and failure behavior

1. A batch item is identified by its existing immutable mutation id and client
   mutation key mapping; retries must not create a second canonical mutation.
2. Graph must complete all fallible validation for an item before that item's
   canonical write boundary, as in the single-item path.
3. A returned item failure is not a batch-wide rollback signal.
4. A trap or transport rejection before a batch result is returned is retried
   with the same item mutation ids; no fresh ids may be minted by the caller.
5. Router recovery continues to inspect each mutation journal entry separately.
6. Multi-shard requests issue size-bounded chunks per Graph canister; a target
   with more than one chunk receives one call per chunk until the Router budget
   requires a continuation.

## Consequences

Positive:

- Seed execution can pay Graph inter-canister overhead once per size-bounded
  target chunk rather than once per mutation.
- Callers may retain a client-side page size when desired, while the default
  consumes all remaining input through the continuation cursor.
- The dynamic cursor API owns the simple public batch name; the former separate
  dynamic endpoint is intentionally removed as a breaking change.

Costs and limitations:

- Graph must expose and maintain a new Candid endpoint.
- Batch response and request payloads require explicit bounded-size checks.
- The page remains partially successful rather than all-or-nothing.
- Multi-shard pages still require one call per target Graph canister.
- GQL parsing/planning in Router remains per mutation in this slice.

## Validation requirements

- Unit-test the Graph batch decision core for ordered success, later-item
  failure, replay, and item-local atomic rejection.
- Test Router grouping by Graph canister and one-call-per-target behavior.
- Test encoded-payload rejection before any item executes.
- Add a PocketIC path with at least two mutations and verify canonical state,
  journal state, and retry behavior after a partial result.
- Preserve the existing fixed-page end-to-end seed path.

## Follow-up

The Router uses `ic_cdk::api::call_context_instruction_counter()` to select how
many size-bounded Graph batches fit in the current call. It must not change the
per-item idempotency or partial-success contract established here.
