# 0136. lara-graph replaces ic-stable-lara as the Graph shard's edge store

Date: 2026-11-25
Status: proposed
Last revised: 2026-11-25 (lara-graph cdad0e6: proxies, bucket fills, every-label reads, value
layout changes)
Anchor timestamp: 2026-11-25 14:40:22 UTC +0900

## Context

A Graph shard keeps its topology and inline edge values in `crates/ic-stable-lara`
(`DeferredBidirectionalLabeledLaraGraph`), reached only through the `GraphStore` facade in
`crates/graph/src/facade/**`. The store is two separate one-orientation graphs (forward and
reverse), each with slab, spill, free-span and inline-byte regions, joined by a durable deferred
maintenance queue. It takes 33 MemoryIds (0–30, 53, 54).

[lara-graph](https://github.com/perforate-org/lara-graph) is a rewrite of the same idea as one
labeled multigraph store with a different contract:

- **One graph, two halves per edge.** A directed edge is an `Outgoing` half at its source and an
  `Incoming` half at its target; an undirected edge is an `Undirected` half at each end. A bucket
  is `(node, Label, Direction)`; `Label` is a full `u16`, with direction kept apart from it.
- **Positions do not move on removal.** A removal leaves a hole. Positions move only in
  `compact(node, (label, direction))`, which keeps live edges in order, moves at most one page per
  call and returns every move.
- **No deferred work inside the store.** Every update finishes within its call with a bounded
  cost. Compaction (`compact`) and DETACH (`detach`, `detach_step`, `sweep`) are bounded steps
  the caller drives; the store keeps no queue.
- **Values per label.** `declare_width(label, width)` gives every edge of a label a fixed-width
  value (at most 4096 bytes) in both halves. `change_width(label, width)` gives the label a new
  layout and its next `ValueVersion`; buckets keep stored values in their version until
  `migrate_values` rewrites them (Decision 7).
- **Counterparts by lookup.** `counterpart(EdgeRef)` finds the other half through the bucket's
  target index; `Halves`/`EdgeRef::is_canonical` fix the canonical half (directed: the `Outgoing`
  half; undirected: the half at the larger node id, the same rule as
  `canonical_undirected_owner`; either way the local half when the other end is a proxy).
- **Seven memories** (`MemoryRole::ALL`), opened by `init` for a fresh or existing layout. The
  seventh, `Proxies`, holds proxy records (Decision 10) and stays empty, with no header, until
  the first proxy is added.
- **Every label at once.** `visit_all_neighbors(node, direction, order, f)` and
  `ReadBatch::visit_all_neighbors_each` read a node's edges on one direction under all its labels
  in one walk of its label bucket list: labels ascending and positions ascending, or both
  reversed, as `OutEdgeOrder` reads. With 8 labels it costs a third of a loop of
  `visit_neighbors` over `edge_labels` (7.7 M against 21.2 M instructions for 1024 nodes).
- **Removals report bucket fill.** `RemovedEdge::canonical_fill` / `other_fill` give each changed
  bucket's live edges and holes (`BucketFill`), so a caller deciding whether to compact needs no
  further read.
- **Node labels with postings.** `set_node_labels` replaces a node's label set and updates the
  label → node postings in the same call, with no cap on labels per node. `has_node_label` is one
  5-byte read for a node with up to two labels; `visit_nodes_with_label(label, after, f)` visits
  a label's nodes lazily in ascending id order; a node keeps its labels while detaching and loses
  them once deleted.

What Gleaph uses today, from a survey of `crates/graph` and `crates/graph-kernel`:

- **Remote edges are not written.** `VertexRef` reserves a remote bit, but every insert path
  rejects a remote target with `RemoteEdgeNotSupported`, and no code constructs one. Both
  endpoints of every stored edge are local vertex ids below `vertex_count`.
- **Edge handles are physical.** `EdgeHandle(owner, tagged label u16, slot u32)` keys
  `EDGE_PROPERTIES`, property-index postings, index-build fences, canonical-export cursors and
  inline-backfill resume entries. `GlobalEdgeId` carries the same triple to clients, and
  [ADR 0090](0090-edge-element-id-label-attribution.md) already documents it as unstable across
  compaction. Clients never send it back.
- **Edge labels** are catalog `EdgeLabelId` 1..=0x7FFF; the stored key adds the directed bit
  (`BucketLabelKey`, bit 15).
- **Ordering** comes from the Graph Type: `ORDER BY INSERTION` labels append only and back
  `ORDER BY INSERTION(e) DESC … OFFSET` queries; other labels reuse tombstones.
- **Maintenance** runs through a one-shot timer ([ADR 0020](0020-deferred-maintenance-timer-drain.md))
  with a 32 B instruction budget, draining the store's queue (MemoryId 30): remove-side
  compaction of append-only buckets, bulk-ingest finalize, DETACH pre-compaction and purge, and
  store-internal leaf, slab, bypass-row and width-materialize items.
  `PENDING_VERTEX_PURGES` (MemoryId 40) hides a purging vertex's edges until the purge completes.
- **Inline property widths** are declared by the Router per catalog label (scalars 1–64 bytes,
  STRUCTs up to 4096 bytes) and set per bucket in ic-stable-lara. Once a label has an inline
  schema, the Router rejects a different one (`InlineSchemaConflict`,
  `crates/router/src/facade/stable/edge_inline_property_profiles.rs`); only a label without one
  may gain one.
- **Vertex labels** live in `VertexLabelStore`: label sets in MemoryId 32 and the label → vertex
  postings of [ADR 0123](0123-shard-local-label-postings.md) in MemoryId 55, both stable B-trees.
  A membership check is a B-tree descent (about 23 K instructions per vertex in ADR 0123's
  measurement).

Gleaph is pre-launch: [ADR 0135](0135-pre-launch-format-version-policy.md) requires a fresh
install for a layout change, with no migration.

## Decision

1. **The Graph shard stores its edges in lara-graph.** `crates/graph` and `crates/graph-kernel`
   depend on it through a git dependency pinned by `rev`. `GraphStore` stays the only caller, so
   the change is confined to the facade and the kernel's edge vocabulary. After the cutover
   `crates/ic-stable-lara` is deleted. Existing state needs a fresh install (ADR 0135).

2. **Vertices are lara-graph nodes.** A local `VertexId` maps one to one onto a `NodeId`:
   `add_node` / `add_nodes` return dense ids in creation order, ids are never reused, and
   `node_count` replaces `vertex_count`. A remote vertex that local edges point at is a proxy
   node (Decision 10). The `Vertex` row type and its `Csr*` traits go away.
   - **Tombstones.** The two tombstones of today's rows have no stored counterpart. A removed
     edge is a hole, which reads as no edge, so the edge row's tombstone bit
     (`tombstone_edge`, `is_deleted_slot`) goes. A vertex row's tombstone becomes
     `node_state(id) != Live`: `Detaching` and `Deleted` both read as deleted, as the tombstone
     does today from the start of DETACH, so the `is_tombstone()` callers (scans, backfills,
     `is_vertex_live`, the GQL runner) keep their meaning.
   - **`VertexRef` stays in memory only.** Edge rows no longer store it; the facade still builds
     it as the kernel's `EdgeTarget`, local or remote. The vector page store's 30-bit payload,
     with its own tombstone bit, mirrors it and is untouched.
   - **Capacity.** `MAX_PAYLOAD_VERTEX_ID` (2^30 − 1) stays the shard's local vertex bound: the
     vector page store and federation backfill keep 30-bit ids. lara-graph's local range (below
     2^31) leaves room above it.

