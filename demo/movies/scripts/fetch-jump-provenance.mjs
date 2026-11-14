#!/usr/bin/env node
// Resolve the catalog's Weekly Shonen Jump films to their source Work and Magazine.
//
//   node scripts/fetch-jump-provenance.mjs
//
// Films already enter the catalog through the P144/P179 franchise seeds in
// fetch-catalog.mjs. This script asks Wikidata for the P144 source of each jump-tier film and
// projects the result into the committed `seeds/jump-provenance.json` that build-seeds.mjs reads
// offline. The film -> work mapping is data, not queried at load time.

import { readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");
const QLEVER = "https://qlever.dev/api/wikidata";
const USER_AGENT = "gleaph-jump-provenance/0.1 (+https://gleaph.com)";

const IMPRINT = "ジャンプ";
const WEEKLY_JUMP = { qid: "Q220015", name: "週刊少年ジャンプ" };
const JUMP_PLUS = { qid: "Q55522683", name: "少年ジャンプ+" };

// P144 can land on the manga, a franchise item, or an intermediate anime. Keep the canonical
// source Work here; a film whose P144 points at an intermediate item is folded to its parent work.
const WORKS = {
  Q2020: { titleJa: "ドラゴンボール", titleEn: "Dragon Ball", kind: "franchise", magazine: WEEKLY_JUMP },
  Q673: { titleJa: "ONE PIECE", titleEn: "One Piece", kind: "franchise", magazine: WEEKLY_JUMP },
  Q275956: { titleJa: "聖闘士星矢", titleEn: "Saint Seiya", kind: "manga", magazine: WEEKLY_JUMP },
  Q714760: { titleJa: "SLAM DUNK", titleEn: "Slam Dunk", kind: "manga", magazine: WEEKLY_JUMP },
  Q65231576: { titleJa: "SPY×FAMILY", titleEn: "Spy × Family", kind: "manga", magazine: JUMP_PLUS },
  Q904: { titleJa: "BLEACH", titleEn: "Bleach", kind: "franchise", magazine: WEEKLY_JUMP },
  Q764135: { titleJa: "銀魂", titleEn: "Gintama", kind: "manga", magazine: WEEKLY_JUMP },
  Q216439: { titleJa: "るろうに剣心", titleEn: "Rurouni Kenshin", kind: "manga", magazine: WEEKLY_JUMP },
  Q733498: { titleJa: "北斗の拳", titleEn: "Fist of the North Star", kind: "manga", magazine: WEEKLY_JUMP },
  Q26971382: { titleJa: "NARUTO -ナルト-", titleEn: "Naruto", kind: "manga", magazine: WEEKLY_JUMP },
  Q24862683: { titleJa: "鬼滅の刃", titleEn: "Demon Slayer: Kimetsu no Yaiba", kind: "manga", magazine: WEEKLY_JUMP },
};
const WORK_ALIASES = { Q710324: "Q673" };

const catalog = JSON.parse(await readFile(join(ROOT, "seeds/catalog.json"), "utf8"));
const jumpQids = catalog.films.filter((film) => film.tier === "jump").map((film) => film.qid);
if (jumpQids.length === 0) throw new Error("catalog has no jump-tier films");

const values = jumpQids.map((qid) => `wd:${qid}`).join(" ");
const query = `PREFIX wd: <http://www.wikidata.org/entity/>
PREFIX wdt: <http://www.wikidata.org/prop/direct/>
SELECT ?film ?work WHERE {
  VALUES ?film { ${values} }
  { ?film wdt:P144 ?work } UNION { ?film wdt:P179 ?work }
}`;

const response = await fetch(QLEVER, {
  method: "POST",
  headers: {
    "content-type": "application/x-www-form-urlencoded",
    accept: "application/sparql-results+json",
    "user-agent": USER_AGENT,
  },
  body: new URLSearchParams({ query }).toString(),
});
if (!response.ok) throw new Error(`qlever ${response.status}: ${await response.text()}`);
const body = await response.json();
if (!body.results) throw new Error(`qlever: ${JSON.stringify(body).slice(0, 300)}`);

const byWork = new Map();
const unmapped = new Set(jumpQids);
for (const binding of body.results.bindings) {
  const film = binding.film.value.split("/").pop();
  const rawWork = binding.work.value.split("/").pop();
  const work = WORK_ALIASES[rawWork] ?? rawWork;
  if (!jumpQids.includes(film)) continue;
  if (!WORKS[work]) continue;
  unmapped.delete(film);
  const films = byWork.get(work) ?? [];
  if (!films.includes(film)) films.push(film);
  byWork.set(work, films);
}
if (unmapped.size > 0) {
  throw new Error(`jump films without a P144 work: ${[...unmapped].sort().join(", ")}`);
}

const magazines = new Map();
const works = [...byWork.entries()]
  .map(([qid, films]) => {
    const work = WORKS[qid];
    magazines.set(work.magazine.qid, work.magazine);
    return {
      qid,
      titleJa: work.titleJa,
      titleEn: work.titleEn,
      kind: work.kind,
      magazine: work.magazine.qid,
      films: films.sort(),
    };
  })
  .sort((left, right) => left.qid.localeCompare(right.qid));

const provenance = {
  version: 1,
  imprint: IMPRINT,
  magazines: [...magazines.values()].sort((left, right) => left.qid.localeCompare(right.qid)),
  works,
};
await writeFile(join(ROOT, "seeds/jump-provenance.json"), `${JSON.stringify(provenance, null, 2)}\n`);
console.log(
  `jump provenance: ${works.length} works, ${works.reduce((sum, work) => sum + work.films.length, 0)} films, ${provenance.magazines.length} magazines`,
);
