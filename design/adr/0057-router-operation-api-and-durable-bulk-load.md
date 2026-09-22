# 0057. Router operation API and durable bulk-load lifecycle

Date: 2026-08-02
Status: implemented
Last revised: 2026-08-09
Anchor timestamp: 2026-08-09 02:46:41 UTC +0000

## Context

ADR 0056 reduced and layered the Router surface, but its initial L1 names mixed execution mechanism,
transport aggregation, and client intent. In particular, the former typed insert endpoint was a
bounded shard-local atomic mutation while the former cursor-list endpoint was a caller-resumable
list of independent mutations. Their shared `batch` terminology did not expose incompatible
atomicity, identity, replay, and failure contracts.

The former social-demo seed flow waited for derived-index convergence before it could use `MATCH`
to recover inserted vertices. The durable loader now returns committed IDs so later edge inserts
address canonical vertices directly without Property Index lookup.

The project is pre-release. Public Candid, SDK, and persisted development state may change without
compatibility wrappers or dual wire paths; activation must explicitly reset incompatible development
stable state.

## Problem

The Router L1 surface is harder to understand than the supported client workflows:

- read-only GQL, mutating GQL, prepared reads, and prepared mutations are not named symmetrically;
- the atomic typed insert guarantee was hidden behind an ambiguous legacy name;
- status lookup does not expose the mutation family it accepts;
- the existing cursor list is not a durable initial-load job because the caller must retain the
  complete input and there is no job identity, append/finalize/abort lifecycle, or receipt recovery;
- an atomic insert cannot currently return the generated IDs required for direct subsequent edges;
- exposing internal Graph/index batch transports as client concepts would leak ownership and invalid
  combinations rather than simplify the API.

## Existing architecture assessment

Router already owns external entrypoints, authentication, graph/name resolution, physical placement,
public ID encoding, mutation identity, cross-canister orchestration, and public result aggregation.
Graph owns vertex allocation, canonical graph writes, local vertex existence checks, and shard-local
mutation journals. Property and Vector indexes own derived projections and their watermarks.

The existing ordered Graph mutation and Router mutation record are the atomic-insert replay
substrate. A caller-owned cursor and independent item records are not a durable bulk-load job
manifest. The implemented bulk-load design extends the existing mutation lifecycle and adds only
the storage that paged receipts require; it does not introduce a second scheduler or recovery owner.

## Decision

### 1. Operation-execution subset of the client L1 surface

The operation-execution subset of the client-facing Router data plane uses these methods. Existing
`prepare`, `drop_prepared`, `list_prepared`, and `vector_search` remain unchanged and are part of the
complete L1 surface even though they are not operation-execution renames.

| Method                 | IC call | Contract                                                                                                 |
| ---------------------- | ------- | -------------------------------------------------------------------------------------------------------- |
| `gql_query`            | query   | Read-only ad-hoc GQL; rejects a mutating plan.                                                           |
| `gql_mutate`           | update  | Idempotent ad-hoc GQL mutation.                                                                          |
| `prepared_query`       | query   | Read-only registered operation; manifest kind must match.                                                |
| `prepared_mutate`      | update  | Idempotent registered mutation; manifest kind must match.                                                |
| `atomic_insert`        | update  | Bounded typed vertex/edge insertion whose accepted canonical request is shard-local atomic.              |
| `bulk_load`            | update  | Durable client-driven initial-load lifecycle; chunks commit as a prefix and the whole job is not atomic. |
| `mutation_status`      | query   | GQL/prepared mutation lifecycle only.                                                                    |
| `atomic_insert_status` | query   | Atomic-insert lifecycle and exact durable receipt only.                                                  |
| `bulk_load_status`     | query   | Bulk job state and paged committed-chunk receipts only.                                                  |

The non-bulk wire signatures are fixed as follows; `GqlQueryResult`, `ReadMode`, prepared sort, and
parameter encodings retain their existing definitions:

```text
gql_query(query, params, read_mode) -> Result<GqlQueryResult, RouterError> query
gql_mutate(query, params, client_mutation_key) -> Result<GqlQueryResult, RouterError> update
prepared_query(name, params, sort, read_mode) -> Result<GqlQueryResult, RouterError> query
prepared_mutate(name, params, client_mutation_key) -> Result<GqlQueryResult, RouterError> update
atomic_insert(AtomicInsertRequest) -> Result<AtomicInsertResponse, RouterError> update
mutation_status(logical_graph_name, client_mutation_key) -> Result<MutationStatus, RouterError> query
atomic_insert_status(logical_graph_name, client_mutation_key) -> Result<AtomicInsertResponse, RouterError> query
```

