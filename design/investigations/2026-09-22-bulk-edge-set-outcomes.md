# Bulk edge SET: durable outcomes and retry ownership

Status: Planned — proposed contract; bulk edge SET is not implemented
Date: 2026-09-22
Anchor timestamp: 2026-09-22 09:41:27 UTC +0000
Inspected HEAD: `9a64049f47fc07542446d651d4b09421636b73c5` (dirty working tree)
Tracking: [GAP-2026-09-11-004](../implementation-gaps.md#gap-2026-09-11-004--bulk-load-edge-property-update-needs-replicated-resolution-and-retry-safe-target-validity)

## Recommendation and boundary

Extend the **existing Graph mutation journal, Router mutation record, and chunk receipt** with a
bulk edge row family. Retain a typed successful count or definitive rejection; do not retain edge
occurrence identities merely to replay a completed row. Reuse explicit `Active → Retired` journal
retirement instead of assuming a time limit on unfinished Router recovery.

This develops the selected
[first-execution all-match direction](2026-09-12-bounded-edge-membership-witnesses.md#selected-direction-and-storage-accounting):
one directed, same-Graph input row updates every eligible canonical parallel occurrence, zero
matches succeeds, and admission overflow rejects the whole row before canonical writes. The
[pre-admission singleton investigation](2026-09-12-bulk-edge-set-prerequisites.md) is not revived.
Endpoint/policy semantics, concrete resource limits, wire/codec encoding, and whole-row storage
preparation still require decisions before implementation. This is not an implementation approval.

The observable execution point is **publication of a durable Graph terminal outcome in the same
message execution as its canonical effects**. A pre-effect retryable refusal or rolled-back trap
has not established a row outcome; no snapshot of that abandoned attempt is promised. A retry
without an outcome can therefore evaluate later state. After an outcome exists, even a zero or
rejection, selection never runs again for that identity. The future API must state this distinction
rather than promising a snapshot at Router admission, first send, or a trapped Graph attempt.

## Verified owners and representation limits

| Owner/source | Implemented representation | Required change for this proposal |
| --- | --- | --- |
| [Graph wire/validator](../../crates/graph-kernel/src/plan_exec.rs), `MutationJournalState`, `GraphMutationRequestIdentityV1`, `validate_graph_mutation_journal_fields` | `Incomplete / Completed`; unit `PlanExecution`; fingerprinted ordered insert families. `Completed` means durable local outcome, not index convergence. | A distinct row family and typed positive/zero/rejected result; existing variants cannot encode it as-is. Every journal consumer must distinguish family and outcome before reading a success count. |
| [Graph stable journal](../../crates/graph/src/facade/stable/label_stats_delta.rs), `GraphMutationJournalEntryV1`, `checked_journal_record_size`, `GraphMutationJournal::evict_expired` | Region 39 map keyed by `MutationId`; manual 52-byte primary plus appendices; `Active` is pinned, `Retired` ages from retirement, `NotApplicable` ages from recording. | Extend this value/codec and its single sizing/validation boundary, not a new table or copied codec. The new family requires active/retired evidence. |
| [Graph journal facade](../../crates/graph/src/facade/store/label_stats_delta.rs), `retire_ordered_mutation_at` and vertex/mixed equivalents | Exact family/fingerprint check, idempotent retirement, receipt-bearing acknowledgement; absence returns `None`. | Extend the existing retirement ownership to the row family; do not pretend an edge-insert receipt is a SET result. |
| [Router row identity](../../crates/router/src/facade/stable/label_stats.rs), `RouterMutationRequestIdentityV1`; [reservation/GC](../../crates/router/src/facade/store/idempotency.rs) | Region 7; scalar `PlanExecution` fingerprint and optional owning chunk key retained through compaction. | Typed bulk edge row identity/payload, including authored ordinal, immutable Graph request and terminal result while needed. Do not hide these in vertex fields or a diagnostic string. |
| [Router chunk record](../../crates/router/src/facade/stable/bulk_load.rs), `BulkLoadChunkReceiptRecordV1`; [transition owner](../../crates/router/src/facade/store/bulk_load.rs) | Region 49; vertex-specific saved IDs and `updated_row_count`; one parent/child transition boundary. | Typed edge-update progress/receipt with input-row frontier separate from matched-edge total. Existing optional vertex fields are not that representation. |
| [Router update coordinator](../../crates/router/src/bulk_load.rs), `classify_update_row`, `settle_outcomes`, `append_bulk_load_updates` | Vertex count 0 is `NotWritten`, 1 is `Committed`, greater than 1 rejects. Unknown/unresolved cannot terminalize Abort. | New family-specific outcome interpretation. Do not change the existing vertex zero/singleton contract. |

LARA continues to own physical traversal/storage, Graph owns selection and canonical properties,
Router owns authorization/lowering, placement, admission and recovery, and indexes own derived
postings. No portable GQL/planner changes, new MemoryId, per-edge metadata, or second scheduler are
needed by the outcome design. Shared wire schemas belong in Gleaph's kernel/integration owners.

## Current scalar retention gap

This finding affects an **existing** recovery path, independently of the proposed edge family:

1. Router `evict_expired_client_mutation_keys` never age-evicts non-terminal records;
   `reserve_plan_mutation_at` expires terminal records only. The pending-child pin additionally
   retains terminal bulk row evidence. A dispatched, unconfirmed scalar can remain retryable
   beyond nine days.
2. Graph `PlanExecution` requires `NotApplicable` retirement. `evict_expired` removes that entry
   when its recorded age exceeds the nine-day `GRAPH_MUTATION_JOURNAL_RETENTION_NS`.
3. [Graph `run_wire_plans_last_read_row_count`](../../crates/graph/src/gql_run.rs) short-circuits a fresh call only while the
   journal exists. After absence it proceeds to execution. Router's bulk classifier treats an
   absent Graph entry for a dispatched row as `Unresolved`, allowing an explicit identical Append
   to resume the same row identity.

Counterexample: Graph durably records zero, but the Router callback fails **before the scalar
record captures zero**. The row remains pending; after more than nine days a real Graph sweep
removes its evidence. Eligibility changes. An identical retry can now write instead of replaying
zero. Positive outcomes can likewise be reapplied against later property state. This is a
source-derived failure trace at the initial design checkpoint, not an attribution of 0364's failures.

**Follow-up, 2026-09-22 10:54:06 UTC +0000:** the separate
[PocketIC reproduction](2026-09-22-scalar-journal-retention-reproduction.md) confirmed both positive
reapplication and zero becoming a write after actual nine-day Graph GC, while Router retained the
same IDs as `CanonicalPending`. The desired assertions are red; production repair remains open.
The proposed bulk edge contract below is unchanged.

The existing
[`assert_bulk_update_policy_and_expired_row_replay`](../../crates/pocket-ic-tests/tests/adr0057_durable_bulk_load.rs)
advances eight days and sweeps **Router** records after Router already retained zero. It protects
that valuable contract but does not test this lost-scalar-confirmation/Graph-GC window.

[ADR 0027](../adr/0027-graph-mutation-journal-retention.md)'s old bounded-Router-TTL rationale cannot
justify these longer-lived records. The separate
[scalar retention gap](../implementation-gaps.md#gap-2026-09-22-001--recoverable-plan-execution-can-outlive-graph-journal-retention)
requires a regression-backed repair. Merely increasing a finite TTL cannot cover unbounded pending
recovery. The row-retirement proposal below does not silently repair ordinary GQL or bulk vertices.

## Immutable request identity

The proposed row identity is the owning chunk key `(job_mutation_id, chunk_index)` plus authored
row ordinal, under the existing caller/logical-graph-scoped Router authority. Allocate a distinct
nonzero row `MutationId` once; neither parent nor chunk ID is its Graph journal key. A fresh job
that reuses an expired public key must not collide with a previous job's row reservations.

- Keep the chunk's exact authored fingerprint and winning admitted bindings. Before any row
  reservation, initial endpoint/name resolution must not change an already-admitted suffix on
  retry. Exact binding and authorization-freshness rules remain an endpoint/policy gate.
- Once a row request is reserved, retain its immutable Graph envelope in the existing Router row
  payload. Verify the incoming retry against the original identity before mutable re-resolution.
  Conflicting caller, graph, owner, ordinal, target or payload rejects before counters/dispatch.
- Reuse the existing client-key map. A single helper may derive a lookup key from the typed owner;
  never parse that string to recover authority. Identity/family conflicts fail closed even if an
  ordinary user key collides. No separate row-key registry is proposed.
- Define one domain-separated canonical Graph fingerprint encoder in the shared owner. It covers
  the row family/version, graph and fixed shard/canister target, row owner/ordinal, admitted
  endpoints/direction/label, SET values and every semantic predicate/binding. A bound-profile
  version must participate if it changes the row's definitive admission result. Graph recomputes
  and verifies it before consulting the saved outcome. Exact bytes/tags are not assigned here.
- Transient transport/projection metadata can be outside that fingerprint only if it cannot change
  the row's semantic input. Authorization freshness, mutable index catalogs, and policy revocation
  must not be silently conflated with replay identity; the later API review must settle them.

Replay still requires authorized caller/graph access. It must not re-evaluate current canonical
eligibility to replace an already-retained result. Denying access to a receipt does not erase it.

## Graph terminal outcome and write boundary

The proposed logical result is:

```text
Applied { matched_edge_count }
Rejected { code }
```

These are proposed domain shapes, not existing Rust types or selected persistence bytes.
`Applied { 0 }` is success. `matched_edge_count` counts eligible canonical occurrences, not changed
values, orientations, property assignments, or projected RETURN rows. An unchanged SET still
counts its matched occurrence. `Rejected` has no successful count; it cannot be encoded as zero.
Use a bounded typed rejection code, not an arbitrary error string containing property data.

The new family stores that result once in its existing journal value. Any common `row_count`
view must project the successful count, not persist a second authoritative counter. If the common
`Completed` terminal marker is reused, readers must validate family and result first: `Completed`
alone is not an Applied test. This requires explicit stable/wire changes and exhaustive admission,
decode, replay, status and retirement validation. Do not widen unrelated APIs with loose flags.

Graph execution order:

1. Authenticate, validate immutable request identity/fingerprint and bounded envelope. If a journal
   entry exists, check exact family/fingerprint and return its terminal result without traversal,
   property reads, index admission, or SET. A conflicting/invalid entry fails closed.
2. With no outcome, completely select eligible local occurrences and prepare **the whole row**.
   Admit physical reads, decoding/heap, selected count, properties, primary writes, index intents,
   response/journal size, and all later counter updates before the first canonical effect.
3. A definitive domain rejection publishes `Rejected` with `Active` retention before responding.
   It changes no canonical property values or derived posting intents/watermarks. Journal writes
   are intentional; this is not a promise of zero I/O, zero initialization, or zero backing growth.
4. For Applied, commit prepared canonical writes plus their existing derived intents and the
   `Applied` journal entry in one no-external-call message execution. Unexpected post-fence
   failures trap; a recoverable `Err` after writes is not rollback. Zero follows the same terminal
   journal path without primary property mutation.
5. Any subsequent projection/outbox work uses its existing durable owner. No callback may resume
   original-row selection or canonical SET. Canonical completion does not assert index convergence.

Only rejection paths proven to precede canonical effects may produce `Rejected`. Bound exhaustion
under the admitted profile is definitive. A retryable index-build Sealing refusal, unavailable
service, instruction/allocator trap, malformed transport envelope or unreadable journal is **not**
a recorded definitive result merely because an error was returned. A valid domain rejection needs
its journal write to succeed; if that write traps, no terminal result exists. The later handler
must enumerate the exact allowed rejection codes and their pre-effect enforcing owners.

0364 prepares a single B-tree property at an index fence; it does not compose multiple properties
or allocations into this row boundary. Existing bounded topology/inline collectors and experimental
extent preparation likewise do not establish production sidecar admission or whole-row safety.
Do not implement this sequence as a loop of fallible scalar setters followed by a journal write.

## Router settlement and accounting

For a chunk of `N` authored rows, retain:

- `p`: exclusive end of the contiguous **successful input-row** prefix, `0 ≤ p ≤ N`;
- `m`: matched-edge total for exactly that prefix, `m = Σ count(i)` for `0 ≤ i < p`.

Applied zero advances `p` by one and adds zero to `m`. Rejection advances neither. For example,
`Applied(3), Applied(0), Rejected(limit)` yields `p = 2, m = 3`, not three successful input rows and
not a prefix ending before the zero row. Public `next_offset` refers to authored input, never `m`.
The existing vertex `updated_row_count` must not be reinterpreted as an edge count.

One Router store transition captures the exact Graph result and updates row/chunk state together.
For ordinal `p`, Applied advances both counters exactly once; duplicate or stale callbacks cannot
add the count again or rewind the prefix. A different result for the same identity is corruption,
not a new observation. A successful row behind an unresolved/rejected gap fails closed. Checked
worst-case `N × per-row match bound` and receipt sizing must be admitted before dispatch so the
Router cannot discover a recoverable aggregate overflow after Graph commits.

| Evidence | Explicit Append | Abort after closing new-row permission |
| --- | --- | --- |
| Durable proof no Graph send was permitted/performed | May start the next row while the gate is Open | May abandon the suffix; revoke any suspended preparation before terminalizing |
| Router-local definitive pre-dispatch rejection, retained under the row owner | Return same rejection; no re-resolution or new reservation | Stop at the prior successful prefix; no Graph retirement for an unsent row |
| Exact Graph `Applied(n)`, including zero | Capture/account once; never select again | Account the successful row, including zero; continue settlement |
| Exact Graph `Rejected(code)` | Capture and return same rejection; stop the suffix | Keep the rejection and prior prefix; retire its Graph evidence |
| Canonical dispatch pending, Graph entry absent or no terminal result | Only explicit identical-row retry with the saved ID/request and current dispatch gate | Remain pending; absence is not cancellation |
| Call timeout, `SYS_UNKNOWN`, unreadable/mismatched journal | Remain unresolved; do not allocate a new ID or change payload | Remain pending; never report a proven unwritten suffix |

A Router-local pre-dispatch rejection is a different typed payload branch from a Graph receipt;
it must not fabricate Graph acknowledgement or retirement. `Unknown` is an observation of a
pending dispatch, not another terminal Graph outcome to persist alongside Applied/Rejected.

Recheck durable parent/chunk/row permission **at the actual outbound-send boundary**, after any
awaiting resolution/preparation. Persist the dispatch envelope/permission consumption and send in
one Router message segment with no intervening external await. A previously read grant or routing
lease alone cannot authorize a suspended task after Abort or retirement starts.

`AbortPending` forbids never-started rows. An explicit identical Append may settle a previously
permitted dispatch; it may still execute if the first call never landed. Thus Abort is not rollback
or a promise of no effects after the Abort request arrives. Background recovery and Abort may
read/capture evidence, advance derived work and retire, but do not initiate/re-drive canonical SET.
An unresolved row keeps Abort pending; liveness requires an available Graph and, where needed,
the client's unchanged input. Terminal Abort requires every possibly sent row accounted for,
no success beyond a gap, closed send permission, and completed retirement obligations.

A definitive rejection stops this chunk; later rows do not skip it. Retain its ordinal/code along
with the successful prefix so replay after Abort cannot disguise it as a normally completed chunk.
Corrected input belongs to a new job after the old job closes. The exact public response variant
and status projection require a separate API decision; an `Updated` count alone is insufficient.

## Retention, retirement, and delayed requests

Use existing lifecycle phases rather than a separate acknowledgement journal:

```text
Router: canonical pending → durable outcome / canonical committed
        → required projection → retirement pending → retired acknowledgement / completion
Graph:  absent → terminal Applied or Rejected, Active → same outcome, Retired
        → existing retirement-age GC
```

These are conceptual transitions for the proposed row payload, not claims that the present scalar
payload or ordered-insert receipt validators accept it.

1. Graph Active row outcomes, including zero and rejection, are never age/LRU-evicted. Router
   pending row evidence and its chunk owner remain retained. Capacity pressure must reject new
   admission, not delete evidence for replayable work; concrete active-work limits are a gate.
2. After Router durably captures the exact outcome, no canonical dispatch is permitted for that
   row. Preserve the fingerprint, Graph target and result until retirement settles. Advance any
   projection obligations which still depend on the journal using the existing recovery owners;
   a posting or label-stats watermark alone is not proof that canonical replay has stopped.
3. Before the retirement await, persist `RetirementPending`. An authenticated exact-family/ID/
   fingerprint retirement message changes only Active to Retired; it does not clear properties,
   intents or the terminal result. A duplicate preserves the original retirement timestamp and
   returns the same receipt-bearing acknowledgement. Router validates the entire acknowledgement.
4. A failed/lost retirement callback leaves Router RetirementPending. Only retirement/evidence
   reads can retry, never SET. If the retired Graph entry is eventually GC'd before Router learns
   the acknowledgement, absence leaves recovery pending explicit repair, as in ADR 0027; it does
   not prove acknowledgement or reopen dispatch. This is safe but not unconditional liveness.
5. A chunk may release its row-evidence pins only after all relevant results are durably folded
   into its terminal receipt and every possibly sent row's retirement is acknowledged. Router
   compaction must retain failure/count identity until that point. Parent closure is unnecessary
   for an already completed chunk; normal terminal receipt GC follows the existing owner.

**Late-send condition:** retirement must also order after every permitted canonical send, not only
after the first response. The IC guarantees ordering of accepted requests from the **same caller
canister to the same callee canister**, not arbitrary callback order. With the durable send gate
closed before retirement, all earlier accepted row requests reach Graph before that retirement
request. The Graph row's canonical section has no downstream await, so those requests either see
its retained outcome or establish one before retirement; they cannot leave a later canonical
continuation. Expired requests may be dropped instead, but timeout is not cancellation.

This argument requires a fixed Router-principal/Graph-principal pair, every send path obeying the
fresh gate, no canonical continuation after a downstream call, and preservation of this authority
through upgrades. A new sender, reinstallation, target remapping, or another dispatch path invalidates
the argument. Do not use a nine-day delay as a substitute for these conditions. The exact-send,
Abort and retirement race tests below are required before accepting the implementation.

## Alternatives, cost, and review

| Alternative | Result |
| --- | --- |
| Keep existing scalar count and age-only retention; increase TTL | Smallest code change, but zero/rejection remain conflated and indefinitely pending recovery still outlives every finite TTL. Reject. |
| Add an execution deadline/fence and retain until all requests are inadmissible | Potentially bounded retention, but changes long-lived resume semantics and needs a Graph-enforced expiry contract; client-call timeout is not that contract. Not selected. |
| Extend existing row journal/receipt families and explicit retirement | Recommended proposal. Preserves existing ownership and no occurrence metadata; costs typed states, result transfer, retirement calls/writes and active retained records. |
| Occurrence registry, lifetime IDs, second outcome journal or generic transaction subsystem | Does not by itself solve terminal-outcome retention; duplicates ownership or adds per-edge storage/transaction rules. Disproportionate to this boundary. Reject for this slice. |

- **Encapsulation:** only Graph's row execution owner publishes canonical outcomes; Router consumes
  typed receipts rather than reading physical slots as durable identities.
- **Separation of concerns:** Graph owns local eligibility/preparation/commit, Router owns dispatch
  permission and progress, indexes remain derived. No language-crate policy or storage leakage.
- **Invariants:** immutable request/result, single row publication, monotonic counters and closed
  send permission must be enforced at existing store boundaries, including decode/reopen.
- **Consistency:** Graph is the source of its outcome; the Router copy is an acknowledged recovery
  receipt, and chunk counts are its guarded fold. Those copies require exact equality, not separate
  selection. Existing outboxes remain the only derived-delivery source.
- **Fitness for purpose:** bounded row-specific extensions rather than whole-GQL transactions.
  Recommendation: **APPROVE WITH CHANGES for further design; implementation remains gated**.
  This is same-assistant review, not independent or security approval.

No adjacency or property-key bytes are added by the result-retention design. That is not zero
storage cost: per-row fingerprint/result/retirement data, Router request retention, and extra
round trips must be measured. Existing Graph primary size is 52 bytes, **not** a proposed final
record size. Report actual full serialized active/retired/compacted records, journal/outbox entry
counts, logically used bytes and retained backing separately. Active work is not a TTL-bounded
working set. No numeric production limit or throughput improvement is inferred here.

Incompatible wire/stable changes require a coherent fresh/reinstalled development federation,
including owners and dependent indexes, not resetting Graph while retaining Router replay state.
Activation must also account for outstanding old requests; reinstall alone does not prove that
those requests can no longer execute. No migration, legacy decoder, parallel format or
heap-authoritative retirement state is proposed.
An accepted ADR revision and exact layout/API review must precede implementation.

## Required validation and implementation gates

The following are **planned tests**, not executed evidence:

| Owning layer | Contract and discriminating oracle |
| --- | --- |
| Kernel/Graph codec unit tests | Positive, zero, rejection and active/retired round trips; exact bytes/bounds; reject wrong family, missing receipt, conflicting success count/rejection, partial cursor and unsupported version. Reject a new row family through ordinary GQL/ordered-insert readers rather than silently returning its count. |
| Graph row tests | Parallel occurrences with different old values plus unrelated guards; exact count, values and derived removals/insertions. Same-value SET counts; late-added/reinserted parallels remain untouched on replay. Zero then insertion still replays zero. |
| Graph admission tests | An invalid/over-budget last occurrence/property rejects before any preceding value or posting intent/watermark changes; durable rejection survives later removal of the cause. Exact and one-over limits, physical tombstones and old-body reads included. |
| Router store tests | `Applied(3), Applied(0), Rejected` gives prefix 2/edges 3; duplicate/stale callbacks do not double-add; success after gap rejects; owner/ordinal/graph/fingerprint conflicts occur before allocation; recreated public job key cannot reuse old row evidence. |
| Router dispatch tests | Suspend before actual send, close permission with Abort/retirement, resume: no send. Not merely a failure hook before admission. Unknown/absent dispatched evidence never produces terminal Abort or a fresh mutation ID. |
| Graph/Router retention tests | Active positive/zero/rejected evidence survives a sweep beyond nine days; unrelated eligible controls are actually removed. Duplicate retirement preserves its timestamp. Wrong acknowledgements fail closed; lost acknowledgement retries retirement only. |
| PocketIC lifecycle | Lose callback **before Router captures Graph outcome**, change eligibility, advance beyond Graph retention and drive real Graph GC, then replay positive/zero/rejection. Include Abort, delayed duplicate sends, retirement callback loss and upgrade/reopen of both durable owners. Inspect direct postings as well as canonical values, since query residuals can hide stale postings. |
| Authorization/isolation E2E | Actual non-tenant allowed/denied residuals, same public key in another graph, endpoint admission and catalog/policy changes. Cannot substitute admin-only success or a guessed fingerprint predicate. |
| Canbench | Actual complete row apply/replay/rejection plus exact receipt encode/retire/accounting/GC. Vary matched parallels and properties independently; setup and assertions outside timing. Capture all journal/outbox/backing costs, not only the scalar setter. |

No fixtures or test binaries were added: **0 Rust tests, 0 PocketIC constructors, 0 canister installs**.
The implementation plan should reuse the existing ADR 0057 lifecycle family, inventory its actual
bootstrap count, and add at most one new single-shard lifecycle fixture if existing scenarios cannot
safely accommodate the extra clock/fault state. Exact tests/constructors/installs and commands must
be fixed by that plan after the implementation boundary is selected, not guessed as passed here.

Dependency order: repair the existing scalar retention gap independently; approve this outcome
contract and its exact representation; settle endpoint/policy and production whole-row admission;
then implement and validate Graph execution plus Router settlement/retirement before public
API/SDK/CLI activation. Property backend selection and 0364's outstanding validation remain
separate. Do not expose a reject-only placeholder endpoint while these prerequisites are open.

## External evidence and validation status

Consulted the official
[multi-canister skill](https://skills.internetcomputer.org/.well-known/skills/multi-canister/SKILL.md)
and checked relevant semantics against the repository's `ic-cdk` **0.20.1** (`Cargo.lock`), rather
than copying its illustrative heap-only authority or compatibility-oriented upgrade examples.

- [Message execution properties](https://docs.internetcomputer.org/references/message-execution-properties):
  one message execution is atomic; callback traps do not undo earlier executions. Successfully
  accepted requests preserve same-caller/callee send order; multiple callbacks may interleave.
  Bounded-wait `SYS_UNKNOWN` can replace a real response even when the callee executed successfully.
- [IC abstract behavior](https://docs.internetcomputer.org/references/ic-interface-spec/abstract-behavior/):
  request queues are keyed by sender/receiver; only a queue head executes. Expiration preserves
  request position and permits dropping expired requests; it is not a guarantee of nonexecution.
  Response messages are not ordered. These facts support the conditional late-send argument,
  not a blanket cross-canister atomicity or application-GC proof.
- [`ic_cdk::call::RejectCode` 0.20.1](https://docs.rs/ic-cdk/0.20.1/ic_cdk/call/enum.RejectCode.html)
  includes `SysUnknown`; it is a transport classification, not the proposed durable rejection code.

Source and specification inspection only. No new Rust build, test, PocketIC run, benchmark, fault
reproduction, deployment or independent review occurred. The design checkpoint's document checks
are recorded in [plan 0365](../../plans/0365-bulk-edge-set-outcome-design.md); they cannot establish
runtime readiness or close either implementation gap.
