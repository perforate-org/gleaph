# 0122. Movie demo embedding model

Date: 2026-11-14
Status: accepted (2026-11-14), implemented as the committed demo embedding contract.
Last revised: 2026-11-14
Anchor timestamp: 2026-11-14 00:55:00 UTC +0900

## Context

The movie demo needs one committed encoder for its synopsis embeddings. ADR 0120 keeps the model
outside the core SDK and requires an explicit embedding contract; ADR 0121 makes the stored anchor
embedding a first-class vector-search source. The remaining decision is which model the demo's
committed embeddings and query sidecar use.

The candidates evaluated in the demo were:

- `hotchpotch/bekko-embedding-v1-a8m` (current committed embeddings)
- `intfloat/multilingual-e5-small` (baseline)
- `ibm-granite/granite-embedding-97m-multilingual-r2`
- `minishlab/potion-multilingual-128M` (static model)

The selection needs a retrieval decision, not only a generic embedding-leaderboard rank: the demo
uses the encoder for synopsis-to-synopsis similarity and topic retrieval.

## Decision

1. **Use Bekko a8m as the movie demo encoder.**
   - model: `hotchpotch/bekko-embedding-v1-a8m`
   - revision: `c721113d59a1d91b447450324f51c4b3332c924a`
   - 384 dimensions, mean pooling, normalized, cosine similarity
   - no query/passage prefix
2. **Keep the committed `seeds/embeddings.jsonl` and vector index in that embedding space.** This is
   a demo-scoped decision; it does not select a model for every Gleaph deployment.
3. **Keep E5-small as the documented baseline.** It remains reproducible from the contract history
   and is used as the comparison in the evaluation records.
4. **Keep the 130 MB Bekko ONNX graph as the browser deployment artifact candidate.** The encoder
   stays an optional provider (ADR 0120); the model-free similar-films browser path does not load
   it.
5. **Re-evaluate when a trigger in "Re-evaluation" occurs.** Do not switch the committed embeddings
   without a new embedding contract and a fresh index generation.

## Evidence

### JMTEB independent-label retrieval

Four JMTEB retrieval tasks, 510 test queries and 637 documents per task
(`nlp_journal_abs_article`, `nlp_journal_title_abs`, `nlp_journal_title_intro`,
`nlp_journal_abs_intro`):

| Model | Mean nDCG@10 | Mean Recall@10 | Mean MRR |
| --- | ---: | ---: | ---: |
| Bekko a8m | **0.891** | 0.957 | 0.869 |
| E5-small | 0.843 | 0.925 | 0.816 |

Bekko leads on all four retrieval tasks. See `demo/movies/eval/JMTEB.md` and
`demo/movies/eval/jmteb-2026-11-13.json`.

### JMTEB JSTS

1,457 sentence pairs with human similarity labels:

| Model | Spearman | 95% CI |
| --- | ---: | ---: |
| Bekko a8m | **0.785** | [0.760, 0.809] |
| E5-small | 0.779 | [0.753, 0.804] |

The STS gap is small and the intervals overlap; retrieval is the decisive signal for this demo.

### Live movie gates

The live Router path passes both deterministic structural gates:

| Gate | Queries | nDCG@8 | Recall@8 |
| --- | ---: | ---: | ---: |
| same-director | 15 | 0.577 | 0.496 |
| franchise (`followedBy`) | 10 | 0.705 | 0.960 |

These gates are regression checks, not independent human relevance labels; they do not by
themselves select a model.

### Size and latency

From the starter encoder comparison:

| Model | f32 bytes | query latency | notes |
| --- | ---: | ---: | --- |
| Bekko a8m | 423.9 MB | 3.7 ms | 130 MB ONNX export matches PyTorch |
| E5-small | 470.6 MB | 13.2 ms | baseline |

The Bekko ONNX/PyTorch vector equivalence check reports minimum cosine `0.999762` over 78
query/passage texts. See `demo/movies/eval/EMBEDDING-EVAL.md`.

## Rejected alternatives

- **E5-small** — weaker on the four retrieval tasks (0.843 vs 0.891 mean nDCG@10), slower in the
  measured query path, and larger as f32.
- **Granite 97M** — underperformed on the starter retrieval set; the model card configuration was
  not established as the intended task mix. Rejected for the current demo, not globally.
- **Model2Vec multilingual** — far behind on Japanese retrieval in the starter set; useful only as
  a canister-feasibility experiment, not a quality choice.

## Re-evaluation

Re-open this decision when any of these changes:

- an independent cinema-specific relevance set is available and disagrees with the structural
  gates;
- the browser/canister memory or latency budget makes the 130 MB / 384-d artifact unacceptable;
- a materially better Japanese embedding model becomes available and passes the same JMTEB and live
  gates;
- the demo's query distribution extends beyond Japanese/English movie titles and synopses;
- the embedding contract, pooling, prefix, or index generation changes.

## Consequences

- The committed embeddings, provenance and vector index stay on Bekko `c721113d`.
- Model comparison remains reproducible through `eval-embeddings.py` and `eval-jmteb.py`.
- The core SDK remains model-free; the similar-films browser path is unaffected.
- Adding a second model requires a new embedding contract and a fresh vector-index generation, not
  an in-place reinterpretation of the existing bytes.

## Evidence paths

- `demo/movies/seeds/embeddings.provenance.json`
- `demo/movies/eval/EMBEDDING-EVAL.md`
- `demo/movies/eval/JMTEB.md`
- `demo/movies/eval/SIMILARITY-GATE.md`
- `demo/movies/eval/jmteb-2026-11-13.json`
- `demo/movies/eval/jmteb-sts-2026-11-13.json`

## Related

- ADR 0064 (Vector canister ownership)
- ADR 0119 (one vector operation, client-side intent)
- ADR 0120 (optional embedding provider)
- ADR 0121 (anchor embedding vector search)
