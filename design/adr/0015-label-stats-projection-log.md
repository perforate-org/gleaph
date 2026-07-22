# 0015. Label stats projection log and graph mutation journal

Date: 2026-06-15  
Status: implemented  
Last revised: 2026-07-22
Anchor timestamp: 2026-07-22 21:22:08 UTC +0000

## Context

Label cardinality is used by router-side count-only paths such as:

```text
MATCH (n:L) RETURN count(*)
```

The canonical source of truth for label membership is still graph shard state:

| State | Owner | Role |
|-------|-------|------|
| Vertex labels | Graph shard | Canonical membership |
| Edge labels | Graph shard / edge storage | Canonical membership |
| Label postings | graph-index | Derived read acceleration for label membership and seeded dispatch |
| Router label stats | Router | Derived aggregate counts for count-only query shapes |

ADR 0004 introduced router label telemetry as a derived aggregate path separate from graph-index
label postings. The implemented system persists graph shard `LabelUsageDelta` events in
`LABEL_TELEMETRY_OUTBOX`, applies them to router label stats, and deduplicates event application
with `ROUTER_APPLIED_LABEL_TELEMETRY`.

Graph shards also persist `APPLIED_MUTATION_REQUESTS` by `MutationId`. That record caches the
mutation outcome and the label telemetry events emitted by the mutation, allowing router retries
to recover the outcome and apply or replay pending label telemetry.

This works, but the concepts have drifted:

| Current concept | Issue |
|-----------------|-------|
| `LABEL_TELEMETRY_SEQ` / `LABEL_TELEMETRY_OUTBOX` | The data is query-correctness projection input, not observational telemetry |
| `ROUTER_APPLIED_LABEL_TELEMETRY` | Per-event dedup grows with every applied event |
| `APPLIED_MUTATION_REQUESTS` | The name suggests request storage, but the record is a graph-local mutation outcome / idempotency journal |
| Normal DML apply and admin replay | Both apply label telemetry; the exactly-once boundary is distributed across paths |

The naming problem is a symptom of a deeper architectural shape: label stats are an event-sourced
projection, but the implementation does not name projection state as the primary concept.

---

## Problem

The current design has three avoidable weaknesses.

### 1. The projection cursor is implicit

Router label stats are derived from graph shard events, but the router tracks applied events as a
set of `(shard_id, shard_event_seq)` keys. The stable invariant is therefore:

```text
An event may update router label stats only if its event key is absent from the dedup set.
```

For a shard-local ordered stream, the more direct invariant is:

```text
Router label stats include every label stats delta up to the shard projection cursor.
```

The current dedup set is robust to duplicate delivery but does not express ordered projection
progress as the source of truth.

### 2. Mutation idempotency and projection replay are coupled by event payload copies

`APPLIED_MUTATION_REQUESTS` stores the emitted label telemetry events inside the mutation outcome.
That makes retries convenient, but it duplicates projection event knowledge across:

- the graph shard outbox,
- the graph mutation idempotency record,
- the router apply path.

The graph mutation journal should explain whether a mutation completed and which durable event
range it emitted. The label stats delta log should own the event payloads.

### 3. Apply paths are wider than necessary

Router dispatch can apply returned telemetry events inline. Admin replay can apply pending outbox
events later. Recovery can query mutation outcome and apply those events, or scan pending telemetry
for the mutation id.

These paths are correct only if they maintain the same event idempotency discipline. A projection
advance API would concentrate that invariant in one place.

---

## Existing architecture assessment

| Domain | Existing responsibility | Assessment |
|--------|-------------------------|------------|
| Graph | Canonical graph mutation execution and shard-local storage | Correct owner of mutation journal and durable delta log |
| Router | Query ingress, shard orchestration, global derived aggregates | Correct owner of label stats projection state and aggregate maps |
| graph-index | Label membership postings and export/intersection reads | Should remain separate; count-only stats should not depend on postings |
| GQL crates | Parse, validate, and plan portable GQL | Should not know projection state, mutation journals, or IC retry rules |

The existing domains can absorb the improvement. No new canister, index subsystem, or cross-crate
execution layer is needed.

The missing concept is not a new domain. It is a sharper boundary inside existing domains:

| Boundary | Owner | Source of truth |
|----------|-------|-----------------|
| Graph mutation idempotency | Graph | `MutationId -> MutationOutcome + emitted seq range` |
| Label stats delta stream | Graph | Ordered shard-local durable delta log |
| Label stats projection progress | Router | `ShardId -> applied_through_seq` |
| Label stats aggregates | Router | Derived from advancing the projection |

