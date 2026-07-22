# 0047. Shared typed Graph bulk execution envelope

Date: 2026-07-22
Status: Proposed
Last revised: 2026-07-22
Anchor timestamp: 2026-07-22 00:33:37 UTC +0000

## Context

Plan 0105 measured the cost of carrying per-item seed relations inside `ExecutePlanArgs` as opaque
nested Candid blobs. For a POSTED-shaped complete-row seed (one variable, one row, one vertex
binding, distinct `local_vertex_id` per item), replacing the nested `Vec<Option<Vec<u8>>>`
structure with an outer typed `Vec<SeedBindingsWire>` reduced Router encoding cost by approximately
263K instructions per item at N=512 and 256K instructions per item at N=32. Typed decoding was
also cheaper by about 289K instructions per item at N=512, and the encoded byte count dropped from
75,791 to 8,836 bytes. Both per-item Router savings exceed the current 156,799-instruction adoption
threshold.

The existing boundaries are:

- `ExecutePlanArgs` is the per-operation Router→Graph request. It repeats `plan_blob`,
  `resolved_labels`, `resolved_properties`, `indexed_properties`, `indexed_embeddings`,
  `unique_claims`, `constrained_properties`, and similar immutable context on every operation.
- `ExecutePlanBatchArgs` is `Vec<ExecutePlanArgs>` plus `ExecutePlanBatchMode`. It is the only
  Router→Graph batch transport.
- `ExecutePlanArgs.seed_bindings_blob: Option<Vec<u8>>` carries the per-operation seed relation as
  an opaque Candid-encoded `SeedBindingsWire`.
- `ExecutePlanBatchResult` returns one `Result<ExecutePlanResult, String>` per operation and an
  optional `next_index` continuation cursor.
- ADR 0044 groups many operations under one `MutationId`. Graph detects a bulk group when all
  operations share the same `mutation_id` and persists one journal entry with a
  `next_index` cursor.
- ADR 0044 already versions `RouterMutationRecord`, `RouterBulkMutationState`, and
  `RouterMutationShard` as enums. The current `RouterMutationShardV1` stores one
  `seed_bindings_blob` per shard, which is sufficient only when every operation in the group has
  the same seed relation.
- ADR 0046 Phase 1 resolves per-item complete-row seeds and materializes a bounded Cartesian
  product into `SeedBindingsWire::rows` with `complete_prefix_rows: true`. The current durable
  record does not preserve distinct per-operation seeds for recovery; it relies on the seed blob
  being reconstructed during replay.

## Problem

Design a production Router→Graph bulk transport that:

1. realizes the measured typed-seed encoding saving in an end-to-end Router ingress path;
2. does not change the scalar `ExecutePlanArgs` production API;
3. prevents a single operation from carrying both a legacy blob and a typed seed;
4. shares truly immutable group fields once while keeping per-operation fields that can actually
   differ;
5. preserves one bulk-group `MutationId`, stable operation ordinals, ordered per-item results,
   `next_index` continuation, partial failure, and Graph journal resume;
6. provides a durable, deterministic replay representation so recovery does not repeat Property
   Index lookup;
7. defines a safe mixed-version deployment and rollback sequence; and
8. fails closed if the actual encoded request exceeds the safe inter-canister payload limit or the
   Graph instruction budget.

## Existing architecture assessment

### Current Router→Graph request construction

The bulk path in `crates/router/src/gql.rs` builds an `ExecutePlanArgs` for every
`(item_index, dispatch_index)` pair. The closure copies:

- `plan_blob`, `element_id_encoding_key`, `mode`, `mutation_id` — identical per group;
- `resolved_labels`, `resolved_properties`, `indexed_properties`, `indexed_embeddings`,
  `unique_claims`, `constrained_properties`, `local_unique_claims`,
  `local_constrained_properties` — identical for the homogeneous POSTED group, but variable across
  heterogeneous calls;
- `params_blob` — per item;
- `seed_bindings_blob` — per item and per shard;
- `resolved_search_blob` — per shard.

The resulting `Vec<ExecutePlanArgs>` is chunked by `graph_batch_chunk_len_for_bulk`, which binary
searches the largest prefix whose encoded `ExecutePlanBatchArgs` length is at most
`MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES` (2 MiB). It already encodes a probe of the full
batch to measure bytes.

