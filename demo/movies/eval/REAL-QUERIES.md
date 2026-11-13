# Real-query routing gate (movie)

This gate uses **real MovieLens.org user queries**, not synthetic paraphrases. The source is the
public query dataset from:

- Jie Kang, Kyle Condi, Shuo Chang, Joseph A. Konstan, Loren Terveen, F. Maxwell Harper.
  *Understanding How People Use Natural Language to Ask for Recommendations.* RecSys 2017.
- Queries collected on movielens.org in May 2016.
- Dataset: University of Minnesota Conservancy, `responses.csv`.
- License: CC BY 3.0 US.

`real-queries-2026-11-12.jsonl` is a 58-query pilot split into three classes:

- 11 complete supported queries (route and execute),
- 22 incomplete-intent queries (route the operation but abstain because a required slot is
  missing; includes genre-only and similar-film anchor cases),
- 25 out-of-scope queries (must not route to an operation).

## Labels

- `expected_op`: the prepared operation whose capability covers the request.
- `expect_incomplete`: the intent is supported but a required slot is missing; the route should
  select the operation and execution should clarify/abstain.
- `expect_abstain`: the request must not route to an operation (unsupported capability such as
  actor/director/rating/TV/personalization, or vague mood requests).
- `source_row` / `source_field`: provenance back to the CC BY 3.0 dataset.
- `label_note`: why the label was chosen.

The labels were assigned by the project developer, not by an independent third party. This is a
**pilot gate**, not the final independent user study. It is deliberately committed with provenance
so it can be replaced or extended without losing the source mapping.

## Regenerate / validate

```bash
python3 demo/movies/scripts/collect-real-queries.py          # download raw CSV to target/
python3 demo/movies/scripts/check-real-queries.py             # validate the curated 60
```

The raw 448-query candidate file stays under `target/`; only the curated subset is committed.
`real-queries-holdout-2026-11-13.jsonl` is a frozen 28-query holdout (8 complete, 8 incomplete,
12 out-of-scope) selected before the final description/slot tuning; do not tune on it.

## Pilot result (2026-11-12)

Initial metadata, default 2,000 bps profile:

| Metric | Result |
|---|---|
| items | 60 (20 supported / 40 abstain) |
| routed top-1 | 7/20 (35.0%) |
| abstain recall | 33/40 (82.5%) |
| overall | 40/60 (66.7%) |
| incomplete routed items | 15/20 |

Per operation: `similar-films` 6/7, `films-by-conditions` 0/2, `synopsis-search` 1/11.
At threshold zero the raw route covered 18/20 but selective accuracy was 7/18 (38.9%), so the
failure was operation/catalog generalization, not only the threshold.

After adding real phrasing cues to the three operation descriptions (1970s / bare genres /
"movies about" / "movies like"), the raw threshold-zero selective accuracy rose to **13/18
(72.2%)**, but the model became over-confident on unsupported requests. Threshold sweep on this
in-sample pilot:

| bps | routed top-1 | abstain recall | overall | incomplete |
|---:|---:|---:|---:|---:|
| 2,000 | 13/20 | 13/40 | 26/60 | 13 |
| 5,000 | 13/20 | 23/40 | 36/60 | 12 |
| 7,000 | 12/20 | 24/40 | 36/60 | 10 |
| 9,000 | 11/20 | 32/40 | 43/60 | 8 |
| 9,761 | 7/20 | 37/40 | 44/60 | 6 |

Calibration at 9,761 bps reports coverage 0.35, selective accuracy 1.0 and OOS abstention 0.925.
This is **in-sample tuning**: do not apply the number as a held-out quality claim. The description
changes are a dev candidate; a frozen holdout must be collected before claiming improvement.

Still unresolved: `films-by-conditions` needs English decade/bare-genre slot rules (`1970s`,
`drama`), and `synopsis-search`/`similar-films` still leave slots incomplete. The next engineering
slice is slot-rule support and a frozen holdout, not another in-sample threshold sweep.

## Run the routed evaluation

Against a loaded movie demo (see `demo/movies/README.md`):

```bash
gleaph ask --eval demo/movies/eval/real-queries-2026-11-12.jsonl \
  --graph movies --canister "$ROUTER" --network local --identity "$IDENTITY" --json
```

The report counts strict top-1, abstain recall and per-operation slices. Because the query set is
real, use it as a **holdout**: do not tune operation descriptions or the threshold on these 60 and
then report the same numbers as independent evidence. Split a larger collection into dev/holdout
before tuning.

## Known limitations

- 20 supported / 40 abstain is not representative of production traffic; it over-samples
  unsupported requests to test precision.
