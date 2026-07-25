# 0048. LARA-owned counterpart tables replace Graph edge aliases

Date: 2026-07-23  
Status: accepted, not activated  
Implementation status: partial  
Adoption status: hold

## Context

Gleaph uses a physical adjacency location as edge identity:

```text
EdgeHandle = (owner_vertex_id, storage_label_id, slot_index)
```

An edge row stores only its target vertex. Its label, owner, orientation, and slot are supplied by the containing Labeled LARA bucket and iterator.

A local logical edge has one or two physical entries:

| Logical edge                  | Physical entries                      | Canonical entry            |
| ----------------------------- | ------------------------------------- | -------------------------- |
| directed `u -> v`             | forward `(u, v)` and reverse `(v, u)` | forward entry              |
| directed self-loop `u -> u`   | separate forward and reverse entries  | forward entry              |
| undirected `u -- v`, `u != v` | one forward entry at each endpoint    | entry owned by `max(u, v)` |
| undirected self-loop `u -- u` | one forward entry                     | that entry                 |

Graph currently stores an `EDGE_ALIASES` B-tree row for each non-self logical edge. Each row uses an 18-byte serialized key/value payload before B-tree node and allocator overhead.

The alias index has several structural problems:

- it duplicates a physical-slot relation already owned by LARA;
- lookup is efficient only from alias to canonical;
- canonical-to-counterpart lookup scans the alias map;
- scalar insertion rediscovers slots that LARA already knew;
- compaction and slot renumbering require Graph-level alias repair; and
- adding a reverse alias index would at least double the serialized payload and consistency surface.

Counterpart resolution belongs to the bidirectional Labeled LARA boundary, which owns both physical projections, insertion order, slot allocation, compaction, and repair.

## Decision

### 1. LARA owns physical counterpart resolution

Graph removes `EDGE_ALIASES` after the replacement path passes its activation gate.

The bidirectional Labeled LARA owner exposes:

```rust
struct PhysicalEdgeRef {
    orientation: LabeledOrientation,
    handle: EdgeHandle,
}

fn counterpart_of(
    edge: PhysicalEdgeRef,
) -> Result<PhysicalEdgeRef, CounterpartLookupError>;

fn canonical_handle(
    edge: PhysicalEdgeRef,
    kind: EdgeKind,
) -> Result<EdgeHandle, CounterpartLookupError>;
```

Canonicalization is:

```text
directed                         -> forward entry
undirected u != v               -> entry owned by max(u, v)
undirected self-loop u == v     -> the sole entry
```

For an undirected self-loop, `counterpart_of` returns the input entry.

GraphStore retains canonical edge properties and derived-index events. It does not retain a duplicate physical counterpart index.

### 2. Pair ordinal is the authoritative counterpart relation

For every non-self logical relation, LARA defines two physical occurrence sequences.

#### Directed relation

For logical relation:

```text
(label, source, target)
```

the sequences are:

```text
left  = live entries in Forward[source, label] targeting target
right = live entries in Reverse[target, label] targeting source
```

#### Undirected relation

For logical relation:

```text
(label, low_endpoint, high_endpoint)
```

the sequences are:

```text
left  = live entries in Forward[high_endpoint, label] targeting low_endpoint
right = live entries in Forward[low_endpoint, label] targeting high_endpoint
```

The entries in each sequence retain their logical insertion order. Their zero-based position within the sequence is the **pair ordinal**.

For every non-self relation:

```text
left.len == right.len
left[k] and right[k] represent the same logical edge
```

The pair ordinal is not persisted edge identity. It is the authoritative relation by which the two physical entries are joined.

A directed self-loop follows the directed rule across separate forward and reverse sequences. An undirected self-loop has one physical entry and no pair ordinal join.

### 3. Pair order is a mandatory write invariant

All scalar, batch, repair, and compaction paths preserve identical logical-edge order in both physical sequences of a relation.

Other neighbors may be arbitrarily interleaved in either label bucket:

```text
Forward[u, label]:
    x, v:e0, y, v:e1, z, v:e2

Counterpart bucket:
    q, u:e0, u:e1, r, u:e2
```

Only the equal-neighbor subsequences must have identical logical order.

Independent sorting or reordering of the two projections is forbidden.

Compaction may change physical slots, but it preserves live pair order. A transformation that cannot prove this invariant must rebuild both projections from one shared logical-ordinal plan before publication.

Permutation metadata is not an accepted steady-state substitute for pair-order correctness.

