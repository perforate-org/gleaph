# 0060. Instruction-budget-driven durable bulk load and shared budget cutoff

Date: 2026-08-03
Status: proposed
Last revised: 2026-08-03
Anchor timestamp: 2026-08-03 13:54:52 UTC +0000

## Context

ADR 0057 introduced the durable bulk-load lifecycle: client-driven `Start`, sequential `Append`,
`Finalize`, and `Abort`; one logical graph and one pinned Graph shard per job; a chunk is a
shard-local atomic ordered insert; the job is a non-atomic committed prefix; durable receipts are
paged through `bulk_load_status`. The public chunk operation bound
`MAX_ATOMIC_INSERT_OPERATIONS = 1024` was introduced by the ADR 0057 implementation commit
(`dc2bf946`, 2026-08-03) without a documented derivation: it is not derived from the 2 MiB payload
bound (a 1024-vertex receipt is ~8 KiB because `ENCODED_VERTEX_ID_BYTES = 8`) and no ADR, benchmark,
or test justifies the value.

ADR 0042 established instruction-budget-driven execution: the 40B update-call ceiling
(`MAX_UPDATE_CALL_INSTRUCTIONS`), 5B headroom (`UPDATE_CALL_INSTRUCTION_HEADROOM`), and the 35B safe
budget (`MAX_DYNAMIC_UPDATE_INSTRUCTIONS`). The non-ordered plan-batch path
(`execute_plan_batch_internal`) executes operations one at a time, learns the measured per-operation
cost, cuts off with `should_cutoff_batch` before starting an operation that would risk the ceiling,
and returns a `next_index` cursor. Index posting, vector sync, and bulk-ingest finalize use the same
shape (`applied`, `next_index`, `instruction_budget_exhausted`).

The perf 0049 commit (`328bbe3e`, 2026-07-29) added
`GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION = 500_000_000` and
`ensure_ordered_batch_instruction_budget`, a static pre-dispatch estimate "for pre-dispatch batch
sizing when an endpoint has no resumable per-operation cursor" — applied to
`execute_ordered_vertex_batch`, `execute_ordered_edge_batch`, and `execute_ordered_mixed_batch`, the
atomic ordered-batch entrypoints used by both atomic insert and bulk-load chunks.

## Problem

1. **A fixed operation cap is the wrong mechanism for bulk load.** `atomic_insert` must complete
   atomically within one execution, so a bound is contractually necessary there. `bulk_load` is a
   durable committed-prefix job; only the atomic chunk unit must fit one execution, and the job
   continues across calls by design. Capping every chunk at a client-visible constant forces the
   client to guess a size instead of letting the runtime instruction budget decide the boundary.

2. **The static estimate over-reserves and is inconsistent with the working e2e.** The historical
   social-demo seeder processed ~3000 entries per chunk on the ADR 0042 dynamic path. The ordered
   path removes Property Index convergence from the initial-load critical path (ADR 0057), so the
   per-entry cost is lower, not higher. Yet the static estimate admits only
   `MAX_DYNAMIC_UPDATE_INSTRUCTIONS / GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION = 70` operations
   per ordered batch. Three inconsistent bounds currently coexist: Router chunk cap 1024 (ADR 0057),
   demo/e2e chunk size 256 (`seed_social_graph` `CHUNK_SIZE`), and Graph static admission 70.
   A PocketIC probe on 2026-08-03 confirmed the boundary empirically: a 70-operation vertex chunk is
   admitted, a 71-operation chunk and a 256-operation chunk are rejected with
   `ordered vertex batch exceeds the dynamic instruction budget estimate`, surfaced to the client as
   `RouterError::Internal`. The rejection message is not among the payload-bound rejection strings the
   social-demo loader adaptively splits on, so `seed_social_graph` (256-operation chunks) and the demo
   load are currently broken, and the opaque `Internal` error leaves the job recoverable-but-ambiguous
   (`AppendPending`) for the client.

