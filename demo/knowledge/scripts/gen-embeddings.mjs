// Deterministic generator for demo/knowledge/seeds/embeddings.jsonl.
//
// Produces one 768-dimensional embedding per `Document` row in seeds/vertices.jsonl, derived
// purely from the row's `source_id` (sha256-seeded xorshift32, L2-normalized). Rerunning the
// script must be byte-stable: no randomness, no time, no locale-dependent formatting — Node's
// JSON.stringify emits the shortest round-trip decimal form for every double, which is
// ECMAScript-mandated and stable across engines.
//
// The derivation core lives in scripts/embedding-core.mjs and is shared with the demo page
// (src/lib/queryEmbedding.ts), so the browser scenario-3 query vector uses the same recipe
// as the ingested document vectors. See that module for the arithmetic contract.
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

import { EMBEDDING_DIMS, SEED_PREFIX, unitVectorFromDigest } from "./embedding-core.mjs";

const DEMO_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");
const VERTICES_PATH = join(DEMO_ROOT, "seeds", "vertices.jsonl");
const OUTPUT_PATH = join(DEMO_ROOT, "seeds", "embeddings.jsonl");

function sha256Digest(text) {
  return createHash("sha256").update(text).digest();
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
  JSON.stringify({
    source_id: row.source_id,
    values: unitVectorFromDigest(sha256Digest(SEED_PREFIX + row.source_id)),
  }),
);
writeFileSync(OUTPUT_PATH, lines.join("\n") + "\n");
console.log(`wrote ${lines.length} embeddings (${EMBEDDING_DIMS} dims each) to ${OUTPUT_PATH}`);
