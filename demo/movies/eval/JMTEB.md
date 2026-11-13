# JMTEB retrieval signal (2026-11-13)

This is the independent-label signal for the movie embedding candidates. It evaluates encoders on
a small retrieval subset of [JMTEB](https://huggingface.co/datasets/sbintuitions/JMTEB) (Japanese
Massive Text Embedding Benchmark), outside the movie catalog and outside the demo's own labels.

It is a model-quality signal, not a replacement for the live movie gate.

## Tasks

Retrieval tasks with 510 test queries and a 637-document corpus:

| Task | Query | Corpus |
| --- | --- | --- |
| `nlp_journal_abs_article` | paper abstract | full article |
| `nlp_journal_title_abs` | paper title | abstract |
| `nlp_journal_title_intro` | paper title | introduction |
| `nlp_journal_abs_intro` | paper abstract | introduction |

Both use one relevant document per query (`relevant_docs` in the JMTEB parquet files). The script
downloads the parquet splits directly from the dataset repository; no `datasets` dependency is
required. Cache files live in `.cache/jmteb/`, which is gitignored.

## Run

```bash
cd demo/movies
../../target/venv/bin/python scripts/eval-jmteb.py \
  --models e5-small,bekko-a8m \
  --tasks nlp_journal_abs_article,nlp_journal_title_abs \
  --output eval/jmteb-2026-11-13.json
```

Metrics: nDCG@10, Recall@10 and MRR. The 95% interval is a bootstrap over queries.

## JSTS semantic similarity

`eval-jmteb-sts.py` evaluates the JMTEB **JSTS** test split: 1,457 sentence pairs with human
similarity labels (0–5). Both sentences use the passage side (symmetric STS, no task prompt); the
metric is Spearman rank correlation.

```bash
../../target/venv/bin/python scripts/eval-jmteb-sts.py   --models e5-small,bekko-a8m   --output eval/jmteb-sts-2026-11-13.json
```

| Model | JSTS Spearman | 95% CI |
| --- | ---: | ---: |
| E5-small | 0.779 | [0.753, 0.804] |
| Bekko a8m | **0.785** | [0.760, 0.809] |

The STS gap is small compared with the retrieval gap, but Bekko still leads.

## Baseline (2026-11-13)

| Model | abs_article | title_abs | title_intro | abs_intro | Mean nDCG@10 | Mean Recall@10 | Mean MRR |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `intfloat/multilingual-e5-small` | 0.841 | 0.943 | 0.745 | 0.842 | **0.843** | 0.925 | 0.816 |
| `hotchpotch/bekko-embedding-v1-a8m` | 0.917 | 0.952 | 0.782 | 0.913 | **0.891** | 0.957 | 0.869 |

Bekko leads on all four tasks. The gap is largest on abstract-to-article retrieval (0.917 vs 0.841)
and abstract-to-introduction retrieval (0.913 vs 0.842); title-to-abstract is the closest task
(0.952 vs 0.943).

The report includes per-task metrics, parameter counts, data hashes and confidence intervals.
Pass `--include-per-query` when the per-query retrieved rows are needed.

## What this does and does not decide

This supports keeping Bekko as the current demo encoder:

```text
Bekko > E5-small on all four retrieval tasks
```

It does **not** establish that Bekko is the best Japanese embedding model overall:

- Only 4 retrieval datasets plus JSTS are used (5 of the 28 JMTEB datasets).
- The domain is Japanese academic papers, not film synopses or user queries.
- The movie-specific live gate (`SIMILARITY-GATE.md`) still uses metadata-derived labels.
- The result is a CPU/f32 evaluation, not a browser, canister, or quantized ONNX result.

The next quality step is to add the remaining JMTEB-lite families (classification, clustering,
pair classification) and an independent cinema-specific annotation set, then combine those with the
live movie gate before making a final model decision.

## License

JMTEB is published under CC BY-SA 4.0. The cached parquet files are external evaluation data and
are not committed; the script records their sha256 in the report.

## Decision (2026-11-14)

Combined with the live movie gates, these results keep Bekko a8m as the demo encoder. ADR 0122
records the decision, the rejected alternatives and the re-evaluation triggers.
