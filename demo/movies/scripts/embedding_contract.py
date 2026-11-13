#!/usr/bin/env python3
"""Single source of truth for the movie demo's embedding space.

The model, prefixes, pooling and vector encoding are one contract. Generation, query encoding and
stored-vector lookup all load this file and fail before touching the Router when any field is
missing or mismatched. The canister never reads it: it sees only the little-endian f32 bytes.

Usage:
  target/venv/bin/python scripts/embedding_contract.py --check
"""

from __future__ import annotations

import argparse
import hashlib
import json
import struct
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Sequence

ROOT = Path(__file__).resolve().parent.parent
CONTRACT_PATH = ROOT / "seeds/embeddings.provenance.json"
EMBEDDINGS_PATH = ROOT / "seeds/embeddings.jsonl"
CONTRACT_VERSION = 1


@dataclass(frozen=True)
class EmbeddingContract:
    contract_version: int
    model: str
    dimensions: int
    query_prefix: str
    passage_prefix: str
    pooling: str
    normalized: bool
    similarity: str
    max_input_tokens: int
    model_revision: str | None = None
    rows: int | None = None
    sha256: str | None = None

    @classmethod
    def load(cls, path: Path = CONTRACT_PATH) -> "EmbeddingContract":
        try:
            raw = json.loads(path.read_text())
        except FileNotFoundError as error:
            raise SystemExit(f"embedding contract not found: {path}") from error
        required = [
            "contract_version",
            "model",
            "dimensions",
            "query_prefix",
            "passage_prefix",
            "pooling",
            "normalized",
            "similarity",
            "max_input_tokens",
        ]
        missing = [field for field in required if field not in raw]
        if missing:
            raise SystemExit(f"embedding contract is missing fields: {', '.join(missing)}")
        contract = cls(
            contract_version=int(raw["contract_version"]),
            model=str(raw["model"]),
            dimensions=int(raw["dimensions"]),
            query_prefix=str(raw["query_prefix"]),
            passage_prefix=str(raw["passage_prefix"]),
            pooling=str(raw["pooling"]),
            normalized=bool(raw["normalized"]),
            similarity=str(raw["similarity"]),
            max_input_tokens=int(raw["max_input_tokens"]),
            model_revision=raw.get("model_revision"),
            rows=raw.get("rows"),
            sha256=raw.get("sha256"),
        )
        contract.validate()
        return contract

    def validate(self) -> None:
        if self.contract_version != CONTRACT_VERSION:
            raise SystemExit(
                f"unsupported embedding contract version {self.contract_version}; "
                f"expected {CONTRACT_VERSION}"
            )
        if not self.model.strip():
            raise SystemExit("embedding contract model is empty")
        if self.dimensions <= 0:
            raise SystemExit("embedding contract dimensions must be positive")
        if not self.pooling.strip():
            raise SystemExit("embedding contract pooling is empty")
        if not self.similarity.strip():
            raise SystemExit("embedding contract similarity is empty")
        if self.max_input_tokens <= 0:
            raise SystemExit("embedding contract max_input_tokens must be positive")

    def validate_vector(self, values: Sequence[float]) -> None:
        if len(values) != self.dimensions:
            raise SystemExit(
                f"embedding dimension mismatch: expected {self.dimensions}, got {len(values)}"
            )

    def vector_bytes(self, values: Sequence[float]) -> bytes:
        self.validate_vector(values)
        return struct.pack(f"<{self.dimensions}f", *values)

    def with_run(self, rows: int, sha256: str) -> "EmbeddingContract":
        return replace(self, rows=rows, sha256=sha256)

    def provenance(self) -> dict:
        return {
            "contract_version": self.contract_version,
            "model": self.model,
            "model_revision": self.model_revision,
            "dimensions": self.dimensions,
            "query_prefix": self.query_prefix,
            "passage_prefix": self.passage_prefix,
            "pooling": self.pooling,
            "normalized": self.normalized,
            "similarity": self.similarity,
            "max_input_tokens": self.max_input_tokens,
            "rows": self.rows,
            "sha256": self.sha256,
        }


def check_embeddings(contract: EmbeddingContract, path: Path = EMBEDDINGS_PATH) -> dict:
    if contract.rows is None or contract.sha256 is None:
        raise SystemExit("embedding contract has no recorded rows/sha256")
    body = path.read_bytes()
    digest = hashlib.sha256(body).hexdigest()
    if digest != contract.sha256:
        raise SystemExit(
            f"embeddings sha256 mismatch: contract {contract.sha256}, file {digest}"
        )
    lines = body.decode("utf-8").strip().splitlines()
    if len(lines) != contract.rows:
        raise SystemExit(
            f"embeddings row mismatch: contract {contract.rows}, file {len(lines)}"
        )
    for line_number, line in enumerate(lines, start=1):
        try:
            record = json.loads(line)
        except json.JSONDecodeError as error:
            raise SystemExit(f"embeddings line {line_number} is not JSON: {error}") from error
        values = record.get("values")
        if not isinstance(values, list):
            raise SystemExit(f"embeddings line {line_number} has no values list")
        contract.validate_vector(values)
    return {
        "model": contract.model,
        "dimensions": contract.dimensions,
        "rows": contract.rows,
        "sha256": digest,
    }


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--json", action="store_true")
    args = parser.parse_args()
    contract = EmbeddingContract.load()
    if not args.check:
        parser.error("pass --check")
    result = check_embeddings(contract)
    if args.json:
        print(json.dumps(result, indent=2))
    else:
        print(
            f"contract ok: {result['model']} {result['dimensions']}d, "
            f"{result['rows']} rows, sha256 {result['sha256'][:16]}…"
        )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