3. **The cutoff decision logic is duplicated.** `should_cutoff_batch`, the static preflight, the
   per-operation cost learning, the wasm/host-gated `current_instruction_counter()`, and the
   `instruction_budget_exhausted` progress reporting are re-implemented in `graph` handlers, `router`
   GQL dispatch, and four `ic-stable-lara` maintenance loops, with the budget constants owned by
   `gleaph-graph-kernel`. There is no shared home analogous to `gleaph-message-sizing`, which
   centralizes the byte-budget policy.

## Existing architecture assessment

The resumable per-operation cursor pattern is already the standard execution model in this codebase:

- `execute_plan_batch_internal` cuts off with measured cost (`max_op_instr_so_far`, 50M fallback),
  a drain reserve (`BATCH_DRAIN_BUDGET_ESTIMATE`), and a bookkeeping headroom
  (`GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM`), returning `next_index`;
- `IndexPostingBatchProgress`, `VectorSyncBatchProgress`, and `BulkIngestFinalizeResult` return
  `applied` / `next_index` / `instruction_budget_exhausted`;
- LARA maintenance budgets carry `max_instructions`, `reserve_instructions`, `checkpoint_every`, and
  `max_work_items`, with the same `used + reserve >= max` cutoff predicate.

`GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION` is explicitly documented as a stopgap: "Conservative
cost estimate used only for pre-dispatch batch sizing when an endpoint has no resumable per-operation
cursor. Measured operation cost remains authoritative for continuation paths." The ordered-batch
path is the last endpoint without a resumable cursor.

`gleaph-message-sizing` centralizes byte-budget fitting (`SizingPolicy`, `adaptive_fitting_prefix`,
the 2 MiB ceiling constant) with no Candid or canister-type dependency because the future `gleaph
build` CLI consumes it on the host. Instruction-budget logic has no host consumer: Router, Graph,
index, vector, and LARA all run inside canisters, so an `ic-cdk` dependency is acceptable. The
dependency direction requires a standalone crate below both `gleaph-graph-kernel` and
`ic-stable-lara` (graph-kernel already depends on lara, and lara currently reads the instruction
counter itself), mirroring where `gleaph-message-sizing` sits.

## Decision

### 1. `gleaph-instruction-budget`: shared instruction-budget policy and cutoff

A new standalone crate mirrors the role of `gleaph-message-sizing` for the instruction dimension. It
is consumed by Router, Graph, index/vector, and `ic-stable-lara`; it depends on nothing in the
canister stack except `ic-cdk`, which is an optional dependency behind a default-on feature so that
`gleaph-graph-kernel` can reference the constants and pure logic with `default-features = false` and
keep `ic-cdk` out of its host build (preserving its current `ic-cdk`-optional design).

The crate must be standalone and sit below both `gleaph-graph-kernel` and `ic-stable-lara` because
graph-kernel already depends on lara; if the budget logic lived in graph-kernel, lara could not use
it without a dependency cycle. The same reason forces the budget constants to move into the new
crate: lara reads them at call sites today only because it defines its own local cutoff predicate,
and once the predicate is shared the constants must be reachable from both sides of the
`graph-kernel -> lara` edge. `gleaph-graph-kernel` re-exports the constants so existing callers and
the ADR 0042 ownership statement migrate without a churn pass.

The scope is bounded by the demonstrated call sites: the Graph plan-batch cutoff, the new
`Resumable` ordered-batch cutoff, the `Atomic` admission proof, and the LARA maintenance checks. All
four consume the same predicate shape (`used + next estimate + reserves >= ceiling`), so
`should_cutoff`, `preflight_operation_count`, and `OpCostTracker` are not speculative: each is
exercised by at least two of these paths. The Candid progress wire types (`IndexPostingBatchProgress`,
`VectorSyncBatchProgress`, and the LARA reports) stay in their owning crates; the new crate
centralizes the decision logic and constants only, never the wire envelopes.

