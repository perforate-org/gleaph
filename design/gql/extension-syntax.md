# Gleaph GQL extension syntax

Last updated: 2026-06-26
Anchor timestamp: 2026-06-26 06:32:22 UTC +0000

## Status

**Dialect contract with a canonical Rust manifest and partially implemented pieces.** This document
is the steady-state public syntax contract for Gleaph-specific GQL extensions. It complements:

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

| Class                         | Public shape                                                              | Status                                                                                                                                                          | Owner of meaning                                                     |
| ----------------------------- | ------------------------------------------------------------------------- | --------------------------------------------------------------------------------------------------------------------------------------------------------------- | -------------------------------------------------------------------- |
| IC value type                 | `IC.PRINCIPAL`                                                            | Implemented                                                                                                                                                     | `gleaph-gql-ic` value extension                                      |
| IC runtime function           | `MSG_CALLER()`                                                            | Implemented                                                                                                                                                     | Graph execution context                                              |
| Edge inline value             | `e.distance`, `e.stats.score` with `INLINE` schema modifier               | Planned target                                                                                                                                                  | Router schema/catalog + Graph edge payload execution                 |
| Shortest-path cost            | `COST BY e.distance`                                                      | Planned target                                                                                                                                                  | Graph query planner/executor                                         |
| Current edge weight function  | `GLEAPH.WEIGHT(e)`                                                        | Implemented compatibility surface                                                                                                                               | Graph query executor                                                 |
| Edge insertion-order sequence | `GLEAPH.SEQUENCE(e)`                                                      | Implemented compatibility surface                                                                                                                               | Graph edge storage/execution                                         |
| Edge-payload vector predicate | `GLEAPH.VECTOR.L2_SQUARED(e, $q) <= threshold`                            | Implemented compatibility surface                                                                                                                               | Planner fusion + Graph edge payload executor                         |
| Vertex vector search          | `MATCH ... SEARCH d IN (VECTOR INDEX ... FOR ... LIMIT ...) SCORE AS ...` | Implemented for leading `DISTANCE AS` and `SCORE AS` on exact-scan cosine indexes; `SCORE AS` rejected for distance-only metrics; non-leading `SEARCH`, edge subjects, and `WHERE` remain planned | Router vector-index catalog + vector canister + Graph seed hydration |
| Operational procedures        | `CALL GLEAPH.FINALIZE_*`, `CALL GLEAPH.DRAIN_DEFERRED_MAINTENANCE()`      | Implemented                                                                                                                                                     | Graph mutation executor / Router orchestration                       |

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
  stats STRUCT {
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
  stats STRUCT {
    score FLOAT32,
    confidence FLOAT32
  } INLINE
}
```

Inline structs are restricted to fixed-size fields. Variable-length strings, blobs, arrays, and
embeddings are not inline by default.

### Relationship to `GLEAPH.WEIGHT`

`GLEAPH.WEIGHT(e)` is the implemented compatibility surface for fixed-width edge payload weights.
The target dialect replaces it with ordinary property access:

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

**Status:** Parser and planner representation implemented. Router lowering to the existing vector search API is implemented for the narrow accepted shape: a leading `NodeScan(variable = d, label: optional)` immediately followed by `PlanOp::Search { binding = d, provider: VectorIndex, output: DISTANCE AS alias or SCORE AS alias }`, vertex-only, no `WHERE`. `SCORE AS` is accepted only for indexes whose metric exposes a score (currently exact-scan `Cosine`, `nlist == 1`); it is rejected for distance-only metrics such as `L2Squared`. Unsupported shapes (non-leading `SEARCH`, edge subjects, `WHERE` filtering, multi-graph `SEARCH`, or any mutation tail) fail closed with an explicit `InvalidArgument` error.

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

Initial implementation should start with the simplest shape:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
```

More complex patterns can be staged after the planner can reason about the interaction between vector
candidate generation, post-filtering, and traversal:

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

`SEARCH ... WHERE` is parsed but rejected at planning time in this slice. It is reserved for future in-index filtering:

```gql
MATCH (d:Document)
  SEARCH d IN (
    VECTOR INDEX document_embedding
    FOR $query
    WHERE d.published_at >= $cutoff
    LIMIT 100
  ) SCORE AS similarity
RETURN d, similarity
```

When implemented, the predicate subset must be explicit and index-owned; unsupported filters must fail closed rather than silently becoming post-filters.
Initial implementation may reject this subclause. When implemented, the predicate subset must be
explicit and index-owned; unsupported filters must fail closed rather than silently becoming
post-filters.

### Internal lowering

Slice 3 lowers a leading `NodeScan(variable = d, label: optional)` followed by `PlanOp::Search`
to the existing Router/vector-index API and then dispatches the remaining graph-tail plan from
row-shaped vector-search seeds. The Router:

1. Resolves the embedding name from `VECTOR INDEX <name>` against the Router catalog.
2. Evaluates `FOR $query` and `LIMIT n` from literals or parameters.
3. Calls the existing `router.vector_search(...)` to obtain hits.
4. Builds per-shard row-shaped seeds carrying the matched vertex binding and the
   `DISTANCE AS` scalar alias.
5. Strips the `NodeScan + Search` prefix and dispatches the tail plan to graph shards with the
   row-shaped seeds.

For this slice the accepted shape is intentionally narrow:

- vertex-only (`d` must be a vertex binding);
- leading `NodeScan + Search` only;
- `DISTANCE AS` accepted for all metrics;
- `SCORE AS` rejected when the metric has no natural score (e.g. `L2Squared`);
- no `WHERE` in-index filtering;
- no edge subjects;
- no multi-graph `SEARCH`.

A raw `PlanOp::Search` that reaches the graph executor fails closed with an explicit error.

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
  evidence STRUCT {
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

| Stage | Scope                                                                                                  | Status                                                                                                        |
| ----- | ------------------------------------------------------------------------------------------------------ | ------------------------------------------------------------------------------------------------------------- |
| 1     | Document the dialect contract and keep existing behavior unchanged                                     | Implemented                                                                                                   |
| 2     | Add the Rust extension manifest for canonical extension names, classes, status, owner, and doc anchors | Implemented                                                                                                   |
| 3     | Add `SEARCH` parser/planner representation without backend-specific storage details                    | Implemented                                                                                                   |
| 4     | Add Router lowering from vector `SEARCH` to the existing vector search API                             | Implemented for leading `NodeScan + Search` prefix, vertex-only, no `WHERE`, `DISTANCE AS` and `SCORE AS` for cosine |
| 5     | Add result hydration from vector hits to graph vertex bindings                                         | Implemented via row-shaped `SeedBindingsWire`                                                                 |
| 6     | Add `SCORE AS` / `DISTANCE AS` validation from vector-index metric definitions                         | Implemented: shape validated against metric; `SCORE AS` works for exact-scan `Cosine`, rejected for `L2Squared` |
| 7     | Add inline edge property schema syntax and lower `e.inline_field` to existing edge-payload reads       | Planned                                                                                                       |
| 8     | Deprecate daily-query use of `GLEAPH.WEIGHT` where ordinary inline property access is available        | Planned                                                                                                       |

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
