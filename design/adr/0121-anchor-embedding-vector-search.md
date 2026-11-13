# ADR 0121: Anchor embeddings as a GQL vector-search source

- **Status:** accepted (2026-11-13; parameter and bound `title_ja`/`title_en` anchors implemented; index-backed anchor prefixes use Router seed resolution)
- **Date:** 2026-11-13
- **Supersedes:** the anchor-film/client-supplied-vector half of ADR 0119
- **Related:** ADR 0034 (SEARCH), ADR 0064 (vector-canister ownership), ADR 0074 (prepared-query publication), ADR 0122 (movie demo embedding model)

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
RETURN f.title_ja AS title, f.year AS year, similarity
ORDER BY similarity DESC
```

For the parameter form, `$anchor` holds the anchor's 8-byte encoded `ELEMENT_ID`
(`Value::Bytes`). For the bound form, the preceding `MATCH` binds the anchor and the Router resolves
its `ELEMENT_ID` by executing the prefix. Either way the Router decodes the id to a `VectorSubject`
and calls the Vector canister's Router-only `vector_embedding(index_id, subject)` query. The Vector
canister reads the live row through its subject map and page store and returns canonical
little-endian F32 bytes. The Router then executes the existing vector search against the same index.

Both the parameter form and a MATCH-bound anchor are implemented. The bound form resolves the
anchor predicate through the Router's generic `SeedAnchorSet` path (any active vertex property index
the planner lowers to a seedable `IndexScan`), evaluates the anchor prefix through the Graph under
the caller's normal row authorization, projects `ELEMENT_ID(anchor)`, and then resolves the stored
embedding from the Vector canister. The `title_ja` and `title_en` indexes are demo instances of
the same path, not special cases. The
supported prefix is exactly one leading anchor scan followed by the searched binding's scan.
Index-backed anchor predicates are resolved by the Router's existing seed-anchor path before the
Graph prefix dispatch; the search-only tail drops the already-resolved anchor binding. Multiple
anchors and prefix shapes with additional bindings are rejected explicitly and deferred. Direct
`anchor.property` expressions in the query's `RETURN` are resolved in the authorized prefix and
injected into the search-only tail as literals; other anchor references remain fail-closed.

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

The movie demo publishes `films-similar-to-ja` with a bound title_ja anchor:

```gql
MATCH (anchor:Film { title_ja: $anchor })
MATCH (f:Film)
SEARCH f IN (
  VECTOR INDEX film_embedding
  FOR anchor.embedding
  LIMIT 8
) SCORE AS similarity
RETURN f.title_ja AS title, f.year AS year, similarity
ORDER BY similarity DESC
```

`gleaph prepared run films-similar-to-ja --param 'anchor="千と千尋の神隠し"'` executes it; the sibling
`films-similar-to-en` binds the same anchor slot through `title_en` for English titles.
 The
`ask` surface binds the same title parameter from the question (for example
`千と千尋の神隠しに似ている映画は？`). `gleaph gql --param-bytes` still exposes the parameter form
for ad-hoc anchor-id queries.

## Consequences

- Browser clients no longer need an embedding model or embedding asset for similar-films; a
  MATCH-bound anchor lets them send a film title instead of a vector.
- Anchor and indexed vectors cannot drift: both come from the same index generation.
- The GQL surface grows by one source form, not by a new Router/SDK function.
- The bound anchor prefix uses Router-side seed resolution for index-backed predicates; the
  search-only tail drops the anchor binding (direct anchor properties in `RETURN` are injected as
  literals from the authorized prefix), so the Graph never sees an IndexScan without an index
  client.
- Multiple anchors and an automatic self-exclusion policy remain open. The current demo output includes the anchor itself, matching Neo4j's behavior; callers can
  filter it in their own projection.
- `films-similar-to-ja` is `ask`-executable when the question names a film title; questions that
  only say "this film" have no anchor and fail closed as `needs input`.
- The local `gleaph ask` slot extractor uses `scripts/gen-film-entities.py` to derive the film
  title vocabulary from the graph catalog rather than keeping a second static title list.
