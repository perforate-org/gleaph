#!/usr/bin/env python3
"""Offline relevance comparison for movie embedding models.

This is an evaluation tool, not the SDK. It loads only the models named on the command line and
never changes the committed embeddings or the live vector index.

Usage:
  ../../target/venv/bin/python scripts/eval-embeddings.py --models e5-small
  ../../target/venv/bin/python scripts/eval-embeddings.py --models e5-small,bekko-a8m,granite-97m
"""

from __future__ import annotations

import argparse
import json
import math
import os
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
# Must be set before importing transformers so downloaded model assets use the project cache.
os.environ.setdefault("HF_HOME", str(ROOT / ".cache" / "huggingface"))

import numpy as np
import torch
from transformers import AutoModel, AutoTokenizer

MODEL_SPECS = {
    "e5-small": {
        "hf": "intfloat/multilingual-e5-small",
        "query_prefix": "query: ",
        "passage_prefix": "passage: ",
        "pooling": "mean",
        "max_length": 512,
    },
    "bekko-a8m": {
        "hf": "hotchpotch/bekko-embedding-v1-a8m",
        "query_prefix": "",
        "passage_prefix": "",
        "pooling": "mean",
        "max_length": 512,
    },
    "granite-97m": {
        "hf": "ibm-granite/granite-embedding-97m-multilingual-r2",
        "query_prefix": "",
        "passage_prefix": "",
        "pooling": "cls",
        "max_length": 512,
    },
}


def load_catalog() -> list[dict]:
    catalog = json.loads((ROOT / "seeds/catalog.json").read_text())
    return [
        film
        for film in catalog["films"]
        if film.get("synopsis") and isinstance(film.get("year"), int)
    ]


def load_queries(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text().splitlines() if line.strip()]


def load_model(model_id: str):
    tokenizer = AutoTokenizer.from_pretrained(model_id)
    model = AutoModel.from_pretrained(model_id)
    model = model.float()
    model.eval()
    return tokenizer, model


def pool(hidden: torch.Tensor, mask: torch.Tensor, kind: str) -> torch.Tensor:
    if kind == "cls":
        return hidden[:, 0]
    if kind == "mean":
        mask_f = mask.unsqueeze(-1).float()
        return (hidden * mask_f).sum(1) / mask_f.sum(1).clamp(min=1e-6)
    raise ValueError(f"unknown pooling {kind}")


@torch.no_grad()
def encode(
    texts: list[str],
    tokenizer,
    model,
    *,
    prefix: str,
    pooling: str,
    max_length: int,
    batch_size: int = 32,
) -> np.ndarray:
    vectors: list[np.ndarray] = []
    for start in range(0, len(texts), batch_size):
        batch = [prefix + text for text in texts[start : start + batch_size]]
        encoded = tokenizer(
            batch,
            padding=True,
            truncation=True,
            max_length=max_length,
            return_tensors="pt",
        )
        hidden = model(**encoded).last_hidden_state
        pooled = pool(hidden, encoded["attention_mask"], pooling)
        pooled = torch.nn.functional.normalize(pooled, p=2, dim=1)
        vectors.append(pooled.cpu().numpy())
    return np.concatenate(vectors, axis=0) if vectors else np.empty((0, 0))


def dcg(grades: list[int]) -> float:
    return sum((2**grade - 1) / math.log2(index + 2) for index, grade in enumerate(grades))


def evaluate_query(
    scores: np.ndarray,
    relevant: dict[str, int],
    qids: list[str],
    k: int,
) -> dict:
    order = np.argsort(-scores)[:k]
    retrieved = [qids[int(index)] for index in order]
    grades = [relevant.get(qid, 0) for qid in retrieved]
    ideal = sorted(relevant.values(), reverse=True)[:k]
    ideal_dcg = dcg(ideal)
    ndcg = dcg(grades) / ideal_dcg if ideal_dcg else None
    relevant_count = len(relevant)
    hits = sum(1 for grade in grades if grade > 0)
    first = next((index + 1 for index, grade in enumerate(grades) if grade > 0), None)
    return {
        "retrieved": retrieved,
        "grades": grades,
        "ndcg_at_10": ndcg,
        "recall_at_10": hits / relevant_count if relevant_count else None,
        "mrr": 1.0 / first if first else 0.0,
        "hit_at_10": bool(hits),
        "false_positive_at_10": relevant_count == 0 and bool(retrieved),
    }


