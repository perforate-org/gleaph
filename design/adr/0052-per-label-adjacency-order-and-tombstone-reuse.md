# 0052. Per-label adjacency ordering and tombstone-reuse maintenance

Date: 2026-07-29
Status: planned
Last revised: 2026-08-01
Anchor timestamp: 2026-08-01 14:41:58 UTC +0000
Implementation status: Slice 1 (2026-08-01, Plan 0196) implements the Graph Type `ORDER BY
INSERTION` declaration and the Router-side per-label `EdgeOrderingPolicy` resolution end-to-end.
Slice 2 (2026-08-01, Plan 0197) implements the query syntax `ORDER BY INSERTION(e)` (replacing
`GLEAPH.SEQUENCE`) with Graph-executor fail-closed acceptance (see below). Slice 3 (2026-08-01,
Plan 0198) implements LARA `EdgePlacementPolicy` (Unordered default / Insertion), Unordered
scalar in-slab tombstone reuse with ordinal-aligned inline property bytes (see below), the Graph
boundary policy mapping, and the Router §10 policy-change rejection on labels with live edges.
Unordered batch placement and compaction reordering remain planned.

## Context

Labeled LARA currently treats the live order of an edge label bucket as insertion order. A
slab tombstone therefore remains in place until compaction can remove it without changing the
relative order of the surviving edges. New inserts prefer append or the overflow log, and batch
placement is constrained by the same order-preserving contract.

This is useful for feeds, event streams, and other adjacency labels whose application semantics
need insertion order. It is unnecessary for ordinary relationship labels, where preserving order
causes tombstones to accumulate, increases overflow-log pressure, and makes batch placement less
able to reuse existing capacity.

The current GQL surface exposes this behavior through `GLEAPH.SEQUENCE(e)`. That helper mixes a
storage capability with a query-time order request and does not provide a schema declaration that
allows Graph to select a cheaper unordered placement and maintenance policy.

This ADR is a deliberate pre-production breaking design. Existing development stable snapshots,
old query fixtures, and old GQL syntax do not require migration or compatibility decoding.

## Prerequisite status

Plan 0195 (implemented 2026-08-01) established the ordering foundation this ADR builds on: every
layer's **default** traversal is now the stable materialization order (**ascending**), with
descending as an explicit opt-in.

- LARA: the unsuffixed `out_edges_iter` / `out_edges` / `visit_out_edges` and the `OutEdgeOrder`
  enum default are ascending; `desc_out_edges_iter` / `OutEdgeOrder::Descending` are the explicit
  descending path; the redundant `asc_*` aliases were removed; the iterator types were renamed
  (`AscOutEdgesIter` → `OutEdgesIter`, `OutEdgesIter` → `DescOutEdgesIter`).
- GraphStore facade: the direction-ambiguous `find_first_forward_handle_descending` /
  `find_first_reverse_handle_descending` helpers became `find_first_forward_handle` /
  `find_first_reverse_handle` (their slot/neighbor predicates are direction-neutral).
- GQL planner: `EdgeSequenceOrder::default()`, the `insertion_order_after_expand` fallback,
  and `insertion_order_sort`'s no-direction case are ascending (SQL `ORDER BY` convention);
  `ORDER BY INSERTION(e) DESC` remains the explicit descending path.

