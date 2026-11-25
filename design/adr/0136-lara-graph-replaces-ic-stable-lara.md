# 0136. lara-graph replaces ic-stable-lara as the Graph shard's edge store

Date: 2026-11-25
Status: proposed
Last revised: 2026-11-25
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
  value (at most 4096 bytes) in both halves. A label's width cannot change once declared.
- **Counterparts by lookup.** `counterpart(EdgeRef)` finds the other half through the bucket's
  target index; `Halves`/`EdgeRef::is_canonical` fix the canonical half (directed: the `Outgoing`
  half; undirected: the half at the larger node id, the same rule as
  `canonical_undirected_owner`).
- **Six memories** (`MemoryRole::ALL`), opened by `init` for a fresh or existing layout.
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
   in a new MemoryId: compaction targets `(node, label, direction)` and detaching nodes, with
   DETACH first. The ADR 0020 timer, its budget and its catalog-guard / compact / flush tick
   stay; a tick runs bounded steps until the instruction budget's headroom is reached.
   - **Compaction:** after a removal in an `Append` bucket, the bucket is listed when
     `holes > degree` (the half-full rule of today's remove-side gate). `ReuseHoles` buckets are
     never compacted; inserts refill them.
   - **DETACH DELETE:** `detach` marks the node; ticks call `detach_step` (and `sweep` for a hub
     bucket shared by several detaching nodes) until `Progress::Done`, then clear the node's bit
     in `PENDING_VERTEX_PURGES`. The DETACH pre-compaction pass is dropped: a purge moves no
     other edge. A plain DELETE of a node with no edges completes in one `detach_step`.
   - **Bulk-ingest finalize** keeps its endpoints. `bulk_load` writes buckets without holes, so
     finalize lists nothing unless a bucket has holes.
   - The store-internal items (dense leaf, inline slab, bypass row, width materialize) have no
     counterpart: lara-graph does that work within the update that causes it.

7. **Widths are declared once per catalog label.** The facade calls `declare_width` the first
   time a label with a nonzero profile is used; directed and undirected edges of the label share
   it, as the Router's profile already does. The Router's schema rule already matches
   lara-graph's: a label without values may gain a width (edges stored before it read as zero
   bytes until written), and a declared width never changes (`InlineSchemaConflict` in the Router,
   `WidthConflict` in lara-graph). 4096 bytes, the largest STRUCT profile, is lara-graph's
   `Width::MAX`.
   - **Width changes later.** The ban is there because neither store can change a width cheaply
     today, not because the schema must stay fixed. lara-graph is to gain it before Gleaph
     launches, while its format can still change without migration: each bucket records the
     width its values were written with, and a bounded, caller-driven step
     `migrate_values(node, (label, direction), transform)` rewrites at most one page per call and
     returns `Progress`, like `compact`. Only Gleaph knows how old bytes map onto the new schema,
     so it passes the transform; reads report each bucket's width while old and new widths
     coexist, and the work list (Decision 6) finds the buckets to migrate by stepping through
     the nodes. Until that lands, the Router's ban stays.

8. **Vertex labels move into lara-graph.** `VertexLabelStore` keeps its API and its place as
   the only writer, but stores through lara-graph: `set_labels` calls `set_node_labels`,
   `vertex_has_label` calls `has_node_label` (batched through `ReadBatch::has_node_label_each`
   where a caller checks many vertices), and `vertices_with_label` pages through
   `visit_nodes_with_label`. ADR 0123's invariant, postings equal to the inverse of the sets,
   now holds inside one store and one call. Index fencing and `label_pending` events stay in
   the facade's `apply_vertex_label_transition`, around the call. `commit_clear_vertex_labels`
   is no longer needed: a deleted node has no labels.

9. **Memories.** lara-graph's six memories take new MemoryIds 56–61, and the facade's work list
   (Decision 6) takes 62, each with its own bucket policy. MemoryIds 0–30, 32, 53, 54 and 55
   become reserved holes, following the existing retired-id policy. `GRAPH_STABLE_LAYOUT`, the layout
   test and `design/storage/stable-memory-inventory.md` are updated with the change.

10. **Remote edges go through proxy nodes in their own id range.** Nothing writes a remote edge
    yet (`RemoteEdgeNotSupported` stays until sharding), but the layout is fixed now so that
    sharding needs no format change. lara-graph is to gain a proxy range before then:
    - **Proxy ids.** A `NodeId` with bit 31 set is a proxy, counted apart from local nodes. A
      proxy has buckets like any node, so an edge to a remote vertex keeps both halves: the
      local half at the local vertex and the other half at the proxy. A proxy has no labels and
      is outside `node_count`, node scans and `visit_nodes_with_label`.
    - **Remote check without a read.** A traversal tells a remote neighbor from its id's bit,
      and resolves it only when it leaves the shard.
    - **Gleaph's map.** The kernel's `RemoteVertexId`, today a shard-local handle meant for a
      flagged edge row, becomes the proxy's `NodeId` without bit 31. The facade maps the global
      vertex (shard, vertex) ↔ `RemoteVertexId` in a MemoryId allocated with the sharding work,
      and repeated edges to one remote vertex reuse its proxy. `MAX_REMOTE_VERTEX_ID` keeps the
      30-bit bound, as local ids do.
    - **Remote deletion stays bounded.** When a remote vertex is deleted, every local edge to it
      is in its proxy's buckets: the work list runs `detach` / `detach_step` on the proxy, then
      drops the map entry. A proxy left with no edges is detached the same way.
    - **The local half is canonical.** lara-graph's rule (directed: `Outgoing`; undirected: the
      larger id) would put the handle at the proxy whenever the remote end is the source or, for
      undirected edges, always, since bit 31 makes the proxy larger. A proxy's id means nothing
      outside its shard, so lara-graph's canonical rule is to prefer the non-proxy half, keeping
      `EdgeHandle` and `GlobalEdgeId` owners local.
    - Which shard owns a cross-shard edge's properties, and how its two copies stay in step, is
      left to the sharding ADR.

## Consequences

- **Smaller surface.** The facade drops the forward/reverse split, the observers, the rank-based
  counterpart scan, the remote-edge encoding in stored rows, and 35 MemoryIds for 7.
- **Fewer moves.** Only `compact` moves positions, and only in `Append` buckets. Re-keying
  sidecars and postings happens at one place instead of three.
- **Gleaph owns scheduling.** The work list's priority and deduplication (today in
  `deferred.rs`) move into the facade, and `arm_if_needed` reads that list.
- **Vertex label checks get cheaper.** A membership check is a node cell read instead of a
  B-tree descent; ADR 0123's per-vertex check measured about 23 K instructions, and lara-graph's
  batched check measured 185 K instructions for 1024 vertices. Labeled scans keep their lazy,
  ascending-id order. The label benches of ADR 0123 are re-run on the cutover.
- **Width migration moves to a caller-driven step.** ic-stable-lara's per-bucket migration with
  a fill byte goes away; lara-graph's `migrate_values` (Decision 7) replaces it with a transform
  Gleaph supplies. Allowing schema changes in the Router is a separate decision once it lands.
- **lara-graph work before launch.** Per-bucket widths with `migrate_values` (Decision 7) and the
  proxy range with its canonical rule (Decision 10) change lara-graph's format, so both land
  before Gleaph launches.
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
