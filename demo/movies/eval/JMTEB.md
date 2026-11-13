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

## Baseline (2026-11-13)

| Model | abs_article nDCG@10 | title_abs nDCG@10 | Mean nDCG@10 | Mean Recall@10 | Mean MRR |
| --- | ---: | ---: | ---: | ---: | ---: |
| `intfloat/multilingual-e5-small` | 0.841 | 0.943 | **0.892** | 0.953 | 0.872 |
| `hotchpotch/bekko-embedding-v1-a8m` | 0.917 | 0.952 | **0.935** | 0.980 | 0.919 |

Bekko leads on both tasks. The gap is larger on abstract-to-article retrieval (0.917 vs 0.841)
than on title-to-abstract retrieval (0.952 vs 0.943).

The report includes per-task metrics, parameter counts, data hashes and confidence intervals.
Pass `--include-per-query` when the per-query retrieved rows are needed.

## What this does and does not decide

This supports keeping Bekko as the current demo encoder:

```text
Bekko > E5-small on the two retrieval tasks
```

It does **not** establish that Bekko is the best Japanese embedding model overall:

- Only 2 of the 28 JMTEB datasets are used, and only the retrieval family.
- The domain is Japanese academic papers, not film synopses or user queries.
- The movie-specific live gate (`SIMILARITY-GATE.md`) still uses metadata-derived labels.
- The result is a CPU/f32 evaluation, not a browser, canister, or quantized ONNX result.

The next quality step is to widen the JMTEB subset (and optionally add JMTEB-lite classification /
STS tasks), then combine it with the movie gate before making a final model decision.

## License

JMTEB is published under CC BY-SA 4.0. The cached parquet files are external evaluation data and
are not committed; the script records their sha256 in the report.