### 4. Counterpart scan is the metadata-free canonical algorithm

Every bucket supports exact counterpart lookup without derived metadata.

Given a physical entry:

1. identify its logical relation and physical side;
2. compute its pair ordinal by counting preceding live entries with the same target in the source label bucket;
3. select the entry with the same ordinal from the counterpart label bucket.

Conceptually:

```rust
let ordinal = source_bucket.rank_equal_target(
    edge.target(),
    edge.slot(),
)?;

let counterpart = counterpart_bucket.select_equal_target(
    counterpart_target,
    ordinal,
)?;
```

This algorithm is called **CounterpartScan**.

Its worst-case work is proportional to the scanned portions of the two label buckets. It is always exact and remains the recovery and validation path when derived metadata is absent, rebuilding, malformed, stale, or rejected.

Ordinary adjacency traversal does not read counterpart metadata.

### 5. Exact insertion locations replace post-insert rediscovery

LARA already determines the physical location written for every entry. Insert APIs return those locations directly.

```rust
enum InsertedEdgeLocations {
    SelfLoop {
        canonical: PhysicalEdgeRef,
    },
    Pair {
        canonical: PhysicalEdgeRef,
        counterpart: PhysicalEdgeRef,
    },
}
```

Batch insertion associates locations with bounded chunk-local logical ordinals.

GraphStore must not scan adjacency to rediscover a newly inserted entry when LARA returned its exact location.

The returned locations are internal heap data. They do not require a public replicated response containing one handle per inserted edge.

### 6. Optional acceleration uses sparse counterpart tables

LARA may publish a derived **CounterpartTable** for relations whose measured lookup cost justifies the persistent bytes.

A CounterpartTable is:

- owned by the canonical forward leaf;
- shared by both physical sides of a logical relation;
- sparse at both bucket and relation level;
- omitted entirely when no relation in the leaf is accelerated; and
- derived from canonical adjacency and pair order.

There is no separate forward and reverse counterpart index.

Given either physical half, LARA derives:

- the canonical relation key;
- the canonical owner;
- the canonical forward leaf;
- the input side; and
- the opposite side.

It then consults the single table owned by that canonical leaf.

### 7. Counterpart tables are relation-aware

Within an indexed canonical bucket, records are keyed by the opposite endpoint.

A relation record has one of two forms.

#### Unique relation

A live relation with cardinality one has implicit pair ordinal zero:

```text
Unique:
    canonical_slot
    counterpart_slot
```

It stores no rank, ordinal, permutation, or per-entry discriminator.

A unique relation may also be omitted entirely when scanning is cheaper than its record.

#### Parallel relation

A relation with cardinality greater than one stores its two physical slot sequences in pair-ordinal order:

```text
Parallel:
    canonical_slots[k]
    counterpart_slots[k]
```

The ordinal is implicit in the array position.

Within each side, slots follow bucket order and are therefore strictly ordered under LARA’s logical slot ordering. Lookup from either half:

1. binary-searches the appropriate side’s slot sequence;
2. obtains the pair ordinal from the found array index; and
3. returns the slot at the same index in the opposite sequence.

No persistent logical edge ID is introduced.

A relation missing from the table uses CounterpartScan.

### 8. The index is sparse by relation, not merely by bucket

Bucket degree alone does not determine counterpart-index value.

A bucket with 1,024 different neighbors has different storage and lookup behavior from a bucket containing 1,024 parallel edges to one neighbor.

Admission therefore considers relation-level shape, including:

```text
bucket live entries
distinct target count
unique relation count
parallel relation count
parallel entry count
maximum relation cardinality
observed counterpart requests
measured scan instructions
encoded table bytes
rebuild cost
read-to-update ratio
```

These values are derived during measurement or rebuild. They are not added to every persisted bucket row.

A table may include only some relations from an indexed bucket. Unlisted relations remain canonical and use CounterpartScan.

This ensures that metadata cost scales with accelerated relations rather than automatically with all graph edges.

### 9. Slot sequences use the smallest valid width

Slot values are packed using the smallest width that can encode the relation’s physical slot domain:

| Width | Bytes per slot |
| ----- | -------------: |
| `U8`  |              1 |
| `U16` |              2 |
| `U24` |              3 |
| `U32` |              4 |

Canonical and counterpart sides may use independently selected widths when their slot domains differ.

The production format must support LARA’s total logical slot ordering, including slab and overflow-log locations. It must not assume that an implementation-specific raw integer encoding is numerically ordered unless the codec proves that property.

