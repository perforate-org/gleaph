# Movie Graph Demo (public experience-demo dataset)

Status: implemented (data slice, 2026-11-12). The browser page and the public deployment are later
slices; this document covers the dataset, the graph contract and the prepared catalog.

## Purpose

A general-audience demo dataset for the "experience demo" page: visitors ask in natural language or
by parameters and watch Gleaph's prepared queries answer over films. The dataset is deliberately
broad (2,000 films) so the visitor recognises titles, and shallow (seven query shapes) so the
authorization and routing surfaces stay small.

Non-goals: TV series and OVA, posters/images, ratings/revenue, any write surface.

## Graph contract

Source of truth: `demo/movies/migrations/` and `demo/movies/seeds/`.

```gql
CREATE GRAPH TYPE MovieGraph {
  NODE Film  { title_ja, title_en, year INT32, synopsis, search_text, qid, embedding BYTES, staff_note }
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

Indexes: `film_title_ja`, `film_title_en`, `film_year` (property); `film_embedding` (vector, 384 dimensions, cosine,
i8/ivf_flat); `film_search` (text on `search_text`, `multilingual`).

## Prepared catalog

| Operation | Shape |
| --- | --- |
| `films-by-conditions` | year range × genre (`g.name CONTAINS $genre`), oldest first |
| `synopsis-search` | `text_score` ranking over `search_text` (Japanese + English intros) |
| `films-by-embedding` | vector similarity over synopsis embeddings (`$embedding` is `Value::Bytes`) |
| `films-similar-to-ja` | vector similarity from a `title_ja` anchor |
| `films-similar-to-en` | vector similarity from a `title_en` anchor |
| `common-cast` | people credited in both of two films |
| `sequel-chain` | `SEQUEL_OF` reach in 1..4 hops |

All seven are published `TO PUBLIC` (`demo/movies/grants/movies.gql`), together with
`films-by-imprint`, `films-by-same-magazine` and the two title resolvers. The same file grants
`PUBLIC` `MATCH`, `TRAVERSE` and `READ` on every label and every property except `Film.staff_note`,
so an anonymous browser principal can also run read-only ad-hoc GQL over the public data.

`Film.staff_note` is a private property for the authorization demo: five films carry a synthetic
operator note (`demo/movies/seeds/staff-notes.json`) and no grant names the property, so a plan that
names it is refused for an anonymous caller. Observed on the local network on 2026-11-14: projecting,
filtering on, ordering by or aggregating `f.staff_note` returns `Forbidden`. `RETURN f` (the whole
vertex) first returned the note; with the whole-value rule of the ADR 0074 amendment of 2026-11-14
it is refused as well, because a whole `Film` demands READ on every declared property. That fix is
in progress as GAP-2026-11-14-001 in [implementation-gaps.md](../implementation-gaps.md).

## Natural-language routing

Routing is ADR 0114's schema-conditioned route (ADR 0116 moved it to a composite query; ADR 0118
renamed the public surface). `interpret` returns the operation plus typed parameters; `ask`
additionally executes the selected read-only prepared query. The Router builds one context per
request from this graph's schema and the caller-visible prepared catalog.

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

Facts come from Wikidata (CC0) with per-row QIDs; `synopsis` is the Japanese Wikipedia article intro
and the catalog also stores an English intro (`synopsisEn`) with per-row `sourceUrlEn`. Both are
CC BY-SA 4.0. `search_text` is the derived Japanese+English projection indexed by `film_search`. `demo/movies/seeds/NOTICE.md` carries the attribution;
no IMDb/TMDB data and no images are used.

Selection: per release year 1950–2025, films (`wdt:P31/wdt:P279* wd:Q11424`, so anime films are
included) ranked by `wikibase:sitelinks`, top 30 plus a Japanese-film quota, unioned with franchise
seeds (Studio Ghibli, David Lynch, Weekly Shōnen Jump adaptations) and trimmed to 2,000. Candidate
queries run against QLever's Wikidata index because the public WDQS endpoint times out.

## Limits

- `films-by-embedding` takes a `Value::Bytes` parameter. The CLI renders it with
  `prepared run --param-bytes NAME=BASE64`; `scripts/semantic-search.sh` encodes a topic with the
  model pinned by `seeds/embeddings.provenance.json` (currently Bekko a8m). `films-similar-to-ja`
  binds an anchor with `MATCH (anchor:Film { title_ja: $anchor })`; the Router seed-resolves the title
  index and the Vector canister reads the stored anchor embedding, so similar films needs no
  client-side model or embedding asset. The canister owns only the vector
  search. `films-by-conditions` and `synopsis-search` can be routed and executed with `gleaph ask`
  (graph-backed; executes by default). `common-cast` and `sequel-chain` still need their film titles
  supplied by a picker/vocabulary.
- The popularity sample favours well-known films; obscure titles are absent by construction.
- Variable-length queries with a labelled destination required a graph-side fix
  (`var_len_destination_label_filter_sees_the_vertex_binding`) before `sequel-chain` returned rows.
- Re-registering a prepared operation drops its grants; apply `gleaph grants apply` afterwards.
- The router extracts decades and ASCII genres, but not kana-only genres or film titles, so questions
  whose only parameter is a title abstain; the page supplies titles through a picker.

## Evidence

Local bring-up on 2026-11-12 (icp-cli managed network): 8,563 vertices / 18,293 edges loaded, 2,000
embeddings applied, `film_search` `ndocs=2000`, four CLI-executable ops returning the expected rows
(`sequel-chain` 2 rows, `common-cast` 2 rows, `films-by-conditions` 12 rows, `synopsis-search` 10
rows), and the routing baseline recorded in `demo/movies/README.md`.
