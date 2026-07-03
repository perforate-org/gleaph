# Gleaph GQL extension syntax

Last updated: 2026-07-03
Anchor timestamp: 2026-07-03 09:54:43 UTC +0000

## Status

**Dialect contract with a canonical Rust manifest and implemented pieces. ADR 0034 Slice 6 leading labeled `SEARCH ... WHERE` equality filter through Slice 19 bounded heterogeneous equality/range `OR` disjunctions are implemented; **Slice 20 scalar `INLINE` edge-property schema registration (`CREATE EDGE LABEL ... { <property> <scalar> INLINE }`), Slice 21 ordinary read access (`e.property`, `WHERE e.property`, `ORDER BY e.property`), Slice 22 ordinary mutation packing (`INSERT ... {distance: 7}`, `SET e.distance = 9`), Slice 23 ordinary `COST BY e.distance` shortest-path cost of scalar values into the inline payload, Slice 24 fixed-size inline struct schema registration (`CREATE EDGE LABEL ... { <property> { ... } INLINE }`), and Slice 25 ordinary read access to fixed-size inline edge struct fields (`e.stats` returns a record; `e.stats.field` works in projection, filter, `ORDER BY`, aggregate input, and sidecar precedence) are implemented**; struct mutation packing, `COST BY` over a struct field, property indexes on inline struct fields, nested structs, and generic `CREATE GRAPH TYPE` `INLINE` annotations remain planned; vector-index DDL remains planned.** This document
is the steady-state public syntax contract for Gleaph-specific GQL extensions.

- [layers.md](layers.md), which defines crate and execution boundaries.
- [ADR 0034](../adr/0034-gleaph-gql-extension-syntax.md), which accepts a dedicated dialect contract.
- [vector-index.md](../index/vector-index.md), which defines the vector-index storage and canister
  architecture.

Implementation status in this document is explicit per feature. Planned syntax is not implemented
runtime behavior until marked implemented.

## Goals

1. Keep graph queries readable and close to ordinary GQL.
2. Avoid exposing low-level execution concepts as query-time functions.
3. Treat vector search as a first-class search operation, not as a public procedure call.
4. Treat edge-local fast values as ordinary property access with an inline storage modifier.
5. Keep operational and maintenance procedures under the `GLEAPH.*` namespace.
6. Keep `gleaph-gql` and `gleaph-gql-planner` general-purpose: Gleaph/IC-specific backend meaning
   belongs in Router/Graph integration layers.

## Rust manifest

The canonical Rust registry for Gleaph GQL extension names lives in `gleaph-graph-kernel::gql_dialect`.
It records canonical names, syntax classes, implementation status, owners, and documentation anchors.
It is a registry and recognizer layer, not an execution dispatcher. Router, Graph, planner
integration, `gleaph-gql-ic`, and `gleaph-graph-vector-index` continue to own their respective
semantics.

## Syntax classes

