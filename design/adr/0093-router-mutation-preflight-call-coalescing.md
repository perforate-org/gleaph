# ADR 0093: Router mutation preflight call coalescing

Status: Implemented

## Context

ADR 0041 batches the Router→Graph canonical-write path so a fixed page of
mutations pays the inter-canister overhead once per target Graph canister.
ADR 0042 adds a cursor/budget API so large pages can continue across ingress
calls. Both ADRs cover the dispatch boundary *after* planning.

Before dispatch, each mutation in a `gql_execute_idempotent_batch` wave still
issues its own Router→Graph/Index inter-canister calls:

- seed-anchor lookups via `lookup_label_page` / `lookup_anchor_hits`;
- idempotency replay via `get_mutation_journal_entry`;
- read-consistency barrier via `index_pending_min_mutation_id`.

With many mutations planning concurrently, `ic-cdk 0.20` call reservations
exhaust the Router's *liquid* cycles balance even though its total cycles
balance is large. The observed failure is:

```
graph get_mutation_journal_entry call failed: insufficient liquid cycles balance,
available: 9046205698, required: 42102453000
```

## Decision

Add a per-wave `PreflightContext` inside `gql_execute_idempotent_batch` that
coalesces duplicate inter-canister lookups and throttles planning concurrency.

1. **No planning semaphore.** The original `PREFLIGHT_CONCURRENCY`
   semaphore was removed once preflight coalescing made it unnecessary. With
   inter-canister calls batched and cached per wave, the number of concurrent
   call reservations no longer scales with the number of mutations, so no
   artificial planning chunking is required.

2. **Anchor lookup cache.** Collect unique `(IndexAnchor, ShardId)` pairs across
   the wave and issue one lookup per unique pair. Cached results are reused for
   all mutations referencing the same anchor.

3. **Journal batch endpoint.** Add a Graph canister query
   `get_mutation_journal_entries(Vec<MutationId>) -> GetMutationJournalEntriesResult`.
   The Router groups uncached `(graph_canister, mutation_id)` pairs by canister,
   chunks each canister's ids by encoded Candid payload size, and issues one call
   per chunk. The Graph canister stops early when its instruction counter nears
   the dynamic budget and returns `next: Option<MutationId>`; the Router pages
   forward transparently from that cursor.

4. **Read-consistency sharing.** Cache `index_pending_min_mutation_id` per Graph
   canister within the wave; the first mutation that needs the barrier pays for
   the lookup, later mutations reuse it.

5. **Wave-level journal prefetch.** After the ADR 0041 dispatch coordinator returns
   results, collect all unique target Graph canisters for the current mutation and
   issue one batched journal read before per-shard recovery/projection. Missing
   entries are cached as `None` and handled by the existing per-shard logic.

6. **Equality/edge-equality anchor batching.** The anchor cache in point 2
   originally coalesced only label and label-intersection lookups per shard.
   Equality (`IndexAnchor::Equal`) and edge-equality (`IndexAnchor::EdgeEqual`)
   anchors are now collected across the entire wave, grouped by index canister,
   and resolved through two new index canister query endpoints:
   `lookup_equal_batch` and `lookup_edge_equal_batch`. The Router chunks the
   batched request by encoded Candid payload size, and the index canister pages
   per bucket and stops early when it nears the query instruction budget (5B
   instructions), returning a `next` spec index so the Router can resume in a
   follow-up call.

7. **Instruction-budget-aware paging.** Query-side paging now uses the known IC
   per-message limits: query calls stop at 5B instructions, update calls at 40B
   instructions. The Graph canister `get_mutation_journal_entries` query and the
   new index `lookup_equal_batch` / `lookup_edge_equal_batch` queries use the 5B
   query budget; the Graph `execute_plan_update_batch` update path uses the 40B
   update budget.

8. **Remove `max_items` from the batch API.** `GqlExecuteIdempotentBatchArgs`
   no longer carries a `max_items` field. Pagination is driven exclusively by
   `instruction_budget`: when the Router's per-message instruction counter nears
   the budget it returns `next_index`, and the caller continues from there. This
   removes the artificial item-count cap that was added as a workaround for
   liquid-cycles exhaustion and lets the budget be the single paging signal.

The `PreflightContext` is intentionally wave-local: it is created at the start of
one `gql_execute_idempotent_batch` ingress call, shared by reference across the
planning futures, and dropped when the wave finishes. It is not persisted across
waves or upgrades.

## Consequences

Positive:

- A wave of `N` mutations with the same seed anchor issues one
  `lookup_label_page` per `(anchor, shard)`, not `N`.
- A wave of `N` mutations that are already completed issues at most one
  `get_mutation_journal_entries` call per target Graph canister, not `N`
  `get_mutation_journal_entry` calls. If the request or response would exceed the
  safe inter-canister payload limit, the Router chunks by encoded size; if the
  Graph canister nears its instruction budget, it returns `next` and the Router
  pages transparently.
- A wave touching `M` shards issues at most `M` `index_pending_min_mutation_id`
  calls.
- A wave of `N` equality- or edge-equality-anchored mutations issues at most one
  `lookup_equal_batch` / `lookup_edge_equal_batch` call per index canister (plus
  size/instruction-budget follow-ups), instead of `N` individual equality calls.


Costs and limitations:

- A new Candid query endpoint is added to the Graph canister; the single-mutation
  `get_mutation_journal_entry` endpoint is retained for other callers.
- The cache is scoped to one wave. Cross-wave duplicate calls are not eliminated.
- Preflight coalescing is wired only through the `gql_execute_idempotent_batch`
  path; single-mutation entry points create a fresh empty context and do not
  coalesce.

## Validation

- `cargo fmt --all -- --check`
- `cargo clippy -p gleaph-router --all-targets --all-features -- -D warnings`
- `cargo clippy -p gleaph-graph --all-targets --all-features -- -D warnings`
- `cargo test -p gleaph-router --lib`
- `cargo test -p gleaph-graph --lib`
- `cargo test -p gleaph-pocket-ic-tests --test router_gql_query`
- `icp network stop; icp network start local -d; GLEAPH_DEMO_FORCE_VITE_IC_HOST=1 ./scripts/deploy-social-demo-local.sh`

## Follow-up

The batch journal read is now both message-size chunked and instruction-paged,
so the remaining limit is the Router's *liquid* cycles balance, not the number of
mutations in the wave. The per-call reservation is fixed by the IC cycle model
(~42.1B cycles per outbound call plus ~1K per byte of payload). The original
`PREFLIGHT_CONCURRENCY` semaphore was removed because coalescing already bounds the
number of outbound calls; re-introducing planning chunking is unnecessary and,
with `ic-cdk 0.20`, previously caused `protected task outlived its canister method`
panics.
