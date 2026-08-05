//! User-facing value conversions: native JS types (bigint, number, `Temporal`, decimal.js) to and
//! from the Router wire [`ApiValue`].
//!
//! Mirrors the Rust cdk binding design: users work with real types directly — `bigint` for
//! integers, `Temporal` for temporal values, `decimal.js` `Decimal` for `Decimal`, and the
//! `GqlFloat16` / `GqlFloat128` / `GqlFloat256` wrappers for the float widths JavaScript lacks.

import { Principal } from "@icp-sdk/core/principal";
import { Temporal } from "@js-temporal/polyfill";
import GqlDecimal from "decimal.js";
import type {
  ApiExecutePreparedRequest,
  ApiPathElement,
  ApiPrepareRequest,
  ApiPreparedMutationRequest,
  ApiQueryRequest,
  ApiValue,
  ApiValueHint,
  GqlZonedTime,
  PreparedOptions,
  PreparedSortSpec,
} from "./types.ts";
import { GqlFloat128, GqlFloat256 } from "./float-values.ts";

const API_VALUE_TAGS = new Set([
  "Null",
  "Bool",
  "Int8",
  "Int16",
  "Int32",
  "Int64",
  "Uint8",
  "Uint16",
  "Uint32",
  "Uint64",
  "Int128",
  "Uint128",
  "Int256",
  "Uint256",
  "Float16",
  "Float32",
  "Float64",
  "Float128",
  "Float256",
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

const NINETY_SIX_BIT_MAX = (1n << 96n) - 1n;
const MAX_DECIMAL_SCALE = 28;

function isPlainObject(value: unknown): value is Record<string, unknown> {
  return (
    typeof value === "object" && value !== null && Object.getPrototypeOf(value) === Object.prototype
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
  if (isPlainObject(value) && value.Vertex instanceof Uint8Array) {
    return value as ApiPathElement;
  }
  if (isPlainObject(value) && value.Edge instanceof Uint8Array) {
    return value as ApiPathElement;
  }
  throw new Error("Cannot convert value to ApiPathElement");
}

// ---------------------------------------------------------------------------
// Float16 (pure JavaScript bit conversions)
// ---------------------------------------------------------------------------

/** Convert a canonical IEEE 754 binary16 bit pattern to a JavaScript number (lossless). */
export function f16BitsToNumber(bits: number): number {
  const sign = (bits & 0x8000) !== 0 ? -1 : 1;
  const exponent = (bits >> 10) & 0x1f;
  const fraction = bits & 0x3ff;
  if (exponent === 0) {
    if (fraction === 0) {
      return sign < 0 ? -0 : 0;
    }
    // Subnormal: fraction * 2^-24.
    return sign * fraction * 2 ** -24;
  }
  if (exponent === 0x1f) {
    return fraction === 0 ? sign * Infinity : NaN;
  }
  return sign * (1 + fraction / 1024) * 2 ** (exponent - 15);
}

function roundToEven(value: number, shift: number): number {
  const half = 1 << (shift - 1);
  const dropped = value & ((1 << shift) - 1);
  const kept = value >> shift;
  if (dropped > half) {
    return kept + 1;
  }
  if (dropped === half) {
    return kept + (kept & 1);
  }
  return kept;
}

/** Convert a JavaScript number to the nearest canonical IEEE 754 binary16 bit pattern. */
export function f16NumberToBits(value: number): number {
  if (Number.isNaN(value)) {
    return 0x7e00;
  }
  if (value === Infinity) {
    return 0x7c00;
  }
  if (value === -Infinity) {
    return 0xfc00;
  }
  const sign = value < 0 || (value === 0 && 1 / value === -Infinity) ? 0x8000 : 0;
  const abs = Math.abs(value);
  if (abs === 0) {
    return sign;
  }
  if (abs < 2 ** -24) {
    // Underflow to zero.
    return sign;
  }
  // Round through f32 so the exponent extraction below is exact.
  const f32 = new Float32Array(1);
  const u32 = new Uint32Array(f32.buffer);
  f32[0] = abs;
  const f32bits = u32[0] ?? 0;
  const f32Exponent = (f32bits >>> 23) & 0xff;
  const f32Fraction = f32bits & 0x7fffff;
  if (f32Exponent === 0) {
    // f32 subnormal (abs < 2^-126): far below f16 range.
    return sign;
  }
  if (f32Exponent >= 0x80 + 15) {
    // Overflow to infinity.
    return sign | 0x7c00;
  }
  const f16Exponent = f32Exponent - 127 + 15;
  if (f16Exponent >= 0x1f) {
    return sign | 0x7c00;
  }
  if (f16Exponent <= 0) {
    // Subnormal f16: the leading 1 joins the fraction; shift right by 14 - f16Exponent.
    const shifted = roundToEven((0x800000 | f32Fraction) >> (14 - f16Exponent), 13 - f16Exponent);
    // (The rounding above renormalizes: a carry into bit 10 means a normal f16.)
    if (shifted >= 0x400) {
      return sign | 0x0400;
    }
    return sign | shifted;
  }
  let fraction = roundToEven(f32Fraction, 13);
  let exponent = f16Exponent;
  if (fraction === 0x400) {
    fraction = 0;
    exponent += 1;
    if (exponent >= 0x1f) {
      return sign | 0x7c00;
    }
  }
  return sign | (exponent << 10) | fraction;
}

/** Half-precision float row binding carrying the canonical 16-bit bit pattern. */
export class GqlFloat16 {
  private readonly bits: number;

  private constructor(bits: number) {
    this.bits = bits;
  }

  /** Wrap the canonical IEEE 754 binary16 bit pattern. */
  static fromBits(bits: number): GqlFloat16 {
    if (!Number.isInteger(bits) || bits < 0 || bits > 0xffff) {
      throw new Error("GqlFloat16 requires a 16-bit pattern");
    }
    return new GqlFloat16(bits);
  }

  /** Convert a JavaScript number to the nearest half-precision value. */
  static fromNumber(value: number): GqlFloat16 {
    return GqlFloat16.fromBits(f16NumberToBits(value));
  }

  /** Return the canonical 16-bit bit pattern. */
  toBits(): number {
    return this.bits;
  }

  /** Return the numeric value (lossless for the widened value). */
  toNumber(): number {
    return f16BitsToNumber(this.bits);
  }

  toString(): string {
    return String(this.toNumber());
  }

  equals(other: GqlFloat16): boolean {
    return this.bits === other.bits;
  }
}

// ---------------------------------------------------------------------------
// Integer and decimal wire forms
// ---------------------------------------------------------------------------

/** Encode a bigint as `width` little-endian bytes (two's complement when `signed`). */
function bigIntToLeBytes(value: bigint, width: number, signed: boolean): Uint8Array {
  const bits = BigInt(width * 8);
  const modulus = 1n << bits;
  const minimum = signed ? -(1n << (bits - 1n)) : 0n;
  const maximum = signed ? (1n << (bits - 1n)) - 1n : modulus - 1n;
  if (value < minimum || value > maximum) {
    throw new Error(`integer is outside its ${width}-byte wire width`);
  }
  let encoded = signed && value < 0n ? modulus + value : value;
  const out = new Uint8Array(width);
  for (let index = 0; index < width; index += 1) {
    out[index] = Number(encoded & 0xffn);
    encoded >>= 8n;
  }
  return out;
}

/** Decode `width` little-endian bytes into a bigint (two's complement when `signed`). */
function leBytesToBigInt(bytes: Uint8Array, signed: boolean): bigint {
  let value = 0n;
  for (let index = bytes.byteLength - 1; index >= 0; index -= 1) {
    value = (value << 8n) | BigInt(bytes[index] ?? 0);
  }
  if (signed && bytes.byteLength > 0 && (bytes[bytes.byteLength - 1]! & 0x80) !== 0) {
    value -= 1n << BigInt(bytes.byteLength * 8);
  }
  return value;
}

/** Encode a decimal.js value into the 16-byte rust_decimal packed wire form. */
export function decimalToWireBytes(value: GqlDecimal): Uint8Array {
  let text = value.toString();
  let negative = false;
  if (text.startsWith("-")) {
    negative = true;
    text = text.slice(1);
  }
  let exponent = 0;
  const eMatch = /^([\d.]+)e([+-]\d+)$/i.exec(text);
  if (eMatch) {
    text = eMatch[1]!;
    exponent = Number(eMatch[2]);
  }
  const dot = text.indexOf(".");
  const integer = dot === -1 ? text : text.slice(0, dot);
  const fraction = dot === -1 ? "" : text.slice(dot + 1);
  let digits = `${integer}${fraction}`;
  let scale = fraction.length - exponent;
  if (scale < 0) {
    digits += "0".repeat(-scale);
    scale = 0;
  } else {
    const trailing = /0+$/.exec(digits);
    if (trailing && trailing[0].length > 0 && scale > 0) {
      const trimmed = Math.min(trailing[0].length, scale);
      digits = digits.slice(0, digits.length - trimmed);
      scale -= trimmed;
    }
  }
  const coefficient = BigInt(digits);
  if (coefficient === 0n) {
    negative = false;
  }
  if (scale > MAX_DECIMAL_SCALE || coefficient > NINETY_SIX_BIT_MAX) {
    throw new Error("Decimal is outside the supported 96-bit / 28-scale range");
  }
  const bytes = new Uint8Array(16);
  const view = new DataView(bytes.buffer);
  const flags = (scale << 16) | (negative ? 0x8000_0000 : 0);
  view.setUint32(0, flags >>> 0, true);
  let rest = coefficient;
  view.setUint32(4, Number(rest & 0xffffffffn), true);
  rest >>= 32n;
  view.setUint32(8, Number(rest & 0xffffffffn), true);
  rest >>= 32n;
  view.setUint32(12, Number(rest & 0xffffffffn), true);
  return bytes;
}

/** Decode the 16-byte rust_decimal packed wire form into a decimal.js value. */
export function decimalFromWireBytes(bytes: Uint8Array): GqlDecimal {
  if (bytes.byteLength !== 16) {
    throw new Error("Decimal wire form requires 16 bytes");
  }
  const view = new DataView(bytes.buffer, bytes.byteOffset, bytes.byteLength);
  const flags = view.getUint32(0, true);
  const scale = (flags >>> 16) & 0xff;
  const negative = (flags & 0x8000_0000) !== 0;
  const lo = view.getUint32(4, true);
  const mid = view.getUint32(8, true);
  const hi = view.getUint32(12, true);
  const coefficient = (BigInt(hi) << 64n) | (BigInt(mid) << 32n) | BigInt(lo);
  const sign = negative ? "-" : "";
  return new GqlDecimal(`${sign}${coefficient}e-${scale}`);
}

// ---------------------------------------------------------------------------
// Proleptic Gregorian civil-date helpers (same algorithms as gleaph-gql-value)
// ---------------------------------------------------------------------------

const DAYS_FROM_CE_EPOCH = 719_468;

function civilFromDays(days: number): { year: number; month: number; day: number } {
  let z = days + DAYS_FROM_CE_EPOCH;
  const era = Math.floor(z / 146_097);
  const dayOfEra = z - era * 146_097;
  const yearOfEra = Math.floor(
    (dayOfEra -
      Math.floor(dayOfEra / 1460) +
      Math.floor(dayOfEra / 36_524) -
      Math.floor(dayOfEra / 146_096)) /
      365,
  );
  const year = yearOfEra + era * 400;
  const dayOfYear =
    dayOfEra - (365 * yearOfEra + Math.floor(yearOfEra / 4) - Math.floor(yearOfEra / 100));
  const monthPrime = Math.floor((5 * dayOfYear + 2) / 153);
  const day = dayOfYear - Math.floor((153 * monthPrime + 2) / 5) + 1;
  const month = monthPrime < 10 ? monthPrime + 3 : monthPrime - 9;
  return { year: year + (month <= 2 ? 1 : 0), month, day };
}

function daysFromCivil(year: number, month: number, day: number): number {
  const adjustedYear = year - (month <= 2 ? 1 : 0);
  const era = Math.floor(adjustedYear / 400);
  const yearOfEra = adjustedYear - era * 400;
  const monthPrime = month > 2 ? month - 3 : month + 9;
  const dayOfYear = Math.floor((153 * monthPrime + 2) / 5) + day - 1;
  const dayOfEra =
    yearOfEra * 365 + Math.floor(yearOfEra / 4) - Math.floor(yearOfEra / 100) + dayOfYear;
  return era * 146_097 + dayOfEra - DAYS_FROM_CE_EPOCH;
}

// ---------------------------------------------------------------------------
// Temporal conversions
// ---------------------------------------------------------------------------

function plainTimeToNanos(time: Temporal.PlainTime): bigint {
  return (
    (BigInt(time.hour) * 3600n + BigInt(time.minute) * 60n + BigInt(time.second)) * 1_000_000_000n +
    BigInt(time.millisecond) * 1_000_000n +
    BigInt(time.microsecond) * 1_000n +
    BigInt(time.nanosecond)
  );
}

function plainTimeFromNanos(nanos: bigint): Temporal.PlainTime {
  const total = Number(nanos);
  const hour = Math.floor(total / 3_600_000_000_000) % 24;
  const minute = Math.floor(total / 60_000_000_000) % 60;
  const second = Math.floor(total / 1_000_000_000) % 60;
  const millisecond = Math.floor(total / 1_000_000) % 1000;
  const microsecond = Math.floor(total / 1_000) % 1000;
  const nanosecond = total % 1000;
  return new Temporal.PlainTime(hour, minute, second, millisecond, microsecond, nanosecond);
}

function instantToSecondsNanos(instant: Temporal.Instant): { seconds: bigint; nanos: number } {
  const epochNanoseconds = instant.epochNanoseconds;
  return {
    seconds: epochNanoseconds / 1_000_000_000n,
    nanos: Number(epochNanoseconds % 1_000_000_000n),
  };
}

function instantFromSecondsNanos(seconds: bigint, nanos: number): Temporal.Instant {
  return Temporal.Instant.fromEpochNanoseconds(seconds * 1_000_000_000n + BigInt(nanos));
}

function plainDateTimeToSecondsNanos(datetime: Temporal.PlainDateTime): {
  seconds: bigint;
  nanos: number;
} {
  const days = BigInt(daysFromCivil(datetime.year, datetime.month, datetime.day));
  const timeNanos =
    (BigInt(datetime.hour) * 3600n + BigInt(datetime.minute) * 60n + BigInt(datetime.second)) *
      1_000_000_000n +
    BigInt(datetime.millisecond) * 1_000_000n +
    BigInt(datetime.microsecond) * 1_000n +
    BigInt(datetime.nanosecond);
  return {
    seconds: days * 86_400n + timeNanos / 1_000_000_000n,
    nanos: Number(timeNanos % 1_000_000_000n),
  };
}

function plainDateTimeFromSecondsNanos(seconds: bigint, nanos: number): Temporal.PlainDateTime {
  const totalSeconds = Number(seconds);
  const civil = civilFromDays(Math.floor(totalSeconds / 86_400));
  const time = plainTimeFromNanos(BigInt(totalSeconds % 86_400) * 1_000_000_000n + BigInt(nanos));
  return new Temporal.PlainDateTime(
    civil.year,
    civil.month,
    civil.day,
    time.hour,
    time.minute,
    time.second,
    time.millisecond,
    time.microsecond,
    time.nanosecond,
  );
}

function offsetToTimeZone(offsetSeconds: number): string {
  const sign = offsetSeconds < 0 ? "-" : "+";
  const abs = Math.abs(offsetSeconds);
  const hours = Math.floor(abs / 3600);
  const minutes = Math.floor((abs % 3600) / 60);
  const seconds = abs % 60;
  const mm = String(minutes).padStart(2, "0");
  return seconds === 0
    ? `${sign}${String(hours).padStart(2, "0")}:${mm}`
    : `${sign}${String(hours).padStart(2, "0")}:${mm}:${String(seconds).padStart(2, "0")}`;
}

function zonedDateTimeToSecondsNanos(zoned: Temporal.ZonedDateTime): {
  seconds: bigint;
  nanos: number;
  offset_seconds: number;
} {
  const epochNanoseconds = zoned.epochNanoseconds;
  return {
    seconds: epochNanoseconds / 1_000_000_000n,
    nanos: Number(epochNanoseconds % 1_000_000_000n),
    offset_seconds: Math.trunc(zoned.offsetNanoseconds / 1_000_000_000),
  };
}

function zonedDateTimeFromSecondsNanos(
  seconds: bigint,
  nanos: number,
  offset_seconds: number,
): Temporal.ZonedDateTime {
  return new Temporal.ZonedDateTime(
    seconds * 1_000_000_000n + BigInt(nanos),
    offsetToTimeZone(offset_seconds),
  );
}

function durationToMonthsNanos(duration: Temporal.Duration): { months: number; nanos: bigint } {
  const months = duration.years * 12 + duration.months;
  const nanos =
    (BigInt(duration.days) * 86_400n +
      BigInt(duration.hours) * 3_600n +
      BigInt(duration.minutes) * 60n +
      BigInt(duration.seconds)) *
      1_000_000_000n +
    BigInt(duration.milliseconds) * 1_000_000n +
    BigInt(duration.microseconds) * 1_000n +
    BigInt(duration.nanoseconds);
  return { months, nanos };
}

function durationFromMonthsNanos(months: number, nanos: bigint): Temporal.Duration {
  const sign = months !== 0 ? Math.sign(months) : nanos === 0n ? 0 : nanos < 0n ? -1 : 1;
  const absMonths = BigInt(Math.abs(months));
  const years = Number(absMonths / 12n) * sign;
  const monthPart = Number(absMonths % 12n) * sign;
  let absNanos = nanos < 0n ? -nanos : nanos;
  const days = absNanos / 86_400_000_000_000n;
  absNanos %= 86_400_000_000_000n;
  const hours = absNanos / 3_600_000_000_000n;
  absNanos %= 3_600_000_000_000n;
  const minutes = absNanos / 60_000_000_000n;
  absNanos %= 60_000_000_000n;
  const seconds = absNanos / 1_000_000_000n;
  absNanos %= 1_000_000_000n;
  const milliseconds = absNanos / 1_000_000n;
  absNanos %= 1_000_000n;
  const microseconds = absNanos / 1_000n;
  const nanoseconds = absNanos % 1_000n;
  return new Temporal.Duration(
    years,
    monthPart,
    0,
    Number(days) * sign,
    Number(hours) * sign,
    Number(minutes) * sign,
    Number(seconds) * sign,
    Number(milliseconds) * sign,
    Number(microseconds) * sign,
    Number(nanoseconds) * sign,
  );
}

// ---------------------------------------------------------------------------
// Encode: user value -> ApiValue
// ---------------------------------------------------------------------------

function toSafeBigInt(value: bigint | number, name: string): bigint {
  if (typeof value === "bigint") {
    return value;
  }
  if (!Number.isSafeInteger(value)) {
    throw new Error(`${name} must be a safe integer`);
  }
  return BigInt(value);
}

function encodeTemporal(hint: string, value: unknown): ApiValue {
  switch (hint) {
    case "Date": {
      const date = value as Temporal.PlainDate;
      return { Date: daysFromCivil(date.year, date.month, date.day) };
    }
    case "Time":
      return { Time: plainTimeToNanos(value as Temporal.PlainTime) };
    case "LocalTime":
      return { LocalTime: plainTimeToNanos(value as Temporal.PlainTime) };
    case "DateTime":
      return { DateTime: instantToSecondsNanos(value as Temporal.Instant) };
    case "LocalDateTime":
      return { LocalDateTime: plainDateTimeToSecondsNanos(value as Temporal.PlainDateTime) };
    case "ZonedDateTime":
      return { ZonedDateTime: zonedDateTimeToSecondsNanos(value as Temporal.ZonedDateTime) };
    case "ZonedTime": {
      const zonedTime = value as GqlZonedTime;
      return {
        ZonedTime: { nanos: zonedTime.nanos, offset_seconds: zonedTime.offset_seconds },
      };
    }
    case "Duration":
      return { Duration: durationToMonthsNanos(value as Temporal.Duration) };
    default:
      throw new Error(`unsupported temporal hint ${hint}`);
  }
}

/**
 * Convert a user-facing value into the wire [`ApiValue`].
 *
 * Generated code passes the semantic-type `hint` so the exact wire variant is used (for example a
 * `bigint` becomes `Int256` bytes, not `Int64`); dynamic callers omit it and the variant is
 * inferred from the JavaScript value.
 */
export function toApiValue(value: unknown, hint?: ApiValueHint): ApiValue {
  if (value === null || value === undefined) {
    return { Null: null };
  }
  if (hint !== undefined) {
    if (isApiValue(value)) {
      return value;
    }
    switch (hint) {
      case "Null":
        return { Null: null };
      case "Bool":
        return { Bool: value as boolean };
      case "Int8":
        return { Int8: value as number };
      case "Int16":
        return { Int16: value as number };
      case "Int32":
        return { Int32: value as number };
      case "Int64":
        return { Int64: toSafeBigInt(value as bigint | number, "Int64") };
      case "Uint8":
        return { Uint8: value as number };
      case "Uint16":
        return { Uint16: value as number };
      case "Uint32":
        return { Uint32: value as number };
      case "Uint64":
        return { Uint64: toSafeBigInt(value as bigint | number, "Uint64") };
      case "Int128":
        return { Int128: toSafeBigInt(value as bigint | number, "Int128") };
      case "Uint128":
        return { Uint128: toSafeBigInt(value as bigint | number, "Uint128") };
      case "Int256":
        return { Int256: bigIntToLeBytes(value as bigint, 32, true) };
      case "Uint256":
        return { Uint256: bigIntToLeBytes(value as bigint, 32, false) };
      case "Float16":
        return { Float16: (value as GqlFloat16).toBits() };
      case "Float32":
        return { Float32: value as number };
      case "Float64":
        return { Float64: value as number };
      case "Float128":
        return { Float128: (value as GqlFloat128).toBytes() };
      case "Float256":
        return { Float256: (value as GqlFloat256).toBytes() };
      case "Decimal":
        return { Decimal: decimalToWireBytes(value as GqlDecimal) };
      case "Text":
        return { Text: value as string };
      case "Bytes":
        return { Bytes: value as Uint8Array };
      case "Principal":
        return { Principal: value as Principal | string };
      case "List":
        return { List: (value as unknown[]).map((item) => toApiValue(item)) };
      case "Path":
        return { Path: (value as ApiPathElement[]).map(toApiPathElement) };
      case "Record": {
        const record = value as Record<string, unknown>;
        return {
          Record: Object.fromEntries(
            Object.entries(record).map(([key, nested]) => [key, toApiValue(nested)]),
          ),
        };
      }
      default:
        return encodeTemporal(hint, value);
    }
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
  if (value instanceof GqlFloat16) {
    return { Float16: value.toBits() };
  }
  if (value instanceof GqlFloat128) {
    return { Float128: value.toBytes() };
  }
  if (value instanceof GqlFloat256) {
    return { Float256: value.toBytes() };
  }
  if (value instanceof GqlDecimal) {
    return { Decimal: decimalToWireBytes(value) };
  }
  if (value instanceof Temporal.Instant) {
    return { DateTime: instantToSecondsNanos(value) };
  }
  if (value instanceof Temporal.PlainDate) {
    return { Date: daysFromCivil(value.year, value.month, value.day) };
  }
  if (value instanceof Temporal.PlainTime) {
    return { Time: plainTimeToNanos(value) };
  }
  if (value instanceof Temporal.PlainDateTime) {
    return { LocalDateTime: plainDateTimeToSecondsNanos(value) };
  }
  if (value instanceof Temporal.ZonedDateTime) {
    return { ZonedDateTime: zonedDateTimeToSecondsNanos(value) };
  }
  if (value instanceof Temporal.Duration) {
    return { Duration: durationToMonthsNanos(value) };
  }
  if (isPlainObject(value) && "nanos" in value && "offset_seconds" in value) {
    return {
      ZonedTime: {
        nanos: toSafeBigInt(value.nanos as bigint | number, "ZonedTime.nanos"),
        offset_seconds: value.offset_seconds as number,
      },
    };
  }
  if (value instanceof Date) {
    return {
      DateTime: {
        seconds: BigInt(Math.trunc(value.getTime() / 1000)),
        nanos: (value.getTime() % 1000) * 1_000_000,
      },
    };
  }
  if (value instanceof Principal) {
    return { Principal: value };
  }
  if (Array.isArray(value)) {
    return { List: value.map((item) => toApiValue(item)) };
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

// ---------------------------------------------------------------------------
// Decode: ApiValue -> user value
// ---------------------------------------------------------------------------

/** Convert a wire [`ApiValue`] into the user-facing representation. */
export function fromApiValue(value: ApiValue): unknown {
  if ("Null" in value) {
    return null;
  }
  if ("Bool" in value) {
    return value.Bool;
  }
  if ("Int8" in value) return value.Int8;
  if ("Int16" in value) return value.Int16;
  if ("Int32" in value) return value.Int32;
  if ("Int64" in value) return value.Int64;
  if ("Uint8" in value) return value.Uint8;
  if ("Uint16" in value) return value.Uint16;
  if ("Uint32" in value) return value.Uint32;
  if ("Uint64" in value) return value.Uint64;
  if ("Int128" in value) return value.Int128;
  if ("Uint128" in value) return value.Uint128;
  if ("Int256" in value) return leBytesToBigInt(value.Int256, true);
  if ("Uint256" in value) return leBytesToBigInt(value.Uint256, false);
  if ("Float16" in value) return GqlFloat16.fromBits(value.Float16);
  if ("Float32" in value) return value.Float32;
  if ("Float64" in value) return value.Float64;
  if ("Float128" in value) return GqlFloat128.fromBytes(value.Float128);
  if ("Float256" in value) return GqlFloat256.fromBytes(value.Float256);
  if ("Decimal" in value) return decimalFromWireBytes(value.Decimal);
  if ("Text" in value) return value.Text;
  if ("Bytes" in value) return value.Bytes;
  if ("Date" in value) {
    const civil = civilFromDays(value.Date);
    return new Temporal.PlainDate(civil.year, civil.month, civil.day);
  }
  if ("Time" in value) return plainTimeFromNanos(toSafeBigInt(value.Time, "Time"));
  if ("LocalTime" in value) {
    return plainTimeFromNanos(toSafeBigInt(value.LocalTime, "LocalTime"));
  }
  if ("DateTime" in value) {
    return instantFromSecondsNanos(
      toSafeBigInt(value.DateTime.seconds, "DateTime.seconds"),
      value.DateTime.nanos,
    );
  }
  if ("LocalDateTime" in value) {
    return plainDateTimeFromSecondsNanos(
      toSafeBigInt(value.LocalDateTime.seconds, "LocalDateTime.seconds"),
      value.LocalDateTime.nanos,
    );
  }
  if ("ZonedDateTime" in value) {
    return zonedDateTimeFromSecondsNanos(
      toSafeBigInt(value.ZonedDateTime.seconds, "ZonedDateTime.seconds"),
      value.ZonedDateTime.nanos,
      value.ZonedDateTime.offset_seconds,
    );
  }
  if ("ZonedTime" in value) {
    return {
      nanos: toSafeBigInt(value.ZonedTime.nanos, "ZonedTime.nanos"),
      offset_seconds: value.ZonedTime.offset_seconds,
    } satisfies GqlZonedTime;
  }
  if ("Duration" in value) {
    return durationFromMonthsNanos(
      value.Duration.months,
      toSafeBigInt(value.Duration.nanos, "Duration.nanos"),
    );
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

// ---------------------------------------------------------------------------
// Request builders
// ---------------------------------------------------------------------------

export function toApiParams(
  params: Record<string, unknown | ApiValue> = {},
): Record<string, ApiValue> {
  return Object.fromEntries(Object.entries(params).map(([key, value]) => [key, toApiValue(value)]));
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

export function makePreparedMutationRequest(
  name: string,
  params: Record<string, unknown | ApiValue> | undefined,
  clientMutationKey: string,
  sort?: PreparedSortSpec[],
): ApiPreparedMutationRequest {
  const request = makeExecutePreparedRequest(name, params, sort);
  return { ...request, client_mutation_key: clientMutationKey };
}
