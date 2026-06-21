# 0023. Federated index/store consistency across upgrade and compaction: remove the shard index registry; router-sourced ephemeral catalog; precise emit; failure-only durable repair

Date: 2026-06-20
Status: accepted
Last revised: 2026-06-21
Anchor timestamp: 2026-06-21 05:36:08 UTC +0000

## Revision history

| Date | Change |
|------|--------|
| 2026-06-21 | ADR 0029 consistency-contract sync: distinguish shard-local canonical mutation completion from cross-canister projection completion; a repair-journaled flush may leave the distributed mutation `ProjectionPending`. |
| 2026-06-20 | Proposed; follow-up to [0009](0009-edge-property-index-and-index-ddl.md) (Phase A shard index registry) and [0020](0020-deferred-maintenance-timer-drain.md) (timer drain). |
| 2026-06-20 | `adr-review` pass (APPROVE WITH CHANGES): added P7 + D6 (`DROP INDEX` does not purge postings today; make purge real, load-bearing for D4); framed repair journal as durable extension of existing pending queues; marked async tick as amending ADR 0020; prefer reusing the existing router indexed-catalog surface for the timer fetch. |
| 2026-06-20 | Accepted; policy frozen pending implementation per §Migration (phases 1–6). |
| 2026-06-20 | Phases 1–2 implemented: removed shard `registry.rs`; added router-sourced `IndexedPropertyCatalog` on `ExecutePlanArgs` + backfill requests, consulted via ephemeral `catalog_context`. Implementation carries the full per-graph catalog (safe superset of label-scoped). PocketIC P1 repro (`adr0023_index_store_consistency`) now green. Phases 3–6 (async tick/in-tick flush, durable repair journal, `DROP INDEX` purge, INV oracle) pending. |
| 2026-06-20 | Phase 3 implemented (P2): maintenance tick is now `async` (`facade/maintenance_timer.rs`) — it fetches the router catalog via the new shard-callable `indexed_property_catalog` query, installs it ephemerally for the pass so compaction's `EdgeSlotMove` observers enqueue posting re-keys, runs the budgeted compaction, then flushes `pending`/`edge_pending`/`label_pending` in-tick. Defers (floor-delay retry) when the router catalog is unavailable instead of re-keying the store blind; a `MAINTENANCE_RUNNING` flag prevents duplicate overlapping passes across the tick's awaits. Phases 4–6 pending. |
| 2026-06-20 | Phase 4a implemented (P3 part): durable repair journal (`facade/stable/repair_journal.rs`, new stable `MemoryId 41`). Flush failure with successful compensation now persists the whole batch to stable memory (all three posting kinds) rather than the volatile queue, so the store-ahead delta survives upgrade/trap; `arm_if_needed` arms on a non-empty journal, `post_upgrade` replays it, and the async tick re-applies entries in-tick (`index/repair_journal.rs`) and reschedules at the relaxed delay while non-empty. Stable-memory inventory + typed layout registry updated (graph: 42 regions). Phase 4b (dirty marker + catalog-driven rebuild + retire compensation trap), 5, 6 pending. |
| 2026-06-20 | Phase 4 completed (P4): the compensation-failure trap is retired across all three flush paths (`index/pending.rs`, `edge_pending.rs`, `label_pending.rs`) — the branch now journals the full batch and returns an error instead of `ic_cdk::trap`, since idempotent journal re-application converges the index to the store regardless of partial compensation state (store becomes the source of truth; no whole-message rollback). Refinement: the durable `index-dirty` marker + the catalog-driven *scan* rebuild from the original D5 wording are re-sequenced into phase 5 (they need D6's index-side posting-purge primitive and share its machinery); the journal alone closes P4. Phases 5–6 pending. |
| 2026-06-20 | Phase 6 (P5): INV test oracle added. `graph/src/index/inv_oracle.rs` drives vertex/edge/label posting ops through the real flush paths (incl. an edge compaction re-key) with a mid-batch index failure, then asserts `recorded postings == projection(store over indexed properties)` exactly after compensation + repair-journal drain (verification items 1–2). PocketIC P1 is now green for edges too (`post_upgrade_indexed_edge_write_stays_consistent_with_store`). Remaining gap: the wasm-only timer-driven compaction+upgrade e2e (item 3) needs a PocketIC time-advance/timer-fire harness and is tracked as a follow-up; relocation slot-invariance (item 4) stays covered by existing ic-stable-lara tests. |
| 2026-06-20 | Phase 6 completed (P5, item 3): added the wasm-only timer-driven compaction+upgrade e2e (`timer_compaction_after_upgrade_rekeys_edge_postings_consistently`). New PocketIC time-advance/timer-fire harness (`drain_maintenance_via_timer`) plus feature-gated (`pocket-ic-e2e`) graph-shard admin hooks — `e2e_enqueue_forward_compaction` (enqueue `CompactVertexEdgeSpan` + arm timer, no inline drain), `e2e_delete_directed_edge_with_property`, `e2e_maintenance_queue_len`. The enqueue-only hook is required because production delete/insert budgets fully reclaim inline at test scale. The test deletes a slot-0 edge, upgrades the shard (asserting the queued compaction survives the stable queue), then fires the re-armed timer; the async tick runs the real slot-moving LARA compaction and re-keys postings in-tick, after which index-served lookups still resolve each weight to the correct target. Verification plan item 3 is now done. |
| 2026-06-20 | Item 3 widened: the timer-driven compaction+upgrade e2e now also asserts the **alias/property sidecars** that ride the same `EdgeSlotMove` as the index postings. Forward compaction re-keys the edge-alias canonical target (`move_canonical_target`) and physically moves the property sidecar (`EDGE_PROPERTIES`); the test reads each surviving edge's weight through the reverse in-edge → alias → canonical path via a new feature-gated query hook `e2e_reverse_resolved_edge_property`, which resolves at the moved slot and catches a stale alias (wrong sibling weight) or an un-moved sidecar (missing value) that the forward index lookup alone cannot. |
| 2026-06-20 | Phase 5 completed (P7 / D6): `DROP INDEX` now purges postings. Added a bounded, resumable index-side primitive `admin_purge_property_postings` (`graph-index`, `facade/store/posting_purge.rs`) with kernel wire types (`federation/index_posting_purge.rs`), scoped to a contiguous `property_id` range (vertex: whole range; edge: filtered by catalog `label_id` → `(property_id, label_id)`). `router::drop_index` drives the purge fan-out across `graph_index_lookup_targets` only when no remaining index needs the postings (`is_property_registered` for vertex; new `edge_index_uses_property_label` for edge). Stateless (no new stable region). PocketIC `drop_index_purges_postings_from_graph_index` is green. The `index-dirty` marker + catalog-driven scan-rebuild (re-sequenced from phase 4) are dropped as unneeded — the phase-4 journal already provides idempotent repair. Phase 6 (INV oracle) pending. |

## Context

ADR [0009](0009-edge-property-index-and-index-ddl.md) places property equality
postings on **graph-index** and introduces a **shard-local index registry**
(`crates/graph/src/index/registry.rs`) as an opt-in DML gate: graph DML enqueues
postings only for properties the registry marks indexed, avoiding a synchronous
cross-canister definition lookup on every write. The **router is SSOT for index
definitions** (named indexes, `ROUTER_INDEXED_PROPERTY_SET`); **graph-index is
SSOT for posting data**; **the graph shard is SSOT for property data**.

ADR [0020](0020-deferred-maintenance-timer-drain.md) added a timer that drains
the deferred LARA maintenance queue. That maintenance includes **compaction**,
which renumbers edge slot indices and therefore must re-key edge property
postings.

### Invariant under review (INV)

> The set of postings on graph-index equals the projection of the shard property
> store over **indexed** properties (no missing postings, no orphan postings),
> at the completion boundary of each operation, across normal mutations,
> compaction, and the canister upgrade boundary.

Cross-canister atomicity is impossible on the IC (shard store and index live in
different canisters; an update is split by `await`/commit points). The achievable
guarantee is therefore: **a synchronous projection completion satisfies INV; a
repair-journaled flush may complete the canonical mutation while projection is pending,
and the durable store-ahead delta eventually restores INV.** Canonical mutation success
must not be interpreted as a cross-canister freshness barrier; see
[ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md).

### Established facts that scope this ADR

- **Only compaction changes `slot_index`.** `EdgeSlotMove` is produced solely by
  `crates/ic-stable-lara/src/labeled/graph/compact.rs`
  (`labeled/graph.rs:85-99`). Physical leaf/segment **relocation/rebalance**
  (`relocate_labeled_leaf_physical_block`) move physical addresses but preserve
  the bucket-relative `slot_index`. Posting keys
  `(property_id, value, label_id, shard_id, owner_vertex_id, slot_index)` are
  therefore **invariant under relocation/rebalance**; the only maintenance
  operation that touches postings is compaction.
- **Vertex label membership is an always-on index** (not opt-in). There is no
  label registration in the registry, and `record_vertex_label_set`
  (`index/label_pending.rs:31-53`) emits label postings unconditionally. Only
  **property** indexes are opt-in / catalog-gated.
- **The shard already calls the router** (`index/federation_routing.rs:33-48`,
  `list_shards_for_graph` via `router_call::call_router1`), so a shard→router
  dependency edge already exists; the shard is not autonomous from the router.
- **graph-index `remove` is idempotent** (`facade/store/edge_postings.rs:60-63`
  `postings.remove(&key)`; missing key is a no-op, and value-key length is not
  validated on remove — designed for legacy cleanup).
- **`DROP INDEX` does not purge postings today.** `drop_index`
  (`crates/router/src/index_catalog.rs:112-143`) removes the named index from the
  router catalog and fans out `unregister_indexed_*` (which only clears the shard
  registry); it makes **no** graph-index posting-removal call. Postings for a
  dropped index therefore orphan — a pre-existing gap (P7) that this ADR's
  orphan-cleanup delegation depends on.

## Problems

| # | Severity | Symptom | Root cause / evidence |
|---|----------|---------|------------------------|
| P1 | Critical | After upgrade, new writes emit no postings and timer compaction skips re-key → permanent stale/orphan postings | Registry is `thread_local` (`registry.rs:16-20`) and `post_upgrade` does not rebuild it (`canister/handlers.rs:73-76`); empty registry makes `dispatch_property_index_ops` early-return (`property/index_dispatch.rs:9-19`) |
| P2 | High | Timer-driven compaction's posting re-key is not flushed in-tick; relies on a later request flush | `on_tick`/`run_timer_maintenance_tick` are synchronous and never call `flush_pending` (`facade/maintenance_timer.rs:64-91`, `facade/store/maintenance.rs:48-52`); request path flushes at `gql_run.rs:262-264` |
| P3 | High | Failed-flush re-queue and timer-enqueued moves are lost on upgrade/restart/trap | Pending queues are `thread_local` (`index/pending.rs:57-58`, `index/edge_pending.rs:28-29`, `index/label_pending.rs:16-18`) |
| P4 | Medium | Compensation failure traps with acknowledged unrepairable divergence | `index/pending.rs:169-173` (comment self-admits) |
| P5 | Medium | INV has no test oracle; wasm-only timer path uncovered — **oracle added (phase 6); wasm timer-compaction+upgrade e2e added (phase 6 follow-up)** | Non-wasm builds drain maintenance inline and unbounded (`facade/store/maintenance.rs:62-63`), masking the wasm timer gap. `graph/src/index/inv_oracle.rs` asserts `postings == projection(store)` incl. failure recovery + compaction re-key; `timer_compaction_after_upgrade_rekeys_edge_postings_consistently` now drives the wasm async timer tick over a real slot-moving compaction across the upgrade boundary in PocketIC |
| P6 | Design | Indexed-ness decision sits in a shard-local volatile gate | `registry.rs`; decision authority should be the router (definitions SSOT) |
| P7 | High | `DROP INDEX` leaves orphan postings on graph-index — **fixed (D6, phase 5)** | `router::drop_index` now fans the bounded `admin_purge_property_postings` primitive out across the graph's index canisters once the postings are unreferenced |

These share one disease: **volatile derived state (registry, pending queues)
cannot cross the upgrade boundary or the router-less timer context.**

## Decision

### D1. Indexed-ness decision is router-sourced; remove the shard registry (§5-1 = option C)

Delete `crates/graph/src/index/registry.rs`. The shard keeps **no durable or
persistent catalog**. The indexed-property catalog is sourced from the router
(its definitions SSOT) per operation:

- **Request paths** (INSERT, property SET/REMOVE, DELETE, vertex label changes):
  the router includes a **label-scoped indexed-property set** in
  `ExecutePlanArgs`. Scoping to the request's labels is sufficient because inline
  compaction only moves edges within the same `(owner, label)` bucket as the
  mutation, so the same label slice that gates named-property writes also gates
  the inline-compaction re-key.
- **Timer path**: the async tick fetches the **full** indexed catalog from the
  router **once per drain** and reuses it for the whole pass.

This extends an existing shard→router dependency rather than introducing a new
one, and keeps **zero persisted derived state**, so P1/P6 disappear structurally.

### D2. Async maintenance tick; in-tick flush (§5-4)

**This amends ADR [0020](0020-deferred-maintenance-timer-drain.md)**, which
deliberately kept the tick body synchronous (wrapped in a non-suspending async
block). Make the maintenance tick genuinely `async`. Order within a drain:

1. `await router.get_indexed_catalog()` — **before** any LARA mutation, so this
   commit point holds no partial state.
2. Run the budgeted compaction pass, re-keying postings for indexed properties
   only.
3. `await flush_pending(...)` for all posting kinds, **in-tick**.

**Inviolable rule:** if the catalog cannot be fetched (router unavailable /
upgrading), the drain is **deferred** (re-arm at floor delay), never run with a
missing/empty catalog. Skipping re-key would create stale postings; deferral is
safe because maintenance is deferrable by construction (ADR 0020). This closes
P2.

### D3. Ephemeral per-operation catalog context (§D2 = ephemeral)

The router-sourced catalog snapshot is held in an **ephemeral per-operation
context** (set at operation/drain start, cleared at end, **never persisted**),
consulted by `dispatch_property_index_ops` and the posting enqueue sites. This is
**not** a reintroduction of the registry anti-pattern: it is repopulated fresh
from the router on every operation and has a single-message lifetime, so it can
never be stale across an upgrade. Chosen over threading the catalog through every
`commit_*` signature (minimal blast radius).

### D4. Precise emit; no blind ops (§5-2 = option a)

With the catalog available in every context, emit postings **precisely**:

| Path | Posting kind | Catalog | Emit |
|------|--------------|---------|------|
| Vertex/edge property SET (incl. INSERT) / value change | property | required | indexed → `remove(old)` + `insert(new)`; skip non-encodable values |
| Vertex/edge property REMOVE | property | required | indexed → `remove(old)` |
| Vertex DELETE (properties) | property | required | scan stored properties × catalog → `remove` indexed only |
| Edge DELETE | property | required | scan stored properties × catalog → `remove` indexed only |
| COMPACTION (inline + timer) | property | required | indexed → `remove(old_slot)` + `insert(new_slot)`; skip non-encodable |
| Vertex label INSERT / change / DELETE | label | **none (always-on)** | emit all changes (`remove`/`insert`) |

No blind idempotent ops and no new graph-index `move-if-exists` primitive.
**Orphan posting cleanup is the responsibility of `DROP INDEX` (purge)**, not of
compaction or delete — see D6, which makes that purge real (it does not exist
today, P7).

### D6. `DROP INDEX` purges postings (closes P7; load-bearing for D4) — implemented

`DROP INDEX` must remove the dropped property's postings from graph-index, not
only clear the catalog/registry. Because precise emit (D4) deliberately does not
clean orphans and the per-drain snapshot can briefly lag a concurrent drop, this
purge is the **single owner** of orphan removal and a load-bearing prerequisite
for D4's correctness.

Implemented as a **bounded, resumable, scoped purge** mirroring the shard-detach
primitive (`graph-kernel/src/federation/index_posting_purge.rs`):

- **Index-side primitive** (`graph-index`): a router-guarded
  `admin_purge_property_postings(kind, property_id, label_id, resume)` deletes a
  bounded slice of one posting set per call and returns a resume cursor until
  `done` (`facade/store/posting_purge.rs`). Posting keys order `property_id`
  first, so each scope is a contiguous `property_id` range: **vertex** keys carry
  no label (purge the whole `property_id` range); **edge** keys carry the catalog
  `label_id` (direction stripped), so the purge filters by `label_id` →
  `(property_id, label_id)` scope.
- **Router orchestration** (`router/src/index_catalog.rs::drop_index`): after
  removing the catalog entry, the router purges only when no remaining index
  still needs the postings — vertex postings are shared across a property's
  indexes (purge once `is_property_registered` is false); edge postings are
  per-`(property, label)` (purge once `edge_index_uses_property_label` is false).
  The purge fans out to every index canister backing the graph's live shards
  (`graph_index_lookup_targets`), driving each resume loop to `done`
  (`index_sync::admin_purge_property_postings`).

The purge is stateless (no new stable region). PocketIC
`drop_index_purges_postings_from_graph_index` asserts zero orphan postings
remain after drop.

**Re-sequenced out of phase 5:** the `index-dirty` marker + catalog-driven full
*scan* rebuild (folded in from D5/phase 4) are **not** implemented. The durable
repair journal (D5, phase 4) already provides idempotent eventual-consistency
repair on the failure path, so the scan rebuild is an unneeded escalation; the
purge primitive built here is the machinery a future rebuild would reuse if one
is ever justified.

### D5. Failure-only durable repair journal + catalog-driven rebuild backstop (§5-3 = option a)

This **extends the existing pending-posting queues** (`index/pending.rs`,
`edge_pending.rs`, `label_pending.rs`) from volatile `thread_local` to durable
(stable-memory) — it is not a new maintenance subsystem. The happy path performs
**no durable journaling** (in-tick flush success persists nothing — the hot write
path stays lean). On failure:

- **Flush failure (compensation succeeded):** persist the failed batch to a
  durable (stable-memory) repair journal, replayed on `post_upgrade` and drained
  by the maintenance driver; cleared on successful re-flush. Surgical retry.
- **Compensation failure (P4):** **do not trap.** *(Implemented, refined.)* The
  failure branch now journals the full batch (as above) and returns an error;
  idempotent journal re-application converges the index to the store regardless of
  the partial compensation state, which is what retires the trap. The originally
  proposed durable `index-dirty` marker + **catalog-driven deterministic rebuild**
  (scan the shard store and reconstruct postings, `index = f(store)`, reusing the
  backfill machinery driven by the router catalog) is retained as the **escalation
  backstop** for divergence the journal cannot converge, and is re-sequenced into
  **phase 5** because it needs the same index-side posting-purge primitive that D6
  introduces.

This makes the store-ahead/index-behind delta always durably recorded and
eventually repaired, which is what makes INV's eventual-consistency form
provable.

## Consequences

### Positive

- P1/P6 removed structurally: no persisted derived catalog → no post-upgrade
  staleness class; router is the single catalog SSOT.
- P2 removed: timer compaction flushes in-tick.
- P3/P4 addressed: durable repair journal + rebuild backstop; the terminal trap
  is retired.
- Bandwidth-optimal: precise emit sends only indexed postings (no non-indexed
  cross-canister no-ops).
- Relocation/rebalance provably need no posting work (slot-invariant).

### Trade-offs

- The maintenance tick becomes `async` (a deliberate change from ADR 0020's
  synchronous tick), adding `await`/commit points to reclamation; the catalog
  fetch is placed before mutations to keep atomicity clean.
- Maintenance progress couples to router availability (deferred, not broken, on
  unavailability). The router takes one catalog call per drain per shard.
- Catalog snapshot is fixed per drain; a concurrent `CREATE/DROP INDEX` during a
  long drain uses a slightly stale snapshot, reconciled by that DDL's
  backfill/purge.
- A new durable repair-journal / dirty-marker stable region is added on the
  graph shard.

## Alternatives considered

### Decision placement (D1)

- **A. Durable shard catalog (router push, persisted, rebuilt on upgrade).**
  Maximal autonomy, synchronous tick OK, fast local lookup; rejected because it
  creates **N durable derived copies** (one per shard) — the worst fit for the
  single-source-of-truth principle — with DDL-sync and rebuild burden.
- **B. graph-index holds the catalog and filters; shard sends blindly.** No new
  dependency edge, synchronous tick OK, one durable copy at the index; rejected
  as primary because the shard then ships **non-indexed property data** on every
  delete/compaction move (wire/cycles cost). Retained as a fallback if router
  per-drain coupling proves problematic.

### Emit precision (D4)

- **Blind idempotent (`move-if-exists` + blind remove).** Correct and robust to
  catalog drift / orphans, but turns local no-ops into cross-canister no-ops and
  needs a new index primitive; rejected because C makes precise filtering free.
  May be added later only if drift becomes a real problem.

### Repair durability (D5)

- **Durable dirty-marker → full rebuild on any failure.** One mechanism, but a
  full-shard rebuild is an operationally risky hammer for transient failures.
- **Per-op durable journal.** Strongest durability but persists on the hot write
  path; rejected.

## Verification plan (addresses P5)

1. **INV property test** — **done (native oracle, phase 6).**
   `graph/src/index/inv_oracle.rs` drives vertex / edge / label posting ops
   through the real `pending` / `edge_pending` / `label_pending` flush paths,
   including an edge **compaction re-key** (slot delete + slot move, the only op
   that changes `slot_index`), then asserts `recorded postings ==
   projection(store over indexed properties)` exactly via a `RecordingIndex` that
   models graph-index set semantics. (Real LARA compaction is exercised
   end-to-end through the timer path separately; see item 3.)
2. **Failure injection** — **done (phase 6).** The same oracle injects a
   mid-batch index failure: it asserts batch-atomic compensation (rollback to the
   pre-batch state) and that draining the durable repair journal converges the
   index to INV; the phase-4 `repair_journal.rs` tests cover the bounded/partial
   drain.
3. **PocketIC red repro then green** — **done.** P1 (post-upgrade indexed write)
   is green for both vertex and edge in `adr0023_index_store_consistency`
   (`post_upgrade_indexed_write_stays_consistent_with_store`,
   `post_upgrade_indexed_edge_write_stays_consistent_with_store`). The **timer-
   driven compaction + upgrade** e2e (the wasm-only async-tick path) is now
   covered by `timer_compaction_after_upgrade_rekeys_edge_postings_consistently`:
   it seeds three indexed `KNOWS` edges (slots 0/1/2), deletes slot 0 to leave a
   tombstone, enqueues a `CompactVertexEdgeSpan` **without** an inline drain,
   upgrades the graph shard (asserting the work survives in the stable queue),
   then advances PocketIC time so the re-armed maintenance timer fires. The async
   tick fetches the router catalog, runs the real LARA compaction (moving the
   surviving edges' `slot_index`), and flushes the re-keyed postings in-tick;
   index-served equality lookups then still resolve each weight to the correct
   target. The same `EdgeSlotMove` also re-keys the **edge-alias canonical
   target** (`move_canonical_target`) and physically moves the **property
   sidecar** (`EDGE_PROPERTIES`); the test widens its assertion to read each
   surviving edge's weight through the reverse in-edge → alias → canonical path
   (the `e2e_reverse_resolved_edge_property` hook), which resolves at the moved
   slot and so catches a stale alias (wrong sibling weight) or an un-moved
   sidecar (missing value) that the forward index lookup alone cannot. The
   reclaimable-state setup and the time-advance / timer-fire harness
   (`drain_maintenance_via_timer`) live in the `pocket-ic-tests` crate, driven by
   feature-gated (`pocket-ic-e2e`) admin hooks on the graph shard
   (`e2e_enqueue_forward_compaction`, `e2e_delete_directed_edge_with_property`,
   `e2e_maintenance_queue_len`, `e2e_reverse_resolved_edge_property`); the
   enqueue-only hook is required because the production delete/insert budgets
   fully reclaim inline at test scale, so the timer never arms through the normal
   path.
4. **Relocation/rebalance regression** — slot-invariance under physical
   relocation/rebalance is an established fact backed by the existing
   `ic-stable-lara` tests (posting keys carry the bucket-relative `slot_index`,
   unchanged by relocation); no new posting-set test added.
5. **`DROP INDEX` purge (D6)** — **done (phase 5).** `drop_index_purges_postings_from_graph_index`
   asserts zero orphan postings remain after drop.

## Migration / phases

1. **(implemented)** Extend `ExecutePlanArgs` with the indexed-property catalog
   (`indexed_properties`) and the backfill requests
   (`VertexPropertyBackfillRequest` / `EdgePropertyBackfillRequest`); add the
   ephemeral per-operation catalog context (`graph/src/index/catalog_context.rs`).
   The router builds the catalog from its existing per-graph stats
   (`RouterGraphStats::to_indexed_property_catalog`). Implementation passes the
   **full per-graph catalog** (a safe superset of the label-scoped set; label
   scoping remains a future wire-size optimization). For the timer fetch, **reuse
   the existing router indexed catalog** rather than a new endpoint where possible.
2. **(implemented)** Switch `dispatch_property_index_ops`, the edge-property store
   scans, and backfill from the registry to the ephemeral context; install the
   context at `execute_plan_impl` / e2e / backfill entrypoints; delete
   `registry.rs` and the `register/unregister_indexed_*` fan-out, endpoints, and
   graph-client calls.
3. **(implemented)** Make the maintenance tick `async`
   (`facade/maintenance_timer.rs`): fetch the router-sourced catalog per pass via
   the new `indexed_property_catalog` router query, install it ephemerally for the
   pass (so compaction's `EdgeSlotMove` observers enqueue posting re-keys), run
   the budgeted compaction, then flush `pending` / `edge_pending` / `label_pending`
   in the same tick. Defer-on-unavailable: if the router catalog cannot be
   fetched, skip the pass and retry at the floor delay rather than re-key the store
   blind. A `MAINTENANCE_RUNNING` flag prevents a concurrent enqueue from arming a
   duplicate pass across the tick's awaits. (Amends ADR 0020.) The label-scoped
   per-graph reuse note from phase 1 is satisfied by reusing `RouterGraphStats`;
   the timer fetch needed a shard-callable endpoint, so a dedicated
   `indexed_property_catalog` query was added (the existing catalog had no
   shard-facing read).
4. **(implemented)** Durable repair journal
   (`facade/stable/repair_journal.rs`, `MemoryId 41`): on flush failure with
   successful compensation, the whole batch is persisted to stable memory instead
   of the volatile queue (all three posting kinds), `arm_if_needed` arms on a
   non-empty journal, `post_upgrade` replays it (the timer re-arms and drains),
   and the async tick re-applies entries in-tick (`index/repair_journal.rs`),
   rescheduling at the relaxed delay while the journal is non-empty. The
   **compensation-failure trap (P4) is retired**: the failure branch now journals
   the full batch and returns an error instead of `ic_cdk::trap`, because
   journal re-application is idempotent (graph-index `remove` is a no-op on a
   missing key; `insert` sets membership) and the journaled batch fully specifies
   the desired delta, so re-application converges the index to the store
   regardless of how far compensation got. This makes the store the source of
   truth on the failure path (no whole-message rollback) — the ADR's intended
   store-ahead/index-repaired model.
   **Refinement vs the original D5 wording:** D5 proposed an `index-dirty` marker
   + a separate catalog-driven *scan* rebuild as the compensation-failure
   backstop. Implementation found the journal's idempotent re-application already
   provides the eventual-consistency repair that retires the trap, so the
   scan-rebuild is **not** required for P4 closure. The scan rebuild also needs an
   index-side "purge postings for `(shard, kind, label, property)`" primitive that
   does not exist yet and is exactly what D6 introduces — so the dirty marker +
   catalog-driven rebuild are **re-sequenced into phase 5 alongside D6**, where
   they share that purge/rebuild machinery (D5/D6 explicitly share it). This is a
   simplicity-driven refinement, not a change to the decision's correctness goal.
5. **Implement `DROP INDEX` posting purge (D6)** — done. Added the bounded,
   resumable index-side purge primitive (`admin_purge_property_postings`) and the
   router fan-out from `drop_index`, scoped to `property_id` (vertex) or
   `(property_id, label_id)` (edge); the PocketIC repro asserts no orphan postings
   remain after drop. The `index-dirty` marker + catalog-driven full scan-rebuild
   (folded in from phase 4) are **dropped as unneeded** — the phase-4 repair
   journal already provides idempotent eventual-consistency repair; the purge
   primitive is the reusable machinery a future rebuild would need.
6. **Land the verification suite (P5)** — done. Added the native INV oracle
   (`graph/src/index/inv_oracle.rs`: postings == store projection, incl. failure
   recovery + compaction re-key), the edge variant of the PocketIC P1 repro, and
   the wasm-only timer-driven compaction+upgrade e2e
   (`timer_compaction_after_upgrade_rekeys_edge_postings_consistently`) backed by
   a PocketIC time-advance/timer-fire harness (`drain_maintenance_via_timer`) and
   feature-gated graph-shard admin hooks. Relocation slot-invariance stays covered
   by existing ic-stable-lara tests.

No stable graph/index posting wire change; the new repair journal is additive
stable state.

## Interaction with the mutation journal (ADR 0024)

A flush failure that is journaled here is **not** a mutation failure: the store mutation
and emitted label-stats deltas are already durable, and the index converges via the
maintenance timer. ADR 0024 uses this guarantee to record the ADR 0015 mutation journal
`Completed` on a repair-journaled (deferred) flush — surfaced as
`PlanQueryError::IndexFlushDeferred` from the three `flush_pending` functions — instead of
leaving the mutation wedged `Incomplete`. See
[adr/0024-mutation-journal-completion-vs-index-flush.md](0024-mutation-journal-completion-vs-index-flush.md).

In the lifecycle defined by ADR 0029, the graph-journal `Completed` state proves a
replayable shard-local canonical outcome. When repair work remains, the distributed
mutation is still `ProjectionPending`; only the Router owns the final cross-canister
`Completed` transition.

## Design documentation impact

| Document | Update | Status |
|----------|--------|--------|
| [adr/README.md](README.md) | Index ADR 0023 | done |
| [adr/0009-edge-property-index-and-index-ddl.md](0009-edge-property-index-and-index-ddl.md) | Note: shard index registry superseded by router-sourced ephemeral catalog (0023) | done |
| [index/property-index.md](../index/property-index.md) | Replace shard-registry gate with router-sourced ephemeral catalog; precise emit; repair journal | done (registry→ephemeral catalog); repair journal pending |
| [adr/0020-deferred-maintenance-timer-drain.md](0020-deferred-maintenance-timer-drain.md) | Tick becomes async; per-drain catalog fetch; in-tick flush; defer-on-router-unavailable | done |
| [index/derived-state-query-semantics.md](../index/derived-state-query-semantics.md) | INV statement, eventual-consistency/repair model, and canonical-success freshness caveat | done |
