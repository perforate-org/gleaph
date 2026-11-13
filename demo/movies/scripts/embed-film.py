#!/usr/bin/env python3
"""Return the pinned synopsis embedding of one film as base64 little-endian f32 bytes.

This is the client-side similar-films intent. The graph already stores the passage embedding of
each film, so no model runs here; the stored vector is bound to `films-by-embedding` as
`Value::Bytes`.

Usage:
  target/venv/bin/python scripts/embed-film.py "千と千尋の神隠し"
  target/venv/bin/python scripts/embed-film.py --json "千と千尋の神隠し"
"""

from __future__ import annotations

import argparse
import base64
import json
import struct
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent


def load_catalog() -> dict[str, str]:
    catalog = json.loads((ROOT / "seeds/catalog.json").read_text())
    return {film["title"]: film["qid"] for film in catalog["films"]}


def load_embeddings() -> dict[str, list[float]]:
    embeddings: dict[str, list[float]] = {}
    for line in (ROOT / "seeds/embeddings.jsonl").read_text().splitlines():
        record = json.loads(line)
        embeddings[record["source_id"]] = record["values"]
    return embeddings


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("title")
    parser.add_argument("--json", action="store_true", help="print the f32 list instead of base64")
    args = parser.parse_args()

    titles = load_catalog()
    qid = titles.get(args.title)
    if qid is None:
        matches = [title for title in titles if title.casefold() == args.title.casefold()]
        if len(matches) == 1:
            qid = titles[matches[0]]
    if qid is None:
        parser.error(f"film title not found in the demo catalog: {args.title!r}")
    vector = load_embeddings().get(f"f-{qid}")
    if vector is None:
        parser.error(f"no synopsis embedding for film {args.title!r} ({qid})")
    if len(vector) != 384:
        parser.error(f"embedding has {len(vector)} dims; expected 384")
    if args.json:
        print(json.dumps(vector))
    else:
        payload = struct.pack("<384f", *vector)
        print(base64.b64encode(payload).decode())
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
