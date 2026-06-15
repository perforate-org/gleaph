# Derived-state query semantics

Last updated: 2026-06-11  
Anchor timestamp: 2026-06-11 23:23:04 UTC +0000

## Status

**Implemented** — documents current query behavior when derived indexes, telemetry, or maintenance
state may lag canonical graph data. Complements [stable-memory-inventory.md](../storage/stable-memory-inventory.md).

## Purpose

State honestly what federated and standalone queries observe when derived stores are incomplete,
stale, or unavailable. Derived state is never consulted to recover canonical data; query paths must
not paper over sync gaps with graph-side tombstone filtering at the index layer.

## Principles

1. **Canonical wins.** Vertex rows, properties, labels, and forward edges are authoritative on the
   graph shard. Derived stores are projections for read optimization.
2. **No index-side tombstone sieve.** Property and label index reads do not re-check live vertex
   existence on the graph shard ([standalone-mode.md](../sharding/standalone-mode.md)). Stale
   postings are a sync or backfill problem, not a query-time filter.
3. **Intentional index-only miss ≠ staleness.** Unindexable or null property values are omitted by
   design ([property-index.md](property-index.md)); equality/range scans will not find them without a
   full-scan path.
4. **Maintenance cursors are not data.** Router `BackfillShardState` and graph pending queues track
   repair progress; they must not be read as membership or count truth.

## Sync vs lag policy

| Derived store | Update contract | Acceptable lag | Query impact when lagging |
|---------------|-----------------|----------------|---------------------------|
| Edge property postings (graph-index) | **Async** via `edge_pending` flush on federated DML | graph-index may lag canonical | Expand equality may miss until backfill; use `backfill_edge_property_postings` |
| Edge aliases | **Sync** on edge insert/delete | None (bug if mismatched) | Wrong reverse/undirected expand; use `check_edge_aliases` / `rebuild_edge_aliases` |
| Property postings (graph-index) | DML enqueue + `pending` flush | Pending queue before flush; flush retry; historical **backfill** in progress | **Under-posted:** equality/range/seed miss live vertices. **Over-posted:** extra hits until remove syncs. No silent drop at read time |
| Label postings (graph-index) | DML enqueue + `label_pending` flush | Same as property postings | **Under-posted:** label sieve / export / intersection miss. **Over-posted:** extra hits until remove syncs |
| Router label telemetry | Graph outbox event apply | Unacked outbox events; router down before replay | **Count-only** paths (`COUNT(*)`, stats) under/over-count. Vertex-list paths use label **postings**, not telemetry |
| Router placements (`ROUTER_PLACEMENTS`) | Placement commit on graph DML | Rebuild from placement map only on repair | `resolve_placement(GlobalVertexId)` wrong or missing until rebuilt |

## Scenarios

### Pending queue not flushed

Graph DML enqueues posting ops in `index/pending.rs` / `label_pending.rs`. Until
`flush_pending` succeeds, graph-index lags canonical shard state. Mutations after enqueue are ordered
per shard; a failed flush batch is compensated and re-queued ([`pending.rs`](../../crates/graph/src/index/pending.rs)).

**Query behavior:** Index anchors and router seeds reflect last successful flush only. Operators
should not assume read-your-writes through the index until flush completes.

### No index client configured

Without a wired index client, graph may drop index maintenance on DML. Canonical stores still
update.

**Query behavior:** Index-backed plans return empty or fail at router dispatch depending on path.
This is a deployment misconfiguration, not a supported degraded mode.

### Backfill in progress

`backfill_label_postings` / `backfill_property_postings` replay historical canonical state into
graph-index. Router `admin_*_backfill_step` advances per-shard cursors (`BackfillShardState`).

**Query behavior:** Vertices before the cursor may be missing from the index. DML after deployment
still flows through pending flush independently. Run backfill loops until `done` before relying on
historical completeness.

### Label stats projection lag

`admin_label_stats_projection_step` drains graph `LABEL_STATS_DELTA_LOG` into router aggregates
via `advance_label_stats_projection`. Per-shard `ROUTER_LABEL_STATS_PROJECTION` cursors must
advance contiguously; a gap in the delta log fails the step until the graph catches up.

**Query behavior:** Count-only labeled queries may under-count until projection catches up. Label
membership export and property+label compound seeds use **postings**, not router label stats.

### Upgrade / ephemeral loss

Pending queues and router ephemeral planner catalogs are lost on upgrade ([stable-memory-inventory.md](../storage/stable-memory-inventory.md)). Stable backfill cursors and projection cursors survive on router; graph delta log survives on shard.

**Query behavior:** Run label stats projection and posting backfill after upgrade when index or
count completeness is required.

## Operator expectations

| Symptom | Likely cause | Remediation |
|---------|--------------|-------------|
| Index miss for known property value | Unindexable value, pending not flushed, or backfill incomplete | Check `property_indexability`; flush pending; run property backfill |
| Extra index hit for deleted vertex | Remove posting not synced | Flush/retry pending; verify DML index path |
| `COUNT(*)` wrong for label | Projection lag | `admin_label_stats_projection_step` per shard |
| Expand equality wrong | graph-index edge posting lag or unregistered property | `backfill_edge_property_postings`; verify index registry |
| Reverse expand wrong | Edge alias drift | `check_edge_aliases`; `rebuild_edge_aliases` |

## Related documents

- [stable-memory-inventory.md](../storage/stable-memory-inventory.md)
- [property-index.md](property-index.md)
- [label-index.md](label-index.md)
- [../sharding/standalone-mode.md](../sharding/standalone-mode.md)
- [../federation/query-semantics.md](../federation/query-semantics.md)
- [../architecture/refactoring-roadmap.md](../architecture/refactoring-roadmap.md)
