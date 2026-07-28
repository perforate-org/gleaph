# 0049. Input-order-preserving batch edge insertions

Date: 2026-07-23
Status: Planned
Last revised: 2026-07-28
Anchor timestamp: 2026-07-28 12:58:54 UTC +0000

## Context

ADR 0045 defines a high-throughput unordered Graph mutation path that exposes a
complete bounded batch to Graph and LARA before the first canonical write. Its
partially implemented substrate includes:

- logical ordinals and expansion of one logical edge into directed
  forward/reverse, undirected two-forward-half, or self-loop physical intents;
- read-only placement and full-leaf occupancy projection;
- one-orientation slab, overflow-log, and pending-aware expansion plans;
- reserve-all-then-commit orchestration with rollback before canonical writes;
- exact edge/inline-property-bytes location capture joined by logical ordinal; and
- focused scalar-versus-batch benchmark coverage.

ADR 0045 intentionally leaves the relative order of new edges unspecified. That
semantic freedom is broader than the intended product contract. A caller that
preorders a feed, event stream, dependency list, or other adjacency sequence
must be able to use the batch path without losing that order.

ADR 0048 makes canonical adjacency order and equal-neighbor pair rank the source
of truth for physical counterpart resolution. Its Graph caller migration and
`EDGE_ALIASES` removal are complete. CounterpartScan is the only production
algorithm; no persistent counterpart substrate is owned by this batch-mutation
design. Any future adaptive algorithm requires a separate ADR and measured
adoption decision.

This ADR remains planned ahead of its batch implementation. ADR 0048's owner
boundary and alias removal are prerequisites; this ADR does not introduce
another counterpart compatibility path.

The term **input-order-preserving batch** is used instead of **sorted batch**.
Graph does not interpret an application timestamp, target id, ranking value, or
other sort key. The caller supplies the intended order; Graph and LARA preserve
it at the adjacency boundary that owns scan order.

## Problem

Provide one standard high-throughput batch edge-insertion API that preserves
caller input order while retaining the counterpartrial benefits of ADR 0045:

1. one read-only projection over the complete bounded pending set;
2. capacity planning by orientation, PMA leaf, vertex, and label bucket;
3. contiguous edge and inline property bytes writes;
4. pending-aware overflow-log, expansion, fold, and relocation decisions;
5. shard-local failure atomicity and idempotent retry;
6. exact forward/reverse or undirected counterpart association, including parallel
   edges; and
7. no persistent per-edge sequence field or second source of adjacency truth.

The design must define the scope of the order guarantee precisely. Labeled
adjacency is physically partitioned by orientation, owner vertex, storage label,
and inline-property schema. A traversal spanning several label buckets already
uses label-bucket order, so a global cross-label insertion sequence is not
representable in the current four-byte edge row without a new persistent
sequence key.

The design must also account for the repository's transitional state:

- ADR 0045 is partially implemented and has not introduced its planned public
  unordered wire API.
- ADR 0048's production counterpart boundary is complete; former adaptive
  counterpart storage is removed and its development MemoryIds remain reserved.
- Existing optimized batch geometry is deliberately narrower than scalar
  insertion.

The replacement must reuse completed work without describing planned behavior
as implemented or making ADR 0049 depend on an unaccepted adaptive accelerator.

## Existing architecture assessment

The existing owners can absorb the new contract without a new subsystem:

- Router owns ingress order, authentication, label/property resolution,
  shard routing, public request-size admission, durable client-key identity,
  and its Router-side exact retry fingerprint.
- Graph owns logical mutation order, logical ordinals, physical-intent
  expansion, the independent Graph-side ordered request fingerprint and durable
  receipt, canonical sidecars, label deltas, and durable derived-index events.
- Bidirectional LARA owns physical pair ordering, paired-orientation
  validation, exact returned locations, and CounterpartScan resolution.
- One-orientation LARA owns bucket scan order, edge/inline-property-bytes slab and log
  placement, PMA density, expansion, relocation, compaction, and stable
  allocation.

Canonical adjacency order remains the single source of truth. A logical ordinal
is bounded request-local planning metadata, not a new stable edge identity.
Sampled/Packed counterpart blobs remain derived acceleration. Edge properties remain
GraphStore sidecars and property-index postings remain asynchronously derived.

The current ADR 0045 implementation already produces intents in input ordinal
order, requires strictly increasing ordinals inside each bucket run, and joins
returned locations by ordinal. These are the correct seams to strengthen.
The first ADR 0049 implementation slice now merges undirected owner/alias
projections by physical bucket before reservation and passes a Graph-created
request-local pair table to LARA. LARA validates the merged plan as exact
reversed pairs covered by that table, so the same-bucket case cannot be
represented as two independent reservations. The current internal clean-slab
path now admits mixed directed, undirected, and self-loop shapes through one
owner reservation. Unsupported bucket geometry, the public ordered API, and
the remaining replay/write contract are still planned; this internal slice
does not activate ADR 0049.

The Graph journal now carries the ordered-batch request identity and stable
retirement state in `GraphMutationJournalEntryV1`; its wire projection carries
the identity and the projected retirement enum in
`GraphMutationJournalEntryWireV1`. The fixed V1 stable codec appends these
optional sections under the existing appendix flags, with the old default
(`PlanExecution` plus `NotApplicable`) decoding when the sections are absent.
Router's `bulk_group_fingerprint` remains order-sensitive, but Router state
cannot substitute for Graph's direct replay authority. Section 10 deliberately
removes internal multi-chunk execution from v1, so no chunk identity is needed.
The journal-first endpoint, identity comparison, receipt commit, and replay
algorithm are now implemented for the Graph boundary. The remaining fresh
mutation work is the supported-geometry expansion beyond clean-slab placement
and the Router-side lifecycle integration.

## Prerequisite: complete ADR 0048

ADR 0049 implementation starts only after ADR 0048 satisfies its completion
contract, including:

1. ordinary Graph callers use the LARA-owned counterpart/canonicalization boundary;
2. `EDGE_ALIASES` and its repair paths are removed;
3. scalar insert, batch insert, update, delete, compaction, and reverse repair
   enforce exact pair rank;
4. ScanOnly remains a correct fallback for absent, rebuilding, malformed, or
   stale counterpart acceleration;
5. mutation invalidation and maintenance rebuild scheduling cover all owning
   write paths; and
6. adoption benchmarks and stable-memory accounting justify the selected
   ScanOnly/Sampled/Packed policy.

ADR 0049 must not reactivate alias ownership or add an intermediate counterpart index
to work around an unfinished ADR 0048.

## Decision

### 1. Replace the unordered public contract with one order-preserving edge-insert contract

The standard public edge-insertion batch API preserves input order. There is no public
`ordered: bool`, no `BatchOrderMode`, and no reserved unordered endpoint by
default.

Clients submit each logical edge once in the intended order. They do not submit
forward/reverse rows, undirected counterpart records, LARA bucket keys, physical
locations, or placement instructions.

The v1 operation set is deliberately exhaustive and narrow. The public item is
unresolved and contains no Router-interned catalog id, LARA storage label,
inline-property width, or local vertex id:

```text
OrderedEdgeBatchPublicRequest =
    V1(OrderedEdgeBatchPublicRequestV1)

OrderedEdgeBatchPublicRequestV1 {
    logical_graph_name: String,
    items: [OrderedEdgeInsertPublicItemV1],
}

OrderedEdgeInsertPublicItemV1 {
    source: EncodedVertexId,
    target: EncodedVertexId,
    directed,
    edge_label_name: Option<String>,
    inline_property: Option<CanonicalGqlValueBytesV1>,
    initial_edge_properties: [
        OrderedEdgePropertyPublicV1 {
            property_name: String,
            value: CanonicalGqlValueBytesV1,
        },
    ],
}
```

`CanonicalGqlValueBytesV1` is the existing canonical compact binary encoding of
one GQL `Value`, with an explicit byte bound. `gleaph-gql-ic` provides the Rust
encoder and decoder. Other language bindings implement the same normative
codec contract against the conformance vectors owned by
`gleaph-graph-kernel`; the ordered API does not introduce another value model.
Router decodes both endpoint ids with the graph's `ElementIdEncodingKey`,
resolves the optional edge-label name and every property name through its
graph-scoped catalogs, validates the declared inline schema, and converts the
inline property to the exact fixed-width LARA bytes. Missing, duplicate, malformed,
oversized, or type-incompatible names/values fail during pre-envelope admission.

It does not expose vertex insertion, combined new-vertex/new-edge mutation,
existing inline-property update, or existing vertex/edge property update. ADR 0045
stages 5–7 remain planned internal GraphStore/LARA primitives, but they do not
retain or create an unordered public endpoint. A later public batch operation
requires an explicit revision of this ADR with an exhaustive operation enum,
operation-specific sequential semantics, replay identity, atomicity proof, and
benchmarks. Until then, the ordered edge-insert endpoint is the only specialized
public batch surface and fully replaces ADR 0045's unshipped unordered endpoint.

Every v1 public batch targets exactly one Graph shard. Router must resolve both
endpoints of every logical edge to that same shard, and the one Graph request
owns every complete directed forward/reverse pair or undirected forward-half
set in the batch. A batch containing an edge whose endpoints resolve to
different shards, or containing otherwise shard-local edges assigned to
different shards, is rejected before the active ordered replay envelope is
persisted or any Graph call is dispatched. Cross-shard logical edges and
multi-shard public batches require a separate canonical representation,
atomicity/recovery contract, and ADR revision.

For one admitted public batch:

```text
logical_ordinal(public_input[i]) = i

graph_items = public_input
logical_ordinal(graph_items[i]) = i
```

The item position is the sole request-local ordinal. After resolution, Router
constructs a distinct typed Graph envelope. The item does not duplicate its
ordinal in a field or parallel ordinal array:

```text
OrderedEdgeBatchGraphRequest =
    V1(OrderedEdgeBatchGraphRequestV1)

OrderedEdgeBatchGraphItemV1 {
    source_local_vertex_id: LocalVertexId,
    target_local_vertex_id: LocalVertexId,
    directed,
    catalog_edge_label_id: Option<EdgeLabelId>,
    inline_property_bytes: Vec<u8>,
    resolved_initial_edge_properties: [
        ResolvedOrderedEdgePropertyV1 {
            property_id: PropertyId,
            value: CanonicalGqlValueBytesV1,
        },
    ],
}

OrderedEdgeBatchGraphRequestV1 {
    graph_id,
    target_shard_id,
    target_graph_canister,
    resolved_labels: ResolvedLabelTable,
    resolved_properties: ResolvedPropertyTable,
    items: [OrderedEdgeBatchGraphItemV1],
}
```

Version envelopes exist only at independently encoded persistence or wire
boundaries. `OrderedEdgeBatchPublicRequest` and
`OrderedEdgeBatchGraphRequest` therefore carry outer `V1(...)` variants.
Stable Router/Graph records and independently returned Graph results use the
same rule. Types that exist only inside a parent V1 schema—request identity,
payload, progress, diagnostics, receipt payloads, and physical intents—use
their `*V1` type directly and do not add a second `V1(...)` wrapper. Their enum
variant names are semantic names such as `PlanExecution` and
`OrderedEdgeBatch`, without a redundant version suffix. A nested type gains its
own version envelope only if it later becomes an independent encode/decode
boundary with a separately supported compatibility lifecycle.

| Independent boundary          | Sole outer envelope                 | Directly nested V1 schema                                               |
| ----------------------------- | ----------------------------------- | ----------------------------------------------------------------------- |
| Public ingress                | `OrderedEdgeBatchPublicRequest::V1` | public request, items, and properties                                   |
| Router → Graph canonical call | `OrderedEdgeBatchGraphRequest::V1`  | resolved request, items, and properties                                 |
| Graph canonical response      | `GraphOrderedEdgeBatchResult::V1`   | result and receipt payloads                                             |
| Graph retirement response     | `OrderedMutationRetirementAck::V1`  | acknowledgement and receipt payload                                     |
| Router stable record          | `RouterMutationRecord::V1`          | identity, payload, progress, diagnostics, failures, and receipt payload |
| Graph stable journal          | `GraphMutationJournalEntry::V1`     | identity, retirement, progress, and receipt fields                      |
| Graph journal wire read       | `GraphMutationJournalEntryWire::V1` | identity, projected retirement, progress, and receipt fields            |

