import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import test from "node:test";

import { build, ROOT } from "./build-seeds.mjs";

const catalog = JSON.parse(await readFile(join(ROOT, "seeds/catalog.json"), "utf8"));

test("the build is deterministic", () => {
  const first = build(catalog);
  const second = build(catalog);
  assert.equal(first.vertices, second.vertices);
  assert.equal(first.edges, second.edges);
});

test("the committed seed artifacts match a fresh build", async () => {
  const built = build(catalog);
  assert.equal(await readFile(join(ROOT, "seeds/vertices.jsonl"), "utf8"), built.vertices);
  assert.equal(await readFile(join(ROOT, "seeds/edges.jsonl"), "utf8"), built.edges);
});

test("every vertex and edge row is a closed, typed seed record", () => {
  const built = build(catalog);
  const vertices = built.vertices.trim().split("\n").map((line) => JSON.parse(line));
  const edges = built.edges.trim().split("\n").map((line) => JSON.parse(line));
  const ids = new Set(vertices.map((vertex) => vertex.source_id));
  assert.equal(ids.size, vertices.length, "source ids are unique");
  for (const vertex of vertices) {
    assert.equal(vertex.labels.length, 1);
    for (const [key, value] of Object.entries(vertex.properties)) {
      const [kind] = Object.keys(value);
      assert.ok(["Text", "Int32"].includes(kind), `${vertex.source_id}.${key} is ${kind}`);
    }
  }
  for (const row of edges) {
    assert.ok(ids.has(row.source), `edge source ${row.source} exists`);
    assert.ok(ids.has(row.target), `edge target ${row.target} exists`);
    assert.equal(row.directed, true);
  }
});

test("every film carries a director, cast, year and a Wikipedia-sized synopsis", () => {
  const films = catalog.films.filter(
    (film) => film.directors.length > 0 && film.cast.length > 0,
  );
  assert.ok(films.length >= 1500, `expected at least 1500 films, got ${films.length}`);
  for (const film of films) {
    assert.ok(Number.isInteger(film.year) && film.year >= 1900, `${film.qid} year`);
    assert.ok(film.synopsis.length >= 80 && film.synopsis.length <= 520, `${film.qid} synopsis`);
    assert.ok(film.sourceUrl?.startsWith("https://ja.wikipedia.org/"), `${film.qid} source`);
  }
});

test("the Jump provenance is closed over the built graph", async () => {
  const provenance = JSON.parse(await readFile(join(ROOT, "seeds/jump-provenance.json"), "utf8"));
  assert.equal(provenance.imprint, "ジャンプ");
  assert.ok(provenance.works.length > 0);
  assert.ok(provenance.magazines.length > 0);
  const built = build(catalog);
  const vertices = built.vertices.trim().split("\n").map((line) => JSON.parse(line));
  const ids = new Set(vertices.map((vertex) => vertex.source_id));
  const filmQids = new Set(vertices.filter((v) => v.labels.includes("Film")).map((v) => v.properties.qid.Text));
  for (const work of provenance.works) {
    assert.ok(ids.has(`w-${work.qid}`), `work ${work.qid} vertex`);
    assert.ok(ids.has(`m-${work.magazine}`), `magazine ${work.magazine} vertex`);
    for (const filmQid of work.films) {
      assert.ok(filmQids.has(filmQid), `film ${filmQid} is in the graph`);
    }
  }
});

test("the catalog keeps the franchise seeds and the sequel chains", () => {
  const { films } = catalog;
  assert.ok(films.filter((film) => film.tier === "ghibli").length >= 15, "ghibli");
  assert.ok(films.filter((film) => film.tier === "lynch").length >= 7, "lynch");
  assert.ok(films.filter((film) => film.tier === "jump").length >= 20, "jump");
  const { edges } = build(catalog);
  assert.ok(edges.includes('"SEQUEL_OF"'), "sequel edges exist");
});
