#!/usr/bin/env python3
"""Collect the MovieLens natural-language query dataset as raw query candidates.

Source: Kang et al., "Understanding How People Use Natural Language to Ask for
Recommendations" (RecSys 2017), MovieLens.org queries collected in May 2016.
Dataset: https://conservancy.umn.edu/items/... (responses.csv, CC BY 3.0 US).
The raw CSV is downloaded under target/ and is not committed; the curated,
labelled gate lives in demo/movies/eval/real-queries-2026-11-12.jsonl.

The raw JSONL stays under target/; only the curated, labelled subset in
demo/movies/eval/real-queries-2026-11-12.jsonl is committed.

Usage:
  python3 demo/movies/scripts/collect-real-queries.py
  python3 demo/movies/scripts/collect-real-queries.py --check
"""

from __future__ import annotations

import argparse
import csv
import json
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
RAW = ROOT / "target" / "datasets" / "movielens-query"
CSV_URL = (
    "https://conservancy.umn.edu/server/api/core/bitstreams/"
    "76e4283f-9e49-4ecf-9659-1ae8cd584ab3/content"
)
OUT = RAW / "queries.raw.jsonl"
CITATION = (
    "Kang, Condi, Chang, Konstan, Terveen, Harper. "
    "Understanding How People Use Natural Language to Ask for Recommendations. RecSys 2017."
)


def download(check: bool) -> Path:
    path = RAW / "responses.csv"
    if path.exists():
        return path
    if check:
        raise SystemExit(f"missing {path}; run without --check to download")
    RAW.mkdir(parents=True, exist_ok=True)
    print(f"downloading {CSV_URL}", flush=True)
    urllib.request.urlretrieve(CSV_URL, path)
    return path


def read_queries(path: Path) -> list[dict]:
    # The upstream CSV is cp1252, not UTF-8.
    with path.open(newline="", encoding="cp1252") as handle:
        rows = list(csv.DictReader(handle))
    seen: set[str] = set()
    queries: list[dict] = []
    for row_index, row in enumerate(rows):
        for field in ("firstQuery", "followUpQuery"):
            question = (row.get(field) or "").strip()
            if len(question) < 2:
                continue
            key = question.casefold()
            if key in seen:
                continue
            seen.add(key)
            queries.append(
                {
                    "question": question,
                    "source": "movielens-2016",
                    "source_row": row_index,
                    "source_field": field,
                    "modality": row.get("modality") or None,
                    "license": "CC BY 3.0 US",
                    "citation": CITATION,
                }
            )
    return queries


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true", help="verify inputs without writing")
    args = parser.parse_args()
    path = download(args.check)
    queries = read_queries(path)
    if args.check:
        print(f"{len(queries)} unique queries")
        return 0
    OUT.parent.mkdir(parents=True, exist_ok=True)
    with OUT.open("w", encoding="utf-8") as handle:
        for query in queries:
            handle.write(json.dumps(query, ensure_ascii=False) + "\n")
    print(f"wrote {len(queries)} candidate queries to {OUT}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
