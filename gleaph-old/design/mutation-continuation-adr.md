# ADR: Mutation Continuation — Self-Call vs. Coordinator Canister

**Status**: Accepted
**Date**: 2026-02-27
**Deciders**: Gleaph core team
**Context**: W5.4 (TODO-integrated.md)

## Problem

Heavy GQL mutations (bulk CREATE, large DELETE with MATCH scan, SET across many vertices) can exceed the IC's per-call instruction limit (~5B instructions for updates). Currently, mutations that exhaust the budget trap and lose all progress. We need a mechanism to split long-running mutations across multiple IC calls, each with a fresh instruction budget.

## Options

### Option A: Self-Call (`ic_cdk::call(ic_cdk::id(), "resume_mutation", ...)`)

The graph canister calls itself to continue a suspended mutation.

**Flow:**
1. Client sends `mutate(gql)` → update call begins.
2. Mutation executor runs with an instruction budget. If budget exhausts before completion, executor returns `Suspended { checkpoint, partial_result }`.
3. Canister serializes checkpoint to stable memory, then issues `ic_cdk::call(ic_cdk::id(), "resume_mutation", checkpoint_id)`.
4. Self-call arrives as a new update message with fresh ~5B instructions. Canister loads checkpoint, resumes mutation.
5. Repeat until `Done`. Final result returned to original caller via the self-call chain.

**Pros:**
- Simple architecture: no registry dependency, no inter-canister coordination.
- Mutation stays within a single canister — no cross-canister state.
- IC guarantees: self-calls are processed sequentially on the same subnet, so there's no reordering concern.
- Latency: ~2s per self-call round (single consensus round on same subnet).

**Cons:**
- Cycle cost: each self-call round costs ~590K cycles (call overhead) plus the execution cycles.
- Call depth: IC has no hard limit on self-call depth, but deeply chained calls accumulate callback contexts.
- Error handling: if any self-call traps, the entire chain unwinds. Checkpoint in stable memory survives for manual recovery.
- The original caller's response is delayed until the full chain completes (or can be made async with a separate query endpoint to poll status).

### Option B: Coordinator Canister (via Registry)

The registry canister orchestrates mutation resumption across calls.

**Flow:**
1. Client sends `mutate(gql)` to graph canister.
2. On budget exhaustion, graph canister stores checkpoint, returns `Suspended(checkpoint_id)` to client.
3. Client (or registry) calls `resume_mutation(checkpoint_id)` on graph canister.
4. Repeat until done.

**Pros:**
- No self-call chain — each call is independent.
- Registry could track mutation state across canisters.
- Client has visibility into progress.

**Cons:**
- Complex: requires registry involvement or client-side polling.
- Cross-canister calls add latency (~2-4s per hop if registry is on different subnet).
- Either the client must poll (bad UX) or the registry must drive the loop (complex coordination).
- Registry becomes a bottleneck/dependency for mutations.

### Option C: Client-Driven Polling

Client sends `mutate`, gets back `Suspended(token)`, then calls `resume_mutation(token)` repeatedly.

**Pros:**
- Simplest canister-side implementation.
- Client controls pacing.

**Cons:**
- Client must implement retry loop — bad DX for simple use cases.
- Incompatible with IC agent libraries that expect single request-response.
- Network latency adds up (client → IC → canister per round).

## Decision

**Option A: Self-Call** — chosen for simplicity and correctness.

### Rationale

1. **Simplicity**: Self-call is purely local to the graph canister. No registry dependency, no client-side polling logic, no cross-canister coordination.
2. **Transparency**: From the client's perspective, `mutate(gql)` is a single update call that returns `MutationResult` when done. The self-call chain is an internal implementation detail.
3. **Latency**: ~2s per round on the same subnet is acceptable. Bulk mutations that would take 50B instructions split into ~10 rounds = ~20s total, vs. trapping with zero progress today.
4. **Cycle cost**: Marginal. Self-call overhead (~590K cycles/round) is negligible compared to the mutation execution itself (~5B instructions = ~2.5B cycles per round).
5. **Error recovery**: Checkpoint in stable memory survives traps. A `get_pending_mutations()` query endpoint can expose orphaned checkpoints for manual recovery or GC.

### Key Design Points for W5.5 Implementation

**Checkpoint storage**: Keyed by `mutation_id: u64` (monotonic counter) in a dedicated stable-memory region. Each checkpoint stores:
- Original GQL string
- Mutation type (CREATE/DELETE/SET/REMOVE/MERGE)
- Progress cursor (e.g., next vertex ID to process, remaining elements in CREATE list)
- Partial result accumulator (affected_vertices, affected_edges so far)
- Timestamp (for expiry/GC)

**Instruction budget**: Use `ic_cdk::api::performance_counter(0)` to track instructions consumed. Suspend when approaching 80% of the per-call limit (~4B instructions) to leave headroom for checkpoint serialization and self-call setup.

**Self-call mechanism**:
```rust
#[update]
async fn resume_mutation(mutation_id: u64) -> Result<MutationResult, GleaphError> {
    let checkpoint = load_checkpoint(mutation_id)?;
    let outcome = execute_mutation_from_checkpoint(checkpoint);
    match outcome {
        MutationOutcome::Done(result) => {
            gc_checkpoint(mutation_id);
            Ok(result)
        }
        MutationOutcome::Suspended(new_checkpoint) => {
            save_checkpoint(mutation_id, new_checkpoint);
            ic_cdk::call(ic_cdk::id(), "resume_mutation", (mutation_id,)).await
                .map_err(|e| GleaphError::ExecutionError(format!("self-call failed: {e:?}")))?
        }
    }
}
```

**Concurrency guard**: Only one mutation continuation chain can be active at a time (thread-local `MUTATION_IN_FLIGHT: Cell<bool>`). Concurrent `mutate()` calls wait or reject while a continuation is in progress.

**GC policy**: Checkpoints older than 5 minutes are garbage-collected on next `mutate()` call. Orphaned checkpoints (from traps) are detected in `post_upgrade` and cleaned up.

**Which mutations benefit**:
- `DELETE ... WHERE <filter>` with large MATCH scans — the scan phase is suspendable.
- `SET` / `REMOVE` across many matched vertices — the write loop is suspendable.
- Large `CREATE` with many elements — the create loop is suspendable.
- `MERGE` combines match + create, both phases are suspendable.

**Future enhancement**: If self-call latency proves problematic, the design is forward-compatible with client-driven polling by exposing `resume_mutation` as a public endpoint. No architectural change needed.

## Consequences

- `mutate(gql)` becomes an `async` function internally (for self-call await).
- Graph canister must handle the case where a self-call fails (trap recovery).
- Stable memory usage increases by checkpoint size (bounded: ~16KB per checkpoint for typical mutations).
- The `MutationResult` type may gain a `rounds: u32` field for observability.
