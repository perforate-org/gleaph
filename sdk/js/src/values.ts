import { Principal } from "@icp-sdk/core/principal";
import type {
  ApiExecutePreparedRequest,
  ApiPathElement,
  ApiPrepareRequest,
  ApiQueryRequest,
  ApiValue,
  PreparedOptions,
  PreparedSortSpec,
} from "./types";

const API_VALUE_TAGS = new Set([
  "Null",
  "Bool",
  "Int64",
  "Uint64",
  "Int128",
  "Uint128",
  "Int256",
  "Uint256",
  "Float64",
  "Decimal",
  "Text",
  "Bytes",
  "Date",
  "Time",
  "LocalTime",
  "DateTime",
  "LocalDateTime",
  "ZonedDateTime",
  "ZonedTime",
  "Duration",
  "Principal",
  "List",
  "Path",
  "Record",
]);

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return (
    typeof value === "object" &&
    value !== null &&
    Object.getPrototypeOf(value) === Object.prototype
  );
}

export function isApiValue(value: unknown): value is ApiValue {
  if (!isPlainObject(value)) {
    return false;
  }
  const keys = Object.keys(value);
  return keys.length === 1 && API_VALUE_TAGS.has(keys[0] ?? "");
}

export function toApiPathElement(value: unknown): ApiPathElement {
  if (isPlainObject(value) && "Vertex" in value) {
    return value as ApiPathElement;
  }
  if (isPlainObject(value) && "Edge" in value) {
    return value as ApiPathElement;
  }
  throw new Error("Cannot convert value to ApiPathElement");
}

export function toApiValue(value: unknown): ApiValue {
  if (value === null || value === undefined) {
    return { Null: null };
  }
  if (isApiValue(value)) {
    return value;
  }
  if (typeof value === "boolean") {
    return { Bool: value };
  }
  if (typeof value === "bigint") {
    return { Int64: value };
  }
  if (typeof value === "number") {
    if (Number.isInteger(value)) {
      return { Int64: value };
    }
    return { Float64: value };
  }
  if (typeof value === "string") {
    return { Text: value };
  }
  if (value instanceof Uint8Array) {
    return { Bytes: value };
  }
  if (value instanceof Date) {
    return {
      DateTime: {
        seconds: Math.trunc(value.getTime() / 1000),
        nanos: (value.getTime() % 1000) * 1_000_000,
      },
    };
  }
  if (value instanceof Principal) {
    return { Principal: value };
  }
  if (Array.isArray(value)) {
    return { List: value.map(toApiValue) };
  }
  if (isPlainObject(value)) {
    return {
      Record: Object.fromEntries(
        Object.entries(value).map(([key, nested]) => [key, toApiValue(nested)]),
      ),
    };
  }
  throw new Error(`Cannot convert value to ApiValue: ${typeof value}`);
}

export function fromApiValue(value: ApiValue): unknown {
  if ("Null" in value) {
    return null;
  }
  if ("Bool" in value) {
    return value.Bool;
  }
  if ("Int64" in value) {
    return value.Int64;
  }
  if ("Uint64" in value) {
    return value.Uint64;
  }
  if ("Int128" in value) {
    return value.Int128;
  }
  if ("Uint128" in value) {
    return value.Uint128;
  }
  if ("Int256" in value) {
    return value.Int256;
  }
  if ("Uint256" in value) {
    return value.Uint256;
  }
  if ("Float64" in value) {
    return value.Float64;
  }
  if ("Decimal" in value) {
    return value.Decimal;
  }
  if ("Text" in value) {
    return value.Text;
  }
  if ("Bytes" in value) {
    return value.Bytes;
  }
  if ("Date" in value) {
    return value.Date;
  }
  if ("Time" in value) {
    return value.Time;
  }
  if ("LocalTime" in value) {
    return value.LocalTime;
  }
  if ("DateTime" in value) {
    return new Date(Number(value.DateTime.seconds) * 1000);
  }
  if ("LocalDateTime" in value) {
    return value.LocalDateTime;
  }
  if ("ZonedDateTime" in value) {
    return value.ZonedDateTime;
  }
  if ("ZonedTime" in value) {
    return value.ZonedTime;
  }
  if ("Duration" in value) {
    return value.Duration;
  }
  if ("Principal" in value) {
    return typeof value.Principal === "string"
      ? Principal.fromText(value.Principal)
      : value.Principal;
  }
  if ("List" in value) {
    return value.List.map(fromApiValue);
  }
  if ("Path" in value) {
    return value.Path;
  }
  if ("Record" in value) {
    return Object.fromEntries(
      Object.entries(value.Record).map(([key, nested]) => [key, fromApiValue(nested)]),
    );
  }
  return value;
}

export function toApiParams(
  params: Record<string, unknown | ApiValue> = {},
): Record<string, ApiValue> {
  return Object.fromEntries(
    Object.entries(params).map(([key, value]) => [key, toApiValue(value)]),
  );
}

export function makeQueryRequest(
  query: string,
  params?: Record<string, unknown | ApiValue>,
): ApiQueryRequest {
  return {
    query,
    params: toApiParams(params),
  };
}

export function makePrepareRequest(
  name: string,
  query: string,
  options?: PreparedOptions,
): ApiPrepareRequest {
  const request: ApiPrepareRequest = {
    name,
    query,
  };
  if (options !== undefined) {
    request.options = options;
  }
  return request;
}

export function makeExecutePreparedRequest(
  name: string,
  params?: Record<string, unknown | ApiValue>,
  sort?: PreparedSortSpec[],
): ApiExecutePreparedRequest {
  const request: ApiExecutePreparedRequest = {
    name,
    params: toApiParams(params),
  };
  if (sort !== undefined) {
    request.sort = sort;
  }
  return request;
}
