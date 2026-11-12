# Movie Graph Demo (public experience-demo dataset)

Status: implemented (data slice, 2026-11-12). The browser page and the public deployment are later
slices; this document covers the dataset, the graph contract and the prepared catalog.

## Purpose

A general-audience demo dataset for the "experience demo" page: visitors ask in natural language or
by parameters and watch Gleaph's prepared queries answer over films. The dataset is deliberately
broad (2,000 films) so the visitor recognises titles, and shallow (five query shapes) so the
authorization and routing surfaces stay small.

Non-goals: TV series and OVA, posters/images, ratings/revenue, any write surface.

## Graph contract

Source of truth: `demo/movies/migrations/` and `demo/movies/seeds/`.

```gql
CREATE GRAPH TYPE MovieGraph {
  NODE Film  { title, original_title, year INT32, synopsis, qid }
  NODE Person { name, qid }
  NODE Studio { name, qid }
  NODE Genre { name }
  DIRECTED EDGE Directed  LABEL DIRECTED   CONNECTING (Person -> Film)
  DIRECTED EDGE ActedIn   LABEL ACTED_IN   CONNECTING (Person -> Film)
  DIRECTED EDGE ProducedBy LABEL PRODUCED_BY CONNECTING (Film -> Studio)
  DIRECTED EDGE HasGenre  LABEL HAS_GENRE  CONNECTING (Film -> Genre)
  DIRECTED EDGE SequelOf  LABEL SEQUEL_OF  CONNECTING (Film -> Film)
}
```

`SEQUEL_OF` points from a film to the film that follows it (Wikidata `P156` is "followed by").

Indexes: `film_title`, `film_year` (property); `film_embedding` (vector, 384 dimensions, cosine,
i8/ivf_flat); `film_synopsis` (text, `unicode_bigram`).

## Prepared catalog

| Operation | Shape |
| --- | --- |
| `films-by-conditions` | year range × genre (`g.name CONTAINS $genre`), oldest first |
| `synopsis-search` | `text_score` ranking over synopses (declares `VALUE query STRING = $query`) |
| `similar-films` | vector similarity over synopsis embeddings (`$query` is `Value::Bytes`) |
| `common-cast` | people credited in both of two films |
| `sequel-chain` | `SEQUEL_OF` reach in 1..4 hops |

All five are published `TO PUBLIC` (`demo/movies/grants/movies.gql`) so an anonymous browser
principal can execute exactly these and nothing else.

## Natural-language routing

Routing is ADR 0114's general schema-conditioned route (ADR 0116 moved it to a composite query, so
it is free and needs no tokenizer or model pack). The Router builds one context per request from this
graph's schema and the caller-visible prepared catalog; the reply is one operation plus typed slots,
or an abstain.

Verified accents on 2026-11-12 (anonymous Router caller, local network):

- `1990年代のSF映画を教えて` -> `films-by-conditions` with `from=1990`, `genre=SF`, `to=1999`
- `宇宙が出てくる映画` -> `synopsis-search` with `query=宇宙`
- `今日の天気は？` -> abstain

Genre values are graph-scoped parameter domains (ADR 0117). The canonical values are the Japanese
`Genre.name` labels; `demo/movies/domains/genre-aliases.json` adds English surfaces that the
inference canister canonicalizes before execution, so `1970s drama` executes against `ドラマ映画`
rather than returning zero rows. The alias map is routing data supplied by the graph owner; it is
not embedded in the router or the inference model.

Routing slot extraction reads the prepared manifest, so a parameter must appear in a typed position
to exist at all. Inline pattern property maps (`{name: $genre}`) now enter the manifest through the
GQL type checker's constraint walk
(`crates/gql/src/type_check/constraint.rs::collect_from_inline_properties`); a parameter used only as
a function argument (here `text_score(..., $query)`) needs the explicit `VALUE <name> <TYPE> = $<name>`
declaration.

## Data provenance and licences

Facts come from Wikidata (CC0) with per-row QIDs; synopsis text is the Japanese Wikipedia article
intro (CC BY-SA 4.0) with per-row `sourceUrl`. `demo/movies/seeds/NOTICE.md` carries the attribution;
no IMDb/TMDB data and no images are used.

Selection: per release year 1950–2025, films (`wdt:P31/wdt:P279* wd:Q11424`, so anime films are
included) ranked by `wikibase:sitelinks`, top 30 plus a Japanese-film quota, unioned with franchise
seeds (Studio Ghibli, David Lynch, Weekly Shōnen Jump adaptations) and trimmed to 2,000. Candidate
queries run against QLever's Wikidata index because the public WDQS endpoint times out.

## Limits

- `similar-films` takes a `Value::Bytes` parameter that `gleaph prepared run --param` cannot express;
  the op is exercised from the browser, not the CLI. `films-by-conditions` and `synopsis-search`
  can be routed and executed from the CLI with `ask --plan --execute --entities entities.json`;
  `common-cast` and `sequel-chain` still need their film titles supplied by a picker/vocabulary.
- The popularity sample favours well-known films; obscure titles are absent by construction.
- Variable-length queries with a labelled destination required a graph-side fix
  (`var_len_destination_label_filter_sees_the_vertex_binding`) before `sequel-chain` returned rows.
- Re-registering a prepared operation drops its grants; apply `gleaph grants apply` afterwards.
- The router extracts decades and ASCII genres, but not kana-only genres or film titles, so questions
  whose only parameter is a title abstain; the page supplies titles through a picker.

## Evidence

Local bring-up on 2026-11-12 (icp-cli managed network): 8,563 vertices / 18,293 edges loaded, 2,000
embeddings applied, `film_synopsis` `ndocs=2000`, four CLI-executable ops returning the expected rows
(`sequel-chain` 2 rows, `common-cast` 2 rows, `films-by-conditions` 12 rows, `synopsis-search` 10
rows), and the routing baseline recorded in `demo/movies/README.md`.
