# Live similarity relevance gate (2026-11-13)

This gate measures the published title-bound similarity operations against a running Router and
vector index:

```text
films-similar-to-ja   title_ja anchor
films-similar-to-en   title_en anchor
```

It is a **structural gate**, not an independent human relevance benchmark.

## Why a second gate

`EMBEDDING-EVAL.md` compares encoders offline with a Python harness. It cannot catch regressions in
the live path: anchor resolution, stored embedding lookup, vector index dispatch, or prepared-query
authorization. This gate calls the actual published operations and scores the rows they return.

## Labels

`similarity-gate-2026-11-13.jsonl` has 15 queries:

- 8 Japanese anchors
- 7 English anchors
- each anchor appears with `films-similar-to-ja` or `films-similar-to-en`

Relevance is **same-director metadata derived from the catalog**:

```text
2 = another film by any of the anchor's directors
0 = everything else
```

The anchor itself is grade 0 because it is not a recommendation. It remains in the result set, so
the gate also measures the current self-inclusion behaviour.

Regenerate the frozen labels with:

```bash
cd demo/movies
../../target/venv/bin/python scripts/build-similarity-gate.py
```

This is not an independent annotation set. It is deterministic and useful for regression detection;
an independent human/JMTEB-style set is still required for a model-selection claim.

## Run

The Router must be running and `browser/config.json` must point at it (quickstart writes that file):

```bash
cd demo/movies
node scripts/eval-similarity-gate.mjs --gate
```

The script calls the public operations anonymously, so it also exercises the prepared-query and
vector-read grants.

Default thresholds:

```text
mean nDCG@8   >= 0.50
mean Recall@8 >= 0.30
```

Override for a deliberate experiment:

```bash
node scripts/eval-similarity-gate.mjs \
  --min-ndcg 0.55 --min-recall 0.35 --output /tmp/similarity-gate.json
```

## Baseline (2026-11-13)

Current committed Bekko index:

```text
queries: 15
mean nDCG@8:   0.577
mean Recall@8: 0.496
mean MRR:      0.478
gate:          pass
```

| Slice | Queries | nDCG@8 | Recall@8 | MRR |
| --- | ---: | ---: | ---: | ---: |
| Japanese | 8 | 0.570 | 0.496 | 0.479 |
| English | 7 | 0.585 | 0.496 | 0.476 |

The weakest anchor is `インターステラー` / `Interstellar` at nDCG@8 0.224. Its same-director label
set spans Nolan films across very different genres, so the label is deliberately blunt; the result
does not by itself justify changing the encoder.

The report retains per-query retrieved titles and qids in
`eval/similarity-gate-2026-11-13.json`.

## Limits

- Same-director relevance is a proxy. It conflates directorial style, franchise, and genre.
- The result set includes the anchor at rank 1, which depresses MRR to about 0.5 by construction.
- One Japanese catalog and one English label surface per anchor; no multilingual expansion yet.
- The gate uses the local Router and local index; it is not a hosted-network benchmark.
- A 15-query structural gate is not enough to select a model. Use it to catch live regressions and
  combine it with the offline `EMBEDDING-EVAL` and a future independent annotation set.
