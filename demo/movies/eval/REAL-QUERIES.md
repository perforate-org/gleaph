# Real-query routing gate (movie)

This gate uses **real MovieLens.org user queries**, not synthetic paraphrases. The source is the
public query dataset from:

- Jie Kang, Kyle Condi, Shuo Chang, Joseph A. Konstan, Loren Terveen, F. Maxwell Harper.
  *Understanding How People Use Natural Language to Ask for Recommendations.* RecSys 2017.
- Queries collected on movielens.org in May 2016.
- Dataset: University of Minnesota Conservancy, `responses.csv`.
- License: CC BY 3.0 US.

`real-queries-2026-11-12.jsonl` is a 60-query pilot: 20 queries that one of the five movie
operations can serve, and 40 that must not execute with this catalog (unsupported capability,
missing required slots, or personalization).

## Labels

- `expected_op`: the prepared operation whose capability covers the request.
- `expect_abstain`: the request must not execute (unsupported feature such as actor/director/
  rating/TV/personalization, or a request without the operation's required slots).
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
