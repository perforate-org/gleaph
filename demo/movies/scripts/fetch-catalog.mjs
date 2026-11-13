#!/usr/bin/env node
// Builds `seeds/catalog.json`: the movie demo's film catalog with Wikidata facts and
// Japanese Wikipedia intro text.
//
//   node scripts/fetch-catalog.mjs                 # full run (writes seeds/catalog.json)
//   node scripts/fetch-catalog.mjs --years 2000-2002 --target 60   # small smoke run
//
// Stages (each cached under .cache/ so a rerun only redoes what changed):
//   1. per-year popularity ranking + Japanese quota, from the QLever Wikidata index
//      (the public WDQS endpoint times out on the same query)
//   2. franchise seeds: Studio Ghibli, David Lynch, and Weekly Shonen Jump adaptations
//   3. entity facts: labels, P577 year, P57, P161/P725, P272, P136, P156, jawiki sitelink
//   4. Japanese Wikipedia intro extracts (`prop=extracts&exintro`) with their source URL
//
// The output is committed. `scripts/build-seeds.mjs` and its test read it offline; nothing
// in the demo build path touches the network.

import { mkdir, readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
const ROOT = join(HERE, "..");
const CACHE = join(ROOT, ".cache");

const QLEVER = "https://qlever.dev/api/wikidata";
const WIKIDATA = "https://www.wikidata.org/w/api.php";
const WIKIPEDIA = "https://ja.wikipedia.org/w/api.php";
const USER_AGENT = "gleaph-demo-catalog/0.1 (+https://gleaph.com)";

const FIRST_YEAR = 1950;
const LAST_YEAR = 2025;
/** Candidates fetched per year before the popularity trim. */
const PER_YEAR = 40;
/** Japanese-film quota per year (P495 = Japan). */
const PER_YEAR_JAPAN = 12;
/** Final catalog size. */
const TARGET = 2000;
/** Candidates carried into the expensive per-entity stages (synopsis gaps are dropped). */
const FINALISTS = Math.round(TARGET * 1.25);
const CAST_LIMIT = 5;
const SYNOPSIS_MAX_CHARS = 480;
const GENRE_LIMIT = 2;

const STUDIO_GHIBLI = "Q182950";
const DAVID_LYNCH = "Q2071";
/** Weekly Shonen Jump manga/franchise items; films are reached through P144 (based on) and
 *  P179 (part of the series). */
const SHONEN_JUMP = {
  "ドラゴンボール": "Q2020",
  "ONE PIECE": "Q673",
  NARUTO: "Q26971382",
  BLEACH: "Q904",
  銀魂: "Q764135",
  僕のヒーローアカデミア: "Q18047903",
  "ハイキュー!!": "Q326642",
  "SPY×FAMILY": "Q65231576",
  チェンソーマン: "Q65240543",
  るろうに剣心: "Q216439",
  "幽☆遊☆白書": "Q18225473",
  "HUNTER×HUNTER": "Q696071",
  ジョジョの奇妙な冒険: "Q696918",
  北斗の拳: "Q733498",
  キャプテン翼: "Q647486",
  "Dr.スランプ": "Q18008936",
  聖闘士星矢: "Q275956",
  "SLAM DUNK": "Q714760",
  呪術廻戦: "Q55522899",
  鬼滅の刃: "Q24862683",
};

/** Genre keywords in preference order; the first match wins and labels are normalised. */
const GENRE_KEYWORDS = [
  ["アニメーション", "アニメーション映画"],
  ["SF", "SF映画"],
  ["サイエンス", "SF映画"],
  ["ホラー", "ホラー映画"],
  ["犯罪", "犯罪映画"],
  ["スリラー", "スリラー映画"],
  ["ミステリー", "ミステリー映画"],
  ["アクション", "アクション映画"],
  ["アドベンチャー", "アドベンチャー映画"],
  ["ファンタジー", "ファンタジー映画"],
  ["コメディ", "コメディ映画"],
  ["ロマンス", "ロマンス映画"],
  ["戦争", "戦争映画"],
  ["西部劇", "西部劇"],
  ["ミュージカル", "ミュージカル映画"],
  ["ドキュメンタリー", "ドキュメンタリー映画"],
  ["青春", "青春映画"],
  ["時代劇", "時代劇"],
  ["ドラマ", "ドラマ映画"],
];

const args = process.argv.slice(2);
const flag = (name, fallback) => {
  const at = args.indexOf(`--${name}`);
  return at === -1 ? fallback : args[at + 1];
};
const [yearFrom, yearTo] = (flag("years", `${FIRST_YEAR}-${LAST_YEAR}`) ?? "")
  .split("-")
  .map(Number);
const target = Number(flag("target", TARGET));

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

const CACHE_SCOPE =
  yearFrom === FIRST_YEAR && yearTo === LAST_YEAR && target === TARGET ? "" : "smoke-";

async function cached(name, build) {
  const file = join(CACHE, `${CACHE_SCOPE}${name}.json`);
  try {
    return JSON.parse(await readFile(file, "utf8"));
  } catch {
    const value = await build();
    await mkdir(CACHE, { recursive: true });
    await writeFile(file, JSON.stringify(value));
    return value;
  }
}

async function sparql(query, attempt = 1) {
  const response = await fetch(QLEVER, {
    method: "POST",
    headers: {
      "content-type": "application/x-www-form-urlencoded",
      accept: "application/sparql-results+json",
      "user-agent": USER_AGENT,
    },
    body: new URLSearchParams({ query }).toString(),
  });
  if (response.status === 429 || response.status >= 500) {
    if (attempt > 6) throw new Error(`qlever ${response.status} after ${attempt} attempts`);
    // QLever is a shared public endpoint: back off politely instead of retrying hard.
    await sleep(1000 * 2 ** (attempt - 1));
    return sparql(query, attempt + 1);
  }
  if (!response.ok) throw new Error(`qlever ${response.status}: ${await response.text()}`);
  const body = await response.json();
  if (!body.results) throw new Error(`qlever: ${JSON.stringify(body).slice(0, 300)}`);
  return body.results.bindings;
}

async function getJson(base, params, label, attempt = 1) {
  const url = `${base}?${new URLSearchParams({ ...params, format: "json" })}`;
  const response = await fetch(url, { headers: { "user-agent": USER_AGENT } });
  if (response.status === 429 || response.status >= 500) {
    if (attempt > 6) throw new Error(`${label} ${response.status} after ${attempt} attempts`);
    await sleep(1500 * 2 ** (attempt - 1));
    return getJson(base, params, label, attempt + 1);
  }
  if (!response.ok) throw new Error(`${label} ${response.status}: ${await response.text()}`);
  const body = await response.json();
  if (body.error) throw new Error(`${label}: ${JSON.stringify(body.error).slice(0, 300)}`);
  return body;
}

const wikidata = (params) => getJson(WIKIDATA, params, "wikidata");
const wikipedia = (params) => getJson(WIKIPEDIA, params, "wikipedia");

async function mapLimit(items, limit, worker) {
  const results = new Array(items.length);
  let next = 0;
  const runners = Array.from({ length: Math.min(limit, items.length) }, async () => {
    while (next < items.length) {
      const index = next++;
      results[index] = await worker(items[index], index);
    }
  });
  await Promise.all(runners);
  return results;
}

const id = (binding) => binding.film.value.split("/").pop();
const year = (binding) => Number(binding.first.value.slice(0, 4));

function trimYearQuery(where, limit) {
  return `PREFIX wd: <http://www.wikidata.org/entity/>
PREFIX wdt: <http://www.wikidata.org/prop/direct/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX wikibase: <http://wikiba.se/ontology#>
SELECT ?film ?label ?sitelinks (MIN(?date) AS ?first) WHERE {
  ?film wdt:P31/wdt:P279* wd:Q11424 ; wdt:P577 ?date ; wikibase:sitelinks ?sitelinks .
  ${where}
  ?film rdfs:label ?label . FILTER(LANG(?label) = "ja")
} GROUP BY ?film ?label ?sitelinks ORDER BY DESC(?sitelinks) LIMIT ${limit}`;
}

/** Stage 1: per-year popularity ranking plus the Japanese quota. */
async function yearCandidates() {
  const years = [];
  for (let y = yearFrom; y <= yearTo; y++) years.push(y);
  const perYear = await mapLimit(years, 2, async (y) => {
    const rows = await sparql(trimYearQuery(`FILTER(YEAR(?date) = ${y})`, PER_YEAR));
    await sleep(400);
    const japan = await sparql(
      trimYearQuery(`?film wdt:P495 wd:Q17 . FILTER(YEAR(?date) = ${y})`, PER_YEAR_JAPAN),
    );
    await sleep(400);
    return [...rows, ...japan].map((binding) => ({
      qid: id(binding),
      label: binding.label.value,
      sitelinks: Number(binding.sitelinks.value),
      year: year(binding),
    }));
  });
  return perYear.flat();
}

/** Stage 2: franchises that must be present regardless of the popularity ranking. */
async function franchiseCandidates() {
  const perFranchise = await mapLimit(Object.entries(SHONEN_JUMP), 1, async ([, qid]) => {
    const rows = await sparql(`PREFIX wd: <http://www.wikidata.org/entity/>
PREFIX wdt: <http://www.wikidata.org/prop/direct/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX wikibase: <http://wikiba.se/ontology#>
SELECT ?film ?label ?sitelinks (MIN(?date) AS ?first) WHERE {
  ?film wdt:P31/wdt:P279* wd:Q11424 ; wdt:P577 ?date ; wikibase:sitelinks ?sitelinks .
  { ?film wdt:P144 wd:${qid} } UNION { ?film wdt:P179 wd:${qid} }
  ?film rdfs:label ?label . FILTER(LANG(?label) = "ja")
} GROUP BY ?film ?label ?sitelinks`);
    await sleep(400);
    return rows.map((binding) => ({
      qid: id(binding),
      label: binding.label.value,
      sitelinks: Number(binding.sitelinks.value),
      year: year(binding),
      tier: "jump",
    }));
  });

  const [ghibli, lynch] = await Promise.all([
    sparql(`PREFIX wd: <http://www.wikidata.org/entity/>
PREFIX wdt: <http://www.wikidata.org/prop/direct/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX wikibase: <http://wikiba.se/ontology#>
SELECT ?film ?label ?sitelinks (MIN(?date) AS ?first) WHERE {
  ?film wdt:P31/wdt:P279* wd:Q11424 ; wdt:P272 wd:${STUDIO_GHIBLI} ; wdt:P577 ?date ; wikibase:sitelinks ?sitelinks .
  ?film rdfs:label ?label . FILTER(LANG(?label) = "ja")
} GROUP BY ?film ?label ?sitelinks`),
    sparql(`PREFIX wd: <http://www.wikidata.org/entity/>
PREFIX wdt: <http://www.wikidata.org/prop/direct/>
PREFIX rdfs: <http://www.w3.org/2000/01/rdf-schema#>
PREFIX wikibase: <http://wikiba.se/ontology#>
SELECT ?film ?label ?sitelinks (MIN(?date) AS ?first) WHERE {
  ?film wdt:P31/wdt:P279* wd:Q11424 ; wdt:P57 wd:${DAVID_LYNCH} ; wdt:P577 ?date ; wikibase:sitelinks ?sitelinks .
  ?film rdfs:label ?label . FILTER(LANG(?label) = "ja")
} GROUP BY ?film ?label ?sitelinks`),
  ]);
  const seeds = (rows, tier) =>
    rows.map((binding) => ({
      qid: id(binding),
      label: binding.label.value,
      sitelinks: Number(binding.sitelinks.value),
      year: year(binding),
      tier,
    }));
  return [...perFranchise.flat(), ...seeds(ghibli, "ghibli"), ...seeds(lynch, "lynch")];
}

const IDENTIFIER = (qid) => (qid.startsWith("Q") ? qid : `Q${qid}`);

/** Stage 3: entity facts for the finalists. */
async function entityFacts(qids) {
  const facts = {};
  const batches = [];
  for (let index = 0; index < qids.length; index += 50) batches.push(qids.slice(index, index + 50));
  await mapLimit(batches, 2, async (batch) => {
    const body = await wikidata({
      action: "wbgetentities",
      ids: batch.join("|"),
      props: "claims|labels|sitelinks|descriptions",
      languages: "ja|en",
    });
    for (const [qid, entity] of Object.entries(body.entities ?? {})) {
      if (!entity || entity.missing !== undefined) continue;
      const claims = entity.claims ?? {};
      const valueIds = (property) =>
        (claims[property] ?? [])
          .map((claim) => claim.mainsnak?.datavalue?.value?.id)
          .filter((value) => typeof value === "string")
          .map(IDENTIFIER);
      const times = (claims.P577 ?? [])
        .map((claim) => claim.mainsnak?.datavalue?.value?.time)
        .filter((value) => typeof value === "string")
        .sort();
      const released = times.length ? Number(times[0].slice(1, 5)) : undefined;
      facts[qid] = {
        qid,
        titleJa: entity.labels?.ja?.value ?? entity.labels?.en?.value,
        titleEn: entity.labels?.en?.value ?? undefined,
        description: entity.descriptions?.ja?.value ?? entity.descriptions?.en?.value ?? "",
        year: released,
        directors: valueIds("P57"),
        cast: [...new Set([...valueIds("P161"), ...valueIds("P725")])],
        studios: valueIds("P272"),
        genres: valueIds("P136"),
        followedBy: valueIds("P156")[0],
        article: entity.sitelinks?.jawiki?.title,
      };
    }
    await sleep(300);
  });
  return facts;
}

/** Stage 4: labels for every referenced person, studio and genre. */
async function labels(qids) {
  const found = {};
  const batches = [];
  for (let index = 0; index < qids.length; index += 50) batches.push(qids.slice(index, index + 50));
  await mapLimit(batches, 2, async (batch) => {
    const body = await wikidata({
      action: "wbgetentities",
      ids: batch.join("|"),
      props: "labels",
      languages: "ja|en",
    });
    for (const [qid, entity] of Object.entries(body.entities ?? {})) {
      const label = entity?.labels?.ja?.value ?? entity?.labels?.en?.value;
      if (label) found[qid] = label;
    }
    await sleep(300);
  });
  return found;
}

/** Stage 5: Japanese Wikipedia intro extracts for the film's article. */
async function synopses(articles) {
  const found = {};
  const batches = [];
  for (let index = 0; index < articles.length; index += 20) {
    batches.push(articles.slice(index, index + 20));
  }
  await mapLimit(batches, 2, async (batch) => {
    const body = await wikipedia({
      action: "query",
      prop: "extracts",
      exintro: "1",
      explaintext: "1",
      exlimit: "20",
      redirects: "1",
      titles: batch.join("|"),
    });
    for (const page of Object.values(body.query?.pages ?? {})) {
      if (!page.extract || page.missing !== undefined) continue;
      found[page.title] = page.extract.replace(/\s+/g, " ").trim();
    }
    await sleep(300);
  });
  return found;
}

function normaliseGenre(label) {
  for (const [keyword, name] of GENRE_KEYWORDS) {
    if (label.includes(keyword)) return name;
  }
  return undefined;
}

/** Clip at the last sentence end within `limit`, falling back to the limit itself. */
function clip(text, limit) {
  if (text.length <= limit) return text;
  const cut = text.slice(0, limit);
  const at = cut.lastIndexOf("。");
  return at >= 80 ? cut.slice(0, at + 1) : cut;
}

function pick(list, limit) {
  return [...new Set(list)].slice(0, limit);
}

async function main() {
  console.log(`catalog: years ${yearFrom}-${yearTo}, target ${target}`);
  const perYear = await cached("years", yearCandidates);
  const franchises = await cached("franchises", franchiseCandidates);
  // A film can arrive both from the per-year ranking and from a franchise query; the franchise
  // tag wins so the seeds are guaranteed in the catalog and counted as such.
  const franchiseTier = new Map(franchises.map((film) => [film.qid, film.tier]));
  const merged = new Map();
  for (const film of [...perYear, ...franchises]) {
    if (!merged.has(film.qid)) merged.set(film.qid, { ...film });
  }
  for (const film of merged.values()) film.tier = franchiseTier.get(film.qid) ?? "year";
  const ranked = [...merged.values()].sort(
    (left, right) => right.sitelinks - left.sitelinks || left.qid.localeCompare(right.qid),
  );
  const seeds = ranked.filter((film) => film.tier !== "year");
  const seedIds = new Set(seeds.map((film) => film.qid));
  const finalists = [
    ...seeds,
    ...ranked.filter((film) => !seedIds.has(film.qid)).slice(0, Math.max(0, FINALISTS - seeds.length)),
  ];
  console.log(
    `candidates: ${merged.size} (${seeds.length} franchise seeds), finalists ${finalists.length}`,
  );

  const facts = await cached("entities", () => entityFacts(finalists.map((film) => film.qid)));
  const referenced = new Set();
  for (const fact of Object.values(facts)) {
    for (const qid of [
      ...(fact.directors ?? []),
      ...(fact.cast ?? []),
      ...(fact.studios ?? []),
      ...(fact.genres ?? []),
    ]) {
      referenced.add(qid);
    }
  }
  const names = await cached("labels", () => labels([...referenced].sort()));

  const articles = Object.values(facts)
    .map((fact) => fact.article)
    .filter((article) => typeof article === "string" && article.length > 0);
  const intro = await cached("synopses", () => synopses([...new Set(articles)].sort()));

  const films = [];
  for (const seed of finalists) {
    const fact = facts[seed.qid];
    if (!fact) continue;
    const raw = fact.article ? intro[fact.article] : undefined;
    // Keep the leading sentences only: the intro of a well-known film easily runs past 600
    // characters, and a uniform short passage is better for both full-text and embeddings.
    const text = raw ? clip(raw, SYNOPSIS_MAX_CHARS) : undefined;
    if (!text || text.length < 80) continue;
    // Keep catalog and graph the same scope: a film without a director or cast would be dropped
    // by scripts/build-seeds.mjs, so leave it out here rather than carry a dead row.
    if (!fact.directors?.length || !fact.cast?.length) continue;
    // Omnibus films credit Lynch as one of dozens of segment directors; they are not his works.
    if (seed.tier === "lynch" && (fact.directors?.length ?? 0) > 2) continue;
    const genres = [];
    for (const genre of fact.genres ?? []) {
      const name = normaliseGenre(names[genre] ?? "");
      if (name && !genres.includes(name)) genres.push(name);
      if (genres.length >= GENRE_LIMIT) break;
    }
    films.push({
      qid: fact.qid,
      titleJa: fact.titleJa ?? seed.label,
      titleEn: fact.titleEn ?? null,
      year: fact.year ?? seed.year,
      tier: seed.tier,
      directors: pick(fact.directors?.map((qid) => ({ qid, name: names[qid] })).filter((p) => p.name), 3),
      cast: pick(
        fact.cast?.map((qid) => ({ qid, name: names[qid] })).filter((person) => person.name),
        CAST_LIMIT,
      ),
      studios: pick(fact.studios?.map((qid) => ({ qid, name: names[qid] })).filter((s) => s.name), 2),
      genres,
      followedBy: fact.followedBy ?? null,
      synopsis: text,
      sourceUrl: fact.article
        ? `https://ja.wikipedia.org/wiki/${encodeURIComponent(fact.article.replace(/ /g, "_"))}`
        : null,
    });
    if (films.length >= target) break;
  }
  films.sort((left, right) => left.qid.localeCompare(right.qid));

  const catalog = { version: 1, generatedFrom: { qlever: QLEVER, wikipedia: WIKIPEDIA }, films };
  const lines = [
    "{",
    '"version": 1,',
    `"films": [`,
    films.map((film) => JSON.stringify(film)).join(",\n"),
    "]",
    "}",
  ];
  await writeFile(join(ROOT, "seeds/catalog.json"), `${lines.join("\n")}\n`);
  console.log(
    `wrote ${films.length} films: ` +
      `${films.filter((film) => film.tier === "ghibli").length} ghibli, ` +
      `${films.filter((film) => film.tier === "lynch").length} lynch, ` +
      `${films.filter((film) => film.tier === "jump").length} jump`,
  );
}

main().catch((error) => {
  console.error(error);
  process.exit(1);
});
