---
name: "Router bulk mutation key"
overview: "Implement wave-level saga coalescing for homogeneous seed groups as specified in ADR 0044."
todos:
  - id: "version-stable-records"
    content: "Version RouterMutationRecord, RouterMutationShard, and GraphMutationJournalEntry as enums with V1 variants."
    status: in_progress
    note: "RouterMutationRecord and RouterMutationShard versioning compile (gleaph-router lib+tests). GraphMutationJournalEntry versioning pending."
  - id: "extend-graph-batch-journal"
    content: "Extend Graph execute_plan_update_batch and mutation journal to support one MutationId with multiple operations and a durable operation cursor."
    status: pending
  - id: "group-input-mutations"
    content: "Add Router ingress logic to detect consecutive compatible mutations and assign a single client_mutation_key / MutationId per group."
    status: pending
  - id: "bulk-dispatch-and-continuation"
    content: "Dispatch bulk groups through the existing Graph batch endpoint and handle partial results / next_index inside one ingress call."
    status: pending
  - id: "per-mutation-results"
    content: "Map bulk group results back to one GqlQueryResult per input mutation while preserving input order."
    status: pending
  - id: "update-recovery"
    content: "Ensure Router recovery and projection semantics handle bulk mutation records correctly."
    status: pending
  - id: "disable-bulk-under-constraints"
    content: "Reject bulk path for mutations under active uniqueness or constrained-property constraints (ADR 0030)."
    status: pending
  - id: "tests"
    content: "Add focused unit and PocketIC tests for bulk grouping, partial progress, retry idempotency, and single-mutation compatibility."
    status: pending
  - id: "measure"
    content: "Run social-demo deploy with batch-instr-log and verify per-ingress seed count improves over baseline."
    status: pending
  - id: "docs"
    content: "Update ADR 0029/0041/0042/0030 references as listed in ADR 0044."
    status: pending
isProject: false
---

# Plan 0097: Router bulk mutation key

## Objective

Implement ADR 0044: when `gql_execute_idempotent_batch` receives consecutive
homogeneous mutations (same query plan, graph, and mode), coalesce them into one
bulk group with a single `client_mutation_key` and `MutationId`. Execute the
group through the existing Graph batch endpoint so the Router stable saga cost
is paid once per group instead of once per seed.

Success signal: a fresh `SOCIAL_DEMO_USER_SCALE=5 POST_SCALE=20` deploy shows a
significant increase in seeds processed per Router ingress call, and existing
single-mutation and PocketIC tests still pass.

## Context

