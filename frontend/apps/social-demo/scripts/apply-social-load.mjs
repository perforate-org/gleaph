#!/usr/bin/env node
import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { pathToFileURL } from "node:url";
import { IDL } from "@icp-sdk/core/candid";
import { encodeCanonicalGqlValue } from "../../../../sdk/client/js/src/canonical-value.ts";
import { GqlQueryRows } from "../../../../sdk/client/js/src/idl.ts";
import { createRouterActor } from "./actor.mjs";
import { ingestEmbeddingsBatch } from "./ingest-social-embeddings.mjs";

export const SOCIAL_LOAD_KEY = "social-demo-initial-load-v1";
export const MARKER_PROTOCOL_VERSION = 1;
const SENTINEL_ID = 9_000_000;
const SENTINEL_USER_ID = "__gleaph_bootstrap_user";
const DEFAULT_CHUNK_SIZE = 256;
const STATUS_PAGE_SIZE = 64;
const ROUTER_PRE_ADMISSION_PAYLOAD_LIMITS = new Set([
  "bulk-load command exceeds the safe payload bound",
  "ordered vertex batch request exceeds the safe payload limit",
  "ordered edge batch request exceeds the safe payload limit",
]);

const MARKER_QUERY =
  "MATCH (m:SeedLoad {logical_seed_key: $logical_seed_key}) RETURN " +
  "m.protocol_version AS protocol_version, m.logical_seed_key AS logical_seed_key, " +
  "m.artifact_sha256 AS artifact_sha256, m.bulk_job_key AS bulk_job_key, m.state AS state";

const SENTINELS = [
  [
    "User",
    "social-bootstrap-user",
    `INSERT (n:User {demo_id: ${SENTINEL_ID}, demo_graph: 'social', name: '__bootstrap', user_id: '${SENTINEL_USER_ID}'})`,
  ],
  [
    "Post",
    "social-bootstrap-post",
    `INSERT (n:Post {demo_id: ${SENTINEL_ID}, demo_graph: 'social', body: '__bootstrap'})`,
  ],
  [
    "Feed",
    "social-bootstrap-feed",
    `INSERT (n:Feed {demo_id: ${SENTINEL_ID}, demo_graph: 'social', name: '__bootstrap'})`,
  ],
  [
    "Topic",
    "social-bootstrap-topic",
    `INSERT (n:Topic {demo_id: ${SENTINEL_ID}, demo_graph: 'social', name: '__bootstrap'})`,
  ],
  [
    "Community",
    "social-bootstrap-community",
    `INSERT (n:Community {demo_id: ${SENTINEL_ID}, demo_graph: 'social', name: '__bootstrap'})`,
  ],
];

const INDEXES = [
  ["User", "user_id"],
  ["Post", "demo_id"],
  ["Feed", "demo_id"],
  ["Topic", "demo_id"],
  ["User", "demo_id"],
];

const sleep = (milliseconds) =>
  new Promise((resolve) => setTimeout(resolve, milliseconds));

function unwrap(result, operation) {
  if (result && "Ok" in result) return result.Ok;
  throw new Error(
    `${operation} failed: ${JSON.stringify(result?.Err ?? result)}`,
  );
}

function isNotFound(result) {
  return Boolean(
    result && "Err" in result && result.Err && "NotFound" in result.Err,
  );
}

function encodeParams(params = {}) {
  return encodeCanonicalGqlValue({
    Record: Object.fromEntries(
      Object.entries(params).map(([name, value]) => [
        name.startsWith("$") ? name : `$${name}`,
        value,
      ]),
    ),
  });
}

function propertyList(properties) {
  return Object.entries(properties)
    .sort(([left], [right]) => Buffer.from(left).compare(Buffer.from(right)))
    .map(([property_name, value]) => ({
      property_name,
      value: encodeCanonicalGqlValue(value),
    }));
}

function chunks(items, size) {
  const result = [];
  for (let start = 0; start < items.length; start += size)
    result.push(items.slice(start, start + size));
  return result;
}

function makeVertexChunk(vertices) {
  return {
    sourceIds: vertices.map(({ source_id }) => source_id),
    chunk: {
      Vertices: vertices.map((vertex) => ({
        vertex_labels: [...vertex.labels],
        initial_properties: propertyList(vertex.properties ?? {}),
      })),
    },
  };
}

