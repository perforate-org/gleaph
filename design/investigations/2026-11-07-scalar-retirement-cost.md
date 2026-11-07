# Scalar retirement: what option A costs

Dated record, 2026-11-07. Question asked: if GAP-2026-09-22-001 is closed by **option A** - Router's
replay permission bounded by Graph evidence lifetime, i.e. PlanExecution evidence retained until the
owner explicitly retires it, with a Router→Graph retirement call - what does it cost at runtime, in
call counts and in the operation's own instructions?

This file records the measurements and the sizing. It does not decide the repair; the ledger entry
owns that.

## Result

- **Calls: +1 Graph update per terminal scalar mutation**, the same shape the ordered families
  already pay (`retire_ordered_mutation` is awaited in `gql.rs` after the projection record). No
  GC-side call count changes in either canister: both sweeps stay amortized on existing write paths.
- **Graph-side work of that call: 14.63 K instructions**, 0 heap pages, 0 stable pages. Measured with
  `bench_graph_journal_scalar_retirement_transition` (see below).
- **Retention does not grow if retirement is prompt.** A PlanExecution entry currently ages out 9 days
  after it is *recorded*; under option A it ages out 9 days after it is *retired*. Retiring in the same
  update that captures the outcome moves the anchor by one round trip, not by days.
- **A schema predicate blocks the state, not the layout.** `graph-kernel`'s
  `validate_graph_mutation_journal_fields` refuses a PlanExecution entry carrying any retirement state
  ("PlanExecution journal entry must not have retirement state"), so option A changes that predicate -
  a semantics change on an already-encoded enum (`NotApplicable | Active | Retired`), not a layout change
  and not a reinstall.

## Measurement

`crates/graph/src/bench/mod.rs::bench_graph_journal_scalar_retirement_transition` measures the durable
work the new endpoint performs: one stable `get` with its full record decode, the identity comparison,
the `Active → Retired { at_ns }` transition, one stable overwrite, and the acknowledgement value the
caller validates.

| Operation | Instructions | Heap | Stable |
| --- | ---: | ---: | ---: |
| Retirement transition (this bench, new) | 14.63 K | 0 | 0 |

It is measured with an **ordered** identity because the PlanExecution predicate above refuses a
retirement state; the durable work (decode, compare, overwrite) is identical for either family, which is
the same work `retire_ordered_mutation_at` already does. Run with
`canbench bench_graph_journal_scalar_retirement` from `crates/graph` (no `--persist`: the artifact was
not updated).

Existing measurements the new number is read against, from `crates/graph/canbench_results.yml` and the
[same-slice investigation](2026-09-22-scalar-journal-retention-reproduction.md):

| Baseline | Instructions | Source |
| --- | ---: | --- |
| Journal entry encode (scalar) | 2,295 | `bench_graph_journal_entry_encode_scalar_vertex` |
| Journal stable insert (fresh key) | 7,323 | `bench_graph_journal_stable_insert_fresh_key` |
| Journal stable insert (reused key) | 9,065 | `bench_graph_journal_stable_insert_reused_key` |
| Router fresh durable dispatch gate, 1 KiB fingerprint | ~4.89 M | reproduction investigation |
| Router fresh durable dispatch gate, 1 MiB fingerprint | ~125.74 M | reproduction investigation |
| Envelope publication, 1 KiB / 1 MiB | ~13.18 M / ~258.33 M | reproduction investigation |

The Graph half of the new call is therefore about **0.3 %** of the Router-side gate read at 1 KiB. The
material cost of the option sits in the Router-side durable record work and the extra awaited round
trip, not in the journal transition.

## Call-count shape

One scalar mutation today, as implemented in `crates/router/src/gql.rs`:

1. `execute_plan_on_graph` - the mutation itself (update);
2. a journal read - `get_mutation_journal_entry`, or a batched `resolve_journal_entries` preflight
   (query);
3. `list_pending_label_stats_deltas` + `ack_label_stats_deltas_through` - the projection loop
   (query + update per batch).

Option A adds one awaited `retire_plan_execution` (update) on the terminal path, i.e. roughly a
**20-25 % call-count increase** on that path and one extra round trip, then nothing on replay: the call
is idempotent, so a retried record re-sends it rather than adding new calls.

Alternatives were sized and rejected as premature in this investigation:

- **Piggyback on `ack_label_stats_deltas_through`**: zero extra calls, but the ack call does not happen
  when the projection is already complete or `target_seq` is `None`, so a fallback call is still needed
  - more state, less saving.
- **Carry the previous mutation's retirement in the next `execute_plan`**: zero steady-state calls, but
  the last mutation's retirement is carried by nobody, so the recovery driver must guarantee it anyway.
- **Router-side fence with no Graph call**: zero calls, but a possibly-committed request becomes a
  returned error after the evidence window - a user-visible contract change, not a cost win.

