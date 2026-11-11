// Routing quality gate for the natural-language query surface.
//
// Evaluates a served decision-model checkpoint against the hand-written realistic set and fails
// the process when any metric drops below its floor. The criteria come from the same source the
// CLI uses: the leading `///` comment of each prepared query, plus the abstain option appended
// last (matching `gleaph ask`).
//
//   node scripts/routing-gate.mjs \
//     --model-url http://127.0.0.1:8084/v1/systemone --model m11 \
//     --input eval/routing/wild-handwritten-2026-11-09.jsonl

import { createHash } from "node:crypto";
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";

const HERE = dirname(fileURLToPath(import.meta.url));
const DEFAULT_INPUT = join(HERE, "..", "eval", "routing", "wild-handwritten-2026-11-09.jsonl");
const PREPARED_DIR = join(HERE, "..", "prepared");
export const ABSTAIN = "none-of-these";
export const ABSTAIN_DESCRIPTION =
  "Out of scope: no listed prepared query answers this question (weather, booking, writing or deleting data).";

/** Leading `///` doc comment of a prepared query file. */
export function leadingDocComment(text) {
  const lines = [];
  for (const line of text.split("\n")) {
    const trimmed = line.trimStart();
    if (!trimmed.startsWith("///")) {
      break;
    }
    const comment = line.slice(line.indexOf("///") + 3).trim();
    lines.push(comment);
  }
  return lines.join(" ");
}

/** Route criteria exactly as `gleaph ask` derives them: prepared queries sorted by name + abstain. */
export function buildCriteria(preparedDir = PREPARED_DIR) {
  const criteria = {};
  for (const name of readdirSync(preparedDir)
    .filter((file) => file.endsWith(".gql"))
    .sort()) {
    const query = name.slice(0, -".gql".length);
    criteria[query] = leadingDocComment(readFileSync(join(preparedDir, name), "utf8"));
  }
  criteria[ABSTAIN] = ABSTAIN_DESCRIPTION;
  return criteria;
}


/** SHA-256 of a file, used to freeze evaluation sets. */
export function sha256File(path) {
  return createHash("sha256").update(readFileSync(path)).digest("hex");
}

/**
 * Verify a frozen evaluation set against its `<file>.sha256` sidecar.
 *
 * Returns `{ frozen: false }` when no sidecar exists (the set is still a development set), and
 * `{ frozen: true, ok, expected, actual }` otherwise. A mismatch means the set changed after it
 * was frozen: treat the run as invalid rather than silently measuring a different distribution.
 */
export function verifyFrozen(path) {
  let expected;
  try {
    expected = readFileSync(`${path}.sha256`, "utf8").trim();
  } catch {
    return { frozen: false };
  }
  const actual = sha256File(path);
  return { frozen: true, ok: actual === expected, expected, actual };
}

export function loadRows(path = DEFAULT_INPUT) {
  return readFileSync(path, "utf8")
    .split("\n")
    .filter((line) => line.trim().length > 0)
    .map((line) => JSON.parse(line));
}

/**
 * Pure metric computation.
 *
 * `predict` returns `{ [criterion]: probability }`. A question abstains when the abstain option's
 * probability reaches `threshold`; otherwise the highest scoring real operation is the route.
 */
export function evaluate(rows, predict, options = {}) {
  const threshold = options.threshold ?? 0.23;
  const positives = rows.filter((row) => row.expected_op);
  const stats = {
    rows: rows.length,
    positives: positives.length,
    oos: rows.length - positives.length,
    correct: 0,
    routed: 0,
    abstained: 0,
    oosRefused: 0,
    perOperation: {},
    failures: [],
  };
  for (const row of positives) {
    const operation = row.expected_op;
    const per = (stats.perOperation[operation] ??= { total: 0, correct: 0 });
    per.total += 1;
    const probabilities = predict(row.question);
    if (probabilities[ABSTAIN] >= threshold) {
      stats.abstained += 1;
      stats.failures.push({ question: row.question, gold: operation, got: "abstain" });
      continue;
    }
    stats.routed += 1;
    const ranked = Object.keys(probabilities)
      .filter((name) => name !== ABSTAIN)
      .sort((left, right) => probabilities[right] - probabilities[left]);
    const chosen = ranked[0];
    if (chosen === operation) {
      stats.correct += 1;
      per.correct += 1;
    } else {
      stats.failures.push({ question: row.question, gold: operation, got: chosen });
    }
  }
  for (const row of rows.filter((row) => !row.expected_op)) {
    const probabilities = predict(row.question);
    if (probabilities[ABSTAIN] >= threshold) {
      stats.oosRefused += 1;
    } else {
      const ranked = Object.keys(probabilities)
        .filter((name) => name !== ABSTAIN)
        .sort((left, right) => probabilities[right] - probabilities[left]);
      stats.failures.push({ question: row.question, gold: "out-of-scope", got: ranked[0] });
    }
  }
  stats.accuracy = (stats.correct + stats.oosRefused) / stats.rows;
  stats.coverage = stats.positives === 0 ? 0 : stats.routed / stats.positives;
  stats.precision = stats.routed === 0 ? 0 : stats.correct / stats.routed;
  stats.oosRefusal = stats.oos === 0 ? 1 : stats.oosRefused / stats.oos;
  return stats;
}

