import { readFile } from "node:fs/promises";
import { encodeCanonicalGqlValue, bytesToHex } from "../src/canonical-value.ts";
import { makeAtomicInsertRequest } from "../src/atomic.ts";

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

const atomicInsertRequest = makeAtomicInsertRequest({
  client_mutation_key: "sdk-conformance",
  logical_graph_name: "tenant.main",
  operations: [
    {
      vertex: {
        vertex_labels: ["User", "Person"],
        initial_properties: {
          zeta: { Int64: 7n },
          alpha: { Text: "first" },
        },
      },
    },
    {
      edge: {
        source: { new_vertex_ordinal: 0 },
        target: { existing: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]) },
        directed: true,
        edge_label_name: "follows",
        inline_property: { Text: "inline" },
        initial_edge_properties: {
          zeta: { Int64: 7n },
          alpha: { Text: "first" },
        },
      },
    },
  ],
});
const [vertexOperation, edgeOperation] = atomicInsertRequest.V1.operations;
if (
  !vertexOperation ||
  !("Vertex" in vertexOperation) ||
  vertexOperation.Vertex.vertex_labels.join(",") !== "Person,User" ||
  vertexOperation.Vertex.initial_properties.map(({ property_name }) => property_name).join(",") !==
    "alpha,zeta"
) {
  throw new Error("atomic insert request does not canonicalize vertex catalog order");
}
if (
  !edgeOperation ||
  !("Edge" in edgeOperation) ||
  edgeOperation.Edge.source.NewVertexOrdinal !== 0 ||
  edgeOperation.Edge.edge_label_name.length !== 1 ||
  edgeOperation.Edge.inline_property.length !== 1 ||
  edgeOperation.Edge.initial_edge_properties.map(({ property_name }) => property_name).join(",") !==
    "alpha,zeta"
) {
  throw new Error("atomic insert request does not preserve the canonical Candid edge shape");
}

function assertBuilderRejects(input, name) {
  let rejected = false;
  try {
    makeAtomicInsertRequest(input);
  } catch {
    rejected = true;
  }
  if (!rejected) throw new Error(`${name}: invalid atomic insert request was accepted`);
}

assertBuilderRejects(
  {
    client_mutation_key: "invalid-endpoint",
    logical_graph_name: "tenant.main",
    operations: [
      {
        edge: {
          source: { existing: Uint8Array.of(1) },
          target: { existing: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]) },
          directed: true,
        },
      },
    ],
  },
  "invalid endpoint width",
);
assertBuilderRejects(
  {
    client_mutation_key: "",
    logical_graph_name: "tenant.main",
    operations: [{ vertex: {} }],
  },
  "empty client key",
);
assertBuilderRejects(
  {
    client_mutation_key: "empty-graph",
    logical_graph_name: "",
    operations: [{ vertex: {} }],
  },
  "empty graph name",
);
assertBuilderRejects(
  {
    client_mutation_key: "empty-edge-label",
    logical_graph_name: "tenant.main",
    operations: [
      {
        edge: {
          source: { existing: Uint8Array.from([1, 1, 1, 1, 1, 1, 1, 1]) },
          target: { existing: Uint8Array.from([2, 2, 2, 2, 2, 2, 2, 2]) },
          directed: true,
          edge_label_name: "",
        },
      },
    ],
  },
  "empty edge label",
);
assertBuilderRejects(
  {
    client_mutation_key: "unknown-vertex",
    logical_graph_name: "tenant.main",
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
  "unknown new vertex ordinal",
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-existing-endpoint",
    logical_graph_name: "tenant.main",
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
  "existing endpoint width",
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-label",
    logical_graph_name: "tenant.main",
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
  "empty edge label in mixed atomic insert",
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-label",
    logical_graph_name: "tenant.main",
    operations: [{ vertex: { vertex_labels: [""] } }],
  },
  "empty vertex label",
);
assertBuilderRejects(
  {
    client_mutation_key: "invalid-property",
    logical_graph_name: "tenant.main",
    operations: [{ vertex: { initial_properties: { "": { Text: "invalid" } } } }],
  },
  "empty property name",
);

console.log(
  `sdk/js conformance: ${fixture.vectors.length} value vectors and unified atomic insert request builder passed`,
);
