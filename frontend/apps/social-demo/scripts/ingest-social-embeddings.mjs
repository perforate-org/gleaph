#!/usr/bin/env node
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join, resolve } from "node:path";
import { IDL } from "@icp-sdk/core/candid";
import { createRouterActor } from "./actor.mjs";

const SCRIPT_DIR = dirname(fileURLToPath(import.meta.url));
const ROOT = process.cwd();
const MANIFEST_PATH =
  process.argv[2] || `${ROOT}/frontend/apps/knowledge-map/seeds/social-seeds.json`;
const GRAPH_NAME = process.env.GLEAPH_DEMO_GRAPH_NAME || "gleaph.pocket_ic";
const ROUTER_CANISTER = process.env.GLEAPH_DEMO_ROUTER_CANISTER || "gleaph-router";
const EMBEDDING_NAME = process.env.GLEAPH_DEMO_EMBEDDING_NAME || "post_vec";

const IcWirePathElement = IDL.Variant({
  Vertex: IDL.Vec(IDL.Nat8),
  Edge: IDL.Vec(IDL.Nat8),
});
const IcWireValue = IDL.Rec();
const IcWireValueVariant = IDL.Variant({
  Null: IDL.Null,
  Bool: IDL.Bool,
  Int64: IDL.Int64,
  Uint64: IDL.Nat64,
  Int128: IDL.Int,
  Uint128: IDL.Nat,
  Int256: IDL.Text,
  Uint256: IDL.Text,
  Float64: IDL.Float64,
  Decimal: IDL.Text,
  Text: IDL.Text,
  Bytes: IDL.Vec(IDL.Nat8),
  Date: IDL.Int32,
  Time: IDL.Nat64,
  LocalTime: IDL.Nat64,
  DateTime: IDL.Record({ seconds: IDL.Int64, nanos: IDL.Nat32 }),
  LocalDateTime: IDL.Record({ seconds: IDL.Int64, nanos: IDL.Nat32 }),
  ZonedDateTime: IDL.Record({
    seconds: IDL.Int64,
    nanos: IDL.Nat32,
    offset_seconds: IDL.Int32,
  }),
  ZonedTime: IDL.Record({ nanos: IDL.Nat64, offset_seconds: IDL.Int32 }),
  Duration: IDL.Record({ months: IDL.Int32, nanos: IDL.Int64 }),
  Principal: IDL.Principal,
  ExtensionLeaf: IDL.Record({
    type_name: IDL.Text,
    payload: IDL.Vec(IDL.Nat8),
  }),
  ValueBinary: IDL.Vec(IDL.Nat8),
  List: IDL.Vec(IcWireValue),
  Path: IDL.Vec(IcWirePathElement),
  Record: IDL.Vec(IDL.Tuple(IDL.Text, IcWireValue)),
});
IcWireValue.fill(IcWireValueVariant);
const WireResult = IDL.Record({
  rows: IDL.Vec(
    IDL.Record({ columns: IDL.Vec(IDL.Tuple(IDL.Text, IcWireValue)) }),
  ),
});

function decodeRowsBlob(bytes) {
  const [decoded] = IDL.decode([WireResult], Uint8Array.from(bytes));
  return decoded.rows;
}

function extractBytes(row, column) {
  for (const [name, value] of row.columns) {
    if (name === column) {
      if ("Bytes" in value) {
        return new Uint8Array(value.Bytes);
      }
      if ("ValueBinary" in value) {
        return new Uint8Array(value.ValueBinary);
      }
    }
  }
  throw new Error(`Missing bytes column ${column}`);
}

function extractInt64(row, column) {
  for (const [name, value] of row.columns) {
    if (name === column) {
      if ("Int64" in value) {
        return BigInt(value.Int64);
      }
    }
  }
  throw new Error(`Missing Int64 column ${column}`);
}

