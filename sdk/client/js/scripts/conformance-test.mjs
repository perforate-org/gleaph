import { readFile } from "node:fs/promises";
import { encodeCanonicalGqlValue, bytesToHex } from "../src/canonical-value.ts";

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

console.log(`sdk/js canonical value conformance: ${fixture.vectors.length} vectors passed`);
