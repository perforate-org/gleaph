# 0048. LARA-owned counterpart resolution replaces Graph edge aliases

Date: 2026-07-23  
Status: accepted  
Implementation status: CounterpartScan production reads migrated to the ADR 0050 `traverse_next` logical-slot surface; first bounded Graph scan-only canonicalization caller group migrated; remaining sidecar lookup paths and alias map remain transitional
Adoption status: partially activated (scan-only Graph canonicalization)

Traversal dependency: ordinary-caller adoption may begin after ADR 0050 Phases 1–2 have produced
the tested and benchmarked `traverse_next` read surface. This is a one-way hand-off: ADR 0048 uses
that surface for migrated callers while the legacy `traverse` module remains available. ADR 0048
does not wait for the final ADR 0050 module rename, and it must not introduce a second traversal
primitive.

## Context

Gleaph identifies a canonical adjacency occurrence by its owner, label, and tombstone-inclusive bucket position:

```text
EdgeHandle = (owner_vertex_id, storage_label_id, BucketEntryPosition)
```

The persisted edge row stores only its target vertex. Its owner, label, orientation, and logical
slot are supplied by the containing Labeled LARA bucket and iterator; raw slab/log locations stay
inside LARA.

A canonical adjacency occurrence is represented at the LARA boundary as follows:

| Logical edge                  | Physical entries                      | Canonical entry            |
| ----------------------------- | ------------------------------------- | -------------------------- |
| directed `u -> v`             | forward `(u, v)` and reverse `(v, u)` | forward entry              |
| directed self-loop `u -> u`   | separate forward and reverse entries  | forward entry              |
| undirected `u -- v`, `u != v` | one forward entry at each endpoint    | entry owned by `max(u, v)` |
| undirected self-loop `u -- u` | one forward entry                     | that entry                 |

Graph currently stores an `EDGE_ALIASES` stable B-tree row for each non-self logical edge.

The alias index is structurally misplaced:

- physical slot allocation and movement are owned by LARA;
- alias rows duplicate a relation already implied by LARA adjacency order;
- lookup is efficient only from alias to canonical;
- canonical-to-counterpart lookup scans the alias map;
- scalar insertion may rescan adjacency after LARA already determined the written slots;
- slot-renumbering maintenance requires Graph-level alias repair; and
- a second reverse index would further increase persistent bytes and consistency work.

Counterpart resolution belongs to the bidirectional Labeled LARA boundary, which owns both physical projections, their order, slot allocation, compaction, and repair.

## Decision

### 1. LARA owns counterpart resolution

Graph removes `EDGE_ALIASES` after all callers use LARA counterpart APIs.

The bidirectional Labeled LARA owner exposes these final internal types:

```rust
#[repr(transparent)]
pub struct BucketEntryPosition(u32);

pub struct EdgeHandle {
    owner_vertex_id: VertexId,
    label_id: BucketLabelKey,
    slot: BucketEntryPosition,
}

struct CanonicalEdgeOccurrence {
    orientation: LabeledOrientation,
    handle: EdgeHandle,
}

fn counterpart_of(
    edge: CanonicalEdgeOccurrence,
) -> Result<CanonicalEdgeOccurrence, CounterpartLookupError>;

fn canonical_handle(
    edge: CanonicalEdgeOccurrence,
) -> Result<EdgeHandle, CounterpartLookupError>;
```

The implementation uses `counterpart` consistently. Existing `mate` names in APIs, types, modules, tests, benchmarks, and documentation are renamed or removed.

GraphStore retains canonical edge properties and derived-index events. It does not retain a canonical counterpart occurrence map.

`BucketEntryPosition` is the single slot type accepted by `EdgeHandle` and
`CanonicalEdgeOccurrence`. Graph-kernel's `EdgeSlotIndex` is an alias for this same row-local
logical position, not an alternate slot domain. Raw slab offsets and overflow-log entry encodings are
storage-internal values and are never accepted by counterpart APIs. Raw `u32` values appear only at
explicit wire/stable-key codec boundaries. The Graph alias codec rejects logical slots with bit 31
set before adding its transitional reverse-in marker. This ADR has no compatibility alias for the
previous raw-slot shape.