function makeEdgeChunk(edges, sourceIdMap) {
  return {
    chunk: {
      Edges: edges.map((edge) => {
        const source = sourceIdMap.get(edge.source_id);
        const target = sourceIdMap.get(edge.target_id);
        if (
          !(source instanceof Uint8Array) ||
          !(target instanceof Uint8Array)
        ) {
          throw new Error(
            `edge ${edge.source_id}->${edge.target_id} lacks a replay-proven endpoint`,
          );
        }
        return {
          source,
          target,
          directed: edge.directed,
          edge_label_name: [edge.label],
          inline_property:
            edge.inline_value === null || edge.inline_value === undefined
              ? []
              : [encodeCanonicalGqlValue(edge.inline_value)],
          initial_edge_properties: propertyList(edge.properties ?? {}),
        };
      }),
    },
  };
}

function assertArtifact(artifact) {
  if (
    !artifact ||
    !Array.isArray(artifact.vertices) ||
    !Array.isArray(artifact.edges)
  ) {
    throw new Error(
      "social-load artifact must contain vertices and edges arrays",
    );
  }
  if (!artifact.embeddings || typeof artifact.embeddings !== "object") {
    throw new Error(
      "social-load artifact must contain embeddings keyed by source_id",
    );
  }
  const ids = new Set();
  for (const vertex of artifact.vertices) {
    if (
      typeof vertex.source_id !== "string" ||
      vertex.source_id.length === 0 ||
      ids.has(vertex.source_id)
    ) {
      throw new Error(
        "social-load vertices require unique non-empty source_id values",
      );
    }
    ids.add(vertex.source_id);
    if (!Array.isArray(vertex.labels) || vertex.labels.length === 0) {
      throw new Error(`social-load vertex ${vertex.source_id} requires labels`);
    }
    propertyList(vertex.properties ?? {});
  }
  for (const edge of artifact.edges) {
    if (!ids.has(edge.source_id) || !ids.has(edge.target_id)) {
      throw new Error(
        "social-load edge endpoint does not resolve to a vertex source_id",
      );
    }
    if (typeof edge.label !== "string" || edge.label.length === 0) {
      throw new Error("social-load edge label must not be empty");
    }
    propertyList(edge.properties ?? {});
    if (edge.inline_value !== null && edge.inline_value !== undefined) {
      encodeCanonicalGqlValue(edge.inline_value);
    }
  }
  for (const sourceId of Object.keys(artifact.embeddings)) {
    if (!ids.has(sourceId))
      throw new Error(`embedding source_id ${sourceId} is not a vertex`);
  }
}

export function makeVertexChunks(artifact, chunkSize = DEFAULT_CHUNK_SIZE) {
  assertArtifact(artifact);
  return chunks(artifact.vertices, chunkSize).map(makeVertexChunk);
}

export function makeEdgeChunks(
  artifact,
  sourceIdMap,
  chunkSize = DEFAULT_CHUNK_SIZE,
) {
  return chunks(artifact.edges, chunkSize).map((edges) =>
    makeEdgeChunk(edges, sourceIdMap),
  );
}

async function gqlMutate(actor, graphName, query, params, key) {
  return unwrap(
    await actor.gql_mutate(
      `USE ${graphName} ${query}`,
      encodeParams(params),
      key,
    ),
    `gql_mutate(${key})`,
  );
}

export async function waitMutationCompleted(
  actor,
  graphName,
  key,
  options = {},
) {
  const attempts = options.statusAttempts ?? 240;
  const intervalMs = options.statusIntervalMs ?? 250;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    const status = unwrap(
      await actor.mutation_status([graphName], key),
      `mutation_status(${key})`,
    );
    if ("Completed" in status.phase) return status;
    if ("Failed" in status.phase) {
      throw new Error(
        `mutation ${key} failed: ${status.last_error?.[0] ?? "unknown failure"}`,
      );
    }
    if (attempt + 1 < attempts && intervalMs > 0) await sleep(intervalMs);
  }
  throw new Error(`mutation ${key} did not reach Completed`);
}

function decodeRows(result) {
  if (!result.rows_blob || result.rows_blob.length !== 1) return [];
  const [decoded] = IDL.decode([GqlQueryRows], result.rows_blob[0]);
  return decoded.rows.map(({ columns }) => Object.fromEntries(columns));
}

function scalar(row, name, variant) {
  const value = row[name];
  if (!value || typeof value !== "object" || !(variant in value)) {
    throw new Error(`SeedLoad marker ${name} must be ${variant}`);
  }
  return value[variant];
}

