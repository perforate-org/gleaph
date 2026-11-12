# Movie real-query gate seed (2026-11-12)

Status: data seed prepared; routed evaluation pending a loaded movie demo.

## Source

The gate uses the public MovieLens.org query dataset from:

- Jie Kang, Kyle Condi, Shuo Chang, Joseph A. Konstan, Loren Terveen, F. Maxwell Harper.
  *Understanding How People Use Natural Language to Ask for Recommendations.* RecSys 2017.
- 347 respondents, first and follow-up queries collected on movielens.org in May 2016.
- University of Minnesota Conservancy `responses.csv`; CC BY 3.0 US.
- Reproduction: `demo/movies/scripts/collect-real-queries.py` (downloads to `target/`, dedupes
  448 unique non-empty queries).

## Seed gate

`demo/movies/eval/real-queries-2026-11-12.jsonl` is a 60-query pilot selected from the 448:

- 20 `expected_op` items covering `films-by-conditions`, `similar-films`, `synopsis-search`;
- 40 `expect_abstain` items covering unsupported capabilities (actor/director/rating/TV/
  personalization), genre-only requests missing a required year range, and vague mood queries.

Every row carries `source_row`, `source_field`, `modality`, `license`, `citation` and a
`label_note`. The upstream CSV is cp1252; the collector handles that and dedupes case-insensitively.

`demo/movies/scripts/check-real-queries.py` validates the gate against the five prepared operation
files and requires a supported/OOS balance. `crates/cli` has a parse test for the file so the
existing `ask --eval` holdout cannot silently break on format.

## Pilot result (loaded local movie demo)

Initial metadata, default 2,000 bps profile:

- 60 items: 20 supported, 40 abstain.
- routed top-1: 7/20 (35.0%).
- abstain recall: 33/40 (82.5%).
- overall: 40/60 (66.7%).
- incomplete routed items: 15/20.
- per operation: `similar-films` 6/7, `films-by-conditions` 0/2, `synopsis-search` 1/11.

At threshold zero, raw routing coverage was 18/20 with selective accuracy 7/18 (38.9%). The
supported-query failure was therefore mostly operation selection/catalog generalisation, not just
the threshold.

After adding real phrasing cues to the three operation descriptions (documented in
`demo/movies/prepared/*.gql`), raw selective accuracy rose to **13/18 (72.2%)**, but unsupported
false accepts increased at the default threshold. In-sample threshold sweep:

| bps | top-1 | abstain | overall |
|---:|---:|---:|---:|
| 2,000 | 13/20 | 13/40 | 26/60 |
| 5,000 | 13/20 | 23/40 | 36/60 |
| 7,000 | 12/20 | 24/40 | 36/60 |
| 9,000 | 11/20 | 32/40 | 43/60 |
| 9,761 | 7/20 | 37/40 | 44/60 |

9,761 bps calibration: coverage 0.35, selective 1.0, OOS abstention 0.925. These are in-sample
numbers and must not be reported as held-out quality; a frozen holdout is required.

Remaining defects: English decade/bare-genre slot extraction (`1970s drama`) is missing, and
`synopsis-search` / `similar-films` still leave required slots incomplete.

## Why this is only a pilot

- The labels were assigned by the project developer, not by independent annotators.
- The five-operation movie catalog covers only part of what MovieLens users ask. `common-cast` and
  `sequel-chain` are absent from this sample; a second source (StackExchange Movies & TV) is needed.
- 20 supported / 40 abstain over-samples unsupported queries to test precision; it is not a
  production traffic distribution.
- The questions are short recommendation queries, not multi-turn agent prompts.
- Do not tune catalog descriptions or the threshold on these 60 and then report the same numbers as
  independent evidence. Split a larger collection into dev/holdout before tuning.

## Run

```bash
python3 demo/movies/scripts/collect-real-queries.py
python3 demo/movies/scripts/check-real-queries.py
# with a loaded movie demo:
gleaph ask --eval demo/movies/eval/real-queries-2026-11-12.jsonl \
  --graph movies --canister "$ROUTER" --network local --identity "$IDENTITY" --json
```

## Next collection pass

1. Expand to 150+ queries by adding StackExchange Movies & TV questions for common-cast and
   sequel intent.
2. Have a second annotator label at least 30% and report agreement.
3. Freeze a 50-item holdout before any tuning; keep the rest as dev.
4. Add a second graph (knowledge) with the same provenance format.
