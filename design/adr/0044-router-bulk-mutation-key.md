# ADR 0044: Router bulk mutation key for wave-level saga coalescing

Status: Implemented
Date: 2026-07-19 15:12:46 UTC
Last revised: 2026-07-22
Anchor timestamp: 2026-07-22 02:53:05 UTC +0000

## Context

`gql_execute_idempotent_batch` accepts a list of idempotent GQL mutations and
executes each mutation through the Router mutation lifecycle described in ADR
0029 and ADR 0041/0042. Before the bulk path was implemented, every mutation
carried its own `client_mutation_key`, so the Router stable saga
(`ROUTER_MUTATION_BY_CLIENT_KEY`) and Graph mutation journal (`MUTATION_JOURNAL`)
contained one record per seed. As verified from the repository on 2026-07-21
UTC, eligible homogeneous groups now share one bulk key and `MutationId`; the
remaining parameter-dependent seed limitation is documented below.

Measurement recorded on 2026-07-19 UTC (`batch-instr-log`) for the social-demo seed workload
(`SOCIAL_DEMO_USER_SCALE=5 POST_SCALE=20`) shows that Router-side preparation
of a single seed consumes about 25.7M instructions on average. The dominant
costs are stable saga operations:

- `reserve`: ~6.3M
- `replay`: ~6.5M
- `envelope`: ~8.0M

Because each seed creates its own saga record, the Router per-ingress
instruction budget (≈35B) can process only about 570 seeds before it must return
a continuation cursor. The Graph batch endpoint already executes many seeds in
one inter-canister call, so the remaining Router-side per-seed overhead is the
primary bottleneck.

## Problem

Reduce the Router stable-saga overhead for bulk seed workloads without
weakening per-mutation idempotency, shard-local atomicity, recovery, or the
public Candid API. The change must preserve single-mutation callers and keep
the existing ADR 0029/0030/0041/0042 contracts intact.

## Existing architecture assessment

ADR 0029 defines the Router mutation lifecycle:

- `client_mutation_key` maps caller+graph+key to a `MutationId`.
- `RouterMutationRecord` tracks routing lease, shard envelope, and completion.
- Graph `MUTATION_JOURNAL` records the durable outcome of each `MutationId`.
- Recovery and projection use these records as the single source of truth.

ADR 0041 added the Graph `execute_plan_update_batch` endpoint, which already
executes multiple independent mutations in one inter-canister call. Each item
keeps its own `mutation_id`, plan blob, and journal entry.

ADR 0042 added the dynamic `gql_execute_idempotent_batch` cursor/budget API,
which returns `next_index` when the Router ingress budget is exhausted.

The current architecture therefore already has:

1. A Graph batch endpoint that can execute many operations in one call.
2. A Router cursor API that can continue across ingress calls.
3. A stable mutation journal per `MutationId`.

What it does **not** have is a way for many seed operations to share one
`MutationId` / one saga record while still reporting per-op progress and
per-op results. This is the gap the proposal addresses.

## Decision

Introduce a **bulk mutation key** path inside `gql_execute_idempotent_batch`.
When the caller supplies a list of mutations that share the same query plan and
mode, the Router may group them under a single `client_mutation_key` and a
single `MutationId`. The group is executed as one bulk mutation against each
target Graph canister. The Graph batch endpoint treats the group as a sequence
of operations belonging to the same `MutationId` and persists a single durable
journal entry with an operation cursor.

### Public API impact

The public Candid API of `gql_execute_idempotent_batch` does not change. The
existing `GqlExecuteIdempotentBatchArgs` already contains a `Vec<Mutation>`;
the Router decides internally whether to coalesce consecutive compatible
mutations into a bulk group. The returned `GqlExecuteIdempotentBatchResult`
still reports one `GqlQueryResult` per input mutation and a `next_index`
pointing at the next unprocessed input mutation. The caller need not know that
coalescing happened.

### Stable record versioning

This ADR originally defined V1 stable records sufficient for homogeneous bulk groups where every
operation shares the same seed relation. Because Gleaph has no deployed Router stable state to
preserve, ADR 0047 redefines `RouterMutationRecord::V1` incompatibly with exhaustive `Scalar`,
`LegacyBulk`, `TypedSeedBulk`, and terminal `CompletedBulk` payload variants. The typed payload persists the exact ordered per-operation relation before
the first Graph await and cannot coexist with a legacy seed blob. `CompletedBulk` compacts away the
plan, params, seeds, target, and resolved tables once every shard outcome and projection converges,
while typed completion retains only its bounded ordered operation row counts for exact retry,
satisfying the existing
ADR 0025 mechanism-E contract. No Router V2 or stable migration is introduced. Initial installation
and rollback to older Router Wasm require fresh install/reset. See ADR 0047 for the schema, bounds,
capability activation, and reset procedure.

The following is the proposed replacement shape. Retaining the `V1` tag names the first launch
schema; it does not promise decode compatibility with the superseded, never-deployed V1 bytes.
After launch, future extensions must add variants or provide an explicit migration rather than
redefining V1 again:

