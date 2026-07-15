# Label index

Last updated: 2026-07-15
Anchor timestamp: 2026-07-15 08:58:08 UTC +0000
Implementation verified as of: 2026-07-15 (postings, read paths A–D, label stats projection path B, backfill orchestration and batch delivery)

## Status

**Implemented** — Postings, DML sync, read paths **A** / **B** / **C1** / **C2** / **D**, label
posting backfill orchestration, compound read seeds, and paginated seed export. Seed routing no
longer falls back to unseeded all-shard execution on large hit lists (see
[ADR 0004](../adr/0004-label-index.md)).

## Purpose

Vertex **label membership** on graph-index: `contains(label, shard, vertex)` and, when necessary,
**vertex-id export** for seeds.

Most labeled queries should **not** bulk-export membership. Use:

- **Property index + label sieve** when properties participate.
- **Label stats projection** when only counts are needed.
- **Paginated `lookup_label_page`** when a **vertex list** is required.

## Non-goals

- Edge label postings.
- Bulk `lookup_label` as the default for large labels on every query shape.
- Replacing label stats projection for count-only queries.

## Posting model

```text
LabelPostingKey { vertex_label_id, shard_id, vertex_id }
```

Separate `BTreeSet` from property postings.

## Read paths

### A — Vertex list export — Implemented

| API | When |
|-----|------|
| `lookup_label(label_id)` | Full label bucket (graph-index direct API; router uses paginated export) |
| `lookup_label_for_shard(label_id, shard_id)` | One shard's postings |
| `lookup_label_page(req)` | Paginated shard-local export (`after` + `limit`) |
| `lookup_label_intersection_page(req)` | Paginated shard-local walk with all sieve labels applied in graph-index |

Router seed routing uses **`lookup_label_page` per registered shard** (10k hits/page) instead of
one bulk `lookup_label`.

**Policy:** No fallback to unseeded all-shard execution based on hit count. Large lists are
acceptable when vertex ids are required; unseeded graph scans are worse.

**Not for:** `COUNT(*)` without vertices; `GROUP BY` indexed property (use C).

### B — Label stats projection — Implemented (router)

| Source | When |
|--------|------|
| `vertex_label_stats` (`live_count`), `vertex_label_shard_live_count` | `MATCH (n:L) RETURN count(*)` and other **count-only** shapes |

Graph shards append `LabelStatsDelta` events to `LABEL_STATS_DELTA_LOG` on DML. Router applies
them via `advance_label_stats_projection` into `ROUTER_VERTEX_LABEL_STATS` and per-shard live
maps (`ROUTER_VERTEX_LABEL_LIVE_BY_SHARD`); `ROUTER_LABEL_STATS_PROJECTION` tracks each shard's
`applied_through_seq`. No graph-index call. See [ADR 0015](../adr/0015-label-stats-projection-log.md)
and the write-path **Label stats projection** section below.

### C — Property path + label sieve — Implemented

**C1 — Small property hit set**

```text
filter_hits_by_label(label_id, hits)
```

After `lookup_equal` (or similar). Label applies **contains** to each hit — cost ∝ `len(hits)`.

**C2 — GROUP BY property, label on MATCH**

```text
count_postings_by_value_for_label(property_id, label_id, min_count)
```

Walk property bucket; label sieve per posting; return `(value, count)` only.

```gql
MATCH (n:Person) GROUP BY n.country     → C2
MATCH (n:Person) WHERE n.region = 'US' GROUP BY n.country  → C1 then count
```

### D — Multi-label vertex list — Implemented

`lookup_label_intersection` on graph-index for small intersections; router seed routing
pages the walk label per shard through `lookup_label_intersection_page`, where graph-index
applies all sieve labels in the same call. The cursor still advances over the walk-label
postings, including rows removed by the sieve. The older `lookup_label_page` plus
`filter_hits_by_label` composition remains the default behavior for injected/native test
clients. Used when the plan needs explicit ids for `:L1:L2:…`
(`IndexAnchor::LabelIntersection` from `NodeScan` + `IsLabeled` filters).

## Write path — Implemented

`label_pending` + graph-index `label_posting_insert/remove` on label DML.

**Rebuild inventory:** [stable-memory-inventory.md](../storage/stable-memory-inventory.md) (`INDEX_VERTEX_LABEL_POSTINGS`, `ROUTER_LABEL_BACKFILL_STATE`).

**Backfill:** `backfill_label_postings` on graph shards replays `VertexLabelStore` into
graph-index for pre-existing data, using the budget-driven `posting_batch` transport when the
concrete client supports it and retaining the per-posting fallback for native/legacy clients.
Router orchestrates per-shard cursors via
`admin_label_backfill_step` / `admin_list_label_backfill_status` (Admin-only).

**Label stats projection:** Graph shards persist unacked `LabelStatsDelta` events in
`LABEL_STATS_DELTA_LOG` (seq in `LABEL_STATS_DELTA_SEQ`). Router aggregates land in
`ROUTER_VERTEX_LABEL_STATS`, `ROUTER_EDGE_LABEL_STATS`, and per-shard live maps;
`ROUTER_LABEL_STATS_PROJECTION` records the per-shard applied-through seq. Normal DML applies
deltas inline via `advance_label_stats_projection` and acks the graph log. After router downtime
or partial apply, drain pending deltas per shard via `admin_label_stats_projection_step`
(Admin-only; call in a loop until `done`). Already-applied seqs are skipped by cursor
advance without changing aggregates. There is no full historical rebuild from vertex label scans —
replay depends on the graph delta log retaining pending events. See [ADR 0015](../adr/0015-label-stats-projection-log.md).

