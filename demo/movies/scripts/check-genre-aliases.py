#!/usr/bin/env python3
"""Check that the movie genre alias map covers exactly the graph's genre labels.

The map is routing data, not graph data: its keys must be canonical Genre.name values, and each
alias must be a non-empty alternate surface. JSON source order is not significant.
"""

from __future__ import annotations

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CATALOG = ROOT / "seeds" / "catalog.json"
ALIASES = ROOT / "domains" / "genre-aliases.json"


def main() -> int:
    catalog = json.loads(CATALOG.read_text(encoding="utf-8"))
    genres = {genre for film in catalog["films"] for genre in film["genres"]}
    aliases = json.loads(ALIASES.read_text(encoding="utf-8"))
    keys = set(aliases)
    if keys != genres:
        missing = sorted(genres - keys)
        extra = sorted(keys - genres)
        raise SystemExit(f"genre alias keys mismatch: missing={missing} extra={extra}")
    for value, surfaces in aliases.items():
        if not isinstance(surfaces, list) or not surfaces:
            raise SystemExit(f"genre {value!r}: aliases must be a non-empty list")
        if any(not isinstance(alias, str) or not alias.strip() for alias in surfaces):
            raise SystemExit(f"genre {value!r}: aliases must be non-empty strings")
        if len(set(surfaces)) != len(surfaces):
            raise SystemExit(f"genre {value!r}: duplicate alias")
    print(f"validated {len(keys)} genre aliases")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
