#!/usr/bin/env python3
"""Evaluate the movie embedding candidates on a small JMTEB retrieval subset.

This is an external, independent-label signal for model selection. It downloads the parquet
splits directly from `sbintuitions/JMTEB` (no `datasets` dependency) and does not touch the movie
catalog, committed embeddings, or the live vector index.

Usage:
  ../../target/venv/bin/python scripts/eval-jmteb.py --models e5-small,bekko-a8m
  ../../target/venv/bin/python scripts/eval-jmteb.py --tasks nlp_journal_abs_article,nlp_journal_title_abs
"""

from __future__ import annotations

import argparse
import hashlib
import json
import math
import sys
import urllib.request
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq

import importlib.util

_EMBEDDING_EVAL = importlib.util.spec_from_file_location(
    "movie_eval_embeddings", Path(__file__).with_name("eval-embeddings.py")
)
assert _EMBEDDING_EVAL is not None and _EMBEDDING_EVAL.loader is not None
_EMBEDDING_EVAL_MODULE = importlib.util.module_from_spec(_EMBEDDING_EVAL)
_EMBEDDING_EVAL.loader.exec_module(_EMBEDDING_EVAL_MODULE)
MODEL_SPECS = _EMBEDDING_EVAL_MODULE.MODEL_SPECS
bootstrap_mean_ci = _EMBEDDING_EVAL_MODULE.bootstrap_mean_ci
load_encoder = _EMBEDDING_EVAL_MODULE.load_encoder

ROOT = Path(__file__).resolve().parent.parent
CACHE = ROOT / ".cache/jmteb"
HF_BASE = "https://huggingface.co/datasets/sbintuitions/JMTEB/resolve/main/data"
DEFAULT_TASKS = (
    "nlp_journal_abs_article",
    "nlp_journal_title_abs",
    "nlp_journal_title_intro",
    "nlp_journal_abs_intro",
)


def fetch(url: str, target: Path) -> Path:
    if target.exists():
        return target
    target.parent.mkdir(parents=True, exist_ok=True)
    with urllib.request.urlopen(url, timeout=120) as response:
        body = response.read()
    target.write_bytes(body)
    return target


def load_task(task: str, split: str) -> dict:
    query_path = CACHE / f"{task}_query_{split}.parquet"
    corpus_path = CACHE / f"{task}_corpus.parquet"
    fetch(f"{HF_BASE}/{task}-query/{split}.parquet", query_path)
    fetch(f"{HF_BASE}/{task}-corpus/corpus.parquet", corpus_path)
    query_rows = pq.read_table(query_path).to_pylist()
    corpus_rows = pq.read_table(corpus_path).to_pylist()
    queries = []
    for row in query_rows:
        relevant = row["relevant_docs"]
        if isinstance(relevant, str):
            relevant = [part for part in relevant.split(",") if part]
        queries.append({"qid": str(row["qid"]), "query": row["query"], "relevant": set(relevant)})
    docs = [(str(row["docid"]), row["text"]) for row in corpus_rows]
    return {
        "task": task,
        "split": split,
        "query_path": query_path,
        "corpus_path": corpus_path,
        "query_sha256": hashlib.sha256(query_path.read_bytes()).hexdigest(),
        "corpus_sha256": hashlib.sha256(corpus_path.read_bytes()).hexdigest(),
        "queries": queries,
        "docs": docs,
    }


def dcg(grades: list[int]) -> float:
    return sum((2**grade - 1) / math.log2(index + 2) for index, grade in enumerate(grades))


def evaluate_query(scores: np.ndarray, docids: list[str], relevant: set[str], k: int) -> dict:
    order = np.argsort(-scores)[:k]
    retrieved = [docids[int(index)] for index in order]
    grades = [1 if docid in relevant else 0 for docid in retrieved]
    ideal = [1] * min(len(relevant), k)
    ndcg = dcg(grades) / dcg(ideal) if ideal else None
    hits = sum(grades)
    first = next((index + 1 for index, grade in enumerate(grades) if grade), None)
    return {
        "retrieved": retrieved,
        "ndcg_at_10": ndcg,
        "recall_at_10": hits / len(relevant) if relevant else None,
        "mrr": 1.0 / first if first else 0.0,
    }