**Compound read seeds:** `MATCH (n:L) WHERE n.p = v RETURN n` uses `SeedAnchorSet` to
intersect label and property index hits before per-shard `seed_bindings_blob` dispatch
(`compound_label_and_property_seed_routing_intersects_hits` in `router/src/gql.rs`).

## Router

| Query need | Path |
|------------|------|
| Seed / return vertices for `:L` | A (paginated) |
| `COUNT(*)` for `:L` only | B |
| `:L` + indexed property filter / `GROUP BY` | C1 / C2 |
| `:L1:L2:…` seed export | D (paginated walk + sieve) |
| Property only | Property index (no label) |

Graph shards skip leading `NodeScan` + `PropertyFilter` (`IsLabeled` sieve) when the router
supplies `seed_bindings_blob` for label-intersection anchors (`seeded_skip_leading_label_intersection_plan_uses_seed_only`;
wire path: `wire_plan_seed_bindings_skip_label_intersection_prefix` in `gql_run.rs`;
canister handler: `execute_plan_query_seed_bindings_skip_label_intersection` in `handlers.rs`;
router fan-out: `label_intersection_seed_routing_fans_out_with_bindings` in `router/src/gql.rs`).

### Instruction bounds (retained)

| Bound | Effect |
|-------|--------|
| C1 aggregate packed `vertex_filter` > 10k hits | Aggregate fast path returns `None` → generic federated execution |
| `lookup_label_page` page size | Shard-scoped pagination for seed export |

These are platform/instruction limits — not a downgrade to unseeded shard scans.

## Query → path cheat sheet

| GQL sketch | Path |
|------------|------|
| `MATCH (n) WHERE n.p = v` | Property |
| `MATCH (n:L) RETURN n` | A |
| `MATCH (n:L) RETURN count(*)` | B |
| `MATCH (n:L) GROUP BY n.p` | C2 |
| `MATCH (n:L) WHERE n.q = v …` | C1 (+ property) |
| `MATCH (n:L1:L2) RETURN n` | D |

## Access patterns (`BTreeSet`)

Both property and label postings live in stable `BTreeSet`s keyed for **lexicographic
`range`**. There is no separate index structure; scans and sieves compose `range` on one
dimension with **`contains` point lookups** on the other.

### `range` — walk a prefix or bucket

Use when exporting hits or aggregating along a sorted dimension.

| Operation | Set | Bounds |
|-----------|-----|--------|
| `lookup_label(L)` | label | `prefix_lower(L) ..= prefix_upper(L)` |
| `lookup_equal(p, v)` | property | `(p, v, …)` prefix range |
| `count_postings_by_value(p)` | property | half-open `property_posting_bucket(p)` → `[low, high)` |
| `count_postings_by_value_for_label(p, L)` | property | same bucket `range` as above |

Bounds helpers: `LabelPostingKey::prefix_lower/upper`, `PostingKey::prefix_lower/upper`,
`property_posting_bucket` ([`posting_range.rs`](../../crates/graph-index/src/posting_range.rs),
[`label_key.rs`](../../crates/graph-index/src/label_key.rs)).

### `contains` — label sieve on known vertices

Use when the query already has `(shard_id, vertex_id)` candidates and only needs **membership**
in label `L`. Cost ∝ number of candidates, not label cardinality.

```text
LabelPostingKey { vertex_label_id: L, shard_id, vertex_id }
label_set.contains(key)   // O(log n) per check
```

| Path | Walk (`range`) | Sieve |
|------|----------------|-------|
| A export | label `range` / paginated pages | — |
| C1 `filter_hits_by_label` | — (input hits from property `range`) | label `contains` per hit |
| C2 `count_postings_by_value_for_label` | property bucket `range` | label `contains` per posting |
| D multi-label seeds | smallest-label `range` per page | label `contains` per hit for other labels |

### Why not `range` for both sides?

A label **membership test for one vertex** is a single key, not a prefix. Walking the full label
bucket to intersect with a small property hit set is correct only when the label bucket is the
**smaller** side; for `GROUP BY` the property bucket is the natural scan axis (values are already
grouped in key order). Default: **property `range` + label `contains`**.

Property-only `lookup_intersection` uses multiple property `range`s plus in-memory set
intersection — a different pattern, documented in [lookup-intersection.md](lookup-intersection.md).

## Derived-state lag

Posting export and compound seeds follow label **postings**; count-only paths follow router
**label stats projection** (path **B**). Lag scenarios, DML vs read asymmetry, and operator
expectations: [derived-state-query-semantics.md](derived-state-query-semantics.md) (label stats
projection lag).

## Capacity

Label postings scale **linearly with labeled vertices** (one key per membership). For 500 GiB planning formulas and split thresholds, see [capacity-planning.md](capacity-planning.md).

## Related documents

- [derived-state-query-semantics.md](derived-state-query-semantics.md)
- [ADR 0004](../adr/0004-label-index.md)
- [ADR 0015](../adr/0015-label-stats-projection-log.md)
- [property-index.md](property-index.md)
- [../adr/0003-federated-aggregate-merge.md](../adr/0003-federated-aggregate-merge.md)