`AtomicInsertRequest`, `AtomicInsertRequestV1`, `AtomicInsertOperationV1`,
`AtomicInsertVertexV1`, `AtomicInsertEdgeV1`, `AtomicInsertEndpointV1`, and
`AtomicInsertPropertyV1` replace the corresponding public `Batch*` types one-for-one.
`AtomicInsertResponse` contains `status: MutationStatus` and
`receipt: Option<AtomicInsertReceiptV1>`; the receipt is absent only before canonical commit.
`AtomicInsertReceiptV1` contains `logical_operation_count`, `logical_vertex_count`,
`logical_edge_count`, and `allocated_vertex_ids: Vec<Vec<u8>>` in vertex-operation ordinal order.
`MAX_ATOMIC_INSERT_OPERATIONS` replaces `MAX_BATCH_OPERATIONS` with the same initial bound.

The public `RouterError` wire enum remains shared by non-bulk and bulk methods. The durable bulk-load
slice adds this exact retryable variant, which generated Candid, SDK, and CDK bindings must represent
exhaustively; non-bulk methods do not emit it:

```text
Busy { operation: String }
```

`operation` is the stable identifier of the persisted operation that must settle before the requested
command can transition. Bulk-load v1 emits exactly `bulk_load.append` or `bulk_load.abort`. Retrying
the same command is safe after that operation advances; `Busy` is not a conflict or terminal failure.

The Router validates method kind at its owning boundary. A query method rejects a mutation plan; a
mutation method rejects a read-only plan; prepared methods reject a manifest-kind mismatch. The SDK
does not infer query versus update from raw GQL because procedure side effects and durable
idempotency identity are Router-owned contracts.

The old public names and cursor-list endpoint are removed without aliases. Durable initial loading
has one public owner: `bulk_load` and its status query.

### 2. Atomic insert and generated IDs

`atomic_insert` replaces the legacy typed-insert name without weakening ADR 0049:

- the request is bounded and typed;
- every accepted request maps to one Graph shard and one Graph request;
- the canonical Graph message is all-or-nothing;
- vertex-only, edge-only, and mixed forms share one public contract;
- same identity and same fingerprint returns the exact receipt; conflicting reuse rejects before
  dispatch;
- derived-index convergence remains a later durable lifecycle phase and is not required to address a
  canonical vertex by encoded ID.

`AtomicInsertReceiptV1` contains encoded vertex IDs in vertex-operation ordinal order. Edge-only
receipts contain an empty list. Graph records a distinct bounded `allocated_vertex_ids` appendix in
its existing ordered journal before it sorts or deduplicates projection telemetry. The existing
Router ordered record persists that Graph receipt together with the target shard. Router derives the
public encoded IDs deterministically from the persisted ordered local IDs, target shard, and the
graph's immutable ID-encoding key when serving the initial response or status; it does not persist a
second encoded-ID list as another source of truth.

`atomic_insert_status` returns the same receipt after response loss. Before dispatch, Router computes
the exact worst-case public Candid receipt size from the fixed eight-byte encoded-ID width and vertex
count. Before vertex allocation, Graph computes the exact journal appendix size from the fixed
four-byte local-ID width and vertex count. Both checks, plus the existing request/stable-record
bounds, reject before the first canonical mutation; post-allocation encoding must not introduce a
recoverable oversize error.

`RouterMutationRecordV1` gains the sole authoritative `terminal_at_ns: Option<u64>`; payloads and
coordinator variants do not duplicate it. The Router sets it exactly once in the same message
segment as an irreversible terminal transition and uses it, rather than `created_at_ns`, for terminal
expiry and retry-expired decisions. An irreversible terminal transition is a completed payload or
`terminal_failure.is_some()` for ordinary mutations, and `Completed`, `Aborted`, or non-retryable
`Failed` for bulk jobs. A retryable `MutationLifecyclePhase::Failed` is not terminal and must not set
the timestamp. Non-terminal records require `None` and remain ineligible for ordinary GC. Terminal
Router receipts are guaranteed recoverable for seven days
measured from `terminal_at_ns`; after that boundary the record is GC-eligible and status may return
the ordinary not-found result after physical deletion. No permanent expiry tombstone is added.
This terminal anchor replaces `created_at_ns` for every Router mutation family so one shared GC and
retry-expiry policy remains the source of truth; ADR 0025 is amended accordingly and tests cover
scalar, atomic-insert, durable bulk-load, and ordered records. Idempotency and receipt-recovery guarantees end with
retention. Retired Graph evidence retains the existing nine-day margin under ADR 0027, whose
retirement anchor remains unchanged. Expiry removes receipt recovery, not the canonical vertices or
validity of IDs already persisted by the client.