The crate provides:

- the budget constants moved from `gleaph-graph-kernel`
  (`MAX_UPDATE_CALL_INSTRUCTIONS`, `UPDATE_CALL_INSTRUCTION_HEADROOM`,
  `MAX_DYNAMIC_UPDATE_INSTRUCTIONS`, `GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION`,
  `GRAPH_BATCH_FINAL_BOOKKEEPING_INSTRUCTION_HEADROOM`, `ROUTER_BATCH_WORK_INSTRUCTION_HEADROOM`,
  `MAX_TIMER_MAINTENANCE_INSTRUCTIONS`, `TIMER_MAINTENANCE_INSTRUCTION_HEADROOM`) and their
  compile-time assertions; `gleaph-graph-kernel` re-exports them for compatibility;
- `should_cutoff(ceiling, used, next_op_estimate, response_reserve, drain_reserve) -> bool`, the
  generalized pure cutoff predicate behind `should_cutoff_batch` and the LARA maintenance checks;
- `preflight_operation_count(count, estimate_per_operation, safe_budget) -> Result<(), BudgetError>`,
  the generalized static admission behind `ensure_ordered_batch_instruction_budget`;
- `OpCostTracker { fallback_estimate, max_seen }` with `next_op_estimate()` and `observe(cost)` for
  measured per-operation cost learning;
- `instruction_counter()`, a single wasm/host-gated helper (wasm:
  `ic_cdk::api::instruction_counter()`; host: 0) replacing the five local copies.

### 2. Resumable per-operation cursor for ordered batches

The Graph ordered-batch entrypoints gain an explicit execution mode. The mode is a transport
property, not request content: it is carried on the args envelope (for example
`OrderedVertexBatchGraphArgsV1`) outside the fingerprinted request, so the same chunk content has
the same fingerprint regardless of mode and the chunk-fingerprint replay identity from ADR 0057 is
unchanged.

- `Atomic` (used by `atomic_insert`): the entire request commits atomically or fails. Admission
  proves the request fits the safe budget using the measured per-operation estimate (`OpCostTracker`,
  seeded by `GRAPH_BATCH_INSTRUCTION_ESTIMATE_PER_OPERATION`). `MAX_ATOMIC_INSERT_OPERATIONS` remains
  the protocol ceiling, but it is a hard cap, not the effective cap: the budget-bound measured
  estimate decides the practical admission (with the 500M seed the first requests are limited to ~70
  operations until `OpCostTracker` learns). The 5B headroom below the 40B trap ceiling absorbs
  estimate drift; a trap is not a silent loss because the Router parent record is durable before
  dispatch and the failed child is visible through the existing mutation status, so recovery is
  status-driven retry, never a fresh create.
- `Resumable` (used by bulk-load chunk execution): operations execute one at a time with
  `should_cutoff` checked between operations (measured cost + drain reserve + bookkeeping headroom,
  as in the plan-batch path). On cutoff, the completed prefix commits atomically as one ordered
  journal entry and the response returns the committed count. The chunk is the committed prefix; the
  receipt covers the prefix, preserving shard-local atomicity and the ordinal
  `allocated_vertex_ids` mapping.

The static `ensure_ordered_batch_instruction_budget` is removed from the resumable mode; admission
there is the payload bound plus the runtime budget. Because the cutoff fires with headroom, the call
returns normally instead of trapping at the ceiling, and each call commits at least one operation
unless that operation itself is fundamentally invalid (a deterministic rejection, not a retry). The
progress guarantee holds because the cutoff reads Graph's per-message instruction counter, which is
fresh for every Router-to-Graph dispatch; the first operation of a call always starts against a full
budget.

### 3. `bulk_load` Append becomes a batch plus a cursor