| Class                         | Public shape                                                              | Status                                                                                                                                                                                                                                                                                               | Owner of meaning                                                                            |
| ----------------------------- | ------------------------------------------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------- |
| IC value type                 | `IC.PRINCIPAL`                                                            | Implemented                                                                                                                                                                                                                                                                                          | `gleaph-gql-ic` value extension                                                             |
| IC runtime function           | `MSG_CALLER()`                                                            | Implemented                                                                                                                                                                                                                                                                                          | Graph execution context                                                                     |
| Edge inline value             | `e.distance`, `e.stats.score` with `INLINE` schema modifier               | **Scalar `INLINE` schema registration, read access, mutation packing, and shortest-path `COST BY e.property` implemented** (`CREATE EDGE LABEL ... { <property> <scalar> INLINE }`; ordinary `e.property`, `WHERE e.property`, `ORDER BY e.property`, `INSERT ... {distance: 7}`, `SET e.distance = 9`, `ANY SHORTEST ... COST BY e.distance`); **fixed-size struct schema registration and ordinary struct field reads are implemented** (`CREATE EDGE LABEL ... { <property> { ... } INLINE }`; `e.stats` returns a record, `e.stats.field` works in projection, filter, `ORDER BY`, aggregate input, and sidecar precedence); struct mutation packing, `COST BY` over a struct field, property indexes on inline struct fields, and nested structs remain planned | Router schema/catalog + Graph edge payload execution                                        |
| Shortest-path cost            | `COST BY e.distance`                                                      | **Implemented** for the bounded direct-property shape: one concrete edge label, one declared edge variable, and exactly `e.<inline-property>`; `GLEAPH.COST BY GLEAPH.WEIGHT(e)` compatibility surface preserved; compound expressions and label-expression costs remain planned | Graph query planner/executor                                                                |
| Current edge weight function  | `GLEAPH.WEIGHT(e)`                                                        | Implemented compatibility surface                                                                                                                                                                                                                                                                    | Graph query executor                                                                        |
| Edge insertion-order sequence | `GLEAPH.SEQUENCE(e)`                                                      | Implemented compatibility surface                                                                                                                                                                                                                                                                    | Graph edge storage/execution                                                                |
| Edge-payload vector predicate | `GLEAPH.VECTOR.L2_SQUARED(e, $q) <= threshold`                            | Implemented compatibility surface                                                                                                                                                                                                                                                                    | Planner fusion + Graph edge payload executor                                                |
| Vertex vector search          | `MATCH ... SEARCH d IN (VECTOR INDEX ... FOR ... LIMIT ...) SCORE AS ...` | Implemented for one top-level `SEARCH`: leading `DISTANCE AS` / `SCORE AS` on exact-scan cosine, leading `SEARCH ... WHERE` with one to eight `AND`-connected same-binding equality predicates on distinct properties backed by active vertex property indexes, one or two same-binding numeric range predicates on the same property (one lower `>`/`>=` and one upper `<`/`<=`, intersected into one encoded interval), one to eight equality predicates plus one one- or two-sided numeric range predicate on a distinct property, two to eight `OR`-connected same-binding same-property equality predicates backed by one active vertex property index (union of `lookup_equal_page` streams with global deduplication and the 4096 candidate bound), two to eight `OR`-connected same-binding cross-property pure equality predicates backed by active vertex property indexes (one `lookup_equal_page` stream per distinct `(property_id, encoded_value)` source, with the same union, deduplication, label filtering, and candidate bound), two to eight `OR`-connected same-binding same-property numeric range predicates (one one-sided range per arm, union of `lookup_range_page` streams with interval merge within the property, global deduplication, and the 4096 candidate bound), two to eight `OR`-connected same-binding cross-property numeric range predicates (one one-sided range per arm, per-property interval merge, union of `lookup_range_page` streams across distinct property ids, global deduplication, and the 4096 candidate bound), two to eight `OR`-connected same-binding heterogeneous equality/range predicates (each leaf independently equality or one-sided numeric range, per-property range interval merge, union of `lookup_equal_page` and `lookup_range_page` streams with global deduplication and the 4096 candidate bound), and non-leading `SEARCH` inner-joined on a bound vertex with the same filtered shapes; `SCORE AS` rejected for distance-only metrics; `WHERE` is fail-closed and index-owned; edge subjects, nested/multiple search, correlated `FOR`/`LIMIT`, text/bytes/temporal/boolean/collection/path range predicates, mixed OR/AND remain planned, and nine-or-more disjunctive arms are rejected fail-closed | Router vector-index catalog + vector canister + Graph seed hydration / resolved-search join |
| Operational procedures        | `CALL GLEAPH.FINALIZE_*`, `CALL GLEAPH.DRAIN_DEFERRED_MAINTENANCE()`      | Implemented                                                                                                                                                                                                                                                                                          | Graph mutation executor / Router orchestration                                              |

## Namespace policy

Daily query syntax should avoid `GLEAPH.*` when the concept is part of the Gleaph GQL dialect:

- Use `SEARCH` for vector retrieval.
- Use ordinary property access for inline edge values.
- Use `COST BY e.distance` for shortest-path cost.
- Use `MSG_CALLER()` for caller context.
- Use `IC.PRINCIPAL` for principal values.

Reserve `GLEAPH.*` for operational procedures and compatibility surfaces:

```gql
CALL GLEAPH.FINALIZE_BULK_INGEST(...)
CALL GLEAPH.FINALIZE_FORWARD_EDGE_SPAN(...)
CALL GLEAPH.DRAIN_DEFERRED_MAINTENANCE()
```

Compatibility surfaces also include existing query-time helpers whose current semantics are
intentionally Gleaph-specific. Some may later be replaced by more ordinary GQL syntax, but their
current names remain part of the implemented dialect contract: `GLEAPH.WEIGHT(e)`,
`GLEAPH.SEQUENCE(e)`, `GLEAPH.COST`, and `GLEAPH.VECTOR.*`.

## IC extensions

### `IC.PRINCIPAL`

**Status:** Implemented.

`IC.PRINCIPAL` is a GQL extension value type for Internet Computer principals. It is encoded and
decoded by `gleaph-gql-ic`; `gleaph-gql` remains free of IC dependencies.

Use it for parameters and property values that must carry a principal:

```gql
MATCH (a:Account)
WHERE a.owner = $caller
RETURN a
```

The parameter value may be an `IC.PRINCIPAL` extension value.

### `MSG_CALLER()`

**Status:** Implemented.

`MSG_CALLER()` evaluates to the canister caller principal in graph execution context. It is
unqualified and takes no arguments:

```gql
MATCH (a:Account)
WHERE a.owner = MSG_CALLER()
RETURN a
```

This function is an execution-context extension, not portable GQL core. Host tests without caller
context must provide one or receive a runtime-function error.

## Edge inline properties

### Target syntax

**Status:** Planned target. Existing code still exposes `GLEAPH.WEIGHT(e)` and fixed-width edge
payload profiles.

`INLINE` is a storage/layout modifier for one edge-label property. It is not a logical type. The
logical query surface is ordinary property access:

```gql
CREATE EDGE LABEL ROAD {
  distance FLOAT32 INLINE
}

MATCH (a)-[e:ROAD]->(b)
RETURN b, e.distance
ORDER BY e.distance ASC
```

### One inline slot per edge label

Each edge label may define at most one `INLINE` field. The inline slot may be a scalar or a fixed-size
struct.

Allowed:

```gql
CREATE EDGE LABEL LIKED {
  score FLOAT32 INLINE
}
```

Allowed:

```gql
CREATE EDGE LABEL AFFINITY {
  stats {
    score FLOAT32,
    confidence FLOAT32,
    updated_at UINT64
  } INLINE
}
```

Not allowed:

```gql
CREATE EDGE LABEL LIKED {
  score FLOAT32 INLINE,
  confidence FLOAT32 INLINE
}
```

Use a fixed-size struct instead:

```gql
CREATE EDGE LABEL LIKED {
  stats {
    score FLOAT32,
    confidence FLOAT32
  } INLINE
}
```

Inline structs are restricted to fixed-size fields. Variable-length strings, blobs, arrays, and
embeddings are not inline by default.

### Scalar schema registration (Slice 20)

**Status:** Implemented.

The Router now accepts a standalone scalar-only `INLINE` schema declaration:

```gql
CREATE EDGE LABEL ROAD {
  distance FLOAT32 INLINE
}
```

For each edge label, exactly one inline slot is allowed; in Slice 20 it is a scalar. The Router owns the canonical schema
record `(graph_id, edge_label_id) → { property_id, scalar_type, derived EdgePayloadProfile }` in the
existing `ROUTER_EDGE_PAYLOAD_PROFILES` stable region. The physical profile is derived from the scalar
type; Graph receives it on the existing `ResolvedEdgeLabel` wire and stores/executes payload bytes
with that width and encoding.

Accepted scalar types are the fixed-width forms that map losslessly to an existing
`EdgePayloadProfile`: `UINT8`, `UINT16`, `UINT32`, `UINT64`, `INT8`, `INT16`, `INT32`, `INT64`,
`UINT128`, `INT128`, `FLOAT16`, `FLOAT32`, `FLOAT64`, `FIXED32`, and `FIXED64`. Every other type
(including fixed-size structs, strings, variable-width blobs, arrays, embeddings, and weight encodings) is
rejected fail-closed in this slice.

The Router DDL is idempotent for the exact same declaration and returns `Conflict` for any
incompatible redeclaration (different property, scalar type, or a label that already has a legacy
unnamed payload profile installed through the admin API). Authorization follows the same
admin/manager-with-prepare-register policy as index and constraint DDL.

Slice 21 adds ordinary `e.distance` property access for scalar inline slots
(`e.property`, `WHERE e.property`, `ORDER BY e.property`). Slice 22 adds ordinary mutation packing
of scalar values into the inline payload (`INSERT ... {distance: 7}`, `SET e.distance = 9`,
`SET e = {distance: 9, ...}`), with fail-closed rejection of missing, duplicate, `NULL`, overflowing,
non-finite, and malformed fixed-byte values before any canonical write. This section does not add
`COST BY e.distance`, fixed-size struct inline slots, or `INLINE` inside generic `CREATE GRAPH TYPE`
definitions. Those remain planned.

### Struct schema registration (Slice 24)

**Status:** Implemented.

The Router now accepts a standalone fixed-size inline struct declaration:

```gql
CREATE EDGE LABEL AFFINITY {
  stats {
    score FLOAT32,
    confidence FLOAT32,
    updated_at UINT64
  } INLINE
}
```

For each edge label, exactly one inline slot is allowed; it may be a scalar (Slice 20) or a
fixed-size struct (Slice 24), but not both. The Router owns the canonical schema record
`(graph_id, edge_label_id) -> { property_id, declaration-ordered logical field specs [name, scalar_type] }`
in the existing `ROUTER_EDGE_PAYLOAD_PROFILES` stable region. The physical profile sent to Graph is
`EdgePayloadProfile::opaque_bytes(total_byte_width)`; Graph receives only the existing top-level
inline property identity and the derived opaque `RawBytes` profile in this slice. Struct fields are
stored in declaration order with no padding. Bounds are enforced fail-closed: non-empty struct,
unique field names within the struct, accepted fixed-width scalar field types only, field count
limit, checked offset/width arithmetic, a conservative execution-safe total byte width limit, and an
encoded stable-record size limit.

Field specs are comma-separated; missing commas and trailing commas are rejected. The stored record
persists only the declaration-ordered logical `(field_name, scalar_type)` pairs; byte offsets, widths,
and total width are deterministically derived by the owner, so the stable record cannot hold inconsistent
derived state.

Exact replay is idempotent only when the same top-level property name, the same field names in the
same declaration order, and the same field scalar types are supplied. Any incompatible
redeclaration, any scalar/struct conflict, any legacy unnamed payload profile, and any property-index
conflict on the same (label, property) are rejected before any catalog or schema mutation. Struct
field reads (`e.stats.score`), struct mutation packing, `COST BY` over a struct field, and generic
`CREATE GRAPH TYPE ... INLINE` annotations remain planned.

### Relationship to `GLEAPH.WEIGHT`

`GLEAPH.WEIGHT(e)` is the implemented compatibility surface for fixed-width edge payload weights.
Ordinary inline property access (`e.distance`) and the existing edge-payload predicate surface
(`GLEAPH.WEIGHT(e) = ...`) share the same Router-resolved `EdgePayloadProfile` and the same
Graph inline-aware read helper. The target dialect
replaces it with ordinary property access:

```gql
MATCH ANY SHORTEST (a)-[e:ROAD]->{1,5}(b)
COST BY e.distance
RETURN b
```

