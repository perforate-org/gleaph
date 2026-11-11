"""Build a compact real-prose corpus from LongBench v2, llm-jp-longbench JEMHop, and MuSiQue.

Each output row is a short chunk of one source document so the benchmark can pack contexts of a
target token length from several sources. Raw downloads stay under target/datasets.
"""
import csv, json, re, sys

csv.field_size_limit(10**9)
from pathlib import Path

ROOT = Path(__file__).resolve().parents[4]
RAW = ROOT / "target" / "datasets"
OUT = ROOT / "target" / "laya-routing" / "corpus" / "external.jsonl"
MAX_CHARS = 1200


def chunks(text, max_chars=MAX_CHARS):
    text = re.sub(r"\s+", " ", text).strip()
    if not text:
        return []
    pieces, current = [], ""
    for sentence in re.split(r"(?<=[.!?。！？])\s+", text):
        if not sentence:
            continue
        if len(current) + len(sentence) + 1 > max_chars and current:
            pieces.append(current)
            current = sentence
        else:
            current = (current + " " + sentence).strip()
    if current:
        pieces.append(current)
    return pieces


def write_chunks(handle, source, doc_id, domain, text):
    for index, piece in enumerate(chunks(text)):
        if len(piece) < 120:
            continue
        handle.write(json.dumps({"source": source, "doc_id": doc_id, "domain": domain,
                                 "chunk": index, "text": piece}) + "\n")


def longbench(handle):
    import ijson
    kept = roww = 0
    with open(RAW / "longbench-v2.json", "rb") as raw:
        for item in ijson.items(raw, "item"):
            roww += 1
            if roww % 17 != 0:
                continue
            context = item.get("context") or ""
            if len(context) < 2000:
                continue
            write_chunks(handle, "longbench-v2", str(item.get("_id", roww)),
                         item.get("domain", "unknown"), context[:20000])
            kept += 1
            if kept >= 60:
                break
    print("longbench docs", kept)


def jemhop(handle):
    kept = 0
    with open(RAW / "jemhop_with_context.csv", newline="", errors="ignore") as raw:
        reader = csv.DictReader(raw)
        for index, row in enumerate(reader):
            if index % 23 != 0:
                continue
            context = row.get("context") or ""
            if len(context) < 2000:
                continue
            write_chunks(handle, "llm-jp-longbench", str(row.get("qid", index)),
                         "jemhop", context[:20000])
            kept += 1
            if kept >= 40:
                break
    print("jemhop docs", kept)


def musique(handle):
    kept = 0
    with open(RAW / "musique_ans_dev.jsonl", errors="ignore") as raw:
        for index, line in enumerate(raw):
            if index % 13 != 0:
                continue
            row = json.loads(line)
            paragraphs = row.get("paragraphs") or []
            if len(paragraphs) < 4:
                continue
            text = " ".join(paragraph.get("paragraph_text", "") for paragraph in paragraphs[:8])
            if len(text) < 2000:
                continue
            write_chunks(handle, "musique", str(row.get("id", index)), "wikipedia", text[:20000])
            kept += 1
            if kept >= 40:
                break
    print("musique docs", kept)


OUT.parent.mkdir(parents=True, exist_ok=True)
with open(OUT, "w") as handle:
    longbench(handle)
    jemhop(handle)
    musique(handle)
print("wrote", OUT, "chunks", sum(1 for _ in open(OUT)))
