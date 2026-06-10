# 0004. Label membership index on graph-index

Date: 2026-06-09  
Status: accepted  
Last revised: 2026-06-10

## Revision history

| Date | Change |
|------|--------|
| 2026-06-09 | Accepted; initial label postings model and graph-index APIs (`e6bafe3d`). |
| 2026-06-09 | Implementation: DML sync, router seeds, interim aggregate fast path via `lookup_label`, scale guard (`9f5c661c`–`dc727b13`). |
| 2026-06-10 | Read paths A/B/C/D: telemetry for count-only; property `range` + label `contains` sieve; narrow vertex export; deprecate unseeded fallback on hit size. |
| 2026-06-10 | Completed: router backfill orchestration; compound read seeds; paginated multi-label intersection; removed unseeded seed fallback. |

## Context

[ADR 0003](0003-federated-aggregate-merge.md) added federated aggregate **index fast path** via
`count_postings_by_value` on property buckets. Vertex **labels** (`MATCH (n:Person)`) are separate
from indexed **properties** (`n.region`, `GROUP BY n.country`).

Router **label telemetry** (`LabelUsageDelta`, `vertex_label_shard_live_count`,
`vertex_label_stats`) already maintains **aggregate live counts per label per shard** on the
router. It does not materialize `{ shard_id, vertex_id }` membership.

Label **postings** on graph-index materialize per-vertex membership for membership checks and,
when unavoidable, **vertex-id export**. Bulk export is expensive for large labels; most query shapes
do not need it.

### Query shapes and the right source

| Shape | Needs vertex ids? | Right mechanism |
|-------|-------------------|-----------------|
| `MATCH (n) WHERE n.uid = $x` | Yes (few) | Property `lookup_equal` |
| `MATCH (n:L) RETURN n` / traverse seeds | Yes (listed) | **A** paginated `lookup_label_page` → per-shard seeds |
| `MATCH (n:L) RETURN count(*)` | **No** | **B** label telemetry (sum per shard) |
| `MATCH (n:L) GROUP BY n.p` | No (group counts) | **C2** property bucket walk + label sieve |
| `MATCH (n:L) WHERE n.q = v GROUP BY n.p` | No | **C1** selective property hits + label sieve |

**Property-only and label-only queries are both common.** Property index handles property entry;
label postings are **not** a second full-graph export path for every labeled query.

`PLAN_WIRE_VERSION` remains **1**.

## Decision

Maintain label membership in a **separate** `LabelPostingKey` `BTreeSet` on graph-index (not merged
into `PostingKey`). Choose the read path by **whether the query needs a vertex list**.

### Posting model (storage)

```text
LabelPostingKey { vertex_label_id, shard_id, vertex_id }
```

Sorted: `vertex_label_id → shard_id → vertex_id`. One posting per (label, vertex). Multi-label
vertices appear in multiple buckets.

Writes: graph DML → `label_posting_insert/remove` with compensate-and-retry (**Implemented**).

### Read paths

#### A — Vertex list export

**When:** The execution plan needs an explicit list of `(shard_id, vertex_id)` for a label — e.g.
seed routing for `NodeScan { label: Some(L) }`, `RETURN n` / traverse entry without a selective
property anchor.

**APIs (Implemented):**

- `lookup_label(label_id) -> Vec<PostingHit>` — full bucket (small labels / tests)
- `lookup_label_for_shard(label_id, shard_id) -> Vec<PostingHit>`
- `lookup_label_page(req) -> LabelLookupPageResult` — paginated shard-local export

Router seeds call **`lookup_label_page` per shard** (not bulk `lookup_label`).

**No unseeded-shard fallback on list size.** Shipping a large hit list to build
`seed_bindings_blob` is preferred over fan-out to all shards **without seeds**. Instruction/response
limits are handled by shard-scoped pagination, not silent downgrade to unseeded execution.

**Not for:** `COUNT(*)` without returning vertices; `GROUP BY` on an indexed property (use C).

#### B — Label telemetry (counts without vertex list) — Implemented

**When:** Only **cardinality** or **per-shard totals** for a label are required — no vertex ids.

**Source:** Router stable label stats (`vertex_label_shard_live_count`, `vertex_label_stats`),
updated from graph `LabelUsageDelta` on DML.

**Example:**

```gql
MATCH (n:Person) RETURN count(*)
```

Router (or planner fast path) sums live counts across shards for `Person`; **no** `lookup_label`.

**Invariant:** Telemetry must stay consistent with label postings on DML (same events). Drift
checks are out of scope for v1.

#### C — Property path + label sieve (membership checks) — Implemented

**When:** A **property index path** already narrows or organizes work; label restricts which
vertices count.

**C1 — Small property hit set** (selective `WHERE` on indexed property):

```text
lookup_equal(q, v)  → small hits
filter_hits_by_label(L, hits)  → contains checks only
→ seeds or packed filter for count_postings_by_value
```

**API (Implemented):** `filter_hits_by_label(label_id, hits) -> Vec<PostingHit>`