Compacted ordered edge, vertex, and mixed atomic-insert completions retain their fixed 32-byte
Graph-request fingerprint throughout that terminal-retention window. They drop the full Graph
request and target after retirement acknowledgement; the retained fingerprint is immutable terminal
identity, not a digest recomputed from the discarded envelope.

Graph journal codec v1 allocates appendix flag `0x10` to `allocated_vertex_ids`, encoded as a checked
`u32` count followed by input-ordinal `u32` local IDs. Edge-only entries require the appendix to be
absent; vertex-only and mixed entries require it to be present with the exact logical vertex count.
The decoder updates its known-flag mask and still requires exact input exhaustion. Because backward
compatibility is intentionally unsupported, old vertex/mixed v1 bytes without this appendix are
rejected and activation requires a development stable-state wipe; absence is not decoded as an
empty successful receipt.

### 3. Durable bulk load

Bulk-load v1 is deliberately narrow:

- one logical graph and one Router-pinned live Graph shard per job;
- client-driven `Start`, sequential `Append`, `Finalize`, and `Abort` commands;
- self-contained vertex chunks or edge chunks; edge endpoints are existing encoded IDs;
- no cross-call symbolic vertex map—the client obtains IDs from a committed vertex chunk and passes
  them into later edge chunks;
- at most the atomic-insert operation bound per chunk, plus exact encoded request/response admission;
- one chunk is a shard-local atomic ordered insert; the job is a non-atomic committed prefix;
- same job key/chunk index/fingerprint replays exactly, while conflicting or out-of-order append
  rejects before dispatch;
- `Abort` stops future append/finalize work and never rolls back committed chunks;
- `Finalize` completes only after every accepted chunk has completed required projection and Graph
  retirement work;
- canonical work resumes only through an explicit client command. Background recovery may reconcile
  or advance derived/retirement state but does not autonomously re-dispatch canonical DML.

Every bulk command and status query re-presents the original logical graph and client bulk key. The
public job identity is exactly `(caller, logical_graph_id, client_bulk_key)`, represented by the
existing `ClientMutationKey`. A caller may therefore reuse the same textual `client_bulk_key` on a
different logical graph; it creates or addresses an independent job. No public reverse lookup by
internal mutation ID is required:

```text
bulk_load(BulkLoadCommand) -> Result<BulkLoadResponse, RouterError> update
bulk_load_status(logical_graph_name, client_bulk_key, receipt_cursor, max_receipts)
  -> Result<BulkLoadStatusPage, RouterError> query

BulkLoadCommand =
  Start { logical_graph_name: String, client_bulk_key: String }
  | Append {
      logical_graph_name: String,
      client_bulk_key: String,
      chunk_index: u32,
      chunk: BulkLoadChunkV1,
    }
  | Finalize { logical_graph_name: String, client_bulk_key: String }
  | Abort { logical_graph_name: String, client_bulk_key: String }

BulkLoadChunkV1 = Vertices(Vec<AtomicInsertVertexV1>)
  | Edges(Vec<BulkLoadEdgeV1>)

BulkLoadEdgeV1 = {
  source: Vec<u8>,
  target: Vec<u8>,
  directed: bool,
  edge_label_name: Option<String>,
  inline_property: Option<Vec<u8>>,
  initial_edge_properties: Vec<AtomicInsertPropertyV1>,
}
```

Bulk-load update chunks (`BulkLoadChunkV1::Updates(Vec<BulkLoadUpdateV1>)`, CLI
`gleaph load --mode update --updates FILE`) extend the same job with vertex property updates
instead of inserts. A row carries one `{ vertex_label, property_name, match_value }` match key,
absolute `set_properties`, and `remove_properties`; the Router resolves every match key through the
converged property index before admission (missing, non-unique, non-comparable, or unknown
catalog/mutation-name rows reject the whole candidate batch), then applies one linear
`MATCH … [SET …] [REMOVE …] RETURN …` statement per row through the existing GQL authorization
and per-row mutation journal. The saved vertex ID, not the mutable match property, supplies the
execution input.

Update-lane contracts that differ from insert chunks:

- the generated statement executes against the resolved `graph_id` of the job, not the caller's
  HOME/session graph re-resolved at GQL ingress, so a bulk-load update can never write another
  logical graph;
