# 0065. `CREATE VECTOR INDEX` as Router-owned semantic DDL

Date: 2026-08-11
Status: proposed contract (v1 parser, Router registration, logical-index-name resolution, and migration-path provisioning landed; focused PocketIC ingress E2E passed; activated ANN success and `DROP VECTOR INDEX` deferred)
Last revised: 2026-08-21
Anchor timestamp: 2026-08-21 18:39:26 UTC +0000

## Context

The Gleaph dialect contract reserves the following shape for vertex vector indexes:

```gql
CREATE VECTOR INDEX document_embedding
FOR (d:Document)
ON d.embedding
OPTIONS {
  indexConfig: {
    dimensions: 768,
    similarity_function: "cosine",
    encoding: "f32",
    algorithm: "ivf_flat"
  }
}
```

The v1 vendor parser, Router registration path, versioned definition record and allocator, and
logical-name resolution for GQL/direct search have landed. The parser's 22 tests, check, clippy,
format, and diff checks pass, and the focused PocketIC ingress test
`gql_create_vector_index_nested_options_is_idempotent_and_fail_closed` passed (1 passed, 0 failed,
6 filtered). Migration-path provisioning (ADR 0071) now routes a `CREATE VECTOR INDEX` migration
through the same `create_vector_index` provisioning/registration logic; activated ANN success and
`DROP VECTOR INDEX` remain deferred. The statement is owned by
`crates/index-ddl` and intercepted by Router before generic GQL parsing; generic `gleaph-gql` and
`gleaph-gql-planner` remain provider-neutral.

The Router owns vector definitions and the write path that validates the graph, resolves one label,
interns the distinct logical index and embedding-field names, and stores a fail-closed definition.
The V2 durable record is keyed by `(GraphId, index_id)` and contains `index_name_id`,
`embedding_name_id`, labels, dimensions, metric, encoding, target, and activation state. GQL and
direct `vector_search` now resolve the logical name; ingestion continues to identify the embedding
field represented by `ON d.embedding`.

The active vector redesign makes the vector canister the owner of indexed embedding bytes; Router
MemoryId 53 owns pending/retry payload bytes until exact marker retirement. The stamp request now
carries metadata and the Router-issued stamp only. Router validates public value dimensions and
finiteness before allocation or the Graph call; Graph validates vertex state, required labels, and
payload-independent embedding metadata/encoding before returning the stamp. Router then forwards
the bytes directly to the vector canister.

## Problem

Adding only a parser would create a public statement whose names, dimensions, encoding, physical id,
activation behavior, and failure semantics are not representable by the current Router catalog. It
would also risk making a Manager-authorized schema DDL perform Admin-only target attachment or global
activation, and would incorrectly imply that existing Graph data can backfill a newly created vector
index.

## Existing Architecture Assessment

Router is the correct owner for this operation: it owns external authorization, graph and label
resolution, logical catalogs, vector readiness, and orchestration. Graph owns vertex existence and
label membership; it must not own vector bytes or vector-index metadata. The vector canister owns
embedding bytes, subject clocks, ANN structures, and ranking. The generic GQL crates can represent the
provider-neutral `SEARCH` plan but must not acquire Router, Graph, or canister concepts.

The existing `admin_register_vector_index` mutation is the nearest extension point. It is synchronous,
performs every fallible check before its first durable definition write, and has no remote side effect.
The DDL should call a shared Router registration service rather than write stable maps directly or
call a public ingress method recursively.

## Alternatives

### A. Minimum change: alias the DDL name to the embedding field

Treat `CREATE VECTOR INDEX x ... ON d.x` as an embedding-field registration and keep the numeric id as an
internal detail.

This is small, but it contradicts the published syntax, prevents independent index and embedding
field names, and makes future multiple indexes or model versions ambiguous. It is rejected.

### B. Chosen: named semantic definition, existing Router registration boundary

Use the existing graph-scoped `IndexNameCatalog` as the logical name source of truth. Extend the
Router vector definition with an `IndexNameId`, keep the physical `index_id` opaque and Router
allocated, and resolve a logical name to the definition through the Router catalog. Do not add a
second vector-name catalog or let GQL/Graph write stable memory.

