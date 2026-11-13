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
| genre-year | 4 | exhaustive catalog metadata (genre and decade) |
| director | 4 | exhaustive `directors` metadata |
| studio | 2 | exhaustive `studios` metadata |
| semantic | 9 | curated keyword positives in Japanese synopses |
| cross-lingual | 12 | same concept sets, English/French/Spanish/German queries |
| similar-film | 2 | Miyazaki/Ghibli and Mad Max franchise |
| out-of-scope | 5 | no relevant film |

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
`retrieval_mean_ndcg_at_10` over the 23 semantic, cross-lingual and similar-film queries.

## Run

```bash
cd demo/movies
../../target/venv/bin/python scripts/eval-embeddings.py \
  --models e5-small,bekko-a8m,bekko-onnx,granite-97m,model2vec-multilingual \
  --output eval/embedding-eval-2026-11-13.json
```

The script only loads the models named on the command line. It does not modify
`seeds/embeddings.jsonl`, the vector index, or the Router. The optional static-model comparison
needs `pip install model2vec`; the transformer models are downloaded by `transformers` on first
use.

## Starter result

| Model | Params | f32 bytes | retrieval nDCG@10 | 95% CI | all nDCG@10 | Recall@10 | MRR | query ms |
| --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| `intfloat/multilingual-e5-small` | 117.7M | 470.6 MB | **0.363** | [0.249, 0.483] | 0.429 | 0.253 | 0.658 | 13.2 |
| `hotchpotch/bekko-embedding-v1-a8m` | 106.0M | 423.9 MB | **0.537** | [0.446, 0.624] | 0.595 | 0.386 | 0.855 | 3.7 |
| `bekko-onnx` (130 MB graph) | — | 130.1 MB | **0.542** | [0.451, 0.629] | 0.598 | 0.385 | 0.870 | 0.8 load |
| `ibm-granite/granite-embedding-97m-multilingual-r2` | 97.4M | 389.8 MB | **0.272** | [0.173, 0.373] | 0.295 | 0.142 | 0.611 | 13.7 |
| `minishlab/potion-multilingual-128M` (static) | 128.1M | 512.4 MB | **0.162** | [0.085, 0.255] | 0.147 | 0.084 | 0.376 | 0.1 |

The 95% intervals are bootstrap over the 23 primary retrieval queries. Bekko's interval still
overlaps the top of E5-small's interval; the expanded starter set narrows but does not eliminate
uncertainty.

Document encoding on this machine: E5 49.2 s, Bekko 18.1 s, Bekko ONNX (CPU) 18.9 s,
Granite 84.6 s, Model2Vec 0.2 s for 2,000 synopses. These are CPU/f32 numbers, not WebGPU.

### Observations

- Bekko a8m is the strongest on this starter set, both in quality and query latency. Its advantage
  is largest on the semantic and cross-lingual categories.
- E5-small is weak on several concept queries (`サメ`, `人工知能`) even though the operation routes.
  This is exactly the gap that the routing eval could not see.
- Granite 97M underperformed the model card's retrieval claims here. Before drawing conclusions,
  verify the model revision, tokenizer, pooling configuration and whether this task/language mix is
  the intended use. Do not select or reject it on this starter set alone.
- Bekko ONNX matches Bekko PyTorch on the gate (0.542 vs 0.537) and on vector equivalence:
  minimum cosine 0.999762 over 78 query/passage texts. The 130 MB ONNX graph is the deployment
  artifact; its quality is not reduced by export.
- Model2Vec is orders of magnitude faster (0.1 ms/query, 0.2 s for 2,000 docs) but its static
  representation is far behind on this Japanese corpus. It is a canister-feasibility candidate,
  not a quality winner.
- The similar-film query is subjective and has only one sample; it is not a model-selection gate.
- The out-of-scope queries have no relevant films and are not part of nDCG. All models return
  rows for them without an abstention threshold, so the current harness does not provide a
  usable out-of-scope decision gate.

## Independent JMTEB signal

[JMTEB.md](JMTEB.md) is the independent-label signal for model selection: four JMTEB retrieval
tasks (510 queries each) plus JSTS, evaluated offline with the same encoder adapters. On
2026-11-13 Bekko leads E5-small on all four retrieval tasks and narrowly on JSTS.

## Live gate

The offline comparison above does not exercise the published operations. The live Router/vindex
regression gate is [SIMILARITY-GATE.md](SIMILARITY-GATE.md); quickstart runs its `--gate` mode.

## Limits

- 23 primary retrieval queries; confidence intervals are still wide.
- Labels are keyword/metadata-derived, not independently annotated.
- No negative relevance labels beyond unlisted positives.
- The corpus is 2,000 Japanese synopses; cross-lingual behaviour is measured on this corpus only.
- f32 CPU timings are not browser or canister timings.

## Next

1. Expand to 50–100 queries with independent labels.
2. Add public Japanese/multilingual retrieval tasks (JMTEB/MMTEB) as a second signal.
3. Only then choose Bekko vs E5 vs another model.
4. If Bekko wins, test its quantized ONNX browser path and re-run this gate.

## Canonical encoder switch (2026-11-13)

After the ONNX equivalence check (minimum cosine 0.999762 vs PyTorch), the demo's committed
`seeds/embeddings.jsonl` and `seeds/embeddings.provenance.json` were regenerated with
`hotchpotch/bekko-embedding-v1-a8m` at revision
`c721113d59a1d91b447450324f51c4b3332c924a`. The vector index definition is unchanged (384d,
cosine, i8, ivf_flat); only the embedding space changed. E5-small remains the baseline in the
table above and can be regenerated from the contract history. The starter gate is still small, so
this is a demo-level switch, not a final model decision.

## Decision (2026-11-14)

ADR 0122 selects Bekko a8m for the movie demo. The table above is the evidence record; E5-small
remains the reproducible baseline. This is a demo-scoped decision, not a global model selection.