- `resolved_update_vertex_ids` pins every authored row at initial admission. Each dispatch and
  retry passes that ID through `SeedBindingsWire.mutation_target` in the existing scalar journal
  envelope. No property lookup or match-value predicate reselects an admitted target. Thus Alice
  renamed to Bob in one row cannot broaden the following row from the original Bob to both
  vertices. Target placement participates in the scalar request fingerprint, distinct from
  ordinary unbound GQL. The retained RETURN property preserves match-property READ authorization
  without filtering by its changing value;
- fixed-vertex policy lowering preserves the labeled NodeScan and every applicable property
  predicate as a residual, even when an equality property has an index. Ordinary GQL may still
  replace its scan with that index. A real non-tenant conditional grant therefore filters the
  saved vertex rather than choosing a new candidate set. EXISTS-chain policies on the target
  label remain unsupported in this lane: Router returns
  `NotImplemented("bulk vertex updates with EXISTS policies are not supported")` before scalar
  reservation or Graph dispatch. It does not drop the policy or relax Graph's plan guard;
- one **row** is the atomic unit and one chunk is the client's candidate batch. Each row that
  returns successfully advances `updated_row_count` on the pending child before the next row
  dispatches, so the durable count is always a committed prefix of the authored batch;
- the public projection reports an update chunk only once it is `Completed`. An admitted-but-
  unfinished chunk is invisible, exactly like a pending insert child (whose `public_receipt` is
  `None`), so a resuming client re-sends the whole in-flight chunk at `next_chunk_index` from the
  boundary after the last completed chunk rather than skipping its prefix. For the retry to be
  accepted the payload must be byte-identical (the same chunk-index fingerprint rule as inserts),
  which is why the CLI's update-lane chunk formation is deliberately hint-free and therefore a
  pure function of the row stream;
- the resume boundary is the **durable per-row journal**, not the recorded count alone. Each
  update row key is itself an idempotent ADR 0029 `client_mutation_key`, so the Router already
  owns a per-row saga record and, while a row is in flight, the Graph owns a mutation-journal
  entry for the same mutation id. `Append` classifies rows from the recorded prefix upward:
  a completed exact-target outcome with count **1** advances the prefix without re-resolution
  or re-dispatch, whether confirmed by the Router or recovered from Graph. Count **0** is a
  terminal write-free outcome, not a committed bulk row. An absent or released-routing record
  is also write-free and may be dispatched while permission remains open; unreadable journals
  report retryable `Busy`. Both the prefix and the uncommitted suffix retain their original IDs
  when their own or another row's match property changes. Authored ordinals and payload remain
  unchanged;
- each scalar row reservation atomically stores its owning `(bulk_job_mutation_id, chunk_index)`
  in `PlanExecution.bulk_load_chunk`, alongside request identity. Compaction retains that link.
  While the referenced update child is unfinished, both ordinary terminal GC and client-key
  retry expiry preserve the row, including completed zero and terminal failures. Thus an old
  zero receipt cannot disappear and become a new write after target eligibility changes.
  Retry cannot change or remove the owner; a fresh owned reservation requires a pending child.
  Child completion releases the pin: completed Append replay uses the compact chunk receipt,
  so scalar rows may then expire normally without waiting for parent completion. This changes
  Router region 7's persisted identity and requires fresh Router state or reinstall; no migration
  or additional journal/map is introduced;
- Graph's exact-target lane is restricted to one labeled vertex, property SET/REMOVE, residual
  predicates before mutation, and an optional terminal projection. It rechecks the original
  NodeScan label and policy predicates, and checks liveness immediately before the no-RPC
  canonical segment. Missing/tombstoned/filtered targets yield a completed scalar journal with
  count 0; an empty seed never falls back to a label scan. Router returns
  `NotFound("bulk-load update target is no longer eligible")` for this zero-effect result.
  The same journal replays zero rather than writing a replacement vertex or becoming eligible
  again. Abort can discard that proven-write-free row, including after response loss. An eligible
  target counts as 1 even for an unchanged SET or a registered-but-unset REMOVE. Ordinary GQL
  seed and zero-match semantics are unchanged;
- the recorded prefix is **monotonic and bounded** at its owning write boundary: a progress
  notification above the admitted row count is rejected, and a duplicate or delayed smaller
  notification is an idempotent no-op, so a re-entrant or retried callback can never rewind the
  prefix or claim rows the chunk was not admitted with;
- `Updated { next_offset, updated_row_count }` reports the completed row count, including the
  prefix a retry did not re-dispatch. It equals the authored batch size on normal completion, or
  the settled prefix length when Abort abandons an unwritten suffix;
