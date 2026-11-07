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