export async function readMarker(actor, graphName) {
  let response;
  try {
    response = await actor.gql_query(
      `USE ${graphName} ${MARKER_QUERY}`,
      encodeParams({ logical_seed_key: { Text: SOCIAL_LOAD_KEY } }),
      { Eventual: null },
    );
  } catch (error) {
    throw new Error(
      `SeedLoad marker query is ambiguous; reset the graph: ${error.message}`,
    );
  }
  if (!response || !("Ok" in response)) {
    throw new Error(
      `SeedLoad marker query failed; reset the graph: ${JSON.stringify(response?.Err)}`,
    );
  }
  const result = response.Ok;
  const rows = decodeRows(result);
  if (rows.length === 0) return null;
  if (rows.length !== 1)
    throw new Error("SeedLoad marker is duplicated; reset the graph");
  const row = rows[0];
  return {
    protocol_version: Number(scalar(row, "protocol_version", "Int64")),
    logical_seed_key: scalar(row, "logical_seed_key", "Text"),
    artifact_sha256: scalar(row, "artifact_sha256", "Text"),
    bulk_job_key: scalar(row, "bulk_job_key", "Text"),
    state: scalar(row, "state", "Text"),
  };
}

export function validateMarker(marker, digest) {
  if (!marker) return null;
  if (
    marker.protocol_version !== MARKER_PROTOCOL_VERSION ||
    marker.logical_seed_key !== SOCIAL_LOAD_KEY ||
    marker.bulk_job_key !== SOCIAL_LOAD_KEY ||
    !/^[0-9a-f]{64}$/.test(marker.artifact_sha256) ||
    !["Prepared", "Complete"].includes(marker.state)
  ) {
    throw new Error("SeedLoad marker is malformed; reset the graph");
  }
  if (marker.artifact_sha256 !== digest) {
    throw new Error(
      "SeedLoad marker artifact differs from social-load.json; reset the graph",
    );
  }
  return marker;
}

async function prepareFreshGraph(actor, graphName, digest, options) {
  for (const [, key, query] of SENTINELS) {
    await gqlMutate(actor, graphName, query, {}, key);
    await waitMutationCompleted(actor, graphName, key, options);
  }
  for (const [label, property] of INDEXES) {
    unwrap(
      await actor.index_vertex_property(graphName, label, property),
      `index ${label}.${property}`,
    );
  }

  const markerKey = "social-seed-load-marker-prepared-v1";
  await gqlMutate(
    actor,
    graphName,
    "INSERT (:SeedLoad {protocol_version: $protocol_version, logical_seed_key: $logical_seed_key, artifact_sha256: $artifact_sha256, bulk_job_key: $bulk_job_key, state: $state})",
    {
      protocol_version: { Int64: MARKER_PROTOCOL_VERSION },
      logical_seed_key: { Text: SOCIAL_LOAD_KEY },
      artifact_sha256: { Text: digest },
      bulk_job_key: { Text: SOCIAL_LOAD_KEY },
      state: { Text: "Prepared" },
    },
    markerKey,
  );
  await waitMutationCompleted(actor, graphName, markerKey, options);
  unwrap(
    await actor.index_vertex_property(
      graphName,
      "SeedLoad",
      "logical_seed_key",
    ),
    "index SeedLoad.logical_seed_key",
  );
  const marker = validateMarker(await readMarker(actor, graphName), digest);
  if (!marker || marker.state !== "Prepared") {
    throw new Error(
      "SeedLoad marker creation was not durably verified as Prepared; reset the graph",
    );
  }

  const cleanupKey = "social-bootstrap-delete";
  await gqlMutate(
    actor,
    graphName,
    `MATCH (n:User|Post|Feed|Topic|Community {demo_id: ${SENTINEL_ID}, demo_graph: 'social'}) DETACH DELETE n`,
    {},
    cleanupKey,
  );
  await waitMutationCompleted(actor, graphName, cleanupKey, options);
  return marker;
}

async function loadStatusPage(actor, graphName, cursor = []) {
  const result = await actor.bulk_load_status(
    [graphName],
    SOCIAL_LOAD_KEY,
    cursor,
    STATUS_PAGE_SIZE,
  );
  if (isNotFound(result)) return null;
  return unwrap(result, "bulk_load_status is ambiguous; reset the graph");
}

async function loadStatus(actor, graphName) {
  return loadStatusPage(actor, graphName);
}

async function startFreshJob(actor, graphName) {
  const command = {
    Start: { graph_name: [graphName], client_bulk_key: SOCIAL_LOAD_KEY },
  };
  try {
    const response = unwrap(await actor.bulk_load(command), "bulk_load Start");
    if (!("Started" in response))
      throw new Error("bulk_load Start returned an unexpected response");
  } catch (error) {
    const status = await loadStatus(actor, graphName);
    if (!status)
      throw new Error(
        `bulk_load Start outcome is ambiguous; reset the graph: ${error.message}`,
      );
    return status;
  }
  const status = await loadStatus(actor, graphName);
  if (!status)
    throw new Error(
      "bulk_load Start completed without durable status; reset the graph",
    );
  return status;
}