- a `Completed` update chunk replays from its stored receipt: the Router returns the recorded
  `Updated { … }` without re-resolving the chunk or re-applying rows, so later graph changes cannot
  turn a completed append into an error or a second mutation;
- **dispatch permission is an explicit durable grant, not a side effect of a retry.**
  `admit_bulk_load_update_child` is that grant: it returns dispatch permission only while the job
  is still working on that exact chunk (`AppendPending` for full permission, `AbortPending` for the
  settle-only path), and a finished chunk returns "replay the stored receipt" instead — never
  permission — so a caller that was suspended before admission can never start a row under a
  terminal job. Before granting a pending attempt, admission compares the caller's complete
  ordered target-ID list with the saved list. Concurrent first Append attempts can resolve mutable
  keys differently; a losing contender with different IDs receives
  `Conflict("bulk-load update targets differ from the admitted chunk")`, without changing the
  parent, child, or prefix. An identical-payload retry reads the winning saved IDs. This also
  protects an unstarted suffix row that has no scalar request fingerprint yet. Completed receipt
  replay does not require the compacted IDs. Every row is additionally re-checked against the durable gate
  (`bulk_load_update_dispatch_gate`) immediately before its dispatch, which is the boundary that
  stops an append from starting *new* rows after an `Abort` ingressed during a previous row's
  Graph call;
- the generated row uses the scalar `execute_plan_update` handler. That handler disables
  intermediate `Incomplete` journal writes and persists `Completed` before draining the derived
  index outbox. Its completed receipt records 0 or 1 eligible mutation inputs, independently of
  RETURN projection rows. Callback loss therefore leaves either no canonical outcome or a
  completed positive/zero-effect Graph receipt, not an intermediate multi-statement journal. The bulk classifier must not generalize
  this rule to arbitrary GQL bundles or count an `Incomplete` journal as a completed row;
- `Abort` first closes new-row dispatch permission (`AbortPending`), then classifies every row
  outside the recorded prefix. Only `NotWritten` is write-free. `Committed` extends the contiguous
  prefix; a committed row behind a gap fails closed. `Unknown` (unreadable journal) and
  `Unresolved` (no completed receipt for an admitted dispatch) both return
  `Busy { operation: "bulk_load.append" }` without publishing a terminal receipt or advancing the
  bulk prefix. An identical re-Append may settle an already-admitted dispatch under `AbortPending`,
  but cannot start a never-dispatched row. After settlement, another Abort closes at the true
  prefix. An absent or released-routing row needs no retry: Abort discards its unwritten suffix.
  The authored payload is not persisted, so Abort itself cannot re-drive a row;
- `Finalize` requires `committed_chunk_count == completed_chunk_count`, so a pending child must be
  completed (or the job aborted) before it can finalize. Update chunks advance both counters
  together at completion: unlike inserts there is no earlier "canonical commit" event to report,
  because each row is its own GQL mutation-journal commit.

**Failure and convergence:** a GQL error alone is not proof of a write-free row. The existing
mutation owner supplies that evidence. For example, SET on a uniqueness-constrained property is
rejected with `NotImplemented` by `reject_unsupported_constrained_writes`; its routing reservation
is released without dispatch. A batch with an earlier successful row and this deterministic later
failure can be aborted at that prefix without removing the constraint or changing the payload.
Transport failures differ: an admitted row with an unknown outcome remains pending until its
journal is readable and its outcome can be settled. Convergence requires available dependencies
and a successful retry/recovery; it has no unconditional time bound. Authorization failures and
other GQL admission checks retain their owning contracts.

