# 0015. Label stats projection log and graph mutation journal

Date: 2026-06-15  
Status: proposed  
Last revised: 2026-06-15  
Anchor timestamp: 2026-06-15 09:35:20 UTC +0000

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

## Decision

Replace the telemetry-centered model with an explicit label stats projection model.

### 1. Rename the durable event stream to label stats deltas

Graph stable regions should represent an ordered projection input stream:

| Current | Target |
|---------|--------|
| `LABEL_TELEMETRY_SEQ` | `LABEL_STATS_DELTA_SEQ` |
| `LABEL_TELEMETRY_OUTBOX` | `LABEL_STATS_DELTA_LOG` |
| `LabelTelemetryEventWire` | `LabelStatsDeltaEventWire` |
| `LabelUsageDelta` | `LabelStatsDelta` |

`LABEL_STATS_DELTA_LOG` is an append-only logical stream keyed by shard-local sequence. Physical
retention may remove events only after the router projection has safely advanced past them.

### 2. Replace per-event router dedup with per-shard projection cursors

Router should own:

| Target region | Key | Value | Role |
|---------------|-----|-------|------|
| `ROUTER_LABEL_STATS_PROJECTION` | `ShardId` | `applied_through_seq` | Cursor for graph shard label stats deltas |
| `ROUTER_VERTEX_LABEL_STATS` | `VertexLabelId` | aggregate stats | Derived count store |
| `ROUTER_EDGE_LABEL_STATS` | `EdgeLabelId` | aggregate stats | Derived count store |
| `ROUTER_VERTEX_LABEL_LIVE_BY_SHARD` | `(ShardId, VertexLabelId)` | live count | Derived per-shard count |
| `ROUTER_EDGE_LABEL_LIVE_BY_SHARD` | `(ShardId, EdgeLabelId)` | live count | Derived per-shard count |

`ROUTER_APPLIED_LABEL_TELEMETRY` should be retired after migration. The cursor is the durable
idempotency boundary for ordered replay.

Projection apply invariant:

```text
For each shard, router label stats include exactly the contiguous prefix
of LABEL_STATS_DELTA_LOG ending at ROUTER_LABEL_STATS_PROJECTION[shard_id].
```

The router must reject or stop on gaps. It must not advance the cursor past a missing event.

### 3. Make projection advance the only stats apply path

Introduce one router-owned apply path:

```text
advance_label_stats_projection(graph_id, shard_id, limit)
  -> list deltas after current cursor
  -> apply each contiguous delta in sequence order
  -> advance cursor after each successful apply
  -> ask graph shard to retain/ack deltas through the new cursor
```

Normal DML dispatch may call this function after shard execution. Admin repair and replay should
call the same function. Recovery from a partially completed mutation should not apply event payloads
directly; it should advance the projection.

### 4. Split graph mutation journal from delta payload storage

Replace `APPLIED_MUTATION_REQUESTS` with a graph-local mutation journal:

| Current | Target |
|---------|--------|
| `APPLIED_MUTATION_REQUESTS` | `GRAPH_MUTATION_JOURNAL` |
| `AppliedMutationRequest` | `GraphMutationJournalEntry` |
| Stored event payload copies | `emitted_delta_seq_range` |

Suggested record shape:

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

### 5. Keep query semantics unchanged

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

---

## Alternatives considered

| Alternative | Verdict |
|-------------|---------|
| Keep current design and only rename symbols | Rejected as incomplete; it improves readability but keeps per-event dedup and duplicated event payload ownership |
| Keep telemetry outbox but rename `APPLIED_MUTATION_REQUESTS` only | Rejected; it fixes the most misleading name but not the distributed projection invariant |
| Keep per-event dedup forever | Rejected; it is robust but grows with history and hides ordered stream progress |
| Rebuild router label stats from graph label scans | Rejected for normal operation; too expensive and overlaps graph-index backfill concerns |
| Use graph-index label postings for count-only queries | Rejected; postings are membership indexes, not aggregate stats, and count-only fast paths should not require export scans |
| Introduce a separate projection canister | Rejected; Router already owns global derived aggregates and orchestration |

---

## Consequences

### Positive

- Names match the data model: label stats are a derived projection, not telemetry.
- Router has a compact idempotency boundary: one cursor per shard.
- Mutation idempotency and projection payload storage have separate sources of truth.
- Normal dispatch, recovery, and admin replay share the same projection advance path.
- Query semantics become easier to document: count-only reads depend on projection freshness.

### Trade-offs

- Requires stable layout changes on graph and router.
- Requires migration from per-event dedup to per-shard projection cursors.
- Requires careful handling of gaps, partial apply, and cursor/ack ordering.
- A single cursor assumes each shard log is applied in sequence order. If future routing needs
  sparse event application, this ADR would need revision.
- Debugging a single mutation requires journal range lookup plus delta log inspection instead of
  reading copied event payloads from the mutation record.

---

## Migration

This repository is still pre-production, so the implementation may repack stable regions without a
compatibility layer if the active development workflow permits it. If preserving existing local
state becomes necessary, use the staged migration below.

1. Add target types and APIs behind existing behavior:
   - `LabelStatsDelta`
   - `LabelStatsDeltaEventWire`
   - `GraphMutationJournalEntry`
   - `advance_label_stats_projection`
2. Change graph mutation execution to append deltas to `LABEL_STATS_DELTA_LOG` and store emitted
   delta sequence ranges in `GRAPH_MUTATION_JOURNAL`.
3. Change router dispatch recovery and admin replay to call projection advance instead of applying
   event payloads directly.
4. Replace `ROUTER_APPLIED_LABEL_TELEMETRY` with `ROUTER_LABEL_STATS_PROJECTION`.
5. Rename public/admin APIs after the internal invariant is stable:
   - `admin_label_telemetry_replay_step` -> `admin_label_stats_projection_step`
   - `pending_label_telemetry_events` -> `pending_label_stats_deltas`
   - `ack_label_telemetry_event` -> `ack_label_stats_deltas_through`
6. Update design docs and tests in the same patch.

Migration from existing state, if required:

```text
For each shard:
  cursor = max applied shard_event_seq in ROUTER_APPLIED_LABEL_TELEMETRY for that shard
  require the applied set to be contiguous from the initial sequence through cursor
  write ROUTER_LABEL_STATS_PROJECTION[shard] = cursor
  retain or replay pending graph deltas after cursor
```

If the applied set is not contiguous, stop migration and use graph/delta replay or a controlled
stats rebuild before writing a cursor.

---

## Design documentation impact

When this ADR is implemented, update at least:

| Document | Required update |
|----------|-----------------|
| `design/index/label-index.md` | Replace telemetry replay with label stats projection terminology and APIs |
| `design/index/derived-state-query-semantics.md` | Describe cursor lag, gap handling, and count-only query impact |
| `design/storage/stable-memory-inventory.md` | Rename graph/router regions and classify projection cursor/log regions |
| `design/adr/0004-label-index.md` | Mark telemetry terminology as superseded by this ADR for label stats maintenance |
| `design/adr/0007-stable-memory-layout.md` | Record stable layout changes if region ids are repacked |

---

## Implementation phases

| Phase | Scope | Status |
|-------|-------|--------|
| P0 | Add projection terminology and ADR references | Proposed |
| P1 | Introduce delta log and mutation journal types; keep old API wrappers temporarily | Proposed |
| P2 | Route normal dispatch, recovery, and admin replay through projection advance | Proposed |
| P3 | Replace router event dedup with per-shard cursor | Proposed |
| P4 | Remove telemetry names and stale wrappers; update design docs and tests | Proposed |