The equivalent implementation-era shape is:

```gql
MATCH ANY SHORTEST (a)-[e:ROAD]->{1,5}(b)
GLEAPH.COST BY GLEAPH.WEIGHT(e)
RETURN b
```

## Edge insertion-order sequence

### `GLEAPH.SEQUENCE(e)`

**Status:** Implemented compatibility surface.

`GLEAPH.SEQUENCE(e)` exposes Gleaph's edge insertion-order compensation for a bound edge variable.
The ordering value is owned by Graph edge storage and execution. It is keyed by the edge identity and
edge-label-local insertion sequence; it is not decoded from edge payload bytes and is not a property
store lookup.

Use it when a query needs deterministic ascending or descending edge order:

```gql
MATCH (a)-[e:FOLLOWS]->(b)
RETURN b
ORDER BY GLEAPH.SEQUENCE(e) ASC
```

Descending order is explicit:

```gql
MATCH (a)-[e:FOLLOWS]->(b)
RETURN b
ORDER BY GLEAPH.SEQUENCE(e) DESC
```

This function must be classified separately from `GLEAPH.WEIGHT(e)` and `GLEAPH.VECTOR.*` in the Rust
manifest. Those helpers read or score fixed-width edge payload bytes; `GLEAPH.SEQUENCE(e)` reads
Graph-owned edge ordering metadata.

## Edge-payload vector predicates

**Status:** Implemented compatibility surface.

`GLEAPH.VECTOR.*` is implemented as SIMD scoring over fixed-width **edge payload** bytes, not vertex
embedding ANN search:

```gql
MATCH (a)-[e:SIMILAR_TO]->(b)
WHERE GLEAPH.VECTOR.L2_SQUARED(e, $q) <= 4.0
RETURN b
```

Supported functions:

- `GLEAPH.VECTOR.L2_SQUARED(e, query) <= threshold`
- `GLEAPH.VECTOR.COSINE_DISTANCE(e, query) <= threshold`
- `GLEAPH.VECTOR.DOT(e, query) >= threshold`

The planner accepts these only when it can fuse them into a fixed-label edge expansion predicate.
Unfused use is rejected. Do not reuse this surface for vertex embedding search.

## Embeddings and vector indexes

### Embeddings are not inline properties

**Status:** Planned schema syntax; ADR 0031 storage and vector-index APIs are implemented in slices.

Embeddings belong to the canonical vertex embedding store and derived vector-index model. They are not
edge inline payloads and are not ordinary variable-size property-store values.

Target schema shape:

```gql
CREATE VERTEX LABEL Document {
  title STRING,
  body STRING,
  embedding EMBEDDING<FLOAT32, 768>
}
```

Alternative spelling such as `VECTOR<FLOAT32, 768> EMBEDDING` may be considered later if it fits the
parser/type system better, but the semantic owner remains the embedding store, not the property store.

### Vector index DDL

**Status:** Planned GQL syntax; Router admin APIs and vector-index catalog already exist.

Target shape:

```gql
CREATE VECTOR INDEX document_embedding
FOR (d:Document)
ON d.embedding
OPTIONS {
  metric: "cosine",
  algorithm: "ivf_flat"
}
```

The public DDL names a vector index and an embedding field. The Router remains the source of truth for
vector-index definitions, embedding-name interning, activation state, policy, and target resolution.

`algorithm: "ivf_flat"` is the baseline. Other algorithms such as HNSW are future options and must not
be implied by the initial syntax.

## `SEARCH` subclause

### Target vector-search syntax

**Status:** Parser and planner representation implemented. Router lowering to the existing vector search API is implemented for the narrow accepted shape: a leading `NodeScan(variable = d, label: optional)` immediately followed by `PlanOp::Search { binding = d, provider: VectorIndex, output: DISTANCE AS alias or SCORE AS alias }`, and one top-level non-leading `PlanOp::Search` after preceding graph operators have bound the vertex variable. Both shapes are vertex-only. Both leading and non-leading shapes accept `SEARCH ... WHERE` with one same-binding labeled equality predicate, one to eight `AND`-connected same-binding labeled equality predicates on distinct properties, one same-binding numeric range predicate (`<`, `<=`, `>`, `>=`), exactly two same-property range predicates forming a two-sided range, one to eight equality predicates on distinct properties plus one one- or two-sided numeric range predicate on a distinct property, two to eight `OR`-connected same-binding same-property equality predicates, two to eight `OR`-connected same-binding pure equality predicates where property names may repeat or differ, two to eight `OR`-connected same-binding same-property numeric range predicates, two to eight `OR`-connected same-binding cross-property numeric range predicates, or two to eight `OR`-connected same-binding heterogeneous comparison predicates where each leaf is independently an equality or a one-sided numeric range comparison (ADR 0034 Slices 6-19); every other `WHERE` predicate is rejected. `SCORE AS` is accepted only for indexes whose metric exposes a score (currently exact-scan `Cosine`, `nlist == 1`); it is rejected for distance-only metrics such as `L2Squared`. Unsupported shapes (multiple `SEARCH`, nested `SEARCH`, edge subjects, `WHERE` filtering beyond the implemented equality, numeric-range, mixed equality-plus-range, same-property/cross-property equality-disjunction, and same-property/cross-property range-disjunction shapes, `OR` with mixed logical operators, two numeric ranges on different properties in a conjunction, correlated `FOR`/`LIMIT`, or any mutation tail) fail closed with an explicit `InvalidArgument` error.

