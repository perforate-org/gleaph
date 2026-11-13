#!/usr/bin/env python3
"""Check that the Bekko ONNX graph matches the PyTorch sentence encoder.

The browser/SDK embedding provider will run ONNX; this gate compares the exported graph's pooled,
normalized vectors with the PyTorch model on the movie relevance queries and a sample of synopses.
It is not a quality gate; use `eval-embeddings.py` for that.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

import numpy as np
import torch
from transformers import AutoModel, AutoTokenizer

ROOT = Path(__file__).resolve().parent.parent
MODEL = "hotchpotch/bekko-embedding-v1-a8m"
ONNX_DIR = ROOT / ".cache/bekko-onnx"


def pool(hidden: torch.Tensor, mask: torch.Tensor) -> torch.Tensor:
    mask_f = mask.unsqueeze(-1).float()
    return (hidden * mask_f).sum(1) / mask_f.sum(1).clamp(min=1e-6)


@torch.no_grad()
def encode_torch(texts, tokenizer, model, batch_size=32):
    out = []
    for start in range(0, len(texts), batch_size):
        encoded = tokenizer(
            texts[start : start + batch_size],
            padding=True,
            truncation=True,
            max_length=512,
            return_tensors="pt",
        )
        hidden = model(**encoded).last_hidden_state
        vectors = torch.nn.functional.normalize(pool(hidden, encoded["attention_mask"]), p=2, dim=1)
        out.append(vectors.cpu().numpy())
    return np.concatenate(out, axis=0)


def encode_onnx(texts, tokenizer, session, batch_size=32):
    out = []
    for start in range(0, len(texts), batch_size):
        encoded = tokenizer(
            texts[start : start + batch_size],
            padding=True,
            truncation=True,
            max_length=512,
            return_tensors="np",
        )
        feed = {
            item.name: encoded[item.name].astype(np.int64)
            for item in session.get_inputs()
        }
        hidden = session.run(None, feed)[0]
        mask = encoded["attention_mask"].astype(np.float32)
        pooled = (hidden * mask[..., None]).sum(1) / np.maximum(mask.sum(1, keepdims=True), 1e-6)
        pooled /= np.maximum(np.linalg.norm(pooled, axis=1, keepdims=True), 1e-12)
        out.append(pooled.astype(np.float32))
    return np.concatenate(out, axis=0)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--min-cosine", type=float, default=0.999)
    args = parser.parse_args()

    import onnxruntime as ort

    queries = [
        json.loads(line)["query"]
        for line in (ROOT / "eval/relevance-2026-11-13.jsonl").read_text().splitlines()
        if line.strip()
    ]
    films = json.loads((ROOT / "seeds/catalog.json").read_text())["films"]
    passages = [film["synopsis"] for film in films[:40]]
    texts = queries + passages

    torch_tokenizer = AutoTokenizer.from_pretrained(MODEL)
    torch_model = AutoModel.from_pretrained(MODEL).float().eval()
    torch_vectors = encode_torch(texts, torch_tokenizer, torch_model)

    onnx_tokenizer = AutoTokenizer.from_pretrained(ONNX_DIR, local_files_only=True)
    onnx_session = ort.InferenceSession(
        str(ONNX_DIR / "onnx/model.onnx"), providers=["CPUExecutionProvider"]
    )
    onnx_vectors = encode_onnx(texts, onnx_tokenizer, onnx_session)

    cosine = np.sum(torch_vectors * onnx_vectors, axis=1)
    max_abs = np.max(np.abs(torch_vectors - onnx_vectors), axis=1)
    print(f"texts={len(texts)} min_cosine={cosine.min():.8f} mean_cosine={cosine.mean():.8f}")
    print(f"max_abs_diff={max_abs.max():.8f}")
    if cosine.min() < args.min_cosine:
        print(f"FAIL: ONNX/PyTorch cosine {cosine.min():.8f} < {args.min_cosine}")
        return 1
    print("OK: Bekko ONNX matches the PyTorch sentence encoder")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