The ADR body below remains **planned** except for the parts implemented by Slice 1 (Plan 0196,
2026-08-01), Slice 2 (Plan 0197, 2026-08-01), Slice 3 (Plan 0198, 2026-08-01), Slice 4
(Plan 0199, 2026-08-01), and Slice 5 (Plan 0200, 2026-08-02). Slice 1
delivered §1/§4's `EdgeOrderingPolicy` (Unordered default / Insertion) as a resolved wire field on
`ResolvedEdgeLabel`, and §2's Graph Type `ORDER BY INSERTION` declaration parses (gql, opaque key)
and resolves per label in the Router from the canonical Graph Type definition, fail-closed on
unknown keys and conflicting declarations. Slice 2 delivered §3's query syntax: `GLEAPH.SEQUENCE(e)`
is removed and `ORDER BY INSERTION(e)` executes only for a single fixed edge label whose resolved
policy is `Insertion` (Unordered labels, ambiguous label expressions, and cross-boundary bindings
fail closed in the Graph executor, which consumes the resolved wire policy). Slice 3 delivered
§5's Unordered scalar placement (in-slab tombstone reuse before tail/log append) and §6's
Insertion scalar placement (append only) through a storage-owned `EdgePlacementPolicy`
(Unordered default / Insertion) threaded through every scalar insert entry point; §9's synchronized
slab-backed inline property bytes (the reused slot's live ordinal, log-backed bytes fall back to
the ordered path); the Graph mutation-boundary policy mapping; and §10's Router DDL-time rejection
of a per-label policy change on labels with live edges (see below).

**Slice 4 implementation note (2026-08-01, Plan 0199):** the batch planner now counts in-slab
tombstone holes as reusable capacity before reserving tail or log space (ADR 0052 §5). An
`Unordered` run on a chain-free, slab-bytes-backed bucket scans the stored window once, fills the
first `R` holes (per-hole `write_slot` writes or a whole-window rewrite, chosen by a
measured-constant cost comparison from the `lara_slab_*` micro-benches), and appends the tail
remainder contiguously when the vertex span has room; `Insertion` runs keep the ordered
substrate. Hole-fill is restricted to chain-free buckets whose inline property bytes are
slab-backed; log-backed bytes keep the ordered fallback (ADR 0052 §9), buckets with an existing
edge overflow chain keep the log path, and a run that cannot be admitted by holes plus tail
keeps the current tail-first/log path (no mixed slab+log split). Inline-property bytes are
inserted at the reused live ordinal with the trailing shift (ADR 0052 §9).

**Slice 5 implementation note (2026-08-02, Plan 0200):** maintenance is now policy-aware (ADR
0052 §7/§8/§9). `compact_vertex_edge_span_one_step` and `maintenance_with_observers` thread a
per-label placement resolver: `Unordered` buckets on chain-free, slab-bytes-backed (or width-0)
buckets swap-compact (the last live row moves into the first interior tombstone, reordering live
rows), while `Insertion` buckets keep the order-preserving left-pack and any bucket with an
inline-property-bytes log falls back to the full left-pack step. The swap moves the edge's
inline-property block from live ordinal `degree - 1` to the hole ordinal `k` with the `[k..degree-1)`
trailing shift in ordinal space, keeping the dense value span degree-sized; all writes are
preflighted in-bounds so the step is atomic-in-practice. `EdgeSlotMove` observers publish the
exact swap moves so sidecars, reverse rows, and the inline-scalar index rekey follow (§8). The
Graph facade resolves policies from the Router-projected table with an order-preserving fallback
when no table is active (a `Unordered` permission is not a reorder requirement), so timer-driven
maintenance stays order-preserving until a persisted ordering table lands; swap-compaction runs
in execution-context drains, tests, and canbench. Policy-change migration/rebuild (§10) remains
planned.

## Problem

Gleaph needs all of the following without introducing a second source of edge identity or a
per-edge insertion-sequence table:

1. unordered edge labels by default;
2. optional bucket-local insertion-order preservation for selected labels;
3. tombstone reuse for unordered slab buckets;
4. tombstone-first placement for unordered batch writes;
5. compaction that can reorder unordered live rows and move tombstones to the tail;
6. synchronized edge and inline-property storage;
7. explicit query syntax for requesting insertion order; and
8. a future syntax shape that can coexist with property-based edge ordering.

## Existing architecture assessment

The existing boundaries are sufficient:

- Router `GraphCatalog` owns the logical Graph Type definition and schema binding.
- Router resolves graph-scoped edge labels and sends resolved edge-label metadata to Graph.
- Graph owns logical mutation order, canonical edge/property sidecars, counterpart association,
  derived-index intents, and shard-local atomicity.
- LARA owns labeled buckets, slab/log placement, tombstone state, physical compaction, relocation,
  and exact physical-location results.
