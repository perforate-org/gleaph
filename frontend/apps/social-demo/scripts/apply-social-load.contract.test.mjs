import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import test from "node:test";
import { IDL } from "@icp-sdk/core/candid";
import { GqlQueryRows } from "../../../../sdk/client/js/src/idl.ts";
import {
  applySocialLoad,
  isExplicitPreAdmissionPayloadRejection,
  SOCIAL_LOAD_KEY,
} from "./apply-social-load.mjs";

const id = (value) => Uint8Array.from([0, 0, 0, 0, 0, 0, 0, value]);
const artifact = {
  vertices: [
    { source_id: "user", labels: ["User"], properties: { user_id: { Text: "user" } } },
    { source_id: "post", labels: ["Post"], properties: { demo_id: { Int64: 1 } } },
  ],
  edges: [{
    source_id: "user",
    target_id: "post",
    directed: true,
    label: "POSTED",
    inline_value: null,
    properties: { demo_kind: { Text: "posted" } },
  }],
  embeddings: { post: { dims: 1, values: [0.5] } },
};
const artifactBytes = Buffer.from(`${JSON.stringify(artifact, null, 2)}\n`);
const digest = createHash("sha256").update(artifactBytes).digest("hex");

test("payload rejection classifier accepts only explicit transport and exact Router signals", () => {
  const http413 = Object.assign(new Error("request rejected"), { status: 413 });
  const payloadCode = Object.assign(new Error("request rejected"), { code: "PAYLOAD_TOO_LARGE" });
  const routerLimit = Object.assign(new Error("Router rejected the request"), {
    routerError: { InvalidArgument: "bulk-load command exceeds the safe payload bound" },
  });
  assert.equal(isExplicitPreAdmissionPayloadRejection(http413), true);
  assert.equal(isExplicitPreAdmissionPayloadRejection(payloadCode), true);
  assert.equal(isExplicitPreAdmissionPayloadRejection(routerLimit), true);
  assert.equal(isExplicitPreAdmissionPayloadRejection(new Error("network timeout")), false);
  assert.equal(isExplicitPreAdmissionPayloadRejection(new Error("payload too large")), false);
  assert.equal(
    isExplicitPreAdmissionPayloadRejection(Object.assign(new Error("Router rejected the request"), {
      routerError: { InvalidArgument: "atomic insert receipt exceeds the safe payload bound" },
    })),
    false,
  );
  assert.equal(
    isExplicitPreAdmissionPayloadRejection(Object.assign(new Error("Router rejected the request"), {
      routerError: { InvalidArgument: "413 pre-admission payload rejection" },
    })),
    false,
  );
});

function artifactPayload(value) {
  const bytes = Buffer.from(`${JSON.stringify(value, null, 2)}\n`);
  return { bytes, digest: createHash("sha256").update(bytes).digest("hex") };
}

function preparedMarker(artifactDigest = digest, state = "Prepared") {
  return {
    protocol_version: 1,
    logical_seed_key: SOCIAL_LOAD_KEY,
    artifact_sha256: artifactDigest,
    bulk_job_key: SOCIAL_LOAD_KEY,
    state,
  };
}

function markerBlob(marker, copies = 1) {
  if (!marker) return [];
  const row = () => ({ columns: [
    ["protocol_version", { Int64: BigInt(marker.protocol_version) }],
    ["logical_seed_key", { Text: marker.logical_seed_key }],
    ["artifact_sha256", { Text: marker.artifact_sha256 }],
    ["bulk_job_key", { Text: marker.bulk_job_key }],
    ["state", { Text: marker.state }],
  ] });
  return [IDL.encode([GqlQueryRows], [{ rows: Array.from({ length: copies }, row) }])];
}

