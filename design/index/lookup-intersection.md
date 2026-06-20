# Index lookup intersection

## Status

**Implemented** — graph-index `lookup_intersection` returns `IndexIntersectionResult` (vertex, mixed, and all-edge arms per ADR 0009). Router seeds vertices (`PostingHit`) and edges (`LocalEdgePosting` via `EdgeIndexScan` / all-edge intersection). Graph skips leading `IndexIntersection` / `EdgeIndexScan` when seeded. Shard-local `EDGE_EQUALITY_POSTINGS` retired (ADR 0009 phase D). The vertex-only intersection query path is **streamed** (paged walk + `filter_hits_by_equal` `contains` sieve) so it no longer materializes a full posting bucket per arm; edge/mixed intersection still materializes server-side — see [Streaming intersection status](#streaming-intersection-status).

## Purpose

Define **`lookup_intersection`** on the graph-index canister: intersect equality postings for multiple indexed vertex properties in one index-local operation, returning `PostingHit` rows for Router per-shard slicing.

## Non-goals

- Planner changes (`PlanOp::IndexIntersection` shape stays) — *vertex-only v1*.
- Range predicates inside intersection (v1: equality only).
- Logical vertex id intersection (index uses physical `(shard_id, vertex_id)` keys).
- Graph canister calls during intersection.

**Implemented ([ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md) phase C):** `IndexSubject` on
each arm; mixed vertex+edge intersection projects edge owners to vertex hits; all-edge arms return
`Edges(Vec<EdgePostingHit>)`.

## Problem

Queries such as:

```gql
MATCH (n:User WHERE n.uid = 'alice' AND n.email = 'alice@example.com') RETURN n
```

emit `PlanOp::IndexIntersection` with two indexed equalities. Intersecting posting lists belongs to the **index data plane**, not graph CSR traversal.

## Execution and Data Boundaries

| Step | Boundary |
|------|----------|
| Detect intersection in plan | `gleaph-gql-planner` (unchanged) |
| Resolve property names → ids, params → encoded bytes | **Router** (target) or graph during standalone transition |
| Posting lookup + intersect | **graph-index** |
| Slice hits by `shard_id`, build seeds | **Router** |
| Bind seeded local vertices, execute plan | **Graph shard** |

Intersection completes **entirely inside graph-index** — no graph canister query. Graph/Router only invoke the index canister API.

## Wire types (`graph-kernel`)

Add to `crates/graph-kernel/src/index.rs`:

```rust
/// One equality arm. `value` is the sortable index key (`value_to_index_key_bytes`).
pub struct IndexEqualSpec {
    pub property_id: u32,
    pub value: Vec<u8>,
}

/// At least two specs; v1 supports equality only.
pub struct IndexIntersectionRequest {
    pub specs: Vec<IndexEqualSpec>,
}
```

Response: `Vec<PostingHit>` (existing type).

## Canister API (`graph-index`)

```rust
#[query]
fn lookup_intersection(req: IndexIntersectionRequest) -> Vec<PostingHit>;
```

### Algorithm (index store)

For each spec in `req.specs`:

1. Range scan postings with prefix `(property_id, value)` — same bounds as `lookup_equal` (`PostingKey::prefix_lower` / `prefix_upper`).
2. Collect set of keys `(shard_id, vertex_id)` (e.g. pack into `u64`).

Intersect sets starting from the smallest set. Emit `PostingHit` for surviving keys.

**Complexity:** O(Σ |postings_i|) time; no graph access.

### Streaming intersection status

The equality export reads (`lookup_equal` / `lookup_range` / `lookup_edge_equal`) are paginated via
`*_page` APIs so query paths honor the **no full-bucket heap materialization** invariant
([capacity-planning.md](capacity-planning.md), [property-index.md](property-index.md#read-apis)).

**All-vertex intersection is streamed (Implemented).** The planner's `IndexIntersection` is
vertex-only, and the query consumers no longer call the materializing `lookup_intersection` for that
shape. Instead they page the first arm (`lookup_equal_page`) and sieve each page against the
remaining arms with `filter_hits_by_equal` (a per-hit `contains` check), so the index never builds an
in-heap set for any arm. This mirrors the label path
(`collect_label_intersection_hits_for_shards` in `router/src/federation/label_export.rs`). The
streaming composition lives in:

- Router: `collect_vertex_intersection_hits_paged` in `router/src/index_lookup.rs`, used by both
  `IndexLookup::lookup_intersection` impls (`RouterIndexClient`, `RouterIndexLookup`) when
  `all_vertex_specs(&specs)` holds.
- Standalone graph: `IcPropertyIndexClient::collect_vertex_intersection_hits` in
  `graph/src/index/ic.rs`.

The walk arm is the first spec (callers order arms; matches the label precedent — no size-based
smallest-arm selection yet, since there is no cheap per-`(property,value)` cardinality signal).

**Remaining gap (edge / mixed intersection):** the server-side `lookup_intersection` still
**materializes one in-heap set per arm** for all-edge and mixed vertex+edge intersection (used by
`EdgeIndexScan` / all-edge arms, not the vertex-only planner op), and `lookup_label_intersection`
retains its materializing server impl (the router label query path already streams via
`filter_hits_by_label`). Extending the same walk + `contains` sieve to edge owners (a prefix-existence
check per arm) is the remaining work; it is **not yet implemented**.

### Validation

- `specs.len() < 2` → return empty vec (caller should use `lookup_equal`).
- Unknown `property_id` is not an error; empty posting list for that arm yields empty intersection.

## Index invariants and tombstones

Index does **not** read graph tombstone state.

**Invariant:** DML on graph shards keeps index postings consistent:

- Property delete / vertex delete → `posting_remove(shard_id, property_id, value, vertex_id)`.
- Live vertices only appear in postings.

Query read paths do not filter tombstones when this invariant holds. See [../sharding/standalone-mode.md](../sharding/standalone-mode.md).

## Router integration

```text
anchor = IndexAnchor::from_plans(plans)  // IndexScan or IndexIntersection
hits = lookup_equal / lookup_intersection(anchor)
for each shard_id in participating_shards:
  local_ids = hits where hit.shard_id == shard_id
  seed_blob = encode(variable, local_ids)
  execute_plan_on_graph(shard, plan_blob, seed_blob)
  // graph skips leading IndexScan / IndexIntersection op
```

**Implemented** in `router/src/seed.rs` (`IndexAnchor`), `router/index_client.rs`, and `gql.rs` dispatch.

## Graph executor

**Federated wire path (implemented):** graph shards called via `execute_plan_query` do not hold an index client when `seed_bindings_blob` is set or federation routing is configured (`handlers.rs`). Leading `IndexScan` / `IndexIntersection` / labeled `NodeScan` / `PropertyFilter` anchors are skipped when seeds are present (`skip_leading_index_anchor_ops`). Without seeds, federated shards return `IndexScan(no index client)` / `IndexIntersection(no index client)`.

**Standalone / native dev:** graph may still call `lookup_intersection` via `PropertyIndexLookup` when an index client is wired and federation routing is absent. Hits are filtered to `shard_id == local` — no client-side set intersection on the hot path.

**Transition cleanup:** graph `execute_index_intersection` calls `lookup_intersection` once (no client-side N× `lookup_equal` loop). Federated wire dispatch supplies router seeds so shards skip the op.

## Clients

| Client | Method |
|--------|--------|
| `RouterIndexClient` | `lookup_intersection` (wasm IC) |
| `PropertyIndexLookup` (graph) | `lookup_intersection` — transition only |
| `MockPropertyIndex` (tests) | delegate to store or test vectors |

## Tests

| Layer | Cases |
|-------|--------|
| `graph-index` | two/three specs, disjoint intersection, single hit, empty arm |
| graph executor | mock index; seeded skip op (future) |
| planner | existing `IndexIntersection` plan shape tests (unchanged) |

## Implementation phases

1. `graph-kernel` types + `graph-index` store + canister endpoint + unit tests.
2. Extend `PropertyIndexLookup` / `RouterIndexClient`.
3. Graph executor: single `lookup_intersection` call; standalone bind filter.
4. Router seed module: intersection anchor + per-shard slice + skip op on graph (**Implemented**).
5. Remove legacy client-side intersection (**Done**); defer graph direct index calls when router seeds present (**In progress** — `skip_leading_index_anchor_ops` skips leading `IndexScan`, `IndexIntersection`, labeled `NodeScan`, and `PropertyFilter`; executor tests in `executor/scan/tests.rs`).

## Related documents

- [property-index.md](property-index.md)
- [../sharding/federation-target.md](../sharding/federation-target.md)
- [../execution/operators.md](../execution/operators.md)
