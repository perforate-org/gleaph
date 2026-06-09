# 0004. Label membership index on graph-index

Date: 2026-06-08
Status: proposed

## Context

[ADR 0003](0003-federated-aggregate-merge.md) added a federated aggregate **index fast path**:
`graph-index::count_postings_by_value` walks property postings and the router returns merged
`GqlQueryResult` without shard `PlanOp::Aggregate` execution.

**Implemented fast-path prefixes:**

- Empty or unlabeled `NodeScan` — count all postings in a property bucket.
- Single `IndexScan` / `IndexIntersection` — restrict counts to index seed hits
  (`vertex_filter_packed` on `count_postings_by_value`).

**Still on the generic shard-merge path:**

```gql
MATCH (n:Person)
RETURN n.country, COUNT(*)
GROUP BY n.country
```

Property postings record `(property_id, encoded_value, shard_id, vertex_id)` but **not** vertex
label membership. `MATCH (n:Label)` is expressed as `PlanOp::NodeScan { label: Some(...) }`; the
router has no global vertex set for that label.

Router **label telemetry** (`LabelUsageDelta`, `vertex_label_shard_live_count`) tracks
**aggregate** live counts per label per shard. It does not materialize `{ shard_id, vertex_id }`
membership and cannot drive `GROUP BY` on an indexed property within a label.

Graph shards store labels in `VertexLabelStore` (`crates/graph/src/facade/stable/vertex_labels.rs`)
and sync **property** postings to graph-index via `graph/src/index/pending.rs`. There is no
parallel label posting sync today.

`PLAN_WIRE_VERSION` remains **1**. This ADR adds graph-index read/write APIs and graph DML
sync; it does not change the plan wire format in v1 (router detects `NodeScan.label` from
existing `PlanOp`s).

## Decision

Add a **label membership index** on the **same graph-index canister** as property postings, using
a separate stable `BTreeSet` and the same shard-ownership invariants as property postings.

### Posting model

```text
LabelPostingKey { vertex_label_id, shard_id, vertex_id }
```

Lexicographic order: `vertex_label_id → shard_id → vertex_id`.

- One posting per **(label, vertex)** pair. Multi-label vertices appear in multiple label buckets.
- `vertex_label_id` is the router/graph catalog id ([`VertexLabelId`](../../crates/graph-kernel/src/entry/label.rs)), resolved at plan time via `ResolvedLabelTable`.
- No encoded-value dimension (membership is boolean per label).

Property and label postings are **independent sets**; do not overload `PostingKey` with a
synthetic property id for labels.

### graph-index APIs (v1)

| API | Kind | Role |
|-----|------|------|
| `label_posting_insert(shard_id, label_id, vertex_id)` | update | DML sync from owning graph shard |
| `label_posting_remove(shard_id, label_id, vertex_id)` | update | Label remove / vertex delete |
| `lookup_label(label_id) -> Vec<PostingHit>` | query | All vertices with label globally |

`PostingHit` is reused unchanged (`{ shard_id, vertex_id }`).

**v2 (planned, not required for initial merge):**

- `lookup_label_intersection(label_ids)` for multi-label `NodeScan`.
- `count_postings_by_value_for_label(property_id, label_id, min_count)` — bucket scan with
  label filter inside the canister (avoids shipping huge hit lists to the router).

### Graph shard maintenance

Mirror property posting sync (`pending.rs`):

1. Add `graph/src/index/label_pending.rs` (or extend pending with a label queue).
2. On `insert_vertex`, `set_labels` / `add_label` / `remove_label`, and `delete_vertex`, enqueue
   `label_posting_insert` / `label_posting_remove` for the affected `(label_id, vertex_id)` pairs.
3. `flush_pending` uses the same compensate-and-retry batch semantics as property postings.

**Invariant:** Label postings reflect live label membership. graph-index does not read graph
tombstones on the query path; stale postings are a sync bug (same as property index).

### Router integration

**Seed routing (future extension):** `lookup_label` can seed `NodeScan { label: Some(L) }` the same
way `lookup_equal` seeds `IndexScan` — slice hits by `shard_id`, build `seed_bindings_blob`.
Not required for aggregate fast path v1.

**Aggregate fast path ([ADR 0003](0003-federated-aggregate-merge.md)):**

Extend `try_aggregate_index_fast_path` prefix eligibility:

| Prefix | Vertex filter source |
|--------|----------------------|
| `NodeScan { label: None }` | None (all postings) |
| `NodeScan { label: Some(L) }` | `lookup_label(resolve(L))` → `vertex_filter_packed` |
| `IndexScan` / `IndexIntersection` | Existing seed lookup (unchanged) |

Flow for `MATCH (n:Person) … GROUP BY n.country`:

1. Router resolves `Person` → `VertexLabelId` via label catalog.
2. `lookup_label(person_id)` → `PostingHit` list.
3. `count_postings_by_value(country_id, min_count, pack(hits))` → `GqlQueryResult`.

**Combined label + property seed (v2):** `MATCH (n:Person) WHERE n.region = 'US' GROUP BY n.country`
requires `vertex_filter = lookup_label(Person) ∩ lookup_equal(region, US)`. Initial implementation
may intersect packed sets on the router; move to graph-index when hit sets are large.

**Scale guard (v1):** If `lookup_label` exceeds an instruction/size budget, fall back to generic
shard aggregate merge (same pattern as oversized seed lists).

### Relationship to label telemetry

Keep **label telemetry** (`LabelTelemetryEventWire`, router `LabelStats`). Telemetry remains
O(1) cardinality metadata for DML and ops; label postings are the **membership** source for
routing and aggregate fast path. Both should be updated on DML; optional drift checks are out of
scope for v1.

### Documentation

- Steady-state detail: [../index/label-index.md](../index/label-index.md) (Planned).
- Property index sibling: [../index/property-index.md](../index/property-index.md).

## Consequences

- `MATCH (n:Label) RETURN … GROUP BY indexed_prop, COUNT(*)` can use the index fast path without
  shard-local aggregation when other eligibility rules hold.
- graph-index stable layout gains a second posting set; migrations must initialize empty label
  postings on upgrade (or backfill — backfill strategy TBD at implementation time).
- DML paths gain label posting sync latency and failure modes identical to property postings.
- Large labels may require v2 in-canister `count_postings_by_value_for_label` to avoid routing
  megabyte-scale hit lists.

## Alternatives considered

- **Pseudo-property labels** — store labels as a reserved `property_id` in `PostingKey`.
  Rejected: blurs catalog semantics; awkward for multi-label and `value` encoding.
- **Label telemetry only** — extend `LabelUsageDelta` for GROUP BY. Rejected: no per-vertex
  membership; cannot restrict property bucket counts to a label.
- **Per-shard label scans on query** — ask each graph shard for local label membership.
  Rejected: defeats federation fast path and duplicates index responsibility.
- **Separate label-index canister** — rejected for v1; same router client and shard ownership
  model as property index is sufficient.

## Implementation order (suggested)

1. `LabelPostingKey` + stable set + `label_posting_insert/remove/lookup_label` + tests.
2. Graph `label_pending` + DML hooks + IC flush (mirror property pending).
3. `RouterIndexClient::lookup_label` + `IndexLookup` trait extension.
4. Fast path: `NodeScan { label: Some }` → label filter on `count_postings_by_value`.
5. Design status updates; optional backfill job for existing graphs.
