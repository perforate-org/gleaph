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

// ---------------------------------------------------------------------------
// New value-layer conformance: byte-based integers/decimals, Temporal, floats.
// ---------------------------------------------------------------------------

import {
  decimalFromWireBytes,
  decimalToWireBytes,
  f16BitsToNumber,
  f16NumberToBits,
  fromApiValue,
  toApiValue,
} from "../src/values.ts";
import { GqlFloat128, GqlFloat256 } from "../src/float-values.ts";
import { Temporal } from "@js-temporal/polyfill";
import GqlDecimal from "decimal.js";

// Decimal: rust_decimal packed wire (flags first, scale << 16, sign bit 31).
{
  const cases = [
    ["123.45", "00000200393000000000000000000000"],
    ["0", "00000000000000000000000000000000"],
    ["-0.001", "00000380010000000000000000000000"],
    ["7922816251426433759354395.0335", "00000400ffffffffffffffffffffffff"],
  ];
  for (const [text, hex] of cases) {
    const wire = decimalToWireBytes(new GqlDecimal(text));
    const actual = bytesToHex(wire);
    if (actual !== hex) throw new Error(`decimal ${text}: expected ${hex}, got ${actual}`);
    const back = decimalFromWireBytes(wire);
    if (!back.eq(new GqlDecimal(text))) throw new Error(`decimal ${text} did not round-trip`);
  }
}

// Integers: Int256/Uint256 as 32 little-endian bytes.
{
  const wire = toApiValue(-2n, "Int256");
  if (!("Int256" in wire)) throw new Error("Int256 hint did not produce Int256");
  if (fromApiValue(wire) !== -2n) throw new Error("Int256 round-trip failed");
  const unsigned = toApiValue(2n ** 255n + 1n, "Uint256");
  if (fromApiValue(unsigned) !== 2n ** 255n + 1n) throw new Error("Uint256 round-trip failed");
}

// Temporal round trips.
{
  const date = new Temporal.PlainDate(2024, 1, 15);
  const dateWire = toApiValue(date);
  if (!("Date" in dateWire) || dateWire.Date !== 19737) throw new Error("Date encode failed");
  const dateBack = fromApiValue(dateWire);
  if (!(dateBack instanceof Temporal.PlainDate) || dateBack.year !== 2024)
    throw new Error("Date decode failed");

  const instant = Temporal.Instant.from("2023-11-14T22:13:20.123456789Z");
  const instantWire = toApiValue(instant);
  const instantBack = fromApiValue(instantWire);
  if (
    !(instantBack instanceof Temporal.Instant) ||
    instantBack.epochNanoseconds !== instant.epochNanoseconds
  )
    throw new Error("DateTime round-trip failed");

  const local = new Temporal.PlainDateTime(2024, 1, 15, 10, 30, 0, 0, 0, 0);
  const localWire = toApiValue(local);
  const localBack = fromApiValue(localWire);
  if (
    !(localBack instanceof Temporal.PlainDateTime) ||
    localBack.year !== 2024 ||
    localBack.hour !== 10
  )
    throw new Error("LocalDateTime round-trip failed");

  const zoned = Temporal.ZonedDateTime.from({
    timeZone: "+09:00",
    year: 2024,
    month: 1,
    day: 15,
    hour: 10,
    minute: 30,
  });
  const zonedWire = toApiValue(zoned);
  const zonedBack = fromApiValue(zonedWire);
  if (
    !(zonedBack instanceof Temporal.ZonedDateTime) ||
    zonedBack.epochNanoseconds !== zoned.epochNanoseconds
  )
    throw new Error("ZonedDateTime round-trip failed");

  const duration = new Temporal.Duration(1, 2, 0, 3, 4, 5, 6, 7, 8, 9);
  const durationWire = toApiValue(duration);
  const durationBack = fromApiValue(durationWire);
  // Duration has no `equals` in the final Temporal spec; compare fields.
  const durationFields = [
    "years",
    "months",
    "days",
    "hours",
    "minutes",
    "seconds",
    "milliseconds",
    "microseconds",
    "nanoseconds",
  ];
  if (
    !(durationBack instanceof Temporal.Duration) ||
    durationBack.sign !== duration.sign ||
    durationFields.some((field) => durationBack[field] !== duration[field])
  )
    throw new Error("Duration round-trip failed");

  const time = new Temporal.PlainTime(23, 59, 59, 999, 999, 999);
  const timeWire = toApiValue(time);
  if (!("Time" in timeWire)) throw new Error("Time encode failed");
  const timeBack = fromApiValue(timeWire);
  if (!(timeBack instanceof Temporal.PlainTime) || !timeBack.equals(time))
    throw new Error("Time round-trip failed");

  const zonedTime = { nanos: 37800000000000n, offset_seconds: 9 * 3600 };
  const zonedTimeWire = toApiValue(zonedTime);
  const zonedTimeBack = fromApiValue(zonedTimeWire);
  if (zonedTimeBack.nanos !== zonedTime.nanos || zonedTimeBack.offset_seconds !== 32400)
    throw new Error("ZonedTime round-trip failed");
}