---

## Implementation policy

This repository is **pre-production**. ADR 0015 is implemented as a **single breaking change**
with **no backward-compatibility layer**.

| Policy | Requirement |
|--------|-------------|
| Stable layout | Repack regions in place; do not retain legacy storable encodings |
| Wire types | Delete legacy telemetry names; one primary type per concept |
| Canister APIs | Rename or remove old methods in the same patch; no deprecated wrappers |
| Router apply paths | One path only: `advance_label_stats_projection` |
| Local / CI state | Reset graph and router canisters (or wipe stable memory) after deploy |
| In-flight mutations | Not preserved across the breaking deploy |
| Phased dual-write | **Rejected** — no temporary coexistence of dedup set and cursor, no alias APIs |

Rationale: a compatibility layer would duplicate projection invariants across old and new paths,
leave misleading telemetry terminology in production code, and delay the cursor model that is the
point of this ADR. Pre-production status makes a clean break cheaper than maintaining two models.

---

## Decision

Replace the telemetry-centered model with an explicit label stats projection model in one pass.

### 1. Rename the durable event stream to label stats deltas

Graph stable regions represent an ordered projection input stream:

| Remove | Introduce |
|--------|-----------|
| `LABEL_TELEMETRY_SEQ` | `LABEL_STATS_DELTA_SEQ` |
| `LABEL_TELEMETRY_OUTBOX` | `LABEL_STATS_DELTA_LOG` |
| `LabelTelemetryEventWire` | `LabelStatsDeltaEventWire` |
| `LabelUsageDelta` | `LabelStatsDelta` |

`LABEL_STATS_DELTA_LOG` is an append-only logical stream keyed by shard-local sequence. Physical
retention may remove events only after the router projection has safely advanced past them.

Memory ids for graph regions 37–39 are **reused** with new storable types. Existing encoded state
for those regions is invalid after the change.

### 2. Replace per-event router dedup with per-shard projection cursors

Router owns:

| Region | Key | Value | Role |
|--------|-----|-------|------|
| `ROUTER_LABEL_STATS_PROJECTION` | `ShardId` | `applied_through_seq` | Cursor for graph shard label stats deltas |
| `ROUTER_VERTEX_LABEL_STATS` | `VertexLabelId` | aggregate stats | Derived count store (unchanged) |
| `ROUTER_EDGE_LABEL_STATS` | `EdgeLabelId` | aggregate stats | Derived count store (unchanged) |
| `ROUTER_VERTEX_LABEL_LIVE_BY_SHARD` | `(ShardId, VertexLabelId)` | live count | Derived per-shard count (unchanged) |
| `ROUTER_EDGE_LABEL_LIVE_BY_SHARD` | `(ShardId, EdgeLabelId)` | live count | Derived per-shard count (unchanged) |

`ROUTER_APPLIED_LABEL_TELEMETRY` is **deleted**. Memory id 17 is repacked as
`ROUTER_LABEL_STATS_PROJECTION`. The cursor is the durable idempotency boundary for ordered replay.

Projection apply invariant:

```text
For each shard, router label stats include exactly the contiguous prefix
of LABEL_STATS_DELTA_LOG ending at ROUTER_LABEL_STATS_PROJECTION[shard_id].
```

The router must reject or stop on gaps. It must not advance the cursor past a missing event.

### 3. Make projection advance the only stats apply path

One router-owned apply path:

```text
advance_label_stats_projection(graph_id, shard_id, limit)
  -> list deltas after current cursor
  -> apply each contiguous delta in sequence order
  -> advance cursor after each successful apply
  -> ack graph shard deltas through the new cursor
```

All callers use this function:

| Caller | Behavior |
|--------|----------|
| Normal DML dispatch | Advance after shard execution |
| Admin repair | `admin_label_stats_projection_step` loops advance |
| Mutation recovery | Read journal seq range; advance through `emitted_delta_last_seq` |

Router dispatch **must not** apply delta payloads inline from `ExecutePlanResult` or
`MutationOutcomeWire`. Those wire types no longer carry event vectors.

### 4. Split graph mutation journal from delta payload storage

Replace `APPLIED_MUTATION_REQUESTS` with a graph-local mutation journal:

| Remove | Introduce |
|--------|-----------|
| `APPLIED_MUTATION_REQUESTS` | `GRAPH_MUTATION_JOURNAL` |
| `AppliedMutationRequest` | `GraphMutationJournalEntry` |
| Stored `Vec<LabelTelemetryEventWire>` in journal | `emitted_delta_first_seq` / `emitted_delta_last_seq` |

