# Lightweight text vectorization research (2026-11-13)

Status: research record. No model or pipeline changes were made by this investigation.

Scope: text embedding models and representation-compression techniques that could replace or
augment the movie demo's current `intfloat/multilingual-e5-small` client-side encoder, including
browser and canister deployment. Excludes training a model from scratch, multimodal embeddings, and
purely sparse retrieval.

Method: primary model cards and papers, benchmark documentation, the repository's on-chain
tokenizer measurements, and local latency measurements of the current client path. Numbers from
model author comparisons are marked as author-reported; they are not independently reproduced here.

## Executive summary

1. **Keep the current client-side model until a labelled relevance comparison exists.** The
   current E5-small path is correct in shape: the canister owns vector search, the client owns
   model inference. Its cold start is the real UX cost (about 11.8 s for a fresh Python process;
   the warm sidecar answered in 19.5 ms in this environment).
2. **The most interesting client/browser candidate is `bekko-embedding-v1-a8m`.** It has 7.7M
   active parameters (106M total, mostly the token table), 384 dimensions truncatable to
   256/128/64, 100+ languages, 8K context, MIT license, and a 124 MiB int8 ONNX artifact that runs
   in Transformers.js. The author reports MMTEB multilingual v2 retrieval 56.2 and mean 56.7,
   above multilingual-e5-small (50.9/56.4) at about one third of E5-small's active parameters.
   This is the first candidate to benchmark against the current model.
3. **`granite-embedding-97m-multilingual-r2` is the quality-oriented small alternative.** 97M
   parameters (28.3M active), 384 dimensions, 200+ languages, 32K context, Apache 2.0, ONNX and
   OpenVINO int8 artifacts. The author reports 60.3 multilingual retrieval and 51.9 mean across
   MMTEB; it trades general-task quality for retrieval quality.
4. **`embeddinggemma-300m` is on-device-oriented but not small in active compute.** 308M
   parameters, 768 dimensions with MRL down to 128, 100+ languages, about 2K context. It is
   relevant when quality matters more than model size; it is not the lightweight path.
5. **Model2Vec static embeddings are the only family that is clearly plausible inside a canister
   query.** `potion-multilingual-128M` is a 128M-parameter static table (256 dimensions, 101
   languages) with mean 47.31/retrieval 37.86 on its model card; inference is lookup plus pooling,
   not a transformer forward. Quality is below modern transformer encoders, so this is a
   canister-only trade-off, not the default client choice.
6. **Do not put E5-small or Granite-97M into the current Router/Graph layer.** A dedicated
   embedding canister with update/DTS and a client-visible `embed` method is the only sane on-chain
   shape if privacy requires it; `ask` cannot await an update. Local measurements show the
   canister's instruction budget is the binding constraint for transformer encoders, while
   tokenization itself is cheap (0.23–1.03M instructions per question).

## 1. Current baseline

Current movie demo path:

```text
client: intfloat/multilingual-e5-small, passage:/query: prefixes
        mean pooling, L2 normalization, 384 f32
canister: vector index film_embedding, cosine, i8, ivf_flat
```

Facts from the model card and local pipeline:

- 117.65M parameters total; about 21.6M active parameters according to the MMTEB comparison table
  in the Bekko model card.
- 384 dimensions, 512-token input limit, 94 languages.
- f32 weights about 470–493 MB depending on artifact subset.
- Query and passage prefixes are required: `query: ` and `passage: `.
- Local cold start (fresh Python process): **11.8 s** in this workspace.
- Warm sidecar (`embed-query.py --serve`): **19.5 ms** for a short Japanese query after the
  first lazy request; the first request was 580 ms.
- The graph stores the passage embeddings in `seeds/embeddings.jsonl`; the canister never runs the
  model.

That baseline is the comparison point. A replacement must be evaluated on relevance, not only
latency or parameter count.

## 2. Taxonomy of lightweight vectorization

### 2.1 Smaller transformer encoders

Reduce depth, hidden size, vocabulary, or all three:

- **Distillation**: train a small student to match a teacher's representations or similarity
  scores. TinyBERT and MobileBERT are the classic encoder examples; Sentence Transformers also
  supports embedding distillation.
- **Layer pruning**: remove transformer layers and continue training. Granite R2 prunes 22 layers
  to 12; Bekko prunes mmBERT-small from 22 layers to 4 or 13.
- **Vocabulary selection**: multilingual token tables dominate parameter count. Granite reduces
  262K to 180K; Bekko's total 106M includes a large multilingual table but only 7.7M active
  parameters.
- **Width reduction**: hidden size 384 instead of 768/1024; cheap retrieval indexes.

The important distinction is **total parameters vs active parameters**. A 106M-total model with
7.7M active parameters behaves like an 8M model at inference; the large token table is a lookup.
This is the central insight behind Bekko and Model2Vec.

### 2.2 Static embeddings

Model2Vec distills a sentence transformer into one static vector per vocabulary item. At inference:

```text
tokenize -> look up token vectors -> mean pool -> normalize
```

There is no transformer forward. Reported properties:

- Up to 50x smaller and up to 500x faster than the source sentence transformer.
- `potion-base-8M` is about 8 MB on disk; `potion-base-32M` about 30 MB.
- `potion-multilingual-128M` is 128M parameters, 256 dimensions, 101 languages, MIT.
- Model card MTEB mean 47.31, retrieval 37.86; about 90.9% of LaBSE's mean in its comparison.
- Context is theoretically unlimited because each token is static; tokenization and pooling decide
  the effective context.

Strengths: tiny compute, no transformer kernels, easy browser/canister port.

Weaknesses: no contextual token representations; quality is below transformer encoders on
retrieval and harder tasks; multilingual quality depends on the vocabulary and distillation
corpus. It is a candidate for a constrained canister or a very small browser budget, not
automatically a replacement for E5-small.

### 2.3 Matryoshka Representation Learning (MRL)

MRL trains one embedding whose first `n` dimensions are themselves usable embeddings:

- No additional inference cost; truncate the vector after encoding.
- MRL paper reports up to 14x smaller representations and speedups on retrieval, with accuracy at
  least as good as independently trained low-dimensional representations.
- Examples: EmbeddingGemma 768 -> 128; Bekko 384 -> 256/128/64; nomic-embed-text-v1.5 and
  mixedbread models.

For storage-constrained retrieval, MRL is the cleanest first lever. For 2,000 movie vectors it is
not urgent; for millions of vectors it matters. Truncation must be evaluated per model; a model
not trained with MRL can lose much more quality when truncated.

### 2.4 Output quantization and rescoring

Hugging Face's quantization experiments:

| Representation | Storage reduction | Retrieval retention |
| --- | ---: | ---: |
| float32 | 1x | baseline |
| int8 | 4x | about 97% |
| binary + float32 rescore | 32x | about 96% |
| binary without rescore | 32x | about 92.5% |

The standard pattern is:

1. use the compressed vector (int8 or binary) for candidate generation;
2. take `rescore_multiplier * k` candidates;
3. rescore with the original float query or a reranker.

Bekko's own truncation/quantization table shows the same pattern:

- 384-dim float: overall 0.545
- 256-dim float: 0.536 (-1.76%)
- int8 without rescore: 0.515 (-5.48%)
- int8 with rescore: 0.545 (-0.04%)
- binary without rescore: 0.475 (-12.93%)
- binary with rescore: 0.543 (-0.44%)

For canister-side vector search, binary + rescore can be attractive because Hamming distance is
cheap; for a small 2,000-film demo, int8 is already enough and rescore is easy.

### 2.5 Retrieval architecture

Compression is not the only lever:

- **Hybrid search**: lexical/BM25 or full-text score + dense retrieval, then fuse. This is already
  present in the demo as `synopsis-search` versus `films-by-embedding`.
- **Cross-encoder reranking**: retrieve top-k cheaply, rerank with a small classifier. More
  accurate than bi-encoder cosine, more expensive; keep it off the canister query path unless the
  model is tiny.
