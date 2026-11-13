#!/usr/bin/env python3
"""Evaluate the movie embedding candidates on the JMTEB JSTS task.

This adds the semantic-similarity family to the external JMTEB signal. It is a symmetric STS
evaluation: both sentences use the encoder's passage side (no task prompt), and the metric is
Spearman rank correlation against the human labels.

Usage:
  ../../target/venv/bin/python scripts/eval-jmteb-sts.py --models e5-small,bekko-a8m
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
from pathlib import Path

import numpy as np
import pyarrow.parquet as pq

_JMTeB = importlib.util.spec_from_file_location(
    "movie_eval_jmteb", Path(__file__).with_name("eval-jmteb.py")
)
assert _JMTeB is not None and _JMTeB.loader is not None
_JMTeB_MODULE = importlib.util.module_from_spec(_JMTeB)
_JMTeB.loader.exec_module(_JMTeB_MODULE)

CACHE = _JMTeB_MODULE.CACHE
HF_BASE = _JMTeB_MODULE.HF_BASE
MODEL_SPECS = _JMTeB_MODULE.MODEL_SPECS
load_encoder = _JMTeB_MODULE.load_encoder
fetch = _JMTeB_MODULE.fetch
ROOT = _JMTeB_MODULE.ROOT


def rankdata(values: np.ndarray) -> np.ndarray:
    order = np.argsort(values, kind="stable")
    ranks = np.empty(len(values), dtype=np.float64)
    index = 0
    while index < len(order):
        end = index + 1
        while end < len(order) and values[order[end]] == values[order[index]]:
            end += 1
        average = (index + end - 1) / 2.0 + 1.0
        ranks[order[index:end]] = average
        index = end
    return ranks


def spearman(left: np.ndarray, right: np.ndarray) -> float:
    left_ranks = rankdata(left) - (len(left) + 1) / 2.0
    right_ranks = rankdata(right) - (len(right) + 1) / 2.0
    denominator = np.linalg.norm(left_ranks) * np.linalg.norm(right_ranks)
    return float(left_ranks @ right_ranks / denominator) if denominator else None


def bootstrap_spearman(scores: np.ndarray, labels: np.ndarray, iterations: int = 1000) -> list[float]:
    rng = np.random.default_rng(0)
    values = []
    for _ in range(iterations):
        indices = rng.integers(0, len(labels), size=len(labels))
        values.append(spearman(scores[indices], labels[indices]))
    return [float(np.percentile(values, 2.5)), float(np.percentile(values, 97.5))]


def load_jsts(split: str) -> tuple[Path, list[str], list[str], np.ndarray]:
    path = CACHE / f"jsts_{split}.parquet"
    fetch(f"{HF_BASE}/jsts/{split}.parquet", path)
    rows = pq.read_table(path).to_pylist()
    return (
        path,
        [row["sentence1"] for row in rows],
        [row["sentence2"] for row in rows],
        np.asarray([float(row["label"]) for row in rows], dtype=np.float64),
    )


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--models", default="e5-small,bekko-a8m")
    parser.add_argument("--split", default="test")
    parser.add_argument("--output", type=Path, default=ROOT / "eval/jmteb-sts-2026-11-13.json")
    args = parser.parse_args()
    models = [model for model in args.models.split(",") if model]
    for model in models:
        if model not in MODEL_SPECS:
            parser.error(f"unknown model {model!r}")
    path, left, right, labels = load_jsts(args.split)
    results = []
    for model in models:
        encoder = load_encoder(MODEL_SPECS[model])
        left_vectors = encoder.encode(left)
        right_vectors = encoder.encode(right)
        scores = np.sum(left_vectors * right_vectors, axis=1)
        rho = spearman(scores, labels)
        results.append(
            {
                "model": model,
                "hf": MODEL_SPECS[model]["hf"],
                "revision": encoder.revision,
                "parameters": encoder.parameters,
                "model_bytes": encoder.model_bytes,
                "pair_count": len(labels),
                "spearman": rho,
                "spearman_ci95": bootstrap_spearman(scores, labels),
            }
        )
        print(f"{model:12s} JSTS Spearman={rho:.3f}")
    payload = {
        "task": "jsts",
        "split": args.split,
        "sha256": hashlib.sha256(path.read_bytes()).hexdigest(),
        "pair_count": len(labels),
        "results": results,
    }
    args.output.parent.mkdir(parents=True, exist_ok=True)
    args.output.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n")
    print(f"wrote {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
