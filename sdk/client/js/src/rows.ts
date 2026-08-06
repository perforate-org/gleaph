import { GleaphSdkError } from "./errors.ts";
import type {
  ApiValue,
  PreparedManifestColumn,
  PreparedManifestRecordField,
  PreparedSemanticType,
} from "./types.ts";
import { fromApiValue } from "./values.ts";

/**
 * The [`ApiValue`] variant key that carries the value for each scalar semantic
 * type. The decoder validates the runtime tag against the declared column type
 * before converting, so schema drift fails loudly instead of producing garbage
 * through `as` casts.
 */
type ApiValueTag =
  | "Null"
  | "Bool"
  | "Int8"
  | "Int16"
  | "Int32"
  | "Int64"
  | "Int128"
  | "Int256"
  | "Uint8"
  | "Uint16"
  | "Uint32"
  | "Uint64"
  | "Uint128"
  | "Uint256"
  | "Float16"
  | "Float32"
  | "Float64"
  | "Float128"
  | "Float256"
  | "Decimal"
  | "Text"
  | "Bytes"
  | "Date"
  | "Time"
  | "LocalTime"
  | "DateTime"
  | "LocalDateTime"
  | "ZonedDateTime"
  | "ZonedTime"
  | "Duration"
  | "Principal"
  | "Path"
  | "List"
  | "Record";

/** The scalar (string) members of [`PreparedSemanticType`]. */
type ScalarSemanticType = Exclude<PreparedSemanticType, { List: unknown } | { Record: unknown }>;

const TAG_BY_SEMANTIC_TYPE: Record<ScalarSemanticType, ApiValueTag> = {
  Null: "Null",
  Bool: "Bool",
  Int8: "Int8",
  Int16: "Int16",
  Int32: "Int32",
  Int64: "Int64",
  Int128: "Int128",
  Int256: "Int256",
  Uint8: "Uint8",
  Uint16: "Uint16",
  Uint32: "Uint32",
  Uint64: "Uint64",
  Uint128: "Uint128",
  Uint256: "Uint256",
  Float16: "Float16",
  Float32: "Float32",
  Float64: "Float64",
  Float128: "Float128",
  Float256: "Float256",
  Decimal: "Decimal",
  Text: "Text",
  Bytes: "Bytes",
  Date: "Date",
  Time: "Time",
  LocalTime: "LocalTime",
  DateTime: "DateTime",
  LocalDateTime: "LocalDateTime",
  ZonedDateTime: "ZonedDateTime",
  ZonedTime: "ZonedTime",
  Duration: "Duration",
  Principal: "Principal",
  Path: "Path",
};

/**
 * Semantic types whose wire values may arrive as a JS `number` (the SDK type
 * allows `number | bigint` for these). Normalize them to `bigint` so consumers
 * never need to handle both. `Int256`/`Uint256` stay with `fromApiValue`,
 * which already returns `bigint` from their byte payloads.
 */
const WIDE_INTEGER_TYPES = new Set<ScalarSemanticType>(["Int64", "Uint64", "Int128", "Uint128"]);

const apiValueTag = (value: ApiValue): string => Object.keys(value)[0] ?? "unknown";

const schemaMismatch = (message: string): GleaphSdkError =>
  new GleaphSdkError(message, "GLEAPH_ROW_SCHEMA_MISMATCH");

/**
 * Convert one [`ApiValue`] against a declared semantic type:
 *
 * - scalar types are validated by runtime tag and converted (wide integers
 *   normalize to `bigint`, everything else goes through [`fromApiValue`]);
 * - `List`/`Record` recurse with the element/field schema, so nested values are
 *   normalized too;
 * - `Null` values return `null` (nullability is enforced by the caller for
 *   columns and by [`decodeRecordFields`] for record fields).
 */
function decodeValue(value: ApiValue, type: PreparedSemanticType): unknown {
  if ("Null" in value) {
    return null;
  }
  if (typeof type === "string") {
    const tag = TAG_BY_SEMANTIC_TYPE[type];
    if (!(tag in value)) {
      throw schemaMismatch(`declared ${type} but received ${apiValueTag(value)}`);
    }
    if (WIDE_INTEGER_TYPES.has(type)) {
      const raw = (value as unknown as Record<string, bigint | number>)[tag];
      return BigInt(raw as bigint | number);
    }
    return fromApiValue(value);
  }
  if ("List" in type) {
    if (!("List" in value)) {
      throw schemaMismatch(`declared List but received ${apiValueTag(value)}`);
    }
    return value.List.map((item) => decodeValue(item, type.List.element));
  }
  if (!("Record" in value)) {
    throw schemaMismatch(`declared Record but received ${apiValueTag(value)}`);
  }
  return decodeRecordFields(value.Record, type.Record.fields);
}

function decodeRecordFields(
  record: Record<string, ApiValue>,
  fields: PreparedManifestRecordField[],
): Record<string, unknown> {
  const decoded: Record<string, unknown> = {};
  for (const field of fields) {
    const value = record[field.name];
    if (value === undefined) {
      throw schemaMismatch(`row is missing field "${field.name}"`);
    }
    if ("Null" in value) {
      if (!field.nullable) {
        throw schemaMismatch(`field "${field.name}" is null but declared non-null`);
      }
      decoded[field.name] = null;
      continue;
    }
    decoded[field.name] = decodeValue(value, field.semantic_type);
  }
  return decoded;
}

/**
 * Decode one result row against a column schema.
 *
 * The column schema is the `result.columns` of a prepared-operation manifest
 * (`PreparedManifestOperation`), or an equivalent authored spec. Each declared
 * column is validated (present in the row, runtime value tag matches the
 * declared semantic type, `Null` only for nullable columns) and converted:
 *
 * - `Int64`/`Uint64`/`Int128`/`Uint128` normalize to `bigint`;
 * - `List`/`Record` columns decode recursively against their element/field
 *   schema;
 * - every other type converts through [`fromApiValue`] (dates become Temporal
 *   values, `Bytes` become `Uint8Array`);
 * - nullable columns decode to `null`; missing columns, type drift, and null in
 *   a non-null column throw a `GleaphSdkError` instead of silently yielding
 *   `undefined`.
 *
 * Columns present in the row but absent from the schema are ignored.
 */
export function decodeRow(
  row: Record<string, ApiValue>,
  columns: PreparedManifestColumn[],
): Record<string, unknown> {
  const decoded: Record<string, unknown> = {};
  for (const column of columns) {
    const value = row[column.name];
    if (value === undefined) {
      throw schemaMismatch(`row is missing column "${column.name}"`);
    }
    if ("Null" in value) {
      if (!column.nullable) {
        throw schemaMismatch(`column "${column.name}" is null but declared non-null`);
      }
      decoded[column.name] = null;
      continue;
    }
    decoded[column.name] = decodeValue(value, column.semantic_type);
  }
  return decoded;
}

/** Decode every row of a query result against one column schema. */
export function decodeRows(
  rows: Record<string, ApiValue>[],
  columns: PreparedManifestColumn[],
): Record<string, unknown>[] {
  return rows.map((row) => decodeRow(row, columns));
}