function fakeActor({
  marker = null,
  markerCopies = 1,
  markerDigest = digest,
  markerCreateNoop = false,
  statusExists = false,
  loseStartResponse = false,
  loseAppendResponseAt = null,
  ambiguousAppendRaceAt = null,
  loseFinalizeResponse = false,
  rejectAbove = null,
  statusTransform = (page) => page,
  shared = null,
} = {}) {
  const calls = [];
  const durable = shared ?? {
    marker,
    storedChunks: new Map(),
    nextVertex: 1,
    state: statusExists ? { Open: null } : null,
  };
  let lostAppendResponse = false;
  let pendingAmbiguousAppend = null;
  let lostStartResponse = false;
  let lostFinalizeResponse = false;
  const actor = {
    calls,
    durable,
    async gql_mutate(query, _params, key) {
      calls.push(["gql_mutate", key, query]);
      if (key === "social-seed-load-marker-prepared-v1" && !markerCreateNoop) {
        durable.marker = {
          protocol_version: 1,
          logical_seed_key: SOCIAL_LOAD_KEY,
          artifact_sha256: markerDigest,
          bulk_job_key: SOCIAL_LOAD_KEY,
          state: "Prepared",
        };
      }
      if (key === "social-seed-load-marker-complete-v1") durable.marker.state = "Complete";
      return { Ok: { row_count: 1n, rows_blob: [], phase: [], token: [] } };
    },
    async mutation_status(_graph, key) {
      calls.push(["mutation_status", key]);
      return { Ok: { phase: { Completed: null }, last_error: [], mutation_id: 1n, next_action: "none", target_shard: [] } };
    },
    async index_vertex_property(_graph, label, property) {
      calls.push(["index_vertex_property", `${label}.${property}`]);
      return { Ok: null };
    },
    async gql_query() {
      calls.push(["gql_query", durable.marker?.state ?? "missing", markerCopies]);
      return { Ok: {
        row_count: durable.marker ? BigInt(markerCopies) : 0n,
        rows_blob: markerBlob(durable.marker, markerCopies),
        phase: [],
        token: [],
      } };
    },
    async bulk_load(command) {
      if ("Start" in command) {
        calls.push(["bulk_load", "Start"]);
        durable.state = { Open: null };
        if (loseStartResponse && !lostStartResponse) {
          lostStartResponse = true;
          throw new Error("simulated lost Start response");
        }
        return { Ok: { Started: { next_chunk_index: 0 } } };
      }
      if ("Append" in command) {
        const { chunk_index, chunk } = command.Append;
        const count = "Vertices" in chunk ? chunk.Vertices.length : chunk.Edges.length;
        calls.push([
          "bulk_load",
          `Append:${chunk_index}`,
          "Vertices" in chunk ? "Vertices" : "Edges",
          count,
          structuredClone(command.Append),
        ]);
        const existing = durable.storedChunks.get(chunk_index);
        const envelope = JSON.stringify(chunk, (_key, value) => value instanceof Uint8Array ? [...value] : value);
        if (existing && existing.envelope !== envelope) return { Err: { Conflict: "chunk differs" } };
        if (existing) return { Ok: { Appended: { chunk_index, receipt: existing.receipt } } };
        if (!("Open" in durable.state)) return { Err: { Conflict: "job is not Open" } };
        if (rejectAbove !== null && count > rejectAbove) {
          const error = new Error("HTTP 413 Payload Too Large");
          error.status = 413;
          throw error;
        }
        if (chunk_index === ambiguousAppendRaceAt && !lostAppendResponse) {
          lostAppendResponse = true;
          pendingAmbiguousAppend = { chunk_index, chunk: structuredClone(chunk), envelope };
          throw new Error("simulated ambiguous network timeout");
        }
        if ("Edges" in chunk) {
          const allocated = new Set(
            [...durable.storedChunks.values()]
              .flatMap(({ receipt }) => receipt.allocated_vertex_ids)
              .map((value) => Buffer.from(value).toString("hex")),
          );
          for (const edge of chunk.Edges) {
            if (
              !(edge.source instanceof Uint8Array) ||
              !(edge.target instanceof Uint8Array) ||
              !allocated.has(Buffer.from(edge.source).toString("hex")) ||
              !allocated.has(Buffer.from(edge.target).toString("hex"))
            ) {
              throw new Error("edge endpoint is not an encoded ID returned by a vertex receipt");
            }
          }
        }
        const allocated_vertex_ids = "Vertices" in chunk
          ? chunk.Vertices.map(() => id(durable.nextVertex++))
          : [];
        const receipt = {
          logical_operation_count: BigInt("Vertices" in chunk ? chunk.Vertices.length : chunk.Edges.length),
          logical_vertex_count: BigInt("Vertices" in chunk ? chunk.Vertices.length : 0),
          logical_edge_count: BigInt("Edges" in chunk ? chunk.Edges.length : 0),
          allocated_vertex_ids,
        };
        durable.storedChunks.set(chunk_index, { envelope, receipt });
        if (chunk_index === loseAppendResponseAt && !lostAppendResponse) {
          lostAppendResponse = true;
          throw new Error("simulated lost Append response");
        }
        return { Ok: { Appended: { chunk_index, receipt } } };
      }
      calls.push(["bulk_load", "Finalize"]);
      durable.state = { Completed: null };
      if (loseFinalizeResponse && !lostFinalizeResponse) {
        lostFinalizeResponse = true;
        throw new Error("simulated lost Finalize response");
      }
      return { Ok: { FinalizeAccepted: { state: durable.state } } };
    },
    async bulk_load_status(_graph, _key, cursor, maxReceipts) {
      calls.push(["bulk_load_status", durable.state ? Object.keys(durable.state)[0] : "NotFound", cursor]);
      if (!durable.state) return { Err: { NotFound: SOCIAL_LOAD_KEY } };
      const start = cursor[0] ?? 0;
      const allRows = [...durable.storedChunks.entries()]
        .sort(([left], [right]) => left - right)
        .map(([chunk_index, value]) => ({ chunk_index, receipt: value.receipt }));
      const receipts = allRows.filter(({ chunk_index }) => chunk_index >= start).slice(0, maxReceipts);
      const next = receipts.at(-1)?.chunk_index + 1;
      const page = {
        state: durable.state,
        next_chunk_index: durable.storedChunks.size,
        committed_chunk_count: durable.storedChunks.size,
        completed_chunk_count: durable.storedChunks.size,
        terminal_at_ns: [],
        expires_at_ns: [],
        receipts,
        next_receipt_cursor: next !== undefined && next < allRows.length ? [next] : [],
      };
      if (pendingAmbiguousAppend) {
        const { chunk_index, chunk, envelope } = pendingAmbiguousAppend;
        const allocated_vertex_ids = "Vertices" in chunk
          ? chunk.Vertices.map(() => id(durable.nextVertex++))
          : [];
        const receipt = {
          logical_operation_count: BigInt("Vertices" in chunk ? chunk.Vertices.length : chunk.Edges.length),
          logical_vertex_count: BigInt("Vertices" in chunk ? chunk.Vertices.length : 0),
          logical_edge_count: BigInt("Edges" in chunk ? chunk.Edges.length : 0),
          allocated_vertex_ids,
        };
        durable.storedChunks.set(chunk_index, { envelope, receipt });
        pendingAmbiguousAppend = null;
      }
      return { Ok: statusTransform(structuredClone(page)) };
    },
  };
  return actor;
}