function statusStateName(status) {
  if (!status?.state || typeof status.state !== "object") {
    throw new Error("bulk status state is malformed; reset the graph");
  }
  const names = Object.keys(status.state);
  const known = new Set([
    "Open",
    "AppendPending",
    "FinalizePending",
    "AbortPending",
    "Completed",
    "Aborted",
    "Failed",
  ]);
  if (names.length !== 1 || !known.has(names[0])) {
    throw new Error("bulk status state is malformed; reset the graph");
  }
  return names[0];
}

function validateStatus(status) {
  const state = statusStateName(status);
  for (const name of [
    "next_chunk_index",
    "committed_chunk_count",
    "completed_chunk_count",
  ]) {
    if (!Number.isSafeInteger(status[name]) || status[name] < 0) {
      throw new Error(`bulk status ${name} is malformed; reset the graph`);
    }
  }
  const active = state === "AppendPending" || state === "AbortPending";
  if (
    status.completed_chunk_count > status.committed_chunk_count ||
    status.committed_chunk_count > status.next_chunk_index + (active ? 1 : 0) ||
    status.completed_chunk_count !== status.next_chunk_index ||
    (!active && status.committed_chunk_count !== status.next_chunk_index)
  ) {
    throw new Error("bulk status counters are inconsistent; reset the graph");
  }
  return state;
}

function optionalValue(value, name) {
  if (!Array.isArray(value) || value.length > 1) {
    throw new Error(`bulk status ${name} is malformed; reset the graph`);
  }
  return value[0];
}

function receiptOperationCount(row) {
  const count = row?.receipt?.logical_operation_count;
  if (
    typeof count !== "bigint" ||
    count <= 0n ||
    count > BigInt(Number.MAX_SAFE_INTEGER)
  ) {
    throw new Error(
      "bulk receipt operation_count is malformed; reset the graph",
    );
  }
  return Number(count);
}

function receiptsEqual(left, right) {
  if (
    left.logical_operation_count !== right.logical_operation_count ||
    left.logical_vertex_count !== right.logical_vertex_count ||
    left.logical_edge_count !== right.logical_edge_count ||
    left.allocated_vertex_ids.length !== right.allocated_vertex_ids.length
  )
    return false;
  return left.allocated_vertex_ids.every(
    (value, index) =>
      value instanceof Uint8Array &&
      right.allocated_vertex_ids[index] instanceof Uint8Array &&
      Buffer.from(value).equals(Buffer.from(right.allocated_vertex_ids[index])),
  );
}

async function loadReceiptSnapshot(actor, graphName) {
  let cursor = [];
  let baseline;
  const receipts = [];
  const seenCursors = new Set();
  while (true) {
    const page = await loadStatusPage(actor, graphName, cursor);
    if (!page) return null;
    const state = validateStatus(page);
    const signature = JSON.stringify([
      state,
      page.next_chunk_index,
      page.committed_chunk_count,
      page.completed_chunk_count,
    ]);
    if (baseline === undefined) baseline = signature;
    if (signature !== baseline) {
      throw new Error(
        "bulk status changed while reading receipt pages; reset the graph",
      );
    }
    if (!Array.isArray(page.receipts)) {
      throw new Error("bulk status receipts are malformed; reset the graph");
    }
    if (page.receipts.length > STATUS_PAGE_SIZE) {
      throw new Error(
        "bulk status receipt page exceeds the requested bound; reset the graph",
      );
    }
    for (const row of page.receipts) {
      if (
        !Number.isSafeInteger(row?.chunk_index) ||
        row.chunk_index !== receipts.length
      ) {
        throw new Error(
          "bulk receipt pages are not an ordered prefix; reset the graph",
        );
      }
      receiptOperationCount(row);
      receipts.push(row);
    }
    const next = optionalValue(page.next_receipt_cursor, "next_receipt_cursor");
    if (next === undefined) {
      if (receipts.length !== page.committed_chunk_count) {
        throw new Error(
          "bulk receipt pages do not cover the committed prefix; reset the graph",
        );
      }
      if (
        state === "AppendPending" &&
        page.committed_chunk_count === page.next_chunk_index
      ) {
        throw new Error(
          "active bulk chunk has no recoverable receipt boundary; reset the graph",
        );
      }
      return { status: page, receipts };
    }
    if (!Number.isSafeInteger(next) || next < 0 || seenCursors.has(next)) {
      throw new Error("bulk receipt cursor is malformed; reset the graph");
    }
    seenCursors.add(next);
    cursor = [next];
  }
}