3. **Edge labels map onto `(Label, Direction)` at the facade.** lara-graph's `Label` is the
   catalog `EdgeLabelId` (unlabeled is 0). A directed edge is stored as `Outgoing`/`Incoming`; an
   undirected edge as `Undirected`. Gleaph keeps `TaggedEdgeLabelId` as its own encoding for
   `EDGE_PROPERTIES`, postings, fences, wire formats and `GlobalEdgeId`, and translates at the
   `GraphStore` boundary: `0x8000 | l` is `(l, Outgoing)` for the canonical half, and `l` is
   `(l, Undirected)`. No persisted Gleaph key changes shape.

4. **The canonical half is the edge's handle.** `EdgeHandle(owner, tagged label, slot)` is the
   canonical `EdgeRef`'s `(node, label, position)`. Since removals no longer move positions:
   - `EdgeRemoval.moves`, `EdgeSlotMoveObserver` and `DeleteEdgeObserver` are removed. The only
     moves come from `compact`, returned as `Move`s that Gleaph applies to sidecars and postings
     in the same message (`Move::edge_refs` names the canonical half before and after).
   - The rank-based CounterpartScan of [ADR 0048](0048-lara-counterpart-resolution.md) is replaced
     by `counterpart`. A stale handle still fails closed: a hole reads as no edge.
   - `GlobalEdgeId` keeps its format and its "unstable across compaction" contract.