// Float16 pure-JS bits.
{
  const cases = [
    [0x3c00, 1],
    [0x8000, -0],
    [0x7c00, Infinity],
    [0xfc00, -Infinity],
    [0x7e00, NaN],
    [0x0001, 5.960464477539063e-8],
    [0x3555, 0.333251953125],
  ];
  for (const [bits, expected] of cases) {
    const number = f16BitsToNumber(bits);
    if (Object.is(number, expected)) continue;
    if (Number.isNaN(expected) && Number.isNaN(number)) continue;
    throw new Error(`f16 0x${bits.toString(16)}: expected ${expected}, got ${number}`);
  }
  if (f16NumberToBits(1) !== 0x3c00) throw new Error("f16 encode 1 failed");
  if (f16NumberToBits(-0) !== 0x8000) throw new Error("f16 encode -0 failed");
  if (f16NumberToBits(Infinity) !== 0x7c00) throw new Error("f16 encode inf failed");
  if (f16NumberToBits(0.333251953125) !== 0x3555) throw new Error("f16 encode 0.33325 failed");
}

function bytesEqual(a, b) {
  if (a.byteLength !== b.byteLength) return false;
  for (let index = 0; index < a.byteLength; index += 1) {
    if (a[index] !== b[index]) return false;
  }
  return true;
}

// Float128/Float256 pure-BigInt conversions (no wasm, no async init).
{
  const f256 = GqlFloat256.fromString("1.5");
  if (f256.toString() !== "1.5") throw new Error("float256 to_string failed");
  if (f256.toNumber() !== 1.5) throw new Error("float256 to_number failed");
  const fromNumber = GqlFloat256.fromNumber(2.25);
  if (fromNumber.toString() !== "2.25") throw new Error("float256 from_number failed");

  const f128 = GqlFloat128.fromString("2.5");
  if (f128.toString() !== "2.5") throw new Error("float128 to_string failed");
  const f128FromNumber = GqlFloat128.fromNumber(1.25);
  if (f128FromNumber.toString() !== "1.25") throw new Error("float128 from_number failed");

  // Special values.
  if (GqlFloat128.fromString("Infinity").toString() !== "Infinity") throw new Error("inf failed");
  if (GqlFloat128.fromString("-Infinity").toNumber() !== -Infinity) throw new Error("-inf failed");
  if (GqlFloat128.fromString("NaN").toString() !== "NaN") throw new Error("nan failed");
  if (GqlFloat128.fromNumber(-0).toString() !== "-0") throw new Error("-0 failed");
  if (GqlFloat128.fromNumber(NaN).toString() !== "NaN") throw new Error("fromNumber nan failed");

  // Exact f64 widening round-trips.
  for (const value of [0, -0, 1, -1.5, 3.141592653589793, Number.MAX_VALUE, 5e-324]) {
    if (!Object.is(GqlFloat128.fromNumber(value).toNumber(), value))
      throw new Error(`f128 f64 round-trip ${value}`);
    if (!Object.is(GqlFloat256.fromNumber(value).toNumber(), value))
      throw new Error(`f256 f64 round-trip ${value}`);
  }

  // Decimal round-trips through the shortest form.
  for (const text of [
    "0",
    "-0",
    "1",
    "-1.5",
    "0.1",
    "1e100",
    "-1e-100",
    "79228162514264337593543950335",
  ]) {
    const f = GqlFloat128.fromString(text);
    const back = GqlFloat128.fromString(f.toString());
    if (!bytesEqual(f.toBytes(), back.toBytes())) throw new Error(`f128 round-trip ${text}`);
  }
  for (const text of ["0.1", "1e-78000", "1e78000", "1.7976931348623157e308"]) {
    const f = GqlFloat256.fromString(text);
    const back = GqlFloat256.fromString(f.toString());
    if (!bytesEqual(f.toBytes(), back.toBytes())) throw new Error(`f256 round-trip ${text}`);
  }

  // Subnormal f128: the old wasm widen/narrow path dropped these to zero.
  const subnormal = new Uint8Array(16);
  subnormal[0] = 1; // 2^-16494
  const subText = GqlFloat128.fromBytes(subnormal).toString();
  if (subText.length > 30 || !subText.includes("e-49"))
    throw new Error(`subnormal shortest too long: ${subText}`);
  if (!bytesEqual(GqlFloat128.fromString(subText).toBytes(), subnormal))
    throw new Error("subnormal f128 round-trip failed");

  // f128 overflow/underflow bounds (max ~1.19e4932, min subnormal ~6.5e-4966).
  if (GqlFloat128.fromString("1e4933").toNumber() !== Infinity) throw new Error("f128 overflow");
  if (!bytesEqual(GqlFloat128.fromString("1e-4966").toBytes(), new Uint8Array(16)))
    throw new Error("f128 underflow");
  if (!bytesEqual(GqlFloat128.fromString("6e-4966").toBytes(), subnormal))
    throw new Error("f128 min subnormal rounding");

  // toApiValue hint / inference for float bindings.
  const wired = toApiValue(GqlFloat128.fromString("1.5"), "Float128");
  if (!("Float128" in wired) || wired.Float128.byteLength !== 16)
    throw new Error("Float128 toApiValue failed");
  const inferred = toApiValue(GqlFloat128.fromString("1.5"));
  if (!("Float128" in inferred)) throw new Error("Float128 inference failed");
}