function chunkOperationCount(chunk) {
  return "Vertices" in chunk ? chunk.Vertices.length : chunk.Edges.length;
}

function splitCandidate(candidate) {
  const count = chunkOperationCount(candidate.chunk);
  const midpoint = Math.ceil(count / 2);
  if ("Vertices" in candidate.chunk) {
    return [
      {
        sourceIds: candidate.sourceIds.slice(0, midpoint),
        chunk: { Vertices: candidate.chunk.Vertices.slice(0, midpoint) },
      },
      {
        sourceIds: candidate.sourceIds.slice(midpoint),
        chunk: { Vertices: candidate.chunk.Vertices.slice(midpoint) },
      },
    ];
  }
  return [
    { chunk: { Edges: candidate.chunk.Edges.slice(0, midpoint) } },
    { chunk: { Edges: candidate.chunk.Edges.slice(midpoint) } },
  ];
}

export function isExplicitPreAdmissionPayloadRejection(error) {
  const candidates = [error, error?.cause];
  for (const candidate of candidates) {
    if (!candidate || typeof candidate !== "object") continue;
    if (
      candidate.status === 413 ||
      candidate.statusCode === 413 ||
      candidate.response?.status === 413 ||
      candidate.code === "PAYLOAD_TOO_LARGE" ||
      candidate.name === "PAYLOAD_TOO_LARGE" ||
      candidate.message === "PAYLOAD_TOO_LARGE" ||
      /^HTTP(?: status)? 413\b/.test(candidate.message ?? "")
    ) {
      return true;
    }
    const routerError = candidate.routerError ?? candidate.Err;
    if (
      routerError &&
      typeof routerError === "object" &&
      ROUTER_PRE_ADMISSION_PAYLOAD_LIMITS.has(routerError.InvalidArgument)
    ) {
      return true;
    }
  }
  return false;
}

async function sendAppend(actor, graphName, chunkIndex, chunk, operation) {
  const command = {
    Append: {
      graph_name: [graphName],
      client_bulk_key: SOCIAL_LOAD_KEY,
      chunk_index: chunkIndex,
      chunk,
    },
  };
  const result = await actor.bulk_load(command);
  if (!result || !("Ok" in result)) {
    const error = new Error(
      `${operation} failed: ${JSON.stringify(result?.Err ?? result)}`,
    );
    Object.defineProperty(error, "routerError", { value: result?.Err });
    throw error;
  }
  const response = result.Ok;
  if (
    !("Appended" in response) ||
    response.Appended.chunk_index !== chunkIndex
  ) {
    throw new Error(
      `bulk_load Append(${chunkIndex}) returned an unexpected response`,
    );
  }
  return response.Appended.receipt;
}

async function appendAdaptive(actor, graphName, chunkIndex, candidate) {
  try {
    const receipt = await sendAppend(
      actor,
      graphName,
      chunkIndex,
      candidate.chunk,
      `bulk_load Append(${chunkIndex})`,
    );
    return { entries: [{ ...candidate, receipt }], nextIndex: chunkIndex + 1 };
  } catch (error) {
    const status = await loadStatus(actor, graphName);
    if (!status) {
      throw new Error(
        `bulk_load Append(${chunkIndex}) outcome is ambiguous; reset the graph: ${error.message}`,
      );
    }
    const state = validateStatus(status);
    const active = state === "AppendPending" || state === "AbortPending";
    const atOriginalBoundary =
      state === "Open" &&
      status.next_chunk_index === chunkIndex &&
      status.committed_chunk_count === chunkIndex;
    if (
      !active &&
      !atOriginalBoundary &&
      status.next_chunk_index <= chunkIndex
    ) {
      throw new Error(
        `bulk_load Append(${chunkIndex}) status cannot prove a replay boundary; reset the graph`,
      );
    }
    if (isExplicitPreAdmissionPayloadRejection(error) && atOriginalBoundary) {
      if (chunkOperationCount(candidate.chunk) === 1) throw error;
      const [left, right] = splitCandidate(candidate);
      const first = await appendAdaptive(actor, graphName, chunkIndex, left);
      const second = await appendAdaptive(
        actor,
        graphName,
        first.nextIndex,
        right,
      );
      return {
        entries: [...first.entries, ...second.entries],
        nextIndex: second.nextIndex,
      };
    }
    const receipt = await sendAppend(
      actor,
      graphName,
      chunkIndex,
      candidate.chunk,
      `bulk_load exact replay Append(${chunkIndex})`,
    );
    return { entries: [{ ...candidate, receipt }], nextIndex: chunkIndex + 1 };
  }
}

