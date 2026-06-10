# Label index

Last updated: 2026-06-10  
Implementation verified as of: 2026-06-09 (label index commits through `dc727b13`)

## Status

**Partially Implemented** — Postings, DML sync, `lookup_label` (path **A**), `lookup_label_intersection`
(path **D**), label posting backfill, and router paths **B** / **C1** / **C2** are implemented.
Seed routing no longer falls back to unseeded all-shard execution on large hit lists (see
[ADR 0004](../adr/0004-label-index.md)).

## Purpose

Vertex **label membership** on graph-index: `contains(label, shard, vertex)` and, when necessary,
**vertex-id export** for seeds.

Most labeled queries should **not** bulk-export membership. Use:

- **Property index + label sieve** when properties participate.
- **Label telemetry** when only counts are needed.
- **`lookup_label`** only when a **vertex list** is required.

## Non-goals

- Edge label postings.
- Bulk `lookup_label` as the default for large labels on every query shape.
- Replacing label telemetry for count-only queries.

## Posting model

```text
LabelPostingKey { vertex_label_id, shard_id, vertex_id }
```

Separate `BTreeSet` from property postings.

## Read paths

### A — Vertex list export (`lookup_label`) — Implemented

| API | When |
|-----|------|
| `lookup_label(label_id)` | Seeds, `RETURN n`, any plan that needs explicit `(shard_id, vertex_id)` |

**Target policy:** No fallback to unseeded all-shard execution based on hit count. Large lists are
acceptable when vertex ids are required; unseeded graph scans are worse.

**Not for:** `COUNT(*)` without vertices; `GROUP BY` indexed property (use C).

### B — Label telemetry — Implemented (router)

| Source | When |
|--------|------|
| `vertex_label_shard_live_count`, `vertex_label_stats` | `MATCH (n:L) RETURN count(*)` and other **count-only** shapes |

Updated from graph `LabelUsageDelta` on DML. No graph-index call.

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

`lookup_label_intersection` when the plan needs explicit ids for `:L1:L2:…` (router
`IndexAnchor::LabelIntersection` from `NodeScan` + `IsLabeled` filters).

## Write path — Implemented

`label_pending` + graph-index `label_posting_insert/remove` on label DML.

**Backfill:** `backfill_label_postings` on graph shards (router-guarded, cursor-based) replays
`VertexLabelStore` into graph-index for pre-existing data.

## Router (target)

| Query need | Path |
|------------|------|
| Seed / return vertices for `:L` | A |
| `COUNT(*)` for `:L` only | B |
| `:L` + indexed property filter / `GROUP BY` | C1 / C2 |
| Property only | Property index (no label) |

### v1 code (to migrate)

| Behavior | Target |
|----------|--------|
| Aggregate fast path via `lookup_label` + packed filter | C2 or C1 |
| Scale guard → unseeded multi-shard fan-out | **Remove** |
| Scale guard on aggregate packed filter | Replace with C2; no silent unseeded fallback |

Keep instruction/output bounds on canister APIs where the platform requires them; do not treat
10k hits as “give up on seeds.”

## Query → path cheat sheet

| GQL sketch | Path |
|------------|------|
| `MATCH (n) WHERE n.p = v` | Property |
| `MATCH (n:L) RETURN n` | A |
| `MATCH (n:L) RETURN count(*)` | B |
| `MATCH (n:L) GROUP BY n.p` | C2 |
| `MATCH (n:L) WHERE n.q = v …` | C1 (+ property) |

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
| `count_postings_by_value_for_label(p, L)` (planned) | property | same bucket `range` as above |

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
| A export | label `range` | — |
| C1 `filter_hits_by_label` | — (input hits from property `range`) | label `contains` per hit |
| C2 `count_postings_by_value_for_label` | property bucket `range` | label `contains` per posting |
| v1 interim packed filter | property bucket `range` | `HashSet` from prior label export (migrate to C2) |

### Why not `range` for both sides?

A label **membership test for one vertex** is a single key, not a prefix. Walking the full label
bucket to intersect with a small property hit set is correct only when the label bucket is the
**smaller** side; for `GROUP BY` the property bucket is the natural scan axis (values are already
grouped in key order). Default: **property `range` + label `contains`**.

Property-only `lookup_intersection` uses multiple property `range`s plus in-memory set
intersection — a different pattern, documented in [lookup-intersection.md](lookup-intersection.md).

## Related documents

- [ADR 0004](../adr/0004-label-index.md)
- [property-index.md](property-index.md)
- [../adr/0003-federated-aggregate-merge.md](../adr/0003-federated-aggregate-merge.md)
