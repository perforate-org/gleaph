// Paraphrase a routing golden-set split with a local instruct model.
//
//   node scripts/paraphrase-routing.mjs \
//     --input eval/routing/test.jsonl --output eval/routing/test-paraphrased.jsonl \
//     --limit 120 --endpoint http://127.0.0.1:8082 --model qwen
//
// Why: the op-level golden set is template-generated, so a router can learn template wording
// instead of intent. Paraphrased items are new wordings of the same labelled questions. They are
// kept in separate files: paraphrases of `train` feed future fine-tuning, paraphrases of `test`
// measure paraphrase robustness and are never trained on.
//
// Every paraphrase is validated: non-empty, different from the source, same language, and it must
// keep the relation labels, entity names and numbers that the source question contains. Invalid
// generations are retried once with a stricter prompt and otherwise skipped.

import { readFileSync, writeFileSync } from "node:fs";

const SYSTEM_PROMPT =
  "You rewrite user questions. Keep the same intent, entities, relation names, numbers and language. " +
  "Output only the rewritten question, with no quotes and no explanation.";
const STRICT_SUFFIX =
  "\n\nKeep every entity name, relation label (for example RELATED_TO or CITES) and number exactly as written. " +
  "Change the wording or the sentence order instead.";

const CJK = /[\u3040-\u30ff\u3400-\u4dbf\u4e00-\u9fff]/;

/** Seeded PRNG so sampling is reproducible. */
function rng(seed) {
  let state = seed >>> 0;
  return () => {
    state = (state + 0x6d2b79f5) >>> 0;
    let t = state;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

function normalise(text) {
  return text
    .toLowerCase()
    .replace(/[「」『』"'.。、?!！？]/g, "")
    .replace(/\s+/g, " ")
    .trim();
}

/** Relation labels, entity names and numbers that a paraphrase must preserve. */
export function requiredTokens(question, entities) {
  const tokens = new Set();
  for (const match of question.matchAll(/\b[A-Z][A-Z0-9]*(?:_[A-Z0-9]+)*\b/g)) {
    if (match[0].length >= 2) {
      tokens.add(match[0]);
    }
  }
  for (const entity of entities) {
    if (entity && question.includes(entity)) {
      tokens.add(entity);
    }
  }
  for (const match of question.matchAll(/\b\d+\b/g)) {
    tokens.add(match[0]);
  }
  return [...tokens];
}

/**
 * Decide whether a generated paraphrase is usable.
 *
 * @returns {{ ok: true, text: string } | { ok: false, reason: string }}
 */
export function validateParaphrase(original, candidate, entities) {
  const text = candidate
    .trim()
    .replace(/^["'「『]+/, "")
    .replace(/["'」』]+$/, "")
    .trim();
  if (text.length === 0) {
    return { ok: false, reason: "empty" };
  }
  if (text.includes("undefined") || text.includes("NaN")) {
    return { ok: false, reason: "unfilled" };
  }
  if (normalise(text) === normalise(original)) {
    return { ok: false, reason: "identical" };
  }
  if (CJK.test(original) !== CJK.test(text)) {
    return { ok: false, reason: "language-changed" };
  }
  for (const token of requiredTokens(original, entities)) {
    if (!text.includes(token)) {
      return { ok: false, reason: `missing:${token}` };
    }
  }
  return { ok: true, text };
}

/** Deterministic sample: shuffle with a seeded PRNG, then take `limit`. */
export function sampleItems(items, limit, seed) {
  const random = rng(seed);
  const copy = [...items];
  for (let index = copy.length - 1; index > 0; index -= 1) {
    const swap = Math.floor(random() * (index + 1));
    [copy[index], copy[swap]] = [copy[swap], copy[index]];
  }
  return copy.slice(0, Math.min(limit, copy.length));
}

export function parseJsonl(text) {
  return text
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line) => JSON.parse(line));
}

async function generate(endpoint, model, question, strict) {
  const body = {
    model,
    messages: [
      { role: "system", content: SYSTEM_PROMPT },
      { role: "user", content: strict ? `${question}${STRICT_SUFFIX}` : question },
    ],
    temperature: strict ? 0.9 : 0.7,
    top_p: 0.9,
    max_tokens: 160,
  };
  const response = await fetch(`${endpoint}/v1/chat/completions`, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    throw new Error(`HTTP ${response.status}`);
  }
  const data = await response.json();
  return data.choices?.[0]?.message?.content ?? "";
}

/**
 * Paraphrase every item, returning accepted items and a rejection histogram.
 *
 * @param {Array<object>} items golden-set items
 * @param {object} options {endpoint, model, entities, sleepMs}
 */
export async function paraphraseItems(items, options) {
  const { endpoint, model, entities, sleepMs = 0 } = options;
  const accepted = [];
  const rejected = new Map();
  const seen = new Set();
  for (const item of items) {
    let result = null;
    for (const strict of [false, true]) {
      try {
        const candidate = await generate(endpoint, model, item.question, strict);
        const validation = validateParaphrase(item.question, candidate, entities);
        if (validation.ok && !seen.has(normalise(validation.text))) {
          result = validation.text;
          break;
        }
        if (!validation.ok) {
          rejected.set(validation.reason, (rejected.get(validation.reason) ?? 0) + 1);
        } else {
          rejected.set("duplicate", (rejected.get("duplicate") ?? 0) + 1);
        }
      } catch (error) {
        rejected.set(`request:${error.message}`, (rejected.get(`request:${error.message}`) ?? 0) + 1);
      }
      if (sleepMs > 0) {
        await new Promise((resolve) => setTimeout(resolve, sleepMs));
      }
    }
    if (result !== null) {
      seen.add(normalise(result));
      accepted.push({
        ...item,
        question: result,
        paraphrase_of: item.question,
        paraphrased: true,
      });
    }
  }
  return { accepted, rejected };
}

function arg(name, fallback) {
  const index = process.argv.indexOf(`--${name}`);
  return index >= 0 ? process.argv[index + 1] : fallback;
}

if (process.argv[1]?.endsWith("paraphrase-routing.mjs")) {
  const input = arg("input");
  const output = arg("output");
  if (!input || !output) {
    console.error("usage: node scripts/paraphrase-routing.mjs --input <jsonl> --output <jsonl> [--limit N] [--endpoint URL] [--model NAME] [--seed N]");
    process.exit(2);
  }
  const endpoint = arg("endpoint", "http://127.0.0.1:8082");
  const model = arg("model", "qwen");
  const limit = Number.parseInt(arg("limit", "0"), 10);
  const seed = Number.parseInt(arg("seed", "20261109"), 10);
  const items = parseJsonl(readFileSync(input, "utf8"));
  const sample = limit > 0 ? sampleItems(items, limit, seed) : items;
  const entitiesFile = JSON.parse(readFileSync(arg("entities", "eval/routing/entities.json"), "utf8"));
  const entities = Array.isArray(entitiesFile) ? entitiesFile : Object.values(entitiesFile).flat();
  const { accepted, rejected } = await paraphraseItems(sample, { endpoint, model, entities });
  writeFileSync(output, accepted.map((item) => JSON.stringify(item)).join("\n") + (accepted.length ? "\n" : ""));
  console.log(`wrote ${output}: ${accepted.length}/${sample.length} paraphrases`);
  if (rejected.size > 0) {
    console.log("rejections:", [...rejected.entries()].map(([reason, count]) => `${reason}=${count}`).join(", "));
  }
}
