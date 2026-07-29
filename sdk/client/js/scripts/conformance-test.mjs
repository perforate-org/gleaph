import { readFile } from "node:fs/promises";
import { encodeCanonicalGqlValue, bytesToHex } from "../src/canonical-value.ts";
import { makeOrderedEdgeBatchPublicRequest } from "../src/ordered-edge-batch.ts";

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

function assertBuilderRejects(input, name) {
  let rejected = false;
  try {
    makeOrderedEdgeBatchPublicRequest(input);
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

console.log(
  `sdk/js conformance: ${fixture.vectors.length} value vectors and ordered request builder passed`,
);