const recoveryArtifact = {
  vertices: [
    { source_id: "v0", labels: ["User"], properties: { user_id: { Text: "v0" } } },
    { source_id: "v1", labels: ["Post"], properties: { demo_id: { Int64: 1 } } },
    { source_id: "v2", labels: ["Post"], properties: { demo_id: { Int64: 2 } } },
    { source_id: "v3", labels: ["Topic"], properties: { demo_id: { Int64: 3 } } },
  ],
  edges: [
    { source_id: "v0", target_id: "v1", directed: true, label: "POSTED", inline_value: null, properties: {} },
    { source_id: "v1", target_id: "v2", directed: true, label: "REPLIED", inline_value: null, properties: {} },
  ],
  embeddings: {},
};

async function seedPreparedDurable(value = recoveryArtifact, chunkSize = 2) {
  const payload = artifactPayload(value);
  const actor = fakeActor({ markerDigest: payload.digest });
  await assert.rejects(
    applySocialLoad({
      actor,
      artifact: value,
      artifactBytes: payload.bytes,
      freshBootstrap: true,
      chunkSize,
      statusAttempts: 2,
      statusIntervalMs: 0,
      ingestEmbeddings: async () => { throw new Error("hold Prepared"); },
    }),
    /hold Prepared/,
  );
  assert.equal(actor.durable.marker.state, "Prepared");
  assert.deepEqual(actor.durable.state, { Completed: null });
  return actor.durable;
}

