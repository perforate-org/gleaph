import { readFile } from "node:fs/promises";
import { encodeCanonicalGqlValue, bytesToHex } from "../src/canonical-value.ts";
import { makeOrderedEdgeBatchPublicRequest } from "../src/ordered-edge-batch.ts";
import { makeOrderedVertexBatchPublicRequest } from "../src/ordered-vertex-batch.ts";
import { makeOrderedMixedBatchPublicRequest } from "../src/ordered-mixed-batch.ts";

const fixturePath = new URL(
  "../../../../crates/graph-kernel/conformance/gql_value_vectors.json",
  import.meta.url,
);
const fixture = JSON.parse(await readFile(fixturePath, "utf8"));

function normalize(value) {
  if (Array.isArray(value)) return value.map(normalize);
  if (value === null || typeof value !== "object") return value;
  const entries = Object.entries(value).map(([key, nested]) => {
    if (
      ["Int64", "Uint64", "Int128", "Uint128", "Int256", "Uint256", "Time", "LocalTime"].includes(
        key,
      )
    ) {
      return [key, BigInt(nested)];
    }
    if (["DateTime", "LocalDateTime", "ZonedDateTime", "ZonedTime", "Duration"].includes(key)) {
      return [key, normalizeTemporal(key, nested)];
    }
    if (key === "Bytes")
      return [key, Uint8Array.from(nested.match(/../g)?.map((part) => parseInt(part, 16)) ?? [])];
    return [key, normalize(nested)];
  });
  return Object.fromEntries(entries);
}

function normalizeTemporal(kind, value) {
  const result = { ...value };
  for (const key of ["seconds", "nanos", "offset_seconds", "months"]) {
    if (key in result && ["seconds", "nanos"].includes(key)) result[key] = BigInt(result[key]);
  }
  if (kind === "Duration") result.months = Number(value.months);
  if ("nanos" in result && kind !== "Duration" && kind !== "ZonedTime")
    result.nanos = Number(value.nanos);
  if ("offset_seconds" in result) result.offset_seconds = Number(value.offset_seconds);
  return result;
}

for (const vector of fixture.vectors) {
  const actual = bytesToHex(encodeCanonicalGqlValue(normalize(vector.value)));
  if (actual !== vector.canonical_bytes_hex) {
    throw new Error(`${vector.name}: expected ${vector.canonical_bytes_hex}, got ${actual}`);
  }
}

for (const vector of fixture.invalid_sdk_values) {
  const value = {
    Float64NaN: { Float64: Number.NaN },
    Float64Infinity: { Float64: Number.POSITIVE_INFINITY },
    UnsafeInt64Number: { Int64: Number.MAX_SAFE_INTEGER + 1 },
  }[vector.kind];
  let rejected = false;
  try {
    encodeCanonicalGqlValue(value);
  } catch {
    rejected = true;
  }
  if (!rejected) throw new Error(`${vector.name}: invalid SDK value was accepted`);
}

const orderedRequest = makeOrderedEdgeBatchPublicRequest({
  client_mutation_key: "sdk-conformance",
  logical_graph_name: "tenant.main",
  items: [
    {
      source: Uint8Array.from([1, 1, 1, 1, 1, 1, 1, 1]),
      target: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]),
      directed: true,
      edge_label_name: "follows",
      inline_property: { Text: "inline" },
      initial_edge_properties: {
        zeta: { Int64: 7n },
        alpha: { Text: "first" },
      },
    },
  ],
});
const [item] = orderedRequest.V1.items;
if (
  !item ||
  item.initial_edge_properties.map(({ property_name }) => property_name).join(",") !== "alpha,zeta"
) {
  throw new Error("ordered public request does not canonicalize property order");
}
if (item.edge_label_name.length !== 1 || item.inline_property.length !== 1) {
  throw new Error("ordered public request option shape is not Candid-compatible");
}

const orderedVertexRequest = makeOrderedVertexBatchPublicRequest({
  client_mutation_key: "sdk-vertex-conformance",
  logical_graph_name: "tenant.main",
  target_shard_id: 7,
  items: [
    {
      vertex_labels: ["User", "Person"],
      initial_properties: {
        zeta: { Int64: 7n },
        alpha: { Text: "first" },
      },
    },
  ],
});
const [vertexItem] = orderedVertexRequest.V1.items;
if (
  !vertexItem ||
  vertexItem.vertex_labels.join(",") !== "Person,User" ||
  vertexItem.initial_properties.map(({ property_name }) => property_name).join(",") !== "alpha,zeta"
) {
  throw new Error("ordered vertex public request does not canonicalize catalog order");
}