### Current Graph batch execution

`execute_plan_update_batch` calls `execute_plan_batch`, which:

1. re-encodes the received `ExecutePlanBatchArgs` to check the request size;
2. detects a bulk `MutationId` when all operations share `mutation_id`;
3. loads the durable journal entry and resumes from `next_index`;
4. iterates operations sequentially, calling `execute_plan_impl` for each;
5. on partial failure, persists `next_index` pointing after the failed operation and stops;
6. on success, persists one journal entry with total row count, delta seq range, hot-forward
   vertices, total operation count, and completed count.

`ExecutePlanResult` carries `row_count`, `rows_blob`, and `hot_forward_vertices`. Results are
returned in input order.

### Durable Router state

`RouterMutationRecord::V1` stores shared resolved tables, per-shard `RouterMutationShardV1`, and a
`bulk_state` with `total_ops`. `RouterMutationShardV1` stores only one `seed_bindings_blob`. When a
bulk group has distinct per-operation seeds (ADR 0046 Phase 1), the current record cannot replay
without reconstructing the seeds. ADR 0046 already identifies this gap.

### Durable Graph state

`GraphMutationJournalEntryV1` stores `mutation_id`, `state`, `committed_row_count`,
`next_index`, and `bulk_progress`. It does not store per-operation seeds; it relies on the Router
resending them.

## Decision

### Selected alternative: new typed batch endpoint with a shared group header

Add a **new Candid update method** and a corresponding new `ExecutePlanBatchTypedArgs` request
shape. Keep `execute_plan_update` (scalar) and `execute_plan_update_batch` (legacy blob batch)
unchanged. The new method is the only production path that carries typed per-operation seeds.

Rationale:

- Changing `ExecutePlanArgs` in place would require proving Candid subtyping for every existing
  caller and would introduce the invalid blob-plus-typed-seed combination. A new request type
  avoids both problems.
- A new method lets Graph prove capability simply by being deployed; Router can fall back to the
  legacy batch method during any mixed-version window.
- The minimum-change alternative (keep nested blobs but use a cheaper inner codec) cannot retain
  the measured typed-vector saving without either a second seed schema or hand-written Candid
  patching, neither of which is acceptable.
- The large-change alternative (a general versioned execution-envelope protocol) adds query/update
  generality we do not need and would couple unrelated surfaces. Rejected.

### Wire ownership and crate placement

All new wire types live in `gleaph-graph-kernel` under `plan_exec`. Router and Graph only use them;
they do not define parallel copies.

### Request shape

```rust
pub struct ExecutePlanBatchTypedArgs {
    /// Shared immutable context for the entire group.
    pub shared: ExecutePlanBatchTypedShared,
    /// Ordered per-operation records. The order is the public item order seen by the caller.
    pub operations: Vec<ExecutePlanTypedOp>,
    pub mode: ExecutePlanBatchMode,
}

pub struct ExecutePlanBatchTypedShared {
    pub target_shard_id: ShardId,
    pub element_id_encoding_key: [u8; 16],
    pub mutation_id: MutationId,
    pub plan_blob: Vec<u8>,
    pub mode: GqlExecutionMode,
    pub resolved_labels: Option<ResolvedLabelTable>,
    pub resolved_properties: Option<ResolvedPropertyTable>,
    pub indexed_properties: Option<IndexedPropertyCatalog>,
    pub indexed_embeddings: Option<IndexedEmbeddingCatalog>,
    pub unique_claims: Option<Vec<UniqueClaimDispatch>>,
    pub constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
    pub local_unique_claims: Option<Vec<UniqueClaimDispatch>>,
    pub local_constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
}

pub struct ExecutePlanTypedOp {
    /// Per-operation GQL parameter map, already encoded.
    pub params_blob: Vec<u8>,
    /// The seed relation for this operation. Absent means the same semantics as
    /// `seed_bindings_blob: None` on scalar `ExecutePlanArgs`.
    pub seed: Option<SeedBindingsWire>,
    /// Per-operation resolved non-leading SEARCH relation. `None` for operations whose shard
    /// has no such relation.
    pub resolved_search: Option<ResolvedSearchWire>,
}
```

Notes:

- `mutation_id` is still in the shared header. Every operation in the batch belongs to the same
  bulk group and shares one `MutationId`, matching the current ADR 0044 implementation.
- `target_shard_id` is shared because one batch call targets one Graph canister. Cross-shard
  dispatch remains separate calls.
- `plan_blob`, resolved tables, and catalogs are shared for the homogeneous POSTED case. The Router
  builder is responsible for not moving heterogeneous fields into the shared header.
- `mode` appears both in shared header (group mode) and could be omitted per op. It is kept once
  because all operations in one batch have the same mode.
- `resolved_search` remains per operation because it can differ per shard and per item even within
  a homogeneous group.

### Seed representation and invalid-state prevention

`ExecutePlanTypedOp.seed` is `Option<SeedBindingsWire>`. The legacy scalar path uses
`ExecutePlanArgs.seed_bindings_blob: Option<Vec<u8>>`. Because the two request shapes are distinct,
there is no value that contains both. This satisfies the "no blob-plus-typed-seed dual state"
requirement without an exhaustive enum.

### Result shape

Reuse `ExecutePlanBatchResult`. It already returns ordered per-operation results and
`next_index`. The new method returns the same type.

### Graph behavior

1. The new method decodes `ExecutePlanBatchTypedArgs`.
2. It converts the typed request into the same internal representation the legacy batch uses:
   ordered operations, each with its own `ExecutePlanArgs`-equivalent state. The conversion is local
   to Graph; the durable journal remains unchanged.
3. Bulk detection, journal resume, sequential execution, result ordering, partial failure, and
   journal commit use the existing `execute_plan_batch` machinery.
4. The instruction-budget cutoff (`Dynamic` mode) continues to set `next_index` to the first
   unattempted operation.

This means the Graph durable state does not need a new variant to support typed seeds; it only
needs the Router to resend the same operation sequence on retry. The durable replay envelope is
Router's responsibility.

### Durable Router replay envelope

A new `RouterMutationRecord` variant is required. The V1 record cannot represent distinct ordered
per-operation seeds. Define:

```rust
pub enum RouterMutationRecord {
    V1(RouterMutationRecordV1),
    V2(RouterMutationRecordV2),
}

pub struct RouterMutationRecordV2 {
    pub mutation_id: MutationId,
    pub created_at_ns: u64,
    pub request_fingerprint: Vec<u8>,
    pub routing_in_progress: bool,
    pub routing_lease_ns: Option<u64>,
    pub terminal_failure: Option<String>,
    pub last_error: Option<String>,
    pub completed_row_count: Option<u64>,
    // Shared immutable group context, exactly what the typed batch sends.
    pub shared: TypedBatchSharedReplay,
    // Ordered per-operation replay records.
    pub operations: Vec<TypedBatchOperationReplay>,
    // Per-shard outcome state, parallel to legacy shard records but without seed blobs.
    pub shard_outcomes: Vec<TypedBatchShardOutcome>,
}

pub struct TypedBatchSharedReplay {
    pub plan_blob: Vec<u8>,
    pub element_id_encoding_key: [u8; 16],
    pub mode: GqlExecutionMode,
    pub resolved_labels: Option<ResolvedLabelTable>,
    pub resolved_properties: Option<ResolvedPropertyTable>,
    pub indexed_properties: Option<IndexedPropertyCatalog>,
    pub indexed_embeddings: Option<IndexedEmbeddingCatalog>,
    pub unique_claims: Option<Vec<UniqueClaimDispatch>>,
    pub constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
    pub local_unique_claims: Option<Vec<UniqueClaimDispatch>>,
    pub local_constrained_properties: Option<Vec<ConstrainedPropertyDispatch>>,
}

pub struct TypedBatchOperationReplay {
    pub params_blob: Vec<u8>,
    pub seed: Option<SeedBindingsWire>,
    pub resolved_search: Option<ResolvedSearchWire>,
    pub fingerprint: Vec<u8>,
}

pub struct TypedBatchShardOutcome {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub completed: bool,
    pub projection_advanced: bool,
    pub row_count: u64,
}
```

Design constraints on the durable envelope:

- It is the single source of truth for replay. Recovery never repeats Property Index lookup.
- Seed relations are stored as `SeedBindingsWire`, not as a second encoded blob, so there is one
  canonical in-memory representation.