test("fresh load preserves sentinel/index/marker/cleanup order and uses receipt IDs", async () => {
  const actor = fakeActor();
  const ingested = [];
  const result = await applySocialLoad({
    actor,
    artifact,
    artifactBytes,
    freshBootstrap: true,
    chunkSize: 1,
    statusAttempts: 2,
    statusIntervalMs: 0,
    ingestEmbeddings: async (ids, embeddings) => ingested.push([new Map(ids), embeddings]),
  });
  assert.equal(result.skipped, false);
  const compact = actor.calls.map(([method, detail]) => `${method}:${detail}`);
  assert.deepEqual(compact.slice(0, 16), [
    "gql_query:missing",
    "gql_mutate:social-bootstrap-user", "mutation_status:social-bootstrap-user",
    "gql_mutate:social-bootstrap-post", "mutation_status:social-bootstrap-post",
    "gql_mutate:social-bootstrap-feed", "mutation_status:social-bootstrap-feed",
    "gql_mutate:social-bootstrap-topic", "mutation_status:social-bootstrap-topic",
    "gql_mutate:social-bootstrap-community", "mutation_status:social-bootstrap-community",
    "index_vertex_property:User.user_id", "index_vertex_property:Post.demo_id",
    "index_vertex_property:Feed.demo_id", "index_vertex_property:Topic.demo_id",
    "index_vertex_property:User.demo_id",
  ]);
  const cleanup = actor.calls.find((call) => call[1] === "social-bootstrap-delete");
  assert.match(cleanup[2], /User\|Post\|Feed\|Topic\|Community/);
  assert.doesNotMatch(cleanup[2], /SeedLoad/);
  const details = actor.calls.map(([, detail]) => detail);
  const markerCreate = details.indexOf("social-seed-load-marker-prepared-v1");
  const markerCompleted = actor.calls.findIndex(
    ([method, detail], index) =>
      method === "mutation_status" && detail === "social-seed-load-marker-prepared-v1" && index > markerCreate,
  );
  const markerIndex = details.indexOf("SeedLoad.logical_seed_key");
  const markerVisible = actor.calls.findIndex(([method], index) => method === "gql_query" && index > markerIndex);
  const cleanupMutation = details.indexOf("social-bootstrap-delete");
  const cleanupCompleted = actor.calls.findIndex(
    ([method, detail], index) => method === "mutation_status" && detail === "social-bootstrap-delete" && index > cleanupMutation,
  );
  const start = actor.calls.findIndex(([method, detail]) => method === "bulk_load" && detail === "Start");
  assert.ok(markerCreate > 14);
  assert.equal(markerCompleted, markerCreate + 1);
  assert.equal(markerIndex, markerCompleted + 1);
  assert.equal(markerVisible, markerIndex + 1);
  assert.equal(actor.calls[markerVisible][2], 1);
  assert.deepEqual(
    actor.calls.slice(markerIndex + 1, cleanupMutation).filter(([method]) => method === "gql_query"),
    [actor.calls[markerVisible]],
  );
  assert.equal(cleanupMutation, markerVisible + 1);
  assert.ok(cleanupCompleted > cleanupMutation);
  assert.ok(start > cleanupCompleted);
  assert.equal(actor.calls.filter(([method, detail]) => method === "bulk_load" && detail === "Start").length, 1);
  assert.deepEqual(
    actor.calls.filter(([method]) => method === "index_vertex_property").map(([, detail]) => detail),
    ["User.user_id", "Post.demo_id", "Feed.demo_id", "Topic.demo_id", "User.demo_id", "SeedLoad.logical_seed_key"],
  );
  assert.deepEqual([...ingested[0][0].keys()], ["user", "post"]);
  assert.deepEqual(ingested[0][0].get("post"), id(2));
  const edgeAppend = actor.calls.find(([, , kind]) => kind === "Edges");
  assert.deepEqual(edgeAppend[4].chunk.Edges[0].source, id(1));
  assert.deepEqual(edgeAppend[4].chunk.Edges[0].target, id(2));
});