5. **Placement follows the Graph Type.** `ORDER BY INSERTION` labels insert with
   `Placement::Append`, other labels with `Placement::ReuseHoles`. `ORDER BY INSERTION(e) DESC`
   reads with `Order::Descending`; its `OFFSET` uses `nth_position`, which skips whole pages by
   their live counts instead of visiting the skipped edges.

6. **Gleaph owns the maintenance work list.** lara-graph keeps no queue, so the facade keeps one
   in a new MemoryId: detaching nodes, migration targets (Decision 7) and compaction targets
   `(node, label, direction)`, in that order: compacting a bucket cancels a migration under way
   in it, so a bucket listed for both is migrated first. The ADR 0020 timer, its budget and its
   catalog-guard / compact / flush tick stay; a tick runs bounded steps until the instruction
   budget's headroom is reached.
   - **Compaction:** after a removal, each bucket whose `BucketFill` (from the `RemovedEdge`)
     has `holes >= live` is listed, for `Append` and `ReuseHoles` buckets alike. Refilling holes
     alone does not bound a bucket: it keeps the most positions its degree ever reached, and
     that peak creeps up as degrees fluctuate. In lara-graph's `examples/churn.rs` (100 k nodes,
     a sliding window of 800 k edges turned over four times), holes refilled without compaction
     ended at 1.51 times the size of the same edges built fresh and still growing (1.73 times
     with 8 labels); listing at `holes >= live` held it at 1.13 times (1.24 times) from the
     first turn. Deciding from `BucketFill` costs nothing extra; reading `holes` and `degree`
     back cost 30 % more per update (`churn_*_1024` in lara-graph's canbench). A bucket left
     empty is dropped by its compaction, which also keeps label bucket lists short.
   - **DETACH DELETE:** `detach` marks the node; ticks call `detach_step` (and `sweep` for a hub
     bucket shared by several detaching nodes) until `Progress::Done`, then clear the node's bit
     in `PENDING_VERTEX_PURGES`. The DETACH pre-compaction pass is dropped: a purge moves no
     other edge. A plain DELETE of a node with no edges completes in one `detach_step`.
   - **Bulk-ingest finalize** keeps its endpoints. `bulk_load` writes buckets without holes, so
     finalize lists nothing unless a bucket has holes.
   - The store-internal items (dense leaf, inline slab, bypass row, width materialize) have no
     counterpart: lara-graph does that work within the update that causes it.

7. **Widths are declared per catalog label and change by migration.** The facade calls
   `declare_width` the first time a label with a nonzero profile is used; directed and
   undirected edges of the label share it, as the Router's profile already does. A label without
   values may gain a width (edges stored before it read as zero bytes until written), and
   declaring a different width fails (`WidthConflict`, as `InlineSchemaConflict` does in the
   Router). 4096 bytes, the largest STRUCT profile, is lara-graph's `Width::MAX`.
   - **Changing a profile.** lara-graph can change a label's layout since cdad0e6, so the
     Router's ban is no longer a store limit; lifting it is the Router's decision, once the
     facade does the following. `change_width(label, width)` (the same width included, for a
     profile that changes only its meaning) returns the label's next `ValueVersion`, in constant
     time. The catalog keeps each version's profile, so old bytes can be decoded.
   - **Reading.** Buckets that hold values keep them in the version they were written in, and
     reads return them in that width. `bucket_value_version(node, (label, direction))` tells the
     version to decode a bucket's values with, and `RemovedEdge::value_version` a removed
     edge's, for index maintenance on removal. A bucket that holds no values reads zeros of the
     new width.
   - **Writing.** A bucket in an older version, or one being migrated, takes no insert or value
     write (`Error::StaleValues`). The facade then migrates that bucket in the same message when
     it finishes within the budget headroom (a small bucket takes one or two steps) and retries;
     otherwise it fails the write retryably and puts the bucket first in the work list.
   - **Migrating.** `migrate_values(node, (label, direction), transform)` rewrites one value page
     per call through Gleaph's transform (old bytes to new, given zeroed) and returns
     `Progress`, like `compact`. Reads keep the old values until the last step puts the new ones
     in place, so a bucket is in one version at a time, and a migration under way survives an
     upgrade. After a change, the work list steps through the nodes, asks `needs_migration` for
     the label's buckets in each direction (both halves of an edge are migrated, each at its
     node), and migrates those that need it.
   - **One pass at a time.** A small bucket records only its version's low three bits, so a
     label may change at most seven more times while a bucket of an older version is left. The
     Router refuses a change of a label whose previous migration pass has not finished.