`BulkLoadCommand::Append` keeps the graph/key identity and `chunk_index`, but the payload is a
candidate batch that may span more than one atomic chunk. **One Append call commits at most one
budget-bounded atomic chunk** — the `Resumable`-mode committed prefix of the candidate batch — and
returns exactly one receipt plus `next_offset` (operations of this batch committed, i.e. the next
position the client resumes from). Multiple chunks are never produced by one call; a larger batch
simply means the client loops again with `batch[next_offset..]`. This keeps the response shape a
single receipt (`BulkLoadResponse::Appended { chunk_index, next_offset, receipt }`) and matches the
ADR 0042 wave model. The client loop is:

```text
while offset < len:
    append(batch[offset..]) -> next_offset
    offset = next_offset
```

- `MAX_ATOMIC_INSERT_OPERATIONS` is retained for `atomic_insert` only; `bulk_load` has no fixed
  operation cap.
- The public graph identity becomes `graph_name: Option<String>` (renamed from
  `logical_graph_name`): `None` resolves the caller's default (HOME) graph through
  `graph_context::resolve_graph_id_or_default`, `Some(name)` resolves the named graph with the
  caller's tenancy authorization. The same optional-name treatment applies to the other L1
  data-plane entry points that take a graph name (`bulk_load_status`, `mutation_status`,
  `atomic_insert_status`, `atomic_insert` including its Router-internal ordered-batch chain,
  `list_prepared`, and `vector_search`, which keeps its non-authorized named resolution); the
  admin control surface (`register_graph`, `unregister_graph`, `ensure_*`, `index_*`,
  `admin_register_vector_index`, `list_vector_indexes`) stays explicit because provisioning
  targets a named graph and no CLI/SDK consumer needs the default there yet.
- Durable resume is unchanged: `bulk_load_status` receipts let the client reconstruct committed
  boundaries (the receipt-count partitioning pattern already proven by the social-demo loader), and
  an ambiguous response resolves via status plus exact replay of the unchanged candidate batch.
- `BulkLoadResponse::Appended` gains `next_offset` (breaking Candid/SDK/CDK change; the project is
  pre-release).

### 4. `gleaph load` CLI as a cursor loop

The CLI is the first consumer of the batch-plus-cursor protocol. Its driver contains no chunk-sizing
logic: each request is fitted to the 2 MiB ingress payload bound via `gleaph-message-sizing`, chunk
boundaries are Router-owned, and the driver loops on `next_offset`. The CLI surface is summarized
here and the full user-facing specification (artifact schema, flags, exit codes) is written when the
command is implemented:

- artifact: versioned YAML/JSON single-file (`format_version: 1`) with `vertices` (unique
  `source_id`, labels, canonical GQL value properties) and `edges` (endpoints by source/target,
  label, directed), plus NDJSON as the large-data form (`vertices.jsonl` + `edges.jsonl`, one row
  per vertex/edge with the same row schema). Single-file NDJSON is designated with
  `--vertices FILE` / `--edges FILE` (mutually exclusive with positional ARTIFACT); `--edges`-only
  loads require property-based endpoints and are rejected until that capability lands;
- remote connection: `--canister`, `-n/--network`, `--identity`, `--fetch-root-key` (same convention
  as `gleaph migration`); `--graph` (omitted → the caller's default graph), `-k/--key` (default
  `initial-load-v1`); `--format` is optional and inferred from the file extension when omitted;
- lifecycle: skip on `Completed` (state-file digest must match), resume from `bulk_load_status`
  receipt boundaries, `--fresh` derives a fresh job key (`{key}.{nonce}`, since durable bulk-load
  keys are single-use after a terminal state), digest verification via an optional `--state-file`
  that also records the effective key for later resume/skip identity;
- exit codes: 0 complete/skip, 1 operator action required, 2 input validation, 3 remote/auth.

The CLI never hardcodes an operation cap; it references the shared wire constant only if it ever
exposes an `atomic_insert`-style bound.

