#!/usr/bin/env python3
"""Attach English Wikipedia intro text to the movie catalog.

The graph's synopsis is Japanese Wikipedia text; English topic search needs an English text index.
For each catalog QID this script resolves the enwiki sitelink through Wikidata and downloads the
article intro through the Wikipedia API. Output is cached under `.cache/` while iterating; the
committed catalog is the source of truth once generated.

Usage:
  python3 demo/movies/scripts/fetch-english-synopses.py
  python3 demo/movies/scripts/fetch-english-synopses.py --check
"""

from __future__ import annotations

import argparse
import json
import time
import urllib.parse
import urllib.request
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
CATALOG = ROOT / "seeds" / "catalog.json"
CACHE = ROOT / ".cache" / "english-synopses.json"
USER_AGENT = "gleaph-demo-catalog/0.1 (+https://gleaph.com)"
WIKIDATA = "https://www.wikidata.org/w/api.php"
WIKIPEDIA = "https://en.wikipedia.org/w/api.php"
MAX_CHARS = 1200


def get_json(base: str, params: dict[str, str]) -> dict:
    url = f"{base}?{urllib.parse.urlencode(params)}"
    request = urllib.request.Request(url, headers={"User-Agent": USER_AGENT})
    for attempt in range(1, 7):
        try:
            with urllib.request.urlopen(request, timeout=30) as response:
                return json.load(response)
        except OSError as error:
            if attempt == 6:
                raise
            time.sleep(1.5 * attempt)
    raise AssertionError("unreachable")


def resolve_titles(qids: list[str], cache: dict) -> dict[str, str]:
    missing = [qid for qid in qids if qid not in cache]
    for start in range(0, len(missing), 50):
        batch = missing[start : start + 50]
        body = get_json(
            WIKIDATA,
            {
                "action": "wbgetentities",
                "ids": "|".join(batch),
                "props": "sitelinks",
                "sitefilter": "enwiki",
                "format": "json",
            },
        )
        for qid in batch:
            link = (
                body.get("entities", {})
                .get(qid, {})
                .get("sitelinks", {})
                .get("enwiki", {})
                .get("title")
            )
            cache[qid] = link
        time.sleep(0.1)
    return {qid: cache[qid] for qid in qids if cache.get(qid)}


def fetch_extracts(titles: list[str], cache: dict) -> dict[str, str]:
    missing = [title for title in titles if title not in cache]
    for start in range(0, len(missing), 20):
        batch = missing[start : start + 20]
        body = get_json(
            WIKIPEDIA,
            {
                "action": "query",
                "prop": "extracts",
                "explaintext": "1",
                "exintro": "1",
                "exlimit": "max",
                "redirects": "1",
                "format": "json",
                "titles": "|".join(batch),
            },
        )
        normalized = {
            item["from"]: item["to"] for item in body.get("query", {}).get("normalized", [])
        }
        redirected = {
            item["from"]: item["to"] for item in body.get("query", {}).get("redirects", [])
        }
        pages = {page["title"]: page.get("extract", "") for page in body.get("query", {}).get("pages", {}).values()}
        for requested in batch:
            title = normalized.get(requested, requested)
            title = redirected.get(title, title)
            cache[requested] = pages.get(title, "")
        time.sleep(0.2)
    return {title: cache.get(title, "") for title in titles}


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    catalog = json.loads(CATALOG.read_text(encoding="utf-8"))
    films = catalog["films"]
    if args.check:
        missing = [film["qid"] for film in films if not film.get("synopsisEn")]
        print(f"english synopses: {len(films) - len(missing)}/{len(films)}; missing={len(missing)}")
        return 0 if not missing else 1

    cache: dict = json.loads(CACHE.read_text(encoding="utf-8")) if CACHE.exists() else {}
    titles = resolve_titles([film["qid"] for film in films], cache)
    extracts = fetch_extracts(list(titles.values()), cache)
    CACHE.parent.mkdir(parents=True, exist_ok=True)
    CACHE.write_text(json.dumps(cache, ensure_ascii=False), encoding="utf-8")

    missing = 0
    for film in films:
        title = titles.get(film["qid"])
        extract = (extracts.get(title, "") if title else "").strip()
        if len(extract) < 80:
            missing += 1
            film.pop("synopsisEn", None)
            film.pop("sourceUrlEn", None)
            continue
        film["synopsisEn"] = extract[:MAX_CHARS].rstrip()
        film["sourceUrlEn"] = f"https://en.wikipedia.org/wiki/{urllib.parse.quote(title.replace(' ', '_'))}"
    lines = ["{", '"version": 1,', '"films": [']
    for index, film in enumerate(films):
        suffix = "," if index + 1 < len(films) else ""
        lines.append(json.dumps(film, ensure_ascii=False, separators=(",", ":")) + suffix)
    lines.extend([" ]", "}"])
    CATALOG.write_text("\n".join(lines) + "\n", encoding="utf-8")
    print(f"wrote {len(films) - missing}/{len(films)} english synopses; missing={missing}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
