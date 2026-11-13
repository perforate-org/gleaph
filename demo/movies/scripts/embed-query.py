#!/usr/bin/env python3
"""Embed a natural-language topic with the movie demo's pinned multilingual model.

This is the client-side half of semantic topic search. The graph stores the passage embeddings recorded in
`seeds/embeddings.provenance.json`; a topic query is encoded with the contract's query prefix and
passed to `films-by-embedding` as the `Value::Bytes` parameter. The canister never runs the model.

Usage:
  target/venv/bin/python scripts/embed-query.py "films about time travel"   # base64 bytes
  target/venv/bin/python scripts/embed-query.py --json "sharks"             # float list
  target/venv/bin/python scripts/embed-query.py --serve --port 8099         # HTTP /embed
"""

from __future__ import annotations

import argparse
import base64
import json
import os
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from embedding_contract import EmbeddingContract

ROOT = Path(__file__).resolve().parent.parent
os.environ.setdefault("HF_HOME", str(ROOT / ".cache" / "huggingface"))


def load(contract: EmbeddingContract):
    import torch
    from transformers import AutoModel, AutoTokenizer

    revision = contract.model_revision or None
    tokenizer = AutoTokenizer.from_pretrained(contract.model, revision=revision)
    model = AutoModel.from_pretrained(contract.model, revision=revision).float()
    model.eval()
    return torch, tokenizer, model


def encode(
    text: str, contract: EmbeddingContract, torch, tokenizer, model
) -> list[float]:
    encoded = tokenizer(
        [contract.query_prefix + text],
        padding=True,
        truncation=True,
        max_length=contract.max_input_tokens,
        return_tensors="pt",
    )
    with torch.no_grad():
        hidden = model(**encoded).last_hidden_state
        mask = encoded["attention_mask"].unsqueeze(-1).float()
        pooled = (hidden * mask).sum(1) / mask.sum(1).clamp(min=1e-6)
        pooled = torch.nn.functional.normalize(pooled, p=2, dim=1)
    return pooled[0].tolist()


def serve(port: int, contract: EmbeddingContract) -> int:
    torch, tokenizer, model = load(contract)

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802
            if self.path == "/health":
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"ok")
                return
            self.send_error(404)

        def do_POST(self):  # noqa: N802
            if self.path != "/embed":
                self.send_error(404)
                return
            length = int(self.headers.get("content-length", "0"))
            body = self.rfile.read(length).decode("utf-8")
            try:
                payload = json.loads(body)
                text = payload["text"]
            except (json.JSONDecodeError, KeyError):
                text = body.strip()
            if not text:
                self.send_error(400, "missing text")
                return
            values = encode(text, contract, torch, tokenizer, model)
            response = json.dumps(
                {
                    "contract_version": contract.contract_version,
                    "model": contract.model,
                    "model_revision": contract.model_revision,
                    "dimensions": contract.dimensions,
                    "base64": base64.b64encode(contract.vector_bytes(values)).decode("ascii"),
                }
            ).encode("utf-8")
            self.send_response(200)
            self.send_header("content-type", "application/json")
            self.send_header("content-length", str(len(response)))
            self.end_headers()
            self.wfile.write(response)

        def log_message(self, *_args):
            pass

    server = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    print(f"embedding server on http://127.0.0.1:{port}/embed", flush=True)
    server.serve_forever()
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("text", nargs="?")
    parser.add_argument("--json", action="store_true", help="print the float list, not base64")
    parser.add_argument("--serve", action="store_true")
    parser.add_argument("--port", type=int, default=8099)
    args = parser.parse_args()
    contract = EmbeddingContract.load()
    if args.serve:
        return serve(args.port, contract)
    if not args.text:
        parser.error("text is required unless --serve")
    torch, tokenizer, model = load(contract)
    values = encode(args.text, contract, torch, tokenizer, model)
    if args.json:
        print(json.dumps(values))
    else:
        print(base64.b64encode(contract.vector_bytes(values)).decode("ascii"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