Current runtime exposes vector search through Router Candid API `vector_search(RouterVectorSearchRequest)`.

Vector search is a first-class `MATCH` / `OPTIONAL MATCH` subclause:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
ORDER BY similarity DESC
```

This follows the graph-query shape used by modern Cypher-style vector search: the enclosing `MATCH`
introduces a node or relationship variable, and `SEARCH` constrains that binding to the vector-index
neighborhood.

### Binding rule

The `SEARCH` binding variable must be a node or relationship variable introduced by the enclosing
`MATCH` / `OPTIONAL MATCH` pattern.

Initial implementation covers two shapes.

The simplest leading shape:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
```

And one non-leading shape where the search variable is bound by preceding graph operators:

```gql
MATCH (a:Author)-[:WROTE]->(d:Document)
SEARCH d IN (
  VECTOR INDEX document_embedding
  FOR $query
  LIMIT 10
) SCORE AS similarity
RETURN a, d, similarity
ORDER BY similarity DESC
```

More complex patterns — multiple `SEARCH`, nested `SEARCH`, correlated `FOR`/`LIMIT`,
`OR`/`XOR`/`NOT` `SEARCH ... WHERE` except the supported two-to-eight same-property or cross-property
pure equality `OR` shape or the supported two-to-eight same-property or cross-property
numeric-range `OR` shape, `OR` with mixed logical operators inside the disjunction, two ranges on
different properties in a conjunction, text/bytes/temporal/boolean/collection/path/extension-value range predicates,
four-or-more conjuncts, other three-leaf equality/range mixtures, repeated equality properties in a
conjunction, duplicate-direction range arms, other bindings in the predicate, edge subjects, and
compound vector shapes — remain staged until the planner can reason about the interaction between
vector candidate generation, post-filtering, and traversal:

```gql
MATCH (u:User { id: $user_id })-[e:LIKED]->(d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity, e.score
```

### `SCORE AS` and `DISTANCE AS`

Similarity metrics naturally expose `SCORE AS name`, where higher is better:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
ORDER BY similarity DESC
```

Distance metrics naturally expose `DISTANCE AS name`, where lower is better:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    LIMIT 100
  ) DISTANCE AS distance
RETURN d, distance
ORDER BY distance ASC
```

An index definition must expose only the scoring shape it can define honestly. For example, an
`L2Squared` index can expose `DISTANCE`; a cosine-similarity index can expose `SCORE`. If a future
index supports both a raw distance and a normalized score, it may expose both through explicit aliases,
but there must be no implicit `distance` or `score` binding.

### Optional in-index filtering

`SEARCH ... WHERE` is implemented for one same-binding labeled equality predicate, any number
of `AND`-connected same-binding equality predicates on **distinct** properties up to the
execution bound of 8 arms, exactly one same-binding numeric range predicate (`<`, `<=`, `>`, `>=`),
exactly two same-property range predicates forming a two-sided range, one to eight equality
predicates plus one one- or two-sided numeric range predicate on a distinct property, two to eight
`OR`-connected same-property equality predicates, two to eight `OR`-connected pure equality
predicates on **distinct** or repeated properties, two to eight `OR`-connected same-binding numeric range
predicates on the same or different properties, or two to eight `OR`-connected same-binding heterogeneous
predicates where each leaf is independently an equality or a one-sided numeric range comparison, on both
leading and non-leading `SEARCH`. The accepted leading shapes are:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    WHERE d.category = $category
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
```

The accepted two-equality conjunction shape is:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    WHERE d.category = $category AND d.tenant_id = $tenant
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
```

The accepted N-equality conjunction shape (2..=8 arms) is:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    WHERE d.category = $category
      AND d.tenant_id = $tenant
      AND d.org_id = $org
      AND d.region_id = $region
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
```

The accepted numeric range shape is:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    WHERE d.price >= $minimum_price
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
```

The accepted mixed equality-plus-range shape is:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    WHERE d.category = $category AND d.price >= $minimum_price
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
```

The accepted same-property numeric-range-`OR` shape is:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    WHERE d.price < $low OR d.price >= $high
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
```

