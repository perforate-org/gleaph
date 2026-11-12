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

## 2026-11-13 follow-up: three-way gate, slot rules and a frozen holdout

The strict 60-query gate was re-read as three classes because "no operation covers this" and "the
operation covers it but a required slot is missing" are different failures: **11 complete
supported**, **22 incomplete-intent**, **25 out-of-scope** (58 items; two ambiguous
genre+plot questions were removed). This is the first evaluation where an incomplete supported
route is not counted as an abstain, and `gleaph ask --eval` now reports the three slices separately.

Taxonomy correction matters: bare genres such as `Sci-fi`, `Thriller`, and `documentary` were
initially labelled out-of-scope, but they are supported genre intents missing the year range. They
were moved to incomplete-intent after inspection.

### Rules and artifact changes

Glev added: English decades (`1970s`, `90s`, `2000s`), bare genre extraction, fan-of anchors,
single-year exact ranges, `with`/`looking-for` text queries, and real-query OOS cues. The pinned
revision is now `d5635e829e6f30a99f6fe338659ce646bb1cd3a6`. A first retrain appended 49 unique real
movie labels (complete and incomplete) with the current catalog descriptions; the gate/movie/
knowledge suites stayed in their existing ranges.

### Dev gate (58 items, profile 4,000 bps)

- complete: **11/11**
- incomplete-intent operation: **20/22**
- out-of-scope abstain: **25/25**
- overall: **56/58 (96.6%)**

This is the tuned evaluation set, not independent evidence.

### Diagnostic holdout (28 items, frozen before the last description/OOS tuning)

At 4,000 bps after the final retrain:

- complete: 7/8
- incomplete-intent: 7/8
- out-of-scope: 11/12
- overall: 25/28 (89.3%)

This set was used to diagnose failures while tuning, so it is no longer a clean holdout.

### Final untouched holdout (22 items, evaluated once after the last artifact)

At 4,000 bps:

- complete: **2/5** (`sci fi with distopian future` and `Films with a setting of Mexico` selected
  `films-by-conditions`; `something with explosions` abstained)
- incomplete-intent: **3/5** (`romance` abstained; `romance movie` selected `synopsis-search`)
- out-of-scope: **11/12** (`show me a movie with harrison ford` selected `synopsis-search`)
- overall: **16/22 (72.7%)**

The target (complete >=85%, incomplete >=80%, OOS abstain >=90%, overall >=85%) is **not met on the
final untouched holdout**. The diagnostic set's 89.3% must not be quoted as held-out quality.

The remaining gap is operation selection for genre-only vs plot-only phrasing and the lack of a
person/actor OOS rule for `with <Name>`; retraining on 49 labels did not move the final holdout.
A larger independent annotation pass is required before another model change; the current labels
are one developer's and the untouched holdout is only 22 questions.

Note: an uncommitted worktree change for ADR 0117 (wire v6 parameter value domains) exists in the
same checkout. It is not part of this record; its canister unit test `wire_reports_five` still
expects wire version 5 and currently fails.