For the first slice, retain the existing vector definition collection and its physical-id key; the
name association is part of the same versioned definition record and name lookup is a bounded scan
of that graph's definitions. This avoids a derived name-to-id map and a second consistency surface.
The physical-id allocator is Router-owned stable state, monotonic, never reused, and never exposed in
GQL. A later measured optimization may change the collection key to the logical name, but that is not
needed to establish the contract.

The first slice does not require a standalone embedding-schema catalog. Instead, `ON d.embedding`
declares Router-owned typed embedding-field metadata within the same vector definition;
`dimensions` and `encoding` are its immutable type/shape. It is not an ordinary Graph property and
does not create Graph-side byte storage. A graph has at most one vector index for an embedding field
in this slice. Exactly one known vertex label is accepted; label scope is creation-fixed.

### C. Large change: introduce a standalone embedding-schema catalog and remote physical creation

Add a Router-owned `EMBEDDING<encoding, dimensions>` standalone schema catalog, make vector DDL
refer to that schema, and synchronously provision or attach vector canisters.

This gives a richer long-term language but introduces a second schema lifecycle, remote effects after
`await`, pending/ack/reconcile states, target selection, and physical deletion/build semantics. It
also cannot backfill bytes under the active vector ownership contract. It is deferred.

## Decision

Adopt Alternative B as a proposed, narrow semantic DDL contract. The v1 code and focused PocketIC
ingress validation have landed; remote provisioning, backfill, activated ANN success, and `DROP
VECTOR INDEX` remain deferred.

### Syntax

The vendor parser, not the ISO GQL parser, accepts exactly one statement in the first slice:

```gql
CREATE VECTOR INDEX <index_name> [IF NOT EXISTS]
FOR (<variable>:<vertex_label>)
ON <variable>.<embedding_field>
OPTIONS {
  indexConfig: {
    dimensions: <positive_integer>,
    similarity_function: "l2_squared" | "cosine",
    encoding: "f32" | "i8",
    algorithm: "ivf_flat"
  }
}
```

The keyword order follows the existing vendor `CREATE INDEX` convention. The canonical accepted
option shape uses one `indexConfig` map with the unprefixed keys shown above. A flat `OPTIONS {
dimensions, metric, encoding, algorithm }` map is accepted as a Gleaph shorthand; `metric` is also an
alias for `similarity_function`, and using both is a duplicate conflict. Single- and double-quoted
values are accepted. Backticked dotted Neo4j keys such as `vector.dimensions` are not part of this
subset. Option names are case-insensitive, each canonical option appears once, and unknown options,
additional nesting, edge patterns, multiple labels, and non-positive or out-of-range dimensions are
rejected. The only physical kind accepted is `IvfFlat`; no `nlist`, centroid, target principal,
`eps_query`, or rebuild knob is part of this DDL.

### Name and shape invariants

- `index_name` is a logical vector-index name and is distinct from `embedding_field`.
- The logical name uses the existing graph-scoped `IndexNameCatalog`; a simultaneous property index
  with the same name is a conflict.
- The embedding field is Router-owned typed metadata in the same vector definition. Its
  `dimensions` and `encoding` are immutable type/shape; it is not a Graph property, Graph byte
  store, or standalone embedding-schema catalog entry.
- A graph has at most one vector index for an embedding field in this slice.
- The label set contains exactly one known vertex label and is immutable after creation.
- `metric` and `IvfFlat` are immutable definition fields.
- The physical `index_id` is allocated by Router, persisted in the definition, and is not accepted in
  GQL or exposed as the semantic name.

`IF NOT EXISTS` is an exact-definition operation: an existing vector definition with the same logical
name is a no-op only when label, embedding field, dimensions, metric, encoding, and algorithm all
match. Any mismatch is a conflict, including when `IF NOT EXISTS` is present. A failed declaration
must not allocate a logical name or physical id, or write definition metadata.

### Execution and authorization

Router performs `authorize_index_ddl`, resolves the default graph using the existing DDL boundary,
preflights the logical name, exactly one known label, typed embedding-field metadata, and all
options, allocates the physical id, and commits the definition through one shared synchronous
registration service. Every fallible operation precedes the first durable write. The operation has no
Graph or vector-canister call and therefore no remote partial-success state.