The request-level resolved tables are the only Graph-envelope definitions of
label/property names, ids, inline profiles, and inline schemas. Items reference
those tables by catalog id; they do not carry independently derived storage
labels or widths. On a fresh request only, after the journal-first replay gate
below returns `Absent`, Graph validates that every referenced id exists exactly
once in the tables, derives the physical storage label and width through its
existing GraphStore/LARA boundary, validates every canonical property value,
and rejects unused or conflicting table entries before planning. A completed
exact replay does not revalidate these facts against mutable current catalogs or
schema. The Router record clears its old top-level resolved tables only after
this envelope, which now owns the resolved catalog projection, is durably
installed.

V1 has no public or Graph `execution_flags`: ordering, location capture,
fallback, and maintenance behavior are derived from the validated request and
owner policy, not caller-selected modes. The shared constructors enforce:

```text
1 <= items.len <= 1_024
initial_edge_properties.len <= 128 per item
logical-graph/label/property name UTF-8 bytes <= 256
CanonicalGqlValueBytesV1.len <= 65_536 per value
physical_projection_count <= 2_048
actual encoded public request <= MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
actual encoded Graph request <= MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
actual encoded Router mutation record in every lifecycle state
    <= MAX_ROUTER_MUTATION_RECORD_BYTES
MAX_ROUTER_MUTATION_RECORD_BYTES
    == MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
```

All count multiplication and encoded-length arithmetic is checked. The full
encoded-message/record bounds remain authoritative even when every individual
field is within its local limit.

Canonical encoding never reorders the edge-item array. Within one item,
property assignments are a semantic map: the public encoder orders them by
property-name UTF-8 bytes, and the Graph encoder orders resolved assignments by
`PropertyId`. Duplicate names, duplicate resolved ids, or a sidecar assignment
that duplicates the label's named inline property are rejected. Resolved label
and property tables use their existing canonical id order. Each fingerprint
domain therefore hashes the same property-map semantics without treating
incidental property-list order as edge insertion order.

Graph logical planning and `PhysicalProjectionIdV1.logical_ordinal` use each
item's zero-based array position. Before the active ordered replay envelope is
persisted, the Router constructor proves that the single Graph request contains
exactly `public_item_count` items in unchanged public input order. Graph repeats
the non-empty/count/bounds checks. The order-sensitive public and Graph
fingerprints reject any replay whose array order changes.

Router also proves at construction that the one request's
`(graph_id, target_shard_id, target_graph_canister)` matches one live registry
binding. The immutable typed request is the sole stored routing identity; no
parallel shard-id/principal fields are maintained. Graph validates
`target_graph_canister` against its own principal and the authorized Router
caller before journal lookup. It validates `target_shard_id` against current
durable routing metadata only after a journal miss and before fresh planning.
An exact completed replay therefore remains available after catalog, schema,
routing metadata, registry capability, or shard-binding state changes; current
caller authorization is still required and is never bypassed by replay.

The normal public receipt is aggregate-only, so Router does not promise or
reconstruct a per-item result sequence. If a later API exposes per-item results,
the authenticated Graph item array position is the only allowed association key
and that response contract requires an ADR revision.

The relevant ordinal is included in each canonical request encoding. Reordering
otherwise identical items therefore changes the corresponding fingerprint. The
public request and the routed Graph request are different messages and
deliberately use different fingerprint domains:

```text
PublicBatchFingerprintV1 =
    SHA-256("gleaph:ordered-edge-public:v1\0" ||
            canonical_public_request_bytes)

GraphRequestFingerprintV1 =
    SHA-256("gleaph:ordered-edge-graph:v1\0" ||
            canonical_exact_graph_request_bytes)
```

`PublicBatchFingerprintV1` covers the complete ordered public input and public
options before routing: the logical graph name plus the exact ordered public
item array, including opaque endpoint bytes, label/property names, and canonical
value bytes. Router uses it for the top-level client-key
identity. The canonical public bytes exclude the client key, mutation id, and
fingerprint field itself; those are stored as identity keys around the hashed
content. `GraphRequestFingerprintV1` covers the exact single-shard request after
label/schema resolution and routing, including graph/shard identity, the
unchanged input sequence, resolved tables, identifiers, payloads, and
properties. It excludes its fingerprint field itself; `mutation_id`
remains the separate journal lookup key and is also excluded from the hashed
content. Both canonical byte sequences include the outer request version tag;
changing `V1` to a future envelope version therefore changes the fingerprint.
The Graph fingerprint need not equal the public fingerprint. A future envelope
version also uses a new domain separator or an explicitly versioned canonical
encoding; it may not reuse a V1 fingerprint for different decoding rules.

`gleaph-graph-kernel`, as the portable Rust owner of both bounded wire schemas,
owns the public request and typed Graph envelope types, their distinct canonical
encoders, bounds, domain separators, hash helpers, and normative cross-language
conformance vectors. Router calls the public encoder/hash helper; Graph calls
the Graph-envelope encoder/hash helper. The generated SDK does not compute or
submit either idempotency fingerprint. Router persists the public fingerprint,
the exact typed Graph replay envelope, and its Graph request fingerprint before
the Graph dispatch `await`. Graph recomputes the Graph fingerprint from the
received envelope and compares it with both the transmitted value and its local
journal identity. Router state is not Graph's replay source of truth, and the
fingerprint is derived integrity metadata rather than a second request schema.

`gleaph-graph-kernel` also owns the bounded opaque
`CanonicalGqlValueBytesV1` wrapper used by the wire without depending on the GQL
value implementation. `gleaph-gql-ic` owns the Rust conversion between that
wrapper and the existing canonical `gleaph-gql::Value` binary codec. The
JavaScript SDK owns only its language binding for producing those public value
bytes. It must pass the same normative value-codec conformance vectors exported
from the Rust owner; it does not define an independent tag table, canonicality
rule, bound, or fingerprint algorithm. Router decodes and canonically re-encodes
SDK-supplied value bytes and rejects a byte sequence that does not round-trip
identically before constructing the Graph envelope.

The public ingress bound must also bound the total encoded durable Router replay
record. Router rejects the public request before active-payload persistence or
Graph dispatch when that owner-defined record bound would be exceeded. Later
diagnostic, terminal, progress, and completed transitions repeat the same
record-wide encoded-size check; bounded owner constructors ensure those
transitions cannot introduce arbitrary external text.

The initial client-key reservation is deliberately earlier than those
envelope-dependent checks. The exact sequence is:

1. Validate the client key, public wire decoding, and the gross
   item/name/value/request bounds required to construct canonical bytes safely;
   compute `PublicBatchFingerprintV1`. Failure here creates no mutation
   reservation and is not an idempotent terminal result.
2. Persist or reacquire `OrderedEdgeBatchRouting` under the client key and its
   routing lease.
3. Resolve endpoint ids, the single target binding, labels, inline schema, and
   property names; construct the exact Graph envelope and its fingerprint.
4. Validate the single-shard rule, constrained-property rule, item/request cost
   bounds, exact encoded Router replay-record bound, and registry/capability
   binding.
5. Atomically replace routing state with the active exact replay envelope and
   clear the routing lease before the Graph dispatch `await`.

Thus “before persistence” in this ADR means before **active ordered payload**
persistence, not before the initial no-dispatch client-key reservation. No
Graph call is possible while `OrderedEdgeBatchRouting` is stored.

### 2. Define order at the bucket that owns it

The order-bearing physical bucket key is:

```text
(orientation, owner_vertex_id, storage_label_id, inline_property_byte_width)
```

For every affected bucket `b`, let:

```text
pending(b) =
    stable_filter(input logical ordinals, projects_to_bucket(b))
```

After a successful commit:

```text
ascending_live_order_after(b) =
    ascending_live_order_before(b) ++ pending(b)
```

Consequently:

- ascending traversal returns pre-existing live rows followed by the pending
  input subsequence;
- descending traversal returns the exact reverse of that bucket-local live
  sequence;
- pre-existing live rows retain their relative order;
- deletion preserves the relative order of remaining live rows; and
- rebalance, overflow-log fold, expansion, relocation, and compaction preserve
  the same live sequence even when physical slots move.

The contract does not promise one global input order across different labels,
owners, or orientations. Cross-label traversal continues to use the existing
label-bucket ordering. Applications requiring a global order either use one
order-bearing label/adjacency or request an explicit query-level `ORDER BY`.
Adding a stable sequence id to every edge is outside this ADR.

### 3. Separate logical pairing from merged physical runs

Graph expands input edges once:

| Logical input                 | Physical projections                                    |
| ----------------------------- | ------------------------------------------------------- |
| directed `u -> v`             | forward `(u, v, ordinal)` and reverse `(v, u, ordinal)` |
| undirected `u -- v`, `u != v` | two forward halves carrying the same ordinal            |
| undirected self-loop `u -- u` | one forward half carrying the ordinal                   |

Each physical bucket receives the stable input subsequence that projects to it.
Physical bucket processing order may differ between orientations and leaves,
but pending rows inside a bucket may not be independently reordered.

For every pair key:

```text
directed:   (kind, label, source, target)
undirected: (kind, label, min(endpoint), max(endpoint))
```

the two projections carry the same ordinal subsequence. The `k`th live forward
entry therefore remains paired with the `k`th live reverse or undirected counterpart
entry. Sorting a forward projection and reverse projection independently is an
invariant violation even if each result is locally deterministic.

Pair validation and physical reservation use different representations:

```text
PhysicalProjectionIdV1 {
    logical_ordinal,
    side,
}

PhysicalProjectionSideV1 =
    DirectedForward
  | DirectedReverse
  | UndirectedLowerOwner
  | UndirectedHigherOwner
  | UndirectedSelfLoop

LogicalBatchPairTable
    logical_ordinal -> logical kind, endpoints, label, width,
                       expected physical projection identities

PhysicalOrientationPlans
    orientation -> bucket key -> stable ordinal-ordered physical rows
```

Graph constructs the logical pair table as the sole request-local source of
pair membership and passes that same ephemeral table through the bidirectional
LARA boundary; no second pairing table is maintained. It then groups every
physical intent exactly once by:

```text
(orientation, owner_vertex_id, storage_label_id, inline_property_byte_width)
```

The projection side is request-local identity metadata, not part of the
physical reservation key. The physical row also carries its expected owner,
orientation, label, width, and endpoint projection so validation does not infer
these from the side name. Directed edges, including directed self-loops, have
two distinct projection ids. A non-self undirected edge has two distinct
lower-owner/higher-owner ids, and an undirected self-loop has one.

Logical owner/canonical/counterpart role is metadata on a projection; it is not part of
the physical reservation key. All roles targeting the same physical bucket are
merged into one stable ordinal-ordered run before reservation. There is at most
one reservation snapshot for an orientation/bucket pair.

For example, input `[3 -- 2, 2 -- 1]` sends both rows owned by vertex `2` to one
forward bucket run even though vertex `2` is the second endpoint of the first
edge and the first endpoint of the second. The run contains ordinals `[0, 1]`;
two role-specific reservations for that bucket are forbidden.

The bidirectional LARA owner validates that the merged physical projection
multiset exactly matches the logical pair table before any reservation. It
returns a map keyed by `PhysicalProjectionIdV1`, rejects missing or duplicate
ids, and Graph joins exact locations back through the pair table. ADR 0049
therefore requires a new internal plan/API shape, such as:

```text
PairedBatchPlan {
    logical_pairs,
    physical_orientations,
}
```

The current `BidirectionalBatchPlan::{Directed, Undirected, SelfLoop}` may be
removed or retained only for a narrower proven caller. It is not the ADR 0049
plan type for arbitrary mixed or undirected batches.

### 4. Preserve physical-planning freedom outside bucket-local row order

Order preservation does not prescribe the order in which LARA processes bucket
runs. The planner may continue to:

- group and process runs by orientation, PMA leaf, vertex, and label;
- reserve forward and reverse capacity independently;
- order bucket work for stable-memory locality;
- combine multiple bucket projections into one leaf expansion or relocation;
- select slab, overflow log, expanded slab, or relocated slab destinations;
- write edge and inline property bytes spans contiguously; and
- aggregate metadata, counterpart invalidation, sidecar, and derived-event updates.

Only the live row sequence within each bucket is constrained. This distinction
retains the principal batch efficiencies of ADR 0045.

### 5. Reuse ADR 0045 plan/reserve/commit ownership

The mutation remains split into three phases:

```rust
let plan = graph.plan_batch_mutation(input)?;       // read-only
let reservation = graph.reserve_batch_mutation(plan)?; // capacity only
let result = graph.commit_batch_mutation(reservation); // no recoverable failure
```