Either operand order is accepted for the range comparison (`d.price >= 10` and `10 <= d.price`
are equivalent). The predicate must be a single equality comparison, a single range comparison, an `AND` of one to eight equality comparisons
between distinct properties of the searched binding and a literal or parameter (either operand order
is accepted), an `AND` of exactly two range comparisons on the same property (one lower `>`/`>=` and one upper `<`/`<=`), an `AND` of one to eight equality comparisons on distinct properties together with one or two range comparisons on the same property with every equality property distinct from the range property, an `OR` of two to eight equality comparisons on the same property, an `OR` of two to eight same-binding equality comparisons where property names may repeat or differ, an `OR` of two to eight same-binding same-property numeric range comparisons, an `OR` of two to eight same-binding cross-property numeric range comparisons, or an `OR` of two to eight same-binding heterogeneous comparison predicates where each leaf is independently an equality or a single one-sided numeric range comparison. Every disjunction arm must be either a pure equality comparison or a single one-sided numeric range comparison; nested logical operators, two-sided range disjuncts, and non-numeric ranges remain unsupported. The range comparison is restricted to numeric values; text, bytes, temporal, boolean,
collection, path, record, and extension-value ranges remain planned. The property or properties must
have active vertex property indexes for the exact `(graph, label, property)` tuple; for a non-leading
search the label is proved from the statically known prefix. Otherwise the query fails explicitly. The
result is the exact vector top-k over the property-index candidate set, not a post-filter over the
unrestricted top-k. Candidate sets are bounded to `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` (4096)
distinct subjects; larger sets fail explicitly. Empty candidates preserve the global aggregate dispatch
contract for both leading and non-leading search. Unsupported predicate forms — two ranges on different properties in a conjunction,
text/bytes/temporal/boolean/collection/path/extension-value ranges, compound
`OR`/`XOR`/`NOT` other than the supported same-property or cross-property pure equality `OR`, the supported same-property or cross-property range `OR`, or the supported same-binding heterogeneous equality/range `OR`, nine or more syntactic disjunction arms, two-sided range disjuncts, nested logical operators inside a disjunction, mixed equality/range forms outside the accepted shapes, repeated equality properties, duplicate-direction range arms, `NULL`, functions, computed
expressions, other bindings, and edge subjects — remain planned and are rejected fail-closed rather than
becoming post-filters.

The accepted non-leading shape binds the searched variable earlier in the graph pattern:

```gql
MATCH (a:Author)-[:WROTE]->(d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    WHERE d.category = $category
    LIMIT 100
  ) SCORE AS similarity
RETURN a, d, similarity
```

The same one-equality, two-equality `AND`, N-equality `AND` (2..=8 arms), single numeric-range, same-property two-sided-range, mixed equality-plus-range, same-property equality-`OR`, cross-property pure equality-`OR`, same-property numeric-range-`OR`, cross-property numeric-range-`OR`, and heterogeneous equality/range-`OR` shapes apply to non-leading
search.

### Internal lowering

Slice 3 lowers a leading `NodeScan(variable = d, label: optional)` followed by `PlanOp::Search`
to the existing Router/vector-index API and then dispatches the remaining graph-tail plan from
row-shaped vector-search seeds. Slice 5 lowers one top-level non-leading `PlanOp::Search` to a
Router-resolved global top-k relation that is partitioned by live shard and inner-joined against the
already-bound vertex rows in Graph execution. ADR 0034 Slice 6 and Slice 7 add a filtered path for
leading and non-leading search: the planner validates expression shape, the Router proves exact
label/property index coverage and resolves a bounded candidate allowlist from the Property Index,
and Vector Index ranks exactly over that allowlist. Slice 8 generalizes the accepted filter to one
same-binding equality or up to eight `AND`-connected same-binding equalities on distinct properties.
For one equality arm the Router resolves the property, proves coverage, encodes the value, and
collects candidates through paginated `lookup_equal_page`; for two to eight arms it collects through
the server-side paginated `lookup_intersection_page`, which canonicalises the walk arm by
`(property_id, encoded_value)` order and materialises all sieve arms per page. Slice 9 adds a
single same-binding numeric range predicate. The Router resolves the property, proves an active
vertex property index for the exact `(graph_id, label_id, property_id)` tuple, resolves the comparison
value once, and derives a finite half-open encoded numeric range through the canonical
`gleaph_gql::numeric_range_bounds` helper. It then collects label-qualified candidates through the
paginated `lookup_range_page` path with `PostingRangeRequest::Between { low, high }`. Property Index
owns structural validation and ordered scanning over opaque encoded bytes; `gleaph-gql` owns the
numeric comparison-domain mapping. For a non-leading filtered search the Router additionally proves
one positive simple label for the searched binding from the top-level prefix.
The Router:

1. Resolves the embedding name from `VECTOR INDEX <name>` against the Router catalog.
2. Evaluates `FOR $query` and `LIMIT n` from literals or parameters; both must be row-invariant.
3. For a filtered search, proves an active vertex property index for the exact
   `(graph_id, label_id, property_id)` tuple for every equality arm and for the range property
   (for non-leading search `label_id` comes from the statically proved prefix label). Equality
   arms are encoded with `gleaph_gql::value_to_index_key_bytes`. One equality arm is collected
   through paginated `lookup_equal_page`; two to eight equality arms are collected through
   paginated `lookup_intersection_page`. Nine or more equality arms are rejected with
   `InvalidArgument`. One to eight equality arms combined with one numeric range dimension are
   collected through paginated `lookup_range_intersection_page`, which walks the finite range and
   sieves each page by every equality arm server-side while preserving the range cursor. A single
   numeric range arm is converted to a finite half-open interval by
   `gleaph_gql::numeric_range_bounds` and collected through paginated `lookup_range_page` with
   `PostingRangeRequest::Between { low, high }`. Two numeric range arms on the same property are intersected into one finite half-open interval (`low = max(first.low, second.low)`, `high = min(first.high, second.high)`) and collected through a single paginated `lookup_range_page` stream; if the intersection is empty the candidate set is empty before any Property Index or Vector Index call. Two to eight same-property or cross-property equality arms inside an `OR` are collected through the Router-owned `EqualityDisjunction` path: each arm resolves one `(graph_id, label_id, property_id)` tuple, each tuple must have an active vertex property index, and the candidate set is the union of paginated `lookup_equal_page` results for every distinct `(property_id, encoded_value)` source, label-filtered per page and deduplicated globally by `(shard_id, vertex_id)`. Every path collects at most
   `MAX_VECTOR_SEARCH_FILTER_CANDIDATES` (4096) distinct candidate subjects. Two to eight
   same-property or cross-property numeric-range arms inside an `OR` are collected through the Router-owned
   `RangeDisjunction` path: each arm resolves one `(graph_id, label_id, property_id)` tuple, each tuple must have an active vertex property index, the arms for each property are converted to finite half-open encoded intervals via
   `gleaph_gql::numeric_range_bounds`, merged so that overlapping and touching intervals become a
   single interval **within that property id**, and the candidate set is the union of paginated `lookup_range_page` results for
   each merged interval across all involved properties, label-filtered per page and deduplicated globally by `(shard_id,
   vertex_id)`. An empty candidate set
   skips the vector canister. For a leading search the stripped plan is dispatched with an empty seed
   relation to every live shard; for a non-leading search the full plan is dispatched with an empty
   resolved-search relation to every live shard. A non-empty set is forwarded as
   `VectorSearchRequest.candidate_subjects`.
