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
