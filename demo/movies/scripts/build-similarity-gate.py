#!/usr/bin/env python3
"""Freeze the metadata-derived labels used by the live similarity gate.

Two structural axes are supported:

- `same-director`: other films by any of the anchor's directors are grade 2.
- `franchise`: films in the same `followedBy` component as the anchor are grade 2.

Both are deterministic catalog metadata, not independent human relevance judgements;
`eval/SIMILARITY-GATE.md` records that limitation.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

SAME_DIRECTOR_ANCHORS = [
    ("Q155653", "ja"),
    ("Q186572", "ja"),
    ("Q21697406", "ja"),
    ("Q13417189", "ja"),
    ("Q1757288", "ja"),
    ("Q47703", "ja"),
    ("Q189540", "ja"),
    ("Q47482560", "ja"),
    ("Q155653", "en"),
    ("Q186572", "en"),
    ("Q21697406", "en"),
    ("Q13417189", "en"),
    ("Q1757288", "en"),
    ("Q189540", "en"),
    ("Q47482560", "en"),
]

FRANCHISE_ANCHORS = [
    ("Q47703", "ja"),
    ("Q102438", "ja"),
    ("Q127367", "ja"),
    ("Q91540", "ja"),
    ("Q167726", "ja"),
    ("Q248775", "ja"),
    ("Q1757288", "ja"),
    ("Q46717", "ja"),
    ("Q162255", "ja"),
    ("Q1757288", "en"),
]


def franchise_components(films: list[dict]) -> dict[str, str]:
    parent = {film["qid"]: film["qid"] for film in films}

    def find(qid: str) -> str:
        while parent[qid] != qid:
            parent[qid] = parent[parent[qid]]
            qid = parent[qid]
        return qid

    def union(left: str, right: str) -> None:
        left_root, right_root = find(left), find(right)
        if left_root != right_root:
            parent[right_root] = left_root

    for film in films:
        followed = film.get("followedBy")
        if followed in parent:
            union(film["qid"], followed)
    return {qid: find(qid) for qid in parent}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--axis",
        choices=("same-director", "franchise"),
        default="same-director",
    )
    args = parser.parse_args()
    catalog = json.loads((ROOT / "seeds/catalog.json").read_text(encoding="utf-8"))
    films = catalog["films"]
    by_qid = {film["qid"]: film for film in films}
    components = franchise_components(films) if args.axis == "franchise" else {}
    anchors = SAME_DIRECTOR_ANCHORS if args.axis == "same-director" else FRANCHISE_ANCHORS
    rows = []
    for qid, lang in anchors:
        film = by_qid[qid]
        if args.axis == "same-director":
            director_ids = {director["qid"] for director in film.get("directors", [])}
            relevant = {
                other["qid"]: 2
                for other in films
                if other["qid"] != qid
                and any(
                    director["qid"] in director_ids
                    for director in other.get("directors", [])
                )
            }
            note = "metadata-derived same-director relevance; not an independent human label"
        else:
            root = components[qid]
            relevant = {
                other["qid"]: 2
                for other in films
                if other["qid"] != qid and components[other["qid"]] == root
            }
            note = "metadata-derived franchise relevance; not an independent human label"
        if not relevant:
            raise SystemExit(f"anchor {qid} has no {args.axis} films")
        anchor = film["titleEn"] if lang == "en" else film["titleJa"]
        if not anchor:
            raise SystemExit(f"anchor {qid} has no {lang} title")
        rows.append(
            {
                "id": f"{args.axis}-{qid}-{lang}",
                "query": (
                    f"Films similar to {anchor}"
                    if lang == "en"
                    else f"{anchor}に似ている映画"
                ),
                "lang": lang,
                "category": f"{args.axis}-{lang}",
                "anchor": anchor,
                "anchor_qid": qid,
                "operation": (
                    "films-similar-to-en" if lang == "en" else "films-similar-to-ja"
                ),
                "relevant": dict(sorted(relevant.items())),
                "note": note,
            }
        )
    suffix = "" if args.axis == "same-director" else f"-{args.axis}"
    target = ROOT / f"eval/similarity-gate{suffix}-2026-11-13.jsonl"
    target.write_text(
        "\n".join(json.dumps(row, ensure_ascii=False) for row in rows) + "\n",
        encoding="utf-8",
    )
    print(f"wrote {len(rows)} queries to {target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
