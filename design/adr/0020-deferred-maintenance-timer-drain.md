# 0020. Timer-driven drain of the deferred LARA maintenance queue

Date: 2026-06-19
Status: accepted (implemented)
Last revised: 2026-06-19

## Context

The graph shard canister stores its adjacency in `ic-stable-lara`'s
`DeferredLaraGraph`, which owns a **stable** `MaintenanceQueue`. Destructive and
hot-path mutations (vertex/edge delete, hot edge insert, bulk-ingest finalize)
tombstone rows synchronously and **enqueue** physical reclamation work
(`CompactVertexEdgeSpan`, segment rebalance, delete-edge steps). Draining is
already budgeted: `MaintenanceBudget` caps work per call
(`max_instructions`, `max_work_items`, `max_segments`, `max_delete_edge_steps`)
and `MaintenanceReport` returns `work.remaining_queue_len` and
`instruction_budget_exhausted`. `GraphStore::run_timer_maintenance_tick()` runs
one budgeted pass at `GRAPH_TIMER_LARA_MAX_INSTRUCTIONS` (32 B, below the 40 B
per-message ceiling in `facade/ic_budget.rs`).

The tombstone bit is the highest-priority scan gate and is set synchronously, so
**queued maintenance is physical reclamation only — it never affects query
visibility**. Deferring it is therefore visibility-safe.

## Problem

The queue has **no execution path**. `run_timer_maintenance_tick()` is
unit-tested but never scheduled: the graph canister registers no timer, no
`#[heartbeat]`, and has only `#[init]` (no `#[post_upgrade]`). Consequently the
delete and finalize paths drain the queue **inline and unbounded**
(`unlimited_lara_maintenance_budget`, i.e. `max_instructions: 0`):

- `commit_detach_delete_vertex` / `commit_delete_detached_vertex` →
  `drain_deferred_maintenance()` (unlimited).
- `finalize_bulk_ingest` drains under a bounded budget, but leftover work has no
  later drain trigger, so a finalize that does not complete in one call strands
  `remaining_queue_len > 0` until the next unrelated mutation happens to drain it.