Scalar row dispatch also checks the retained mutation ID, pinned Graph target and typed pending
bulk child at the actual Graph send, including later dynamic chunks. A captured count prevents
canonical redispatch and is retained before projection can suspend; AbortPending alone does not
cancel admitted unresolved work. [ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md#scalar-count-capture-and-actual-sends)
owns this Router boundary. An outcome lost before Router capture can still age out on Graph;
[ADR 0027](0027-graph-mutation-journal-retention.md) records the unresolved scalar lifetime gap.

There is no whole-chunk rollback or compensating-write contract. A terminal receipt counts
completed row statements, and an aborted chunk's unwritten suffix is abandoned, not re-driven.
The authored payload cannot be changed under the same chunk fingerprint; corrected input belongs
to a new job after closing the old one. The pending receipt layout, exact-target seed blobs,
request fingerprints, and scalar row-count interpretation require fresh Router/Graph state or
reinstall. No old-layout decoder or compatibility path is provided. Existing ADR 0029 journal
retention and recovery contracts still apply; no new journal or retention mechanism is introduced.

Edge-property SET through bulk load is **not** part of this contract: the proposed exactly-one
pre-read used a Graph composite query, which cannot be called from the Router's replicated update
path. Alternative edge targeting designs remain unimplemented. The prerequisite decision is recorded in
[implementation-gaps.md](../implementation-gaps.md) GAP-2026-09-11-004.

`BulkLoadEdgeV1.source` and `.target` are encoded existing vertex IDs only. `BulkLoadResponse` is an
exhaustive `Started | Appended { receipt } | Updated { next_offset, updated_row_count } |
FinalizeAccepted | AbortAccepted` variant, matching the public enum one-for-one.
`BulkLoadStatusPage` contains job state, next accepted chunk index, committed/completed counts,
retention deadline when terminal, a bounded ordered receipt page, and the next receipt cursor.
`max_receipts` is nonzero and capped by the Router before stable iteration or response construction.

The job/in-flight state extends the existing Router client-key mutation record at MemoryId 7 with
exhaustive `RouterMutationRequestIdentityV1::BulkLoadJob` and
`RouterMutationPayloadV1::BulkLoadCoordinator` variants. The coordinator contains the pinned
`(shard_id, graph_canister)` physical target, `next_chunk_index`, aggregate counts, optional active
child identity, and an optional `receipt_gc_cursor`. The internal job mutation ID is derived only
from the enclosing `RouterMutationRecordV1::mutation_id`; the logical graph ID is derived only from
the enclosing `ClientMutationKey`. Neither identifier is duplicated in the coordinator. The Graph
principal is an internal durable recovery locator and is never projected into the public API. No
routing-generation abstraction is introduced. The coordinator uses this canonical lifecycle state
machine:

```text
Open
AppendPending { chunk_index, fingerprint, child_mutation_id }
FinalizePending { stage, cursor }
AbortPending { active_chunk }
Completed
Aborted
Failed { reason }
```

`Completed`, `Aborted`, and non-retryable `Failed` are client-terminal and require the top-level
`terminal_at_ns = Some(_)`; every other lifecycle state requires `None`. Receipt GC progress is
orthogonal to this lifecycle: `receipt_gc_cursor` is `None` before cleanup and
`Some(next_chunk_index)` only while an eligible terminal job's receipt range is being deleted. GC
never replaces or erases the canonical terminal state or a `Failed` reason. Each lifecycle and GC
transition is validated by the Router store facade, and status handlers classify the request
identity/payload variant exhaustively so a mutation-family mismatch rejects rather than projecting
another status shape.

One new canonical Router map, `ROUTER_BULK_LOAD_CHUNK_RECEIPTS` at the next free Router MemoryId 49,
is keyed by
`(bulk_job_mutation_id, chunk_index)` and stores the immutable chunk fingerprint, resolved Graph
request while the child can still be replayed, unique child Graph mutation ID, state
(`CanonicalPending | CanonicalCommitted | ProjectionPending | RetirementPending | Completed`),
exact atomic-insert receipt when committed, and completion metadata. The Router writes
the parent `AppendPending` transition and the complete child row atomically before the first Graph
`await`. This map provides bounded status pagination and response-loss
recovery without growing and rewriting one unbounded parent value. Receipt insertion and parent
cursor advancement occur in one Router message segment before Graph retirement is requested.

Rows are deliberately sized to their lifecycle. The chunk envelope is **not persisted**: the stored
chunk fingerprint is the resume idempotency key, and replay integrity is enforced by the
Graph-request fingerprint handshake with the shard, so retaining a second copy of the payload only
for read-time digest recomputation is dropped. `complete_bulk_load_child` **compacts completed rows**
in place by dropping the resolved Graph request and its fingerprint; a completed child is never
replayed, so status pagination, finalize, and receipt-GC decode only receipt-sized rows regardless
of how large the original chunks were.

Each chunk receives a distinct child `MutationId`; the parent job ID is never reused as a Graph
journal key. Graph uses its existing ordered mutation journal at MemoryId 39 for each chunk and
requires no new region. Router receipt-map values are bounded per chunk; the parent never embeds the
receipt list.
Terminal Router jobs and their receipt ranges become GC-eligible seven days after `terminal_at_ns`.
GC preserves the canonical terminal lifecycle outcome, initializes `receipt_gc_cursor` to `Some(0)`,
deletes at most a fixed number of consecutive receipt rows per step, and durably advances that cursor.
While cleanup is in progress, status continues to project the preserved `Completed`, `Aborted`, or
`Failed` outcome; after retention expires, its receipt page may be partial as rows are removed. GC
removes the parent and client-key binding only after the entire receipt range is gone. Graph chunk
evidence follows the existing retirement-then-nine-day policy. Open and recoverable jobs are not
removed by ordinary terminal GC. Update-row journals additionally follow the pending-child pin
above; their seven-day age alone never removes evidence required to settle an unfinished chunk.

If duplicate Append calls interleave, the first message has already persisted the parent/child
identity before awaiting Graph. A same-index/same-fingerprint call resumes that exact child; a
different fingerprint or index rejects. Finalize while Append or Abort is active returns
`RouterError::Busy` with the exact blocking operation identifier and performs no transition. Abort
from `Open` can become `Aborted` immediately. Abort with an active child transitions to
`AbortPending`, then drives/replays that exact persisted child envelope and mutation ID through
canonical, projection, and retirement completion before marking the job `Aborted`. It never uses an
absence read as cancellation evidence and never permits a second child; therefore no delayed copy of
the active request can commit after the terminal Abort result.

The exact public bulk wire types are:

```text
BulkLoadResponse =
  Started { next_chunk_index: u32 }
  | Appended { chunk_index: u32, receipt: AtomicInsertReceiptV1 }
  | FinalizeAccepted { state: BulkLoadPublicStateV1 }
  | AbortAccepted { state: BulkLoadPublicStateV1 }

BulkLoadPublicStateV1 = Open | AppendPending | FinalizePending | AbortPending
  | Completed | Aborted | Failed { reason: String }

BulkLoadStatusPage = {
  state: BulkLoadPublicStateV1,
  next_chunk_index: u32,
  committed_chunk_count: u32,
  completed_chunk_count: u32,
  terminal_at_ns: Option<u64>,
  expires_at_ns: Option<u64>,
  receipts: Vec<BulkLoadChunkReceiptV1>,
  next_receipt_cursor: Option<u32>,
}

BulkLoadChunkReceiptV1 = {
  chunk_index: u32,
  receipt: AtomicInsertReceiptV1,
}
```

`receipt_cursor: Option<u32>` denotes the first requested chunk index and `max_receipts: u32` is
bounded to `1..=MAX_BULK_LOAD_RECEIPTS_PER_PAGE`. Public status exposes no shard ID or principal.

Command transitions are exhaustive:

| Command     | Current state                                                  | Result                                                                                     |
| ----------- | -------------------------------------------------------------- | ------------------------------------------------------------------------------------------ |
| Any command | `receipt_gc_cursor.is_some()`                                  | reject client-key-expired `Conflict` before dispatch; GC continues from its durable cursor |
| Start       | absent                                                         | create Open and return Started                                                             |
| Start       | same graph-scoped key exists as BulkLoadJob and GC is inactive | return `Started { next_chunk_index }` from the canonical coordinator                       |
| Start       | same graph-scoped key exists as another mutation family        | reject Conflict                                                                            |
| Start       | same textual key exists only on another graph                  | create an independent Open job                                                             |
| Append(i,f) | Open and i == next                                             | persist child then drive it                                                                |
| Append(i,f) | same active or completed i/f                                   | resume or return exact receipt                                                             |
| Append(i,f) | every other lifecycle/fingerprint/index combination            | reject Conflict before dispatch                                                            |
| Finalize    | Open with no active child                                      | enter/resume FinalizePending, then Completed                                               |
| Finalize    | FinalizePending                                                | resume the exact persisted stage/cursor, then Completed                                    |
| Finalize    | AppendPending                                                  | return `Busy { operation: "bulk_load.append" }` without transition                         |
| Finalize    | AbortPending                                                   | return `Busy { operation: "bulk_load.abort" }` without transition                          |
| Finalize    | Completed                                                      | return exact completed result                                                              |
| Finalize    | Aborted                                                        | reject Conflict                                                                            |
| Finalize    | Failed                                                         | return the stored non-retryable failure verbatim                                           |
| Abort       | Open                                                           | enter Aborted without canonical work                                                       |
| Abort       | AppendPending                                                  | enter AbortPending, finish exact child, then Aborted                                       |
| Abort       | AbortPending or Aborted                                        | resume or return exact aborted result                                                      |
| Abort       | FinalizePending or Completed                                   | reject Conflict                                                                            |
| Abort       | Failed                                                         | return the stored non-retryable failure verbatim                                           |

Every `BulkLoadCoordinator::Failed` state is non-retryable; transient remote or projection failures
remain in their resumable pending state and use the enclosing record's diagnostic field. The
GC-active row above takes precedence over the lifecycle rows. `bulk_load_status` is read-only in
every lifecycle and GC phase. During receipt GC it returns the preserved terminal public state and
the still-present suffix selected by the requested receipt cursor; it never exposes the internal GC
cursor. A physically removed post-retention record returns the ordinary not-found result.

### 4. Ownership and visibility

- Router is the source of truth for public job identity, next chunk, placement, public receipt
  encoding, and job lifecycle.
- Graph is the source of truth for shard-local canonical writes, allocated local IDs, and exact chunk
  replay.
- Index canisters remain derived consumers. Bulk completion includes the explicitly required
  projection convergence, but neither bulk load nor atomic insert acquires index ownership.
- Public requests never contain shard IDs, canister principals, plan blobs, Graph journal cursors, or
  index transport state.

## Alternatives

### Rename the existing cursor list to `bulk_load`

Rejected. It would retain caller-owned input, expose no durable job/status/finalize lifecycle, and
overstate what the API provides.

### One generic mutation method with options

Rejected. Atomicity, cursor, identity, request shape, response, and failure semantics would become
independent flags with invalid combinations. Separate workflow methods are easier to use correctly.

### Persist a job-scoped symbolic vertex map

Deferred. It adds another canonical map, key-lifetime policy, conflict semantics, and cleanup path.
Returning encoded IDs and requiring self-contained chunks is sufficient for the first social-demo
load and keeps the initial protocol smaller.

### Store every receipt inside one parent job value

Rejected. Every append would rewrite an ever-growing value and bulk status could not page receipts
within bounded response limits.

### Add a generic durable job scheduler

Rejected. It would duplicate the existing Router mutation lifecycle, retry owner, and observability
path.

## Consequences

Positive:

- API names expose query/update, atomicity, and long-running load intent directly.
- Direct encoded-ID edge insertion removes Property Index convergence from the initial-load critical
  path.
- Existing Router/Graph mutation machinery remains the only canonical dispatch/replay substrate.
- Bulk status and receipt recovery remain bounded and pageable.

Trade-offs:

- The implementation requires a breaking Candid/SDK migration and development stable-state reset.
- The breaking migration removes the former cursor-list surface and requires callers to adopt the
  durable load lifecycle.
- Bulk-load v1 is single-shard and requires the caller to retain or recover returned encoded IDs.
- A new Router stable region and GC range cleanup are required.

## Implemented migration and activation

1. The non-bulk L1 rename and atomic-insert receipt/status contract are implemented.
2. The bulk-load job state and MemoryId 49 receipt map are implemented.
3. The social-demo loader submits vertex chunks followed by receipt-ID-addressed edge chunks.
4. The former cursor-list public types, helpers, and documentation are removed.
5. Candid and SDK bindings are regenerated as one breaking release set.

No compatibility endpoints, decoders, or dual stable shapes are retained. Each incompatible
development stable-format activation requires an explicit wipe/reinstall gate before canister use.

## Required tests

- query/update and prepared-manifest kind mismatch rejection;
- atomic edge/vertex/mixed receipt input-order IDs, direct immediate edge insertion, exact replay,
  response-loss/status recovery, family mismatch, payload preflight, upgrade/reopen, and retention,
  including compacted ordered completions that retain the fixed Graph-request fingerprint while
  dropping the Graph request/target and reject a changed fingerprint without rewriting the record;
- bulk Start/Append/Finalize/Abort state transitions, exact/conflicting/out-of-order chunk replay,
  response loss, paged receipt recovery, prefix preservation, projection/retirement recovery,
  same textual key on distinct graphs, exact `Busy.operation` values, upgrade/reopen, retention range
  cleanup, and payload bounds;
- Append awaiting Graph while Abort arrives, Append awaiting Graph while Finalize arrives, duplicate
  concurrent Append for the same chunk, lost Graph response before the Router receipt row, and
  interrupted receipt-GC cursor cleanup that preserves the terminal public outcome and rejects
  commands as expired;
- Candid/SDK conformance and removal of superseded public names;
- focused canbench coverage for maximum-size Graph receipt journal encode/decode and Router bulk
  receipt insertion/status pagination.

## Related decisions

- [ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md)
- [ADR 0025](0025-client-mutation-journal-retention-sweep.md)
- [ADR 0027](0027-graph-mutation-journal-retention.md)
- [ADR 0041](0041-router-graph-batch-mutation-dispatch.md)
- [ADR 0042](0042-router-dynamic-instruction-budget-batching.md)
- [ADR 0049](0049-input-order-preserving-batch-graph-mutations.md)
- [ADR 0056](0056-router-api-surface-layering-and-consolidation.md)
