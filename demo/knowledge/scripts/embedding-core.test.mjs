// Assertions for scripts/embedding-core.mjs against the checked-in seeds. Run:
//   node --test scripts/embedding-core.test.mjs
//
// The page (src/lib/queryEmbedding.ts) imports this same module, so pinning the module to
// the ingested document vectors here also fixes the browser scenario-3 top-1 by construction.

import test from "node:test";
import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { EMBEDDING_DIMS, SEED_PREFIX, unitVectorFromDigest } from "./embedding-core.mjs";

const DEMO_ROOT = join(dirname(fileURLToPath(import.meta.url)), "..");

function readJsonl(path) {
  return readFileSync(path, "utf8")
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line.length > 0)
    .map((line) => JSON.parse(line));
}

test("unitVectorFromDigest reproduces every checked-in Document embedding exactly", () => {
  const documents = readJsonl(join(DEMO_ROOT, "seeds", "vertices.jsonl")).filter(
    (row) => Array.isArray(row.labels) && row.labels.includes("Document"),
  );
  const stored = new Map(
    readJsonl(join(DEMO_ROOT, "seeds", "embeddings.jsonl")).map((row) => [row.source_id, row.values]),
  );
  assert.ok(documents.length > 0, "seeds must contain Document rows");
  for (const doc of documents) {
    const digest = createHash("sha256").update(SEED_PREFIX + doc.source_id).digest();
    const derived = unitVectorFromDigest(digest);
    const expected = stored.get(doc.source_id);
    assert.ok(expected, `embeddings.jsonl must carry a row for ${doc.source_id}`);
    assert.equal(derived.length, expected.length);
    // Byte-stability contract: exact double equality, no tolerance.
    for (let i = 0; i < derived.length; i++) {
      assert.ok(
        Object.is(derived[i], expected[i]),
        `${doc.source_id}[${i}] diverged: ${derived[i]} vs ${expected[i]}`,
      );
    }
  }
});

test("derived vectors are unit-norm with the documented dimensionality", () => {
  const digest = createHash("sha256").update(SEED_PREFIX + "any-source").digest();
  const vector = unitVectorFromDigest(digest);
  assert.equal(vector.length, EMBEDDING_DIMS);
  const norm = Math.sqrt(vector.reduce((sum, value) => sum + value * value, 0));
  assert.ok(Math.abs(norm - 1) < 1e-12, `norm ${norm} must be 1`);
});