Record shape:

```text
GraphMutationJournalEntry {
  mutation_id,
  state: Incomplete | Completed,
  row_count,
  emitted_delta_first_seq,
  emitted_delta_last_seq,
}
```

The journal is the source of truth for graph-local idempotency and retry outcome. The delta log is
the source of truth for label stats projection payloads.

Memory id 39 is **reused** with the new storable type.

### 5. Slim cross-canister wire types

`graph-kernel` wire types after this ADR:

```text
ExecutePlanResult {
  row_count,
  rows_blob,           // query path only; no label events
}

MutationOutcomeWire {
  mutation_id,
  completed,
  row_count,
  emitted_delta_first_seq,
  emitted_delta_last_seq,
}

LabelStatsDeltaEventWire {
  mutation_id,
  shard_event_seq,
  label_stats_delta,
}
```

Router client mutation records drop cached `label_telemetry_events`. Shard completion tracks
whether projection was advanced for that shard, not whether individual events were acked inline.

### 6. Keep query semantics unchanged

This ADR does not change the semantics of count-only label queries. It changes how the router keeps
its derived label stats synchronized.

| Query path | Source after this ADR |
|------------|-----------------------|
| Count-only label stats | Router label stats projection |
| Vertex/edge membership reads | graph-index label postings |
| Compound label + property seeds | graph-index label postings and property index hits |

---

## Invariants

| Invariant | Owner | Enforcement point |
|-----------|-------|-------------------|
| Delta sequence is shard-local, monotonic, and non-zero | Graph | Delta append |
| Delta payload is stored once | Graph | `LABEL_STATS_DELTA_LOG` write boundary |
| Mutation journal records emitted delta range, not copied payloads | Graph | Mutation commit |
| Router stats apply a contiguous prefix only | Router | Projection advance |
| Cursor advances after aggregate maps are updated | Router | Projection transaction boundary |
| Graph may drop retained deltas only through acknowledged cursor | Graph | Delta log retention / ack |
| Graph-index postings remain the label membership read source | graph-index | Existing posting update/backfill paths |
| Exactly one router code path mutates label stats from graph deltas | Router | `advance_label_stats_projection` |

---

## Public API surface (target)

### Graph canister

| Method | Role |
|--------|------|
| `list_pending_label_stats_deltas(from_seq, limit)` | Read deltas after cursor |
| `ack_label_stats_deltas_through(through_seq)` | Retention: drop acknowledged prefix |
| `get_mutation_journal_entry(mutation_id)` | Idempotency outcome + emitted seq range |

Removed: `list_pending_label_telemetry_events`, `ack_label_telemetry_event`,
`get_mutation_outcome` (superseded by journal entry when only seq range is needed, or journal
entry replaces outcome for router recovery).

### Router canister

| Method | Role |
|--------|------|
| `admin_label_stats_projection_step` | Admin repair loop per shard |

Removed: `admin_label_telemetry_replay_step`.

Internal: `advance_label_stats_projection` is the sole stats apply implementation.

---

## Alternatives considered

| Alternative | Verdict |
|-------------|---------|
| Keep current design and only rename symbols | Rejected as incomplete; it improves readability but keeps per-event dedup and duplicated event payload ownership |
| Phased migration with legacy API wrappers | Rejected; duplicates invariants and prolongs misleading telemetry terminology |
| Keep per-event dedup forever | Rejected; it is robust but grows with history and hides ordered stream progress |
| Rebuild router label stats from graph label scans | Rejected for normal operation; too expensive and overlaps graph-index backfill concerns |
| Use graph-index label postings for count-only queries | Rejected; postings are membership indexes, not aggregate stats, and count-only fast paths should not require export scans |
| Introduce a separate projection canister | Rejected; Router already owns global derived aggregates and orchestration |
| Online migration from dedup set to cursor | Rejected under pre-production policy; reset stable memory instead |

---

## Consequences

### Positive

- Names match the data model: label stats are a derived projection, not telemetry.
- Router has a compact idempotency boundary: one cursor per shard.
- Mutation idempotency and projection payload storage have separate sources of truth.
- Normal dispatch, recovery, and admin replay share the same projection advance path.
- Query semantics become easier to document: count-only reads depend on projection freshness.
- Implementation is smaller and reviewable as one coherent breaking change.

### Trade-offs

- **Breaking deploy:** existing graph/router stable memory and Candid callers must reset or upgrade
  in lockstep.