```rust
pub enum RouterMutationRecord {
    V1(RouterMutationRecordV1),
}

pub(crate) struct RouterMutationRecordV1 {
    mutation_id: MutationId,
    request_fingerprint: Vec<u8>,
    routing_in_progress: bool,
    routing_lease_ns: Option<u64>,
    terminal_failure: Option<String>,
    completed_row_count: Option<u64>,
    resolved_labels: Option<ResolvedLabelTable>,
    resolved_properties: Option<ResolvedPropertyTable>,
    payload: RouterMutationPayloadV1,
    created_at_ns: u64,
    last_error: Option<String>,
}

pub enum RouterMutationPayloadV1 {
    Scalar {
        shards: Vec<RouterMutationShardV1>,
    },
    LegacyBulk {
        total_ops: u32,
        shards: Vec<RouterMutationShardV1>,
    },
    TypedSeedBulk(Box<TypedSeedBulkReplayV1>),
    CompletedBulk {
        total_ops: u32,
        // Empty for legacy bulk; exactly total_ops entries for typed bulk.
        operation_row_counts: Vec<u64>,
    },
}

pub(crate) struct RouterMutationShardV1 {
    shard_id: ShardId,
    graph_canister: Principal,
    seed_bindings_blob: Option<Vec<u8>>,
    completed: bool,
    projection_advanced: bool,
    row_count: u64,
}

// TypedSeedBulkReplayV1 owns one target shard identity/outcome via a dedicated
// `TypedSeedBulkTargetV1` (not `RouterMutationShardV1`), shared execution fields, and the
// ordered per-operation params plus typed seeds. Top-level `resolved_labels` and
// `resolved_properties` remain the sole durable authority. See ADR 0047.

pub enum GraphMutationJournalEntry {
    V1(GraphMutationJournalEntryV1),
}

pub struct GraphMutationJournalEntryV1 {
    pub mutation_id: MutationId,
    pub state: MutationJournalState,
    pub committed_row_count: u64,
    pub next_index: Option<u32>,
    pub bulk_progress: Option<GraphBulkMutationProgress>,
}

pub enum GraphBulkMutationProgress {
    V1(GraphBulkMutationProgressV1),
}

pub struct GraphBulkMutationProgressV1 {
    pub operation_count: u32,
    pub completed_count: u32,
    pub operation_row_counts: Vec<u64>,
}
```

`next_index` already exists in `GraphMutationJournalEntryV1` and is reused as
the bulk operation cursor: for a bulk mutation it points at the next
unexecuted operation index; for a single mutation it has its existing meaning.
No new cursor field is introduced.

The original proposal assumed an initial V1 deployment with no production state. That remains the
explicit prerequisite for the ADR 0047 incompatible V1 replacement: the operator must verify that
there is no deployed state to preserve, then fresh-install/reset the Router. If that prerequisite is
false at implementation or rollout time, this decision is invalid and a separate migration design
is required before changing the schema.

### Router ingress behavior

Inside one `gql_execute_idempotent_batch` call:

1. The Router scans the input list and groups consecutive mutations that share
   the same query plan, graph, and mode. Each group receives one
   `client_mutation_key` derived from the first mutation's key plus a stable
   group ordinal, and one `MutationId`.
2. For each group the Router reserves one saga record, resolves labels and
   properties once, builds one envelope per target shard, and dispatches the
   group as a single bulk mutation to each target Graph canister.
3. Graph executes the operations sequentially, using its existing batch
   endpoint. If the Graph instruction budget is exhausted, Graph returns
   `next_index = Some(k)`. The Router records the partial progress in the
   shard record and, if the Router budget allows, continues the same group
   inside the same ingress call.
4. When all operations of a group are complete, the Router marks the group's
   saga record completed and reports one `GqlQueryResult` per input mutation.
5. If the Router ingress budget is exhausted between groups, it returns
   `next_index` pointing at the next unprocessed input mutation. The durable
   Graph journal cursor ensures that a retry of the same group resumes from
   the correct operation.

The implemented bulk path admits homogeneous groups whose dispatch envelope can be shared
safely. Parameterized index anchors that resolve to selective equality or index predicates are now
handled through the ADR 0046 complete-row seed path: the Router resolves per-item candidate domains
and attaches the exact item-specific `SeedBindingsWire` relation to each `ExecutePlanArgs`. The
first item's seed is never copied to later parameter sets. Unsupported shapes (label-only anchors,
edge anchors, correlated/optional seeds, and constrained mutations) fall back to the existing
sequential path.

Because one bulk `MutationId` may then contain different seed relations for each operation, the
current one-blob-per-shard `RouterMutationShard` representation is insufficient. The ADR 0046 path
requires a versioned ordered per-operation dispatch envelope. Recovery replays the persisted seed
relation and must not repeat Property Index lookup to reconstruct it.

### Graph behavior

Graph's `execute_plan_update_batch` already accepts a list of operations. For a
bulk mutation, all operations carry the same `mutation_id`. Graph persists one
journal entry keyed by that `mutation_id`. The entry records:

- `committed_row_count`: total successful operations so far.
- `next_index`: next unexecuted operation index.
- `bulk_progress`: total/completed counts plus ordered row counts for the committed prefix.

On retry, Graph reads the journal entry and skips already completed operations while replaying their
persisted row counts in ordinal order. Router accepts a typed terminal outcome only when journal
state is `Completed`, `next_index` is absent, and all three progress cardinalities equal the durable
typed operation count. The journal aggregate row count, not callback-local synthetic results, is the
source of truth for Router completion.
Each operation still executes through the existing single-operation core, so
shard-local atomicity and idempotency per operation are preserved.

### Failure and recovery

- If one operation in a bulk group fails, Graph returns the failure for that
  operation and a `next_index` pointing at the following operation. The
  earlier operations remain committed.
- The Router records the partial result and may continue the remaining
  operations or return a continuation cursor to the caller.
- ADR 0029 recovery semantics remain unchanged: the Router recovery timer
  reconciles non-terminal bulk mutations using the durable Graph journal.

### Uniqueness and constrained properties (ADR 0030)

Bulk mutation under an active uniqueness or constrained-property constraint is
out of scope for this slice. The bulk path rejects such mutations until ADR
0030 is extended to handle per-operation claims and effects inside a bulk
mutation. Social-demo seeding does not activate constraints, so this limitation
is acceptable for the target workload.

This rejection is the **current implementation state**, not the long-term seed-resolution rule.
ADR 0046 permits declared constraints to select a semantics-equivalent access path. In particular,
an active single-shard `ShardLocalGlobal` UNIQUE constraint may resolve an equality-bound variable
through the existing canonical local unique-value table. Constraint-backed bulk claims/effects still
require their own ADR 0030 extension before constrained writes themselves are admitted to the bulk
mutation-key path.

## Alternatives

### Keep one `MutationId` per seed

This requires no stable-layout change but leaves the measured bottleneck
untouched. Rejected because the 25.7M per-seed Router cost prevents large
bulk ingestion from fitting in one ingress call.

### Introduce a separate durable batch-job scheduler

Create a new Router subsystem that tracks batch jobs outside the existing
mutation lifecycle. This would avoid touching `RouterMutationRecord` but adds a
new domain, a new recovery path, and duplicated orchestration logic. Rejected
because the existing mutation lifecycle already provides the needed durable
state and recovery hooks.

### Version only the bulk-specific fields

Keep `RouterMutationRecord` and `GraphMutationJournalEntry` flat and add
versioned bulk sub-records. This reduces the initial code churn but leaves the
parent records vulnerable to future layout changes. Selected for rejection in
favor of versioned record envelopes, because the parent records are exactly the
objects whose semantics will evolve as bulk behavior matures.

## Implementation notes

- The ordered bulk group fingerprint (`bulk_group_fingerprint`) covers the plan blob, execution
  mode, operation count, and every operation's params blob in ordinal order. It is used both when
  reserving a new group and when matching a retry to an existing durable record.
- Typed V1 recovery (`recover_typed_bulk_record`) redispatches the exact durable typed replay
  payload without repeating Property Index lookup or consulting the current capability bit.

## Consequences

- The number of Router stable saga records for a homogeneous seed wave drops
  from N to O(1), removing the dominant per-seed Router overhead.
- The Graph batch endpoint and mutation journal are reused; no second journal
  owner is introduced.
- The public Candid API and per-mutation result ordering are unchanged.
- Single-mutation callers continue to use one `MutationId` per mutation.
- Stable value types are versioned, protecting future canister upgrades.
- Selective single-variable and multi-variable anchored groups use the ADR 0046 complete-row
  seed path; unsupported shapes fall back to the sequential path.

## Trade-offs

- Bulk groups must be detected from the input list. The detection logic must
  match the existing planner output exactly; any mismatch creates incorrect
  routing or missed coalescing.
- The Graph journal entry now represents multiple operations. Recovery and
  projection semantics must correctly interpret `committed_row_count` and
  `next_index` for both single and bulk mutations.
- ADR 0030 uniqueness/constrained-property support is deferred.

## Migration

The incompatible V1 replacement depends on an empty production-state inventory. Reverify that
precondition immediately before installation and fresh-install/reset the Router. The previous V1
bytes are intentionally not decodable. If any state must be retained, stop this rollout and create a
separate migration and data-preserving rollback decision.

## Design document impact

- ADR 0029: add a note that `MutationId` may represent a bulk group and that
  `next_index`/`committed_row_count` semantics apply per group.
- ADR 0041/0042: update to describe bulk group dispatch and the reuse of
  `next_index` as the operation cursor.
- ADR 0030: record that bulk mutations under constraints are deferred.

## Related

- ADR 0029: shard-local atomicity and cross-canister consistency.
- ADR 0030: cross-shard uniqueness TCC reservation.
- ADR 0041: Router-to-Graph batch mutation dispatch.
- ADR 0042: Router dynamic instruction-budget batching.
- ADR 0046: multi-variable candidate seed relations and canonical Graph revalidation.
- Plan 0096: Router sequential chunk dispatch.