Graph's logical plan owns:

- input ordinals and the Graph request fingerprint;
- logical-to-physical projection;
- the `LogicalBatchPairTable` and canonical/counterpart roles;
- property and derived-event association; and
- the complete expected ordinal set for each logical shape.

LARA's physical reservation owns:

- the existing bucket/leaf fingerprints;
- the current live sequence and append boundary;
- destination spans and log ranges;
- edge/inline-property-bytes capacity and allocator effects;
- exact physical locations when capture is requested; and
- the proof that every planned transformation preserves the live sequence.

The bidirectional LARA wrapper validates the logical pair table against the
merged physical projection multiset: exact projection-id set and cardinality,
expected owners/orientations, reversed endpoints, equal labels/widths, and
equal ordinal subsequences. It then reserves each physical orientation/bucket
only once before any orientation commits.

All recoverable validation and allocation failure occurs before the first
canonical write. After that point, an invariant failure traps the canister
message so the shard-local state rolls back atomically. Direct non-transactional
library callers do not gain the canister message rollback guarantee.

### 6. Use append semantics, not arbitrary tombstone reuse

An insertion destination is valid only when the resulting ascending live
sequence satisfies the bucket contract.

The following are valid when their preconditions are proved:

- append at the slab live tail;
- append an oldest-to-newest chain at the overflow-log logical tail;
- fold the existing slab/log sequence, then append pending rows;
- expand or relocate while copying existing live rows in order, then append
  pending rows; and
- reuse physical slack that is at the logical append boundary.

An old tombstone hole that precedes a surviving live row is not an ordered
insertion destination. It may be reclaimed by maintenance only if compaction
preserves the relative live order and repairs every affected physical handle,
inline property bytes ordinal, counterpart accelerator, and canonical sidecar through their owning
boundaries.

The planner must validate logical order, not infer it solely from numeric slab
or overflow-log indices. Edge and inline property bytes locations remain separate physical
domains joined by bucket-local live ordinal.

### 7. Keep aggregate-only output as the normal path

Preserving order does not require returning every physical location. The normal
batch path returns a compact receipt. Exact location capture remains an explicit
internal mode for consumers that require physical handles during the same
commit.

Counterpart lookup derives from canonical pair rank and the completed ADR 0048
boundary. It does not require Graph to persist the request ordinal or force
location counterpartrialization for every ordinary batch.

Initial edge properties are such an internal consumer. If any item carries a
non-empty `resolved_initial_edge_properties`, Graph selects location-capture
mode for the whole physical batch before reservation. LARA still returns the
complete physical-location map keyed by `PhysicalProjectionIdV1`. GraphStore
joins that map through the existing `LogicalBatchPairTable` into a distinct
ephemeral result:

```text
CanonicalSidecarHandleByOrdinalV1 {
    logical_ordinal,
    canonical_projection_id: PhysicalProjectionIdV1,
    edge_handle,
}

CanonicalSidecarHandlesV1 =
    logical_ordinal -> CanonicalSidecarHandleByOrdinalV1
```

The key set must be exactly `0..logical_item_count`, including items without
initial properties because location-capture mode applies to the whole batch.
Each value's `logical_ordinal` and
`canonical_projection_id.logical_ordinal` must equal its map key, and the pair
table must classify that projection as the sole canonical property owner.
Directed reverse and undirected non-owner locations are never accepted as
independent property owners. This joined map is not persisted and is not a
second pair table; the request-local pair table remains the sole authority from
which GraphStore derives and validates it.

Before the first LARA canonical write, GraphStore validates every property
value/schema/index rule, rejects duplicate property ids within an item, proves
the complete logical-ordinal/property association, and reserves or otherwise
makes non-failing every required sidecar and durable derived-event operation.
After LARA commit returns the planned locations, GraphStore constructs and
validates the exact canonical-handle key set, then writes canonical sidecars and
durable net property-index events in the same no-`await` Graph message. A
missing, duplicate, wrong-owner, mismatched, or out-of-range logical handle is
an invariant trap, so the canister message rolls back LARA rows, sidecars, and
events together.

When every item is property-free and no other internal consumer requests
handles, aggregate mode may omit location counterpartrialization. In either mode the
public receipt remains aggregate-only; captured locations are discarded only
after all sidecars and derived events have been committed.

An internal captured location also carries the bucket-local `logical_slot` used
by canonical sidecars. This is the physical row position within the bucket,
including slab tombstone positions and preceding overflow-log rows; it is not a
live-edge ordinal, input ordinal, or serialized field in the edge row. Compaction
and relocation may move this slot and must use the existing sidecar move
boundary.

### 8. Define the v1 geometry contract explicitly

ADR 0049 does not claim that the current optimized planner supports every scalar
geometry. The first public version has this admission matrix:

| Input geometry                                                                   | v1 behavior                                                                                         |
| -------------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------- |
| existing named labeled buckets on supported slab/log/expansion/relocation paths  | optimized merged physical plan                                                                      |
| mixed directed, undirected non-self-loop, and undirected self-loop items         | optimized when every projected bucket is supported; the shape mix alone is never a rejection reason |
| one undirected forward bucket receiving both endpoint roles                      | optimized only through one merged physical run and one reservation                                  |
| a new named labeled bucket                                                       | whole-request ordered scalar fallback                                                               |
| default/unlabeled bypass or labeled promotion from default storage               | whole-request ordered scalar fallback                                                               |
| any other scalar-supported but optimized-unsupported geometry                    | whole-request ordered scalar fallback                                                               |
| parallel logical targets with proven pair-rank and sidecar coverage             | optimized when every projected bucket is supported; replay/reservation gates still apply           |
| any initial property governed by a global unique or constrained-property rule    | whole-public-request pre-dispatch rejection in v1                                                   |
| one logical edge whose endpoints resolve to different Graph shards               | whole-public-request pre-dispatch rejection in v1                                                   |
| otherwise shard-local edges resolving to more than one Graph shard               | whole-public-request pre-dispatch rejection in v1                                                   |
| geometry not covered by either a proven optimized path or proven scalar fallback | pre-write rejection                                                                                 |

The whole request is classified before allocator or canonical mutation. Graph
does not optimize a supported prefix and then fall back for the remainder.
Scalar fallback is admitted only after implementation provides whole-request
read-only validation and any required capacity reservation. It then applies
logical items sequentially in input ordinal order within the same Graph
message. Its commit segment must have no recoverable error after the first
canonical write; an invariant failure traps so canister message rollback
preserves shard-local atomicity. Calling today's scalar method repeatedly and
returning its first error is not a valid fallback.

The current `DefaultLabelUnsupported` and unsupported-geometry classifications
remain truthful implementation limitations. Mixed shape is no longer rejected
by the internal clean-slab path; supported scalar-only geometry now uses the
whole-request ordered scalar fallback, while geometry outside both proven paths
still returns before canonical write. These are not the target public contract. Fallback is a correctness and incremental-delivery
mechanism, not the final performance design, and may not weaken ordering,
atomicity, pair rank, counterpart invalidation, sidecar, or derived-event behavior.

ADR 0030 coordination is not silently bypassed. Before persisting an
`OrderedEdgeBatch` payload or dispatching the Graph request, Router resolves every
initial edge property and rejects the whole public request when any property is
governed by a global unique or constrained-property rule. Under the no-dispatch
proof, the initial client-key routing reservation records this deterministic
rejection as `terminal_failure`; it is not released as a retryable reservation.
V1 does not acquire a partial set of TCC claims and does not dispatch an
unconstrained subset. Supporting such properties later requires one
pre-dispatch claim set covering every relevant item, durable claim/release
recovery, and a revision to this ADR. GraphStore continues to validate local
value/schema constraints during whole-request preflight.

### 9. Treat parallel inserts as distinct ordered items

The final contract permits multiple new logical edges with the same endpoint
pair when the Graph data model permits parallel edges. Their logical ordinals,
inline properties, properties, and exact returned locations distinguish them.

The internal Graph planner now admits parallel logical targets as distinct
ordered items. Same-batch directed and undirected parallel inputs preserve
their logical ordinals and may carry distinct or identical inline properties.
The Graph facade also co-writes per-item initial sidecars and resolves each
counterpart by live pair rank. This does not activate a public ordered batch
endpoint: the current Graph journal has no request fingerprint or aggregate
receipt for this payload, so durable exact replay remains a separate gate.

Existing edge/vertex updates are absent from the v1 operation set. A future
operation that permits conflicting updates must define deterministic sequential
semantics; insertion order must not create accidental last-write-wins behavior
for unrelated mutation kinds.

### 10. Make one Graph shard and one Graph request the v1 public batch boundary

The v1 public order-preserving batch admits only an input whose every logical
edge resolves to one target Graph shard and whose complete projection fits in
one bounded Graph request and one Graph message execution budget. Router applies
a conservative admission classifier using target identity, encoded size, item
count, and measured cost-class caps before dispatch; Graph repeats the owner-side
admission check. If the input resolves to multiple shards or would require a
second Graph request, Router rejects the entire public request before sending
the Graph call. An unexpected instruction-limit breach traps the Graph message;
it is never converted into a partial-success receipt.

ADR 0041/0042 transport chunking and continuation remain valid for their
existing multi-mutation APIs, but they are not transparently reused inside one
ADR 0049 public batch. A caller may submit a smaller later batch under a new
mutation identity; ordering between those separate public calls is outside this
ADR.

The admitted Graph request is shard-local atomic and contains every physical
projection of every logical edge in the public batch. Its unchanged input order
is authoritative for every bucket-local subsequence affected by that request.
There is no Router fan-out, shard-map iteration, sibling canonical outcome, or
cross-shard partial-success state in v1.

Because v1 has no internal Graph chunking, no `chunk_index` exists in its
identity. The durable Graph replay identity is:

```text
(mutation_id, GraphRequestFingerprintV1)
```

An exact retry returns the durable receipt. Reuse of a `mutation_id` with
different content, order, schema version, or item count is rejected before any
canonical write. Supporting multiple Graph chunks under one public batch would
require a separate durable sequencer/high-water-mark protocol and a new ADR
revision.

### 11. Remove the unordered API unless benchmarks prove a counterpartrial need

ADR 0049 activation supersedes ADR 0045's planned unordered public endpoint.
No compatibility requirement exists because that wire API has not shipped.

Do not implement or retain a dead unordered path solely to benchmark it. The
current ADR 0045 implementation already writes bucket runs in logical ordinal
order and is not evidence of a counterpartrially faster unordered alternative.

If implementation work discovers a concrete reordering optimization that
cannot be expressed by bucket processing order alone, add controlled canbench
comparisons for:

- fan-out and fan-in with many items in the same bucket;
- mixed owners, labels, and PMA leaves;
- unique and parallel endpoint pairs;
- edge-only and fixed-width payloads;
- clean slab, overflow-log append, expansion, and log fold;
- sparse/tombstone-heavy buckets; and
- representative batch sizes including 128 and 1,024 logical edges where
  instruction limits permit.

Measure setup, planning, canonical commit, counterpart invalidation/rebuild,
sidecar/derived-event creation, and required maintenance separately, plus the
maintenance-inclusive end-to-end total. Primary metrics are instructions per
logical edge, stable reads/writes, stable bytes or pages where meaningful,
relocation, log debt, and admission rate.

An unordered public API may be reconsidered only if an actual implementation
candidate exists and, after removing avoidable ordered-path overhead, persisted
results show at least a 20% end-to-end instruction or maintenance-inclusive
stable-write advantage on at least two
representative workload shapes and two batch sizes, or an equivalently counterpartrial
admission-rate loss. A setup-only, location-capture-only, deferred-maintenance,
or single pathological-case difference is insufficient.

The same evidence gate applies in the other direction: ordered activation may
not remove the ADR 0045 batch substrate when the measured ordered path is at
least 20% worse on the same end-to-end metrics and representative matrix. In
that case the substrate remains an explicitly scoped fallback or requires an
ADR revision to retain an unsorted public surface; the decision cannot be made
from scalar-versus-ordered results alone.

Meeting the gate does not silently change the standard API. Retaining an
unordered endpoint requires an explicit revision to this ADR that records the
measurements, product need, and additional public-surface cost.

## State representability and fresh-layout replacement

The adjacency order contract is representable without changing the persisted
four-byte edge row:

- request order is represented by bounded heap logical ordinals during
  planning/commit;
