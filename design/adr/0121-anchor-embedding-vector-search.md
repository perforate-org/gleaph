# ADR 0121: Anchor embeddings as a GQL vector-search source

- **Status:** accepted (2026-11-13; parameter and bound non-indexed anchor paths implemented)
- **Date:** 2026-11-13
- **Supersedes:** the anchor-film/client-supplied-vector half of ADR 0119
- **Related:** ADR 0034 (SEARCH), ADR 0064 (vector-canister ownership), ADR 0074 (prepared-query publication)

## Context

A similarity query needs an anchor vector. The first movie-demo implementation resolved the anchor
film in the client, read its stored vector from `seeds/embeddings.jsonl`, and bound those bytes to
`films-by-embedding`. That works for a CLI with the seed file, but not for a browser: the client
would need the whole embedding asset, and any stale copy could drift from the indexed vector space.

ADR 0064 makes the vector canister the sole owner of indexed embedding bytes and keeps the Graph
free of embedding state. A `similar()` Router function would duplicate the GQL `SEARCH` operator,
its authorization, and its prepared-query surface. The language already has a vector-search source
expression; the missing capability is a source that names an indexed subject instead of a
client-supplied vector.

## Decision

Extend the `SEARCH ... FOR` source with an **anchor embedding** expression:

```gql
MATCH (f:Film)
SEARCH f IN (
  VECTOR INDEX film_embedding
  FOR $anchor.embedding
  LIMIT 8
) SCORE AS similarity
RETURN f.title AS title, f.year AS year, similarity
ORDER BY similarity DESC
```

For the parameter form, `$anchor` holds the anchor's 8-byte encoded `ELEMENT_ID`
(`Value::Bytes`). For the bound form, the preceding `MATCH` binds the anchor and the Router resolves
its `ELEMENT_ID` by executing the prefix. Either way the Router decodes the id to a `VectorSubject`
and calls the Vector canister's Router-only `vector_embedding(index_id, subject)` query. The Vector
canister reads the live row through its subject map and page store and returns canonical
little-endian F32 bytes. The Router then executes the existing vector search against the same index.

Both the parameter form and a MATCH-bound anchor are implemented. The bound form evaluates the
preceding single-anchor prefix through the Graph under the caller's normal row authorization,
projects `ELEMENT_ID(anchor)`, and then resolves the stored embedding from the Vector canister. The
supported prefix is exactly one leading anchor scan (a non-indexed predicate such as `qid`) followed
by the searched binding's scan. Index-backed anchor predicates, multiple anchors, and prefix shapes
with additional bindings are rejected explicitly and deferred.

### Ownership and authorization

- The Vector canister remains the only owner of the anchor bytes; the Router never receives a
  client-supplied anchor vector for this form.
- The search results still pass through the normal Graph/Router row authorization.
- The bound form evaluates the anchor prefix through the Graph, so the anchor must be row-readable
  by the caller before its embedding is used. The parameter form still accepts an opaque anchor id
  without a Graph read check; prefer the bound form when the anchor is a graph row.
- Missing, deleted, or stale anchors fail closed (`InvalidArgument`); they do not produce bytes or
  an empty-but-successful search.

### Neo4j alignment

Neo4j's `SEARCH` clause accepts an arbitrary vector expression, so `FOR snowWhite.embedding` works
because Neo4j stores embeddings as node properties. Gleaph keeps the same source-expression shape
but resolves the virtual `embedding` field through the Vector canister instead of Graph state.
This is why no `similar()` function or procedure is added.

### Prepared query

The movie demo publishes `films-similar-to` with a bound anchor:

```gql
MATCH (anchor:Film { qid: $anchor_qid })
MATCH (f:Film)
SEARCH f IN (
  VECTOR INDEX film_embedding
  FOR anchor.embedding
  LIMIT 8
) SCORE AS similarity
RETURN f.title AS title, f.year AS year, similarity
ORDER BY similarity DESC
```

`gleaph prepared run films-similar-to --param 'anchor_qid="Q155653"'` executes it; the demo script
resolves a title to its qid first. `gleaph gql --param-bytes` still exposes the parameter form for
ad-hoc anchor-id queries.

## Consequences

- Browser clients no longer need an embedding model or embedding asset for similar-films; a
  MATCH-bound anchor lets them send a qid/title instead of a vector.
- Anchor and indexed vectors cannot drift: both come from the same index generation.
- The GQL surface grows by one source form, not by a new Router/SDK function.
- The client must resolve a title to a qid for the demo prepared query (one existing GQL query);
  title-bound anchors that use an index-backed predicate are deferred.
- Index-backed bound anchors, multiple anchors, and an automatic self-exclusion policy remain
  open. The current demo output includes the anchor itself, matching Neo4j's behavior; callers can
  filter it in their own projection.
- `films-similar-to` is not an `ask`-executable operation yet: natural-language `ask` cannot supply
  a qid parameter without film entity resolution.