- The inline-property-bytes store owns its physical bytes but associates them with the edge through
  the bucket-local live ordinal.

The change extends these owners. It does not add a storage subsystem, an edge identity table, or a
Graph-local schema SSOT.

## Decision

### 1. One per-label ordering policy

Each catalog edge label has one resolved policy:

```rust
pub enum EdgeOrderingPolicy {
    Unordered,
    Insertion,
}
```

The default is `Unordered`. The policy is derived from the edge declaration's Graph Type option;
it is not stored in `LabelBucket` and is not independently configurable per physical bucket.

If one edge type declares several runtime labels, the declaration applies the same policy to every
label in that declaration. Conflicting policies require separate edge declarations.

### 2. Graph Type syntax

The schema declaration uses the same `ORDER BY` vocabulary reserved for future storage-order
capabilities:

```gql
CREATE GRAPH TYPE Social {
  DIRECTED EDGE FeedMembership
    LABEL IN_PUBLIC_FEED
    ORDER BY INSERTION
    CONNECTING (User) -> (Post),

  DIRECTED EDGE Follows
    LABEL FOLLOWS
    CONNECTING (User) -> (User)
}
```

An equivalent pattern-form edge declaration may place `ORDER BY INSERTION` inside the edge type
definition. The parser normalizes both forms to the same AST field.

`ORDER BY INSERTION` in Graph Type is a storage capability declaration. It does not select a
physical slot order directly and does not imply that every query returns edges in that order.

### 3. Query syntax

`GLEAPH.SEQUENCE(e)` is removed. A query explicitly requests the capability with:

```gql
MATCH (u)-[e:IN_PUBLIC_FEED]->(p)
RETURN p
ORDER BY INSERTION(e) DESC
```

`INSERTION(e)` is an order-key expression, not a numeric property and not a persisted sequence
value. `e` is required so a query with multiple edge bindings is unambiguous. `ASC` and `DESC`
are query-time directions; Graph Type declares only the canonical insertion-order capability.

The planner accepts `ORDER BY INSERTION(e)` only when the bound edge label is a single fixed label
whose resolved policy is `Insertion`. Unordered labels, ambiguous label expressions, and queries
whose edge binding crosses an ordering boundary fail closed.

Future property ordering remains ordinary GQL:

```gql
ORDER BY e.created_at DESC
```

If a future schema-level property-order capability is needed, it may use the reserved shape
`ORDER BY PROPERTY(created_at)` without changing the query form.

### 4. Canonical ownership and resolved wire projection

The Graph Type definition stored by Router `GraphCatalog` is the canonical source of the policy.
The edge-label catalog continues to own only graph-scoped name ↔ `EdgeLabelId` identity.

Router resolves the policy into every Graph-facing label table that can perform edge reads or
writes:

```rust
pub struct ResolvedEdgeLabel {
    pub name: String,
    pub id: EdgeLabelId,
    pub ordering: EdgeOrderingPolicy,
    pub inline_schema: Option<ResolvedInlineSchema>,
}
```

Graph validates and consumes the resolved policy at its mutation/query boundary. LARA receives a
storage-owned policy argument; it does not parse GQL, read Router catalogs, or own a duplicate
schema map. Standalone tests may inject a resolved policy fixture.

### 5. Unordered insertion and batch placement

For an `Unordered` bucket, scalar and batch insertion use this preference:

1. reusable in-slab tombstone;
2. available slab tail capacity;
3. the bucket/leaf overflow log; and
4. expansion or relocation when the first three cannot admit the write.

The batch planner counts tombstone holes as reusable capacity before reserving tail or log space.
It may assign pending edges to holes in any physical order. Input logical ordinals remain Graph
request-local metadata for replay, sidecar association, directed/reverse projection, undirected
pairing, and exact returned locations; they are not a physical ordering contract.