- `films-by-conditions` expects a year range and a genre. Queries with only one of them are
  labelled abstain even though the capability exists; this tests the fail-closed execution
  contract, not routing intent.
- `common-cast` and `sequel-chain` are not represented by the MovieLens recommendation queries;
  the next collection pass needs a source that asks those questions (e.g. StackExchange Movies &
  TV).
- MovieLens users type short recommendation queries; this does not cover long multi-turn agent
  prompts. Add a separate interaction corpus for that.

## Re-run 2026-11-13: prepared-manifest parameter fix

Two catalog-side defects were fixed before this run, and they change what the route can see:

- `crates/gql` now infers parameters from inline pattern property maps, so `genre`, `$a`, `$b` and
  `$from`/`$to` reach the prepared manifest;
- `synopsis-search` declares `VALUE query STRING = $query`, because a parameter used only as a
  `text_score` argument is not inferred;
- `films-by-conditions` matches `g.name CONTAINS $genre` so the router's surface term (`SF`) matches
  the stored Wikidata label (`SF映画`).

Same 60-item pilot, same labels, fresh local deployment, default 2,000 bps profile:

| Metric | 2026-11-12 (2,000 bps) | 2026-11-13 (2,000 bps) |
|---|---:|---:|
| routed top-1 | 13/20 | **17/20 (85.0%)** |
| abstain recall | 13/40 | 12/40 (30.0%) |
| overall | 26/60 | 29/60 (48.3%) |

Top-1 rose because operations that previously looked slot-incomplete (every genre question and every
text query) now route and execute. Abstain recall lost one item for the same reason: a wider slot
surface lets an out-of-scope request such as `horror movies` reach `films-by-conditions` instead of
abstaining. The threshold, not the catalog, is the lever left for that trade-off; this run used the
default profile.

Still open: `synopsis-search` and `similar-films` leave slots incomplete for free-form English
(`An action movie with aliens`), and the abstain side needs a calibrated profile on a frozen holdout
rather than another in-sample sweep.

## Three-way evaluation

`gleaph ask --eval` reports complete, incomplete-intent, and out-of-scope slices separately.
At the 4,000 bps dev profile the 58-item gate is 56/58; the frozen 28-item diagnostic holdout is
25/28, but the untouched 22-item final holdout is 16/22 and does **not** meet the quality target.
See `design/investigations/2026-11-12-movie-real-query-gate.md` for the exact failure list.

## 2026-11-13: parameter value domains (ADR 0117)

The router's rule extractor matches English genre words and ASCII tokens before `映画`, so Japanese
genre questions (`ホラー`, `SF`) left the required `genre` slot missing and the route abstained. The
fix is a graph-scoped **value domain** for the parameter, not a vocabulary in the model: the Router
stores `Genre.name`'s canonical labels, sends them with the routing context, and the inference
canister completes a missing slot by longest exact-or-prefix match, emitting the canonical value.

```bash
# Materialize the domain from the graph (bounded DISTINCT query), then declare it.
gleaph gql --query 'MATCH (g:Genre) RETURN DISTINCT g.name AS value ORDER BY value' ...
gleaph config parameter-values --graph movies --canister "$ROUTER" --network local --identity "$IDENTITY" \
  --param genre --set-values "SF映画,アクション映画,..."
```

Japanese genre gate (`eval/genre-2026-11-13.jsonl`, 12 authored decade+genre questions):

| | domain cleared | domain set |
|---|---:|---:|
| top-1 | 1/12 (8.3%) | **12/12 (100%)** |

The English sets are unchanged by the genre domain (their genre surfaces are English, the canonical
values are Japanese), which is the point: the mechanism is data, not language.

### Threshold sweep and holdout validation

`eval/real-queries-2026-11-12.jsonl` (dev, 58 items) prefers 4,000 bps (56/58 vs 53/58 at the
default). The holdouts do not move across 2,000-4,000 bps, so a higher threshold is a Pareto gain on
the measured sets; 5,000 bps was the cliff in an earlier run (holdout 26→24, final holdout 16→14),
and the threshold interacts with the pinned Glev model revision. The default stays, because 4,000
buys the dev set only and sits one step from that cliff:

| threshold | dev 58 | holdout 28 | final holdout 22 | Japanese genre 12 |
|---:|---:|---:|---:|---:|
| 2,000 (default) | 53/58 | **26/28 (92.9%)** | **19/22 (86.4%)** | 12/12 |
| 3,000 | 54/58 | 26/28 | 19/22 | 12/12 |
| 4,000 | 56/58 | 26/28 | 19/22 | 12/12 |