Creation always stores `Registered` with no target. It makes no remote provisioning or vector call:
it never sets the global activation flag, attaches a shard, provisions a canister, trains an index, or
calls a vector endpoint. Target selection and attachment remain Admin-only operational APIs. A newly
created index is empty until clients ingest embeddings; there is no Graph backfill because Graph is
not the byte source. Search remains fail-closed until the existing target, shard-attachment, and
global activation gates are satisfied.

`SEARCH ... VECTOR INDEX <index_name>` and the semantic direct vector-search API resolve the logical
index name. The ingestion API continues to identify the embedding field in this slice; the Router
resolves the corresponding definition through the same catalog.

### Stable state and migration

The versioned vector-definition record has gained `index_name_id`; it keeps the embedding field name,
dimensions, and encoding as Router-owned metadata in that same definition. The existing
`ROUTER_VECTOR_INDEXES` memory region remains the definition source of truth. Router MemoryId 52 now
owns the monotonic vector-id cell, and the stable-memory inventory records its value type,
initialization, and no-reuse rule.

V1 records cannot infer a distinct logical name. The implementation uses a breaking V2 envelope and
explicitly rejects V1 data, requiring an empty V2 development catalog. There is no implicit alias or
compatibility reader; any production migration remains a later gate.

### Explicit non-goals

This decision does not implement `DROP VECTOR INDEX`, rebuild, health, multi-label/fan-out indexes,
a standalone embedding-schema catalog, vector-canister provisioning, or Graph backfill. The
metadata-only stamp boundary is implemented by ADR 0064; a successful CREATE response still must
not imply any of these deferred capabilities.

## Consequences

Positive consequences:

- The public name distinction is representable without a second derived name map.
- Router remains the sole writer of vector definition state and the authorization boundary is reused.
- DDL is atomic within one Router update and cannot report success after a remote side effect.
- Generic GQL and planner crates remain portable.
- Activation and byte ownership remain fail-closed and explicit.

Accepted costs:

- The implementation incurs a versioned stable-record change and a physical-id allocator.
- Name lookup is a bounded graph-local scan until measurements justify a different key or index.
- Users provide dimensions and encoding on the vector definition because this slice has no standalone
  embedding-schema catalog.
- A created definition is not immediately searchable and requires client re-ingestion after activation.
- Existing numeric-id registrations need an explicit breaking-release migration decision.

## Migration

1. Synchronize the active vector ownership/status documents with ADR 0064 and record the current
   stamp-path byte mismatch; do not claim the redesign is fully implemented.
2. Add the versioned Router definition field and allocator, with stable-memory inventory updates and
   upgrade/reset policy decided before the first write.
3. Extract the shared Router registration service and add exact-definition preflight, typed
   embedding-field shape checks, and name-conflict checks.
4. Extend `gleaph-index-ddl` with the vector statement and route it through the existing Router DDL
   interception. Do not add vector syntax to generic GQL planning.
5. Change GQL/direct search resolution to logical index names and add focused unit, Router, and
   PocketIC tests for authorization, invalid-input side effects, exact replay/conflict, activation
   blocking, and create-then-search resolution.
6. Only after this slice is reviewed, design DROP and remote cleanup as separate boundaries.

## Design Documentation Impact

- `design/gql/extension-syntax.md`: records the landed v1 shape and links this ADR; keep status
  Partially Implemented because the focused ingress E2E has passed but remote provisioning, backfill,
  activated ANN success, and `DROP VECTOR INDEX` remain deferred.
- `design/adr/README.md`: add this proposed ADR.
- `design/storage/stable-memory-inventory.md`: documents the V2 vector record and MemoryId 52 allocator.
- `design/index/vector-index.md` and ADR 0064: synchronize implementation status and the stamp-path
  ownership wording before declaring this feature complete.

## Required Axes Impact

- **Encapsulation:** Router owns names, authorization, allocation, and catalog writes; Graph and Vector
  receive no DDL state.
- **Separation of concerns:** vendor parsing is isolated in `index-ddl`; generic GQL remains portable;
  physical ANN and activation remain operational concerns.
- **Invariants:** exact definition equality, one vector index per embedding field per graph, fixed
  label/shape, monotonic physical ids, and fail-closed readiness are enforced by Router catalog
  services.
- **Consistency:** one durable definition is the SSOT; no secondary name-to-id map or remote effect is
  introduced by CREATE.
- **Fitness for purpose:** the slice exposes declarative schema intent without pretending to create
  physical storage or backfill data that the current ownership model cannot provide.