8. **Vertex labels move into lara-graph.** `VertexLabelStore` keeps its API and its place as
   the only writer, but stores through lara-graph: `set_labels` calls `set_node_labels`,
   `vertex_has_label` calls `has_node_label` (batched through `ReadBatch::has_node_label_each`
   where a caller checks many vertices), and `vertices_with_label` pages through
   `visit_nodes_with_label`. ADR 0123's invariant, postings equal to the inverse of the sets,
   now holds inside one store and one call. Index fencing and `label_pending` events stay in
   the facade's `apply_vertex_label_transition`, around the call. `commit_clear_vertex_labels`
   is no longer needed: a deleted node has no labels.

9. **Memories.** lara-graph's seven memories take new MemoryIds 56–62 (62 is `Proxies`, which
   stays empty until sharding adds a proxy), and the facade's work list (Decision 6) takes 63,
   each with its own bucket policy. The proxy tables of Decision 10 take MemoryIds allocated
   with the sharding work. MemoryIds 0–30, 32, 53, 54 and 55 become reserved holes, following
   the existing retired-id policy. `GRAPH_STABLE_LAYOUT`, the layout
   test and `design/storage/stable-memory-inventory.md` are updated with the change.

10. **Remote edges go through proxy nodes in their own id range.** Nothing writes a remote edge
    yet (`RemoteEdgeNotSupported` stays until sharding), but the layout is fixed now so that
    sharding needs no format change. lara-graph has the range (d681fe2):
    - **Proxy ids.** `NodeId::proxy(ordinal)` has bit 31 set; `add_proxy` / `add_proxies` give
      dense ordinals from 0 that are never reused, counted by `proxy_count`, apart from local
      nodes, which stay below 2^31. A proxy has buckets and a state like any node, so an edge to
      a remote vertex keeps both halves: the local half at the local vertex and the other half
      at the proxy. A proxy has no labels (`set_node_labels` fails with `Error::Proxy`), no edge
      joins two proxies, `bulk_load` places none, and it is outside `node_count`, node scans and
      `visit_nodes_with_label`.
    - **Remote check without a read.** A traversal tells a remote neighbor from its id's bit
      (`NodeId::is_proxy`), and resolves it only when it leaves the shard.
    - **The local half is canonical.** lara-graph's rule prefers the non-proxy half when exactly
      one end is a proxy, whatever the direction, so `EdgeHandle` and `GlobalEdgeId` owners stay
      local; otherwise the rule is unchanged.
    - **Gleaph's tables.** lara-graph stores nothing about what a proxy stands for. The facade
      keeps three tables, in MemoryIds allocated with the sharding work:
      - *Proxy → remote vertex*: a vector of 8-byte remote identities indexed by proxy ordinal.
        Ordinals are dense and never reused, so this is an array read, not a lookup: resolving a
        remote neighbor costs one stable read instead of a B-tree descent (about 23 K
        instructions in ADR 0123's measurement), which matters once a cross-shard step batches
        many neighbors into one call. The facade appends to it in the same message as
        `add_proxy`, so ordinal and index agree (a trap rolls both back).
      - *Remote vertex → proxy*: a stable map from the 8-byte identity to the ordinal, so that
        repeated edges to one remote vertex reuse its proxy and a remote deletion finds it.
      - *Shard → canister*: shard id to `Principal`, a handful of rows kept in the heap.
      The identity is `(home shard, vertex id on it)` while vertices never move. If sharding
      later moves vertices between shards, it becomes a global vertex id that does not change on
      a move, with a separate location table; only that table changes on a move, not the
      proxies. The kernel's `RemoteVertexId` is the proxy's ordinal; `MAX_REMOTE_VERTEX_ID`
      keeps the 30-bit bound, as local ids do.
    - **Remote deletion stays bounded.** When a remote vertex is deleted, every local edge to it
      is in its proxy's buckets: the work list runs `detach` / `detach_step` on the proxy, then
      drops the map entry. A proxy left with no edges is detached the same way.
    - **Rejected: identities inside lara-graph.** Storing the 8-byte identity in the proxy's
      record (14 bytes instead of 6) would give the same read cost, but puts a foreign key into a
      store that otherwise keeps topology only; storing the `Principal` (up to 29 bytes) per
      proxy would repeat it for every remote vertex of a shard. Encoding the identity in the
      proxy's id cannot fit: 31 bits hold the 30-bit vertex id but no shard.
    - Which shard owns a cross-shard edge's properties, and how its two copies stay in step, is
      left to the sharding ADR.

## Consequences

- **Smaller surface.** The facade drops the forward/reverse split, the observers, the rank-based
  counterpart scan, the remote-edge encoding in stored rows, and 35 MemoryIds for 8 (lara-graph's
  seven and the work list).
- **Fewer moves.** Only `compact` moves positions, and only in buckets the work list chose
  (Decision 6), `Append` and `ReuseHoles` alike. Re-keying sidecars and postings happens at one
  place instead of three.
- **Gleaph owns scheduling.** The work list's priority and deduplication (today in
  `deferred.rs`) move into the facade, and `arm_if_needed` reads that list.