**Slice 3 implementation note (2026-08-01, Plan 0198):** the scalar reuse gate is O(1). The scan
runs only when `stored_slots > degree` (a slab tombstone outnumbers the live overflow-log edges).
For slab-only buckets this is exact. For log-backed buckets it is a sufficient condition, so a
bucket whose live log edges at least cover its slab tombstones (`log_live >= tombs`) keeps the
O(1) fast path and defers those tombstones to fold/compaction (the pre-slice behavior). This
avoids an O(log-chain) walk on every insert; an exact log-backed reuse gate would need per-block
tombstone accounting and is deferred with the unordered-compaction slice.

### 6. Insertion-ordered placement

For an `Insertion` bucket:

- existing live rows retain their relative order;
- interior tombstones are not reused when reuse would place a new row before a surviving row;
- new rows are appended to the bucket-local live suffix, using slab or overflow-log placement as
  capacity requires; and
- batch rows are projected into each affected bucket in stable input-ordinal order.

The guarantee is per orientation, owner vertex, and edge label bucket. There is no global order
across different labels.

### 7. Policy-specific compaction

`Insertion` compaction preserves bucket-local live order, as in the current ordered path.

`Unordered` compaction is allowed to reorder live rows. Its primary slab operation is
swap-compaction:

1. find an interior tombstone;
2. move a later live row into that hole;
3. move its inline property bytes with the same logical edge, updating the source and destination
   bucket-local live ordinals together;
4. publish exact edge/inline-property physical moves to Graph observers; and
5. leave the source row tombstoned and trim trailing tombstones when the span can shrink.

Overflow-log folding for an unordered bucket may materialize live rows in the chosen physical
order before applying the same hole-filling operation. No insertion-order claim is made for the
result.

The policy is the single source of truth for insert, batch placement, and compaction behavior.
Separate persisted `TombstoneReusePolicy` or `CompactionPolicy` fields are not introduced.

### 8. Edge identity and compaction

`GlobalEdgeId` / `EncodedEdgeId` remain query-time physical handles containing the owner and
physical edge slot. They are already invalidatable by compaction and are not stable logical edge
identities. This ADR therefore does not add per-slot generations or a generation sidecar.

Any physical move or slot reuse may invalidate an earlier query result, path element, or
`ELEMENT_ID` value. Graph sidecars, reverse adjacency, counterpart resolution, and derived index
maintenance must consume exact move/removal results during the canonical mutation or maintenance
boundary; they must not infer moves from query input order.

### 9. Inline properties and canonical deletion

The edge and inline-property-bytes stores remain physically independent. Their canonical
association is the bucket-local live ordinal:

```text
(owner, label, live_ordinal) -> edge row
(owner, label, live_ordinal) -> inline property bytes
```

An unordered insert or swap-compaction must update both domains in one Graph-owned mutation or
maintenance operation. Edge property sidecars and property-index delete/add intents are updated
through the existing Graph canonical and durable-outbox boundaries.

An inline-property-bearing unordered reuse path must not assume that edge slab slot and inline
property slab slot are numerically identical. It must use the existing exact ordinal/location
join. If a proven synchronized implementation is not available, the operation is rejected or
kept on the ordered/fallback path before canonical writes.

### 10. Policy changes

Changing a label from `Unordered` to `Insertion` or vice versa while it has live edges is rejected
in the initial implementation. Existing unordered physical order cannot prove insertion order.
A later explicit maintenance migration may rebuild the bucket under a new policy, but that is not
part of this ADR.

**Slice 3 implementation note (2026-08-01, Plan 0198):** the Router enforces this at DDL commit,
before any catalog mutation, for both inline `CREATE OR REPLACE GRAPH` bindings and named
`CREATE OR REPLACE GRAPH TYPE` replacements on all TypeRef-bound graphs. The live-edge test uses
the aggregated `ROUTER_EDGE_LABEL_STATS` projection (`live_count > 0`), which is Telemetry-class
event-sourced state. The projection is eventually consistent, so a projection-lag window fails
**open** (a policy change applied while the label stats have not yet caught up is accepted); this
is an accepted limitation of the initial implementation and is not a correctness hole for the
canonical edge store — it only means the DDL guard may be momentarily permissive.