const orderedMixedRequest = makeOrderedMixedBatchPublicRequest({
  client_mutation_key: "sdk-mixed-conformance",
  logical_graph_name: "tenant.main",
  target_shard_id: 7,
  operations: [
    { vertex: { vertex_labels: ["User", "Person"], initial_properties: { zeta: { Int64: 7n } } } },
    {
      edge: {
        source: { new_vertex_ordinal: 0 },
        target: { existing: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]) },
        directed: true,
        edge_label_name: "follows",
        inline_property: { Text: "inline" },
        initial_edge_properties: { alpha: { Text: "first" } },
      },
    },
  ],
});
const [mixedVertex, mixedEdge] = orderedMixedRequest.V1.operations;
if (
  !mixedVertex ||
  !("Vertex" in mixedVertex) ||
  mixedVertex.Vertex.vertex_labels.join(",") !== "Person,User" ||
  !mixedEdge ||
  !("Edge" in mixedEdge) ||
  mixedEdge.Edge.source.NewVertexOrdinal !== 0 ||
  mixedEdge.Edge.edge_label_name.length !== 1 ||
  mixedEdge.Edge.inline_property.length !== 1
) {
  throw new Error("ordered mixed public request does not preserve the canonical Candid shape");
}

function assertBuilderRejects(input, name, builder = makeOrderedEdgeBatchPublicRequest) {
  let rejected = false;
  try {
    builder(input);
  } catch {
    rejected = true;
  }
  if (!rejected) throw new Error(`${name}: invalid ordered request was accepted`);
}

assertBuilderRejects(
  {
    client_mutation_key: "invalid-endpoint",
    logical_graph_name: "tenant.main",
    items: [
      {
        source: Uint8Array.of(1),
        target: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]),
        directed: true,
      },
    ],
  },
  "invalid endpoint width",
);
assertBuilderRejects(
  {
    client_mutation_key: "missing-edge",
    logical_graph_name: "tenant.main",
    target_shard_id: 0,
    operations: [{ vertex: { vertex_labels: ["Person"] } }],
  },
  "mixed request without edge",
  makeOrderedMixedBatchPublicRequest,
);
assertBuilderRejects(
  {
    client_mutation_key: "missing-vertex",
    logical_graph_name: "tenant.main",
    target_shard_id: 0,
    operations: [
      {
        edge: {
          source: { new_vertex_ordinal: 0 },
          target: { existing: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]) },
          directed: true,
        },
      },
    ],
  },
  "mixed request without vertex",
  makeOrderedMixedBatchPublicRequest,
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-existing-endpoint",
    logical_graph_name: "tenant.main",
    target_shard_id: 0,
    operations: [
      { vertex: {} },
      {
        edge: {
          source: { new_vertex_ordinal: 0 },
          target: { existing: Uint8Array.of(2) },
          directed: true,
        },
      },
    ],
  },
  "mixed existing endpoint width",
  makeOrderedMixedBatchPublicRequest,
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-label",
    logical_graph_name: "tenant.main",
    target_shard_id: 0,
    operations: [
      { vertex: {} },
      {
        edge: {
          source: { new_vertex_ordinal: 0 },
          target: { existing: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]) },
          directed: true,
          edge_label_name: "",
        },
      },
    ],
  },
  "mixed empty edge label",
  makeOrderedMixedBatchPublicRequest,
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-property",
    logical_graph_name: "tenant.main",
    target_shard_id: 0,
    operations: [
      { vertex: { initial_properties: { "": { Text: "invalid" } } } },
      {
        edge: {
          source: { new_vertex_ordinal: 0 },
          target: { existing: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]) },
          directed: true,
        },
      },
    ],
  },
  "mixed empty property name",
  makeOrderedMixedBatchPublicRequest,
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-forward-ordinal",
    logical_graph_name: "tenant.main",
    target_shard_id: 0,
    operations: [
      { vertex: {} },
      {
        edge: {
          source: { new_vertex_ordinal: 1 },
          target: { existing: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]) },
          directed: true,
        },
      },
    ],
  },
  "mixed forward vertex ordinal",
  makeOrderedMixedBatchPublicRequest,
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-ordinal",
    logical_graph_name: "tenant.main",
    target_shard_id: 0,
    operations: [
      { vertex: {} },
      {
        edge: {
          source: { new_vertex_ordinal: -1 },
          target: { existing: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]) },
          directed: true,
        },
      },
    ],
  },
  "negative mixed vertex ordinal",
  makeOrderedMixedBatchPublicRequest,
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-property",
    logical_graph_name: "tenant.main",
    items: [
      {
        source: Uint8Array.from([1, 1, 1, 1, 1, 1, 1, 1]),
        target: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]),
        directed: true,
        initial_edge_properties: { "": { Text: "invalid" } },
      },
    ],
  },
  "empty property name",
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-shard",
    logical_graph_name: "tenant.main",
    target_shard_id: -1,
    items: [{ vertex_labels: ["Person"] }],
  },
  "negative target shard",
  makeOrderedVertexBatchPublicRequest,
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-label",
    logical_graph_name: "tenant.main",
    target_shard_id: 0,
    items: [{ vertex_labels: [""] }],
  },
  "empty vertex label",
  makeOrderedVertexBatchPublicRequest,
);

console.log(
  `sdk/js conformance: ${fixture.vectors.length} value vectors and ordered edge/vertex/mixed request builders passed`,
);
