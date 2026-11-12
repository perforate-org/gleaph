#!/usr/bin/env python3
"""Generate `seeds/embeddings.jsonl` for the movie demo.

    target/venv/bin/python demo/movies/scripts/gen-embeddings.py            # regenerate
    target/venv/bin/python demo/movies/scripts/gen-embeddings.py --check    # verify the pin

The model is pinned here and recorded in `seeds/embeddings.provenance.json`. The committed
JSONL is the source of truth; regeneration is provenance-pinned, not byte-reproducible (torch
kernels and the tokenizer are not bit-stable across environments).
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
from pathlib import Path

MODEL = "intfloat/multilingual-e5-small"
DIMENSIONS = 384
PASSAGE_PREFIX = "passage: "
ROOT = Path(__file__).resolve().parent.parent
# Keep the model cache inside the workspace: the sandbox does not allow writing ~/.cache.
os.environ.setdefault("HF_HOME", str(ROOT / ".cache" / "huggingface"))


def load_films() -> list[dict]:
    catalog = json.loads((ROOT / "seeds/catalog.json").read_text())
    # Same scope as scripts/build-seeds.mjs: the films that become Film vertices.
    return [
        film
        for film in catalog["films"]
        if film.get("synopsis")
        and film.get("directors")
        and film.get("cast")
        and isinstance(film.get("year"), int)
    ]


def encode(films: list[dict]) -> list[list[float]]:
    import torch
    from transformers import AutoModel, AutoTokenizer

    tokenizer = AutoTokenizer.from_pretrained(MODEL)
    model = AutoModel.from_pretrained(MODEL)
    model.eval()
    vectors: list[list[float]] = []
    with torch.no_grad():
        for start in range(0, len(films), 32):
            batch = films[start : start + 32]
            encoded = tokenizer(
                [PASSAGE_PREFIX + film["synopsis"] for film in batch],
                padding=True,
                truncation=True,
                max_length=512,
                return_tensors="pt",
            )
            hidden = model(**encoded).last_hidden_state
            mask = encoded["attention_mask"].unsqueeze(-1).float()
            pooled = (hidden * mask).sum(1) / mask.sum(1).clamp(min=1e-6)
            pooled = torch.nn.functional.normalize(pooled, p=2, dim=1)
            vectors.extend(pooled.tolist())
    return vectors


def write(films: list[dict], vectors: list[list[float]]) -> None:
    lines = []
    for film, vector in zip(films, vectors, strict=True):
        assert len(vector) == DIMENSIONS, len(vector)
        rounded = [round(value, 5) for value in vector]
        lines.append(
            json.dumps({"source_id": f"f-{film['qid']}", "values": rounded}, separators=(",", ":"))
        )
    body = "\n".join(lines) + "\n"
    (ROOT / "seeds/embeddings.jsonl").write_text(body)
    digest = hashlib.sha256(body.encode()).hexdigest()
    (ROOT / "seeds/embeddings.sha256").write_text(digest + "\n")
    (ROOT / "seeds/embeddings.provenance.json").write_text(
        json.dumps(
            {
                "model": MODEL,
                "dimensions": DIMENSIONS,
                "pooling": "mean",
                "normalised": True,
                "prefix": PASSAGE_PREFIX,
                "rows": len(films),
                "sha256": digest,
            },
            indent=2,
        )
        + "\n"
    )
    print(f"wrote {len(films)} vectors ({DIMENSIONS}d), sha256 {digest[:16]}…")


def check() -> int:
    expected = (ROOT / "seeds/embeddings.sha256").read_text().strip()
    actual = hashlib.sha256((ROOT / "seeds/embeddings.jsonl").read_bytes()).hexdigest()
    rows = (ROOT / "seeds/embeddings.jsonl").read_text().strip().split("\n")
    print(f"rows {len(rows)}, sha256 {'ok' if actual == expected else 'MISMATCH'}")
    return 0 if actual == expected else 1


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    if args.check:
        return check()
    films = load_films()
    if not films:
        print("no films in seeds/catalog.json", file=sys.stderr)
        return 1
    write(films, encode(films))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