Compression may use bounded frame-of-reference or block encoding only when:

- direct lookup remains bounded;
- malformed input fails closed;
- exact counterpart parity is validated;
- encoded bytes improve on the uncompressed relation record; and
- lookup instructions do not regress beyond the accepted gate.

Compression algorithms are codec choices, not additional counterpart semantics.

### 10. Remove obsolete acceleration modes

The production design does not define the following former candidate modes:

```text
Sampled
RankedPacked
SharedOrientation
PairRank
BlockRankPermutation
```

Their useful ideas are absorbed as follows:

- `PairRank` becomes the universal pair-ordinal invariant.
- `RankedPacked` becomes the parallel relation’s pair-ordered slot sequences.
- `SharedOrientation` becomes one counterpart table shared by both physical sides.
- singleton rank elision is represented by `Unique`.
- reordered projection exceptions are rejected or repaired rather than persisted.
- checkpoint-only `Sampled` lookup is removed because it does not bound ordinary non-checkpoint resolution.

Historical measurements for these candidates belong in a separate evidence document, not in the normative ADR.

### 11. One locator row is stored per canonical forward leaf

Counterpart metadata is owned by canonical forward leaves, not by orientation/leaf pairs.

A fixed five-byte locator row identifies the current table blob for each canonical forward leaf:

```text
0      Empty: no published table; use CounterpartScan
1      Rebuilding: table must not be read; use CounterpartScan
n >= 2 Published: blob offset = n - 2
```

The locator does not store hotness, generation, mode, bucket count, or table length.

The table blob is self-describing and contains:

```text
header
sparse bucket directory
sparse relation directory per indexed bucket
unique and parallel relation payloads
```

If no relation in a leaf passes admission, no blob is allocated.

The fixed locator store is a dedicated LARA fixed-row store using exact five-byte reads and writes. It is not added to `LabelBucket`, `LabeledVertex`, PMA node metadata, or edge rows.

### 12. Counterpart blobs are replaceable derived state

Variable-sized table blobs use a dedicated byte store and dedicated free-span indexes.

Replacement follows copy-before-publish order:

1. allocate a new span;
2. encode the complete new blob;
3. validate structure and canonical counterpart parity;
4. publish the new locator; and
5. retire the previous span.

Replaced blobs do not shift later blobs and do not leak append-only stable memory.

The locator, blob store, free-span store, and free-span-by-start index form one reopen-validated layout.

A valid `Empty` locator is always a correct recoverable state.

### 13. Mutation invalidates acceleration before canonical change

Canonical adjacency and pair order are the source of truth. Counterpart tables never make an edge live.

The bidirectional LARA owner is the mutation boundary. Single-orientation canonical mutation methods are not exposed to GraphStore.

For a mutation affecting a published canonical leaf:

```text
invalidate
reserve
mutate canonical adjacency
rebuild or schedule rebuild
validate
publish
```

More precisely:

1. make the affected locator unavailable before the first canonical write;
2. reserve all required canonical and derived storage;
3. commit both physical projections while preserving pair order;
4. rebuild the affected sparse table from canonical rows;
5. validate relation cardinality, slot membership, order, and reciprocal mapping;
6. publish the rebuilt blob or leave the locator `Empty`; and
7. retire the old blob only after successful publication.

A multi-step maintenance operation may expose `Rebuilding`. Reads seeing `Rebuilding` use CounterpartScan.

A trap, interruption, decode error, capacity failure, or validation failure leaves no stale published table.

### 14. Slot-preserving work requires no counterpart repair

An ordinary leaf slide or rebalance that preserves physical `EdgeHandle` values requires no table change.

Operations that renumber indexed slots invalidate and rebuild the affected canonical leaf table.

GraphStore repairs canonical property keys only when canonical physical handles move. It no longer repairs alias keys or alias targets.

Reverse-adjacency repair restores exact pair order and then rebuilds affected counterpart tables. First-match association is forbidden for parallel edges.

### 15. GraphStore retains canonical sidecars

`EDGE_PROPERTIES` remains keyed by canonical physical `EdgeHandle`.

Inline values remain mirrored physical payloads.

Given either half of a logical edge:

- property access calls `canonical_handle`;
- mirrored inline update calls `counterpart_of`;
- logical deletion calls `counterpart_of`; and
- an undirected self-loop updates or deletes its sole physical entry once.

Edge and payload slot spaces remain independent.

Their association is bucket-local live ordinal, not numeric slot equality or log-entry index. Payload operations resolve the edge ordinal and apply the same ordinal to the corresponding payload sequence independently on both physical sides.