def run_model(spec: dict, docs: list[dict], queries: list[dict], k: int) -> dict:
    started = time.perf_counter()
    tokenizer, model = load_model(spec["hf"])
    load_seconds = time.perf_counter() - started
    revision = getattr(model.config, "_commit_hash", None)
    parameters = sum(parameter.numel() for parameter in model.parameters())
    model_bytes = sum(
        parameter.numel() * parameter.element_size() for parameter in model.parameters()
    )

    started = time.perf_counter()
    doc_vectors = encode(
        [film["synopsis"] for film in docs],
        tokenizer,
        model,
        prefix=spec["passage_prefix"],
        pooling=spec["pooling"],
        max_length=spec["max_length"],
    )
    doc_seconds = time.perf_counter() - started
    qids = [film["qid"] for film in docs]

    per_query = []
    query_seconds = 0.0
    for item in queries:
        started = time.perf_counter()
        query_vector = encode(
            [item["query"]],
            tokenizer,
            model,
            prefix=spec["query_prefix"],
            pooling=spec["pooling"],
            max_length=spec["max_length"],
        )[0]
        query_seconds += time.perf_counter() - started
        metrics = evaluate_query(doc_vectors @ query_vector, item["relevant"], qids, k)
        per_query.append({**item, **metrics})

    scored = [row for row in per_query if row["ndcg_at_10"] is not None]
    by_category: dict[str, list[dict]] = {}
    for row in scored:
        by_category.setdefault(row["category"], []).append(row)
    retrieval_categories = {"semantic", "cross-lingual", "similar-film"}
    retrieval_rows = [
        row for row in scored if row["category"] in retrieval_categories
    ]
    return {
        "model": spec["hf"],
        "revision": revision,
        "parameters": parameters,
        "model_bytes": model_bytes,
        "load_seconds": load_seconds,
        "document_encode_seconds": doc_seconds,
        "query_encode_seconds_total": query_seconds,
        "query_count": len(queries),
        "mean_ndcg_at_10": float(np.mean([row["ndcg_at_10"] for row in scored])),
        "mean_recall_at_10": float(
            np.mean([row["recall_at_10"] for row in scored if row["recall_at_10"] is not None])
        ),
        "mean_mrr": float(np.mean([row["mrr"] for row in scored])),
        "retrieval_mean_ndcg_at_10": (
            float(np.mean([row["ndcg_at_10"] for row in retrieval_rows]))
            if retrieval_rows
            else None
        ),
        "retrieval_query_count": len(retrieval_rows),
        "out_of_scope_count": sum(1 for row in per_query if not row["relevant"]),
        "out_of_scope_false_positive_at_10": sum(
            1
            for row in per_query
            if not row["relevant"] and row["false_positive_at_10"]
        ),
        "by_category": {
            category: {
                "count": len(rows),
                "mean_ndcg_at_10": float(np.mean([row["ndcg_at_10"] for row in rows])),
                "mean_recall_at_10": float(
                    np.mean(
                        [
                            row["recall_at_10"]
                            for row in rows
                            if row["recall_at_10"] is not None
                        ]
                    )
                ),
                "mean_mrr": float(np.mean([row["mrr"] for row in rows])),
            }
            for category, rows in sorted(by_category.items())
        },
        "per_query": per_query,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--models", default="e5-small")
    parser.add_argument(
        "--queries",
        type=Path,
        default=ROOT / "eval/relevance-2026-11-13.jsonl",
    )
    parser.add_argument("--limit-queries", type=int)
    parser.add_argument("--top-k", type=int, default=10)
    parser.add_argument("--output", type=Path)
    args = parser.parse_args()

    docs = load_catalog()
    queries = load_queries(args.queries)
    if args.limit_queries:
        queries = queries[: args.limit_queries]
    results = []
    for name in [part.strip() for part in args.models.split(",") if part.strip()]:
        if name not in MODEL_SPECS:
            parser.error(f"unknown model {name!r}; choose from {', '.join(MODEL_SPECS)}")
        result = run_model(MODEL_SPECS[name], docs, queries, args.top_k)
        result["name"] = name
        results.append(result)
        retrieval = result["retrieval_mean_ndcg_at_10"]
        print(
            f"{name:12s} retrieval_nDCG@10={retrieval:.3f} "
            f"all_nDCG@10={result['mean_ndcg_at_10']:.3f} "
            f"Recall@10={result['mean_recall_at_10']:.3f} "
            f"MRR={result['mean_mrr']:.3f} "
            f"params={result['parameters'] / 1e6:.1f}M "
            f"load={result['load_seconds']:.1f}s"
        )
    payload = {"queries": str(args.queries), "documents": len(docs), "results": results}
    text = json.dumps(payload, ensure_ascii=False, indent=2) + "\n"
    if args.output:
        args.output.write_text(text)
        print(f"wrote {args.output}")
    else:
        print(text)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