- Requires careful handling of gaps, partial apply, and cursor/ack ordering.
- A single cursor assumes each shard log is applied in sequence order. If future routing needs
  sparse event application, this ADR would need revision.
- Debugging a single mutation requires journal range lookup plus delta log inspection instead of
  reading copied event payloads from the mutation record.
- DML path may require an extra graph call to list/ack deltas after execute (acceptable; correctness
  over inline convenience).

---

## Deployment impact

No migration from legacy telemetry state is supported.

After the implementing patch lands:

1. **Reset** graph and router canisters used for local dev, PocketIC, and CI fixtures.
2. **Re-seed** test graphs and replay label posting backfill if fixtures depend on persisted state.
3. **Update** any external caller that invoked removed Candid methods or decoded removed wire fields.

Developers must not expect existing stable memory from before the patch to decode successfully.
If a canister fails to decode stable regions after upgrade, wipe stable memory and re-init.

---

## Design documentation impact

Update in the **same patch** as the implementation:

| Document | Required update |
|----------|-----------------|
| `design/index/label-index.md` | Replace telemetry replay with label stats projection terminology and APIs |
| `design/index/derived-state-query-semantics.md` | Describe cursor lag, gap handling, and count-only query impact |
| `design/storage/stable-memory-inventory.md` | Rename graph/router regions and classify projection cursor/log regions |
| `design/adr/0004-label-index.md` | Mark telemetry terminology as superseded by this ADR for label stats maintenance |
| `design/adr/0007-stable-memory-layout.md` | Record stable layout repack for regions 17 and 37–39 |

---

## Implementation checklist

Single patch (or at most two: kernel/graph/router code, then docs/tests if split for review size).

| Step | Scope |
|------|-------|
| 1 | `graph-kernel`: replace wire types; remove `LabelTelemetryEventWire`, `LabelUsageDelta`, event vectors from `ExecutePlanResult` / `MutationOutcomeWire` |
| 2 | Graph stable: rename regions 37–39; new storable types; rename modules to `label_stats_delta` / `mutation_journal` |
| 3 | Graph DML: append to delta log; journal seq range on commit; no event copies in journal |
| 4 | Graph canister: expose target APIs only |
| 5 | Router stable: repack region 17 as `ROUTER_LABEL_STATS_PROJECTION`; delete dedup set |
| 6 | Router: implement `advance_label_stats_projection`; route DML, recovery, and admin through it |
| 7 | Router: slim `RouterMutationShard` and client mutation idempotency state |
| 8 | Delete all `label_telemetry` / `telemetry_replay` naming in graph/router stats path |
| 9 | Tests: projection advance, gap handling, mutation retry, admin step |
| 10 | Design docs listed above |

**Definition of done:**

- No remaining references to `ROUTER_APPLIED_LABEL_TELEMETRY`, `LABEL_TELEMETRY_*`, or
  `APPLIED_MUTATION_REQUESTS` in graph/router label-stats maintenance code.
- `advance_label_stats_projection` is the only path that mutates router label stats from graph
  deltas.
- Count-only label query tests pass after canister reset.
- ADR 0015 status set to `implemented`.

---

## Implementation status

| Item | Status |
|------|--------|
| ADR accepted (no-compat policy) | Done — 2026-06-15 |
| Implementation | Done — 2026-06-15 |

---

## Addendum (ADR 0024, 2026-06-20)

The `Incomplete` state in this ADR predates ADR 0023's durable index repair journal. A
post-mutation index flush that fails but is journaled for repair is **not** a mutation
failure: the store mutation and the emitted label-stats deltas are already durable, and
the index converges asynchronously. ADR 0024 therefore decouples mutation-journal
completion from synchronous flush success — a single-statement DML mutation is recorded
`Completed` even when its index flush was deferred, so the entry is no longer wedged
`Incomplete` forever. See `design/adr/0024-mutation-journal-completion-vs-index-flush.md`.

---

## Addendum: cost attribution (Plan 0119, 2026-07-22)

Non-invasive encode probes show that the label-stats delta event encode cost
(`label_stats_delta_event_encode`) dominates the append path:

- ~101 K instructions to encode a 76-byte event.
- `label_stats_delta_log_insert` total ~116 K, leaving only ~15 K for
  `StableBTreeMap` I/O and sequence allocation.
- The fixed Candid overhead, not byte length or stable-page I/O, is the
  dominant cost.

A fixed-length manual layout for `StoredLabelStatsDeltaEvent` is therefore the
selected follow-up (Plan 0120). This addendum does not change the projection
semantics, retention, or ack boundary recorded above.