// toApiValue inference for the dynamic path.
{
  const inferred = toApiValue(7n);
  if (!("Int64" in inferred) || inferred.Int64 !== 7n) throw new Error("bigint inference failed");
  const decimal = toApiValue(new GqlDecimal("1.5"));
  if (!("Decimal" in decimal)) throw new Error("decimal inference failed");
  const instant = toApiValue(Temporal.Instant.from("2024-01-01T00:00:00Z"));
  if (!("DateTime" in instant)) throw new Error("instant inference failed");
}

console.log("value-layer conformance OK");

// Schema-driven row decoder (rows.ts).
{
  const { decodeRow, decodeRows } = await import("../src/rows.ts");
  const columns = [
    { name: "post_id", semantic_type: "Int64", nullable: false },
    { name: "parent_post_id", semantic_type: "Int64", nullable: true },
    { name: "body", semantic_type: "Text", nullable: false },
    { name: "created_at", semantic_type: "DateTime", nullable: false },
    { name: "edge_id", semantic_type: "Bytes", nullable: false },
    { name: "distance", semantic_type: "Float64", nullable: false },
  ];
  const row = {
    post_id: { Int64: 7 },
    parent_post_id: { Null: null },
    body: { Text: "hello" },
    created_at: { DateTime: { seconds: 1783927200n, nanos: 0 } },
    edge_id: { Bytes: Uint8Array.from([0xab, 0xcd]) },
    distance: { Float64: 0.25 },
    extra: { Bool: true },
  };

  const decoded = decodeRow(row, columns);
  if (decoded.post_id !== 7n) throw new Error("Int64 number did not normalize to bigint");
  if (decoded.parent_post_id !== null) throw new Error("nullable Null did not decode to null");
  if (decoded.body !== "hello") throw new Error("Text decode failed");
  if (
    !(decoded.created_at instanceof Temporal.Instant) ||
    decoded.created_at.epochNanoseconds / 1_000_000_000n !== 1783927200n
  )
    throw new Error("DateTime did not decode to Temporal.Instant");
  if (decoded.edge_id.constructor !== Uint8Array || bytesToHex(decoded.edge_id) !== "abcd")
    throw new Error("Bytes decode failed");
  if (decoded.distance !== 0.25) throw new Error("Float64 decode failed");
  if ("extra" in decoded) throw new Error("undeclared column leaked into decoded row");
  if (decodeRows([row], columns).length !== 1) throw new Error("decodeRows failed");

  const nonNullNull = decodeRow({ ...row, parent_post_id: { Int64: 9n } }, columns);
  if (nonNullNull.parent_post_id !== 9n) throw new Error("nullable Int64 decode failed");

  let threw = false;
  try {
    decodeRow({ ...row, post_id: { Text: "oops" } }, columns);
  } catch (err) {
    threw = err instanceof Error && err.message.includes("declared Int64 but received Text");
  }
  if (!threw) throw new Error("tag mismatch did not throw");
  threw = false;
  try {
    decodeRow({ ...row, post_id: { Null: null } }, columns);
  } catch (err) {
    threw = err instanceof Error && err.message.includes("declared non-null");
  }
  if (!threw) throw new Error("non-null Null did not throw");
  threw = false;
  try {
    decodeRow({ ...row, body: undefined }, columns);
  } catch (err) {
    threw = err instanceof Error && err.message.includes('missing column "body"');
  }
  if (!threw) throw new Error("missing column did not throw");
}