async function replayReceiptBoundary(actor, graphName, row, candidate) {
  const receipt = await sendAppend(
    actor,
    graphName,
    row.chunk_index,
    candidate.chunk,
    `bulk_load exact replay Append(${row.chunk_index})`,
  );
  return { ...candidate, receipt };
}

function partitionReceiptBoundaries(artifact, receiptRows) {
  const vertexRows = [];
  const edgeRows = [];
  let vertexOffset = 0;
  let edgeOffset = 0;
  for (const row of receiptRows) {
    const count = receiptOperationCount(row);
    if (vertexOffset < artifact.vertices.length) {
      if (count > artifact.vertices.length - vertexOffset) {
        throw new Error(
          `bulk receipt ${row.chunk_index} crosses the vertex/edge boundary; reset the graph`,
        );
      }
      vertexRows.push({ row, start: vertexOffset, count });
      vertexOffset += count;
      continue;
    }
    if (count > artifact.edges.length - edgeOffset) {
      throw new Error(
        `bulk receipt ${row.chunk_index} extends beyond the social-load artifact; reset the graph`,
      );
    }
    edgeRows.push({ row, start: edgeOffset, count });
    edgeOffset += count;
  }
  return { vertexRows, edgeRows, vertexOffset, edgeOffset };
}

function validateVertexReceipt(entry, chunkIndex) {
  const { receipt, sourceIds } = entry;
  const count = BigInt(sourceIds.length);
  if (
    receipt.logical_operation_count !== count ||
    receipt.logical_vertex_count !== count ||
    receipt.logical_edge_count !== 0n ||
    receipt.allocated_vertex_ids.length !== sourceIds.length ||
    receipt.allocated_vertex_ids.some((value) => !(value instanceof Uint8Array))
  ) {
    throw new Error(`vertex receipt ${chunkIndex} cardinality mismatch`);
  }
}

function validateEdgeReceipt(entry, chunkIndex) {
  const { receipt, chunk } = entry;
  const count = BigInt(chunk.Edges.length);
  if (
    receipt.logical_operation_count !== count ||
    receipt.logical_vertex_count !== 0n ||
    receipt.logical_edge_count !== count ||
    receipt.allocated_vertex_ids.length !== 0
  ) {
    throw new Error(`edge receipt ${chunkIndex} cardinality mismatch`);
  }
}

async function finalizeBulk(actor, graphName, options) {
  const command = {
    Finalize: { graph_name: [graphName], client_bulk_key: SOCIAL_LOAD_KEY },
  };
  try {
    const response = unwrap(
      await actor.bulk_load(command),
      "bulk_load Finalize",
    );
    if (!("FinalizeAccepted" in response)) {
      throw new Error("bulk_load Finalize returned an unexpected response");
    }
  } catch (error) {
    const status = await loadStatus(actor, graphName);
    if (!status) {
      throw new Error(
        `bulk_load Finalize outcome is ambiguous; reset the graph: ${error.message}`,
      );
    }
    const state = validateStatus(status);
    if (state === "Open") {
      const replay = unwrap(
        await actor.bulk_load(command),
        "bulk_load exact replay Finalize",
      );
      if (!("FinalizeAccepted" in replay)) {
        throw new Error(
          "bulk_load exact replay Finalize returned an unexpected response",
        );
      }
    } else if (state !== "FinalizePending" && state !== "Completed") {
      throw new Error(
        `bulk_load Finalize failed in durable state ${state}; reset the graph`,
      );
    }
  }
  return waitBulkCompleted(actor, graphName, options);
}

async function waitBulkCompleted(actor, graphName, options) {
  const attempts = options.statusAttempts ?? 240;
  const intervalMs = options.statusIntervalMs ?? 250;
  for (let attempt = 0; attempt < attempts; attempt += 1) {
    const status = await loadStatus(actor, graphName);
    if (!status)
      throw new Error(
        "Prepared SeedLoad lost its bulk status; reset the graph",
      );
    const state = validateStatus(status);
    if (state === "Completed") return status;
    if (state === "Failed" || state === "Aborted") {
      throw new Error(
        "SeedLoad bulk job is terminal but incomplete; reset the graph",
      );
    }
    if (attempt + 1 < attempts && intervalMs > 0) await sleep(intervalMs);
  }
  throw new Error("SeedLoad bulk job did not reach Completed");
}