**C2 — GROUP BY on indexed property, label-only `MATCH`:**

```text
count_postings_by_value_for_label(property_id, label_id, min_count)
  walk property bucket; per posting: label contains?
  return (encoded_value, count) only
```

**API (Implemented):** `count_postings_by_value_for_label(...)`

Property provides the **axis** (equality hits or `GROUP BY` bucket). Label is a **sieve**
(`contains`), never a prior full export of all `L` vertices for these shapes.

**Access pattern:** `BTreeSet::range` on the scan axis (property bucket or label prefix for
export); `contains(LabelPostingKey { L, shard, vertex })` for per-vertex sieve. See
[../index/label-index.md](../index/label-index.md#access-patterns-btreeset).

#### D — Multi-label — Implemented

`lookup_label_intersection(label_ids)` on graph-index for direct intersection (small sets).
Router seed routing uses paginated walk + per-hit label sieve
(`collect_label_intersection_hits_for_shards`) instead of bulk export when building seeds.

### Aggregate fast path (router) — Implemented

| Prefix | Mechanism |
|--------|-----------|
| Unlabeled | `count_postings_by_value` (no label) |
| Property anchor only | Existing property path |
| Label + selective property | **C1** |
| Label + `GROUP BY` property | **C2** |
| Label + `COUNT(*)` only, no `GROUP BY` property | **B** telemetry |

**C1 packed-filter budget:** When property+label intersection exceeds 10_000 hits, the aggregate
fast path returns `None` and falls back to generic federated execution — not unseeded shard scans.
This is an instruction/output bound, not a seed-routing downgrade.

### Seed routing — Implemented

| Case | Mechanism |
|------|-----------|
| `IndexScan` / `IndexIntersection` | Property seeds |
| `NodeScan { label: L }` | **A** paginated `lookup_label_page` → per-shard seeds |
| `NodeScan` + `IsLabeled` (multi-label) | **D** paginated intersection + sieve |
| Label + selective property (`MATCH (n:L) WHERE n.p = v`) | **C1** intersected seeds via `SeedAnchorSet` |
| Large hit lists | Paginate; **no** unseeded all-shard fallback |

### Scale guard — Migrated

**Removed** the policy that fell back to **unseeded multi-shard execution** when label (or
property) hit lists exceed 10_000.

Rationale: unseeded shard scans dominate cost; large seed lists are the lesser evil when vertex
ids are required.

**Retained** optional instruction-bounded caps on:

- `count_postings_by_value` **output group count** (graph-index), and
- C1 aggregate packed `vertex_filter` size (10_000 hits → generic federated path),

not as a silent switch to “no seeds / no fast path.”

### Relationship to label telemetry

| Query / state need | Label postings | Label telemetry |
|--------------------|----------------|-----------------|
| Per-vertex membership | Yes | No |
| Per-shard live count | Derivable (expensive) | Yes (O(1) read) |
| Seed / RETURN vertex list | Yes (export) | No |
| `COUNT(*)` for label only | Overkill | **Primary** |

Both update on DML. Postings are not the default read API for count-only labeled queries.

## Consequences

- Large labels (`Person`) remain materialized for sieve and seed export; **bulk export is rare**
  and purposeful (path A only).
- `GROUP BY` + label uses property machinery + sieve (C), aligning with “property partitions, label
  filters.”
- Count-only labeled queries avoid graph-index hit storms via telemetry (B).
- Router backfill orchestration (`admin_label_backfill_step`) replays historical label postings
  per shard with stable cursors.

## Alternatives considered

- **Tier-1-as-default** (v1 interim): `lookup_label` for every labeled aggregate/seed — rejected;
  wrong for counts and `GROUP BY`; scale guard made it worse.
- **Unified posting key** — rejected (see prior revision).
- **Unseeded fallback for large labels** — rejected; heavier than large seed lists.
- **Label telemetry for `GROUP BY`** — rejected; telemetry has no per-value dimension.

## Implementation order

1. ~~Label postings + DML + `lookup_label`~~ (**done**)
2. ~~**B** — Router/planner fast path for `MATCH (n:L) RETURN count(*)` via telemetry~~ (**done**)
3. ~~**C1** — `filter_hits_by_label` + aggregate/seed wiring for label + property~~ (**done**)
4. ~~**C2** — `count_postings_by_value_for_label` for label + `GROUP BY` property~~ (**done**)
5. ~~**Migrate** — drop unseeded fallback scale guard; stop aggregate fast path via bulk `lookup_label`~~ (**done**)
6. ~~**A** — paginated `lookup_label_page` for seed + vertex-list queries~~ (**done**)
7. ~~**D** — paginated multi-label intersection for seeds~~ (**done**)
8. ~~Backfill orchestration on router~~ (**done**)
9. Optional write-side cardinality policy (future)

## Related documents

- [../index/label-index.md](../index/label-index.md)
- [../index/property-index.md](../index/property-index.md)
- [0003-federated-aggregate-merge.md](0003-federated-aggregate-merge.md)
