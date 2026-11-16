# 0123. Shard-local label postings for labeled NodeScan

Date: 2026-11-16
Status: accepted (2026-11-16), implemented.
Last revised: 2026-11-16

## Context

A labeled `NodeScan` of an unbound variable (`MATCH (g:Genre)`) runs inside one Graph shard.
Before this ADR, `node_scan_vertices` (`crates/graph/src/plan/query/executor/scan.rs`) visited every
vertex id from `0` to `vertex_count`, read the vertex row, and asked the label store whether the
vertex carried the label. Vertex rows carry no labels; the only per-shard label state was
`VERTEX_LABEL_SETS` (MemoryId 32), a `StableBTreeMap<u32, VertexLabelSetBlob>` keyed by vertex id.
The scan cost therefore grew with the shard's vertex count, not with the label's cardinality.

While profiling the movie demo, a Router-seeded `MATCH` with several disconnected label-only
patterns cost billions of cycles. Commits 418420da and 4913fc4b removed the row multiplication;
each labeled scan still read every vertex.

### Measured evidence

canbench, `crates/graph/src/bench/node_scan.rs`, 10,000 vertices, 16 of them `BenchRareLabel`:

| Measurement (before) | Instructions |
|---|---|
| `bench_graph_node_scan_rare_label_10k` (full executor path) | 237.44 M |
| `label_checks`: `vertex_has_label` for all 10,000 ids | 233.98 M (~23.4 K per vertex) |
| `vertex_row_reads`: `GraphStore::vertex` for all 10,000 ids | 4.03 M |

The label check dominated. To test option (b) below, the per-vertex check was re-pointed at a
key-only probe (`contains_key((label, vertex))` on a map with no value to decode). It measured
**289.84 M** for the same 10,000 probes, more than the `get` plus blob decode it replaced. The
per-vertex cost is the stable B-tree descent itself, not the blob decode, so no cheaper per-vertex
check can make a scan over every vertex id cheap.

## Options

**(a) A label -> vertex posting structure** that a labeled scan reads directly, so the scan visits
only matching vertices.

**(b) A cheaper per-vertex label check** (avoid the blob decode, a key-only probe, or labels inline
on the vertex row). The measurement above shows the decode is not the cost. Putting labels back on
the CSR row would reverse the move of labels to the label store, cap labels per vertex, and still
leave an O(vertex count) scan.

## Existing owners considered

- **Graph-index label postings** ([ADR 0004](0004-label-index.md)): `LabelPostingKey { label,
  shard_id, vertex_id }` in the graph-index canister, maintained from Graph DML through
  `label_pending` only when federation is configured. A shard's synchronous `NodeScan` cannot call
  another canister. ADR 0004 itself reserves vertex-list export for cases that cannot be avoided,
  so routing every labeled scan through it is not an option either. It stays the federated owner
  for Router seeding.
- **Label stats projection** ([ADR 0015](0015-label-stats-projection-log.md)): per-label counts
  shipped to the Router as deltas. Counts only, no membership.
- **`VertexLabelStore`** (`crates/graph/src/facade/stable/vertex_labels.rs`): the shard's single
  owner of vertex label membership, written only through `set_labels`. This is the owner that the
  inverse direction belongs to.

## Decision

1. **`VertexLabelStore` owns membership in both directions.** Beside `sets` (vertex -> sorted
   label set, MemoryId 32, canonical), it keeps `postings`: a key-only
   `StableBTreeMap<LabelPosting, ()>` with `LabelPosting { label: u16, vertex: u32 }`, encoded as 6
   big-endian bytes and ordered `(label, vertex)`. It lives in the new MemoryId 55
   `VERTEX_LABEL_POSTINGS`, registered `Derived`, `RebuildPath::SyncCoUpdate`, `Rebuildable`.
2. **One writer.** `VertexLabelStore::set_labels` validates the new set first, then removes the
   postings for labels the vertex lost, inserts the ones it gained, and writes or removes the set
   row. Insert, `SET`/`REMOVE` label, and vertex delete (`commit_clear_vertex_labels`) all go
   through it, so `postings` is always the exact inverse of `sets`. A rejected label set writes
   neither map. No other code path writes either map.
