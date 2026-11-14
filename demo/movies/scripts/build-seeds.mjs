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
import { readFileSync } from "node:fs";
import { readFile, writeFile } from "node:fs/promises";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const HERE = dirname(fileURLToPath(import.meta.url));
export const ROOT = join(HERE, "..");

const FILM = "f-";
const PERSON = "p-";
const STUDIO = "s-";
const WORK = "w-";
const MAGAZINE = "m-";
const IMPRINT = "i-";
const genreId = (name) => `g-${createHash("sha256").update(name).digest("hex").slice(0, 8)}`;
const sourceWorkProvenance = JSON.parse(
  readFileSync(join(ROOT, "seeds/jump-provenance.json"), "utf8"),
);
// Synthetic operator notes keyed by film QID. They become `Film.staff_note`, the one property the
// demo keeps private: `grants/movies.gql` grants PUBLIC no READ on it.
const staffNotes = JSON.parse(readFileSync(join(ROOT, "seeds/staff-notes.json"), "utf8")).notes;

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
  for (const qid of Object.keys(staffNotes)) {
    if (!filmsByQid.has(qid)) throw new Error(`staff note points at a film outside the built catalog: ${qid}`);
  }

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

  const magazineIds = new Map();
  for (const magazine of sourceWorkProvenance.magazines) {
    const sourceId = `${MAGAZINE}${magazine.qid}`;
    magazineIds.set(magazine.qid, sourceId);
    vertices.set(sourceId, vertex(sourceId, "Magazine", { name: text(magazine.name), qid: text(magazine.qid) }));
  }
  const imprintId = `${IMPRINT}${createHash("sha256").update(sourceWorkProvenance.imprint).digest("hex").slice(0, 8)}`;
  vertices.set(imprintId, vertex(imprintId, "Imprint", { name: text(sourceWorkProvenance.imprint) }));
  for (const work of sourceWorkProvenance.works) {
    const workId = `${WORK}${work.qid}`;
    vertices.set(
      workId,
      vertex(workId, "Work", {
        title_ja: text(work.titleJa),
        title_en: work.titleEn ? text(work.titleEn) : null,
        kind: text(work.kind),
        qid: text(work.qid),
      }),
    );
    const magazineId = magazineIds.get(work.magazine);
    if (!magazineId) throw new Error(`work ${work.qid} points at unknown magazine ${work.magazine}`);
    edges.push(edge(workId, magazineId, "SERIALIZED_IN"));
    for (const filmQid of work.films) {
      if (!filmsByQid.has(filmQid)) {
        throw new Error(`work ${work.qid} points at a film outside the built catalog: ${filmQid}`);
      }
      edges.push(edge(`${FILM}${filmQid}`, workId, "BASED_ON"));
    }
  }
  for (const magazineId of magazineIds.values()) {
    edges.push(edge(magazineId, imprintId, "PART_OF"));
  }

  for (const film of films) {
    const sourceId = `${FILM}${film.qid}`;
    vertices.set(
      sourceId,
      vertex(sourceId, "Film", {
        title_ja: text(film.titleJa),
        title_en: film.titleEn ? text(film.titleEn) : null,
        year: { Int32: film.year },
        synopsis: text(film.synopsis),
        search_text: text(
          [film.synopsis, film.synopsisEn].filter((value) => typeof value === "string").join("\n\n"),
        ),
        qid: text(film.qid),
        staff_note: staffNotes[film.qid] ? text(staffNotes[film.qid]) : null,
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
      works: sourceWorkProvenance.works.length,
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