test("--fresh with an existing matching Complete marker skips before any bootstrap mutation", async () => {
  const actor = fakeActor({ marker: preparedMarker(digest, "Complete") });
  const result = await applySocialLoad({
    actor,
    artifact,
    artifactBytes,
    freshBootstrap: true,
    ingestEmbeddings: async () => { throw new Error("embeddings must be skipped"); },
  });
  assert.equal(result.skipped, true);
  assert.deepEqual(actor.calls.map(([method]) => method), ["gql_query"]);
  assert.equal(actor.calls.some(([method]) => method === "bulk_load_status"), false);
});

test("--fresh with a Prepared marker resumes from retained status without Start", async () => {
  const actor = fakeActor({ marker: preparedMarker(), statusExists: true });
  const result = await applySocialLoad({
    actor,
    artifact,
    artifactBytes,
    freshBootstrap: true,
    statusAttempts: 2,
    statusIntervalMs: 0,
    ingestEmbeddings: async () => {},
  });
  assert.equal(result.skipped, false);
  assert.equal(actor.calls.some(([method, detail]) => method === "bulk_load" && detail === "Start"), false);
});

test("--fresh with a Prepared marker and NotFound status fails closed without Start", async () => {
  const actor = fakeActor({ marker: preparedMarker() });
  await assert.rejects(
    applySocialLoad({ actor, artifact, artifactBytes, freshBootstrap: true }),
    /no retained bulk status/,
  );
  assert.equal(actor.calls.some(([method]) => method === "bulk_load"), false);
});

test("--fresh with a different marker digest fails closed before bootstrap mutation", async () => {
  const actor = fakeActor({ marker: preparedMarker("0".repeat(64)) });
  await assert.rejects(
    applySocialLoad({ actor, artifact, artifactBytes, freshBootstrap: true }),
    /artifact differs/,
  );
  assert.equal(actor.calls.some(([method]) => method !== "gql_query"), false);
});

test("marker absence requires the explicit fresh proof and never starts twice", async () => {
  const actor = fakeActor({ loseStartResponse: true });
  const result = await applySocialLoad({
    actor,
    artifact,
    artifactBytes,
    freshBootstrap: true,
    statusAttempts: 2,
    statusIntervalMs: 0,
    ingestEmbeddings: async () => {},
  });
  assert.equal(result.skipped, false);
  assert.equal(actor.calls.filter(([method, detail]) => method === "bulk_load" && detail === "Start").length, 1);
});

test("a retained marker-create idempotency receipt cannot bypass Prepared verification", async () => {
  const actor = fakeActor({ markerCreateNoop: true });
  await assert.rejects(
    applySocialLoad({
      actor,
      artifact,
      artifactBytes,
      freshBootstrap: true,
      statusAttempts: 1,
      statusIntervalMs: 0,
    }),
    /marker creation was not durably verified/,
  );
  assert.equal(actor.calls.some(([method, detail]) => method === "bulk_load" && detail === "Start"), false);
});

test("matching Complete marker skips bulk and embeddings even without retained status", async () => {
  const actor = fakeActor({ marker: {
    protocol_version: 1,
    logical_seed_key: SOCIAL_LOAD_KEY,
    artifact_sha256: digest,
    bulk_job_key: SOCIAL_LOAD_KEY,
    state: "Complete",
  } });
  let ingested = false;
  const result = await applySocialLoad({
    actor,
    artifact,
    artifactBytes,
    ingestEmbeddings: async () => { ingested = true; },
  });
  assert.equal(result.skipped, true);
  assert.equal(ingested, false);
  assert.equal(actor.calls.some(([method]) => method === "bulk_load"), false);
  assert.equal(actor.calls.some(([method]) => method === "bulk_load_status"), false);
});

test("Prepared marker with missing retained status fails closed without Start", async () => {
  const actor = fakeActor({ marker: {
    protocol_version: 1,
    logical_seed_key: SOCIAL_LOAD_KEY,
    artifact_sha256: digest,
    bulk_job_key: SOCIAL_LOAD_KEY,
    state: "Prepared",
  } });
  await assert.rejects(
    applySocialLoad({ actor, artifact, artifactBytes }),
    /no retained bulk status/,
  );
  assert.equal(actor.calls.some(([method]) => method === "bulk_load"), false);
});

