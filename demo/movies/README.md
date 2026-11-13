# Movie demo

A 2000-film graph for the public experience demo: films, the people credited in them, their
studios, genres and sequel chains, with six prepared queries that the page routes to.

## Dataset

- 2000 films selected by per-year popularity ranking (QLever's Wikidata index) plus franchise
  seeds: Studio Ghibli, David Lynch and Weekly Shōnen Jump adaptations.
- Facts (titles, years, credits, studios, genres, sequels) from **Wikidata (CC0)**; the synopsis
  text is the Japanese and English Wikipedia article intros (**CC BY-SA 4.0**), combined only in
  the derived `search_text` field. See [`seeds/NOTICE.md`](seeds/NOTICE.md). Every film row keeps
  its Wikidata QID and both article URLs.
- No images, and no IMDb/TMDB data.

| File | Contents |
| --- | --- |
| `seeds/catalog.json` | the curated catalog (one JSON row per film, including Japanese and English synopses) |
| `seeds/vertices.jsonl` / `seeds/edges.jsonl` | the bulk-load artifacts projected from it |
| `seeds/embeddings.jsonl` | one 384-dim vector per film, pinned by `seeds/embeddings.sha256` |
| `domains/genre-aliases.json` | English routing aliases for the Japanese `Genre.name` canonical values |
| `entities.json` | empty CLI entity vocabulary for lexical/interpret fallback on year/genre/text slots |

## Regenerating

```bash
node scripts/fetch-catalog.mjs      # network: QLever + Wikidata + ja.wikipedia (cached under .cache/)
python3 scripts/fetch-english-synopses.py  # network: en.wikipedia intros (cached under .cache/)
node scripts/build-seeds.mjs        # deterministic projection of the catalog
../../target/venv/bin/python scripts/gen-embeddings.py          # needs the pinned model
../../target/venv/bin/python scripts/gen-embeddings.py --check  # verify the pin only
```

Only `fetch-catalog.mjs`, `fetch-english-synopses.py`, and the embedding generator touch the
network; the load path and the tests read the committed files.

Tests (offline):

```bash
node --test scripts/build-seeds.test.mjs scripts/embeddings.test.mjs
python3 scripts/check-real-queries.py
python3 scripts/check-genre-aliases.py
python3 scripts/fetch-english-synopses.py --check
```

The real-query routing gate (real MovieLens user queries, CC BY 3.0) is documented in
[`eval/REAL-QUERIES.md`](eval/REAL-QUERIES.md).

## Local quickstart

Prerequisites: a built `cargo build -p gleaph-cli`, the `wasm32-unknown-unknown` target,
Docker/OrbStack (icp-cli managed network).

```bash
./scripts/quickstart.sh
```

The script is the annotated chain: identity, `gleaph network start`, `gleaph migration apply`,
`gleaph load`, `gleaph embed ingest`, `gleaph prepared apply`, `gleaph grants apply`, then the
index counters and the routing gates (`eval/routing.jsonl`, the Japanese genre gate, and the English
genre/topic gates).

## Prepared queries

| Op | Shape | Parameters |
| --- | --- | --- |
| `films-by-conditions` | year range × genre (`g.name CONTAINS $genre`), oldest first | `$from` `$to` `$genre` |
| `synopsis-search` | full-text ranking over bilingual `search_text` (`text_score`) | `$query` |
| `films-by-embedding` | vector similarity over synopsis embeddings | `$embedding` (`Value::Bytes`, 384 f32) |
| `films-similar-to` | vector similarity from a MATCH-bound anchor film's stored embedding | `$anchor` (title) |
| `common-cast` | people credited in both of two films | `$a` `$b` |
| `sequel-chain` | `SEQUEL_OF` reach in 1..4 hops, in release order | `$from` |

`grants/movies.gql` publishes all six to `PUBLIC`, so an anonymous browser visitor can execute
exactly these six and nothing else. Re-registering a prepared operation drops its grants: run
`gleaph grants apply` after every `gleaph prepared apply`.

`synopsis-search` declares its text parameter explicitly (`VALUE query STRING = $query`) because
`$query` appears only as a `text_score` argument; parameters used only in function arguments are not
inferred from the schema, so without the declaration the manifest carries no parameter and the
router cannot extract the topic.

## Local verification (2026-11-12)

Brought up on the icp-cli managed local network and loaded end to end:

| Step | Result |
| --- | --- |
| `gleaph network start` | platform release with 6 artifacts, Router auto-issued |
| `gleaph migration apply` | 5 migrations applied |
| `gleaph load` | 8,563 vertices, 18,293 edges |
| `gleaph embed ingest` | 2,000 embeddings applied, 0 failed |
| `gleaph text-index --graph movies --name film_search` | `Ready`, `ndocs=2000` |
| `films-by-conditions` | 12 rows (SF films 1990–1999) |
| `synopsis-search` | ranked Japanese and English topic rows (e.g. インデペンデンス・デイ, ジョーズ) |
| `common-cast` | 2 rows for 七人の侍 × 用心棒: 三船敏郎, 志村喬 |
| `sequel-chain` | 2 rows for ゴッドファーザー: PART II, PART III |

`films-similar-to` binds the anchor with `MATCH (anchor:Film { title: $anchor })`. The Router
executes that prefix under the caller's row authorization, asks the Vector canister for the
anchor's live embedding, and runs the existing vector search against the same index.
`scripts/similar-films.sh` sends the title directly, so similar films needs no client-side model or
embedding asset.

`films-by-embedding` remains the raw vector operation for semantic topic search. The CLI binds its
`Value::Bytes` parameter with `prepared run --param-bytes`; `scripts/semantic-search.sh` supplies a
query-text embedding. The browser still needs the embedding sidecar for that intent.

Variable-length paths needed a graph-side fix to get here: the destination filters were evaluated
after the destination had been projected into a record, so `(b:Film)`'s label predicate matched
nothing and every variable-length query with a labelled destination silently returned zero rows.
`var_len_destination_label_filter_sees_the_vertex_binding` (graph crate) pins the fix.

## Natural-language routing (2026-11-12)

`interpret(graph, text)` is a Router **composite query** that returns the chosen operation and typed
parameters. `ask(graph, text, read_mode)` interprets and then executes the selected read-only
prepared query, re-authorizing the caller on the execution path. Neither needs a tokenizer or model
pack; an anonymous caller reaches them on this public graph, so the page can call them directly.

```bash
gleaph interpret --graph movies --canister "$ROUTER" --network http://localhost:32768 \
  -q "1990年代のSF映画を教えて"
```

| Question | Route |
| --- | --- |
| 1990年代のSF映画を教えて | `films-by-conditions from=1990 genre=SF to=1999` |
| 宇宙が出てくる映画 | `synopsis-search query=宇宙` |
| films about time travel | `synopsis-search query=time travel` |
| 今日の天気は？ | abstain |

Enabling routing is one command (controller authority; the first call also provisions the deployment
inference canister):

```bash
gleaph config enable-ask --graph movies --canister "$ROUTER" --network local \
  --identity "$IDENTITY"
```

`ask` now carries the interpreted parameters into the prepared query for the non-vector movie
operations. These slots are years, genres and text, not catalog entities:

```bash
gleaph ask -q "1970s drama" --graph movies --canister "$ROUTER" --network local \
  --identity "$IDENTITY"
```

That prints the `films-by-conditions` answer table (12 rows) with `genre=ドラマ映画`; `--json` returns the full `Answer` envelope including decoded rows.

The routed real-query gate (58 MovieLens-derived queries, `eval/real-queries-2026-11-12.jsonl`)
scores 53/58 (91.4%) at the default profile; the frozen English holdouts score 26/28 and 16/22. The
12-item English genre gate (`eval/english-genre-2026-11-13.jsonl`) routes 12/12 with the alias
domain. See [eval/REAL-QUERIES.md](eval/REAL-QUERIES.md) for the dated runs.

Parameters with a closed value set declare a **domain** (ADR 0117). The router stores it per graph
and the extractor matches it, so a Japanese genre question resolves to the canonical label:

```bash
gleaph config parameter-values --graph movies --canister "$ROUTER" --network local --identity "$IDENTITY" \
  --param genre --from-label Genre --property name --limit 64 \
  --aliases domains/genre-aliases.json
```

The canonical values are the Japanese graph labels. `domains/genre-aliases.json` adds English
surfaces (`drama`, `horror`, `sci-fi`, ...) that canonicalize to those labels before execution, so an
English request such as `1970s drama` filters on `ドラマ映画` instead of returning zero rows. With
the domain and aliases the 12-question Japanese genre gate goes from 1/12 to 12/12; the English gate
is re-measured separately because English genre surfaces now become executable. See
[eval/REAL-QUERIES.md](eval/REAL-QUERIES.md) for the threshold sweep.

One extraction limit belongs to the router, not this dataset: film titles are not extracted, so a
question whose only parameter is a title abstains (its operation is still selected and marked
incomplete). The page therefore offers the title as a picker. Kana genres are covered by the domain
above; English genre words are covered by `domains/genre-aliases.json`. Other languages need their
own alias map; aliases are graph routing data, not code.

## Routing baseline (2026-11-12)

```bash
gleaph ask --dir prepared --eval eval/routing.jsonl
```

On the checked-in 28-item set (20 in-catalog questions, 8 out of scope): top-1 18/20 (90.0%),
abstain recall 5/8 (62.5%), overall 23/28 (82.1%). The set is small and self-authored; the useful
signal is that the six descriptions are separable, not that these numbers generalise.

## Known limits

- `films-by-embedding` takes a `Value::Bytes` parameter. `semantic-search.sh` resolves the
  query-text embedding on the client; the browser still needs the embedding sidecar for semantic
  topic search. `films-similar-to` has no client embedding dependency: it takes an anchor
  title, binds it with MATCH, and resolves the stored vector in the Vector canister.
- The bound anchor form supports a title predicate and one searched binding. The Router resolves
  the title IndexScan through its seed-anchor path before the Graph prefix dispatch; the
  search-only tail drops the anchor binding. Multiple anchors and additional prefix bindings remain
  deferred. The parameter `$anchor.embedding` form remains available.
- `gleaph ask --calibrate` needs `debug_interpret_batch`, whose authority is the graph
  tenant or a controller; the quickstart's anonymous session cannot run it, so the numbers above
  come from the lexical baseline, not from the Router.
- TV series and OVA are out of scope: the graph models films only.
- `gen-embeddings.py` is provenance-pinned, not byte-reproducible: the committed JSONL is the
  source of truth and the test pins its sha256.

## Embedding contract

`seeds/embeddings.provenance.json` is the source of truth for the embedding space: model and
revision, dimensions, query/passage prefixes, pooling, normalization, similarity and input limit.
`scripts/embedding_contract.py` validates it and the committed `seeds/embeddings.jsonl`; generation and
query encoding fail before touching the Router on a mismatch.

```bash
target/venv/bin/python scripts/embedding_contract.py --check
```

This is also the boundary the SDKs expose as an optional `EmbeddingProvider` (ADR 0120): the core
SDK never downloads or bundles a model.

## Embedding relevance gate

`eval/relevance-2026-11-13.jsonl` and `scripts/eval-embeddings.py` compare embedding models on
the movie corpus. This is separate from the routing gates: it measures whether the retrieved films
are relevant, not whether `interpret` chose the right operation.

```bash
../../target/venv/bin/python scripts/eval-embeddings.py   --models e5-small,bekko-a8m,granite-97m   --output eval/embedding-eval-2026-11-13.json
```

The starter result (23 primary retrieval queries, curated labels) is in
[eval/EMBEDDING-EVAL.md](eval/EMBEDDING-EVAL.md). Bekko a8m leads this starter set; its ONNX export
matches the PyTorch vectors and the browser probe lives at
[browser/bekko-probe.html](browser/bekko-probe.html). The demo's committed embeddings are currently
Bekko; E5 remains the baseline in the evaluation record.

## Vector search intents

Similar films and semantic topic search are separate intents with different client dependencies.

**Similar films by title** uses `films-similar-to`:

```bash
./scripts/similar-films.sh "千と千尋の神隠し"
```

The script sends only the title to `films-similar-to`. The Router resolves the bound anchor's
stored embedding from the same index generation, so no model or embedding asset runs on the client.
The current output includes the anchor itself (Neo4j's `SEARCH` behaves the same way); callers that
do not want it can filter the projection.

A title-bearing natural-language question executes the same prepared query through `ask`:

```bash
../../target/debug/gleaph ask -q "千と千尋の神隠しに似ている映画は？" \
  --graph movies --canister "$ROUTER" --network "$NETWORK" --identity "$IDENTITY" --fetch-root-key
```

Without `--graph`, the local slot extractor needs the derived film vocabulary:

```bash
../../target/venv/bin/python scripts/gen-film-entities.py
../../target/debug/gleaph ask -q "千と千尋の神隠しに似ている映画は？" \
  --entities domains/film-entities.json
```

**Semantic topic search with a client embedding** uses `films-by-embedding`:

```bash
./scripts/semantic-search.sh "films about time travel"
# or two steps:
VECTOR=$(../../target/venv/bin/python scripts/embed-query.py "sharks" 2>/dev/null)
../../target/debug/gleaph prepared run films-by-embedding --param-bytes "embedding=$VECTOR" ...
```

The model is pinned in `seeds/embeddings.provenance.json` (currently
`hotchpotch/bekko-embedding-v1-a8m`); `embed-query.py` applies the contract's prefix, mean pooling,
and normalization. It can also serve `POST /embed` for a browser or another client. This is semantic
retrieval, not Router text routing: the caller chooses the vector operation and supplies the vector,
so no model runs in a canister or in a query.