- **Two-stage retrieval**: cheap embedding or static model for recall, stronger model for
  reranking only the candidate set.
- **Caching**: embed query text on the client, cache frequent queries, and avoid model reload per
  request.

## 3. Candidate models

The table mixes independent model-card claims and author comparisons. Treat the MMTEB columns as
comparative evidence, not as a guarantee for the movie domain.

| Model | Total params | Active params | Dim | Languages | Context | Reported MMTEB retrieval / mean | License | Deployment notes |
| --- | ---: | ---: | ---: | --- | ---: | --- | --- | --- |
| `intfloat/multilingual-e5-small` | 117.65M | ~21.6M | 384 | 94 | 512 | 50.9 / 56.4 | MIT | current baseline; `query: `/`passage: ` prefixes |
| `hotchpotch/bekko-embedding-v1-a8m` | 106M | 7.7M | 384 (256/128/64) | 100+ | 8192 | 56.2 / 56.7 | MIT | 4-layer mmBERT; no prefixes; ONNX/OpenVINO; 124 MiB int8 ONNX; Transformers.js demo |
| `hotchpotch/bekko-embedding-v1-a25m` | ~107M | 24.9M | 384 (MRL) | 100+ | 8192 | 57.5 / 58.3 | MIT | quality step up; about 2.7x slower than a8m on CPU |
| `ibm-granite/granite-embedding-97m-multilingual-r2` | 97M | 28.3M | 384 | 200+ | 32768 | 60.3 / 51.9 | Apache 2.0 | 12-layer ModernBERT; strong retrieval, weaker all-task mean; ONNX/OpenVINO int8 |
| `google/embeddinggemma-300m` | 308M | 106.3M | 768 (MRL 128–768) | 100+ | ~2000 | 62.5 / 61.2 | Gemma terms | on-device oriented; <200 MB RAM with quantization; larger compute |
| `minishlab/potion-multilingual-128M` | 128M static | no transformer | 256 | 101 | unlimited (tokenized) | different MTEB suite: mean 47.31, retrieval 37.86 | MIT | lookup + mean pool; only plausible canister-side family; quality below transformers |
| `sentence-transformers/paraphrase-multilingual-MiniLM-L12-v2` | ~118M | ~? | 384 | 50 | 512 | older baseline | Apache 2.0 | well-known fallback; not a clear upgrade over E5-small |
| `BAAI/bge-small-en-v1.5` | 33M | ~? | 384 | English | 512 | English-only | MIT | useful size reference, not multilingual |

### Reading the table

- **Bekko a8m** is the Pareto candidate for the client/browser: much lower active compute than
  E5-small, 384 dimensions, 8K context, MRL, MIT, and a browser path already demonstrated by the
  author.
- **Granite 97M** is the retrieval-quality candidate: highest reported multilingual retrieval
  among the small models here, but it is a 12-layer model with 28.3M active parameters and its
  all-task mean is lower than its retrieval score.
- **EmbeddingGemma** is the quality-on-device candidate; 308M total parameters is not "light" in
  the same sense as Bekko, but it is optimized for mobile/laptop deployment and has MRL.
- **Model2Vec** is the only family whose inference is structurally canister-friendly: token
  lookup and pooling, no transformer forward. Its quality is lower and its 500K vocabulary makes
  the tokenizer/model asset large.

## 4. Browser and on-device deployment

### Transformers.js / ONNX

Bekko a8m's model card provides a complete browser path:

```js
import { pipeline } from "@huggingface/transformers";
const extractor = await pipeline(
  "feature-extraction",
  "hotchpotch/bekko-embedding-v1-a8m",
  { device: navigator.gpu ? "webgpu" : "wasm" }
);
```

The default ONNX artifact stores the static token table as row-wise int8 (about 124 MiB instead
of about 404 MiB fp32), while the transformer layers remain fp32. The author reports cosine
similarity >= 0.9994 to PyTorch in release checks. Transformers.js supports WebGPU with a WASM
fallback; WebGPU global support was around 85% as of March 2026 in the Transformers.js docs.