test("Prepared status-only restart exact-replays from zero without issuing a second Start", async () => {
  const actor = fakeActor({ marker: {
    protocol_version: 1,
    logical_seed_key: SOCIAL_LOAD_KEY,
    artifact_sha256: digest,
    bulk_job_key: SOCIAL_LOAD_KEY,
    state: "Prepared",
  }, statusExists: true });
  const result = await applySocialLoad({
    actor,
    artifact,
    artifactBytes,
    statusAttempts: 2,
    statusIntervalMs: 0,
    ingestEmbeddings: async () => {},
  });
  assert.equal(result.skipped, false);
  assert.equal(actor.calls.some(([method, detail]) => method === "bulk_load" && detail === "Start"), false);
  assert.equal(actor.calls.some(([method, detail]) => method === "bulk_load" && detail === "Append:0"), true);
});

test("digest mismatch fails before any bulk call", async () => {
  const actor = fakeActor({ marker: {
    protocol_version: 1,
    logical_seed_key: SOCIAL_LOAD_KEY,
    artifact_sha256: "0".repeat(64),
    bulk_job_key: SOCIAL_LOAD_KEY,
    state: "Prepared",
  }, statusExists: true });
  await assert.rejects(applySocialLoad({ actor, artifact, artifactBytes }), /artifact differs/);
  assert.equal(actor.calls.some(([method]) => method === "bulk_load"), false);
});

test("lost Append response replays the exact same chunk before using its receipt", async () => {
  const actor = fakeActor({ loseAppendResponseAt: 0 });
  await applySocialLoad({
    actor,
    artifact,
    artifactBytes,
    freshBootstrap: true,
    chunkSize: 2,
    statusAttempts: 2,
    statusIntervalMs: 0,
    ingestEmbeddings: async () => {},
  });
  assert.equal(
    actor.calls.filter(([method, detail]) => method === "bulk_load" && detail === "Append:0").length,
    2,
  );
  assert.deepEqual(
    actor.calls.filter(([method, detail]) => method === "bulk_load" && detail === "Append:0").map((call) => call[3]),
    [2, 2],
  );
});

test("ambiguous timeout with Open status exact-replays the unchanged envelope instead of splitting", async () => {
  const actor = fakeActor({ ambiguousAppendRaceAt: 0 });
  await applySocialLoad({
    actor,
    artifact,
    artifactBytes,
    freshBootstrap: true,
    chunkSize: 2,
    statusAttempts: 2,
    statusIntervalMs: 0,
    ingestEmbeddings: async () => {},
  });
  const firstChunkCalls = actor.calls.filter(
    ([method, detail]) => method === "bulk_load" && detail === "Append:0",
  );
  assert.equal(firstChunkCalls.length, 2);
  assert.deepEqual(firstChunkCalls.map((call) => call[3]), [2, 2]);
  assert.deepEqual(firstChunkCalls[1][4], firstChunkCalls[0][4]);
  const firstAppend = actor.calls.indexOf(firstChunkCalls[0]);
  const firstStatus = actor.calls.findIndex(
    ([method, state], index) => method === "bulk_load_status" && state === "Open" && index > firstAppend,
  );
  assert.ok(firstStatus > firstAppend);
  assert.ok(actor.calls.indexOf(firstChunkCalls[1]) > firstStatus);
});

test("embedding failure leaves the marker Prepared", async () => {
  const actor = fakeActor();
  await assert.rejects(
    applySocialLoad({
      actor,
      artifact,
      artifactBytes,
      freshBootstrap: true,
      statusAttempts: 2,
      statusIntervalMs: 0,
      ingestEmbeddings: async () => { throw new Error("embedding unavailable"); },
    }),
    /embedding unavailable/,
  );
  assert.equal(
    actor.calls.some(([method, detail]) => method === "gql_mutate" && detail === "social-seed-load-marker-complete-v1"),
    false,
  );
});