### 2. Canonical ownership is derived by edge semantics

Canonicalization is:

```text
directed                         -> forward entry
undirected u != v               -> entry owned by max(u, v)
undirected self-loop u == v     -> the sole entry
```

A directed self-loop has distinct forward and reverse physical entries.

An undirected self-loop has one physical entry:

```text
counterpart_of(edge) = edge
canonical_handle(edge) = edge.handle
```

Edge kind and canonical ownership are derived inside LARA from authoritative bucket and relation metadata. Callers do not supply a second, potentially inconsistent edge-kind argument when LARA can determine it.

### 3. Pair ordinal is the authoritative counterpart relation

For every non-self logical relation, LARA defines two live physical occurrence sequences.

#### Directed relation

For:

```text
(label, source, target)
```

the two sequences are:

```text
left:
    live entries in Forward[source, label] targeting target

right:
    live entries in Reverse[target, label] targeting source
```

#### Undirected relation

For:

```text
(label, low_endpoint, high_endpoint)
```

the two sequences are:

```text
left:
    live entries in Forward[high_endpoint, label] targeting low_endpoint

right:
    live entries in Forward[low_endpoint, label] targeting high_endpoint
```

The zero-based position of an entry in its equal-target live subsequence is its **pair ordinal**.

For every non-self relation:

```text
left.len == right.len

for every k:
    left[k] and right[k] represent the same logical edge
```

Pair ordinal is not a persistent logical edge identifier. It is the authoritative relation used to recover the counterpart physical entry.

### 4. Pair ordinal is defined only over live entries

Pair ordinal excludes tombstoned entries:

```text
PairOrdinal is defined over the currently live equal-target subsequence.
A tombstoned entry has no PairOrdinal.
```

Logical deletion removes both physical halves from their live occurrence sequences within one no-await mutation boundary.

Derived or cached lookup state must be invalidated before either live sequence changes.

Compaction may remove tombstones and renumber slots, but it preserves the relative order of surviving entries in every relation.

### 5. Logical commit order defines pair order

“Insertion order” means deterministic logical commit order, not incidental physical write order.

For each relation key, every mutation batch determines one logical edge order before projecting entries to its two physical sides.

The rules are:

```text
scalar insert:
    the inserted logical edge is the next relation entry

input-order-preserving batch:
    relation entries follow their order in the accepted batch input

repair:
    both sides are rebuilt from one authoritative logical order

compaction:
    surviving entries retain their existing logical order
```

Independent sorting or reordering of the two projections is forbidden.

A projection-order mismatch is not an accepted alternate representation. It is an invariant failure requiring repair.

No persistent permutation metadata is introduced to legitimize mismatched projection order.

### 6. CounterpartScan is the canonical lookup algorithm

LARA resolves counterparts without persisted counterpart metadata.

Given a physical entry:

1. determine its relation and physical side;
2. find its pair ordinal by counting preceding live entries with the same target in the source label bucket;
3. select the live entry with the same ordinal from the opposite physical sequence.

Conceptually:

```rust
let ordinal = source_bucket.rank_equal_target(
    edge.target(),
    edge.slot(),
)?;

let counterpart = opposite_bucket.select_equal_target(
    opposite_target,
    ordinal,
)?;
```

This algorithm is called **CounterpartScan**.

CounterpartScan is:

- exact for unique and parallel edges;
- the source of truth for counterpart lookup;
- independent of Graph aliases;
- valid for directed and undirected relations;
- the recovery path for repair and validation; and
- free of persisted per-edge counterpart metadata.

Ordinary adjacency traversal does not perform counterpart lookup and is unaffected.

### 7. Singleton relations require no ordinal metadata

For a relation with one live edge:

```text
relation cardinality = 1
pair ordinal = 0
```

No persisted rank, ordinal, permutation, or logical edge identifier is required.

CounterpartScan may use proven canonical structural facts to skip unnecessary rank work when uniqueness is already known.

This ADR does not mandate a persistent per-relation cardinality index. Uniqueness may be established during the canonical scan or by future derived acceleration.

The fact that a singleton relation has implicit ordinal zero does not by itself require any additional storage.