**Planned extension — property-based edge endpoints (decision, not yet implemented).** For
incremental edge loads (`--edges`), endpoints reference existing vertices by property
(`{ label, property, value }`) instead of an in-artifact `source_id`. Resolution will be
**Router-side** (Option A): `BulkLoadEdgeV1` endpoints become an `Existing | ByProperty` enum and
the Router resolves `ByProperty` through the graph property index during Append. Two design
requirements are fixed: (1) a **pre-resolution pass** rejects the whole candidate chunk before any
operation executes when any endpoint is missing or non-unique, so failures never surface as a
partial commit mid-chunk; (2) resolution requires an existing, converged property index on the
`(label, property)` pair (the CLI reports the missing index as an operator action), and endpoint
property values are restricted to sortable (indexable) value types. Replay safety is preserved
because the durable child row stores the resolved request and `drive_bulk_child` replays it
without re-resolution. Resolution batches: endpoints are grouped by `(label, property)` and
resolved with a batched equality index request (one call per group when the value set fits the
inter-canister payload bound, split into the minimum number of calls otherwise, with duplicate
values deduplicated), so a typical one-property edge chunk resolves in a single index call. The
existing index API already sends multiple `IndexEqualSpec`s in one call, but only with
intersection (AND) semantics and no per-value result association; resolution needs a
union/batch-equality request (values of one property → per-value postings), which is a small
extension of that existing spec-vector machinery.

## Alternatives

### Raise `MAX_ATOMIC_INSERT_OPERATIONS` to a measured larger constant

Rejected. A fixed cap, however large, cannot adapt to data shape, property-size distribution, or
version drift between CLI and deployed canisters, and it keeps the client responsible for sizing.
The demonstration that the current 1024 (and the 70-op static admission) are arbitrary supports
removing the fixed mechanism for bulk load rather than re-deriving a new constant.

### Client-side dynamic sizing only (byte fit + operation cap)

Rejected. `gleaph-message-sizing` measures encoded bytes, which do not bound instruction cost; the
client cannot know the Graph execution budget. `message-sizing` remains only for fitting each request
to the ingress payload bound. The runtime instruction budget is the authoritative boundary and is
owned by the canister that executes the work.

### Keep the static estimate but tune the per-operation constant

Rejected. The estimate's own documentation states that measured operation cost is authoritative for
continuation paths. Tuning a worst-case constant preserves the over-reservation and leaves the
70/256/1024 inconsistency in place.

### Accept the static estimate and cap chunks at 70 operations (minimum change)

Rejected. This is the smallest possible fix for the confirmed break: lower the demo/e2e `CHUNK_SIZE`
and the loader default to 70 operations. It accepts the over-reservation (roughly a 40× regression
from the ~3000-operations-per-chunk the historical dynamic path sustained), keeps the sizing
responsibility on every client, and leaves the opaque `RouterError::Internal` admission failure mode
in place for any caller that does not know the cap.

### Split the client batch at the Router into Graph-atomic chunks (moderate change)

Rejected. The Router would size each chunk's Graph dispatch, but it does not know Graph's measured
execution cost; it could only reuse the static estimate, which returns to 70-operation chunks.
Measured-cost cutoff requires the Graph-side `Resumable` cursor, so a Router-only split cannot
achieve the dynamic boundary this decision needs.

### Extend `gleaph-message-sizing` with an instruction-budget module

Rejected. `gleaph-message-sizing` is consumed on the host by the future `gleaph build` CLI and must
stay free of `ic-cdk` and canister types; the instruction-budget logic is canister-only and needs a
feature-gated `ic-cdk` dependency. Keeping the two dimensions in separate crates preserves
`message-sizing`'s host contract, and the standalone position is forced anyway by the
`graph-kernel -> lara` dependency edge.

### Drive bulk load through `execute_plan_batch` (non-ordered)

