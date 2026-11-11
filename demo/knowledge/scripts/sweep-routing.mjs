// Abstain-threshold sweep over a `gleaph ask --eval --shadow` file.
//
//   node scripts/sweep-routing.mjs /tmp/dev-laya.jsonl [--by top|margin]
//
// The shadow record contains the model's ranked hits per item, so the risk/coverage curve can be
// derived without calling the model again. Pick the threshold on dev, then evaluate test once.
//
// Columns:
//   coverage    answered in-scope items / in-scope items
//   precision   correct answers / answered in-scope items
//   oos recall  out-of-scope items abstained / out-of-scope items

import { readFileSync } from "node:fs";

/** Score used to accept an answer at a given threshold. */
export function acceptanceScore(record, by = "top") {
  const hits = record.model?.hits ?? [];
  if (hits.length === 0) {
    return 0;
  }
  const top = hits[0].score ?? 0;
  if (by === "margin") {
    const second = hits.length > 1 ? (hits[1].score ?? 0) : 0;
    return top - second;
  }
  return top;
}

/**
 * Build the risk/coverage curve for one shadow file.
 *
 * @param {Array<object>} records parsed shadow records
 * @param {number[]} thresholds ascending acceptance thresholds
 * @param {"top"|"margin"} by which score to threshold
 */
export function sweep(records, thresholds, by = "top") {
  const inScope = records.filter((record) => record.expected_op);
  const outOfScope = records.filter((record) => !record.expected_op);
  return thresholds.map((threshold) => {
    const answered = inScope.filter((record) => acceptanceScore(record, by) >= threshold);
    const correct = answered.filter(
      (record) => record.model?.hits?.[0]?.name === record.expected_op,
    );
    const oosAbstained = outOfScope.filter(
      (record) => acceptanceScore(record, by) < threshold,
    );
    return {
      threshold,
      answered: answered.length,
      inScope: inScope.length,
      correct: correct.length,
      oosAbstained: oosAbstained.length,
      outOfScope: outOfScope.length,
      coverage: inScope.length === 0 ? 0 : answered.length / inScope.length,
      precision: answered.length === 0 ? 0 : correct.length / answered.length,
      oosRecall: outOfScope.length === 0 ? 0 : oosAbstained.length / outOfScope.length,
    };
  });
}

/** Parse a shadow JSONL file. */
export function parseShadow(text) {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line) => JSON.parse(line));
}

/** Render the sweep as a Markdown table. */
export function renderTable(rows) {
  const header = [
    "| threshold | answered | coverage | precision | OOS recall |",
    "| ---: | ---: | ---: | ---: | ---: |",
  ];
  const body = rows.map(
    (row) =>
      `| ${row.threshold.toFixed(2)} | ${row.answered}/${row.inScope} | ` +
      `${(row.coverage * 100).toFixed(1)}% | ${(row.precision * 100).toFixed(1)}% | ` +
      `${(row.oosRecall * 100).toFixed(1)}% (${row.oosAbstained}/${row.outOfScope}) |`,
  );
  return [...header, ...body].join("\n");
}

const DEFAULT_THRESHOLDS = [0.2, 0.25, 0.3, 0.35, 0.4, 0.45, 0.5, 0.55, 0.6, 0.7];

if (process.argv[1]?.endsWith("sweep-routing.mjs")) {
  const path = process.argv[2];
  if (!path) {
    console.error("usage: node scripts/sweep-routing.mjs <shadow.jsonl> [--by top|margin]");
    process.exit(2);
  }
  const byIndex = process.argv.indexOf("--by");
  const by = byIndex >= 0 ? process.argv[byIndex + 1] : "top";
  const records = parseShadow(readFileSync(path, "utf8"));
  console.log(`# ${path} (${records.length} items, by ${by})`);
  console.log(renderTable(sweep(records, DEFAULT_THRESHOLDS, by)));
}
