# Routing benchmark harness

Reproducible evaluation scripts for the model-based routing/slot pipeline.

## Ownership

This directory is the canonical **Gleaph acceptance and analysis harness**: it gates Gleaph
behaviour against the pinned Glev artifact set. Model-side training, the routing/slot schemas and
the general-router artifact exporter live in the Glev repository (`experiments/` and `tools/`); the
evaluation scripts are intentionally not duplicated there. They are local tools: tokenizers,
artifacts and external datasets live under `target/` and are not committed.

## Prerequisites

- The repository's Python environment (`target/venv`) with `torch`, `safetensors`, `ijson` and the
  local `laya` package.
- A trained slot/routing model directory, supplied as `GLEV_MODEL_DIR` (default
  `target/glev-model`). There is no installable pack/tokenizer asset (ADR 0116). The training and
  model-side experiment scripts live in the Glev repository; see
  `design/investigations/2026-11-10-canister-inference-optimization-research.md` for the trained
  checkpoints used.
- For the real-document benchmark: the compact corpus and OOS question files built from the public
  datasets (commands below).

## Real-document benchmark (`real_doc_bench.py`)

```bash
# 1. Raw downloads (about 600MB, not committed).
curl -L -o target/datasets/longbench-v2.json \
  https://huggingface.co/datasets/zai-org/LongBench-v2/resolve/main/data.json
curl -L -o target/datasets/jemhop_with_context.csv \
  https://huggingface.co/datasets/llm-jp/llm-jp-longbench-JEMHop/resolve/main/jemhop_with_context.csv
curl -L -o target/datasets/musique_ans_dev.jsonl \
  https://huggingface.co/datasets/dgslibisey/MuSiQue/resolve/main/musique_ans_v1.0_dev.jsonl

# 2. Compact corpora under $GLEV_DATA_DIR/corpus/ (default target/glev-data).
target/venv/bin/python demo/knowledge/scripts/routing-bench/build_external_corpus.py
target/venv/bin/python demo/knowledge/scripts/routing-bench/build_oos_questions.py

# 3. Holdout coverage and full-pipeline OOS refusal.
BENCH_CJK_POLICY=1 BENCH_SPLIT=holdout BENCH_CORPUS=external \
  BENCH_BUCKETS=64,96,128 BENCH_POSITIONS=end \
  target/venv/bin/python demo/knowledge/scripts/routing-bench/real_doc_bench.py \
  "${GLEV_MODEL_DIR:-target/glev-model}"
BENCH_CJK_POLICY=1 BENCH_OOS=real BENCH_SPLIT=holdout BENCH_CORPUS=external \
  BENCH_BUCKETS=64,96,128 BENCH_POSITIONS=end \
  target/venv/bin/python demo/knowledge/scripts/routing-bench/real_doc_bench.py \
  "${GLEV_MODEL_DIR:-target/glev-model}"
```

Useful switches: `BENCH_FILLER=doc|sentences`, `BENCH_CORPUS=repo|external|mixed`,
`BENCH_SOURCES=longbench-v2,llm-jp-longbench,musique`, `BENCH_THRESHOLD`, `BENCH_CJK_THRESHOLD`,
`BENCH_NONE_AGG=mean|min|max|trim|median`, `BENCH_FALLBACK=0|1`, `BENCH_VERBOSE=1`.

## Other harnesses

- `long_fusion_sweep.py` — overlapping-window tag fusion and span F1.
- `chunk_eval.py` — operation aggregation over concatenated long questions.
- `midlen_e2e_eval.py` — mid-length policy boundary and op+slot end-to-end.
- `long_agg_eval.py` — slot-activity op aggregation variants.
- `window_short_eval.py` — short-question window/selection heuristics.

Results and the adopted policy are recorded in
`design/investigations/2026-11-10-canister-inference-optimization-research.md`.

## Pinned Glev runtime

The canister embeds the frozen `glev-general-router` artifact from the pinned git revision; there
is no installable model or tokenizer asset (ADR 0114/0116). The pin lives in
`crates/gleaph-routing-canister/Cargo.toml` and `artifacts.toml`.