### 16. Separate physical entries from mathematical degree

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

Storage, capacity, scan-cost, and compaction logic use physical counts. Graph algorithms requesting mathematical degree add the second self-loop incidence logically.

## Complexity

Let:

- `d₁` be the scanned source bucket length;
- `d₂` be the scanned counterpart bucket length;
- `r` be the number of indexed relation records in a bucket; and
- `m` be the cardinality of the selected relation.

### CounterpartScan

```text
time:  O(d₁ + d₂)
space: 0 derived bytes
```

The exact implementation may stop after the required rank or selection is reached.

### Indexed unique relation

```text
time:  O(log r)
space: two packed slots plus sparse directory overhead
```

No ordinal metadata is stored.

### Indexed parallel relation

```text
time:  O(log r + log m)
space: two packed slot sequences plus sparse directory overhead
```

The relation arrays serve both physical lookup directions.

### Mutation

A canonical mutation invalidates the affected table before changing adjacency.

An unchanged table record may eventually support bounded in-place updates when width, capacity, and ordering remain valid. The baseline contract permits rebuilding the affected leaf:

```text
rebuild time: O(indexed entries in the canonical leaf)
```

Admission accounts for this rebuild cost using measured read-to-update ratios.

## Storage gate

The current alias baseline is:

```text
18 serialized bytes per non-self logical edge
```

before B-tree node, allocator, and memory-manager overhead.

Counterpart-table accounting includes:

```text
locator bytes
blob header
bucket directory
relation directory
unique records
parallel slot sequences
free-span metadata
reserved capacity
rebuild reserve
```

Allocator extent rounding and stable-memory page deltas are reported separately from logical bytes.

A table is eligible only when its complete logical byte cost is smaller than the alias rows it replaces for the same covered logical edges.

A table may not claim savings by charging its overhead to unrelated unindexed edges.

Low-degree, cold, singleton-heavy, or directory-heavy shapes remain unindexed when the complete table would not save bytes.

## Runtime and amortization gate

Persistent acceleration is admitted only when all of the following pass:

1. exact parity with CounterpartScan;
2. malformed, truncated, out-of-range, and stale data fail closed;
3. complete logical bytes are below the removed alias baseline;
4. request-time instructions satisfy the bounded runtime target;
5. rebuild cost is amortized by the measured read-to-update ratio; and
6. the relation remains within the production codec’s cardinality and payload bounds.

A missing measurement selects no table.

A failing measurement selects no table.

CounterpartScan remains the default, not a temporary error state.

## Stable layout

The final design adds four LARA-owned logical regions:

1. `COUNTERPART_LEAF_LOCATORS`
2. `COUNTERPART_BLOBS`
3. `COUNTERPART_FREE_SPANS`
4. `COUNTERPART_FREE_SPAN_BY_START`

`EDGE_ALIASES` is removed only after ordinary callers use the LARA counterpart APIs and every required workload stratum passes the activation gate.

There is no in-place migration from `EDGE_ALIASES` in the development layout. Production migration remains governed by ADR 0039.

## Failure atomicity

Counterpart tables are disposable derived state.

The correctness rule is:

```text
canonical adjacency may exist without a table;
a published table may never disagree with canonical adjacency.
```

Publication validates:

- blob bounds and version;
- directory ordering;
- relation uniqueness;
- cardinality;
- slot width and range;
- slot membership in the expected bucket;
- strict side-sequence order;
- equal side cardinality;
- exact pair-ordinal correspondence; and
- absence of trailing or overlapping payload ranges.

Any failed check suppresses the table and uses CounterpartScan.

## Alternatives considered

### Keep the one-way alias B-tree

Rejected because canonical-to-counterpart operations scan the map, slot ownership remains outside LARA, and insertion rediscovery and repair remain duplicated.

### Add a reverse alias B-tree

Rejected because it at least doubles the serialized per-edge payload and introduces another synchronous consistency surface.

### Persist a logical edge ID in every edge row

Rejected because it enlarges the four-byte traversal row and still requires an ID-to-location index.

### Store a counterpart slot in every edge row

Rejected because all traversal rows would pay for an operation used mainly by mutation and property access.

### Index every bucket entry

Rejected because unique-neighbor and cold buckets can consume more bytes than the alias representation. Counterpart tables are sparse by relation.

### Persist pair ordinals

Rejected because ordinal is determined by canonical equal-neighbor order. A unique relation has implicit ordinal zero, and a parallel record’s ordinal is implicit in array position.