test("pre-admission payload rejection splits from the configured 256 maximum in deterministic halves", async () => {
  const largeArtifact = {
    vertices: Array.from({ length: 257 }, (_, index) => ({
      source_id: `v${index}`,
      labels: ["Post"],
      properties: { demo_id: { Int64: index } },
    })),
    edges: [],
    embeddings: {},
  };
  const payload = artifactPayload(largeArtifact);
  const actor = fakeActor({ markerDigest: payload.digest, rejectAbove: 128 });
  await applySocialLoad({
    actor,
    artifact: largeArtifact,
    artifactBytes: payload.bytes,
    freshBootstrap: true,
    statusAttempts: 2,
    statusIntervalMs: 0,
    ingestEmbeddings: async () => {},
  });
  const appends = actor.calls.filter(([method, detail]) => method === "bulk_load" && detail.startsWith("Append:"));
  assert.deepEqual(appends.slice(0, 3).map((call) => [call[1], call[3]]), [
    ["Append:0", 256],
    ["Append:0", 128],
    ["Append:1", 128],
  ]);
  assert.deepEqual(
    [...actor.durable.storedChunks.values()].map(({ receipt }) => Number(receipt.logical_operation_count)),
    [128, 128, 1],
  );
});

test("pre-admission rejection at one operation fails without an empty split or Finalize", async () => {
  const one = { vertices: [artifact.vertices[0]], edges: [], embeddings: {} };
  const payload = artifactPayload(one);
  const actor = fakeActor({ markerDigest: payload.digest, rejectAbove: 0 });
  await assert.rejects(
    applySocialLoad({
      actor,
      artifact: one,
      artifactBytes: payload.bytes,
      freshBootstrap: true,
      statusAttempts: 2,
      statusIntervalMs: 0,
      ingestEmbeddings: async () => {},
    }),
    (error) => {
      assert.equal(error.message, "HTTP 413 Payload Too Large");
      return true;
    },
  );
  assert.deepEqual(
    actor.calls.filter(([method, detail]) => method === "bulk_load" && detail.startsWith("Append:")).map((call) => call[3]),
    [1],
  );
  assert.equal(actor.calls.some(([method, detail]) => method === "bulk_load" && detail === "Finalize"), false);
  assert.equal(actor.durable.marker.state, "Prepared");
});

for (const [name, option] of [
  ["Start", { loseStartResponse: true }],
  ["Finalize", { loseFinalizeResponse: true }],
]) {
  test(`lost ${name} response converges without issuing a second ${name}`, async () => {
    const actor = fakeActor(option);
    await applySocialLoad({
      actor,
      artifact,
      artifactBytes,
      freshBootstrap: true,
      statusAttempts: 2,
      statusIntervalMs: 0,
      ingestEmbeddings: async () => {},
    });
    assert.equal(
      actor.calls.filter(([method, detail]) => method === "bulk_load" && detail === name).length,
      1,
    );
  });
}

for (const [name, marker, copies = 1] of [
  ["wrong protocol", { ...preparedMarker(), protocol_version: 2 }],
  ["wrong logical key", { ...preparedMarker(), logical_seed_key: "other" }],
  ["malformed digest", { ...preparedMarker(), artifact_sha256: "not-a-digest" }],
  ["wrong bulk key", { ...preparedMarker(), bulk_job_key: "other" }],
  ["unknown state", { ...preparedMarker(), state: "Unknown" }],
  ["duplicate rows", preparedMarker(), 2],
]) {
  test(`malformed marker (${name}) fails closed before bulk`, async () => {
    const actor = fakeActor({ marker, markerCopies: copies, statusExists: true });
    await assert.rejects(applySocialLoad({ actor, artifact, artifactBytes }), /SeedLoad marker/);
    assert.equal(actor.calls.some(([method]) => method === "bulk_load"), false);
  });
}

for (const terminal of ["Failed", "Aborted"]) {
  test(`${terminal} bulk state refuses replay, embeddings, and a second Start`, async () => {
    const actor = fakeActor({ marker: preparedMarker(), statusExists: true });
    actor.durable.state = terminal === "Failed" ? { Failed: { reason: "broken" } } : { Aborted: null };
    let ingested = false;
    await assert.rejects(
      applySocialLoad({
        actor,
        artifact,
        artifactBytes,
        ingestEmbeddings: async () => { ingested = true; },
      }),
      /terminal but incomplete/,
    );
    assert.equal(ingested, false);
    assert.equal(actor.calls.some(([method]) => method === "bulk_load"), false);
  });
}

