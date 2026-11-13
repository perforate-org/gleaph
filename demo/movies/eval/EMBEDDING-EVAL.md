# Embedding relevance gate (starter, 2026-11-13)

This is the first retrieval-quality gate for the movie demo. It is deliberately small and
reproducible; it is **not** a final model-selection benchmark.

## Why

The existing routing gates measure whether `interpret` chooses the right operation. They do not
measure whether the vector operation retrieves relevant films. A semantic query can route
correctly to `films-by-embedding` and still return unrelated rows.

## Dataset

`relevance-2026-11-13.jsonl` contains 21 queries:

| Category | Queries | Labels |
| --- | ---: | --- |
| genre-year | 3 | exhaustive catalog metadata (genre and decade) |
| director | 2 | exhaustive `directors` metadata |
| studio | 1 | exhaustive `studios` metadata |
| semantic | 6 | curated keyword positives in Japanese synopses |
| cross-lingual | 6 | same concept sets as semantic, English query |
| similar-film | 1 | Miyazaki = grade 2, other Studio Ghibli = grade 1 |
| out-of-scope | 2 | no relevant film |

Relevance is graded 0/1/2. Unlisted films are treated as grade 0. The semantic/cross-lingual
positives are concept-based, not independent human judgements: this is a starter gate, not an
annotation-quality claim.

## Metrics

- nDCG@10 (graded)
- Recall@10
- MRR
- out-of-scope false positives at 10

The **primary model comparison** excludes genre-year, director and studio queries. Those are
structured routing cases; the embedding is not the right component for them. The primary number is
`retrieval_mean_ndcg_at_10` over semantic, cross-lingual and similar-film queries.

## Run

```bash
cd demo/movies
../../target/venv/bin/python scripts/eval-embeddings.py \
  --models e5-small,bekko-a8m,granite-97m \
  --output eval/embedding-eval-2026-11-13.json
```

The script only loads the models named on the command line. It does not modify
`seeds/embeddings.jsonl`, the vector index, or the Router.

## Starter result

| Model | Params | f32 bytes | retrieval nDCG@10 | all nDCG@10 | Recall@10 | MRR | query ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `intfloat/multilingual-e5-small` | 117.7M | 470.6 MB | **0.281** | 0.366 | 0.195 | 0.579 | 9.5 |
| `hotchpotch/bekko-embedding-v1-a8m` | 106.0M | 423.9 MB | **0.492** | 0.567 | 0.333 | 0.925 | 3.9 |
| `ibm-granite/granite-embedding-97m-multilingual-r2` | 97.4M | 389.8 MB | **0.219** | 0.270 | 0.120 | 0.588 | 11.3 |

Document encoding on this machine: E5 45.5 s, Bekko 12.3 s, Granite 66.4 s for 2,000 synopses.
These are CPU/f32 numbers, not the browser's quantized ONNX path.

### Observations

- Bekko a8m is the strongest on this starter set, both in quality and query latency. Its advantage
  is largest on the semantic and cross-lingual categories.
- E5-small is weak on several concept queries (`サメ`, `人工知能`) even though the operation routes.
  This is exactly the gap that the routing eval could not see.
- Granite 97M underperformed the model card's retrieval claims here. Before drawing conclusions,
  verify the model revision, tokenizer, pooling configuration and whether this task/language mix is
  the intended use. Do not select or reject it on this starter set alone.
- The similar-film query is subjective and has only one sample; it is not a model-selection gate.
- The out-of-scope queries have no relevant films and are not part of nDCG. They are reported
  separately as false positives.

## Limits

- Only 12 primary retrieval queries; confidence intervals are wide.
- Labels are keyword/metadata-derived, not independently annotated.
- No negative relevance labels beyond unlisted positives.
- The corpus is 2,000 Japanese synopses; cross-lingual behaviour is measured on this corpus only.
- f32 CPU timings are not browser or canister timings.

## Next

1. Expand to 50–100 queries with independent labels.
2. Add public Japanese/multilingual retrieval tasks (JMTEB/MMTEB) as a second signal.
3. Only then choose Bekko vs E5 vs another model.
4. If Bekko wins, test its quantized ONNX browser path and re-run this gate.
