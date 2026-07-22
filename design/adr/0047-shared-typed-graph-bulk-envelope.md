# 0047. Shared typed Graph bulk execution envelope

Date: 2026-07-22
Status: Proposed
Last revised: 2026-07-22
Anchor timestamp: 2026-07-22 01:07:32 UTC +0000

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
7. defines a safe Graph-first activation sequence and the required reset boundary for the
   intentionally incompatible Router V1 stable schema; and
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
- A separately named method preserves Candid compatibility. Activation is an explicit durable
  Router-registry decision after Graph advertises support; method rejection is not used as routine
  feature negotiation.
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
    pub batch_mode: ExecutePlanBatchMode,
}

pub struct ExecutePlanBatchTypedShared {
    pub target_shard_id: ShardId,
    pub element_id_encoding_key: [u8; 16],
    pub mutation_id: MutationId,
    pub plan_blob: Vec<u8>,
    pub resolved_labels: Option<ResolvedLabelTable>,
    pub resolved_properties: Option<ResolvedPropertyTable>,
    pub indexed_properties: Option<IndexedPropertyCatalog>,
    pub indexed_embeddings: Option<IndexedEmbeddingCatalog>,
}

pub struct ExecutePlanTypedOp {
    /// Per-operation GQL parameter map, already encoded.
    pub params_blob: Vec<u8>,
    /// Required complete-row seed relation. Zero matches use an empty `rows` vector.
    pub seed: SeedBindingsWire,
}
```

Notes:

- `mutation_id` is still in the shared header. Every operation in the batch belongs to the same
  bulk group and shares one `MutationId`, matching the current ADR 0044 implementation.
- Typed V1 is limited to one target Graph canister and one target shard. The Router proves
  `dispatches.len() == 1`; multi-shard groups retain their existing semantics-safe path. This makes shared
  `target_shard_id` an enforced invariant rather than a POSTED-only assumption.
- `plan_blob`, resolved tables, and catalogs are shared for the homogeneous POSTED case. The Router
  builder is responsible for not moving heterogeneous fields into the shared header.
- The new method is an update method, so `GqlExecutionMode` is not carried. `batch_mode` retains the
  existing Fixed/Dynamic instruction-budget behavior.
- Typed V1 admits only complete-row seeded groups with no resolved-search relation and with all
  federated/local uniqueness and constrained-property dispatch vectors empty. Other groups retain
  their existing semantics-safe path. The V1 wire therefore carries no dormant claim/search fields.
- Typed V1 also requires a mutation plan whose result shape has a conservative encoded-response
  bound: `rows_blob` is statically absent, and the execution shape has a finite per-operation bound
  for `hot_forward_vertices`. One shared `gleaph-graph-kernel` eligibility/bound helper derives this
  fact from the plan once per group and is used by both Router admission and Graph validation; the
  generic GQL parser/planner does not gain a Gleaph transport flag. Plans without that proof retain
  their existing semantics-safe path.

### Seed representation and invalid-state prevention

`ExecutePlanTypedOp.seed` is a required `SeedBindingsWire`. The legacy scalar path uses
`ExecutePlanArgs.seed_bindings_blob: Option<Vec<u8>>`. Because the two request shapes are distinct,
there is no value that contains both. This satisfies the "no blob-plus-typed-seed dual state"
requirement. Zero-match operations remain explicit typed seeds with empty rows, so absence cannot
silently change complete-prefix semantics.

### Result shape

Reuse `ExecutePlanBatchResult`. It already returns ordered per-operation results and
`next_index`. The new method returns the same type.

### Graph behavior

1. The new method decodes `ExecutePlanBatchTypedArgs`.
2. Graph validates the single-shard target, non-empty bounded operation list, required complete-row
   seeds, and the actual encoded request size before executing an operation.
3. The execution core accepts already-decoded `Option<SeedBindingsWire>` and
   `Option<ResolvedSearchWire>` values. The legacy handlers decode their blobs once and call this
   core; the typed handler passes `Some(op.seed)` directly. It must not encode a typed seed back to
   bytes or invoke the legacy inner decoder.
4. Bulk detection, journal resume, sequential execution, result ordering, partial failure, and
   journal commit use the existing batch machinery after this boundary normalization.
5. The instruction-budget cutoff (`Dynamic` mode) continues to set `next_index` to the first
   unattempted operation.

This means the Graph durable state does not need a new variant to support typed seeds; it only
needs the Router to resend the same operation sequence on retry. The durable replay envelope is
Router's responsibility.

### Durable Router replay envelope

Gleaph has no deployed Router stable state that must survive this change. Do not add a V2 migration
layer. Redefine `RouterMutationRecord::V1` incompatibly around one exhaustive payload so that the
legacy and typed replay forms cannot coexist:

```rust
pub enum RouterMutationRecord {
    V1(RouterMutationRecordV1),
}