// List and Record columns decode recursively against their element/field schema.
{
  const { decodeRow } = await import("../src/rows.ts");
  const columns = [
    {
      name: "tags",
      semantic_type: { List: { element: "Int64" } },
      nullable: false,
    },
    {
      name: "meta",
      semantic_type: { List: { element: { Record: { fields: [] } } } },
      nullable: false,
    },
    {
      name: "profile",
      semantic_type: {
        Record: {
          fields: [
            { name: "handle", semantic_type: "Text", nullable: false },
            { name: "age", semantic_type: "Int64", nullable: true },
          ],
        },
      },
      nullable: false,
    },
  ];
  const row = {
    tags: { List: [{ Int64: 1 }, { Int64: 2n }] },
    meta: { List: [{ Record: {} }] },
    profile: { Record: { handle: { Text: "alice" }, age: { Null: null } } },
  };

  const decoded = decodeRow(row, columns);
  if (decoded.tags.length !== 2 || decoded.tags[0] !== 1n || decoded.tags[1] !== 2n)
    throw new Error("List<Int64> decode failed (elements must normalize to bigint)");
  if (decoded.meta.length !== 1 || typeof decoded.meta[0] !== "object")
    throw new Error("nested List<Record> decode failed");
  if (decoded.profile.handle !== "alice" || decoded.profile.age !== null)
    throw new Error("Record field decode failed");

  let threw = false;
  try {
    decodeRow({ ...row, tags: { Text: "oops" } }, columns);
  } catch (err) {
    threw = err instanceof Error && err.message.includes("declared List but received Text");
  }
  if (!threw) throw new Error("List tag mismatch did not throw");
}

// Manifest boundary normalization (manifest.ts): Candid variant/opt forms -> typed API.
{
  const { normalizeManifest, normalizeSemanticType } = await import("../src/manifest.ts");
  if (normalizeSemanticType({ Text: null }) !== "Text")
    throw new Error("scalar variant did not normalize");
  const listType = normalizeSemanticType({
    List: { element: { Int64: null } },
  });
  if (!("List" in listType) || listType.List.element !== "Int64")
    throw new Error("List element did not normalize");
  const recordType = normalizeSemanticType({
    Record: { fields: [{ name: "a", type: { Bytes: null }, nullable: true }] },
  });
  if (!("Record" in recordType) || recordType.Record.fields[0].semantic_type !== "Bytes")
    throw new Error("Record field did not normalize");
  if (normalizeSemanticType("Text") !== "Text") throw new Error("flat string passthrough failed");

  const manifest = normalizeManifest({
    manifest_version: 1,
    graph: { id: "1", name: [] },
    operations: [
      {
        name: "find_users",
        description: ["docs"],
        kind: { Query: null },
        parameters: [
          {
            name: "term",
            description: [],
            required: true,
            nullable: false,
            type: { Text: null },
          },
        ],
        result: {
          columns: [
            { name: "user_name", type: { Text: null }, nullable: false },
            {
              name: "tags",
              type: { List: { element: { Int64: null } } },
              nullable: true,
            },
          ],
        },
        supports_consistency: true,
        supports_idempotency: false,
        allowed_sorts: [{ key: "created_at", label: ["Created"] }],
      },
    ],
  });
  const op = manifest.operations[0];
  if (op.kind !== "Query") throw new Error("kind variant did not normalize");
  if (op.description !== "docs") throw new Error("operation description opt did not normalize");
  if (op.parameters[0].semantic_type !== "Text")
    throw new Error("parameter type did not normalize");
  if (op.parameters[0].description !== null)
    throw new Error("parameter description opt did not normalize to null");
  if (op.result.columns[1].semantic_type.List.element !== "Int64")
    throw new Error("column List type did not normalize");
  if (op.allowed_sorts[0].label !== "Created") throw new Error("sort label opt did not normalize");
  if (manifest.graph.name !== null) throw new Error("graph name opt did not normalize to null");
}

console.log("row-decoder conformance OK");
