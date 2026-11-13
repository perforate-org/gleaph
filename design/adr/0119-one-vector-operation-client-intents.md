# 0119. One vector operation, client-owned search intents

Date: 2026-11-13
Status: accepted (2026-11-13), implemented for the movie demo catalog.
Last revised: 2026-11-13
Anchor timestamp: 2026-11-13 11:00:00 UTC +0900

## Context

The movie demo exposed the vector search as `similar-films($query: Value::Bytes)`. Two different
client intents used it:

- **similar films**: resolve a film title to its stored synopsis embedding and search for nearby
  films;
- **semantic topic search**: embed free text with the pinned multilingual E5 model and search for
  nearby synopses.

The canister only executes a vector search. It cannot tell where the embedding came from, and
`similar-films` named one of the two intents rather than the operation's actual contract. Reusing
the name for topic search made the prepared catalog lie about what the operation does.

## Decision

1. **The prepared operation is a vector primitive, not an intent.** It is named
   `films-by-embedding` and its parameter is `$embedding` (`Value::Bytes`, 384 little-endian f32).
   Its description says that clients supply either an anchor-film embedding or a query-text
   embedding.
2. **The two intents live in the client.**
   - `scripts/embed-film.py` resolves a film title to its stored passage embedding and
     `scripts/similar-films.sh` binds it to `films-by-embedding`.
   - `scripts/embed-query.py` encodes topic text with the pinned `intfloat/multilingual-e5-small`
     model (`query: ` prefix) and `scripts/semantic-search.sh` binds it to the same operation.
3. **Do not duplicate the prepared query for the two intents.** The query body, vector index,
   authorization grant and execution limits are one contract. Two catalog entries with the same
   body would drift; the intent difference is client-side provenance, not canister behavior.
4. **`synopsis-search` remains the lexical text operation.** `ask` can interpret and execute it
   because its parameter is text. `films-by-embedding` cannot be executed by `ask` unless the
   client supplies the bytes; `interpret` may select it for an anchor-similarity question and
   return missing `embedding`, which is the correct public result.
5. **No model or tokenizer enters a canister.** The graph stores passage embeddings; the browser
   or CLI owns both the query embedding and the anchor-film embedding lookup.

## Consequences

- The prepared catalog names a contract (`by embedding`), while user-facing verbs
  (`similar films`, `semantic search`) remain in application code.
- A semantic topic query is not answered by `ask` alone; it is a client-composed retrieval call.
  This is unchanged in capability, but now explicit in the contract.
- The embedding index and grant are shared by both intents; authorization covers one operation.
- Renaming the operation requires a fresh prepared registration (pre-production; no alias).
- The demo's real-query labels use `films-by-embedding` for anchor-similarity routing. Existing
  dated measurements that say `similar-films` refer to the same historical operation.

## Evidence

- `demo/movies/prepared/films-by-embedding.gql` and `grants/movies.gql`.
- `demo/movies/scripts/embed-film.py` / `similar-films.sh` and
  `demo/movies/scripts/embed-query.py` / `semantic-search.sh`.
- `demo/movies/README.md` prepared-catalog and semantic-search sections.

## Amendment (2026-11-13)

ADR 0121 supersedes the anchor-film half of this decision. Anchor similarity now uses
`films-similar-to` and `SEARCH ... FOR $anchor.embedding`; the Vector canister reads the stored
anchor embedding, so `scripts/embed-film.py` was removed and `similar-films.sh` sends an
`ELEMENT_ID` instead of client-supplied bytes. The semantic-topic intent continues to use
`films-by-embedding` with a client-computed query embedding.