- stable adjacency order remains represented by the existing bucket slab/log
  sequence;
- pair identity remains represented by equal-neighbor occurrence rank under
  ADR 0048; and
- retries extend the existing Graph journal ownership from ADRs 0015, 0044, and
  0047 with an order-sensitive Graph-side identity.

No new stable per-edge sequence table, row field, or counterpart identity is introduced.
If implementation proves that an existing physical transformation cannot
preserve live order without such a field, that geometry remains unsupported
until a separate stable-layout decision is accepted.

The replay contract is **not** representable in the current fields of
`GraphMutationJournalEntryV1` or `GraphMutationJournalEntryWireV1`. ADR 0049
replaces those V1 definitions in place with a complete request-kind identity
while preserving the state required by every current scalar and bulk consumer:

```text
GraphMutationRequestIdentityV1 =
    PlanExecution
  | OrderedEdgeBatch {
        canonical_encoding_version: u16,
        graph_request_fingerprint: [u8; 32],
        logical_item_count: u32,
    }

GraphMutationJournalEntry =
    V1(GraphMutationJournalEntryV1)

GraphMutationJournalEntryV1 {
    mutation_id,
    state,
    row_count,
    emitted_delta_first_seq,
    emitted_delta_last_seq,
    hot_forward_vertices,
    recorded_at_ns,
    retirement: GraphMutationRetirementV1,
    next_index,
    bulk_progress,
    request_identity: GraphMutationRequestIdentityV1,
}

GraphMutationRetirementV1 =
    NotApplicable
  | Active
  | Retired { at_ns: u64 }

GraphMutationJournalEntryWire =
    V1(GraphMutationJournalEntryWireV1)

GraphMutationJournalEntryWireV1 {
    mutation_id,
    state,
    row_count,
    emitted_delta_first_seq,
    emitted_delta_last_seq,
    hot_forward_vertices,
    retirement: GraphMutationRetirementWireV1,
    next_index,
    bulk_progress,
    request_identity: GraphMutationRequestIdentityV1,
}

GraphMutationRetirementWireV1 =
    NotApplicable
  | Active
  | Retired
```

`mutation_id` remains the stable-map key and is stored in the journal entry.
Stable-only `recorded_at_ns` and the `Retired.at_ns` timestamp remain absent
from the wire form, matching the current owner boundary. The wire retirement
enum is a read-only projection, not a second source of truth:
`GraphMutationRequestIdentityV1::PlanExecution` maps to `NotApplicable`,
ordered `Active` maps to `Active`, and ordered `Retired { .. }` maps to
`Retired`.
`GraphMutationRetirementV1` is the Graph-owned proof of whether Router may
still redispatch the canonical request.
`GraphMutationRequestIdentityV1::PlanExecution` requires stable and wire
retirement `NotApplicable`;
`GraphMutationRequestIdentityV1::OrderedEdgeBatch` requires stable and wire
retirement `Active` or `Retired`.
The exact Rust field types and optionality remain those of the current
structures unless this ADR explicitly revises them. The new encoding-version
and item-count fields use the fixed widths shown above; count conversion from
`items.len()` is checked. The schema requires that no current completion, delta
projection, hot vertex, continuation, retention, or bulk-progress state is
dropped.

Request identity also constrains the valid shared journal fields. For
`GraphMutationRequestIdentityV1::OrderedEdgeBatch`, the only valid durable
combination is:

```text
state == Completed
next_index == None
bulk_progress == None
row_count == request_identity.logical_item_count as u64
```

The ordered endpoint has no chunk or continuation boundary and never persists
an intermediate journal entry: its completed identity, canonical mutation, and
receipt commit in one no-`await` Graph message. Ordered `Incomplete`,
`next_index`, or `bulk_progress` combinations are durable corruption, not
recoverable work. A `GraphMutationRequestIdentityV1::PlanExecution` entry whose
retirement is not `NotApplicable`, or an ordered entry whose retirement is
`NotApplicable`, is also corruption. The journal constructor, stable/wire
encoder, decoder, and
request-kind accessors all reject these combinations fail-closed.
`GraphMutationRequestIdentityV1::PlanExecution` alone may use its existing
`Incomplete`, `next_index`, and `bulk_progress` combinations.

`GraphMutationRequestIdentityV1::PlanExecution` preserves the existing scalar,
legacy-bulk, and typed-bulk journal semantics. This ADR does not silently
strengthen their current content-fingerprint behavior. The ordered Graph owner
persists `GraphMutationRequestIdentityV1::OrderedEdgeBatch` in the same atomic
message as the receipt and canonical mutation, initially with retirement
`Active`.

The ordered Graph endpoint has one mandatory journal-first entry sequence:

1. Decode with structural bounds, authorize the current Router caller, verify
   `target_graph_canister` equals the receiving principal, recompute the Graph
   request fingerprint from the received immutable envelope, and load the
   journal by `mutation_id`.
2. If a journal entry exists, compare request kind, encoding version,
   fingerprint, and item count before consulting any mutable catalog, schema,
   shard-routing metadata, or capability state. A matching completed,
   `Active` entry returns its validated durable receipt. A matching `Retired`
   entry returns the typed `MutationRetired` result and is never executed again;
   journal reads project it as retirement `Retired`.
   A conflicting identity or request kind fails closed. An ordered entry with
   any non-completed or continuation state fails closed as durable corruption;
   it is never treated as fresh or passed to PlanExecution recovery.
3. Only when the lookup returns `Absent`, validate current shard ownership,
   resolved-table completeness, schema/value rules, capability-independent
   owner admission, cost, planning, allocation, and writes.

Reusing a mutation id across the `PlanExecution` and `OrderedEdgeBatch`
identity variants is rejected before writes in either call order. Existing
plan endpoints likewise reject an ordered-batch journal entry rather than
interpreting it as their own receipt. Journal-first replay does not waive
current caller authorization or
permit a request addressed to another canister; it only prevents mutable
post-commit metadata from invalidating an already durable exact result.

After projection convergence, Router calls the Graph-owned
`retire_ordered_mutation(mutation_id, graph_request_fingerprint)` endpoint on
the exact stored target. The endpoint authenticates the current Router caller,
verifies the receiving Graph owner, loads the journal by `mutation_id`, and
accepts only an exact completed
`GraphMutationRequestIdentityV1::OrderedEdgeBatch` identity. It atomically
changes retirement from `Active` to `Retired { at_ns }` once and returns
the independent wire envelope:

```text
OrderedMutationRetirementAck =
    V1(OrderedMutationRetirementAckV1)

OrderedMutationRetirementAckV1 {
    mutation_id,
    graph_request_fingerprint,
    receipt: GraphOrderedEdgeBatchReceiptV1,
}
```

The timestamp remains Graph-internal. Repeating the same call is
idempotent and returns the same logical acknowledgement; a different
fingerprint or request kind fails closed without changing the entry.
This is an internal Router-to-Graph capability, not a public or administrator
endpoint. Router is the lifecycle owner and may issue the irreversible
retirement promise only from a durably persisted `RetirementPending` reached
after `ProjectionAdvanced`; Graph owns enforcement of caller authorization,
exact journal identity, and the retention transition.

ADR 0027's ordinary age-only retention remains unchanged for
`GraphMutationRequestIdentityV1::PlanExecution`. For
`GraphMutationRequestIdentityV1::OrderedEdgeBatch`, however, `recorded_at_ns`
alone never makes an entry evictable. An ordered entry is eligible for the existing
bounded amortized GC only in `Retired { at_ns }` after that timestamp is older
than `GRAPH_MUTATION_JOURNAL_RETENTION_NS`. The owner computes eligibility with
checked timestamp addition and fails closed on overflow or a future timestamp;
it does not saturate elapsed time into eligibility. An `Active` ordered entry is
retained regardless of age. There is no operator or count-based bypass for this
predicate.

Implementation replaces `GraphMutationJournalEntryV1`,
`GraphMutationJournalEntryWireV1`, and the current V1 stable codec definition in
place under the existing `GraphMutationJournalEntry::V1` and
`GraphMutationJournalEntryWire::V1` variants. It does not add another version
variant, an old-layout decoder, or an in-place migration. The stable store is
initialized fresh when the replacement lands. The replacement V1 definition
specifies the fixed Graph fingerprint algorithm and domain separator, bounds
for all identity fields, and fail-closed encode/decode behavior. The internal
layout marker remains V1 because it describes the only supported fresh layout
rather than compatibility with the discarded one.

Router replaces its V1 durable mutation payload in place while retaining all
currently supported scalar/legacy/typed variants. Its exhaustive payload shape
adds active and compacted ordered variants:

```text
RouterMutationRequestIdentityV1 =
    PlanExecution {
        request_fingerprint: [byte],
    }
  | OrderedEdgeBatch {
        public_fingerprint: PublicBatchFingerprintV1,
        public_item_count: u32,
    }

RouterMutationLastErrorV1 =
    PlanExecution {
        detail: RouterDiagnosticDetailV1,
    }
  | OrderedEdgeBatch {
        code: OrderedEdgeRetryDiagnosticCodeV1,
        item_ordinal: Option<u16>,
        detail: Option<RouterDiagnosticDetailV1>,
    }

OrderedEdgeRetryDiagnosticCodeV1 =
    RegistryUnavailable
  | OwnerLookupUnavailable
  | ShardBindingUnavailableOrStale
  | OrderedCapabilityUnavailable
  | LeaseOwnerTrap
  | ActiveTargetUnavailable
  | ActiveTargetCapabilityChanged
  | JournalReconciliationPending

RouterMutationTerminalFailureV1 =
    PlanExecution {
        detail: RouterDiagnosticDetailV1,
    }
  | OrderedEdgeBatch {
        code: OrderedEdgeTerminalFailureCodeV1,
        item_ordinal: Option<u16>,
        subject_name: Option<BoundedCatalogNameV1>,
    }

OrderedEdgeTerminalFailureCodeV1 =
    MalformedEndpoint
  | MalformedValue
  | MissingLogicalGraph
  | MissingEdgeLabel
  | MissingProperty
  | CatalogConflict
  | DuplicatePropertyAssignment
  | MixedTargetShards
  | CrossShardEdge
  | ConstrainedProperty
  | InlineSchemaOrTypeMismatch
  | GraphRequestOrRecordCostBoundExceeded

RouterDiagnosticDetailV1 = bounded UTF-8 bytes, maximum 1_024
BoundedCatalogNameV1 = bounded UTF-8 bytes, maximum 256

RouterMutationRecord =
    V1(RouterMutationRecordV1)

RouterMutationRecordV1 {
    mutation_id,
    created_at_ns,
    request_identity: RouterMutationRequestIdentityV1,
    resolved_labels,
    resolved_properties,
    completed_row_count,
    routing_in_progress,
    payload: RouterMutationPayloadV1,
    routing_lease_ns,
    last_error: Option<RouterMutationLastErrorV1>,
    terminal_failure: Option<RouterMutationTerminalFailureV1>,
}

RouterMutationPayloadV1 =
    Scalar {
        shards: [RouterMutationShardV1],
    }
  | LegacyBulk {
        total_ops,
        shards: [RouterMutationShardV1],
    }
  | TypedSeedBulk(TypedSeedBulkReplayV1)
  | OrderedEdgeBatchRouting
  | OrderedEdgeBatch(RouterOrderedEdgeBatchReplayV1)
  | CompletedBulk {
        total_ops,
        operation_row_counts,
    }
  | CompletedOrderedEdgeBatch {
        receipt: GraphOrderedEdgeBatchReceiptV1,
        projection_watermark: MutationTokenShard,
    }

RouterOrderedEdgeBatchReplayV1 {
    target: RouterOrderedEdgeBatchTargetV1,
}

RouterOrderedEdgeBatchTargetV1 {
    graph_request_fingerprint: GraphRequestFingerprintV1,
    request: OrderedEdgeBatchGraphRequestV1,
    progress: OrderedEdgeBatchTargetProgressV1,
}

OrderedEdgeBatchTargetProgressV1 =
    CanonicalPending
  | CanonicalCommitted(GraphOrderedEdgeBatchReceiptV1)
  | ProjectionPending(GraphOrderedEdgeBatchReceiptV1)
  | ProjectionAdvanced(GraphOrderedEdgeBatchReceiptV1)
  | RetirementPending(GraphOrderedEdgeBatchReceiptV1)

GraphOrderedEdgeBatchResult =
    V1(GraphOrderedEdgeBatchResultV1)

GraphOrderedEdgeBatchResultV1 =
    Completed(GraphOrderedEdgeBatchReceiptV1)
  | MutationRetired {
        mutation_id,
        graph_request_fingerprint: GraphRequestFingerprintV1,
    }

GraphOrderedEdgeBatchReceiptV1 {
    logical_edge_count: u64,
    emitted_delta_first_seq: Option<ShardEventSeq>,
    emitted_delta_last_seq: Option<ShardEventSeq>,
    hot_forward_vertices:
        BoundedVec<LocalVertexId, MAX_ORDERED_EDGE_HOT_FORWARD_VERTICES>,
}
```