async function completeMarker(actor, graphName, digest, options) {
  const before = validateMarker(await readMarker(actor, graphName), digest);
  if (!before || before.state !== "Prepared") {
    throw new Error(
      "SeedLoad marker changed before completion; reset the graph",
    );
  }
  const key = "social-seed-load-marker-complete-v1";
  const result = await gqlMutate(
    actor,
    graphName,
    "MATCH (m:SeedLoad {protocol_version: $protocol_version, logical_seed_key: $logical_seed_key, artifact_sha256: $artifact_sha256, bulk_job_key: $bulk_job_key, state: 'Prepared'}) SET m.state = 'Complete' RETURN m",
    {
      protocol_version: { Int64: MARKER_PROTOCOL_VERSION },
      logical_seed_key: { Text: SOCIAL_LOAD_KEY },
      artifact_sha256: { Text: digest },
      bulk_job_key: { Text: SOCIAL_LOAD_KEY },
    },
    key,
  );
  if (result.row_count !== 1n)
    throw new Error("SeedLoad completion did not match exactly one marker");
  await waitMutationCompleted(actor, graphName, key, options);
  const marker = validateMarker(await readMarker(actor, graphName), digest);
  if (!marker || marker.state !== "Complete")
    throw new Error("SeedLoad completion marker verification failed");
}