4. Calls the vector canister exactly once per query to obtain hits; `candidate_subjects = None` keeps
   the existing unrestricted search semantics.
5. Leading and non-leading search share the same derived-index staleness contract: a hit whose
   owning shard is no longer live is ignored rather than failing the query. The remaining hits form
   the global top-k relation according to the configured metric.
6. For a leading search, builds per-shard row-shaped seeds carrying the matched vertex binding and the
   `DISTANCE AS` / `SCORE AS` scalar alias; strips the `NodeScan + Search` prefix and dispatches the
   tail plan to graph shards with the row-shaped seeds.
7. For a non-leading search, converts raw hits to finite user-visible scalar values, partitions them
   by owning shard, and attaches an explicit per-shard resolved relation to the normal read dispatch;
   shards with no local hit receive an explicit empty relation, so empty hits still run any remaining
   plan (e.g., a global aggregate returns one zero row). Graph executes the operator as an inner
   join/filter (`input_rows[d] = H.subject`) that preserves row multiplicity and binds the scalar
   alias.

For this slice the accepted shape is intentionally narrow:

- vertex-only (`d` must be a vertex binding);
- one `SEARCH` per plan, at the top level (no nested or repeated search);
- leading `NodeScan + Search` or one non-leading `SEARCH` after a bound vertex;
- both leading and non-leading `SEARCH ... WHERE` are limited to vertex-only, one same-binding property
  equality predicate, one to eight `AND`-connected same-binding property equality predicates on
  distinct properties, exactly one same-binding numeric range predicate (`<`, `<=`, `>`, `>=`)
  between a property of the searched binding and a literal or parameter, exactly two
  `AND`-connected same-binding numeric range predicates on the same property (one lower `>`/`>=`
  and one upper `<`/`<=`) forming a two-sided range, one to eight same-binding property equality
  predicates on distinct properties together with one one- or two-sided numeric range predicate on
  a distinct property, two to eight `OR`-connected same-binding same-property equality predicates,
  two to eight `OR`-connected same-binding pure equality predicates where property names may repeat or differ,
  two to eight `OR`-connected same-binding same-property numeric range predicates, or two to eight `OR`-connected same-binding cross-property numeric range predicates, and every property must have an active vertex property index for the same
  label (non-leading search obtains the label from the statically proved prefix);
- non-leading `SEARCH` requires row-invariant `FOR` and `LIMIT` (literals or parameters);
- `DISTANCE AS` accepted for all metrics;
- `SCORE AS` rejected when the metric has no natural score (e.g. `L2Squared`);
- no other `WHERE` in-index filtering (compound `OR`/`XOR`/`NOT` other than the supported
  same-property or cross-property pure equality `OR`, the supported same-property or cross-property range `OR`,
  or the supported same-binding heterogeneous equality/range `OR`, two numeric ranges on different properties in a
  conjunction, text/bytes/temporal/boolean/collection/path/extension-value ranges, two-sided range disjuncts,
  nested logical operators inside a disjunction, mixed equality/range forms outside the accepted shapes,
  repeated equality properties, duplicate-direction range arms, functions, other bindings, edge subjects,
  correlated/per-row predicates);
- no correlated/per-row top-k or `FOR`/`LIMIT`;
- no mutation tail;
- hits for non-live shards are ignored consistently for both leading and non-leading search.

A raw `PlanOp::Search` that reaches the graph executor without matching Router-resolved context fails
closed with an explicit error.

This is an internal execution detail. Public GQL should not expose `CALL GLEAPH.VECTOR_SEARCH(...)` as
the primary syntax.

## Full-text, property, and hybrid search

`SEARCH` is a general search shape, but only vector search is in scope for the first implementation.
Future providers may include:

```gql
MATCH (d:Document)
  SEARCH d IN (
    FULLTEXT INDEX document_text
    FOR "distributed graph database"
    LIMIT 20
  ) SCORE AS text_score
RETURN d, text_score
```