- ADR 0029 defines the Router mutation lifecycle and Graph mutation journal.
- ADR 0041 added the Graph `execute_plan_update_batch` endpoint.
- ADR 0042 added the dynamic `gql_execute_idempotent_batch` cursor API.
- ADR 0044 (this slice's design contract) specifies the bulk mutation key
  approach and the versioned stable records.
- Plan 0096 refactored the ingress executor to be sequential and coalesced; this
  slice builds on top of it.

Measured baseline (before this slice):

- Router per-mutation prepare cost: ~25.7M instructions
- Dominant phases: `reserve` 6.3M, `replay` 6.5M, `envelope` 8.0M
- Effective throughput: ~570 seeds per ingress call

## Scope

In scope:

- Version the stable value types listed in ADR 0044.
- Extend the Graph batch execution path to execute multiple operations under one
  `MutationId` and persist a single durable journal entry with an operation
  cursor.
- Detect homogeneous groups inside `gql_execute_idempotent_batch` and assign a
  single `client_mutation_key` / `MutationId` to each group.
- Dispatch bulk groups to Graph and handle `next_index` continuation inside the
  same ingress call.
- Return one `GqlQueryResult` per input mutation, preserving order.
- Keep single-mutation path unchanged.
- Reject bulk path when ADR 0030 uniqueness/constrained-property constraints are
  active for the mutation.
- Update related ADRs and run measured validation.

Out of scope:

- Cross-graph or cross-mode group coalescing.
- Bulk path under active uniqueness/constrained-property constraints.
- Frontend seed script changes beyond using the existing batch API.
- Changing the public Candid shape of `gql_execute_idempotent_batch`.

## Expected Change Surface

Intended paths:

- `crates/router/src/facade/stable/label_stats.rs` — `RouterMutationRecord` / `RouterMutationShard` versioning and bulk fields.
- `crates/graph-kernel/src/plan_exec.rs` — `GraphMutationJournalEntry` versioning and `GraphBulkMutationProgress`.
- `crates/graph/src/canister/handlers.rs` — `execute_plan_update_batch` bulk mutation handling.
- `crates/graph/src/facade/store/mutation_journal.rs` — journal read/write for bulk cursor.
- `crates/router/src/gql.rs` — group detection, bulk dispatch mapping, result mapping.
- `crates/router/src/batch_wave.rs` — bulk-aware `PreparedMutation` if needed.
- `crates/router/src/facade/store/idempotency.rs` — bulk record helpers.
- `design/adr/0029.md`, `design/adr/0041.md`, `design/adr/0042.md`, `design/adr/0030.md` — reference updates.

Known unrelated paths in the current worktree:

- `crates/router/src/gql.rs` already contains `PrepareInstrLogger` and
  preflight-cache instrumentation from earlier measurement work. This slice
  should keep that code but disable the `batch-instr-log` feature in
  `icp.yaml` before final validation.
- Temporary files `router_instr_log_full.txt`, `graph_instr_log_full.txt`, and
  `.icp_home/` must be removed before the final commit.

## Steps

1. **Version stable value types**
   - Convert `RouterMutationRecord` to an enum with `V1(RouterMutationRecordV1)`.
   - Convert `RouterMutationShard` to an enum with `V1(RouterMutationShardV1)`.
   - Convert `GraphMutationJournalEntryWire` to `GraphMutationJournalEntry`
     with `V1(GraphMutationJournalEntryV1)`.
   - Update all read/write sites to unwrap the V1 variant.

2. **Extend Graph mutation journal**
   - Add `GraphBulkMutationProgress::V1` with `operation_count` and
     `completed_count`.
   - Add `bulk_progress: Option<GraphBulkMutationProgress>` to the journal entry.
   - Reuse the existing `next_index` field as the bulk operation cursor.
   - Update `get_mutation_journal_entries` and journal read helpers to return the
     new type.

3. **Extend Graph batch execution**
   - In `execute_plan_update_batch`, when all operations share the same
     `mutation_id`, persist one journal entry and update `next_index` on partial
     progress.
   - On retry, read the journal entry and skip already completed operations.
   - Each operation still executes through the existing single-operation core.

4. **Detect homogeneous groups in Router ingress**
   - Inside `gql_execute_idempotent_batch`, scan the input list and group
     consecutive mutations with the same graph, mode, and plan blob.
   - Assign a single `client_mutation_key` (derived from the first mutation's
     key) and reserve one `MutationId` per group.
   - Build one set of dispatches per group instead of per mutation.

5. **Bulk dispatch and continuation**
   - Send the group as one `ExecutePlanBatchArgs` call per target Graph
     canister.
   - If Graph returns `next_index`, continue the same group in the same ingress
     call until complete or the Router budget requires a cursor.
   - Update the Router saga record with `completed_row_count` and mark completed
     when the group finishes.

6. **Map results back per input mutation**
   - For each group, produce one `GqlQueryResult` per input mutation.
   - Preserve input order in the final `GqlExecuteIdempotentBatchResult`.

7. **Recovery and constraint guards**
   - Ensure recovery reads the durable Graph journal cursor correctly.
   - Reject the bulk path when `unique_claims` or `constrained_properties` are
     non-empty.

8. **Tests**
   - Unit tests for group detection and key assignment.
   - PocketIC test for a bulk group that completes in one Graph call.
   - PocketIC test for partial Graph progress and retry idempotency.
   - Regression test that single-mutation callers still create one saga record
     each.

9. **Measurement**
   - Re-enable `batch-instr-log` in `icp.yaml` for one deploy.
   - Run `SOCIAL_DEMO_USER_SCALE=5 SOCIAL_DEMO_POST_SCALE=20` fresh deploy.
   - Compare seeds per ingress call against the ~570 baseline.
   - Disable `batch-instr-log` before final commit.

10. **Docs**
    - Update ADR 0029, 0041, 0042, and 0030 with the references listed in ADR
      0044.

## Validation

- `cargo fmt --all`
- `cargo clippy -p gleaph-router --all-targets --all-features -- -D warnings`
- `cargo clippy -p gleaph-graph --all-targets --all-features -- -D warnings`
- `cargo test -p gleaph-router --lib`
- `cargo test -p gleaph-graph --lib`
- `cargo test -p gleaph-pocket-ic-tests --test router_bulk_mutation_key`
- Fresh local network deploy:
  ```sh
  icp network stop
  icp network start local -d
  SOCIAL_DEMO_USER_SCALE=5 SOCIAL_DEMO_POST_SCALE=20 GLEAPH_DEMO_FORCE_VITE_IC_HOST=1 ./scripts/deploy-social-demo-local.sh
  ```
- `batch-instr-log` measurement showing improved seeds per ingress.

## Completion Criteria

- [ ] `RouterMutationRecord`, `RouterMutationShard`, and
      `GraphMutationJournalEntry` are versioned enums with V1 variants.
- [ ] Graph batch endpoint executes multiple operations under one `MutationId`
      and persists one durable journal entry with an operation cursor.
- [ ] Router ingress detects homogeneous groups and assigns one bulk key/id per
      group.
- [ ] Partial Graph progress is resumed inside the same ingress call.
- [ ] One `GqlQueryResult` is returned per input mutation in input order.
- [ ] Single-mutation path is unchanged.
- [ ] Bulk path is rejected under active uniqueness/constrained-property
      constraints.
- [ ] New tests pass and social-demo deploy completes.
- [ ] `batch-instr-log` is disabled in `icp.yaml` for the final tree.
- [ ] Related ADRs are updated.

## Later slices

- ADR 0030 extension for bulk uniqueness/constrained-property support.
- Cross-graph or cross-mode group coalescing if workloads require it.
- Frontend seed script group generation to reduce input list size.

## Risks

- Stable layout enum versioning touches many files and must be done carefully
  to avoid leaving partially migrated types.
- Group detection must exactly match planner output; mismatch causes silent
  missed coalescing or incorrect routing.
- Bulk journal semantics changes affect recovery; partial progress tests are
  essential.
- The `batch-instr-log` feature must not remain enabled in `icp.yaml`.