export async function applySocialLoad({
  actor,
  artifact,
  artifactBytes,
  graphName = process.env.GLEAPH_DEMO_GRAPH_NAME || "gleaph.pocket_ic",
  freshBootstrap = false,
  chunkSize = DEFAULT_CHUNK_SIZE,
  ingestEmbeddings = ingestEmbeddingsBatch,
  ...options
}) {
  assertArtifact(artifact);
  let artifactFromBytes;
  try {
    artifactFromBytes = JSON.parse(Buffer.from(artifactBytes).toString("utf8"));
  } catch (error) {
    throw new Error(
      `social-load artifact bytes are not valid JSON: ${error.message}`,
    );
  }
  if (JSON.stringify(artifactFromBytes) !== JSON.stringify(artifact)) {
    throw new Error(
      "social-load artifact object does not match the digested file bytes",
    );
  }
  const digest = createHash("sha256").update(artifactBytes).digest("hex");
  // Classify the durable marker before any fresh-bootstrap mutation. The
  // operator-provided freshBootstrap flag is only a proof for the observed
  // marker-absence path; it must never override an existing marker state.
  let marker = validateMarker(await readMarker(actor, graphName), digest);
  if (marker?.state === "Complete") return { skipped: true, digest };
  if (!marker) {
    if (!freshBootstrap)
      throw new Error(
        "SeedLoad marker is missing on a non-fresh deployment; reset the graph",
      );
    marker = await prepareFreshGraph(actor, graphName, digest, options);
    if (!marker || marker.state !== "Prepared") {
      throw new Error(
        "SeedLoad marker creation was not durably verified as Prepared; reset the graph",
      );
    }
    await startFreshJob(actor, graphName);
  }

  if (!marker || marker.state !== "Prepared")
    throw new Error("SeedLoad marker is not Prepared");
  if (
    !Number.isSafeInteger(chunkSize) ||
    chunkSize < 1 ||
    chunkSize > DEFAULT_CHUNK_SIZE
  ) {
    throw new Error(
      `chunkSize must be an integer in 1..=${DEFAULT_CHUNK_SIZE}`,
    );
  }
  const snapshot = await loadReceiptSnapshot(actor, graphName);
  if (!snapshot)
    throw new Error(
      "Prepared SeedLoad has no retained bulk status; reset the graph",
    );
  const initialState = statusStateName(snapshot.status);
  if (
    initialState === "Failed" ||
    initialState === "Aborted" ||
    initialState === "AbortPending"
  ) {
    throw new Error(
      "SeedLoad bulk job is terminal but incomplete; reset the graph",
    );
  }
  const boundaries = partitionReceiptBoundaries(artifact, snapshot.receipts);

  const vertexEntries = [];
  for (const boundary of boundaries.vertexRows) {
    const candidate = makeVertexChunk(
      artifact.vertices.slice(boundary.start, boundary.start + boundary.count),
    );
    const entry = await replayReceiptBoundary(
      actor,
      graphName,
      boundary.row,
      candidate,
    );
    vertexEntries.push({
      ...entry,
      statusReceipt: boundary.row.receipt,
      chunkIndex: boundary.row.chunk_index,
    });
  }
  let nextIndex = boundaries.vertexRows.length;
  let vertexOffset = boundaries.vertexOffset;
  while (vertexOffset < artifact.vertices.length) {
    const end = Math.min(vertexOffset + chunkSize, artifact.vertices.length);
    const candidate = makeVertexChunk(
      artifact.vertices.slice(vertexOffset, end),
    );
    const appended = await appendAdaptive(
      actor,
      graphName,
      nextIndex,
      candidate,
    );
    for (const entry of appended.entries) {
      vertexEntries.push({ ...entry, chunkIndex: nextIndex });
      nextIndex += 1;
    }
    vertexOffset = end;
  }

  const sourceIdMap = new Map();
  for (const entry of vertexEntries) {
    validateVertexReceipt(entry, entry.chunkIndex);
    if (
      entry.statusReceipt &&
      !receiptsEqual(entry.receipt, entry.statusReceipt)
    ) {
      throw new Error(
        `bulk receipt ${entry.chunkIndex} changed after exact replay; reset the graph`,
      );
    }
    for (
      let ordinal = 0;
      ordinal < entry.receipt.allocated_vertex_ids.length;
      ordinal += 1
    ) {
      sourceIdMap.set(
        entry.sourceIds[ordinal],
        entry.receipt.allocated_vertex_ids[ordinal],
      );
    }
  }

  const edgeEntries = [];
  for (const boundary of boundaries.edgeRows) {
    const candidate = makeEdgeChunk(
      artifact.edges.slice(boundary.start, boundary.start + boundary.count),
      sourceIdMap,
    );
    const entry = await replayReceiptBoundary(
      actor,
      graphName,
      boundary.row,
      candidate,
    );
    edgeEntries.push({
      ...entry,
      statusReceipt: boundary.row.receipt,
      chunkIndex: boundary.row.chunk_index,
    });
    nextIndex += 1;
  }
  let edgeOffset = boundaries.edgeOffset;
  while (edgeOffset < artifact.edges.length) {
    const end = Math.min(edgeOffset + chunkSize, artifact.edges.length);
    const candidate = makeEdgeChunk(
      artifact.edges.slice(edgeOffset, end),
      sourceIdMap,
    );
    const appended = await appendAdaptive(
      actor,
      graphName,
      nextIndex,
      candidate,
    );
    for (const entry of appended.entries) {
      edgeEntries.push({ ...entry, chunkIndex: nextIndex });
      nextIndex += 1;
    }
    edgeOffset = end;
  }
  for (const entry of edgeEntries) {
    validateEdgeReceipt(entry, entry.chunkIndex);
    if (
      entry.statusReceipt &&
      !receiptsEqual(entry.receipt, entry.statusReceipt)
    ) {
      throw new Error(
        `bulk receipt ${entry.chunkIndex} changed after exact replay; reset the graph`,
      );
    }
  }

  const chunkCount = nextIndex;
  let status = await loadStatus(actor, graphName);
  if (status) validateStatus(status);
  if (!status || status.next_chunk_index !== chunkCount) {
    throw new Error(
      "bulk status chunk count differs from regenerated social-load artifact",
    );
  }
  if (!("Completed" in status.state)) {
    if (!("Open" in status.state))
      throw new Error("bulk job is not ready to finalize");
    status = await finalizeBulk(actor, graphName, options);
  }
  if (status.next_chunk_index !== chunkCount) {
    throw new Error(
      "completed bulk status chunk count differs from regenerated artifact",
    );
  }

  await ingestEmbeddings(sourceIdMap, artifact.embeddings);
  await completeMarker(actor, graphName, digest, options);
  return { skipped: false, digest, sourceIdMap };
}

export async function main() {
  const artifactPath =
    process.argv[2] || new URL("../seeds/social-load.json", import.meta.url);
  const artifactBytes = readFileSync(artifactPath);
  const artifact = JSON.parse(artifactBytes.toString("utf8"));
  const actor = await createRouterActor(
    process.env.GLEAPH_DEMO_ROUTER_CANISTER || "gleaph-router",
  );
  const freshBootstrap = process.env.GLEAPH_DEMO_FRESH_BOOTSTRAP === "1";
  const result = await applySocialLoad({
    actor,
    artifact,
    artifactBytes,
    freshBootstrap,
  });
  console.log(
    result.skipped
      ? "[social-demo] typed load already Complete; skipped"
      : "[social-demo] typed load complete",
  );
}

if (
  process.argv[1] &&
  import.meta.url === pathToFileURL(process.argv[1]).href
) {
  main().catch((error) => {
    console.error(error);
    process.exitCode = 1;
  });
}
