# 0045. Unordered batch graph mutations and LARA placement planning

Date: 2026-07-23
Status: Partially Implemented
Last revised: 2026-07-23
Anchor timestamp: 2026-07-23 08:50:57 UTC +0000

## Context

The current Router and Graph batch APIs reduce ingress and inter-canister call
count, but Graph still executes each physical mutation operation sequentially.
Each `PlanOp::InsertEdge` reaches the scalar `GraphStore` edge path, and LARA
performs bucket lookup, capacity preparation, slab-or-log placement, metadata
updates, mirror insertion, and maintenance decisions one edge at a time.

That path is appropriate for ordered or unpredictable incremental mutations. It
does not exploit a client-declared batch whose insertion order is semantically
irrelevant and whose complete topology, inline payloads, and properties are
known before the first canonical write.

The physical representation makes this more than a loop-elimination change:

- A local directed edge is represented by one canonical forward half and one
  derived reverse half. The invariant remains `reverse == projection(forward)`
  for local directed edges (ADR 0026).
- An undirected edge has no reverse row. It is represented by forward halves at
  both endpoints, except that an undirected self-loop has one forward row. The
  existing canonical-owner rule still selects the sidecar owner.
- A labeled edge bucket is scoped by orientation, owner vertex, storage label,
  and inline-value schema. Many buckets share one PMA leaf physical block.
- Edge bytes and inline-value bytes occupy independent slab/log allocation
  domains even though their logical ordinals must remain aligned.
- Edge properties are canonical `GraphStore` sidecars and property-index
  postings are derived state maintained through the existing outbox/repair
  path. They are not LARA storage.

