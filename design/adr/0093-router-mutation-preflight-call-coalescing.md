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

1. **Planning semaphore.** Process at most `PREFLIGHT_CONCURRENCY` (default 16)
   mutations in parallel during the planning phase. The final batch still receives
   all operations; only planning is throttled.

2. **Anchor lookup cache.** Collect unique `(IndexAnchor, ShardId)` pairs across
   the wave and issue one lookup per unique pair. Cached results are reused for
   all mutations referencing the same anchor.

3. **Journal batch endpoint.** Add a Graph canister query
   `get_mutation_journal_entries(Vec<MutationId>) -> Vec<Option<...>>`. The Router
   groups uncached `(graph_canister, mutation_id)` pairs by canister and issues
   one call per canister per wave.

4. **Read-consistency sharing.** Cache `index_pending_min_mutation_id` per Graph
   canister within the wave; the first mutation that needs the barrier pays for
   the lookup, later mutations reuse it.

5. **Wave-level journal prefetch.** After the ADR 0041 dispatch coordinator returns
   results, collect all unique target Graph canisters for the current mutation and
   issue one batched journal read before per-shard recovery/projection. Missing
   entries are cached as `None` and handled by the existing per-shard logic.

The `PreflightContext` is intentionally wave-local: it is created at the start of
one `gql_execute_idempotent_batch` ingress call, shared by reference across the
planning futures, and dropped when the wave finishes. It is not persisted across
waves or upgrades.

## Consequences

Positive:

- A wave of `N` mutations with the same seed anchor issues one
  `lookup_label_page` per `(anchor, shard)`, not `N`.
- A wave of `N` mutations that are already completed issues one
  `get_mutation_journal_entries` per target Graph canister, not `N`
  `get_mutation_journal_entry` calls.
- A wave touching `M` shards issues at most `M` `index_pending_min_mutation_id`
  calls.
- The planning semaphore caps simultaneous inter-canister call reservations,
  preventing liquid-cycles exhaustion for the default social-demo seed workload.

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

Raising `GLEAPH_DEMO_SEED_MAX_ITEMS` above the default still hits the local
replica's per-call liquid-cycles reservation for the batched journal read (and,
at very large wave sizes, the ICP update-call instruction budget). Further work
should either top up the Router canister's liquid balance, paginate the journal
batch read, or both.
