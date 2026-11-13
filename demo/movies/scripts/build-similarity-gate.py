#!/usr/bin/env python3
"""Freeze the metadata-derived labels used by the live similarity gate.

The labels are same-director relevance: for each anchor, other films by any of the anchor's
directors are grade 2 and every other film is grade 0. This is a structural seed, not an
independent human relevance judgement; `eval/SIMILARITY-GATE.md` records that limitation.
"""

from __future__ import annotations

import json
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent

ANCHORS = [
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


def main() -> int:
    catalog = json.loads((ROOT / "seeds/catalog.json").read_text(encoding="utf-8"))
    films = catalog["films"]
    by_qid = {film["qid"]: film for film in films}
    rows = []
    for qid, lang in ANCHORS:
        film = by_qid[qid]
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
        if not relevant:
            raise SystemExit(f"anchor {qid} has no same-director films")
        anchor = film["titleEn"] if lang == "en" else film["titleJa"]
        if not anchor:
            raise SystemExit(f"anchor {qid} has no {lang} title")
        rows.append(
            {
                "id": f"same-director-{qid}-{lang}",
                "query": (
                    f"Films similar to {anchor}"
                    if lang == "en"
                    else f"{anchor}に似ている映画"
                ),
                "lang": lang,
                "category": f"same-director-{lang}",
                "anchor": anchor,
                "anchor_qid": qid,
                "operation": (
                    "films-similar-to-en" if lang == "en" else "films-similar-to-ja"
                ),
                "relevant": dict(sorted(relevant.items())),
                "note": "metadata-derived same-director relevance; not an independent human label",
            }
        )
    target = ROOT / "eval/similarity-gate-2026-11-13.jsonl"
    target.write_text(
        "\n".join(json.dumps(row, ensure_ascii=False) for row in rows) + "\n",
        encoding="utf-8",
    )
    print(f"wrote {len(rows)} queries to {target}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