function normalizeRowsBlob(rowsBlob) {
  let bytes;
  if (rowsBlob instanceof Uint8Array) {
    bytes = rowsBlob;
  } else if (Array.isArray(rowsBlob) && rowsBlob[0] instanceof Uint8Array) {
    bytes = rowsBlob[0];
  } else if (Array.isArray(rowsBlob) && Array.isArray(rowsBlob[0])) {
    bytes = new Uint8Array(rowsBlob[0]);
  } else if (Array.isArray(rowsBlob)) {
    bytes = Uint8Array.from(rowsBlob);
  } else {
    throw new Error(`Unexpected rows_blob shape: ${JSON.stringify(rowsBlob)}`);
  }
  return decodeRowsBlob(bytes);
}

async function resolveAllElementIds() {
  const query = `MATCH (p:Post) WHERE p.demo_graph = 'social' RETURN p.demo_id AS demo_id, ELEMENT_ID(p) AS element_id`;
  const router = createRouterActor(ROUTER_CANISTER);
  const result = await router.gql_query(query, new Uint8Array());
  if ("Err" in result) {
    throw new Error(`gql_query failed: ${JSON.stringify(result.Err)}`);
  }
  const rowsBlob = result.Ok.rows_blob;
  if (!rowsBlob || rowsBlob.length === 0) {
    throw new Error("No rows_blob from element-id lookup");
  }
  const rows = normalizeRowsBlob(rowsBlob);
  const map = new Map();
  for (const row of rows) {
    const demoId = extractInt64(row, "demo_id");
    const elementId = extractBytes(row, "element_id");
    map.set(demoId, elementId);
  }
  return map;
}

function isDuplicateError(text) {
  return (
    text.includes("AlreadyExists") ||
    text.includes("already exists") ||
    text.includes("Conflict") ||
    text.includes("Duplicate") ||
    text.includes("UniquenessViolation")
  );
}

async function ingestEmbeddingsBatch(elementIdMap, embeddings) {
  const entries = Object.entries(embeddings);
  const items = entries.map(([demoIdKey, meta]) => {
    const demoId = BigInt(demoIdKey);
    const elementId = elementIdMap.get(demoId);
    if (!elementId) {
      throw new Error(`No element_id for Post demo_id ${demoIdKey}`);
    }
    return {
      encoded_vertex_id: elementId,
      values: meta.values.map((v) => Number(v)),
    };
  });

  const router = createRouterActor(ROUTER_CANISTER);
  const result = await router.admin_ingest_vertex_embedding_batch({
    logical_graph_name: GRAPH_NAME,
    embedding_name: EMBEDDING_NAME,
    items,
  });

  if ("Err" in result) {
    throw new Error(
      `admin_ingest_vertex_embedding_batch failed: ${JSON.stringify(result.Err)}`
    );
  }
  const results = result.Ok;

  if (results.length !== entries.length) {
    throw new Error(
      `Batch result length mismatch: expected ${entries.length}, got ${results.length}`
    );
  }

  let ingestedCount = 0;
  let duplicateCount = 0;
  for (let i = 0; i < entries.length; i += 1) {
    const [demoIdKey] = entries[i];
    const itemResult = results[i];
    if ("Err" in itemResult) {
      const errText = itemResult.Err;
      if (isDuplicateError(errText)) {
        duplicateCount += 1;
        continue;
      }
      throw new Error(
        `admin_ingest_vertex_embedding_batch failed for ${demoIdKey}: ${errText}`
      );
    }
    ingestedCount += 1;
  }
  console.log(
    `[social-demo] Embedding batch complete: ${ingestedCount} ingested, ${duplicateCount} duplicates skipped`
  );
}

async function main() {
  const manifest = JSON.parse(readFileSync(MANIFEST_PATH, "utf8"));
  const embeddings = manifest.embeddings;
  if (!embeddings || typeof embeddings !== "object") {
    throw new Error("Manifest has no embeddings object");
  }
  const ids = Object.keys(embeddings);
  console.log(`[social-demo] Resolving element ids for ${ids.length} Posts`);
  const elementIdMap = await resolveAllElementIds();
  console.log(
    `[social-demo] Ingesting ${ids.length} Post embeddings through Router (batch)`
  );
  await ingestEmbeddingsBatch(elementIdMap, embeddings);
  console.log("[social-demo] Embedding ingestion complete");
}

main().catch((err) => {
  console.error(err);
  process.exit(1);
});