def mean(values):
    present = [value for value in values if value is not None]
    return float(np.mean(present)) if present else None


def run_model(name: str, encoder, task: dict, k: int) -> dict:
    spec = MODEL_SPECS[name]
    docids = [docid for docid, _ in task["docs"]]
    doc_vectors = encoder.encode([text for _, text in task["docs"]])
    per_query = []
    for query in task["queries"]:
        vector = encoder.encode([query["query"]], query=True)[0]
        metrics = evaluate_query(doc_vectors @ vector, docids, query["relevant"], k)
        per_query.append(
            {
                "qid": query["qid"],
                "query": query["query"],
                "relevant": sorted(query["relevant"]),
                **metrics,
            }
        )
    return {
        "model": name,
        "hf": spec["hf"],
        "revision": encoder.revision,
        "parameters": encoder.parameters,
        "model_bytes": encoder.model_bytes,
        "query_count": len(per_query),
        "document_count": len(docids),
        "mean_ndcg_at_10": mean([row["ndcg_at_10"] for row in per_query]),
        "mean_recall_at_10": mean([row["recall_at_10"] for row in per_query]),
        "mean_mrr": mean([row["mrr"] for row in per_query]),
        "ndcg_ci95": bootstrap_mean_ci([row["ndcg_at_10"] for row in per_query]),
        "per_query": per_query,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--models", default="e5-small,bekko-a8m")
    parser.add_argument("--tasks", default=",".join(DEFAULT_TASKS))
    parser.add_argument("--split", default="test")
    parser.add_argument("--top-k", type=int, default=10)
    parser.add_argument("--output", type=Path, default=ROOT / "eval/jmteb-2026-11-13.json")
    parser.add_argument("--include-per-query", action="store_true")
    args = parser.parse_args()
    if not (ROOT / "seeds/catalog.json").exists():
        print("run from the movie demo", file=sys.stderr)
        return 1
    tasks = [task for task in args.tasks.split(",") if task]
    models = [model for model in args.models.split(",") if model]
    for model in models:
        if model not in MODEL_SPECS:
            parser.error(f"unknown model {model!r}")
    encoders = {model: load_encoder(MODEL_SPECS[model]) for model in models}
    results = []
    for task_name in tasks:
        task = load_task(task_name, args.split)
        for model in models:
            result = run_model(model, encoders[model], task, args.top_k)
            result["task"] = task_name
            result["query_sha256"] = task["query_sha256"]
            result["corpus_sha256"] = task["corpus_sha256"]
            results.append(result)
            print(
                f"{task_name:30s} {model:12s} nDCG@10={result['mean_ndcg_at_10']:.3f} "
                f"Recall@10={result['mean_recall_at_10']:.3f} MRR={result['mean_mrr']:.3f}"
            )
    aggregate = {}
    for model in models:
        rows = [result for result in results if result["model"] == model]
        aggregate[model] = {
            "mean_ndcg_at_10": mean([row["mean_ndcg_at_10"] for row in rows]),
            "mean_recall_at_10": mean([row["mean_recall_at_10"] for row in rows]),
            "mean_mrr": mean([row["mean_mrr"] for row in rows]),
            "task_count": len(rows),
        }
    if not args.include_per_query:
        for result in results:
            result.pop("per_query", None)
    payload = {
        "split": args.split,
        "top_k": args.top_k,
        "tasks": [
            {
                "task": task,
                "query_sha256": next(
                    row["query_sha256"] for row in results if row["task"] == task
                ),
                "corpus_sha256": next(
                    row["corpus_sha256"] for row in results if row["task"] == task
                ),
            }
            for task in tasks
        ],
        "aggregate": aggregate,
        "results": results,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n")
    print(f"wrote {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