A large reclamation backlog (e.g. compaction after deleting a high-degree
vertex's spans) draining inside one message risks exceeding the per-message
instruction / 2 GiB stable read-write budget and trapping, rolling back an
otherwise valid mutation. There is no steady-state, bounded mechanism that makes
progress on the backlog independently of incoming traffic.

## Existing Architecture Assessment

- **Queue, budget, tick, and progress signal already exist** in `ic-stable-lara`
  and `GraphStore`. No new storage domain, queue, or maintenance abstraction is
  needed; the missing piece is purely an **execution trigger** in the graph
  canister surface.
- **`#[heartbeat]` is deprecated** on the Internet Computer; the supported
  mechanism for periodic canister work is the `ic-cdk-timers` crate
  (`set_timer`, `set_timer_interval`, `clear_timer`). Heartbeat is therefore not
  an option.
- **Inline unbounded drain cannot be made safe by tuning alone**: any fixed
  inline budget either traps on large backlogs (too high) or strands the backlog
  with no later trigger (too low). A trigger that runs *between* user messages is
  required.

The graph canister is the correct owner of this trigger: it already owns the
`DeferredLaraGraph`, the budgets (`facade/ic_budget.rs`), and the canister
lifecycle. Extending the graph canister surface preserves the existing boundary
rather than introducing a new maintenance subsystem.

## Alternatives

### A. Keep inline unbounded drain (status quo)
- Benefit: no new code.
- Drawback: unbounded; super-node delete-compaction can trap; stranded backlog.
- Verdict: rejected — this is the demonstrated problem.

### B. `#[heartbeat]` fixed-period drain
- Benefit: simple, always-on.
- Drawback: **deprecated API**; fires every round even when idle, paying cycles
  forever for an empty queue; fixed period cannot adapt to backlog pressure.
- Verdict: rejected (deprecated + idle cost).

### C. `ic-cdk-timers`, fixed `set_timer_interval`
- Benefit: supported API; simple.
- Drawback: a fixed interval still polls an empty queue forever (idle cycle
  cost), and a single period cannot be both responsive under backlog and cheap
  when idle.
- Verdict: rejected (idle cost; not adaptive).

### D. `ic-cdk-timers`, adaptive self-rescheduling one-shot + event-driven re-arm (chosen)
- A single one-shot `set_timer` whose next delay is chosen from the just-finished
  tick's `MaintenanceReport`; **no timer is scheduled while the queue is empty**.
  Mutations that enqueue work **arm** the timer if it is not already armed.
- Benefit: bounded per tick; zero idle cost; responsiveness scales with backlog
  pressure; uses the supported crate.
- Drawback: must thread an "arm if needed" call through enqueue sites and track a
  single `TimerId`/armed flag.
- Verdict: chosen.

## Decision

Add a graph-canister **maintenance-timer module** built on `ic-cdk-timers` that
drives `GraphStore::run_timer_maintenance_tick()` as an **adaptive,
self-rescheduling one-shot timer**, with **event-driven re-arm on enqueue** and
**no timer while the queue is empty**.

### Arming

- A single timer is tracked via a thread-local `TimerId` / armed flag; the module
  never schedules overlapping timers.
- Arm (if not already armed) from:
  - `#[init]` and a new `#[post_upgrade]` when the (stable) queue is non-empty —
    timers do not survive upgrades, so re-arm is required after every upgrade.
  - Every mutation path that enqueues maintenance work (vertex/edge delete, hot
    edge insert, `enqueue_bulk_ingest_finalize` / `finalize_bulk_ingest`).

### Tick and reschedule policy

Each tick runs one budgeted `run_timer_maintenance_tick()` pass, then chooses the
next action from the report. The interval is **not** fixed; it is derived from
backlog and pressure:

| Post-tick state | Next action | Rationale |
|-----------------|-------------|-----------|
| `remaining_queue_len == 0` | **do not reschedule**; clear armed flag | zero idle cost; re-armed on next enqueue |
| `remaining > 0 && instruction_budget_exhausted` | reschedule at **floor delay** | budget was full → drain aggressively while yielding between messages |
| `remaining > 0 && !exhausted` | reschedule at **relaxed delay** | small tail → finish without competing hard with user traffic |

Interval bounds are derived, not guessed:

- **Floor delay** comes from a single-threaded fairness budget: a tick blocks
  user update calls for up to one bounded pass (~`GRAPH_TIMER_LARA_MAX_INSTRUCTIONS`),
  so the floor is set so maintenance consumes at most a target fraction of
  execution time under sustained load. `ic-cdk-timers` resolution is
  block-rate, so the practical floor is on the order of ~1 s (sub-second delays
  are not meaningful).
- **Per-tick work** is owned by `MaintenanceBudget` (tune the budget to cap
  per-tick latency); the **interval** only governs how often that cost is paid.
  Expected drain time ≈ `remaining_queue_len / work_per_tick × interval`.

Initial constants (floor ≈ 1 s, relaxed ≈ 5 s) are starting points to tune with
canbench and observed cycle/latency cost; they live beside the existing budgets
in `facade/ic_budget.rs`.

### Inline drain change

Switch the destructive/hot mutation paths from `unlimited_lara_maintenance_budget`
to a **bounded** inline drain (the timer budget) plus an arm call, so a single
mutation message can no longer trap on a large reclamation backlog; the timer
finishes the remainder.

### Explicitly out of scope

The **logical** O(degree) edge removal inside `delete_vertex_deferred`
(the `while has_incident_edges` loop) and incident-sidecar clearing are
synchronous because correct reads require neighbours' back-edges to be tombstoned
before the statement returns. Making **super-node DETACH DELETE itself**
resumable across messages (a "pending-delete" vertex set with read-time
filtering) is a separate, larger decision and is deferred to a future ADR. This
ADR bounds and schedules only the **physical reclamation queue**.

## Consequences

### Positive
- Removes the unbounded inline drain trap risk on delete/finalize.
- Steady-state progress on reclamation independent of incoming traffic.
- Zero cycle cost when the queue is empty (no idle polling).
- Reuses existing queue/budget/report machinery; no new storage or maintenance
  domain; the graph canister keeps ownership of its own maintenance.
- Uses the supported `ic-cdk-timers` API instead of deprecated `#[heartbeat]`.

### Trade-offs
- Enqueue sites must call "arm if needed"; a single `TimerId`/armed flag must be
  tracked in the canister (non-stable; re-armed on `post_upgrade`).
- Reclamation becomes eventually-consistent: between a mutation and the timer
  ticks, `stored_slots` may exceed `degree` (tombstones awaiting compaction).
  This is already true transiently today and is visibility-neutral.
- A new `#[post_upgrade]` hook is introduced on the graph canister.

## Migration

1. Add the `ic-cdk-timers` dependency (workspace + graph crate).
2. Add the maintenance-timer module to the graph canister surface; arm from
   `#[init]` and a new `#[post_upgrade]` when the queue is non-empty.
3. Add "arm if needed" calls to the enqueue paths (delete, hot insert, finalize).
4. Switch delete/finalize inline drains from `unlimited_*` to the bounded timer
   budget.
5. No stable-format change: the queue is already stable; the `TimerId`/armed flag
   is in-heap and rebuilt on upgrade.

## Design Documentation Impact

| Document | Update | Status |
|----------|--------|--------|
| [adr/README.md](README.md) | Index ADR 0020 | done |
| [storage/bulk-ingest-finalize.md](../storage/bulk-ingest-finalize.md) | Delete/finalize use the bounded timer budget; leftover work drained by the maintenance timer | done |
| [architecture/overview.md](../architecture/overview.md) | Graph canister maintenance-timer execution path | done |
| [index/capacity-planning.md](../index/capacity-planning.md) | Note timer-driven bounded drain of the deferred maintenance queue | deferred (no maintenance section yet) |
| Future ADR | Resumable super-node `DETACH DELETE` (logical edge removal) | deferred |

## Implementation (2026-06-19)

Implemented in the graph canister:

- `facade/maintenance_timer.rs` — single tracked `TimerId`, `arm_if_needed()`
  (self-guarding on queue length, no-op off-canister), and an adaptive
  self-rescheduling one-shot whose successor delay comes from `next_delay()`.
- `facade/ic_budget.rs` — `delete_maintenance_budget()` (timer budget on wasm,
  unlimited natively) and the floor/relaxed `Duration` constants.
- Arm sites: `#[init]`, new `#[post_upgrade]` (also re-installs the rkyv
  extension decode hook, which previously was not re-run after upgrade), and the
  enqueue paths `drain_deferred_maintenance` (vertex/edge delete),
  `run_post_edge_insert_maintenance`, `enqueue_bulk_ingest_finalize`, and
  `run_bulk_ingest_finalize_drain`.
- Delete/finalize inline drains switched from `unlimited_*` to the bounded timer
  budget on canisters (native still drains fully for deterministic tests).
- `ic-cdk-timers = "1.0"` added (workspace + graph crate); `set_timer` in 1.0
  takes a future. The tick is now a genuine `async` pass (ADR 0023 P2): it awaits
  the router's `indexed_property_catalog` query, installs that catalog ephemerally
  for the pass so compaction's `EdgeSlotMove` observers enqueue index posting
  re-keys, runs the budgeted compaction, then flushes the pending posting queues
  in-tick so the index converges with the re-keyed store within the same tick.
  Because the pass now spans awaits, a `MAINTENANCE_RUNNING` flag guards
  `arm_if_needed` from scheduling an overlapping duplicate pass, and the tick
  defers (retries at the floor delay) when the router catalog is unavailable
  rather than re-keying the store without the index.