Rejected. Bulk-load chunks require ordered semantics: the receipt returns `allocated_vertex_ids` in
operation ordinal order for the client's `source_id` mapping, and ADR 0049 `ORDER BY INSERTION` edge
insertion requires input-order preservation. Per-operation atomicity of the plan-batch path is
incompatible with the chunk as the atomic unit.

### Add a generic durable job scheduler

Rejected in ADR 0057 for the same reasons it is rejected here: it would duplicate the Router mutation
lifecycle, retry owner, and observability path. This decision extends the existing lifecycle and
ordered-batch substrate.

## Consequences

Positive:

- One execution mechanism across ordered and non-ordered batches; the static 500M estimate is retired
  from the resumable path and becomes only the `Atomic` seed.
- Bulk-load chunks reliably progress: no operator-tunable chunk size, no fixed cap on the durable job,
  and each call commits at least one operation unless the operation is fundamentally invalid.
- Removes the 70/256/1024 bound inconsistency and the confirmed e2e failure it implies.
- The cutoff predicate, cost learning, budget constants, and instruction counter are centralized once,
  deduplicating five `instruction_counter()` copies and the duplicated maintenance checks.

Trade-offs:

- Breaking Candid/SDK change to `BulkLoadResponse::Appended` (pre-release, accepted without a
  compatibility alias, consistent with ADR 0056/0057 policy).
- Graph ordered-batch execution must support prefix commit in `Resumable` mode; journal and
  retirement semantics must cover the committed prefix only.
- The 2 MiB payload bound still limits a single request; a batch larger than the bound is split by
  the client loop, and the payload-fitting stays in `gleaph-message-sizing`.
- `atomic_insert` keeps its one-execution contract and therefore keeps an admission proof; the
  `Atomic` mode preflight is conservative by design.

## Implemented migration and activation