The Graph canonical endpoint returns
`GraphOrderedEdgeBatchResult::V1(GraphOrderedEdgeBatchResultV1)`. A completed
entry returns `Completed(GraphOrderedEdgeBatchReceiptV1)`; a retired entry
returns `MutationRetired` with the exact mutation id and Graph fingerprint.
Router decodes this single independent result envelope, validates the identity,
and stores only the inner `GraphOrderedEdgeBatchReceiptV1` inside
`RouterMutationRecordV1`; the parent record version already owns that nested
stable schema. The retirement acknowledgement remains a separate
`OrderedMutationRetirementAck::V1` envelope because it is a distinct
Router-to-Graph capability response.

`MAX_ORDERED_EDGE_HOT_FORWARD_VERTICES = 2_048` is owned with the receipt wire
in `gleaph-graph-kernel` and equals the maximum physical projection count of one
admitted request. Graph sorts the list by `LocalVertexId`, removes duplicates,
and rejects a non-canonical or oversized list before committing the ordered
journal/receipt. The ordered Graph journal constructor and wire encoder, Graph
response encoder, Router response decoder, Router progress constructor, and
stable-record decoder all enforce the same constant and canonical order.
Existing `GraphMutationRequestIdentityV1::PlanExecution` journal entries retain
their current independent 4,096 hot-vertex codec bound.

The replacement V1 removes the current unbounded diagnostic strings. The
Router facade owns the two bounded diagnostic types and their constructors;
callers cannot construct them from arbitrary upstream rejection text.
Plan-execution variants retain their existing user-visible diagnostic text
inside the bounded detail wrapper. Ordered admission uses an exhaustive terminal
code, while routing/recovery uses a disjoint retry-diagnostic code, so a
transient capability or binding error cannot be stored in
`terminal_failure`. External reject text is normalized to an owner-defined code
and an optional sanitized bounded excerpt; if an excerpt is shortened, its
rendered diagnostic explicitly records that fact rather than pretending to
preserve the full upstream string. Ordered terminal errors are reconstructed
exactly from their code and bounded structured fields.

`MAX_ROUTER_MUTATION_RECORD_BYTES` equals
`MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES` for this fresh V1 layout. Every
Router payload variant—routing, active, terminal, retry-diagnostic, and
completed—is encoded and checked against that logical bound in its owning
constructor, decoder, and before every stable-map replacement.
`RouterMutationRecord::Storable::BOUND` nevertheless remains
`Bound::Unbounded`: this is the stable-structures physical allocation strategy,
not permission for an unbounded logical value. Setting a 2 MiB variable-size
`Bounded` maximum can make B-tree node allocation scale with that maximum, so
ADR 0049 does not overturn the measured unbounded-Storable strategy merely to
encode a logical admission rule. The small fixed terminal representation makes
the atomic lease-clear plus terminal-write transition non-failing after
deterministic classification; implementations may not format the full request
or an unbounded upstream error into durable state.

A record is not admitted merely because its initial encoding fits. Before an
ordered active envelope is persisted, Router computes the maximum encoded size
of every reachable non-compacted shape using that exact request: each target
progress variant with a receipt containing exactly
`MAX_ORDERED_EDGE_HOT_FORWARD_VERTICES` canonical ids, both optional sequence
fields present, and the largest permitted active retry diagnostic. The maximum
must fit
`MAX_ROUTER_MUTATION_RECORD_BYTES`; otherwise admission fails terminally before
dispatch. Routing-terminal and completed shapes are checked independently and
are smaller by construction. Fresh-layout constructors for retained
plan-execution variants likewise reserve their maximum permitted diagnostic and
progress overhead. The maximum includes `RetirementPending` with its full
receipt. Consequently, a callback after canonical Graph success
cannot discover that adding its receipt or diagnostic would overflow the
Router record. Per-transition checks remain corruption guards, not recoverable
post-commit admission points.

For this request kind, one public item is one affected logical edge regardless
of whether LARA writes one or two physical projections and regardless of the
number of initial property sidecars or derived events. The following equality
is mandatory:

```text
receipt.logical_edge_count
    == Graph request.items.len
    == Graph journal request_identity.logical_item_count
    == Router request_identity.public_item_count
```

The shared Graph journal retains its existing generic `row_count` field for
`GraphMutationRequestIdentityV1::PlanExecution`; for
`GraphMutationRequestIdentityV1::OrderedEdgeBatch`, that field must equal the
same logical edge count. Graph validates this equality before first committing
the journal entry and whenever returning a replayed receipt. Router validates the
receipt against the stored typed request and public identity before accepting
any callback or advancing target progress. A callback mismatch is rejected and
reconciled from the Graph journal; a journal whose stored count disagrees with
its ordered request identity is durable corruption and must fail closed without
advancing or compacting the Router saga. Physical projection, sidecar, and
derived-event counts never enter `completed_row_count`.

`OrderedEdgeBatchRouting` represents the pre-envelope resolution state and is
valid only while `routing_in_progress` is true or after that no-dispatch lease
is safely released for retry. The transition that persists
`RouterOrderedEdgeBatchReplayV1` also clears `routing_in_progress`, its lease,
and any pre-envelope retry diagnostic in the same Router message, before the
first Graph dispatch. Later active recovery may record a new active-target
diagnostic from the ordered retry-code set.

Pre-dispatch failure has two exhaustive outcomes:

- A transient routing/deployment dependency failure, lease-owner trap, or
  explicitly retryable availability failure clears or expires only the routing
  lease under the no-dispatch proof. It retains `OrderedEdgeBatchRouting`,
  leaves `terminal_failure = None`, records only a retry diagnostic, and allows
  the same public request and client key to resolve again.
- A deterministic post-reservation admission failure—malformed endpoint/value
  semantics, missing or conflicting logical graph/label/property catalog data,
  mixed target shards, cross-shard edge, constrained property, inline
  schema/type mismatch, or encoded Graph-request/active-record cost bound
  violation—atomically clears the routing lease and stores `terminal_failure`
  while retaining the public request identity. The same client key thereafter
  returns that exact stored failure and can never acquire a new routing lease or
  dispatch, even if catalogs or routing later change. A caller must use a new
  client key for a new attempt.

The owner and outcome of every registry/capability gate are fixed as follows:

| Pre-envelope condition                                                                                                                                             | Classification and same-key behavior                                                                                                      |
| ------------------------------------------------------------------------------------------------------------------------------------------------------------------ | ----------------------------------------------------------------------------------------------------------------------------------------- |
| Logical graph, label, or property name is absent or conflicts with its catalog; endpoint/value semantics are invalid                                               | Deterministic admission failure; store the exact `terminal_failure`                                                                       |
| Registry or owning-canister lookup rejects, traps, times out, or is otherwise unavailable                                                                          | Transient dependency failure; clear/expire the lease, retain routing state, and allow same-key retry                                      |
| The logical graph exists but its shard-owner binding is absent, stale, changes during resolution, or no longer matches the resolved target                         | Transient routing failure; do not freeze or dispatch the candidate envelope, and allow same-key resolution against one later live binding |
| The resolved target does not advertise the exact ordered-batch capability, canonical encoding version, or fresh V1 layout required by this ADR                     | Transient deployment incompatibility; retain no active envelope and allow same-key retry after a compatible release set is installed      |
| The request deterministically resolves to more than one target, contains a cross-shard edge, violates schema/constraint policy, or exceeds a post-resolution bound | Deterministic admission failure; store the exact `terminal_failure`                                                                       |

Capability absence is not converted into a request-specific terminal result
because deployment state can change without changing public request identity.
Conversely, a missing logical catalog name is terminal for that client key even
if an administrator could later create it; this preserves exact stored-error
semantics for the request as admitted.

Only these no-envelope/no-dispatch states may set `terminal_failure`. Once the
active replay envelope is persisted, no admission result may convert the saga
to `Failed`; it remains exact-retry roll-forward state. A later registry move,
capability withdrawal, or target call rejection cannot reroute that active
request. Recovery retries the same stored target and fingerprint after the
compatible owner is restored, or reports the saga as pending operator repair.

The stored typed request is then the sole replay envelope. On retry Router
resends that envelope; it does not re-resolve labels or reconstruct the input
sequence from current routing state. Once the active ordered payload is
persisted, top-level `resolved_labels` and `resolved_properties` are cleared
because the typed request owns the resolved identifiers. Those top-level fields
remain available to the existing plan-execution variants. The replacement also
preserves the current Router record's mutation id, creation time, routing lease,
retry diagnostic, terminal-failure proof, and top-level
`completed_row_count`. `RouterOrderedEdgeBatchTargetV1::progress` is the sole
ordered lifecycle source; parallel booleans are forbidden.

`RouterMutationRequestIdentityV1` replaces the untyped top-level
`request_fingerprint` field in the fresh V1 layout. Its `PlanExecution` variant
preserves current scalar/legacy/typed fingerprint semantics, while
`OrderedEdgeBatch` is the sole owner of the public ordered fingerprint and
public item count.

The owning constructor and decoder accept only these identity/payload pairs:
`RouterMutationRequestIdentityV1::PlanExecution` with the four existing
plan-execution payload variants, and
`RouterMutationRequestIdentityV1::OrderedEdgeBatch` with routing, active, or
completed ordered payload.
Cross-kind pairs, a routing payload after any possible Graph dispatch, an active
payload with routing state/tables still present, a missing or additional target,
request/registry identity mismatch, or an
outer target request fingerprint whose value does not match the typed request
and Graph journal identity fail closed. Any present diagnostic or terminal
failure must have the same `PlanExecution`/`OrderedEdgeBatch` request kind as
the record identity. Ordered terminal codes are accepted only in no-envelope
routing state; ordered retry-diagnostic codes are never accepted as terminal
failures.

For `OrderedEdgeBatchRouting`, the decoder additionally accepts only
`routing_in_progress = true` with a live/expired lease and no terminal failure,
or `routing_in_progress = false` with no lease and either a retryable
no-dispatch diagnostic or `terminal_failure`. An active or completed ordered
payload with `terminal_failure`, a routing lease, or `routing_in_progress` is
invalid.

The ordered payload maps to the existing ADR 0029 lifecycle as follows:

- `OrderedEdgeBatchRouting` with an active lease means `Routing`; after safe
  lease release with no envelope, no dispatch, and no terminal failure it is
  retryable `Failed`; with `terminal_failure` it is irreversible `Failed`;
- target `CanonicalPending` means `CanonicalPending`;
- target `CanonicalCommitted` means `CanonicalCommitted`;
- target `ProjectionPending` means `ProjectionPending`;
- target `ProjectionAdvanced` means all required projections reached their
  watermarks but Graph replay protection has not yet been retired;
- before the retirement call Router persists `RetirementPending`, retaining the
  validated receipt and exact target/fingerprint; a callback or timeout may
  repeat only the idempotent retirement call;
- an exact retirement acknowledgement atomically writes the aggregate
  `completed_row_count` from the validated `logical_edge_count`, compacts the
  active payload to `CompletedOrderedEdgeBatch`, clears `last_error`, and reports
  `Completed`; and
- `Failed` is permitted only before the Graph target can have committed.

After recording the canonical receipt, Router persists `ProjectionPending`
before the first projection/index advancement `await`. Retrying that state may
repeat only the existing idempotent projection work; it never redispatches the
canonical Graph request in the background.

After persisting `ProjectionAdvanced`, Router atomically advances to
`RetirementPending` before the retirement `await`. From that point neither
explicit retry nor background recovery may query or invoke the canonical
execution endpoint. They call only `retire_ordered_mutation` on the exact stored
target. If the retirement callback is lost, an exact repeat returns the stored
retirement result while the retired entry remains retained. If the journal has
already been removed by post-retirement GC, `Absent` leaves the Router in
`RetirementPending` for operator repair. Absence alone is never accepted as a
retirement acknowledgement: it could also reflect a separately reset or
corrupted Graph owner. No outcome from `RetirementPending` authorizes canonical
query or redispatch.