## Growth, if retirement is never acknowledged

An unacknowledged retirement keeps its entry (`Active` is never age-evicted). The Router record that
owes the acknowledgement is itself durable and re-driven, so the entry is bounded by records that stop
making progress, not by traffic. The record is small: the fixed scalar entry encodes to roughly
47 bytes plus its B-tree key overhead (`encode_journal_v1`'s fixed fields: version 1, mutation id 8,
state 1, row count 8, validity 1, three u64 slots 24, next index 4, flags 1, placeholder 4, identity
and retirement tags), so a leak of N stuck records costs about N × 50 B, not N × record size.

The amortized sweep is unchanged: `gc_mutation_journal_at` still examines
`MUTATION_JOURNAL_GC_BUDGET = 2` entries per completed mutation, skipping while the journal is shorter
than `MUTATION_JOURNAL_GC_MIN_LEN = 16`. A larger journal therefore costs the same instructions per
mutation; it only lengthens the time one lap takes.

## What this does not establish

No end-to-end cost: the Router-side retirement-pending record write and the inter-canister call
overhead are not measured here, and the PocketIC fixture's instruction counter is the only place they
would be. No production capacity approval - the same status the earlier Router measurement carries.
No claim about the fleet-wide journal footprint under a retirement that is delayed for days.

## When the defect can fire, and how often

The cost question turned into a "when does it fire at all" question, so this section records the
conditions the code actually imposes. Everything below is read from the current tree; there is **no
production traffic** (Gleaph is pre-production), so no observed frequency exists and none is invented
here.

### Ordering that creates the window

A scalar mutation's commit is one update: the canonical segment applies its writes inside
`apply_canonical_mutation_segment` (`crates/graph/src/gql_run.rs:1230`), which then writes the
`Incomplete` journal entry at its end (`:1346`); the `Completed` entry follows after the outbox persist
and result materialisation (`:1717-1729`). So two failure shapes leave committed state with evidence
that is not `Completed`:

| Shape | Trap lands | Evidence state | When absence starts |
| --- | --- | --- | --- |
| **b (dangerous, no wait)** | after the canonical writes, before the `Incomplete` write (`:1346`) | **no entry at all** | immediately |
| **a (the gap's 9-day story)** | after `:1346`, before `:1717` | `Incomplete` entry | 9 days after recording (GC evicts it) |

A **lost reply** is *not* a trigger: if Graph committed it also wrote `Completed`, and every reader of
that evidence prefers it.

### Retries do not create the trigger, and they are abundant

The autonomous recovery timer (`crates/router/src/recovery.rs`) runs a bounded pass with a
`RECOVERY_FLOOR_DELAY = 2 s` floor, `RECOVERY_SCAN_BUDGET = 16` records per tick and a durable
round-robin cursor, armed from the mutation path and from `init`/`post_upgrade`. It never
re-dispatches canonical DML (documented in its module header and in `recover_mutation_record`); for a
scalar shard it only calls `capture_scalar_mutation_outcome`, which adopts a `Completed` entry and
otherwise records "canonical write not yet committed; retry the idempotent mutation to resume".
Across nine days that is on the order of 10^5 passes, so the window is **not** a shortage of retries -
it is the absence of evidence for the driver to adopt.

### The only thing that re-executes is a client retry

`execute_plan` is reached only through `scalar_dispatch_gate`
(`crates/router/src/facade/store/idempotency.rs:2147`), which answers `Completed(count)` when the
record holds a captured count and `Reconcile` when the shard is already complete; only a record with
neither - the lost-outcome state - falls through to `Dispatch`. That path is taken by a **client
retrying the same `ClientMutationKey`**, which the driver's own diagnostic tells the client to do.
Graph then finds no evidence and executes the plan again
(`run_wire_plans_last_read_row_count`, `crates/graph/src/gql_run.rs:1678-1686`).

### So the frequency is a product of three things, and the 9-day variant is the rarer one

1. **An unhandled trap inside the commit window** (not a lost reply, not a normal timeout). The window
   is the mutation tail after the canonical writes; the reproduction needed an *injected* fault 11 to
   enter it, and no organic occurrence is recorded anywhere in this repository.
2. **A client that retries that exact mutation** after the evidence is gone - immediately for shape (b),
   after nine days for shape (a). A client that gave up, or one that only polls `mutation_status`,
   never triggers it.
3. **State that makes re-execution differ** (a later property or eligibility change, or an originally
   zero-row plan that now writes) for the harm to be visible; without it the second execution is
   usually idempotent by value.

Shape (b) therefore deserves more attention than the nine-day narrative: it needs no wait, and its
window is the narrower half of the commit tail. That ordering is also the argument for a fix that does
not depend on retention at all - if the evidence is written **before** the canonical writes (an
`Incomplete` marker at admission rather than at the end of the segment), then absence would mean
"never admitted", which the Graph could refuse safely.