```gql
MATCH (d:Document)
  SEARCH d IN (
    HYBRID {
      VECTOR INDEX document_embedding WEIGHT 0.7,
      FULLTEXT INDEX document_text WEIGHT 0.3
    }
    FOR $query
    LIMIT 20
  ) SCORE AS hybrid_score
RETURN d, hybrid_score
```

Property equality/range lookup should usually remain ordinary GQL (`MATCH` pattern predicates and
`WHERE`) rather than being forced into `SEARCH`. A future `PROPERTY INDEX` provider must justify why
it is a search/ranking operation instead of standard indexed pattern matching.

## GraphRAG example

```gql
CREATE EDGE LABEL MENTIONS {
  evidence {
    confidence FLOAT32,
    source_rank UINT16,
    observed_at UINT64
  } INLINE
}

CREATE VECTOR INDEX chunk_embedding
FOR (chunk:Chunk)
ON chunk.embedding
OPTIONS {
  metric: "cosine",
  algorithm: "ivf_flat"
}

MATCH (chunk:Chunk)
  SEARCH chunk IN (
    VECTOR INDEX chunk_embedding
    FOR $query
    LIMIT 100
  ) SCORE AS similarity
MATCH (chunk)-[e:MENTIONS]->(entity:Entity)
RETURN
  chunk,
  entity,
  similarity,
  e.evidence.confidence
ORDER BY
  similarity DESC,
  e.evidence.confidence DESC
LIMIT 30
```

This expresses the intended flow:

1. Vector search generates candidate chunks.
2. Graph traversal expands candidates to entities.
3. Inline edge evidence contributes fast traversal-time ranking signals.
4. Final ranking combines semantic similarity and graph-local evidence.

## Implementation staging

| Stage | Scope                                                                                                  | Status                                                                                                               |
| ----- | ------------------------------------------------------------------------------------------------------ | -------------------------------------------------------------------------------------------------------------------- |
| 1     | Document the dialect contract and keep existing behavior unchanged                                     | Implemented                                                                                                          |
| 2     | Add the Rust extension manifest for canonical extension names, classes, status, owner, and doc anchors | Implemented                                                                                                          |
| 3     | Add `SEARCH` parser/planner representation without backend-specific storage details                    | Implemented                                                                                                          |
| 4     | Add Router lowering from vector `SEARCH` to the existing vector search API                             | Implemented for leading `NodeScan + Search` prefix and non-leading `SEARCH` after a bound vertex, vertex-only, leading and non-leading `SEARCH ... WHERE` with one to eight `AND`-connected equalities on distinct properties, one numeric range, a two-sided numeric range, mixed equality-plus-one-sided-range, mixed equality-plus-two-sided-range, same-property equality disjunctions, cross-property pure equality disjunctions (where property names may repeat or differ), same-property pure numeric-range disjunctions, and cross-property pure numeric-range disjunctions, all backed by active vertex property indexes; `DISTANCE AS` and `SCORE AS` for cosine |
| 5     | Add result hydration from vector hits to graph vertex bindings                                         | Implemented via row-shaped `SeedBindingsWire`                                                                        |
| 6     | Add `SCORE AS` / `DISTANCE AS` validation from vector-index metric definitions                         | Implemented: shape validated against metric; `SCORE AS` works for exact-scan `Cosine`, rejected for `L2Squared`      |
| 7     | Add scalar `INLINE` edge-property schema syntax, ordinary read access (`e.inline_field`), ordinary mutation packing (`INSERT`/`SET`/`REMOVE` semantics), and bounded ordinary `COST BY e.inline_field` shortest-path cost into the inline payload; add fixed-size struct inline schema registration and ordinary struct field reads (`e.stats`, `e.stats.field`) | Implemented: scalar inline schema registration, read access, mutation packing, and bounded `COST BY e.property` are complete; fixed-size struct schema registration and ordinary struct field reads are complete; struct mutation packing, `COST BY` over a struct field, property indexes on inline struct fields, nested structs, and generic `CREATE GRAPH TYPE` `INLINE` annotations remain planned |
| 8     | Deprecate daily-query use of `GLEAPH.WEIGHT` where ordinary inline property access is available        | Planned                                                                                                              |

Every stage that changes public syntax must update this document and add parser/planner/executor tests.

## Boundary rules

- Do not add IC canister calls, shard ids, stable-memory concepts, or vector-canister routing to
  `gleaph-gql`.
- Do not make `gleaph-gql-planner` depend on GraphStore, Router stable state, or vector-index canister
  clients.
- Router resolves vector-index names, embedding names, graph context, activation gates, and target
  canisters.
- Graph executes shard-local property access, inline payload decode, runtime functions, and
  shortest-path cost evaluation.
- Vector Index executes ANN search and owns search/rebuild/maintenance internals.

## Related documents

- [GQL stack layers](layers.md)
- [Vector index](../index/vector-index.md)
- [Derived-state query semantics](../index/derived-state-query-semantics.md)
- [ADR 0031: Vertex embedding store and derived vector index canister](../adr/0031-vertex-embedding-store-and-derived-vector-index.md)
- [ADR 0034: Gleaph GQL extension syntax surface](../adr/0034-gleaph-gql-extension-syntax.md)
