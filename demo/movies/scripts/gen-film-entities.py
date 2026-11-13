#!/usr/bin/env python3
"""Generate the film entity vocabulary used by local `gleaph ask --entities`.

The graph is the source of truth; this file is derived routing data, not a second catalog. It is
written to `domains/film-entities.json` and contains the Japanese title surface for each film.
"""

from __future__ import annotations

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "domains/film-entities.json"


def main() -> int:
    films = json.loads((ROOT / "seeds/catalog.json").read_text(encoding="utf-8"))["films"]
    documents: set[str] = set()
    for film in films:
        value = film.get("title")
        if isinstance(value, str) and value.strip():
            documents.add(value.strip())
    payload = {
        "concepts": [],
        "documents": sorted(documents),
        "persons": [],
        "teams": [],
        "document_ids": {},
    }
    OUT.write_text(json.dumps(payload, ensure_ascii=False, indent=2) + "\n", encoding="utf-8")
    print(f"wrote {len(documents)} film title surfaces to {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
