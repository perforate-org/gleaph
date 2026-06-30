# Index lookup intersection

Last updated: 2026-06-30
Anchor timestamp: 2026-06-30 16:03:13 UTC +0000

## Status

**Implemented** — graph-index `lookup_intersection` returns `IndexIntersectionResult` (vertex, mixed, and all-edge arms per ADR 0009). Router seeds vertices (`PostingHit`) and edges (`LocalEdgePosting` via `EdgeIndexScan` / all-edge intersection). Graph skips leading `IndexIntersection` / `EdgeIndexScan` when seeded. Shard-local `EDGE_EQUALITY_POSTINGS` retired (ADR 0009 phase D). The vertex-only intersection query path is **streamed** (paged walk + `filter_hits_by_equal` `contains` sieve) so it no longer materializes a full posting bucket per arm; edge/mixed intersection still materializes server-side — see [Streaming intersection status](#streaming-intersection-status). N-way vertex equality intersection for `SEARCH ... WHERE` (ADR 0034 Slice 13) supports 2..=8 arms with deterministic walk-arm selection. One to eight equality arms combined with a single numeric range on a distinct property for `SEARCH ... WHERE` (ADR 0034 Slice 14) are executed through the streamed `lookup_range_intersection_page` path, which walks the finite encoded range one page at a time and sieves each page against every equality arm server-side while preserving the range cursor.

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
shape. Instead they loop the server-side **`lookup_intersection_page`** query: the index walks the
first arm one page at a time (`lookup_equal_page` bounds) and sieves each page against the remaining
arms in-heap via the merge-join `filter_hits_by_equal`, returning a bounded page of survivors plus
the walk-arm cursor. The index never builds an in-heap set for any arm, and the walk + sieve fold
into **one inter-canister message per page** instead of one `lookup_equal_page` call plus one
`filter_hits_by_equal` call per arm per page. The consumers are:

- Router: `collect_vertex_intersection_hits_paged` in `router/src/index_lookup.rs` loops
  `RouterIndexClient::lookup_intersection_page`, used by both `IndexLookup::lookup_intersection`
  impls (`RouterIndexClient`, `RouterIndexLookup`) when `all_vertex_specs(&specs)` holds.
- Standalone graph: `IcPropertyIndexClient::collect_vertex_intersection_hits` in
  `graph/src/index/ic.rs` loops the same `lookup_intersection_page` query.

`lookup_intersection_page` returns an empty terminal page for fewer than two specs, and an explicit
[`IndexError::TooManyEqualityIntersectionArms`] for more than
`MAX_EQUALITY_INTERSECTION_ARMS` (8) specs; it also returns an empty terminal page for any non-vertex
spec. Callers guard with `all_vertex_specs` and fall back to the materializing `lookup_intersection`
otherwise. The walk arm is selected by canonical `(property_id, encoded_value)` order so that paging is
deterministic regardless of how the caller ordered the specs. `filter_hits_by_equal` remains an internal
store method (no longer a canister endpoint).

**Edge / mixed intersection (dormant — intentionally not streamed).** The server-side
`lookup_intersection` still **materializes one in-heap set per arm** for all-edge and mixed
vertex+edge intersection. This path is **unreachable from GQL today**: `PlanOp::IndexIntersection`'s
`IndexScanSpec` has no edge subject and the planner binds the variable as a vertex, so only the
vertex-only shape is generated (`IndexEqualSpec::edge(..)` appears only in tests/benches). Because
there is no live consumer, and because the edge key orders `label_id` before `shard_id`/`owner` (so
**unlabeled** edge-owner existence is not a contiguous range), streaming is deliberately deferred —
see the §3 status note in [ADR 0009](../adr/0009-edge-property-index-and-index-ddl.md). Revisit when a
planner change introduces an edge-led or mixed `IndexIntersection`. `lookup_label_intersection`
likewise retains its materializing server impl (the router label query path already streams via
`filter_hits_by_label`).

### Benchmarks

`crates/graph-index/src/bench.rs` (canbench, one shard, walk arm = 1024 ids with `vid % 8` in `{0,1}`,
every sieve arm contains the same 1024 ids, so the intersection is also 1024 ids and the per-arm
merge shape is identical across 2/4/8 arms; `canbench_results.yml` baseline):

| Benchmark | Instructions | Heap increase |
|-----------|--------------|---------------|
| `bench_lookup_intersection_two_arms` (materializing, server-side) | 9.41 M | 1 page |
| `bench_lookup_intersection_page_two_arms` (server-side paged walk + one-arm dense sieve) | 7.90 M | 0 pages |
| `bench_lookup_intersection_page_four_arms` (server-side paged walk + three-arm dense sieve) | 16.00 M | 0 pages |
| `bench_lookup_intersection_page_eight_arms` (server-side paged walk + seven-arm dense sieve) | 32.22 M | 0 pages |
| `bench_lookup_intersection_page_eight_arms_scattered` (server-side paged walk + seven-arm point-lookup sieve) | 201.64 M | 0 pages |
| `bench_lookup_equal_page_walk_arm` (one streamed walk page) | 3.89 M | 0 pages |
| `bench_filter_hits_by_equal_page` (merge-join sieve over the 1024-hit walk page) | 4.00 M | 0 pages |

**Result:** the streamed `lookup_intersection_page` path stays heap-bounded (0 pages) and scales
linearly with the number of sieve arms when the walk page and sieve arms keep the same shape:
two-arm ≈ 7.90 M, four-arm ≈ 16.00 M, eight-arm ≈ 32.22 M. Because the fixture now holds every arm at
1024 postings inside a 4096-id range, all dense sieves exercise the same bounded merge-join over the
same walk page rather than switching to point lookups or shrinking the walk bucket. The prior 24.3 M /
24.8 M / 39.9 M / 48.1 K baselines came from fixtures where the walk arm itself shrank with arm count
or several arms became empty mid-intersection, so they did not compare arm count in isolation.

The scattered 8-way shape (only one vertex survives across all arms and the walk hits are far apart)
measures the point-lookup fallback path at 201.64 M instructions; this is intentionally separate from
the dense series because it exercises a different code path inside `filter_hits_by_equal`.

### Validation

- `specs.len() < 2` → return an empty terminal page (callers must use `lookup_equal_page` for one
  arm and `lookup_intersection` for non-vertex arms).
- `specs.len() > 8` → return [`IndexError::TooManyEqualityIntersectionArms`] (callers must reject
  nine-or-more equality arms before calling).
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