Granite 97M and EmbeddingGemma also publish ONNX/OpenVINO or browser-compatible artifacts, but
download size and active compute are larger.

### Practical browser constraints

- First load downloads the model; cache it in Cache Storage/IndexedDB.
- WebGPU is faster but not universal; keep a WASM path.
- Tokenizer and model both need version pinning.
- A 124 MiB int8 model is a reasonable one-time download for a demo; a 300M model is not.

## 5. Canister feasibility

### 5.1 Measured local evidence

The repository already measured tokenization and a small transformer pack on canister:

- 34.3 MB tokenizer load: **9.85B instructions**; `post_upgrade` reload: **9.84B**.
- Tokenize per question: **0.23–1.03M instructions** (short/long, English/Japanese).
- Shipped 1-layer h640 route pack: **~0.1B** forward at <=32 tokens; **~2.6B** at 256 tokens.
- Earlier 14-layer candidate at 141 tokens: **~18.5B** forward (not the shipped pack).
- Query budget: **5B**; update budget: **20B**, with DTS up to **40B**.

These numbers are for the routing model, not E5-small, but they bound the order of magnitude.

### 5.2 What this implies

- Tokenization is cheap relative to a transformer forward; a canister tokenizer is technically
  viable (and was measured), but the model forward dominates.
- A 1-layer 640-wide model fits a query at short context; a 12-layer 384-wide E5-small is roughly
  4–10x the per-layer cost of that 1-layer pack at the same token count, so short-query forward
  may be in the low-billions of instructions. That is potentially inside a 5B query budget, but
  it needs a real wasm measurement; the 470 MB f32/118 MB int8 weight footprint and per-query
  stable-memory reads are the practical obstacles, not just FLOPs.
- A 4-layer Bekko a8m (7.7M active) is a more plausible transformer candidate than E5-small, but
  still requires a tokenizer and a wasm kernel; the 106M total token table is the storage cost.
- A Model2Vec static model has no transformer forward: tokenization + lookup + mean pool. That is
  the only design that clearly fits the query budget without a new neural runtime. Quality is the
  trade-off.

### 5.3 Layer recommendation

| Layer | Recommendation |
| --- | --- |
| Router | No model. It is control plane and composite-query orchestration; model inference would concentrate cost and break the query-only contract. |
| Graph | No model. It owns data and vector search; model lifecycle would be replicated per shard. |
| Inference canister | No E5-small. It is query-only routing; a tiny model could be evaluated, but not a 118M encoder. |
| Dedicated embedding canister | Only if on-chain privacy is a hard requirement: `embed(text) -> bytes` update with DTS, client caches, then calls `prepared_query`/`films-by-embedding`. It cannot be transparently composed into `ask`. |
| Client/sidecar | Default. Best latency/quality trade-off, no canister model lifecycle, text stays on device. |
| Browser WASM/WebGPU | Best path to remove the sidecar; Bekko a8m is the first candidate. |

If an on-chain embedding is required, prototype **Model2Vec static** first, then **Bekko a8m**;
measure a wasm forward, tokenizer, stable bytes, and query instructions before committing.

## 6. Recommendation for this repository

### Immediate

1. Keep `multilingual-e5-small` as the production/client baseline.
2. Keep the sidecar warm in browser demos; do not pay the 11.8 s cold start per request.
3. Do not move E5-small into Router/Graph.

### Next evaluation (recommended order)

1. **Bekko a8m**: client and browser candidate; test on the movie catalog. It is the only model
   with a reported combination of 7.7M active parameters, 384 dimensions, 100+ languages, MRL,
   MIT, and a browser ONNX path.
2. **Granite 97M multilingual r2**: quality candidate if Bekko loses on Japanese/movie relevance.
3. **EmbeddingGemma 300M**: quality-on-device candidate; compare download size and latency.
4. **Model2Vec multilingual**: canister-only candidate; test with the same relevance set before
   accepting the quality drop.

### Representation-level work

- For the current 2,000-film index, model choice dominates; quantization/MRL is not the primary
  win.
