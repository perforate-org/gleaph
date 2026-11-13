# ADR 0121: Anchor embeddings as a GQL vector-search source

- **Status:** accepted (2026-11-13; parameter-anchor path implemented)
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

`$anchor` is a parameter holding the anchor's 8-byte encoded `ELEMENT_ID` (`Value::Bytes`). The
Router decodes it to a `VectorSubject` and calls the Vector canister's Router-only
`vector_embedding(index_id, subject)` query. The Vector canister reads the live row through its
subject map and page store and returns canonical little-endian F32 bytes. The Router then executes
the existing vector search against the same index.

The parameter-only form is implemented. A MATCH-bound anchor variable (`MATCH (a:Film) ...
FOR a.embedding`) is rejected with an explicit error until prefix evaluation can resolve the anchor
before the search dispatch. This keeps the first slice free of a second execution path.

### Ownership and authorization

- The Vector canister remains the only owner of the anchor bytes; the Router never receives a
  client-supplied anchor vector for this form.
- The search results still pass through the normal Graph/Router row authorization.
- The anchor lookup itself is not yet gated by a Graph read check. A caller who knows an opaque
  encoded id can use it as an anchor; the response only exposes rows the caller may already read.
  Adding the anchor-read gate belongs with the MATCH-bound form, where the anchor is a real row in
  the query plan.
- Missing, deleted, or stale anchors fail closed (`InvalidArgument`); they do not produce bytes or
  an empty-but-successful search.

### Neo4j alignment

Neo4j's `SEARCH` clause accepts an arbitrary vector expression, so `FOR snowWhite.embedding` works
because Neo4j stores embeddings as node properties. Gleaph keeps the same source-expression shape
but resolves the virtual `embedding` field through the Vector canister instead of Graph state.
This is why no `similar()` function or procedure is added.

### Prepared query

The movie demo publishes `films-similar-to`:

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

`gleaph prepared run films-similar-to --param-bytes "anchor=<base64 ELEMENT_ID>"` executes it.
`gleaph gql --param-bytes` supports the same source for ad-hoc queries.

## Consequences

- Browser clients no longer need an embedding model or embedding asset for similar-films; they need
  the anchor's encoded element id only.
- Anchor and indexed vectors cannot drift: both come from the same index generation.
- The GQL surface grows by one source form, not by a new Router/SDK function.
- The client must obtain `ELEMENT_ID(f)` first (for example through an existing GQL query).
- The MATCH-bound form, anchor-read authorization, and an automatic self-exclusion policy remain
  open. The current demo output includes the anchor itself, matching Neo4j's behavior; callers can
  filter it in their own projection.
- `films-similar-to` is not an `ask`-executable operation yet: natural-language `ask` cannot supply
  an anchor parameter without entity resolution.