### 8. CounterpartScan may use canonical local optimizations

CounterpartScan is a correctness contract, not a requirement to materialize complete bucket vectors.

Its implementation may use:

- direct slab or overflow-log range traversal;
- chunk prefetch;
- early termination after the required ordinal is found;
- descending or ascending iterators;
- proven singleton shortcuts;
- existing bucket degree and span metadata; and
- heap-only temporary counters.

These optimizations must not introduce persistent per-edge counterpart metadata or change pair-ordinal semantics.

The implementation must not allocate a complete logical-slot vector for each lookup when direct range traversal is available.

### 9. Insertion returns exact physical locations

LARA already determines the physical slot written for each entry.

Insert APIs return those locations directly:

```rust
enum InsertedEdgeLocations {
    SelfLoop {
        canonical: CanonicalEdgeOccurrence,
    },
    Pair {
        canonical: CanonicalEdgeOccurrence,
        counterpart: CanonicalEdgeOccurrence,
    },
}
```

Batch insertion associates physical locations with bounded chunk-local logical ordinals.

Returned locations are internal heap data. They do not require returning one replicated handle per public batch element.

GraphStore must not rescan adjacency to rediscover entries whose exact locations were returned by LARA.

### 10. The bidirectional LARA owner is the mutation boundary

The bidirectional wrapper is the smallest owner that sees:

- directed forward and reverse projections;
- both forward halves of an undirected edge;
- pair order;
- physical slot allocation; and
- compaction and repair.

Canonical single-orientation mutation methods are not exposed to GraphStore as general-purpose APIs.

GraphStore, batch insertion, deletion, inline-property update, compaction, and repair use owner-facing operations that preserve the pair-order invariant.

Read-only forward and reverse accessors may remain public.

### 11. Failure atomicity is based on canonical adjacency

Counterpart correctness depends only on canonical adjacency and pair order.

For a paired mutation:

```text
validate and reserve
→ write both physical projections
→ update mirrored inline property bytes state
→ publish success
```

A successful return must not expose only one live physical half.

Pre-write validation and capacity failures may return recoverable errors.

An impossible post-write invariant failure traps so message rollback preserves atomicity.

No separate counterpart index participates in the canonical commit.

### 12. Reverse repair restores exact pair order

Reverse-adjacency repair must restore more than relation counts.

For every repaired directed relation, it restores:

```text
forward equal-target live sequence
    ↔
reverse equal-target live sequence
```

with exact pair-ordinal correspondence.

First-match association is forbidden for parallel edges.

Repair derives both sides from one authoritative logical ordering plan.

For undirected relations, repair likewise preserves identical logical order in both endpoint projections.

### 13. GraphStore retains canonical sidecars only

`EDGE_PROPERTIES` remains keyed by the canonical sidecar `EdgeHandle`; its position is a `BucketEntryPosition`, not a raw slab/log location.

Canonical property ownership is:

```text
directed                         -> forward handle
undirected u != v               -> max-owner handle
undirected self-loop u == v     -> sole handle
```

Given a non-canonical physical entry, GraphStore calls `canonical_handle`.

Mirrored inline update and logical deletion call `counterpart_of`.

An undirected self-loop updates or removes one physical entry once.

### 14. Edge and inline property bytes slots remain separate domains

Edge slots and inline-property-bytes slots are not numerically paired.

Their association is bucket-local live ordinal.

For every inline-property operation, LARA:

1. resolves the edge entry’s bucket-local live ordinal;
2. applies that ordinal to the inline property bytes sequence;
3. resolves the canonical counterpart occurrence using `counterpart_of`;
4. resolves the counterpart bucket’s inline property bytes ordinal independently; and
5. updates or removes both mirrored values in one no-await commit.

Edge-log indices, inline property bytes log indices, blob positions, and physical slot numbers must not be assumed equal.

Compaction may move edge and inline property bytes storage independently while preserving corresponding live sequence order.

### 15. Logical-slot-preserving movement requires no Graph repair

A leaf slide or rebalance that preserves the logical slot in an `EdgeHandle` requires no
counterpart or property-key repair.

A slot-renumbering operation:

- preserves pair order;
- emits canonical slot moves;
- lets GraphStore repair canonical property keys only for moved canonical entries; and
- does not repair alias keys or alias targets because aliases are removed.

### 16. Physical entry count and mathematical degree remain distinct

An undirected self-loop is stored once.

APIs distinguish:

```text
physical adjacency entries
mathematical graph degree
```

For an undirected vertex:

```text
physical entries = non-loop entries + self-loops
degree           = non-loop incidences + 2 * self-loops
```

Storage, capacity, scan-cost, and compaction logic use physical counts.

Graph algorithms requesting mathematical degree add the second self-loop incidence logically.

## Complexity

Let:

- `d_source` be the scanned portion of the source label bucket;
- `d_opposite` be the scanned portion of the opposite label bucket.

CounterpartScan has:

```text
time:
    O(d_source + d_opposite)

persistent counterpart metadata:
    0 bytes per edge
    0 bytes per relation
```

Implementations may terminate before scanning the full buckets.

For a proven singleton relation, the source ordinal is implicit zero, but the opposite entry may still require a target search.

The purpose of this ADR is correctness, ownership, alias removal, and zero counterpart metadata—not a universal constant-time counterpart lookup guarantee.

## Stable layout

This ADR introduces no persistent counterpart-table layout.

It removes `EDGE_ALIASES` once all callers have migrated.

No stable-layout compatibility is required because the feature is not deployed. Existing development data may be recreated.

Any dormant `MATE_*`, `COUNTERPART_*`, sampled, packed, locator, blob, free-span, or measurement-only stable regions that exist solely for the superseded adaptive-index design are removed unless independently required by another accepted design.

Production stable-memory evolution remains governed by ADR 0039.

## Implementation strategy

Implementation proceeds as a destructive replacement, not a compatibility migration.

### Phase 1: terminology and API boundary

- rename `mate` to `counterpart`;
- introduce final `CanonicalEdgeOccurrence`;
- expose `counterpart_of` and `canonical_handle`;
- remove duplicate edge-kind inputs where LARA can derive semantics;
- simplify `CounterpartLookupError`.

### Phase 2: canonical scan

- implement direct equal-target rank traversal;
- implement direct equal-target select traversal;
- support slab and overflow-log locations;
- avoid per-lookup slot-vector materialization;
- cover directed, undirected, and self-loop contracts.

### Phase 3: exact write locations

- return exact locations from scalar insertion;
- return exact locations by logical ordinal from batch insertion;
- remove post-insert adjacency rediscovery.

### Phase 4: mutation ownership

- make single-orientation canonical mutations crate-private;
- route GraphStore and repair through bidirectional owner methods;
- enforce pair-order preservation on every mutation path.

### Phase 5: alias removal

- migrate canonicalization, update, and deletion callers;
- remove `EDGE_ALIASES`;
- remove alias rebuild, repair, scan, and migration code;
- remove alias-specific tests and benchmarks after replacement coverage exists.

### Phase 6: cleanup

- remove dormant adaptive counterpart-index substrates;
- remove retired codecs, selectors, feature gates, fixtures, and production candidates;
- keep historical measurements only in non-normative evidence documents when still useful.

## Alternatives considered

### Keep the one-way alias B-tree

Rejected because it duplicates LARA physical ownership, canonical-to-counterpart lookup scans the map, and slot repair remains outside LARA.

### Add a reverse alias B-tree

Rejected because it increases persistent bytes and creates another synchronous consistency surface.

### Persist a logical edge ID in each canonical edge occurrence

Rejected because it enlarges the four-byte traversal row and still requires ID-to-location lookup.

### Store a counterpart slot in every edge row

Rejected because every traversal row would pay for an operation mainly needed by updates, deletes, and canonical property access.

### Retain adaptive counterpart tables in this ADR

Rejected because counterpart correctness and Graph alias removal do not depend on a derived accelerator.

Combining ownership transfer, pair-order correctness, compression experiments, stable blob allocation, promotion policy, and runtime adoption made the previous design difficult to reason about and implement.

Adaptive acceleration requires separate evidence and a separate ADR.

### Allow reordered projections with permutation metadata

