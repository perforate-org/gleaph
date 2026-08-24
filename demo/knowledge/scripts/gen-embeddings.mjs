// Deterministic generator for demo/knowledge/seeds/embeddings.jsonl.
//
// Produces one 768-dimensional embedding per `Document` row in seeds/vertices.jsonl, derived
// purely from the row's `source_id` (sha256-seeded xorshift32, L2-normalized). Rerunning the
// script must be byte-stable: no randomness, no time, no locale-dependent formatting — Node's
// JSON.stringify emits the shortest round-trip decimal form for every double, which is
// ECMAScript-mandated and stable across engines.
//
// Usage: node scripts/gen-embeddings.mjs   (writes seeds/embeddings.jsonl)
//
// These are synthetic stand-ins for real embedding-model output; only determinism matters for
// the demo. The wire format consumed by `gleaph embed ingest` is one NDJSON object per row:
// {"source_id": "...", "values": [<number> × 768]}.

import { createHash } from "node:crypto";
import { readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const DEMO_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const VERTICES_PATH = join(DEMO_ROOT, "seeds", "vertices.jsonl");
const OUTPUT_PATH = join(DEMO_ROOT, "seeds", "embeddings.jsonl");
const DIMS = 768;
const SEED_PREFIX = "gleaph-knowledge-demo-embedding-v1:";

function seededUnitVector(sourceId) {
  const digest = createHash("sha256").update(SEED_PREFIX + sourceId).digest();
  // 32-bit xorshift seeded from the first four digest bytes.
  let state = digest.readUInt32LE(0);
  if (state === 0) state = 0x9e3779b9;
  const raw = new Array(DIMS);
  for (let i = 0; i < DIMS; i++) {
    state ^= state << 13;
    state >>>= 0;
    state ^= state >>> 17;
    state ^= state << 5;
    state >>>= 0;
    // Map to [-1, 1).
    raw[i] = (state / 2147483648) - 1;
  }
  const norm = Math.sqrt(raw.reduce((sum, value) => sum + value * value, 0));
  return raw.map((value) => value / norm);
}

const rows = readFileSync(VERTICES_PATH, "utf8")
  .split("\n")
  .map((line) => line.trim())
  .filter((line) => line.length > 0)
  .map((line) => JSON.parse(line))
  .filter((row) => Array.isArray(row.labels) && row.labels.includes("Document"));

if (rows.length === 0) {
  throw new Error("no Document rows found in vertices.jsonl");
}

const lines = rows.map((row) =>
  JSON.stringify({ source_id: row.source_id, values: seededUnitVector(row.source_id) }),
);
writeFileSync(OUTPUT_PATH, lines.join("\n") + "\n");
console.log(`wrote ${lines.length} embeddings (${DIMS} dims each) to ${OUTPUT_PATH}`);