- At 10^5–10^7 vectors, evaluate int8 + rescore first; binary + rescore second; MRL dimensions
  256/128; keep the original f32 vectors for rescoring.
- Do not truncate a non-MRL model without measuring the quality loss.

## 7. Evaluation plan

The current `ask --eval` harness measures routing, not retrieval relevance. Build a separate
embedding-relevance gate.

### Data

- 100–300 queries over the movie catalog.
- Japanese and English, including cross-lingual queries (English query, Japanese synopsis).
- Intents: genre/year, semantic topic, similar-film/anchors, out-of-scope.
- Graded relevance 0/1/2 per film; at least two labels per query if possible.

### Models

- Baseline: E5-small 384d.
- Bekko a8m 384d (and 256/128 truncation).
- Granite 97M 384d.
- EmbeddingGemma 300M (optional, if on-device quality is the target).
- Model2Vec multilingual 256d (canister-only comparison).

### Metrics

- nDCG@10, Recall@10, MRR.
- Query latency p50/p95: cold start, warm sidecar, browser WebGPU/WASM.
- Model download size, RAM, index size.
- For canister candidates: wasm forward instructions, tokenizer instructions, stable bytes,
  response bytes.

### Acceptance gates

- Relevance: no material regression versus E5-small on the labelled set; target nDCG@10 within
  a pre-agreed margin (for example -1 to -2 points).
- Latency: warm p95 within the product budget; cold start acceptable or hidden by caching.
- Size: browser download and memory fit the page's budget.
- License: compatible with the demo's distribution.

## 8. Open questions

- How much of Bekko's reported MMTEB advantage holds on Japanese movie synopses and short
  conversational queries? The author's benchmark is broad, not domain-specific.
- Does Granite 97M's retrieval advantage survive on this small 2,000-document catalog and short
  queries? Its 12 layers may be slower than the parameter count suggests.
- Does EmbeddingGemma's quality justify a 300M model download for a browser demo?
- Can a Model2Vec multilingual static model meet the relevance bar on the movie catalog?
- Can a 4-layer Bekko or a 1–2-layer distilled model fit the canister query budget with a Rust
  tokenizer, and is the cycle cost acceptable?
- Should the demo keep the E5 `query:`/`passage:` prefixes or adopt a no-prefix model (Bekko)?
  This changes query and re-indexing behavior, so it must be a deliberate migration.

## Sources

Primary model cards and papers:

- [intfloat/multilingual-e5-small](https://huggingface.co/intfloat/multilingual-e5-small)
- [hotchpotch/bekko-embedding-v1-a8m](https://huggingface.co/hotchpotch/bekko-embedding-v1-a8m)
- [Bekko Embedding paper](https://arxiv.org/abs/2607.25180)
- [ibm-granite/granite-embedding-97m-multilingual-r2](https://huggingface.co/ibm-granite/granite-embedding-97m-multilingual-r2)
- [Granite Embedding Multilingual R2 paper](https://arxiv.org/abs/2605.13521)
- [EmbeddingGemma model overview](https://ai.google.dev/gemma/docs/embeddinggemma)
- [minishlab/potion-multilingual-128M](https://huggingface.co/minishlab/potion-multilingual-128M)
- [Model2Vec repository](https://github.com/MinishLab/model2vec)
- [Model2Vec retrieval results](https://github.com/MinishLab/model2vec/blob/main/results/README.md)
- [Matryoshka Representation Learning](https://arxiv.org/abs/2205.13147)
- [Binary and scalar embedding quantization](https://huggingface.co/blog/embedding-quantization)
- [MTEB paper](https://arxiv.org/abs/2210.07316)
- [MMTEB paper](https://arxiv.org/abs/2502.13595)
- [JMTEB](https://github.com/sbintuitions/JMTEB)
- [MIRACL](https://direct.mit.edu/tacl/article/doi/10.1162/tacl_a_00595/117438/MIRACL-A-Multilingual-Retrieval-Dataset-Covering)
- [Transformers.js WebGPU guide](https://huggingface.co/docs/transformers.js/guides/webgpu)
- [Bekko browser demo](https://huggingface.co/spaces/hotchpotch/bekko-embedding-web)

Repository measurements:

- [On-chain tokenizer measurements](2026-11-12-onchain-tokenizer-measurements.md)
- [ADR 0113: tokenizer ownership](../adr/0113-inference-canister-tokenizer.md)
- [ADR 0119: one vector operation, client intents](../adr/0119-one-vector-operation-client-intents.md)

## Starter movie gate (2026-11-13)

The first executable gate landed at
[`demo/movies/eval/EMBEDDING-EVAL.md`](../../demo/movies/eval/EMBEDDING-EVAL.md) with 21
queries (23 primary retrieval queries; structured genre/director/studio queries reported
separately). On the 2,000 Japanese synopses:

| Model | retrieval nDCG@10 | 95% CI | all nDCG@10 | Recall@10 | MRR |
| --- | ---: | ---: | ---: | ---: | ---: |
| `intfloat/multilingual-e5-small` | 0.363 | [0.249, 0.483] | 0.429 | 0.253 | 0.658 |
| `hotchpotch/bekko-embedding-v1-a8m` | 0.537 | [0.446, 0.624] | 0.595 | 0.386 | 0.855 |
| `ibm-granite/granite-embedding-97m-multilingual-r2` | 0.272 | [0.173, 0.373] | 0.295 | 0.142 | 0.611 |
| `minishlab/potion-multilingual-128M` (static) | 0.162 | [0.085, 0.255] | 0.147 | 0.084 | 0.376 |

The bootstrap intervals are wide; 23 primary queries cannot establish a statistically certain
winner.

Bekko leads the expanded starter gate, including query latency (3.7 ms vs 13.2 ms for E5-small on
this CPU path). Model2Vec is extremely cheap (0.1 ms/query, 0.2 s to encode 2,000 docs) but its static
representation is far behind on this Japanese corpus; it remains a canister-feasibility candidate,
not a quality winner. The result is not yet a model-selection decision: 23 primary queries, keyword-derived labels, no
independent annotation. Granite's underperformance also needs a model/configuration
check before it is treated as a model-quality result.

## Bekko ONNX/browser spike (2026-11-13)

- `hotchpotch/bekko-embedding-v1-a8m` publishes `onnx/model.onnx` (130,099,079 bytes) and a
  34,363,442-byte tokenizer.
- Exported ONNX vs PyTorch on 38 queries + 40 synopses: minimum cosine **0.999762**, mean
  **0.999875**, max absolute component delta **0.00355**. The export preserves the vector.
- The ONNX graph scored **0.542** retrieval nDCG@10 on the starter gate vs **0.537** for PyTorch;
  both intervals overlap. This is an export check, not a quality improvement.
- Node `@huggingface/transformers` loaded the local graph and produced a vector whose cosine to
  the Python ONNX vector is **1.0000002**.
- Headless Chromium + Transformers.js (remote model assets, WASM path): cold load **15,974 ms**
  including the 130 MB download; warm embedding **36.3 ms** for one query; 384 dimensions, values
  match the Node/ONNX vector.
- The first browser attempt with a local static-file model failed in Transformers.js tokenizer
  loading (`tokenizer_class` undefined); the remote-asset path works. Local-asset packaging,
  WebGPU measurement and cache verification remain open.
- The browser probe is `demo/movies/browser/bekko-probe.html` and is intentionally not called by
  the SDK.

## Canonical demo encoder switched to Bekko (2026-11-13)

The committed movie embeddings were regenerated with
`hotchpotch/bekko-embedding-v1-a8m` (`c721113d59a1d91b447450324f51c4b3332c924a`) into
`seeds/embeddings.jsonl`; the contract now has empty query/passage prefixes and mean pooling.
The vector index definition remains 384d/cosine/i8/ivf_flat. This is a demo-level switch based on
the starter gate and the ONNX equivalence check, not a final model decision: the primary retrieval
set is still 23 queries with keyword-derived labels. E5-small remains the baseline in
`eval/embedding-eval-2026-11-13.json`.
