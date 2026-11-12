#!/usr/bin/env python3
"""Validate the curated real-query gate against the movie prepared catalog.

Checks: JSONL shape, unique questions, known operation names, explicit
expected_op/expect_abstain, source/license provenance, and a minimum
supported/OOS balance. This is an offline data check; the routed evaluation
needs a running Router (see eval/REAL-QUERIES.md).
"""

from __future__ import annotations

import argparse
import json
from collections import Counter
from pathlib import Path

ROOT = Path(__file__).resolve().parents[3]
DEFAULT = ROOT / "demo" / "movies" / "eval" / "real-queries-2026-11-12.jsonl"
PREPARED = ROOT / "demo" / "movies" / "prepared"


def operations() -> set[str]:
    return {path.stem for path in PREPARED.glob("*.gql")}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("dataset", nargs="?", type=Path, default=DEFAULT)
    args = parser.parse_args()
    known = operations()
    rows = []
    with args.dataset.open(encoding="utf-8") as handle:
        for line_number, line in enumerate(handle, 1):
            line = line.strip()
            if not line or line.startswith("#"):
                continue
            row = json.loads(line)
            rows.append(row)
            for field in ("question", "source", "source_row", "source_field", "license"):
                if not row.get(field):
                    raise SystemExit(f"{args.dataset}:{line_number}: missing {field}")
            expected = row.get("expected_op")
            abstain = row.get("expect_abstain")
            if expected is None and abstain is not True:
                raise SystemExit(f"{args.dataset}:{line_number}: ambiguous label")
            if expected is not None and (abstain is not False or expected not in known):
                raise SystemExit(
                    f"{args.dataset}:{line_number}: bad expected_op {expected!r} "
                    f"(known: {sorted(known)})"
                )
            if row.get("expect_incomplete") and expected is None:
                raise SystemExit(f"{args.dataset}:{line_number}: incomplete needs expected_op")
    questions = [row["question"].casefold() for row in rows]
    if len(questions) != len(set(questions)):
        raise SystemExit("duplicate questions in the gate")
    complete = sum(
        row.get("expected_op") is not None and not row.get("expect_incomplete") for row in rows
    )
    incomplete = sum(bool(row.get("expect_incomplete")) for row in rows)
    oos = sum(bool(row.get("expect_abstain")) for row in rows)
    if complete < 10 or oos < 10 or complete + incomplete + oos != len(rows):
        raise SystemExit(
            f"gate is too small/inconsistent: complete={complete} incomplete={incomplete} oos={oos}"
        )
    print(
        f"validated {len(rows)} real queries: "
        f"complete={complete} incomplete={incomplete} oos={oos}"
    )
    print(f"operations: {dict(Counter(row.get('expected_op') for row in rows if row.get('expected_op')))}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