- Bounds: total operation count, total encoded seed byte size, and per-operation parameter/blob size
  must have explicit limits. Exceeding a bound fails closed with an explicit error.
- The V1 record remains decodable. New records are written as V2 only when the typed batch path is
  used. Rollback recovery must be able to read V2 and either continue with the typed method or
  convert to the legacy batch method if the Graph canister does not yet support the new method.

### Capability and rollout

1. **Graph-first deployment.** Deploy a Graph canister build that exposes the new typed batch method
   while keeping the legacy scalar and batch methods. The new method is inert until a Router calls
   it.
2. **Router capability check.** Router determines whether the target Graph canister supports the
   new method before invoking it. The capability is recorded in the Router registry for each graph
   canister (a stable registry entry or ephemeral cached flag). Do not use call rejection as
   routine feature negotiation after a mutation may have committed.
3. **Router cutover.** Once all target Graph canisters for a graph support the new method, the
   Router uses it for eligible homogeneous bulk groups.
4. **Mixed-version window.** During the window, Router falls back to the legacy batch method for
   canisters that do not yet support the typed method. The durable V2 replay envelope must be
   convertible to legacy `ExecutePlanArgs` (encode each `SeedBindingsWire` as a blob) for this
   fallback.
5. **Router rollback.** Router may be rolled back to a version that does not know the typed method.
   It continues to use the legacy batch method. In-flight V2 records must either be completed by
   the newer Router before rollback or be recoverable by the older Router using legacy conversion.
6. **Graph rollback prohibition.** Do not roll back Graph while any active Router/recovery record
   may issue the typed method. This is the same constraint that applies to any new Candid method.

### Chunking and payload bounds

- Chunking uses the actual encoded byte length of `ExecutePlanBatchTypedArgs` under
  `MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES`, exactly as `graph_batch_chunk_len_for_bulk`
  does today.
- An independent operation-count/instruction bound remains: `Dynamic` mode in Graph stops at the
  first unattempted operation when the instruction budget is exhausted.
- The encoded size probe is performed before the inter-canister call, so no call is issued with an
  oversized payload.

### Performance expectation

The design is expected to retain the Plan 0105 measured saving for the inner seed encoding because
`ExecutePlanTypedOp.seed: Option<SeedBindingsWire>` replaces `ExecutePlanArgs.seed_bindings_blob`
with a typed vector. Additional outer-envelope encoding work is expected to be small because the
shared header is encoded once. The end-to-end adoption gate is 156,799 Router instructions saved
per POSTED item after a fresh social-demo deployment.

## Alternatives considered

### 1. Minimum change: keep nested blobs, only change the inner codec

Rejected. To match the typed-vector saving, the inner codec would need to avoid Candid's
per-message overhead for every operation. A cheaper self-describing format would introduce a second
seed schema and duplicate `SeedBindingsWire` semantics; hand-written Candid byte patching is
unacceptable because it depends on Candid LEB128 layout and can silently corrupt the wire.

### 2. New shared typed batch endpoint

Selected. It cleanly separates legacy and typed paths, prevents dual state, and realizes the
measured saving.

### 3. Large change: general versioned execution-envelope protocol

Rejected. It would couple query/update transport, require broader compatibility analysis, and
introduce scope not justified by the measured POSTED problem.

## Consequences

- A new public Candid method is added to Graph canisters. Router registry gains a capability flag
  per graph canister.
- Router durable mutation records gain a V2 variant with ordered per-operation replay state.
- Recovery no longer needs to repeat Property Index lookup for bulk groups that used the typed
  path.
- The legacy scalar and batch methods remain available for mixed-version support and fallback.
- End-to-end adoption is gated on measured Router ingress savings; if the gate fails, the typed
  method and V2 record are not adopted.
- The new surface must be covered by focused PocketIC Router→Graph tests before production release.

## Related documents

- [ADR 0044](0044-router-bulk-mutation-key.md): bulk mutation identity, progress, and per-item result
  mapping.
- [ADR 0046](0046-multi-variable-candidate-seed-relations.md): versioned seed-relation envelope and
  deterministic replay requirements.
- [Physical plan format](../gql/plan-format.md): `ExecutePlanArgs` and seed transport contract.
- Plan 0105: measurement that justified this design.