The compacted ordered record retains the public item count and fingerprint in
`RouterMutationRequestIdentityV1`, the full validated
`GraphOrderedEdgeBatchReceiptV1` in the completed payload, the exact target
projection watermark, and mutation id plus the validated logical edge count in
the top-level `completed_row_count`. The retained receipt is the source for an
exact same-key replay; the watermark and count reconstruct the `MutationToken`
without retaining the discarded Graph request envelope. The Graph request
envelope is discarded only after the canonical and required projection
outcomes are durable and the Graph retirement transition is acknowledged.

Every remote side effect is fenced by its durable pre-`await` state, and every
successful callback advances the corresponding state atomically. If a Graph
call returns success but the Router callback traps before persisting
`CanonicalCommitted`, the target remains `CanonicalPending`; explicit same-key
retry first queries the Graph journal, verifies its
`GraphMutationRequestIdentityV1::OrderedEdgeBatch` identity variant, and either
records the durable receipt or resends the exact
stored request. An ambiguous call outcome never reroutes, re-resolves, or changes
request kind. Background recovery remains canonical-dispatch-free as in
ADR 0029. For ordered `CanonicalPending` it may query only the exact stored
Graph journal identity: a matching completed entry advances to
`CanonicalCommitted`, while `Absent` records a retry diagnostic and performs no
write. A retirement `Retired` result in `CanonicalPending` is an impossible
lifecycle combination and remains pending operator repair; it is not accepted
as a canonical receipt. Recovery may then advance idempotent projection and
retirement work.
Canonical redispatch requires explicit same-key retry and is forbidden after
`RetirementPending`. A deterministic Graph
rejection of an already persisted valid envelope is retained as a diagnostic
while the single-target saga stays `CanonicalPending`: an ambiguous call may
have committed on that same target, so Router must reconcile its journal rather
than change target or request kind.

There is no production unordered batch wire to migrate. Internal ADR 0045 names
may be renamed incrementally after ADR 0048 completes and ADR 0049
implementation begins. Renaming alone must not obscure which geometry is
implemented versus planned.

### Compatibility-free release-set activation

Replacing the Router request V1, Graph request V1, and Graph journal/wire V1 is
a full-stack pre-production cutover, not a rolling upgrade:

1. stop public ingress, timers, continuations, maintenance dispatch, and other
   mutation producers;
2. reset Router and every Graph shard rather than decoding old durable values;
3. reset or reseed Property Index, Vector Index, Graph Index, and any other
   derived store whose state would otherwise refer to discarded canonical Graph
   data;
4. install one mutually compatible Router/Graph/affected-index/SDK release set,
   then rebootstrap routing, schema, authorization, and seed state required by
   the fresh deployment;
5. verify every required owner's declared ordered-batch capability and fresh V1
   layout; and
6. enable producers only after every required owner passes that gate.

Mixed old/new Router and Graph binaries are unsupported. There is no
data-preserving rollback; rollback installs another mutually compatible release
set after repeating the fresh reset. This activation procedure is allowed only
while the system has accepted no persistent production data.

When ADR 0049 is activated:

| ADR 0045 area                                  | ADR 0049 treatment                                                                                     |
| ---------------------------------------------- | ------------------------------------------------------------------------------------------------------ |
| logical ordinals and physical intent expansion | retain and strengthen                                                                                  |
| full-leaf occupancy and capacity projection    | retain                                                                                                 |
| slab/log/expansion/relocation planning         | retain                                                                                                 |
| reserve/commit/rollback atomicity              | retain                                                                                                 |
| exact physical-location capture                | retain as conditional internal mode; mandatory when any initial edge property is present               |
| size-bounded Graph request and durable receipt | replace with single-request V1 admission and the replacement Graph journal V1 order-sensitive identity |
| unordered public semantics                     | supersede                                                                                              |
| independent projection reordering freedom      | supersede                                                                                              |
| scalar-only ordered mutation assumption        | supersede                                                                                              |
| unordered duplicate/update policy              | replace with ordered edge-insert parallel policy; non-edge public operations are absent in v1          |

ADR 0045 remains the decision history for the physical batch substrate. It is
not rewritten as though unfinished unordered product behavior shipped.

## Implementation sequence

1. Complete ADR 0048 and its adoption/alias-removal gate.
2. Record the final ADR 0048 owner APIs and implemented state in ADR 0049 before
   code changes.
3. Define the unresolved public item, bounded canonical value wrapper, distinct
   resolved Graph item/request, canonical encoders, unchanged single-shard
   array-order proof, and pre-payload constrained-property rejection without
   exposing a public endpoint. Give each independently encoded public/Graph
   request its outer version envelope; keep item and property types directly
   nested in that envelope.
4. Replace the Graph journal/wire V1 structures and V1 codec in place with the
   exhaustive request-kind identity, preserved current fields, codec bounds,
   journal-first exact-replay gate, Ordered-completed-only field combinations,
   stable-only ordered retirement marker, fingerprint-bound idempotent
   retirement endpoint, request-kind-aware GC predicate, and
   same-id/different-request-kind/order rejection. Keep the existing outer
   journal and wire version envelopes and encode their request identity
   directly as nested V1 schema.
5. **Partially implemented (2026-07-28):** replace role-split bidirectional
   plans with a merged physical run once per orientation/bucket, pass a
   Graph-created logical pair table, admit mixed directed/undirected/self-loop
   shapes through one owner reservation, and add same-bucket/pair-table
   rejection tests. Unsupported geometry and the public ordered contract
   remain planned.
6. Add bucket-local order and pair-ordinal adversarial tests to the existing
   ADR 0045 planner/write fixtures without changing the public wire.
7. **Partially implemented (2026-07-28):** reclassify the Graph facade's
   optimized placement comments so they no longer imply an unordered semantic
   contract. The remaining ADR 0045 historical names stay in the decision
   record; no public unordered endpoint is introduced.
8. **Partially implemented (2026-07-28):** extend placement reservations with
   explicit append/live-order validation for clean slab, overflow-log append,
   expanded-slab growth, folded logs, and tombstone-heavy cases. Same-leaf
   multi-bucket expanded-slab coverage checks each bucket independently, and
   expanded folding after a slab tombstone checks that the tombstone remains
   invisible while folded live rows and pending rows retain order. Overflow-log
   tombstone append now checks the same live-tail rule and edge/inline-property
   alignment. A batch append after an already completed scalar relocation also
   preserves the live tail. Edge-only non-tail relocation is now admitted once
   per leaf during batch commit using the storage-owned mutation-free target
   planner; same-leaf multi-bucket coverage checks both folded sequences
   independently. Inline-property-bearing relocation now copies a non-tail
   value span during reservation, folds edge/inline-property logs, and verifies
   replacement-offset and pending-value read-back after commit. A dedicated
   same-leaf multi-bucket fixture also verifies that each bucket's edge and
   inline-property log folds independently while the leaf relocates once.
9. **Partially implemented (2026-07-28 12:58:54 UTC +0000):** LARA's
   one-orientation reservation now prepares all required forward/reverse named
   buckets before clean-slab preflight, so planner-admitted new named buckets
   can use the ordered batch writer while preserving the same label delta,
   hot-forward receipt, and ordered journal contract. Planner-admitted
   reservation failures still use the whole-request scalar fallback; geometry
   outside both proven paths remains pre-write rejected.
10. **Partially implemented (2026-07-28 11:20:24 UTC +0000):** the Graph facade now accepts
    request-local initial sidecar values, validates ids/values/duplicate ids
    before reservation, derives one canonical `CanonicalEdgeOccurrence` from
    the joined captured location for directed, undirected, and self-loop
    shapes, and writes the canonical sidecar plus its derived property event
    after LARA commit. Inline-property/schema conflict rejection is now
    included in the preflight, and the Graph facade co-writes the complete
    preflighted sidecar set before dispatching its derived events. Explicit
    stable-memory sidecar reservation, durable net-event co-write, and parallel
    inserts remain planned. Directed, undirected owner/alias, and self-loop
    canonical-owner tests now cover the location/property boundary, including
    a mixed multi-item batch whose properties remain joined by logical ordinal.
    A property-bearing batch following an existing equal-neighbor edge also
    verifies that CounterpartScan resolves the new reverse row by pair rank,
    not by matching the forward slot; the same coverage now exists for an
    undirected higher-owner/lower-owner pair. A delete-then-batch-insert
    regression test also verifies that CounterpartScan ranks only the
    surviving live parallel rows after deletion. A batch-created canonical
    sidecar is also verified across forward compaction: the moved physical
    handle reads the value and the pre-compaction handle no longer does.
    Reverse compaction is likewise covered: the reverse row resolves back to
    the batch canonical owner while its forward sidecar remains readable.
    Undirected lower-owner alias compaction is also covered. Its underlying
    singleton selector now falls through when slot zero is a tombstone, so a
    live row at a later logical slot remains pair-rank addressable.
    An overflow-log batch append with an initial sidecar also verifies that
    the captured bucket logical slot, rather than the raw log location, owns
    the canonical sidecar.
    Parallel logical targets are now admitted by the internal planner and
    covered by same-batch directed and undirected ordinal/pair-rank tests,
    including distinct per-item sidecar values. Public exact replay remains
    planned because the current journal cannot identify or return this batch
    payload durably.
    Graph-kernel replay contract types now define the ordered-batch request
    identity, stable/wire retirement state, bounded aggregate receipt, and
    canonical hot-vertex validation. The stable and wire journal records now
    carry those identity/retirement fields with round-trip coverage. The
    shared Graph-kernel validator now rejects ordered continuation/partial
    states, row-count mismatches, and retirement on PlanExecution entries at
    the stable codec and wire-accessor boundaries. GraphStore now has an
    ordered completed-entry commit boundary and a journal-first lookup that
    returns `Absent` only when no entry exists, accepts only an exact identity,
    and preserves the existing entry on conflicts. The public endpoint,
    current-caller authorization for the canister guard and the ordered
    retirement update are now wired. The immutable
    `OrderedEdgeBatchGraphRequest::V1` envelope now carries the resolved
    Graph-owned item sequence, target identity, inline bytes, and initial
    sidecars with structural validation and Candid round-trip coverage. The
    full fresh ordered mutation commit path for unsupported geometry and Router
    caller integration remain planned. The Graph canister now exposes a guarded
    journal-first
    ordered execution handler: it verifies the target envelope fingerprint,
    returns an exact durable replay, and on a journal miss decodes immutable
    items into the existing `BatchEdgeInput` form for read-only planner
    admission. LARA prepares all required named forward/reverse buckets before
    clean-slab reservation, so new named buckets can proceed through the Graph-owned
    batch writer, appends the label-stats delta, computes the bounded hot-forward
    receipt fields, and commits the ordered journal in the same no-`await` update
    section. Planner-admitted clean-slab reservation failures still proceed through
    the existing scalar owner boundary in input order and publish the same
    receipt/delta/journal contract; geometry outside the two proven paths still
    fails closed before canonical write rather than entering an unordered executor.
    The Graph-kernel
    now owns the manual V1 Graph request encoder,
    the `gleaph:ordered-edge-graph:v1` SHA-256 domain separator, and the
    order-sensitive fingerprint helper; reorder and payload-change tests
    prove distinct fingerprints while repeated encoding remains stable.
    Graph-owned `GraphOrderedEdgeBatchResult::V1` and
    `OrderedMutationRetirementAck::V1` envelopes now project active/retired
    entries; exact retirement is idempotent and fingerprint-conflict-safe.
    Ordered active entries are retained regardless of age, while retired
    entries use checked `at_ns + retention` eligibility for GC.
    The post-commit sidecar path traps on a violated preflight invariant rather
    than returning a recoverable error. The explicit stable-memory reservation
    remains blocked by the current `EdgePropertyStore` boundary: its
    `StableBTreeMap` exposes per-key insertion but no reservation token or
    capacity preallocation, and placeholder records would violate the durable
    property schema. The implementation therefore retains full preflight and
    invariant-trap behavior until the property store gains a real reservation
    mechanism.