test("forged status counters fail before Append or receipt use", async () => {
  const actor = fakeActor({
    marker: preparedMarker(),
    statusExists: true,
    statusTransform(page) {
      page.next_chunk_index += 1;
      return page;
    },
  });
  await assert.rejects(applySocialLoad({ actor, artifact, artifactBytes }), /status counters are inconsistent/);
  assert.equal(actor.calls.some(([method]) => method === "bulk_load"), false);
});

test("forged prior operation_count reconstructs a candidate but exact replay rejects the changed boundary", async () => {
  const durable = await seedPreparedDurable();
  const actor = fakeActor({
    shared: durable,
    statusTransform(page) {
      if (page.receipts[0]?.chunk_index === 0) page.receipts[0].receipt.logical_operation_count = 1n;
      if (page.receipts[1]?.chunk_index === 1) page.receipts[1].receipt.logical_operation_count = 3n;
      return page;
    },
  });
  const payload = artifactPayload(recoveryArtifact);
  await assert.rejects(
    applySocialLoad({ actor, artifact: recoveryArtifact, artifactBytes: payload.bytes, ingestEmbeddings: async () => {} }),
    /exact replay Append\(0\).*Conflict/,
  );
  assert.equal(actor.calls.some(([method, detail]) => method === "gql_mutate" && detail === "social-seed-load-marker-complete-v1"), false);
});

const recoveryMutations = [
  ["mutated vertex", (value) => { value.vertices[0].properties.user_id = { Text: "changed" }; }],
  ["reordered vertices", (value) => { value.vertices = [value.vertices[1], value.vertices[0], ...value.vertices.slice(2)]; }],
  ["truncated vertices", (value) => { value.vertices = value.vertices.slice(0, 3); }],
  ["extended vertices", (value) => { value.vertices.push({ source_id: "v4", labels: ["Post"], properties: {} }); }],
  ["mutated edge", (value) => { value.edges[0].label = "CHANGED"; }],
  ["reordered edges", (value) => { value.edges.reverse(); }],
  ["truncated edges", (value) => { value.edges = value.edges.slice(0, 1); }],
  ["extended edges", (value) => { value.edges.push({ source_id: "v2", target_id: "v3", directed: true, label: "TAGGED", inline_value: null, properties: {} }); }],
];

for (const [name, mutate] of recoveryMutations) {
  test(`${name} artifact cannot use prior receipts or complete the marker`, async () => {
    const durable = await seedPreparedDurable();
    const changed = structuredClone(recoveryArtifact);
    mutate(changed);
    const payload = artifactPayload(changed);
    durable.marker.artifact_sha256 = payload.digest;
    const actor = fakeActor({ shared: durable });
    let ingested = false;
    await assert.rejects(
      applySocialLoad({
        actor,
        artifact: changed,
        artifactBytes: payload.bytes,
        ingestEmbeddings: async () => { ingested = true; },
      }),
    );
    assert.equal(ingested, false);
    assert.equal(durable.marker.state, "Prepared");
    assert.equal(actor.calls.some(([method, detail]) => method === "bulk_load" && detail === "Start"), false);
  });
}

test("restart reads ordered receipt operation_count pages and exact-replays all boundaries before IDs", async () => {
  const many = {
    vertices: Array.from({ length: 65 }, (_, index) => ({
      source_id: `p${index}`,
      labels: ["Post"],
      properties: { demo_id: { Int64: index } },
    })),
    edges: [],
    embeddings: {},
  };
  const durable = await seedPreparedDurable(many, 1);
  const actor = fakeActor({ shared: durable });
  const payload = artifactPayload(many);
  const result = await applySocialLoad({
    actor,
    artifact: many,
    artifactBytes: payload.bytes,
    chunkSize: 1,
    statusAttempts: 2,
    statusIntervalMs: 0,
    ingestEmbeddings: async () => {},
  });
  assert.equal(result.sourceIdMap.size, 65);
  assert.equal(
    actor.calls.some(([method, , cursor]) => method === "bulk_load_status" && cursor?.[0] === 64),
    true,
  );
  assert.equal(actor.calls.filter(([method, detail]) => method === "bulk_load" && detail.startsWith("Append:")).length, 65);
});