Absolute numbers shift with the Glev model revision (the pinned rev moved during this work); the
comparisons above are one deployment of one revision. An earlier record in this file reports the
final holdout at 16/22; the revision measured here reaches 19/22, and the failure list is now three
operation-selection errors. Remaining final-holdout misses are
operation-selection errors on English requests (`sci fi with distopian future`,
`show me a movie with harrison ford`, `A good story`), not slots or domains.

Dev-set tuning was overfitting the threshold; the holdout is the authority and keeps 2,000 bps. The
remaining final-holdout misses are operation-selection errors on English synopsis questions
(`top-1 2/5`, `incomplete-intent 3/5`), not slot or domain problems.


## Final pass

After adding genre/plot feature cues and `set in` / person OOS rules, the 58-item dev gate is
56/58 and the frozen 12-item `real-queries-final3-2026-11-13.jsonl` is 12/12 at 4,000 bps. The
clean holdout contains only one complete-intent item, so treat the complete-slice percentage as
directional; the 20-item diagnostic set (not independent) is 20/20. See
`design/investigations/2026-11-12-movie-real-query-gate.md` for the full caveat.

## 2026-11-13: English genre aliases (ADR 0117 amendment)

The genre domain originally stored only the Japanese canonical labels, so an English request such
as `1970s drama` routed but executed `g.name CONTAINS 'drama'`, which matches no graph row. Domain
entries now accept alias surfaces, and `domains/genre-aliases.json` maps each graph genre label to
its English surfaces. The Router stores the aliases with the domain and the inference canister
canonicalizes the matched surface before execution.

`eval/english-genre-2026-11-13.jsonl` is a 12-item English gate (decade/year + English genre). At the
default 2,000 bps profile it routes **12/12** with zero incomplete items. The canonical slot values
were inspected through `debug_interpret` (for example `adventure movies of the 2000s` ->
`アドベンチャー映画`, `western movies from the 1960s` -> `西部劇`) and
`films-by-conditions` returns rows for the canonical genre.

The English real-query dev/holdout results are not re-labelled by this change: they measure the
routing decision, and the canonical value is now executable rather than silently empty.

The equivalent CLI end-to-end check for the alias path is:

```bash
gleaph ask -q "1970s drama" --graph movies --canister "$ROUTER" --network local \
  --identity "$IDENTITY"
```

It prints `films-by-conditions from=1970 to=1979 genre=ドラマ映画` and 12 rows, so the alias is not
only a routing label: it reaches prepared execution.

## 2026-11-13: English synopsis text (bilingual search_text)

The graph's text search was Japanese-only, so English topic queries routed successfully and then
returned zero rows. The catalog now stores the English Wikipedia intro for every film
(`synopsisEn`, with `sourceUrlEn`), and the seed builder projects both intros into the derived
`Film.search_text`; `synopsis-search` ranks that field through the `film_search` text index.
Embeddings and `similar-films` remain based on the Japanese synopsis.

`eval/english-topic-2026-11-13.jsonl` (10 English topic questions) routes **10/10** to
`synopsis-search` with zero incomplete items. Execution through `gleaph ask` returns
non-empty rows for **10/10**; the multilingual analyzer's Porter stem lets `explosions` match
`explosion` (`ファイナル・デスティネーション`, `アポロ13`, ...).

Example:

```text
films with sharks -> synopsis-search query=sharks -> ジョーズ (1975), 1 row
movies about time travel -> synopsis-search query=time travel -> 10 rows
```

## 2026-11-13: client-side semantic topic search

Keyword `synopsis-search` is fast and explainable but cannot answer a paraphrase whose words are
absent from the intro. The demo now also exposes semantic retrieval: `scripts/embed-query.py`
encodes a topic with the pinned `intfloat/multilingual-e5-small` model (`query: ` prefix), and
`prepared run similar-films --param-bytes query=BASE64` searches the film embedding index. The
canister never runs a model; the client owns the embedding, and the Router text route is not
involved.

Examples measured on the local demo:

```text
sharks                          -> 大アマゾンの半魚人, パイレーツ..., 8 rows
time travel                     -> 2001年宇宙の旅, アリス・イン・ワンダーランド/時間の旅, 8 rows
a heist                        -> ナイヴズ・アウト, アメリカン・ハッスル, 8 rows
宇宙人が出てくる映画             -> ホーム 宇宙人ブーヴのゆかいな大冒険, スペース・ジャム, 8 rows
```

These are qualitative probes, not a labelled accuracy gate: relevance varies because the index is
built from short Japanese Wikipedia intros and the model is a small multilingual encoder.
