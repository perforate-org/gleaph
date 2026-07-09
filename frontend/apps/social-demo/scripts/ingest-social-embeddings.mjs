#!/usr/bin/env node
import { readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";
import { IDL } from "@icp-sdk/core/candid";

const ROOT = process.cwd();
const MANIFEST_PATH = process.argv[2] || `${ROOT}/frontend/apps/knowledge-map/seeds/social-seeds.json`;
const GRAPH_NAME = process.env.GLEAPH_DEMO_GRAPH_NAME || "gleaph.pocket_ic";
const ROUTER_CANISTER = process.env.GLEAPH_DEMO_ROUTER_CANISTER || "gleaph-router";
const EMBEDDING_NAME = process.env.GLEAPH_DEMO_EMBEDDING_NAME || "post_vec";

function injectIdentity(args, identityName) {
  // For `icp <SUB> <ACTION> [SUB FLAGS] <CANISTER> [METHOD] [ARGS]`,
  // the subcommand + action are the first two non-flag tokens. Sub-flags
  // (e.g. `--json`, `-e local`, `--query`) come after them, and then the
  // positional <CANISTER> argument.  icp's clap parser accepts `--identity
  // <name>` only as a "Common Parameter", which means it must appear after
  // <SUB> <ACTION> and before the positional <CANISTER>.
  const out = [];
  let stage = "pre-sub"; // pre-sub | sub | action | post-action
  let inserted = false;
  for (let i = 0; i < args.length; i++) {
    const a = args[i];
    if (a === "-e" || a === "--environment") {
      out.push(a, args[i + 1]);
      i++;
      if (stage === "action" || stage === "sub") stage = "post-action";
      continue;
    }
    if (a.startsWith("-")) {
      out.push(a);
      continue;
    }
    if (stage === "pre-sub") {
      stage = "sub";
    } else if (stage === "sub") {
      stage = "action";
    } else {
      if (!inserted) {
        out.push("--identity", identityName);
        inserted = true;
        stage = "post-action";
      }
    }
    out.push(a);
  }
  if (!inserted) {
    out.push("--identity", identityName);
  }
  return out;
}

// Call `icp canister call ...` and return the raw response as a Uint8Array
// (Candid bytes).  icp-cli >=1.0 with `--output candid` prints the response
// as a hex blob on stdout; we convert that back into bytes and let callers
// IDL.decode it.
function icpRaw(args) {
  const identityName = process.env.ICP_IDENTITY_NAME;
  let finalArgs = args;
  if (identityName) {
    finalArgs = injectIdentity(finalArgs, identityName);
  }
  if (!finalArgs.includes("--output") && !finalArgs.includes("-o")) {
    finalArgs = [...finalArgs, "--output", "hex"];
  }
  const out = execFileSync("icp", finalArgs, {
    encoding: "utf8",
    stdio: ["pipe", "pipe", "inherit"],
    env: process.env,
  }).trim();
  if (!out) {
    throw new Error(`icp returned empty output for args: ${JSON.stringify(finalArgs)}`);
  }
  return Uint8Array.from(Buffer.from(out, "hex"));
}

// Convenience: call `icp canister call ...` and IDL.decode the response as
// a single type.
function icp(args, expectedType) {
  const bytes = icpRaw(args);
  const [decoded] = IDL.decode([expectedType], bytes);
  return decoded;
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
  const QueryGqlResult = IDL.Variant({
    Ok: IDL.Record({
      rows_blob: IDL.Opt(IDL.Vec(IDL.Nat8)),
      token: IDL.Opt(IDL.Text),
      row_count: IDL.Nat64,
      phase: IDL.Opt(IDL.Text),
    }),
    Err: IDL.Record({ code: IDL.Text, message: IDL.Text }),
  });
  const ok = icp(
    [
      "canister",
      "call",
      "-e",
      "local",
      ROUTER_CANISTER,
      "gql_query",
      argsText,
      "--query",
    ],
    QueryGqlResult,
  );
  if ("Err" in ok) { throw new Error(`gql_query failed for ${demoId}: ${JSON.stringify(ok.Err)}`); }
  const rowsBlob = ok.Ok.rows_blob;
  if (!rowsBlob) {
    throw new Error(`No rows_blob for ${demoId}`);
  }
  // rowsBlob is a Uint8Array (an `opt vec nat8` decoded as Some). Some older
  // @icp-sdk/core versions returned the inner vec as a JS array of numbers; in
  // that case rowsBlob is a plain Array whose first element may be a nested
  // Array (i.e. the wrapped inner array, when the decoder unwraps Some
  // transparently). Handle both shapes.
  let bytes;
  if (rowsBlob instanceof Uint8Array) {
    bytes = rowsBlob;
  } else if (Array.isArray(rowsBlob) && rowsBlob[0] instanceof Uint8Array) {
    // Opt<Vec<Nat8>> decoded as Some(innerBytes) — Array wrapping a single
    // Uint8Array. Take the inner buffer.
    bytes = rowsBlob[0];
  } else if (Array.isArray(rowsBlob) && Array.isArray(rowsBlob[0])) {
    bytes = new Uint8Array(rowsBlob[0]);
  } else if (Array.isArray(rowsBlob)) {
    bytes = Uint8Array.from(rowsBlob);
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
  const values = meta.values.map((v) => `${Number(v).toFixed(1)} : float32`).join("; ");
  const argsText = `(
    record {
      logical_graph_name = "${GRAPH_NAME}";
      encoded_vertex_id = ${blobText(elementId)};
      embedding_name = "${EMBEDDING_NAME}";
      values = vec { ${values} };
    }
  )`;
  // admin_ingest_vertex_embedding returns Result<VertexEmbeddingIngestionResult, RouterError>.
  // The Ok variant is a record { embedding_version: nat64; projection_outcome: Variant{Applied, DeferredForRepair} }.
  // The Err variant can be any RouterError variant (NotAuthorized, NotFound, Conflict,
  // InvalidArgument, GraphUnavailable, Internal, …). Use an empty Variant to allow any shape;
  // we detect Err by key presence and look for the AlreadyExists-style conflict text.
  const IngestResult = IDL.Variant({
    Ok: IDL.Record({
      embedding_version: IDL.Nat64,
      projection_outcome: IDL.Variant({
        Applied: IDL.Null,
        DeferredForRepair: IDL.Null,
      }),
    }),
    Err: IDL.Variant({}),
  });
  const parsed = icp(
    [
      "canister",
      "call",
      "-e",
      "local",
      ROUTER_CANISTER,
      "admin_ingest_vertex_embedding",
      argsText,
    ],
    IngestResult,
  );
  if ("Err" in parsed) {
    const errText = JSON.stringify(parsed.Err);
    if (/AlreadyExists|already exists|Conflict|conflict/i.test(errText)) {
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
