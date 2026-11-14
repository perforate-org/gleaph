# ADR 0121: Anchor embeddings as a GQL vector-search source

- **Status:** accepted (2026-11-13; parameter and bound `title_ja`/`title_en` anchors implemented; index-backed anchor prefixes use Router seed resolution). Amended 2026-11-14: the anchor stays a binding after `SEARCH`.
- **Date:** 2026-11-13
- **Supersedes:** the anchor-film/client-supplied-vector half of ADR 0119
- **Related:** ADR 0034 (SEARCH), ADR 0064 (vector-canister ownership), ADR 0074 (prepared-query publication), ADR 0122 (movie demo embedding model)

## Amendment (2026-11-14) — the anchor stays a binding after SEARCH

Decided and implemented (`crates/router/src/gql_search.rs::rewrite_bound_anchor_plan`):

- The Router no longer drops the anchor prefix from the search dispatch. It resolves the anchor once
  to find the stored vector, as before, then dispatches the whole plan with two rewrites only: the
  `anchor.embedding` source becomes the resolved bytes literal, and the virtual `embedding` field
  leaves the anchor scan's property projection. Index-backed prefix scans are seeded by the generic
  dispatch, the same path the resolution pass already used.
- So the anchor and every variable its prefix binds are real bindings for the operators after
  `SEARCH`: `MATCH (anchor)<-[:DIRECTED]-(ad:Person)`, filters such as `p.name <> ad.name`, and any
  `anchor` expression in `RETURN`. The previous special case that turned direct `anchor.property`
  projections into literals, and its "anchor references only in RETURN" restriction, are deleted.
- The dispatch of the kept prefix depends on the ADR 0046 amendment of the same date: the
  index-anchored anchor is seeded and the searched label scan runs on the shard. That rule holds on
  single-shard graphs only, so on a graph with more than one live shard a bound-anchor search is
  still refused (the prefix falls back to Graph-local execution, which cannot run the anchor's
  `IndexScan`).
- Cost: the anchor prefix runs twice (resolution, then dispatch). For an index-backed anchor this is
  one extra seeded lookup; for an unindexed anchor predicate it is a second scan.

Why: before this change the contract below said prefix shapes with additional bindings "are rejected
explicitly", but the check only covered some operators. A query that matched from the anchor after
`SEARCH` reached the Graph without the anchor binding and failed there with `missing binding`, so the
rejection was not explicit and the shape the movie demo needs (similar films, excluding the anchor's
director) was unreachable. Keeping the prefix removes the restriction instead of completing it.

Still open: multiple anchors, and an automatic self-exclusion policy.

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
Graph prefix dispatch. (Superseded 2026-11-14, see the amendment above: the search dispatch now
keeps the prefix, so the anchor and its prefix bindings stay available after `SEARCH`. Originally
the search-only tail dropped the anchor binding, injected direct `anchor.property` projections as
literals and was meant to reject every other anchor reference.) Multiple anchors remain deferred.

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
- The bound anchor prefix uses Router-side seed resolution for index-backed predicates, both when
  resolving the vector and in the search dispatch, so the Graph never sees an IndexScan without an
  index client. (Until 2026-11-14 the search-only tail dropped the anchor binding; see the
  amendment.)
- Multiple anchors and an automatic self-exclusion policy remain open. The current demo output includes the anchor itself, matching Neo4j's behavior; callers can
  filter it in their own projection.
- `films-similar-to-ja` is `ask`-executable when the question names a film title; questions that
  only say "this film" have no anchor and fail closed as `needs input`.
- The local `gleaph ask` slot extractor uses `scripts/gen-film-entities.py` to derive the film
  title vocabulary from the graph catalog rather than keeping a second static title list.