### 11. Cross-canister and future inter-shard consistency

Graph remains the canonical owner of edge deletion and local sidecars. Property/index updates are
derived durable intents and may converge asynchronously under ADRs 0023, 0024, and 0029.

Future inter-shard edges require durable pending/ack/reconcile states for remote deletion and index
application. A remote success followed by a failed callback is not a terminal delete failure and
must not be retried as an unrelated fresh operation.

## Alternatives considered

### A. Preserve insertion order for every label

Rejected. It prevents tombstone reuse and forces ordinary labels to pay for a product guarantee
they do not need.

### B. Public `ordered: bool` or separate ordered/unordered mutation endpoints

Rejected. Ordering is a schema property of the label, not a caller-selected physical placement
hint. A request flag would allow the same label to have contradictory semantics.

### C. Per-edge sequence or generation table

Rejected for this pre-production design. The current edge handle is intentionally physical and
invalidatable by compaction. Adding sequence/generation state would increase stable footprint and
create a second identity model.

### D. Store the policy in every `LabelBucket`

Rejected. The policy is logical Graph Type schema, while `LabelBucket` owns physical placement
metadata. Duplicating it would require synchronization and could allow bucket/schema drift.

## Consequences

Positive:

- ordinary labels reuse tombstones during scalar, batch, and maintenance paths;
- feed/event labels retain the current insertion-order behavior explicitly;
- query syntax describes the requested order directly and removes `GLEAPH.SEQUENCE`;
- no per-edge sequence or generation storage is introduced;
- future property-order syntax has a reserved conceptual home.

Costs and risks:

- Graph Type AST, Router resolved label tables, GQL parser/planner/executor, LARA placement, and
  maintenance all gain a policy branch;
- unordered compaction invalidates physical handles, as current compaction already does;
- inline-property and sidecar move observers must be correct for swap-compaction;
- ordered and unordered paths require separate adversarial tests and canbench coverage;
- old stable snapshots and old GQL queries are intentionally unsupported.

## Migration and activation

No backward-compatible migration is required. Before implementation activation, reset development
stable data and install one compatible Router, Graph, Graph Index, SDK, and query fixture release
set. `GLEAPH.SEQUENCE` and old Graph Type order declarations are not decoded or retained.

## Required validation

Tests and benchmarks must cover:

- default unordered label tombstone reuse;
- insertion label tombstone non-reuse;
- unordered batch hole-first placement;
- unordered swap-compaction and trailing tombstone trimming;
- ordered compaction preserving live order;
- edge/inline-property ordinal alignment after reuse and moves;
- sidecar, reverse, counterpart, and derived-index move/delete intents;
- `ORDER BY INSERTION(e)` ASC/DESC success for an ordered fixed label;
- fail-closed query planning for unordered or ambiguous labels;
- policy change rejection with live edges; and
- scalar versus batch versus maintenance-inclusive benchmarks for tombstone-heavy buckets.

## Related documents

- [ADR 0001](0001-labeled-segment-slide.md) — labeled PMA physical maintenance.
- [ADR 0016](0016-overflow-log-tombstones-and-src-fields.md) — edge and inline-property log semantics.
- [ADR 0020](0020-deferred-maintenance-timer-drain.md) — deferred maintenance ownership.
- [ADR 0023](0023-federated-index-consistency-upgrade-compaction.md) — derived index convergence.
- [ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md) — canonical and async boundaries.
- [ADR 0034](0034-gleaph-gql-extension-syntax.md) — GQL extension registry and syntax contract.
- [ADR 0045](0045-unordered-batch-graph-mutations-and-lara-placement.md) — physical batch substrate.
- [ADR 0048](0048-lara-counterpart-resolution.md) — live pair rank and counterpart ownership.
- [ADR 0049](0049-input-order-preserving-batch-graph-mutations.md) — ordered batch contract, retained for `Insertion` labels.
- [ADR 0050](0050-lara-traverse-read-api.md) — logical traversal and inline-property read surface.