### Allow projection-order permutations

Rejected because it creates a second pairing authority and increases mutation, repair, and validation complexity. Canonical pair order must be restored instead.

### Keep checkpoint-only sampled lookup

Rejected because non-checkpoint requests still require unbounded canonical fallback and the format does not provide a clear ordinary-lookup complexity bound.

### Store one index per orientation

Rejected because the two physical sides represent one logical relation. One canonical-owner table serves both directions.

## Consequences

Positive:

- Graph’s per-edge alias B-tree is removed.
- Four-byte edge rows remain unchanged.
- Counterpart correctness requires no persistent logical edge ID.
- Unique relations store no ordinal metadata.
- Cold and unprofitable relations store no counterpart metadata.
- Parallel relations use one shared pair-ordered representation for both directions.
- Exact insertion locations remove post-insert adjacency rediscovery.
- Counterpart ownership, invalidation, repair, and compaction remain inside LARA.
- Ordinary traversal remains independent of counterpart metadata.
- Published metadata can always be discarded and rebuilt from canonical adjacency.

Costs and risks:

- identical pair order across both projections becomes a mandatory write invariant;
- indexed leaves require variable-sized blob allocation and reclamation;
- relation directories add overhead that must be measured rather than amortized optimistically;
- source-slot lookup depends on a proven logical slot ordering;
- slot-renumbering operations rebuild affected tables;
- alias removal remains blocked until ordinary-caller and production-layout measurements pass.

## Test contract

Implementation covers:

- directed fan-out, fan-in, and self-loops;
- undirected canonical ownership and one-entry self-loops;
- exact counterpart resolution from either physical side;
- unique and parallel relations;
- interleaved equal-target entries;
- distinct inline values across parallel logical edges;
- slab and overflow-log slot domains;
- edge and payload sequences in different physical domains;
- scalar and batch insertion returning exact physical locations;
- identical pair order across all insertion paths;
- CounterpartScan parity;
- sparse bucket and sparse relation omission;
- unique and parallel table records;
- all slot widths;
- table growth, removal, and rebuild;
- slot-preserving rebalance;
- slot-renumbering compaction;
- reverse-adjacency repair;
- canonical property-key repair;
- malformed, truncated, overlapping, and out-of-range blobs;
- reopen and partial-layout failure;
- invalidation-before-mutation;
- publication and old-span retirement failpoints; and
- complete removal of Graph alias dependencies.

## Benchmark contract

Benchmarks compare:

```text
EDGE_ALIASES
CounterpartScan
CounterpartTable Unique
CounterpartTable Parallel
```

Workloads include:

- low-degree and high-degree buckets;
- all-unique neighbors;
- mixed unique and parallel relations;
- one high-cardinality parallel relation;
- directed and undirected relations;
- directed and undirected self-loops;
- mixed labels;
- dense slab slots;
- sparse overflow-log slots;
- read-heavy, balanced, and write-heavy ratios; and
- compaction and reverse-repair rebuilds.

Measurements report separately:

```text
lookup instructions
update and delete instructions
insert instructions
rebuild instructions
stable reads and writes
logical serialized bytes
allocator/free-span bytes
stable-memory page deltas
```

Promotion is selected from end-to-end byte and amortized operation cost. No microbenchmark result alone activates alias removal.

## Implementation status

| Capability                                   | State                                        |
| -------------------------------------------- | -------------------------------------------- |
| canonical CounterpartScan                    | implemented                                  |
| exact internal insertion locations           | partially implemented                        |
| owner-facing mutation invalidation           | implemented in current mate-index substrate  |
| leaf blob storage and publication            | implemented in dormant predecessor substrate |
| sparse relation-aware CounterpartTable       | not implemented                              |
| canonical-forward single-locator ownership   | not implemented                              |
| production table codec                       | not implemented                              |
| ordinary-caller activation                   | deferred                                     |
| `EDGE_ALIASES` removal                       | deferred                                     |
| codebase-wide `mate` to `counterpart` rename | required                                     |

Historical candidate measurements and superseded wire formats are maintained outside this ADR.

## Related

- ADR 0001: Labeled segment slide and PMA ownership
- ADR 0020: deferred LARA maintenance
- ADR 0026: reverse-adjacency differential repair
- ADR 0039: production stable-memory evolution
- ADR 0045: batch mutation substrate
- ADR 0049: input-order-preserving batch mutations
- LARA storage contract
- LARA and Graph facade contract