Rejected because it creates a second pairing authority. Both projections must preserve one logical relation order.

## Consequences

Positive:

- Graph’s per-edge alias index is removed.
- Counterpart correctness belongs to LARA.
- Four-byte edge rows remain unchanged.
- No persistent counterpart metadata is required.
- Unique and parallel edges are exact without logical edge IDs.
- Exact insertion locations remove post-insert rediscovery.
- Pair-order repair and compaction remain within one owner.
- Ordinary adjacency traversal remains unchanged.
- The implementation surface becomes substantially smaller than the superseded adaptive-index design.
- Future acceleration can be evaluated against a stable, correct, metadata-free baseline.

Costs:

- counterpart lookup may scan both relevant label buckets;
- high-degree parallel relations may remain expensive;
- pair-order preservation becomes a mandatory mutation invariant;
- reverse repair must become pair-exact;
- performance acceleration is deferred to a separate decision.

## Planned validation contract

The following checks are required before implementation status can change from `planned
replacement` or adoption can change from `not activated`. They describe the target contract only;
none of these bullets asserts that the current repository already provides the counterpart API,
batch-location return values, alias removal, or the associated tests.

- directed fan-out and fan-in;
- directed self-loops with separate orientations;
- undirected max-owner canonicalization;
- one-entry undirected self-loops;
- physical count versus mathematical degree;
- singleton relations;
- parallel relations;
- equal-target entries interleaved with other targets;
- distinct inline properties on parallel edges;
- exact update and deletion from either physical half;
- slab and overflow-log source/counterpart combinations;
- edge and inline property bytes sequences in different physical domains;
- scalar insertion returning exact locations;
- batch insertion returning exact per-ordinal locations;
- input-order-preserving pair order;
- compaction preserving survivor order;
- reverse repair restoring exact pair order;
- canonical property-key repair;
- mutation rollback and trap atomicity;
- absence of Graph alias dependencies; and
- codebase-wide absence of active `mate` terminology.

Property-based tests generate paired adjacency sequences with arbitrary interleaving and verify:

```text
counterpart_of(counterpart_of(edge)) == edge
```

for every live non-self physical entry.

They also verify:

```text
canonical_handle(edge) == canonical_handle(counterpart_of(edge))
```

and exact logical-edge preservation across insertion, deletion, compaction, and repair.

## Benchmark contract

Canbench compares:

```text
current EDGE_ALIASES
CounterpartScan
```

during implementation, then removes the alias baseline after migration is complete.

Workloads include:

- bucket degrees 1, 8, 32, 128, 1,024, and hub-sized cases;
- all-unique targets;
- one high-cardinality parallel relation;
- mixed singleton and parallel relations;
- directed and undirected relations;
- mixed labels;
- dense slab slots;
- sparse overflow-log slots;
- scalar insertion;
- batch insertion;
- canonicalization;
- inline update;
- deletion;
- compaction; and
- reverse repair.

Measurements report:

```text
instructions
stable reads and writes
heap growth
stable-memory growth
```

The benchmarks establish the baseline for a later adaptive counterpart-acceleration ADR.

## Deferred decision

A later ADR may introduce derived counterpart acceleration.

That ADR must begin from the following constraints:

- PairOrdinal remains the sole authoritative counterpart relation.
- CounterpartScan remains the exact fallback.
- Derived state must never define edge identity.
- Relations that scan cheaply may store no metadata.
- Singleton relations require no ordinal metadata.
- Any stored locator must beat CounterpartScan under measured byte, runtime, and rebuild-amortization gates.
- Production formats must be simple enough to justify their maintenance and failure-atomicity cost.

No specific sampled, packed, shared-orientation, pair-rank, permutation, leaf-blob, or locator design is accepted by this ADR.

## Related

- ADR 0001: labeled segment slide and PMA ownership
- ADR 0020: deferred LARA maintenance
- ADR 0026: reverse-adjacency differential repair
- ADR 0039: production stable-memory evolution
- ADR 0045: batch mutation substrate
- ADR 0049: input-order-preserving batch mutations
- LARA storage contract
- LARA and Graph facade contract
- labeled edge inline-property contract