/** Gate verdicts; each metric carries its floor. */
export function gate(stats, floors = {}) {
  const limits = {
    accuracy: floors.accuracy ?? 0.85,
    coverage: floors.coverage ?? 0.85,
    precision: floors.precision ?? 0.85,
    oosRefusal: floors.oosRefusal ?? 0.85,
  };
  const checks = Object.entries(limits).map(([metric, floor]) => ({
    metric,
    floor,
    value: stats[metric],
    passed: stats[metric] >= floor,
  }));
  return { passed: checks.every((check) => check.passed), checks };
}

/**
 * Process exit code for a verdict.
 *
 * A regression gate (`reportOnly = false`) fails the process when any floor is missed. A
 * report-only gate (the frozen blind set) still reports PASS/FAIL but exits 0, so a pre-release
 * pipeline can keep the blind quality signal visible without pretending the floor is met.
 */
export function exitCodeFor(verdict, reportOnly = false) {
  return reportOnly || verdict.passed ? 0 : 1;
}

function predictRemote({ modelUrl, model, criteria }) {
  return async (state) => {
    const response = await fetch(modelUrl, {
      method: "POST",
      headers: { "Content-Type": "application/json" },
      body: JSON.stringify({
        model,
        state,
        questions: {
          route: {
            type: "choice",
            instructions: "Which prepared query answers this question?",
            criteria,
          },
        },
      }),
    });
    if (!response.ok) {
      throw new Error(`decision model responded ${response.status}: ${await response.text()}`);
    }
    const body = await response.json();
    return body.answers?.route?.probabilities ?? {};
  };
}

function parseArgs(argv) {
  const args = {
    input: DEFAULT_INPUT,
    modelUrl: "http://127.0.0.1:8084/v1/systemone",
    model: "router",
    threshold: 0.23,
    json: null,
    skipHash: false,
    reportOnly: false,
  };
  for (let index = 0; index < argv.length; index += 1) {
    const flag = argv[index];
    const value = argv[index + 1];
    if (flag === "--input") args.input = resolve(value);
    else if (flag === "--model-url") args.modelUrl = value;
    else if (flag === "--model") args.model = value;
    else if (flag === "--threshold") args.threshold = Number(value);
    else if (flag === "--json") args.json = resolve(value);
    else if (flag === "--skip-hash") {
      args.skipHash = true;
      index -= 1;
    } else if (flag === "--report-only") {
      args.reportOnly = true;
      index -= 1;
    }
    else if (flag === "--help") {
      console.log(
        "usage: node scripts/routing-gate.mjs [--input FILE] [--model-url URL] [--model NAME] " +
          "[--threshold 0..1] [--json REPORT] [--report-only] [--skip-hash]",
      );
      process.exit(0);
    } else throw new Error(`unknown argument ${flag}`);
    index += 1;
  }
  return args;
}

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  const args = parseArgs(process.argv.slice(2));
  const frozen = verifyFrozen(args.input);
  if (frozen.frozen && !frozen.ok && !args.skipHash) {
    console.error(
      `frozen set ${args.input} changed: expected sha256 ${frozen.expected}, got ${frozen.actual}. ` +
        "Restore the frozen file or pass --skip-hash for a deliberate re-freeze.",
    );
    process.exit(1);
  }
  const criteria = buildCriteria();
  const rows = loadRows(args.input);
  const remote = predictRemote({ modelUrl: args.modelUrl, model: args.model, criteria });
  const probabilities = new Map();
  for (const row of rows) {
    probabilities.set(row.question, await remote(row.question));
  }
  const stats = evaluate(rows, (question) => probabilities.get(question), {
    threshold: args.threshold,
  });
  const verdict = gate(stats);
  const report = {
    input: args.input,
    model: args.model,
    threshold: args.threshold,
    stats,
    verdict,
    reportOnly: args.reportOnly,
  };
  if (args.json) {
    writeFileSync(args.json, `${JSON.stringify(report, null, 2)}\n`);
  }
  console.log(`rows ${stats.rows} positives ${stats.positives} oos ${stats.oos}`);
  for (const check of verdict.checks) {
    const mark = check.passed ? "PASS" : "FAIL";
    console.log(
      `${mark} ${check.metric}: ${(check.value * 100).toFixed(1)}% (floor ${(check.floor * 100).toFixed(0)}%)`,
    );
  }
  for (const [operation, per] of Object.entries(stats.perOperation).sort()) {
    console.log(`  ${operation}: ${per.correct}/${per.total}`);
  }
  if (args.reportOnly) {
    console.log("report-only: floor misses do not fail the process");
  }
  console.log(`failures ${stats.failures.length}`);
  for (const failure of stats.failures) {
    console.log(` - ${failure.gold} -> ${failure.got}: ${failure.question}`);
  }
  process.exit(exitCodeFor(verdict, args.reportOnly));
}
