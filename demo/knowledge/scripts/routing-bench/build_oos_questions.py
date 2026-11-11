"""Collect real out-of-scope questions (general QA, Japanese, multi-hop) for abstain calibration."""
import csv, json, re
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
RAW = ROOT / "target" / "datasets"
OUT = ROOT / "target" / "laya-routing" / "corpus" / "oos_questions.jsonl"
csv.field_size_limit(10**9)
rows = []


def clean(text, limit=240):
    text = re.sub(r"\s+", " ", text or "").strip()
    return text[:limit]


import ijson
with open(RAW / "longbench-v2.json", "rb") as raw:
    for index, item in enumerate(ijson.items(raw, "item")):
        if index % 3 != 0:
            continue
        question = clean(item.get("question"))
        if 10 <= len(question):
            rows.append({"source": "longbench-v2", "question": question})
        if len([row for row in rows if row["source"] == "longbench-v2"]) >= 150:
            break

with open(RAW / "jemhop_with_context.csv", newline="", errors="ignore") as raw:
    for index, row in enumerate(csv.DictReader(raw)):
        if index % 7 != 0:
            continue
        question = clean(row.get("question"))
        if 10 <= len(question):
            rows.append({"source": "llm-jp-longbench", "question": question})
        if len([row for row in rows if row["source"] == "llm-jp-longbench"]) >= 120:
            break

with open(RAW / "musique_ans_dev.jsonl", errors="ignore") as raw:
    for index, line in enumerate(raw):
        if index % 5 != 0:
            continue
        question = clean(json.loads(line).get("question"))
        if 10 <= len(question):
            rows.append({"source": "musique", "question": question})
        if len([row for row in rows if row["source"] == "musique"]) >= 120:
            break

OUT.parent.mkdir(parents=True, exist_ok=True)
with open(OUT, "w") as handle:
    for row in rows:
        handle.write(json.dumps(row) + "\n")
print("wrote", OUT, len(rows))