pub struct RouterMutationRecordV1 {
    // Existing mutation identity, fingerprint, lifecycle, resolved tables,
    // routing lease, timestamps, and error fields remain here.
    pub payload: RouterMutationPayloadV1,
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
    },
}

pub struct TypedSeedBulkReplayV1 {
    pub total_ops: u32,
    pub target_shard: RouterMutationShardV1,
    pub shared: TypedSeedBulkSharedHeaderV1,
    pub operations: Vec<TypedSeedBulkOperationV1>,
}

pub struct TypedSeedBulkSharedHeaderV1 {
    pub element_id_encoding_key: [u8; 16],
    pub plan_blob: Vec<u8>,
    pub mode: GqlExecutionMode,
    pub resolved_labels: Option<ResolvedLabelTable>,
    pub resolved_properties: Option<ResolvedPropertyTable>,
    pub indexed_properties: Option<IndexedPropertyCatalog>,
}

pub struct TypedSeedBulkOperationV1 {
    pub params: Vec<u8>,
    pub seed_bindings: SeedBindingsWire,
}
```

`RouterMutationRecordV1.mutation_id`, `request_fingerprint`, `resolved_labels`, and
`resolved_properties` remain authoritative at the record top-level. During recovery, the Router
copies `resolved_labels` and `resolved_properties` from the record top-level into the
`ExecutePlanBatchTypedShared` shared header; `TypedSeedBulkReplayV1` intentionally does not
duplicate them. `TypedSeedBulkReplayV1.target_shard` owns the sole target and its outcome; the remainder
of `TypedSeedBulkReplayV1.shared` owns the shared plan/catalog/label/property data and `operations` owns the ordered params/seeds. The typed
variant has no `seed_bindings_blob`, so the stable source of truth cannot represent both encodings.
Scalar and legacy-bulk lifecycle shape is also exhaustive rather than a boolean plus an optional
bulk record. The Router reconstructs `ExecutePlanBatchTypedArgs` directly from this payload,
including the original execution mode in `shared.mode`.

Typed V1 is single-shard, and `replay.operations.len() == total_ops`. The owning constructor and
stable write boundary validate this invariant; general multi-shard typed replay is a later schema,
not an implicit parallel-vector contract.

The complete typed V1 payload is persisted before the first Graph `await`, in the same Router
message that publishes the outbound call. Any validation or encoding failure occurs before that
await and emits no Graph call. If Graph commits but the Router callback fails, the durable typed
payload remains `CanonicalPending`; recovery resends the same ordered operations and relies on
Graph's journal cursor.

Typed V1 bounds are normative:

- `1 <= total_ops <= 1024`;
- each seed has at most 1,024 complete rows, preserving the existing complete-row bound;
- each already-encoded `params_blob.len()` is at most 2 MiB;
- Candid encoding of the reconstructed full `ExecutePlanBatchTypedArgs` is at most 2 MiB; and
- Candid encoding of the complete `RouterMutationRecord::V1` typed payload is at
  most 2 MiB.

The Router owning constructor checks every bound before the stable write. Graph independently checks
operation count, seed row count, target shard, and full request bytes. The Router must not Candid-
encode each seed to enforce an individual byte bound: that would recreate the per-item encoding cost
this ADR exists to remove. Seed admission uses structural row/count bounds, and the one full typed
request encoding is the sole encoded seed-size proof. If any typed bound fails, the Router selects a
semantics-safe existing path before writing typed state; it never silently truncates or writes a
record that cannot be dispatched. A group with distinct per-operation seeds must fall back to the
existing sequential scalar path, because the legacy bulk record's one blob per shard cannot replay
that group. The legacy batch path is eligible only when its existing replay contract is independently
sufficient, such as a shared seed relation.

### Capability and rollout

1. **Graph-first deployment.** Deploy a Graph canister build that exposes the new typed batch method
   while keeping the legacy scalar and batch methods. The new method is inert until a Router calls
   it.
2. **Capability advertisement and activation.** Graph exposes a read-only
   `execution_capabilities` query containing an exhaustive `TypedSeedBatchV1` capability. A
   control-plane-admin Router refresh update captures the target `GraphShardKey` and Graph canister
   principal, calls that query, then re-reads the registry after the `await`. It writes
   `typed_seed_batch_v1: true` only if the same registry key still names the same live Graph
   principal; otherwise it returns an error without changing capability state. The field extends
   `ShardRegistryEntry` and the current `ShardRegistryStableRecord::V2` write shape; the retained
   decode-only V1 registry variant is not redefined. The durable registry field is the data-plane
   source of truth; no heap cache or call rejection enables the path. Refresh failure does not
   enable the capability, and an explicit guarded admin clear disables future typed admission.
3. **Router cutover.** Once all target Graph canisters for a graph support the new method, the
   Router uses it for eligible homogeneous bulk groups.
4. **Capability-disabled fallback.** Before durable admission, a false capability selects the
   existing scalar path for groups with distinct per-operation seeds. It may select the legacy batch
   path only when that path can replay the group exactly.
5. **Ambiguous typed-call outcome.** Once typed V1 replay is durable, a reject, bounded-wait unknown
   outcome, decode failure, or callback trap never converts that record to scalar or legacy batch.
   The Router leaves it `CanonicalPending` and retries the identical typed request under the same
   `MutationId` and operation order after compatible Graph service is restored; Graph journal resume
   remains the authority for the committed prefix. Clearing capability affects only new admission.
6. **Incompatible V1 installation.** Before installing the Router implementation, stop the local or
   pre-production stack and fresh-install/reset Router stable state. The old and new V1 byte layouts
   are intentionally not mutually decodable; no migration or backward-decoder is provided.
7. **Rollback boundary.** This pre-production decision does not provide data-preserving rollback to
   an older Router. Rolling the Router back requires another fresh install/reset. Before removing
   the Graph method, disable the capability and stop/reset any Router that can still replay typed
   records. Graph-first deployment still allows the new Graph method to be rolled out inertly.

### Chunking and payload bounds

- Typed V1 admits only a group whose complete actual encoded request fits under
  `MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES`. The existing binary-search sizing helper is
  reused as a full-request proof; an oversized distinct-seed group selects the sequential scalar
  path before typed persistence.
  Multi-request typed size chunking is deferred because splitting one Graph journal ordinal space
  requires a separate protocol decision.
- An independent operation-count/instruction bound remains: `Dynamic` mode in Graph stops at the
  first unattempted operation when the instruction budget is exhausted.
- The encoded size probe is performed before the inter-canister call, so no call is issued with an
  oversized payload.
- Before typed persistence, the Router calls the shared graph-kernel helper to prove a conservative
  bound for the complete encoded `ExecutePlanBatchResult`: fixed per-item result overhead plus the
  derived maximum `hot_forward_vertices` must fit under the same portable 2 MiB limit, and
  `rows_blob` must be statically absent. Graph applies the same classifier before executing the first
  operation and retains its final actual-response encode guard as defense in depth, not as the
  admission mechanism after mutations may already have committed. A plan without this proof uses
  the existing scalar path.
> Ownership note:  continues to own portable wire types and payload
> constants, but it cannot own physical-plan classification because it sits below
>  in the dependency graph (a cycle would be required). The renamed
>  crate already depends on both planner and graph-kernel, so it is the
> future owner of the  admission classifier. This slice corrects the planned ownership
> boundary; the classifier itself remains a Plan 0109 deliverable.


### Performance expectation

The design is expected to retain the Plan 0105 measured saving for the inner seed encoding because
`ExecutePlanTypedOp.seed: SeedBindingsWire` replaces `ExecutePlanArgs.seed_bindings_blob`
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
- `RouterMutationRecord::V1` is redefined incompatibly with exhaustive `Scalar`, `LegacyBulk`, and
  `TypedSeedBulk` payload variants. No Router V2 or stable migration is introduced.
- Typed V1 is deliberately single-shard, complete-row seeded, unconstrained, search-free, and
  limited to plans with a statically bounded row-free response shape.
- The stable shard registry gains an explicit admin-refreshed `typed_seed_batch_v1` capability.
- Recovery no longer needs to repeat Property Index lookup for bulk groups that used the typed
  path.
- The legacy scalar method remains the semantics-safe fallback for distinct-seed groups. The legacy
  batch method remains available only where its existing durable replay shape is sufficient.
- Initial Router rollout and rollback to older Router Wasm require fresh install/reset because the
  V1 stable layout is intentionally incompatible and Gleaph has no deployed state to migrate.
- End-to-end adoption is gated on measured Router ingress savings; if the gate fails, the typed
  method and typed V1 payload are not adopted.
- The new surface must be covered by focused PocketIC Router→Graph tests before production release.

Required validation includes: the shared response classifier accepting the measured POSTED shape
and rejecting row-returning or unbounded shapes; capability refresh refusing a target replaced
during its `await`; typed rejection/unknown-outcome recovery retaining the same durable request and
mutation id; response-bound failure selecting scalar before typed persistence or any Graph call; and
a canbench/Router instruction check proving admission does not perform one seed encode per item.

## Related documents

- [ADR 0044](0044-router-bulk-mutation-key.md): bulk mutation identity, progress, and per-item result
  mapping.
- [ADR 0046](0046-multi-variable-candidate-seed-relations.md): versioned seed-relation envelope and
  deterministic replay requirements.
- [Physical plan format](../gql/plan-format.md): `ExecutePlanArgs` and seed transport contract.
- Plan 0105: measurement that justified this design.