11. **Partially implemented (2026-07-28 13:39:36 UTC +0000):** Router's fresh V1
    mutation record now has typed `PlanExecution`/`OrderedEdgeBatch` request
    identity, ordered routing/replay payload states, Graph request fingerprint
    validation, and idempotent `CanonicalPending` → `CanonicalCommitted` →
    `ProjectionPending` → `ProjectionAdvanced` transitions with a durable
    projection watermark. Public ordered admission, retry diagnostics and
    terminal failures, Graph retirement, compaction, and background recovery
    remain planned. **Partially implemented (2026-07-28 13:43:51 UTC +0000):**
    the Router record now persists `RetirementPending` and accepts an exact
    Graph retirement receipt to reach `CompletedOrderedEdgeBatch`, retaining
    the projection watermark and aggregate row count while dropping resolved
    tables. The public retirement driver, background recovery, and final
    compaction policy remain planned.
    **Partially implemented (2026-07-28 13:49:42 UTC +0000):** the wasm Router
    recovery driver now recognizes ordered replay records, refuses to
    redispatch `CanonicalPending` records, and can converge committed ordered
    records through projection, fingerprint-bound Graph retirement, and the
    durable completed state. Public admission retry and independent journal
    reconciliation remain planned.
    Replace Router V1 request identity/payload in place; implement exhaustive
    ordered routing/envelope/target progress, resolved-table authority transfer,
    bounded typed retry diagnostics and terminal failures, a bounded record in
    every lifecycle state while retaining the physical unbounded-Storable
    allocation strategy, retryable no-dispatch release, deterministic terminal
    admission failure, explicit-retry canonical recovery,
    background journal-reconciliation/projection/retirement recovery,
    retirement-before-compaction, completed receipt compaction, and exact replay
    tests. Keep `RouterMutationRecord::V1` as the sole durable version envelope;
    do not add nested version enums around its identity, payload, progress,
    diagnostic, failure, or stored receipt types.
12. **Partially implemented (2026-07-28 13:55:41 UTC +0000):** Router now owns
    the versioned `OrderedEdgeBatchPublicRequest::V1` wire with bounded item,
    endpoint, inline-property, property-value, and encoded-request validation,
    plus Candid round-trip and invalid-property tests. Single-request admission,
    logical-graph/shard/catalog resolution, independently versioned Graph result
    and retirement acknowledgement envelopes, aggregate receipt, SDK packing,
    and PocketIC coverage remain planned. Do not expose non-edge or unordered
    specialized batch operations.
    **Partially implemented (2026-07-28 14:00:07 UTC +0000):** the public V1
    wire now carries a bounded client mutation key and exposes a Router-owned,
    order-sensitive public fingerprint that excludes that key. Admission,
    resolution, and Graph dispatch remain planned.
    **Partially implemented (2026-07-28 14:03:18 UTC +0000):** Router's
    read-only wire admission helper now decodes endpoint bytes with the
    graph-specific encoding key and rejects both cross-endpoint-shard and
    cross-batch-shard geometry before catalog resolution or reservation.
13. Run ordered-versus-scalar canbench gates and Router replay-record
    insert/replace benchmarks at small and maximum reachable shapes, including
    the current ADR 0045 physical batch substrate as the replacement baseline.
    Compare an unordered candidate separately only if a real reordering
    optimization exists.
14. Remove the unordered endpoint/path unless the evidence gate requires an
    explicit ADR revision.
15. Exercise the compatibility-free release-set activation against fresh
    Router, Graph, and derived-index state; reject mixed-version activation.
16. Run unfiltered `canbench --persist` in every affected crate before updating
    final benchmark artifacts and activation status.

## Test contract

At minimum, implementation must cover:

- directed fan-out, fan-in, and mixed source/target input where forward and
  reverse buckets receive different stable ordinal subsequences;
- ascending and descending scan order before and after the batch;
- directed self-loops returning distinct `DirectedForward` and
  `DirectedReverse` locations, undirected non-self edges returning distinct
  lower-owner and higher-owner locations, and undirected self-loops returning
  exactly one self-loop location;
- one undirected forward bucket receiving higher-endpoint and lower-endpoint
  projections from different logical edges, including `[3 -- 2, 2 -- 1]`, with
  one reservation and scan order `[0, 1]`;
- mixed directed, undirected, and self-loop items in one public batch;
- parallel edges with distinct inline properties/properties and exact counterpart rank;
- pre-existing live edges followed by pending edges;
- slab, overflow-log, expanded-slab, folded-log, and relocated destinations;
- edge/inline-property-bytes sequences stored in different physical domains;
- interior tombstones, tail slack, deletion, compaction, and reopen;
- counterpart ScanOnly and Published/fallback behavior after invalidation/rebuild;
- property-free aggregate mode without location counterpartrialization;
- a property-bearing batch forcing whole-batch internal location capture,
  an exact `0..logical_item_count` canonical-handle key set including items
  without properties, canonical-owner sidecar writes, durable net derived
  events, and aggregate-only public output for directed, undirected, self-loop,
  and parallel edges;
- reserve failure in a later orientation with complete rollback before any
  canonical write;
- replacement Graph journal/wire V1 round-trip preserving current scalar,
  legacy/typed bulk continuation, delta, hot-vertex, retention, and completion
  fields, plus stable ordered retirement round-trip, its derived wire retirement
  enum, `Retired.at_ns` absence from the wire, and mandatory
  `GraphMutationRequestIdentityV1::PlanExecution` `NotApplicable` state;
- ordered Graph journal/wire constructor, codec, and accessor rejection of
  `Incomplete`, non-`None` `next_index`, or non-`None` `bulk_progress`, while
  the same fields remain valid for the applicable PlanExecution variants;
- journal-first exact replay returning the stored receipt after current
  catalog/schema/routing metadata/capability changes, while current caller
  authorization remains required and a conflicting fingerprint or request kind
  still fails before mutable owner validation;
- bounded public and Graph fingerprint encodings, exact single-target replay,
  ordered exact retry, same mutation id with reordered content, and
  PlanExecution/OrderedEdgeBatch collisions in both call orders rejected before
  planner/allocation/write;
- normative public-value codec vectors passing in Rust and the JavaScript SDK,
  non-canonical SDK-supplied value bytes rejected by Router, and no SDK API for
  supplying either request fingerprint;
- public unresolved item round-trip containing only encoded endpoint ids,
  label/property names, and canonical value bytes; distinct resolved Graph
  envelope round-trip containing local endpoint ids, resolved tables, catalog
  ids, fixed-width inline bytes, and resolved property ids;
- outer version-envelope round-trips for the public request, Graph request,
  Graph result, retirement acknowledgement, Router stable record, and Graph
  stable/wire journal, plus fail-closed rejection of unsupported version tags;
  canonical byte fixtures must prove the outer version tag is present exactly
  once and that nested identity/payload/receipt bytes contain no second version
  tag;
  nested identities, payloads, progress, diagnostics, failures, items,
  properties, physical intents, and receipt payloads must round-trip only
  through their owning V1 envelope and must not introduce a second version axis;
- canonical value byte bounds and decoding, inline schema/type conversion,
  duplicate property-name/id rejection, and missing/duplicate/conflicting or
  unused resolved-table entry rejection;
- property-list permutation canonicalizing to the same fingerprint while any
  edge-item permutation changes both the public and Graph fingerprints;
- replacement `RouterMutationRecord::V1` round-trip proving its nested
  request-identity/payload types are encoded directly without independent
  version envelopes, for current scalar/legacy/typed variants and the ordered
  routing/`CanonicalPending`/`CanonicalCommitted`/`ProjectionPending`/
  `ProjectionAdvanced`/`RetirementPending` states;
- completed ordered records must retain the full validated receipt needed for
  exact same-key replay after payload compaction, including both delta-sequence
  bounds and the bounded hot-forward vertex list;
- bounded Router retry-diagnostic and terminal-failure round-trips for every
  code and maximum-size field; record-size enforcement in routing, active,
  diagnostic, terminal, progress, and completed states; sanitized external
  reject detail with an observable shortening marker; and absence of unbounded
  durable error strings;
- an ordered request whose initial active envelope fits but whose maximum
  reachable receipt/diagnostic progress shape would exceed the record bound
  rejected before dispatch, plus the largest admitted envelope advancing
  through every progress state without a size failure;
- ordered receipts with zero and exactly 2,048 sorted unique hot-forward
  vertices round-tripping through Graph journal/wire, Graph response, Router
  progress, and stable record; 2,049, duplicate, or descending ids rejected at
  every decoding/construction boundary;
- `RouterMutationRecord::Storable::BOUND` remaining physically unbounded while
  the facade rejects an encoded record one byte over the logical 2 MiB maximum
  in every lifecycle variant;
- routing-lease reclaim before an ordered envelope exists, atomic transition to
  the active envelope before dispatch, and clearing of top-level resolved tables
  once the typed request becomes authoritative;
- transient no-dispatch failure allowing exact same-key resolution retry, and
  every deterministic post-reservation/pre-dispatch admission class durably
  recording `terminal_failure`, clearing its lease, refusing future routing
  acquisition, and returning the same stored error after catalog/routing
  changes;
- registry lookup unavailability, missing/stale/changing shard-owner binding,
  and missing or version-mismatched target capability each remaining
  same-key-retryable without an active envelope, while a missing logical
  catalog name remains the exact stored terminal result;
- invalid client key, undecodable public wire, or gross public field/request
  bound rejection before reservation without creating durable mutation state;
- ordered ambiguous-callback journal reconciliation, explicit-retry exact
  canonical redispatch, canonical-dispatch-free background journal
  reconciliation/projection/retirement recovery, target progress, and
  retirement-gated completed aggregate-receipt compaction with its token
  watermark;
- Graph commit followed by a lost canonical-result Router callback, more than
  nine days of elapsed retention time and repeated Graph GC, then exact same-key
  retry returning the original receipt without adding any edge or physical
  projection;
- retirement callback loss before Router persists completion, idempotent exact
  retirement retry, conflicting-fingerprint retirement rejection, active
  ordered entries surviving the write-path GC and any future operator GC
  regardless of age,
  retired ordered entries becoming evictable only from `Retired.at_ns`, and the
  canonical execution endpoint returning `MutationRetired` without writes after
  retirement;
- retirement callback loss extending beyond post-retirement Graph retention,
  with `Absent` leaving Router in `RetirementPending` for operator repair and
  never authorizing canonical query, redispatch, or completion;
- bounded GC scanning past an old ineligible active ordered entry and still
  evicting a later eligible retired entry without cursor starvation;
- Router refusing to invoke retirement from any state before durably persisted
  `RetirementPending`, including while any required projection is incomplete;
- background recovery of a lost canonical-result callback by exact journal
  lookup, including an `Absent` result that leaves `CanonicalPending` unchanged
  and never invokes the canonical execution endpoint;
- new named buckets, default/unlabeled bypass or promotion, and another
  optimized-unsupported geometry through the whole-request ordered scalar
  fallback;
- duplicate/parallel targets rejected before writes until section 9 activates;
- oversize or over-budget input requiring more than one Graph request rejected
  by Router before Graph dispatch;
- exact preservation of public array position through the single Graph request,
  exact replay, and aggregate-only completion;
- a mixed directed/undirected/self-loop request proving that Graph receipt,
  Graph journal `row_count`, Router `completed_row_count`, and public item count
  all equal the number of logical input items rather than the number of
  physical projections, sidecars, or derived events;
- globally unique/constrained initial property rejection before ordered payload
  persistence or any Graph dispatch, including a mixed constrained/unconstrained
  request;
- cross-shard logical edge or multi-shard public-batch rejection before active
  ordered payload persistence or any Graph dispatch;
- absence from the public v1 operation enum and generated SDK surface of vertex
  insertion, existing-value/property update, and combined vertex/edge batch
  operations;
- no cross-label global-order claim in traversal tests.

Adversarial tests must reject:

- independently sorted forward/reverse projections;
- role-split reservations that target the same orientation/bucket;
- a missing or duplicated physical projection id, including a collision between
  the two halves of one logical edge;
- a property-bearing batch using aggregate/no-location mode, or a captured
  property location whose canonical-handle key set is not exactly
  `0..logical_item_count`, is missing or duplicated, belongs to the wrong
  physical side, maps to a different logical ordinal, or omits a property-free
  item from whole-batch capture;
- a Graph request item count different from `public_item_count`, or any
  reconstruction/filtering that changes the public array order;
- a tombstone reuse that places a new row before a surviving live row;
- stale bucket geometry between reserve and commit;
- order-insensitive request fingerprints;
- ordered payload persistence or Graph dispatch after constrained-property
  admission fails;