ICP also bounds the batch envelope. As verified against the ICP resource-limit
reference on 2026-07-19 UTC, ingress and cross-subnet inter-canister request
payloads are limited to 2 MiB; same-subnet inter-canister requests may be up to
10 MiB; replicated responses are limited to 2 MiB. The existing
`MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES` therefore uses the portable
2 MiB ceiling. See [ICP resource limits](https://docs.internetcomputer.org/references/resource-limits/).

## Problem

Provide a batch mutation path that materially reduces stable-memory reads,
writes, bucket lookups, overflow-log churn, relocations, metadata rewrites, and
derived-state event overhead while preserving:

1. Directed forward/reverse and undirected two-forward-half consistency.
2. Exact association between logical edges, physical halves, inline values,
   canonical sidecars, and mate locations, including parallel edges.
3. Shard-local failure atomicity and idempotent retry.
4. Existing visible ordering and slot identity for pre-existing edges.
5. The maintenance-only ownership of slot-renumbering compaction.
6. Portable 2 MiB request and response behavior.

The design must also decide how a known pending batch participates in LARA PMA
placement: direct slab placement, overflow-log insertion, weighted rebalance,
or dynamic leaf expansion/relocation. A fixed one-step or fixed-factor leaf
growth is insufficient when the pending batch itself exceeds the resulting
capacity.

## Existing architecture assessment

The existing ownership boundaries remain suitable:

- Router owns ingress, authorization, label/property resolution, shard routing,
  message-size chunking, uniqueness coordination, and mutation lifecycle.
- Graph owns logical vertex/edge mutation, canonical sidecars, label deltas, and
  derived-index outbox records. Bidirectional LARA owns physical pair ordering,
  returned slot locations, and the adaptive mate index defined by ADR 0048.
- LARA owns vertex rows, adjacency halves, labeled buckets, edge/payload
  slab/log placement, PMA density, rebalance, relocation, and stable allocation.
- graph-index remains a derived property-index owner and is not pulled into the
  canonical batch commit.

ADR 0041 and ADR 0042 already define size-bounded Router-to-Graph dispatch and
continuation. ADR 0044 defines bulk-group mutation identity and durable progress.
ADR 0001 and the active LARA storage contracts already define PMA leaf blocks,
overflow logs, weighted slide, relocation, free-span retirement, and the
separation between rebalance and maintenance compaction.

The gap is an API that exposes the complete pending set to the owning layers.
Calling existing scalar methods in a loop cannot reserve one final placement,
cannot write one contiguous run, and may write a large known batch to the
overflow log only to fold it back into the slab during repeated rebalances.

## Decision

Implementation status as of 2026-07-23 07:12:03 UTC +0000:

- Plan 0121 read-only placement planning is implemented.
- Plan 0122 one-orientation batch commit is implemented in `ic-stable-lara`:
  the internal `OneOrientationBatchPlan` / `reserve_one_orientation_batch` /
  `BatchReservation::commit` / `BatchReservation::rollback` boundary exists.
  Empty plans are rejected by `reserve_one_orientation_batch`; the batch boundary
  does not define a no-op success path.  Reserve performs all fallible validation,
  edge/payload capacity reservation, and payload allocation before any canonical
  write; on failure it restores the edge-store logical capacity and the payload
  occupied tail to their pre-reserve values.  Any payload bytes already appended
  are retired to the payload free-list as reusable slack; the underlying
  stable-memory pages are not shrunk.  `BatchReservation::rollback` consumes the
  token and applies the same restoration, so a reservation cannot be rolled back
  twice.  Commit validates the reservation token, the originating graph instance,
  and every bucket fingerprint/geometry before the first canonical byte write.
  After the first canonical byte write, a panic is an invariant violation.  In an
  ICP canister message the trap rolls back the entire message, so no partial
  canonical state is published at that boundary; direct library callers without
  such a transaction boundary do not receive the same atomicity guarantee.
  Supported geometries are existing buckets whose run fits the current planned
  slab window (including the first bucket's vertex quota), plus pinned-leaf
  expansion when the edge/payload logs are full. Fixed-width payload spans may
  be reused or grown at the occupied tail; non-tail relocation remains
  unsupported.
- Plan 0123 GraphStore clean-slab orchestration is implemented: `GraphStore::
  try_insert_batch_edges_clean_slab` builds one-orientation plans from the
  existing read-only planner, reserves every orientation before committing any
  orientation, and returns `BatchEdgeInsertResult::Unsupported` before any
  canonical write when the clean-slab path cannot admit the geometry.  If a later
  orientation reservation fails, every previously successful reservation is rolled
  back by consuming its token via `BatchReservation::rollback` /
  `DeferredBidirectionalLabeledLaraGraph::rollback_batch_reservation`, restoring
  the edge-store logical capacity and payload occupied tail and retiring any
  allocated payload bytes to the free-list as reusable slack.  The underlying
  stable-memory pages are not shrunk, so the caller can safely fall back to the
  existing scalar insertion path without leaking logical capacity or payload
  tail.  Focused unit tests cover directed/reverse pairing, undirected
  two-forward-half and self-loop behavior, payload read-back, multi-run commits,
  reserve failure leaving canonical state and allocator headers/logical capacity
  unchanged plus the expected free-list slack shape, and empty/new-bucket
  unsupported rejection.  Canbench coverage compares the clean-slab path against
  scalar insertion for 128 directed edges with widths 0 and 8, with identical
  pre-created buckets and input construction outside the measured closure.
- Plan 0124 per-leaf overflow-log batch append is implemented in
  `ic-stable-lara` and `gleaph-graph`.  `reserve_one_orientation_batch` now admits
  existing-bucket runs that do not fit the current clean slab window but fit the
  per-leaf edge/payload overflow logs.  The reserve/commit boundary still rejects
  empty plans, validates all geometry and capacity before any canonical write,
  and returns unsupported before any write when admission fails.  Overflow-log
  runs do not mutate edge-store logical capacity or payload occupied tail before
  commit; they reserve only ephemeral log capacity.  Commit appends edge and
  payload entries in logical ordinal order, updates bucket overflow heads and
  degree, and leaves stored_slots and vertex slab span unchanged.  Cross-
  orientation reserve-all-then-commit and rollback remain unchanged.  Scalar
  fallback remains for new buckets, default/unlabeled promotion,
  rebalance/relocation, dynamic leaf expansion, tombstone reuse, and other
  unsupported geometry.  Focused unit tests cover successful edge/payload log
  append, log capacity exhaustion, multi-run and multi-orientation rollback,
  read-back order, and unchanged canonical/allocator state after rejection.
- Plan 0125 pending-aware one-shot leaf expansion is implemented for existing
  edge-only buckets. Plan 0128 extends it to fixed, uniform non-zero payload
  widths: reserve projects and allocates the coupled payload span, commit folds
  edge/payload logs and writes aligned slab values, and rollback restores the
  payload tail/free-list and edge allocator state. Payload read-back and full
  canbench coverage are included; malformed edge/payload log lengths reject
  before allocation. New bucket creation, non-tail relocation, and full public
  wire integration remain planned for later slices.
- ADR 0048's persistent mate index is still planned. Plan 0129 implements only
  the internal returned-slot boundary: LARA owns exact physical locations and
  GraphStore joins them by ordinal without a post-insert adjacency scan. Plan
  0130 makes location capture an explicit internal mode; the normal aggregate-
  only path does not materialize locations. The current scalar facade still
  uses `EDGE_ALIASES` until the later mate-index migration lands.

### 1. Add an explicit unordered batch mutation mode

The new path is opt-in. It does not change ordinary GQL insertion ordering.
Clients submit each logical edge once in a size-bounded logical chunk. They do
not send forward/reverse rows, undirected mate records, LARA bucket keys, log
policy, or placement instructions.

The semantic contract is:

- Relative order among new edges in one unordered chunk is unspecified.
- Existing edges are not semantically reordered.
- Chunk order does not become a scan-order contract.
- Duplicate mutations targeting the same existing inline value or the same
  `(element, property)` in one unordered chunk are rejected during preflight;
  unordered batches do not silently introduce last-write-wins semantics.

The initial wire shape is an additive, versioned Router endpoint and an internal
Router-to-Graph endpoint. It groups repeated label, directedness, inline schema,
and property schema metadata instead of repeating a full physical plan per edge.
Fixed-width endpoints and inline payloads may use packed columnar blobs behind a
typed versioned envelope. The exact codec is benchmark-selected and must retain
strict byte-length and count validation.

### 2. Use 2 MiB portable atomic chunks

Every public ingress chunk and portable Router-to-Graph request is bounded by
its actual encoded byte length, not only by item count. An independent maximum
logical-item or physical-half count also protects the instruction budget.

One Graph chunk is shard-local atomic. A logical ingest session may contain
many chunks and is roll-forward across chunks; it is not globally atomic.
`(mutation_id, chunk_index)` plus the request fingerprint identifies a chunk.
A retry of the same identity and fingerprint returns the durable receipt rather
than inserting again; reuse with different bytes is rejected.

The normal response is a compact receipt containing counts, status, and
continuation information. Returning one edge identifier per inserted edge is
not the default because the replicated response is also limited to 2 MiB.
Callers that require identifiers use a separately bounded/paginated result
surface or client-local ordinals resolved inside the same mutation.

The 10 MiB same-subnet request allowance is not a v1 assumption. A later
colocation-aware optimization may coalesce already durable portable chunks, but
must not make correctness or retry depend on Router and Graph remaining on one
subnet.

### 3. Expand logical edges into physical half-edge intents inside Graph

Graph assigns each logical item a chunk-local ordinal and derives physical
intents:

| Logical edge | Physical intents |
| --- | --- |
| directed `u -> v` | canonical forward `(u, v)` plus reverse `(v, u)` |
| undirected `u -- v`, `u != v` | forward `(u, v)` plus forward `(v, u)` |
| undirected self-loop `u -- u` | one canonical forward `(u, u)` |

Each intent carries the logical ordinal and canonical/mate role. Returned LARA
locations are joined by ordinal, never by a post-insert "first matching
neighbor/payload" search. This gives parallel edges an exact forward/reverse or
canonical/mate association. The two projections may be unordered only after one
common order per pair key is chosen; independent projection ordering violates
the pair-rank invariant in ADR 0048.

### 4. Plan placement by allocation domain, then write by bucket run

Write runs are grouped by:

```text
(orientation, owner_vertex_id, storage_label_id, inline_value_width)
```

Capacity is not planned independently for each bucket. LARA aggregates pending
counts through the physical ownership hierarchy:

```text
orientation -> PMA leaf/window -> vertex -> label bucket
```

This prevents two buckets in the same leaf from reserving the same slack. The
forward and reverse stores receive independent plans because fan-out, fan-in,
and undirected workloads produce different leaf pressure.

The LARA mutation shape is explicitly split:

```rust
let plan = graph.plan_batch_mutation(input)?;       // read-only validation/placement
graph.reserve_batch_mutation(&plan)?;               // backing capacity only
let result = graph.commit_batch_mutation(plan, input); // no recoverable failure
```

Planning may allocate heap metadata but publishes no canonical state. Reserve
may grow backing memory or reserve spans/log cells; retained physical capacity
after a rejected reserve is non-canonical and safe. After the first canonical
commit write, all remaining internal failures are invariant violations and trap
the same canister message rather than returning a partial-success `Err`.

### 5. Provide true slab batch writes

When one bucket run fits cleanly in its assigned slab window, LARA performs one
bucket lookup/schema validation, one capacity decision, contiguous edge writes,
contiguous fixed-width payload writes where applicable, and aggregated bucket,
PMA-count, and maintenance updates.

The implementation must not call scalar `insert_edge` once per item under a
batch facade. New buckets created for a substantial known batch receive a
planned slab reservation directly; they do not have to start at zero length and
route the full batch through the overflow log.

### 6. Provide per-leaf overflow-log batch writes

The edge overflow log and inline-value overflow log each receive a low-level
batch append API. The physical log is shared by buckets in a leaf, so planning
aggregates all bucket groups for that leaf before reserving entries.

An overflow-log batch append:

1. Preflights the complete edge/payload log capacity.
2. Reserves every required entry before linking a chain.
3. Writes bucket-local contiguous chains where the representation allows it.
4. Connects each existing chain once.
5. Updates each bucket head/length once and updates segment metadata in aggregate.
6. Returns per-ordinal log locations.

If the complete planned log batch does not fit, it writes nothing and the
placement planner chooses pending-aware rebalance/expansion. It never inserts a
prefix and then discovers `SegmentLogFull`.

Overflow-log batching is a small-spill policy, not the default for large known
batches. The edge and payload logs currently have bounded per-leaf capacity;
using them as a staging area for a large batch would create double writes and
immediate fold debt.

### 7. Rebalance and expand using projected post-batch geometry

For every affected bucket/vertex/leaf, planning includes existing resident slab
slots, existing overflow-log slots that must be preserved, and pending physical
halves:

```text
projected_resident(bucket) =
    current_slab_slots
  + existing_edge_overflow_log_slots
  + pending_edge_slots
```

The leaf projection also includes newly active vertex/bucket anchors and the
existing PMA geometric slack policy. Inline payload capacity is computed in its
independent domain using current payload slab slots, payload-log entries, pending
item count, and fixed width/blob representation.

The physical PMA `segment_size` (vertices per leaf) remains fixed. The leaf's
physical edge-span length is dynamic. A relocation target is chosen from the
larger of:

- the exact projected minimum required to hold resident and pending data;
- the length needed to restore the PMA target fill ratio;
- amortized growth from the old physical length; and
- the minimum allocation block.

It is then rounded up to the allocation quantum with checked arithmetic:

```text
target_len = round_up(
    max(projected_minimum, density_target_len, amortized_len, minimum_block),
    allocation_quantum,
)
```

`target_len >= projected_minimum` is an invariant. A fixed multiplier is only an
amortization floor; it is never proof that the batch fits. A batch requiring
several blocks expands once to the calculated target rather than repeatedly
doubling, relocating, or retrying insertion.

When rebalance or relocation folds an edge overflow log, every preserved log
entry is included in the required capacity and moved into the new slab before
the bucket publishes `overflow_log_head = NONE`. Structural fold preserves the
existing slot-identity contract, including any legacy tombstone entries that
maintenance alone may discard. Inline-value log folding is planned separately
and retains ordinal alignment with edge rows.

The commit order is:

1. Reserve the final edge and payload destinations for every orientation.
2. Copy/reposition existing slab rows while preserving the structural identity
   contract.
3. Fold preserved edge and payload overflow-log entries into their planned slab
   destinations.
4. Write pending batch rows and payloads.
5. Publish bucket ranges, counts, heads, vertex bases, and leaf totals.
6. Publish LARA mate acceleration, Graph sidecars, and durable derived-state events.
7. Retire old physical spans and release folded log segments only after all live
   pointers have moved.

Slot-renumbering tombstone compaction remains maintenance-only. Batch placement
may reuse a tombstone only where the existing insertion contract permits reuse
without invalidating sidecars; it does not treat unordered input as permission
to compact or reorder pre-existing logical edges.

### 8. Keep inline-value and property ownership separate

Inline values supplied with new edges are part of the initial LARA placement,
not post-insert scalar updates. Existing-edge inline updates use a LARA batch
update API that:

1. Canonicalizes all handles and validates exact label-schema widths.
2. Resolves every directed mirror or undirected forward mate before writing.
3. Groups targets by orientation/leaf/bucket/slot.
4. Coalesces contiguous slot runs and updates all physical copies in one
   no-await commit.

Arbitrary edge and vertex properties remain GraphStore sidecars. GraphStore
provides batch `Set`, `Remove`, and `ReplaceAll` mutations. It canonicalizes edge
handles, validates all values and uniqueness/constrained-property prerequisites,
reads old values, writes canonical sidecars, and records the net old-to-final
property-index changes in the durable outbox. Intermediate duplicate updates are
not emitted to graph-index.

Canonical LARA rows, inline mirrors, GraphStore sidecars, mate metadata, label deltas,
and durable derived-state events belong to the same shard-local no-await commit.
Remote graph-index draining remains asynchronous under ADR 0023/0024. An index
outbox failure cannot be converted into a successful canonical batch without a
durable repair record.

Initial support for globally constrained or unique properties may reject the
unordered path until ADR 0030 represents and coordinates claims for every item
in a chunk. The API must fail closed rather than bypassing uniqueness.

### 9. Add vertex batch primitives without coupling vertices to edge layout

The bidirectional LARA wrapper gains a synchronized vertex-row batch append that
returns a contiguous `Range<VertexId>`. It:

1. Validates forward/reverse counts and every row.
2. Computes the final vertex count and segment-tree leaf count.
3. Reserves both vertex columns and all forward/reverse edge/payload segment
   metadata once.
4. Appends both orientations without per-row growth.

The low-level vertex and edge batch APIs remain separate. A higher GraphStore
`GraphMutationChunk` may compose them so edges refer to existing IDs or
chunk-local new-vertex ordinals. Combined planning must use the projected final
vertex count when deriving PMA geometry; it may not commit vertices and only
then discover that edge placement fails.

### 10. Placement policy remains encapsulated in LARA

Clients, Router, GraphStore, and portable GQL crates do not receive flags such
as `ForceOverflowLog`, `ForceRebalance`, or physical bucket sizing. They declare
only semantic unordered batching. LARA selects among:

| Condition | Physical action |
| --- | --- |
| complete run fits current clean slab window | direct slab batch |
| small spill and complete edge/payload log batch is cheaper and fits | overflow-log batch |
| larger spill or log pressure | pending-aware weighted rebalance, then slab batch |
| current PMA window cannot absorb projected geometry | one dynamic leaf expand/relocate, then slab batch |

The small-spill threshold and growth slack are performance policy, not public
wire contract. They are selected and revised using canbench while correctness is
guarded by the capacity inequalities above.

## Invariants

The implementation must make these conditions directly testable:

1. Every committed logical directed edge has exactly its required forward and
   reverse halves; every committed undirected edge has the required one or two
   forward halves.
2. Every half, payload, mate location, and sidecar is joined by logical ordinal or an
   exact planned handle, never by ambiguous first-match lookup.
3. No validation or reservation failure leaves a bucket, bypass promotion,
   vertex row, edge half, payload, property, mate locator, or derived event visible.
4. No recoverable allocation or encoding failure remains after the first
   canonical commit write.
5. Planned edge and payload capacity includes all preserved slab rows, all
   preserved overflow-log rows, and all pending batch rows.
6. Dynamic leaf targets fit projected geometry in one expansion and use checked
   arithmetic.
7. Overflow-log batch append is all-or-nothing per Graph chunk.
8. Existing edge order and slot-identity semantics survive structural
   rebalance/log fold; compaction remains maintenance-owned.
9. Canonical property writes and durable net derived-state events agree.
10. Requests and responses stay inside their measured encoded payload budgets.

## State representation and migration

The final adjacency, vertex, payload, and property states are representable in
the existing stable stores. Batch placement plans, pending overlays, physical
half intents, and ordinal maps are ephemeral heap values and add no new LARA
stable region or persisted physical layout.

The public and Router-to-Graph wire types require additive versioned Candid
types/endpoints. Durable idempotency reuses the existing Router/Graph mutation
lifecycle, but the journal value must gain a versioned chunk receipt/fingerprint
representation if the currently deployed ADR 0044 record cannot distinguish a
specialized chunk replay. This is a value-schema migration under ADR 0039, not a
new adjacency source of truth.

No implementation may persist a placement plan across an `await`. Router may
persist mutation/chunk lifecycle before calling Graph; Graph reconstructs the
ephemeral physical plan inside the target update message and commits without an
inter-canister await. A Router callback failure after Graph success is reconciled
by retrying the same chunk identity and reading the durable Graph receipt.

## Alternatives considered

### Keep scalar LARA insertion under the existing Graph batch endpoint

Minimal change, but retains repeated lookup, allocation, log, rebalance,
maintenance, mate-resolution, property, and event costs. It does not address the
demonstrated architectural limitation. Rejected as the final design; retained as
fallback for unsupported or too-small groups.

### Batch only forward directed edges and insert reverse rows sequentially

This can measure the upper bound of a partial optimization, but leaves reverse
cost dominant and splits one consistency invariant across two physical paths.
Rejected as the production contract.

### Let clients send forward/reverse or both undirected halves

This duplicates wire bytes, approaches the 2 MiB limit sooner, exposes LARA
layout, and makes clients responsible for Graph invariants. Rejected.

### Route every batch through overflow logs and finalize later

Simple append code, but the bounded shared leaf logs fill quickly, payload logs
add a second pressure domain, and large batches pay a write-then-fold penalty.
Rejected for large batches; selected only as an internal small-spill policy.

### Always rebalance/relocate before a batch

Avoids logs but makes small sparse batches rewrite a leaf unnecessarily.
Rejected in favor of the encapsulated slab/log/rebalance decision table.

### Stage multiple ingress chunks on Router and send 10 MiB to colocated Graph

Could reduce call count but adds durable upload state, subnet-colocation
assumptions, and larger instruction-risk while responses remain 2 MiB. Deferred
until portable 2 MiB chunks are measured as the remaining bottleneck.

### Introduce a separate bulk storage subsystem

Would duplicate Graph/LARA ownership, stable layout, recovery, and scan paths.
Rejected. Existing owners are extended instead.

## Consequences

Positive:

- Known unordered batches can turn repeated random stable-memory mutations into
  a bounded number of planned contiguous writes and metadata commits.
- Directed, reverse, undirected, inline-value, mate, property, and index-event
  consistency remain enforced by their existing owners.
- Large batches avoid unnecessary log staging and repeated fixed-step leaf
  growth.
- The 2 MiB portable chunk contract remains valid across subnet placement.
- Scalar insertion remains available for ordered, small, or unsupported cases.

Costs and trade-offs:

- Planning needs temporary heap proportional to one bounded chunk and sorting or
  grouping by physical ownership keys.
- Failure-atomic reserve/commit work must cover edge slab, edge log, payload
  slab/log/blob storage, free spans, vertex columns, mate metadata, sidecars, journals,
  and durable derived events.
- Combined new-vertex/new-edge chunks require projected geometry before vertex
  rows become visible.
- Policy thresholds can regress either immediate insertion or deferred
  maintenance cost and therefore require persistent benchmark coverage.
- The specialized unordered endpoint is an additional product surface; generic
  portable GQL crates must remain unaware of LARA/ICP batching details.

## Test contract

At minimum, tests must cover:

- Directed fan-out and fan-in where forward/reverse plans have different leaf
  pressure.
- Undirected endpoints in the same/different leaves and undirected self-loops.
- Parallel edges with exact forward/reverse and canonical/mate mapping.
- New and existing buckets; exact slab fit; one-slot spill; empty/partial/full
  overflow logs; multi-block projected expansion.
- A batch too large for one fixed-factor growth but accepted by one dynamic
  projected expansion.
- Existing edge and payload logs included in capacity and folded to slab before
  heads/log segments are released.
- Zero-width, narrow fixed-width, wide/blob, and independently pressured payload
  storage.
- Tombstones and pre-existing edge order/slot identity across structural folds.
- Batch inline updates across directed and undirected mirrors.
- Property `Set`, `Remove`, and `ReplaceAll`; canonical old-to-final index events;
  duplicate-update rejection; uniqueness fail-closed behavior.
- Vertex batches crossing segment-tree growth boundaries and failure without
  forward/reverse count divergence.
- Failpoints for every reserve boundary proving no visible partial state.
- Retry with identical and conflicting chunk fingerprints.
- Encoded request/response boundary cases around the configured byte ceilings.

## Benchmark contract

Canbench is required for LARA/Graph/Router-relevant paths. Benchmarks compare
scalar and batch behavior at 1, 8, 32, 128, 1,024, and larger bounded sizes where
the message/instruction budgets permit. Required workload shapes include:

- one new bucket, one existing bucket, many labels, and many vertices in one
  leaf;
- exact fit, small spill, log batch, log fold, weighted slide, in-place expand,
  and relocated multi-block expand;
- directed fan-out, directed fan-in, undirected, and self-loop;
- inline widths 0/W1/W8 and wide/blob payloads;
- vertex batches crossing PMA segment boundaries;
- property-free, inline-only, sidecar-property, and indexed-property mutations.

Report setup, canonical mutation, derived-event creation, remote drain, and
maintenance/finalize separately. Also report end-to-end mutation plus required
maintenance so the selected policy cannot appear cheaper merely by moving work
into overflow logs or the deferred queue. Primary measures are instructions per
logical item, stable-memory reads/writes or pages where observable, relocation
count, log occupancy/debt, maintenance work, encoded bytes, and callback count.

## Implementation sequence

1. **Implemented.** Read-only placement structures and canbench baselines are in
   `crates/graph/src/facade/batch_placement.rs`. The module expands directed,
   undirected, and self-loop logical edges into ordinal-tagged physical intents,
   groups them by `(orientation, PMA leaf segment, owner vertex, storage label,
   inline width)`, reads existing LARA bucket/slab/overflow-log occupancy through
   `ic-stable-lara::labeled::LabelBucketPlacementInfo`, and projects the minimum
   required edge/payload capacity using checked arithmetic. No canonical write,
   wire change, or public API is introduced. The baseline planner fails closed for
   a PMA leaf containing multiple non-zero payload widths; width-specific payload
   byte projection remains a later slice rather than being approximated by a
   single slot count.
2. Add one-orientation slab and edge/payload overflow-log batch primitives with
   plan/reserve/commit and failure-atomic tests.
3. Add pending-aware leaf/window planning, dynamic one-shot expansion, and
   existing-log fold. **Implemented for existing-bucket runs in Plans 0125 and
   0128.** Plan 0125 covers edge-only expansion; Plan 0128 extends the same
   failure-atomic boundary to fixed, uniform non-zero payload widths, including
   payload-log fold and payload-span growth at a reusable or occupied-tail
   span. Relocation and new-bucket creation remain deferred.
   Expansion-success evidence remains owned by LARA; GraphStore exposes only
   internal admission classification and reserve-all rollback behavior. It
   must not publish or fabricate PMA leaf, overflow-log cursor, or bucket-head
   metadata for tests.
4. **Partially implemented.** Plans 0123–0128 provide bidirectional directed
   and two-forward-half undirected orchestration. Plan 0129 now returns exact
   internal slab/overflow-log edge and payload locations from LARA and joins
   them by logical ordinal in GraphStore, including self-loop cardinality. Plan
   0130 makes aggregate-only output the normal path and retains location
   materialization as an explicit capture path.
   Persistent mate indexing and public result exposure remain planned.
5. Add GraphStore edge insertion with initial inline values, properties, label
   deltas, and durable derived events.
6. Add existing inline-value and vertex/edge property batch updates.
7. Add synchronized LARA/GraphStore vertex batches, then optional combined
   new-vertex/new-edge chunks using projected final geometry.
8. Add versioned Router/Graph wire, portable 2 MiB chunking, receipts,
   idempotency/recovery, SDK packing, and PocketIC coverage.
9. Select small-spill and slack policy from benchmark evidence; run unfiltered
   `canbench --persist` for every affected benchmark crate before final artifact
   updates.

Each stage preserves the scalar fallback and may ship only after its own
invariants and failure-atomic boundaries are covered.

## Design document impact

- `design/adr/0045-unordered-batch-graph-mutations-and-lara-placement.md`:
  status remains Partially Implemented; stages 1–3 are implemented for the
  explicitly bounded existing-bucket expansion path, with fixed-width payload
  support limited to reusable or occupied-tail spans.
- `design/storage/lara.md`: link the read-only planning contract and note that
  direct slab/log batch writes, rebalance, and relocation remain planned.
- `design/storage/lara-dgap-contract.md`: pending-aware leaf placement remains
  planned; maintenance-only compaction boundary is unchanged.
- `design/storage/labeled-edge-inline-values.md`: payload slab/log batch
  placement and mirrored update behavior remain planned.
- `design/storage/bulk-ingest-finalize.md`: planned direct batch placement is
  distinct from the existing post-ingest maintenance/finalize hook.
- ADR 0026: reverse remains derived and co-committed; no change to the canonical
  source of truth.
- ADR 0029/0041/0042/0044: specialized chunks reuse shard-local atomicity,
  continuation, and durable bulk identity; their current scalar Graph execution
  remains implemented until this ADR advances.
- ADR 0030: constrained/unique unordered batch support remains fail-closed until
  per-item claims are represented.

## Related

- [ADR 0001](0001-labeled-segment-slide.md): labeled PMA leaf physical layer.
- [ADR 0016](0016-overflow-log-tombstones-and-src-fields.md): overflow-log layout and tombstones.
- [ADR 0020](0020-deferred-maintenance-timer-drain.md): maintenance-only compaction and deferred drain.
- [ADR 0023](0023-federated-index-consistency-upgrade-compaction.md): derived property-index consistency.
- [ADR 0026](0026-reverse-adjacency-differential-repair.md): canonical forward and derived reverse adjacency.
- [ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md): shard-local atomicity.
- [ADR 0030](0030-cross-shard-uniqueness-tcc-reservation.md): uniqueness coordination.
- [ADR 0041](0041-router-graph-batch-mutation-dispatch.md): Router-to-Graph batch dispatch.
- [ADR 0042](0042-router-dynamic-instruction-budget-batching.md): dynamic continuation.
- [ADR 0044](0044-router-bulk-mutation-key.md): durable bulk mutation grouping.
- [ADR 0048](0048-adaptive-lara-mate-index.md): physical pair rank, returned slots,
  and adaptive LARA mate acceleration replacing facade aliases.
- [LARA storage contract](../storage/lara.md).
- [LARA/DGAP alignment](../storage/lara-dgap-contract.md).
- [Bulk ingest finalize](../storage/bulk-ingest-finalize.md).
