# 0094. Synchronous graph-to-index posting flush for wire DML

Date: 2026-07-16
Status: implemented
Last revised: 2026-07-16
Anchor timestamp: 2026-07-16 05:07:07 UTC +0000

## Context

The graph shard queues federated property/edge/label index postings in volatile
thread-local cells while it applies canonical writes. Today the wire DML path
(`execute_plan_update` / `execute_plan_update_batch`) moves any remaining postings
to the durable **derived-index outbox** after the plan bundle finishes
(`crates/graph/src/gql_run.rs::persist_pending_to_outbox`). The maintenance timer
drains that outbox asynchronously.

This creates two problems:

1. **Dependent seeds see a stale graph-index.** A seed that matches a vertex created
   by an earlier seed (e.g. `MATCH (u:User {id:$id}) NEXT INSERT (u)-[:POSTED]->(p:Post)`)
   is planned by the Router using graph-index anchor lookups. If the earlier seed's
   postings are still in the graph outbox, the anchor lookup returns no hits and the
   dependent insert silently creates the wrong state.
2. **Too many graph→index inter-canister calls when flushing per operation.** Flushing
   each operation individually would reserve the IC per-call cycle budget many times;
   the existing async outbox already batches across mutations, so the happy path should
   do the same, but synchronously.

The parsed GQL path does flush before read statements, but a DML-only program ends with
`persist_pending_to_outbox` and is therefore also asynchronous.

## Decision

Make the wire DML update path flush pending postings **synchronously inside the same
graph canister update call**, batching postings from the whole call into as few
graph→index `posting_batch` calls as possible.

### 1. Batch-level flush

- `execute_plan_update` (single mutation): flush at the end of the call.
- `execute_plan_update_batch`: collect pending postings produced by each operation and
  flush once after the loop, so one target graph-index canister receives one combined
  `posting_batch` call (plus pages, see §2).
- Parsed GQL DML-only programs also flush synchronously at the end of the transaction
  block when an index client is available, instead of moving the work to the outbox.

### 2. Message-size + instruction paging

The immediate flush path currently relies on the index canister to return
`IndexPostingBatchProgress::next_index` when it nears its own instruction budget. It does
not yet split requests that would exceed the IC 2 MiB message-size limit.

Add binary-search chunking on the caller side, identical to the pattern already used for
Router→Graph journal batch reads (ADR 0093) and the repair-journal drain
(`crates/graph/src/index/repair_journal.rs::property_batch_prefix`):

- Find the largest prefix of the remaining `Vec<IndexPostingMutation>` whose encoded
  `(ShardId, operations)` payload fits inside
  `gleaph_graph_kernel::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES`.
- Send that prefix in one `posting_batch` call.
- If the index canister applies only part of the prefix due to its instruction budget,
  resume from `progress.next_index` without re-chunking; otherwise advance to the next
  message-size chunk.
- Repeat until every pending posting is delivered or a hard failure occurs.

The chunking function is shared across `pending`, `edge_pending`, `label_pending`, and
`flush_all_pending` to avoid duplicating the size-limit logic.

### 3. Deferred-flush completion (ADR 0024)

When a synchronous flush returns `PlanQueryError::IndexFlushDeferred` and no DML plan
remains unexecuted in the call, the graph shard still records the mutation journal entry
as `Completed`. The repair journal and maintenance timer own eventual index convergence.
This matches the existing ADR 0024 decision; the implementation gap is closed for the
wire DML path.

If a flush fails mid-bundle and there *is* remaining DML, the existing behavior is
preserved: the error propagates and the journal stays `Incomplete`, because continuing
would risk running later writes against a stale index.

### 4. Vector index

`vector_pending` follows the same synchronous-batch policy when an indexed-embedding
catalog is supplied. Its existing batch path also gains message-size chunking so a large
embedding seed does not exceed the 2 MiB payload limit.

### 5. Fallback path

The derived-index outbox and the durable repair journal are retained as fallbacks:

- On a hard error before the synchronous flush, pending postings are still moved to the
  outbox so they are not lost.
- On a flush failure, the remaining postings are appended to the repair journal with their
  originating `mutation_id` so per-mutation index watermarks advance correctly.
- Non-wire callers and maintenance-timer drains continue to use the outbox/journal path.

## Consequences

Positive:

- Dependent seeds and anchor lookups observe index updates within the same or next
  Router call, without waiting for the maintenance timer.
- A batch of DML operations pays the graph→index inter-canister overhead once per target
  index canister (plus size/instruction pages), not once per operation.
- The repair journal remains the single owner of eventual index convergence, and the
  mutation journal is no longer wedged `Incomplete` by a transient flush failure.

Trade-offs:

- Each graph update call that writes indexed data now issues at least one synchronous
  inter-canister call, increasing per-call latency and instruction/cycle consumption.
- Very large batches may still need multiple pages; the paging boundary is explicit and
  observable.

## Alternatives considered

1. **Keep the async outbox and force Router planning to wait with `AtLeast(token)`.**
   Rejected: the social-demo dependent seeds hit the problem during Router planning
   (anchor lookups), not during a client read barrier, so `AtLeast(token)` would not
   help without also making planning blocking and token-aware.

2. **Flush per operation in a batch.** Rejected: it does not reduce the number of
   graph→index calls and therefore does not address the cycle-reservation cost driver.

3. **Flush once at batch end without message-size chunking.** Rejected: a large batch
   would exceed the IC 2 MiB request limit and trap; chunking is required for correctness
   at scale.

## Design documentation impact

- Update `design/adr/0024-mutation-journal-completion-vs-index-flush.md` to note that
  the wire path now implements the deferred-flush completion rule.
- Update `design/adr/0023-federated-index-consistency-upgrade-compaction.md` to document
  that the happy-path synchronous flush also uses the shared message-size chunking
  primitive.
- Update `design/adr/0093-router-mutation-preflight-call-coalescing.md` follow-up to
  note that graph-side synchronous flush removes the dependent-seed staleness class that
  the sequential seeding workaround addressed.

## Migration

No stable-memory layout or wire-type change. The new synchronous flush reuses existing
pending queues, `IndexPostingMutation`, `posting_batch`, and the repair journal. Existing
outbox entries drain normally.
