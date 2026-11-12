#!/usr/bin/env node
// Projects `seeds/catalog.json` into the bulk-load artifacts: `seeds/vertices.jsonl` and
// `seeds/edges.jsonl`. Pure transformation — no network, deterministic output.
//
//   node scripts/build-seeds.mjs
//
// Source ids are QID-derived (`f-<qid>`, `p-<qid>`, `s-<qid>`) so a regenerated catalog keeps
// stable identities; genre ids are content-hashed (`g-<8 hex>`) because Wikidata genres carry no
// stable public id in the catalog.

import { createHash } from "node:crypto";
import { readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
export const ROOT = join(HERE, "..");

const FILM = "f-";
const PERSON = "p-";
const STUDIO = "s-";
const genreId = (name) => `g-${createHash("sha256").update(name).digest("hex").slice(0, 8)}`;

function text(value) {
  return { Text: value };
}

function vertex(sourceId, label, properties) {
  return {
    source_id: sourceId,
    labels: [label],
    properties: Object.fromEntries(
      Object.entries(properties).filter(([, value]) => value !== null && value !== undefined),
    ),
  };
}

function edge(source, target, label) {
  return { source, target, directed: true, label, inline_value: null, properties: {} };
}

/**
 * Build the seed artifacts from a parsed catalog. Returns the two JSONL strings and counts.
 * Exported so the test can pin determinism and referential closure without touching disk.
 */
export function build(catalog) {
  const films = catalog.films.filter(
    (film) =>
      film.directors.length > 0 &&
      film.cast.length > 0 &&
      typeof film.synopsis === "string" &&
      film.synopsis.length >= 80 &&
      film.synopsis.length <= 600 &&
      Number.isInteger(film.year),
  );
  const filmsByQid = new Map(films.map((film) => [film.qid, film]));

  const vertices = new Map();
  const edges = [];
  const addPerson = (person) => {
    const sourceId = `${PERSON}${person.qid}`;
    if (!vertices.has(sourceId)) {
      vertices.set(sourceId, vertex(sourceId, "Person", { name: text(person.name), qid: text(person.qid) }));
    }
    return sourceId;
  };
  const addStudio = (studio) => {
    const sourceId = `${STUDIO}${studio.qid}`;
    if (!vertices.has(sourceId)) {
      vertices.set(sourceId, vertex(sourceId, "Studio", { name: text(studio.name), qid: text(studio.qid) }));
    }
    return sourceId;
  };
  const addGenre = (name) => {
    const sourceId = genreId(name);
    if (!vertices.has(sourceId)) {
      vertices.set(sourceId, vertex(sourceId, "Genre", { name: text(name) }));
    }
    return sourceId;
  };

  for (const film of films) {
    const sourceId = `${FILM}${film.qid}`;
    vertices.set(
      sourceId,
      vertex(sourceId, "Film", {
        title: text(film.title),
        original_title: film.originalTitle ? text(film.originalTitle) : null,
        year: { Int32: film.year },
        synopsis: text(film.synopsis),
        qid: text(film.qid),
      }),
    );
  }
  for (const film of films) {
    const sourceId = `${FILM}${film.qid}`;
    for (const director of film.directors) {
      edges.push(edge(addPerson(director), sourceId, "DIRECTED"));
    }
    for (const person of film.cast) {
      edges.push(edge(addPerson(person), sourceId, "ACTED_IN"));
    }
    for (const studio of film.studios) {
      edges.push(edge(sourceId, addStudio(studio), "PRODUCED_BY"));
    }
    for (const genre of film.genres) {
      edges.push(edge(sourceId, addGenre(genre), "HAS_GENRE"));
    }
  }
  for (const film of films) {
    // P156 is "followed by": the edge points from a film to the film that follows it.
    const next = film.followedBy ? filmsByQid.get(film.followedBy) : undefined;
    if (next && next.year > film.year) {
      edges.push(edge(`${FILM}${film.qid}`, `${FILM}${next.qid}`, "SEQUEL_OF"));
    }
  }

  const orderedVertices = [...vertices.values()].sort((left, right) =>
    left.source_id.localeCompare(right.source_id),
  );
  const orderedEdges = edges
    .slice()
    .sort((left, right) =>
      `${left.source}\u0000${left.target}\u0000${left.label}`.localeCompare(
        `${right.source}\u0000${right.target}\u0000${right.label}`,
      ),
    );
  return {
    vertices: `${orderedVertices.map((row) => JSON.stringify(row)).join("\n")}\n`,
    edges: `${orderedEdges.map((row) => JSON.stringify(row)).join("\n")}\n`,
    counts: {
      films: films.length,
      vertices: orderedVertices.length,
      edges: orderedEdges.length,
    },
  };
}

async function main() {
  const catalog = JSON.parse(await readFile(join(ROOT, "seeds/catalog.json"), "utf8"));
  const built = build(catalog);
  await writeFile(join(ROOT, "seeds/vertices.jsonl"), built.vertices);
  await writeFile(join(ROOT, "seeds/edges.jsonl"), built.edges);
  console.log(
    `wrote ${built.counts.vertices} vertices / ${built.counts.edges} edges for ${built.counts.films} films`,
  );
}

if (process.argv[1] && import.meta.url === `file://${process.argv[1]}`) {
  main().catch((error) => {
    console.error(error);
    process.exit(1);
  });
}