- any Graph envelope that contains only one physical side of a logical edge,
  whose endpoints resolve to different shards, or whose items do not all resolve
  to its single target shard;
- an identity/payload kind mismatch, missing or additional ordered target,
  request/registry identity mismatch, wrong Graph target shard/canister, or
  request/target/journal fingerprint mismatch;
- mutable catalog, schema, shard-routing, registry, or capability validation
  running before an existing matching Graph journal entry is returned, or an
  exact completed replay whose receipt changes after that mutable metadata
  changes;
- a `GraphMutationRequestIdentityV1::OrderedEdgeBatch` Graph journal entry with
  `Incomplete`, `next_index`, or `bulk_progress`, including a decoder that
  accidentally routes it through PlanExecution continuation recovery;
- a `GraphMutationRequestIdentityV1::PlanExecution` stable or wire entry whose
  retirement is not
  `NotApplicable`, an ordered stable or wire entry with `NotApplicable`, or a
  Graph stable-to-wire projection whose `Active`/`Retired` state disagrees;
- an ordered receipt or journal whose logical count differs from the Graph
  request, Router public identity, or Router completed count, including a
  directed batch that incorrectly reports its physical projection count;
- an ordered routing payload after possible dispatch, or an active ordered
  payload that retains routing state or duplicate top-level resolved tables;
- a routing state combining a terminal failure with an active lease, or any
  active/completed ordered payload carrying `terminal_failure`;
- an ordered transient diagnostic representable as a terminal code, arbitrary
  upstream text persisted without the bounded diagnostic constructor, or any
  Router lifecycle record exceeding `MAX_ROUTER_MUTATION_RECORD_BYTES`;
- an active envelope admitted without reserving enough encoded headroom for its
  largest reachable receipt and retry diagnostic, including a callback that
  commits Graph state but cannot persist `CanonicalCommitted`;
- an ordered receipt with more than 2,048 hot-forward vertices, duplicate or
  non-ascending ids, or a Graph/Router boundary applying a different bound;
- a physical 2 MiB `Storable::Bounded` maximum substituted for the logical
  encoded-size guard without the required stable-map benchmark evidence;
- invalid ordered target progress transitions, premature envelope compaction, or
  canonical redispatch by background recovery or from
  `RetirementPending`;
- age-only eviction of an active ordered journal entry, retirement with a
  conflicting fingerprint/request kind, completion before retirement proof, or
  treating journal absence as retirement proof from any Router state;
- partial canonical success returned as a recoverable error;
- mixed Router/Graph release-set activation or stale derived indexes after the
  fresh reset;
- a pre-envelope registry/capability failure assigned to the wrong
  terminal-versus-retryable class, or an active request rerouted after its
  target capability changes; and
- an SDK-generated canonical value that diverges from the normative Rust
  conformance vectors or an SDK surface that accepts caller-supplied
  fingerprints.

## Benchmark contract

Benchmark the actual invariant-preserving paths. Do not disable counterpart
invalidation, payload alignment, sidecars, journals, or required maintenance to
make ordered insertion appear cheaper.

The initial comparison matrix contains:

```text
ordered optimized batch
ordered proven scalar fallback
Router ordered replay-record insert/replace:
    small routing, maximum admitted active, maximum receipt progress,
    retirement pending, compacted
```

The current ADR 0045 physical batch substrate is a mandatory baseline row,
using identical geometry, payload, sidecar, journal, counterpart, and maintenance
work. This measures the cost of enforcing the new order contract even when the
physical writer is otherwise shared. An unordered row is added separately only
when there is a concrete, correctness-complete candidate whose physical
reordering differs from the ordered implementation; ADR 0045's current
ordinal-ordered run writer remains the baseline, not evidence that the new
ordered path is free.

The Router replay-record benchmark uses the selected
`Storable::BOUND = Bound::Unbounded` physical strategy while exercising the
logical encoded-size guard. Any future proposal to replace it with a large
variable-size `Bounded` maximum must first compare fresh-key insert and
same-key replacement instructions and stable-memory growth at small and
maximum-admitted record sizes. A physical-bound change is rejected when it
counterpartrially regresses either workload without a compensating safety property
that the existing owner-side encoded-size checks cannot provide.

Graph journal benchmarks include active-to-retired same-key replacement and
the bounded GC scan when it encounters old active and retired ordered
entries. The former is part of the completion cost; the latter must demonstrate
that skipping an ineligible active entry remains bounded and does not starve
later eligible entries.

The current clean-slab benchmark with one edge per source bucket is retained as
a placement baseline but is insufficient to measure order preservation.
Same-bucket fan-out/fan-in, parallel-pair, overflow, expansion, and
tombstone-heavy fixtures are required for the replacement decision.

Focused development runs use one canbench pattern per invocation. Final
artifact updates use unfiltered `canbench --persist` in each affected crate.

## Consequences

Positive:

- One standard edge-insertion batch API has deterministic, useful semantics.
- Callers can precompute application order without paying scalar mutation cost.
- ADR 0045's placement and atomicity work is reused rather than duplicated.
- ADR 0048 pair rank and counterpart ownership become the direct basis of batch
  correctness.
- Bucket processing and stable-memory locality remain optimizable independently
  of bucket-local row order.
- No per-edge sequence field enlarges the traversal row or creates another
  stable source of truth.
- The unshipped unordered endpoint does not become permanent product surface
  without evidence.
- An ordered Graph journal entry cannot age out while Router can still
  redispatch its canonical request.

Costs and trade-offs:

- Some tombstone holes cannot be reused by insertion and remain maintenance
  debt until order-preserving compaction.
- Planning/reservation must prove final live order as well as capacity.
- Optimized coverage remains incremental while ADR 0045 geometry is incomplete.
- Parallel inserts require stronger ordinal, sidecar, retry, and counterpart tests
  before the current duplicate rejection can be removed.
- Globally unique/constrained initial properties and non-edge batch operations
  remain outside the v1 public surface.
- Global order across labels is deliberately not provided.
- The v1 public endpoint rejects a batch when its items resolve to multiple
  shards or its single-shard projection needs more than one Graph request;
  multi-shard and multi-chunk ordered sessions remain future protocols.
- Benchmark and fresh-layout activation gates delay public activation until ADR
  0048 and the relevant ordered paths are complete.
- Unretired ordered journal entries may outlive the nine-day plan-execution
  window. Bounded autonomous journal reconciliation and retirement recovery are
  therefore part of the liveness contract; safety takes precedence over
  force-evicting an unresolved replay identity.
- Ordered completion adds one idempotent Router-to-Graph retirement call after
  projection convergence. This cost is accepted because an age-only Graph GC
  cannot be sound while Router retains non-terminal ordered sagas without a
  replay deadline.
- If every retirement acknowledgement is lost until the retired Graph entry
  itself ages out, Router remains `RetirementPending` and requires operator
  repair. The design accepts this fail-closed liveness loss rather than infer
  success from absence or risk canonical re-execution.

## Alternatives considered

### Allow one public v1 batch to fan out across Graph shards

Router could stable-filter the public input into one ordered subsequence per
target shard and persist all envelopes before dispatch. This preserves order
inside each shard, but it creates an irreversible partial-success state: one
shard may commit before another deterministically rejects its request. The
remaining saga could neither become `Failed` nor safely reroute, while the
aggregate-only receipt could not report successful completion. A cross-shard
prepare/commit or explicit partial-terminal contract would add a new protocol
unrelated to the forward/reverse ordering problem. Rejected for v1; the entire
public batch must target one Graph shard.

### Generalize v1 to heterogeneous ordered graph operations

One public operation enum could combine vertex insertion, edge insertion,
existing inline/property updates, and combined new-vertex/new-edge references.
That would appear to replace more of ADR 0045 at once, but it requires
operation-specific conflict ordering, projected vertex-id allocation,
cross-operation references, uniqueness claims, and a larger atomic fallback and
benchmark matrix. It is rejected for v1 as broader than the demonstrated
forward/reverse adjacency-order problem. ADR 0045 stages 6–7 may still build
internal primitives without creating a public unordered surface.

### Keep unordered and ordered public APIs permanently

This is the minimum implementation change, but it creates two semantic surfaces,
duplicate caller choices, and a long-term test matrix before evidence shows a
counterpartrial performance need. Rejected by default; it may be reconsidered only
through the benchmark gate.

### Preserve order by scalar insertion

This provides a simple reference path but loses complete-batch capacity
projection, contiguous writes, one-shot expansion, aggregate metadata updates,
and bounded maintenance debt. Retained only as a proven transitional fallback.

### Sort forward and reverse projections independently

This may improve physical locality but breaks equal-neighbor pair rank for
parallel edges and makes counterpart resolution depend on extra metadata. Rejected.
Bucket processing may be reordered; bucket-local pending rows may not.

### Store a monotonic sequence id in every edge row

This can represent a global cross-label sequence but enlarges the four-byte
traversal row or introduces another stable index, migration, repair, and
consistency surface. Rejected as disproportionate to bucket-local insertion
order.

### Store request ordinals durably as edge identity

Request ordinals are request-local and collide across mutations. Making them
durable would require a global allocation protocol and duplicate the physical
handle/pair-rank model. Rejected.

### Implement ordered placement in GraphStore

GraphStore does not own slab/log layout, PMA geometry, tombstones, compaction, or
physical bucket order. Moving final placement there would leak LARA internals
and split invariant ownership. Rejected.

## Design documentation impact

- ADR 0045 remains Partially Implemented and records the retained physical batch
  substrate. It links this planned successor without claiming that the
  unordered public API shipped.
- ADR 0048 remains the prerequisite owner of pair rank, counterpart resolution,
  invalidation, rebuild, and alias removal.
- ADR 0025 retains non-terminal ordered retirement states regardless of age;
  Router compaction remains the only transition to terminal TTL eligibility.
- ADR 0027 keeps its implemented age-only
  `GraphMutationRequestIdentityV1::PlanExecution` policy and records the planned
  request-kind-aware ordered retirement predicate.
- ADR 0029 permits bounded autonomous retirement recovery as post-canonical
  work and exact journal-only reconciliation of an ordered unknown canonical
  outcome while continuing to forbid background canonical redispatch.
- `design/storage/lara.md` records ADR 0049 as planned after ADR 0048 and
  distinguishes its order contract from the implemented ADR 0045 substrate.
- `design/storage/bulk-ingest-finalize.md` distinguishes the planned
  order-preserving direct batch path from the existing maintenance/finalize
  hook.
- Router/Graph wire, SDK, stable-memory inventory, inline-property, and
  LARA/facade documents are updated with implementation slices, not in advance
  as though the planned API were active.

## Related

- [ADR 0001](0001-labeled-segment-slide.md): labeled PMA leaf physical layer.
- [ADR 0015](0015-label-stats-projection-log.md): Graph mutation
  journal and durable label-stats projection boundary.
- [ADR 0016](0016-overflow-log-tombstones-and-src-fields.md): overflow-log and tombstone layout.
- [ADR 0020](0020-deferred-maintenance-timer-drain.md): maintenance compaction and deferred drain.
- [ADR 0023](0023-federated-index-consistency-upgrade-compaction.md): derived-index consistency.
- [ADR 0025](0025-client-mutation-journal-retention-sweep.md): Router terminal-only retention.
- [ADR 0026](0026-reverse-adjacency-differential-repair.md): reverse adjacency repair.
- [ADR 0027](0027-graph-mutation-journal-retention.md): Graph mutation-journal retention.
- [ADR 0029](0029-shard-local-atomicity-and-cross-canister-consistency.md): shard-local atomicity.
- [ADR 0030](0030-cross-shard-uniqueness-tcc-reservation.md): uniqueness coordination.
- [ADR 0041](0041-router-graph-batch-mutation-dispatch.md): Router-to-Graph dispatch.
- [ADR 0042](0042-router-dynamic-instruction-budget-batching.md): dynamic continuation.
- [ADR 0044](0044-router-bulk-mutation-key.md): durable bulk identity.
- [ADR 0045](0045-unordered-batch-graph-mutations-and-lara-placement.md): retained physical batch substrate and superseded future unordered contract.
- [ADR 0047](0047-shared-typed-graph-bulk-envelope.md): shared typed Graph bulk envelope.
- [ADR 0048](0048-lara-counterpart-resolution.md): prerequisite physical pair rank and adaptive counterpart ownership.
- [LARA storage contract](../storage/lara.md).
- [Bulk ingest finalize](../storage/bulk-ingest-finalize.md).
