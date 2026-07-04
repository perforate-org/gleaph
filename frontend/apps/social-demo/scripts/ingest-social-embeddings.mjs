#!/usr/bin/env node
import { readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { IDL } from "@icp-sdk/core/candid";

const ROOT = process.cwd();
const MANIFEST_PATH = process.argv[2] || `${ROOT}/frontend/apps/knowledge-map/seeds/social-seeds.json`;
const GRAPH_NAME = process.env.GLEAPH_DEMO_GRAPH_NAME || "gleaph.pocket_ic";
const ROUTER_CANISTER = process.env.GLEAPH_DEMO_ROUTER_CANISTER || "gleaph-router";
const EMBEDDING_NAME = process.env.GLEAPH_DEMO_EMBEDDING_NAME || "post_vec";

function icp(args) {
  return execFileSync("icp", args, {
    encoding: "utf8",
    stdio: ["pipe", "pipe", "inherit"],
    env: process.env,
  }).trim();
}

function buildWireResultType() {
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
  return IDL.Record({
    rows: IDL.Vec(
      IDL.Record({ columns: IDL.Vec(IDL.Tuple(IDL.Text, IcWireValue)) }),
    ),
  });
}

const WIRE_RESULT_TYPE = buildWireResultType();

function decodeRowsBlob(bytes) {
  const [decoded] = IDL.decode([WIRE_RESULT_TYPE], Uint8Array.from(bytes));
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

function resolveElementId(demoId) {
  const query = `MATCH (p:Post {demo_id: '${demoId}'}) RETURN ELEMENT_ID(p) AS element_id`;
  const argsText = `("${query}", vec {})`;
  const output = icp([
    "canister",
    "call",
    "--json",
    "-e",
    "local",
    ROUTER_CANISTER,
    "gql_query",
    argsText,
    "--query",
  ]);
  const parsed = JSON.parse(output);
  const ok = parsed.Ok;
  if (!ok) {
    throw new Error(`gql_query failed for ${demoId}: ${JSON.stringify(parsed)}`);
  }
  const rowsBlob = ok.rows_blob;
  if (!rowsBlob) {
    throw new Error(`No rows_blob for ${demoId}`);
  }
  let bytes;
  if (Array.isArray(rowsBlob) && Array.isArray(rowsBlob[0])) {
    bytes = rowsBlob[0];
  } else if (Array.isArray(rowsBlob)) {
    bytes = rowsBlob;
  } else {
    throw new Error(`Unexpected rows_blob shape for ${demoId}: ${JSON.stringify(rowsBlob)}`);
  }
  const rows = decodeRowsBlob(bytes);
  if (rows.length !== 1) {
    throw new Error(`Expected one ELEMENT_ID row for ${demoId}, got ${rows.length}`);
  }
  return extractBytes(rows[0], "element_id");
}

function blobText(bytes) {
  return (
    'blob "' +
    Array.from(bytes)
      .map((b) => "\\" + b.toString(16).padStart(2, "0"))
      .join("") +
    '"'
  );
}

function ingestEmbedding(demoId, meta) {
  const elementId = resolveElementId(demoId);
  const values = meta.values.map((v) => `${Number(v)} : float32`).join("; ");
  const argsText = `(
    record {
      logical_graph_name = "${GRAPH_NAME}";
      encoded_vertex_id = ${blobText(elementId)};
      embedding_name = "${EMBEDDING_NAME}";
      values = vec { ${values} };
    }
  )`;
  const output = icp([
    "canister",
    "call",
    "--json",
    "-e",
    "local",
    ROUTER_CANISTER,
    "admin_ingest_vertex_embedding",
    argsText,
  ]);
  const parsed = JSON.parse(output);
  if (!parsed.Ok) {
    const errText = JSON.stringify(parsed);
    if (errText.includes("Conflict") || errText.includes("already exists")) {
      throw new Error(`already ingested ${demoId}: ${errText}`);
    }
    throw new Error(`admin_ingest_vertex_embedding failed for ${demoId}: ${errText}`);
  }
  console.log(`[social-demo] Ingested embedding for ${demoId} -> ${Array.from(elementId).map(b=>b.toString(16).padStart(2,"0")).join("")}`);
}

function isDuplicateError(parsed) {
  if (!parsed || !parsed.Err) return false;
  const text = JSON.stringify(parsed.Err);
  return (
    text.includes("Conflict") ||
    text.includes("already exists") ||
    text.includes("Duplicate") ||
    text.includes("UniquenessViolation")
  );
}

function main() {
  const manifest = JSON.parse(readFileSync(MANIFEST_PATH, "utf8"));
  const embeddings = manifest.embeddings;
  if (!embeddings || typeof embeddings !== "object") {
    throw new Error("Manifest has no embeddings object");
  }
  const ids = Object.keys(embeddings);
  console.log(`[social-demo] Ingesting ${ids.length} Post embeddings through Router`);
  for (const demoId of ids) {
    try {
      ingestEmbedding(demoId, embeddings[demoId]);
    } catch (e) {
      const message = e instanceof Error ? e.message : String(e);
      if (message.includes("already ingested")) {
        console.log(`[social-demo] Skipping duplicate embedding for ${demoId}`);
        continue;
      }
      throw e;
    }
  }
  console.log("[social-demo] Embedding ingestion complete");
}

main();
