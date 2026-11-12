import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";

const ROOT = join(import.meta.dirname, "..");
const read = async (name) => readFile(join(ROOT, name), "utf8");

const catalog = JSON.parse(await read("seeds/catalog.json"));
const provenance = JSON.parse(await read("seeds/embeddings.provenance.json"));
const rows = (await read("seeds/embeddings.jsonl")).trim().split("\n").map((line) => JSON.parse(line));

test("the vector file is pinned and matches its provenance", async () => {
  const digest = createHash("sha256").update(await read("seeds/embeddings.jsonl")).digest("hex");
  assert.equal(digest, (await read("seeds/embeddings.sha256")).trim());
  assert.equal(digest, provenance.sha256);
  assert.equal(provenance.rows, rows.length);
  assert.equal(provenance.dimensions, 384);
});

test("there is one normalised vector per film", () => {
  const films = new Set(
    catalog.films
      .filter((film) => film.synopsis && film.directors.length > 0 && film.cast.length > 0)
      .map((film) => `f-${film.qid}`),
  );
  assert.equal(rows.length, films.size);
  for (const row of rows) {
    assert.ok(films.has(row.source_id), row.source_id);
    assert.equal(row.values.length, 384);
    const norm = Math.hypot(...row.values);
    assert.ok(Math.abs(norm - 1) < 1e-3, `${row.source_id} norm ${norm}`);
  }
});