- **Vertex label checks get cheaper.** A membership check is a node cell read instead of a
  B-tree descent; ADR 0123's per-vertex check measured about 23 K instructions, and lara-graph's
  batched check measured 185 K instructions for 1024 vertices. Labeled scans keep their lazy,
  ascending-id order. The label benches of ADR 0123 are re-run on the cutover.
- **Width migration moves to a caller-driven step.** ic-stable-lara's per-bucket migration with
  a fill byte goes away; lara-graph's `migrate_values` (Decision 7) replaces it with a transform
  Gleaph supplies. Allowing schema changes in the Router is a separate decision. Versioned
  values cost lara-graph's inserts 5-7 % and batched value writes 10 % more instructions; value
  reads are unchanged.
- **lara-graph's format is settled for launch.** The changes this ADR needs from lara-graph have
  landed: the proxy range with its canonical rule (Decision 10, d681fe2) and value layout
  changes with `migrate_values` (Decision 7, cdad0e6).
- **Unlabeled expansion reads once.** The facade's `for_each_directed_out_edges`,
  `for_each_directed_in_edges` and `for_each_undirected_edges` call `visit_all_neighbors`, so a
  pattern edge without a label walks each node's label bucket list once.
- **Benchmarks.** The Graph canbench suite (`crates/graph/src/bench`) is re-run on the cutover
  and compared against the last ic-stable-lara run. Benchmarks that only exercise removed
  substrate (slab placement, span folding, free spans) are deleted.

## Alternatives considered

- **Keep ic-stable-lara.** Its maintenance queue and the store-internal work items are the
  bounded-cost problem lara-graph was written to remove; keeping both stores is not an option
  either.
- **One-sided remote edges** (a flagged target id resolved through a Gleaph `BTreeMap`, with
  only the local half stored). The flag gives the same read-free remote check, but nothing
  indexes the local edges that point at a remote vertex: deleting it needs a scan of every bucket
  or a reverse index, which would be the proxy's bucket again. It would also break lara-graph's
  invariant that every edge has both halves, and bring a remote case into its target index,
  DETACH and counterpart lookup.
- **Proxies as ordinary nodes** (no id range). It needs no lara-graph change, but proxies would
  mix into `node_count` and node scans, telling a remote neighbor apart would cost a label or map
  read per neighbor, and the undirected canonical rule would still pick proxies by id.
- **Persist `(Label, Direction)` instead of the tagged `u16`.** It would change `EDGE_PROPERTIES`,
  graph-index keys and wire formats for no gain: the tagged key is a lossless encoding of the
  canonical half's label and direction.
- **Keep vertex labels in `VertexLabelStore`'s B-trees.** It would leave ADR 0123's storage
  untouched, but keep the per-vertex B-tree descent and two more MemoryIds, while lara-graph
  already stores node labels and their postings together.

## Related issues found by the survey

Not decided here:

- `ELEMENT_ID` encodes `label_index()` and drops the directed bit, so a directed `:L` edge and an
  undirected `:L` edge at the same owner and slot get the same id.
- `gql-params`' `EdgePathElementId` is still 12 bytes, while the Graph canister emits 16-byte edge
  ids.
