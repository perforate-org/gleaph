# Index lookup intersection

## Status

**Partially Implemented** — graph-index `lookup_intersection`, graph executor single-call path, router `IndexAnchor` + `lookup_intersection` seeds, and graph skip of leading `IndexIntersection` are implemented. Client-side intersection in the executor is removed.

## Purpose

Define **`lookup_intersection`** on the graph-index canister: intersect equality postings for multiple indexed vertex properties in one index-local operation, returning `PostingHit` rows for Router per-shard slicing.

## Non-goals

- Planner changes (`PlanOp::IndexIntersection` shape stays).
- Range predicates inside intersection (v1: equality only).
- Logical vertex id intersection (index uses physical `(shard_id, vertex_id)` keys).
- Graph canister calls during intersection.

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

## Graph executor (transition)

**Target:** graph does not call index for intersection on the federation hot path.

**Standalone transition:** until Router performs lookup, graph may call `lookup_intersection` via `PropertyIndexLookup` once, then bind only hits where `shard_id == local` — no client-side set intersection, no `resolve_logical_at` on index hits.

Remove from `execute_index_intersection`:

- N× `lookup_equal` loop
- Client-side `IntSet` intersection
- `placement::resolve_logical_at` for foreign hits

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
5. Remove legacy client-side intersection (**Done**); defer graph direct index calls when router seeds present (**Partial** — skip op only).

## Related documents

- [property-index.md](property-index.md)
- [../sharding/federation-target.md](../sharding/federation-target.md)
- [../execution/operators.md](../execution/operators.md)
