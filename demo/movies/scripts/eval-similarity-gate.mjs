#!/usr/bin/env node
/**
 * Live similarity relevance gate.
 *
 * Calls the published title-bound similarity operations against the running Router and scores
 * their rows against the frozen labels in `eval/similarity-gate-2026-11-13.jsonl`. This is a
 * structural gate: labels are derived from same-director catalog metadata, not independent human
 * judgements. See `eval/SIMILARITY-GATE.md`.
 *
 * Usage:
 *   node scripts/eval-similarity-gate.mjs --gate
 *   node scripts/eval-similarity-gate.mjs --gate --min-ndcg 0.55 --min-recall 0.35
 */
import { readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import { createGleaphClient } from "../../../sdk/client/js/dist/index.mjs";

const ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const DEFAULT_QUERIES = join(ROOT, "eval/similarity-gate-2026-11-13.jsonl");
const DEFAULT_CONFIG = join(ROOT, "browser/config.json");

function parseArgs(argv) {
  const args = {
    queries: DEFAULT_QUERIES,
    config: DEFAULT_CONFIG,
    topK: 8,
    minNdcg: 0.50,
    minRecall: 0.30,
    gate: false,
    output: null,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const value = argv[index];
    if (value === "--gate") args.gate = true;
    else if (value === "--queries") args.queries = argv[++index];
    else if (value === "--config") args.config = argv[++index];
    else if (value === "--top-k") args.topK = Number(argv[++index]);
    else if (value === "--min-ndcg") args.minNdcg = Number(argv[++index]);
    else if (value === "--min-recall") args.minRecall = Number(argv[++index]);
    else if (value === "--output") args.output = argv[++index];
    else if (value === "--help") {
      console.log(
        "usage: node scripts/eval-similarity-gate.mjs [--gate] [--queries FILE] [--config FILE]\n" +
          "       [--top-k K] [--min-ndcg X] [--min-recall X] [--output FILE]",
      );
      process.exit(0);
    } else throw new Error(`unknown argument: ${value}`);
  }
  return args;
}

const text = (value) => {
  if (value === null || value === undefined) return "";
  if (typeof value === "object" && "Text" in value) return String(value.Text);
  return String(value);
};

function dcg(grades) {
  return grades.reduce((sum, grade, index) => sum + (2 ** grade - 1) / Math.log2(index + 2), 0);
}

function scoreRows(rows, relevant, qidByTitle, topK) {
  const retrieved = rows.slice(0, topK).map((row) => ({
    title: text(row.title),
    qid: qidByTitle.get(text(row.title)) ?? null,
  }));
  const grades = retrieved.map((row) => (row.qid ? (relevant[row.qid] ?? 0) : 0));
  const ideal = Object.values(relevant).sort((a, b) => b - a).slice(0, topK);
  const idealDcg = dcg(ideal);
  const hits = grades.filter((grade) => grade > 0).length;
  const firstHit = grades.findIndex((grade) => grade > 0);
  return {
    retrieved,
    grades,
    ndcg: idealDcg > 0 ? dcg(grades) / idealDcg : null,
    recall: Object.keys(relevant).length > 0 ? hits / Object.keys(relevant).length : null,
    mrr: firstHit >= 0 ? 1 / (firstHit + 1) : 0,
  };
}

function mean(values) {
  const present = values.filter((value) => typeof value === "number");
  return present.length ? present.reduce((sum, value) => sum + value, 0) / present.length : null;
}

function format(value) {
  return typeof value === "number" ? value.toFixed(3) : "n/a";
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  const config = JSON.parse(await readFile(args.config, "utf8"));
  if (!config.canisterId) throw new Error(`${args.config} has no canisterId; run quickstart or pass ?router=`);
  const catalog = JSON.parse(await readFile(join(ROOT, "seeds/catalog.json"), "utf8"));
  const qidByTitle = new Map();
  for (const film of catalog.films) {
    qidByTitle.set(film.titleJa, film.qid);
    if (film.titleEn) qidByTitle.set(film.titleEn, film.qid);
  }
  const queries = (await readFile(args.queries, "utf8"))
    .split("\n")
    .filter((line) => line.trim())
    .map((line) => JSON.parse(line));
  const client = await createGleaphClient({
    canisterId: config.canisterId,
    host: config.host,
    fetchRootKey: config.fetchRootKey,
  });
  const results = [];
  for (const item of queries) {
    const result = await client.preparedQuery(item.operation, { anchor: item.anchor });
    const metric = scoreRows(result.rows ?? [], item.relevant, qidByTitle, args.topK);
    results.push({ ...item, ...metric });
    console.log(
      `${item.id.padEnd(32)} nDCG@${args.topK}=${format(metric.ndcg)} ` +
        `Recall@${args.topK}=${format(metric.recall)} MRR=${format(metric.mrr)}`,
    );
  }
  const byLang = {};
  for (const row of results) {
    (byLang[row.lang] ??= []).push(row);
  }
  const summary = {
    generated_at: new Date().toISOString(),
    config: { canisterId: config.canisterId, host: config.host },
    queries: args.queries,
    top_k: args.topK,
    query_count: results.length,
    mean_ndcg: mean(results.map((row) => row.ndcg)),
    mean_recall: mean(results.map((row) => row.recall)),
    mean_mrr: mean(results.map((row) => row.mrr)),
    by_lang: Object.fromEntries(
      Object.entries(byLang).map(([lang, rows]) => [
        lang,
        {
          count: rows.length,
          mean_ndcg: mean(rows.map((row) => row.ndcg)),
          mean_recall: mean(rows.map((row) => row.recall)),
          mean_mrr: mean(rows.map((row) => row.mrr)),
        },
      ]),
    ),
    thresholds: { min_ndcg: args.minNdcg, min_recall: args.minRecall },
    per_query: results,
  };
  console.log(
    `\noverall: nDCG@${args.topK}=${format(summary.mean_ndcg)} ` +
      `Recall@${args.topK}=${format(summary.mean_recall)} MRR=${format(summary.mean_mrr)}`,
  );
  const failed =
    summary.mean_ndcg === null ||
    summary.mean_recall === null ||
    summary.mean_ndcg < args.minNdcg ||
    summary.mean_recall < args.minRecall;
  summary.gate = failed ? "fail" : "pass";
  console.log(
    `gate: ${summary.gate} (nDCG >= ${args.minNdcg}, Recall >= ${args.minRecall})`,
  );
  const output = args.output ?? join(ROOT, "eval/similarity-gate-2026-11-13.json");
  await writeFile(output, `${JSON.stringify(summary, null, 2)}\n`);
  console.log(`wrote ${output}`);
  if (args.gate && failed) process.exitCode = 1;
}

main().catch((error) => {
  console.error(error);
  process.exitCode = 1;
});