Decision 1 is implemented as of 2026-08-03: `gleaph-instruction-budget` is a workspace crate owning the
budget constants, `should_cutoff`, `preflight_operation_count` / `max_operation_count`,
`OpCostTracker`, and the wasm/host instruction counters; `gleaph-graph-kernel` re-exports the
constants; and the Graph plan-batch cutoff, the ordered-batch static admission, the Router dispatch
operation-bound derivation, and the four LARA maintenance loops consume the shared helpers.
Decision 2 is implemented as of 2026-08-03: the ordered-batch args envelopes carry an
`OrderedBatchExecutionModeV1` (`Atomic` / `Resumable`) transport field outside the fingerprinted
request; Graph's vertex and edge handlers execute the budget-fitting prefix item-by-item
(`resumable_prefix_len` with `should_cutoff` + `OpCostTracker`) and commit the prefix as one atomic
journal entry; resumable replay is addressed by mutation id plus the stable request fingerprint;
mixed batches reject `Resumable`; the committed prefix is returned through the receipt counts. The
Graph canister Candid (`gleaph_graph.did`) is updated for the new wire field. Decision 3 is
implemented as of 2026-08-03: `BulkLoadResponse::Appended` gains `next_offset` (operations of the
candidate batch committed as this chunk); the Router dispatches bulk-load chunks in `Resumable`
mode and derives `next_offset` from the receipt's committed operation count; `BulkLoadChunkV1`
validation no longer caps chunks at `MAX_ATOMIC_INSERT_OPERATIONS` (the runtime budget and the
payload / durable-row bounds govern the candidate size); the Router and Graph Candid bindings are
regenerated. Also as of 2026-08-03, the L1 data-plane wire changed `logical_graph_name: String` to
`graph_name: Option<String>`: `bulk_load` (all four variants), `bulk_load_status`, `mutation_status`,
`atomic_insert_status`, `atomic_insert` (including the Router-internal ordered-batch chain),
`list_prepared`, and `vector_search` resolve `None` to the caller's default (HOME) graph via
`graph_context::resolve_graph_id_or_default`, while `Some(name)` keeps the prior resolution; the
admin control surface keeps explicit graph names. Decision 4 is implemented as of 2026-08-03:
the CLI gains a `load` subcommand (`gleaph load`) that drives the batch-plus-cursor protocol to
`Completed`. The artifact is a versioned YAML/JSON single file (`format_version: 1`, `vertices` +
`edges`) or two NDJSON files (`vertices.jsonl` + `edges.jsonl`) with the same row schema; `--format`
is optional and inferred from the file extension when omitted; `--graph` is optional (omitted → the
caller's default graph via the `Option<String>` wire); the driver fits each request to the
inter-canister payload bound with `gleaph-message-sizing` and loops on `next_offset`; `--fresh`
derives a fresh job key (durable bulk-load keys are single-use); an optional `--state-file` records
the effective key and artifact digest for skip-on-Completed verification; exit codes are 0
complete/skip, 1 operator action, 2 input validation, 3 remote/auth. The CLI depends on
`gleaph-router` for the wire types; a shared bulk-load wire crate remains the longer-term
consolidation alongside the Router/SDK duplication. No further wire change is planned, and there is
no development stable-layout change: no
Router or Graph stable region is added, and the existing receipt map, coordinator lifecycle, and
client-key identity remain the durable substrate. The static-admission inconsistency is confirmed
by the 2026-08-03 PocketIC probe (70 admitted, 71 and 256 rejected) and is eliminated by the
`Resumable` dispatch (256-operation chunks are admitted and commit within the runtime budget).

## Required tests

- `gleaph-instruction-budget` unit tests with synthetic counters (mirroring `message-sizing`'s
  `linear_measure` style): `should_cutoff` boundary behavior, `preflight_operation_count` derivation
  (port `ordered_batch_instruction_admission_is_derived_from_shared_budget`), `OpCostTracker`
  learning, and the moved constant assertions.
- Graph ordered-batch `Resumable` mode: prefix commit at cutoff, receipt covering the committed
  prefix, progress guarantee (at least one operation per call), and deterministic rejection of a
  fundamentally invalid operation.
- Graph ordered-batch `Atomic` mode: whole-request atomicity, no mid-request cutoff, and preflight
  proof against the safe budget.
- Router bulk-load Append cursor: batch-to-chunk partitioning, `next_offset` advancement, lost
  response resume via `bulk_load_status` receipt boundaries, and exact replay of the unchanged
  candidate batch.
- PocketIC e2e: a large batch that spans multiple budget-bounded chunks; resume after interruption;
  confirmation that the current 256-operation `seed_social_graph` path is no longer rejected.
- Benchmarks: canbench the `Resumable` op-by-op loop overhead against the current whole-batch
  execution at the same operation count; verify the `Atomic`-mode preflight effect on 1024-operation
  requests (`bench_atomic_insert_max_receipt` measures receipt encoding only and is unaffected, but
  admission of a 1024-operation request changes with the measured estimate).
- `gleaph build` CLI: cursor-loop driver against a fake transport (lost responses, resume, skip,
  `--fresh`), artifact validation, and an e2e run through `bulk_load_as_admin`.

## Related decisions

- [ADR 0057](0057-router-operation-api-and-durable-bulk-load.md): durable bulk-load lifecycle; this
  decision amends its chunk semantics and Append response.
- [ADR 0042](0042-router-dynamic-instruction-budget-batching.md): instruction-budget execution model;
  the 40B/5B/35B constant ownership moves to `gleaph-instruction-budget` with
  `gleaph-graph-kernel` re-export.
- [ADR 0049](0049-input-order-preserving-batch-graph-mutations.md): ordered atomic-insert contract
  and `ORDER BY INSERTION` semantics preserved by the `Resumable` prefix-commit design.
- [ADR 0058](0058-versioned-additive-schema-migrations.md): unchanged; `gleaph build` composes with
  `gleaph migration apply` but owns only the data load.