3. **A labeled scan of an unbound variable reads postings.** `node_scan_vertices` reads
   `GraphStore::vertices_with_label`, which pages through the label's key range 256 keys at a time
   per re-entry of the store. The scan stays lazy, so `LIMIT` pushdown still stops early, and its
   output is still in ascending id order. Each hit's vertex row is still read for the tombstone
   check, the same check the unlabeled scan makes.
4. **Per-vertex membership stays on `sets`.** `vertex_has_label` (a scan of a bound variable,
   `IS LABELED`, canonical export, vector-ingest label gate) keeps reading the label set, which
   measured cheaper than the posting probe.
5. **No rebuild entry point and no migration.** The project is pre-production: the new region
   starts empty on a fresh install, and the postings fill as labels are written. Any state that
   predates this layout needs a reinstall.
6. **The dead row shims are gone.** `Vertex::primary_label_id`, `has_label_sidecar`,
   `with_primary_label_id` and `with_label_sidecar` always returned `None`/`false`/`self` after labels
   moved to the label store. They and the `vertex: Vertex` parameter they fed through the label API
   are deleted. `apply_vertex_label_transition` no longer rewrites an unchanged vertex row through
   `set_vertex`. It now checks that the row exists before writing labels.

## Consequences

Measured after the change (canbench, same fixture):

| Measurement (after) | Instructions |
|---|---|
| `bench_graph_node_scan_rare_label_10k` (16 hits) | 1.05 M (`plan_query_execute_ops` 88.5 K, from 236.49 M) |
| `bench_graph_node_scan_common_label_10k` (9,984 hits), `plan_query_execute_ops` | 39.45 M (~4 K per hit; not measured before the change, when every labeled scan checked all 10,000 ids) |
| `label_postings`: the rare label's posting read | 42.8 K |

- **Write cost:** each label gained or lost costs one stable B-tree insert or remove of a 6-byte
  key. `set_labels` takes the replaced set from the set write's return value, so the posting diff
  adds no extra read. Measured against HEAD 4913fc4b (canbench, total instructions):

  | Bench | HEAD | After | Delta |
  |---|---|---|---|
  | `bench_ordered_vertex_whole_batch_256` | 14.74 M | 23.58 M | +60% (~34 K per labeled vertex) |
  | `bench_graph_vertex_insert_bulk_16` | 1.25 M | 1.53 M | +22% |
  | `bench_graph_canonical_segment_insert_bundle_16` | 0.91 M | 1.19 M | +31% |
  | `bench_graph_canonical_segment_insert_vertex` | 159.25 K | 171.19 K | +7% |
  | `bench_graph_delete_detached_vertex` | 72.34 K | 81.45 K | +13% |

  One labeled insert costs about as much as 1.5 vertex probes in the old scan, and every labeled
  scan of the label saves one probe per non-matching vertex in the shard. Stable memory grows by one
  8-page bucket when the region is first written. The same comparison shows the scan-heavy benches
  getting cheaper, for example `bench_graph_hop_count_shortest_frontier_queue_branch4_depth4`
  2.50 M -> 417.37 K and `bench_graph_match2_insert_10k` 480.52 M -> 1.72 M.
- **Space:** about 6 bytes of key per (label, vertex) pair plus B-tree overhead, in an 8-page
  bucket region like region 32.
- **Encapsulation:** the posting map is private to `VertexLabelStore`. Callers see only
  `vertices_with_label` (a paged read) and the existing `set_labels` writer.
- **Invariant:** `postings == { (l, v) | l ∈ sets[v] }`. Unit tests in `vertex_labels.rs` check it
  after insert, label change, label removal, delete, a rejected write, and reopen. The facade test
  `labeled_node_scan_follows_label_insert_change_and_delete` checks scan results and visit counts
  through the `GraphStore` API.

## Re-evaluation

Revisit if a labeled scan needs a different order than vertex id (for example, with a
property-ordered index), or if label writes become a measured bottleneck in bulk load.
