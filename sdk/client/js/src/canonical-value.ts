import { Principal } from "@icp-sdk/core/principal";
import type { ApiPathElement, ApiValue } from "./types";

const textEncoder = new TextEncoder();

function appendU8(out: number[], value: number): void {
  out.push(value & 0xff);
}

function appendBytes(out: number[], bytes: Uint8Array): void {
  for (const byte of bytes) out.push(byte);
}

function appendU32(out: number[], value: number): void {
  if (!Number.isInteger(value) || value < 0 || value > 0xffffffff) {
    throw new Error("value is outside the unsigned 32-bit wire width");
  }
  const bytes = new Uint8Array(4);
  new DataView(bytes.buffer).setUint32(0, value, true);
  appendBytes(out, bytes);
}

function appendI32(out: number[], value: number): void {
  if (!Number.isInteger(value) || value < -0x80000000 || value > 0x7fffffff) {
    throw new Error("value is outside the signed 32-bit wire width");
  }
  const bytes = new Uint8Array(4);
  new DataView(bytes.buffer).setInt32(0, value, true);
  appendBytes(out, bytes);
}

function appendU64(out: number[], value: bigint): void {
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setBigUint64(0, value, true);
  appendBytes(out, bytes);
}

function appendI64(out: number[], value: bigint): void {
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setBigInt64(0, value, true);
  appendBytes(out, bytes);
}

function appendF64(out: number[], value: number): void {
  if (!Number.isFinite(value)) throw new Error("Float64 must be finite");
  const bytes = new Uint8Array(8);
  new DataView(bytes.buffer).setFloat64(0, value, true);
  appendBytes(out, bytes);
}

function appendF32(out: number[], value: number): void {
  if (!Number.isFinite(value)) throw new Error("Float32 must be finite");
  const bytes = new Uint8Array(4);
  new DataView(bytes.buffer).setFloat32(0, value, true);
  appendBytes(out, bytes);
}

function appendBigIntFixed(out: number[], value: bigint, width: number, signed: boolean): void {
  const bits = BigInt(width * 8);
  const modulus = 1n << bits;
  const minimum = signed ? -(1n << (bits - 1n)) : 0n;
  const maximum = signed ? (1n << (bits - 1n)) - 1n : modulus - 1n;
  if (value < minimum || value > maximum) throw new Error("integer is outside its wire width");
  let encoded = signed && value < 0n ? modulus + value : value;
  for (let index = 0; index < width; index += 1) {
    out.push(Number(encoded & 0xffn));
    encoded >>= 8n;
  }
}

function toSafeBigInt(value: bigint | number, name: string): bigint {
  if (typeof value === "bigint") return value;
  if (!Number.isSafeInteger(value)) throw new Error(`${name} must be a safe integer`);
  return BigInt(value);
}

function appendLengthPrefixed(out: number[], bytes: Uint8Array): void {
  appendU32(out, bytes.byteLength);
  appendBytes(out, bytes);
}

function appendText(out: number[], value: string): void {
  appendLengthPrefixed(out, textEncoder.encode(value));
}

function decimalParts(value: string): { coefficient: bigint; scale: number; negative: boolean } {
  const match = /^(-?)(\d+)(?:\.(\d+))?$/.exec(value);
  if (!match) throw new Error("Decimal must be a plain finite decimal string");
  const fraction = match[3] ?? "";
  const scale = fraction.length;
  const coefficient = BigInt(`${match[2]}${fraction}`);
  return { coefficient, scale, negative: match[1] === "-" && coefficient !== 0n };
}

function appendDecimal(out: number[], value: string): void {
  const { coefficient, scale, negative } = decimalParts(value);
  if (scale > 28 || coefficient > 0xffffffffffffffffffffffffn) {
    throw new Error("Decimal is outside the supported 96-bit / 28-scale range");
  }
  const bytes = new Uint8Array(16);
  const view = new DataView(bytes.buffer);
  let rest = coefficient;
  view.setUint32(0, Number(rest & 0xffffffffn), true);
  rest >>= 32n;
  view.setUint32(4, Number(rest & 0xffffffffn), true);
  rest >>= 32n;
  view.setUint32(8, Number(rest & 0xffffffffn), true);
  view.setUint32(12, (scale << 16) | (negative ? 0x80000000 : 0), true);
  appendBytes(out, bytes);
}

function appendPath(out: number[], items: ApiPathElement[]): void {
  appendU32(out, items.length);
  for (const item of items) {
    if ("Vertex" in item) {
      appendU8(out, 0);
      appendLengthPrefixed(out, item.Vertex);
    } else {
      appendU8(out, 1);
      appendLengthPrefixed(out, item.Edge);
    }
  }
}

function appendValue(out: number[], value: ApiValue): void {
  if ("Null" in value) {
    appendU8(out, 0);
  } else if ("Bool" in value) {
    appendU8(out, 1);
    appendU8(out, value.Bool ? 1 : 0);
  } else if ("Int8" in value) {
    appendU8(out, 2);
    appendBigIntFixed(out, BigInt(value.Int8), 1, true);
  } else if ("Int16" in value) {
    appendU8(out, 3);
    appendBigIntFixed(out, BigInt(value.Int16), 2, true);
  } else if ("Int32" in value) {
    appendU8(out, 4);
    appendBigIntFixed(out, BigInt(value.Int32), 4, true);
  } else if ("Int64" in value) {
    appendU8(out, 5);
    appendBigIntFixed(out, toSafeBigInt(value.Int64, "Int64"), 8, true);
  } else if ("Uint64" in value) {
    appendU8(out, 11);
    appendBigIntFixed(out, toSafeBigInt(value.Uint64, "Uint64"), 8, false);
  } else if ("Int128" in value) {
    appendU8(out, 6);
    appendBigIntFixed(out, toSafeBigInt(value.Int128, "Int128"), 16, true);
  } else if ("Uint128" in value) {
    appendU8(out, 12);
    appendBigIntFixed(out, toSafeBigInt(value.Uint128, "Uint128"), 16, false);
  } else if ("Int256" in value) {
    appendU8(out, 7);
    appendBigIntFixed(out, BigInt(value.Int256), 32, true);
  } else if ("Uint256" in value) {
    appendU8(out, 13);
    appendBigIntFixed(out, BigInt(value.Uint256), 32, false);
  } else if ("Uint8" in value) {
    appendU8(out, 8);
    appendBigIntFixed(out, BigInt(value.Uint8), 1, false);
  } else if ("Uint16" in value) {
    appendU8(out, 9);
    appendBigIntFixed(out, BigInt(value.Uint16), 2, false);
  } else if ("Uint32" in value) {
    appendU8(out, 10);
    appendBigIntFixed(out, BigInt(value.Uint32), 4, false);
  } else if ("Float16" in value) {
    appendU8(out, 14);
    appendBigIntFixed(out, BigInt(value.Float16), 2, false);
  } else if ("Float32" in value) {
    appendU8(out, 15);
    appendF32(out, value.Float32);
  } else if ("Float64" in value) {
    appendU8(out, 16);
    appendF64(out, value.Float64);
  } else if ("Float128" in value) {
    if (value.Float128.byteLength !== 16) throw new Error("Float128 must contain 16 canonical bytes");
    appendU8(out, 31);
    appendBytes(out, value.Float128);
  } else if ("Float256" in value) {
    if (value.Float256.byteLength !== 32) throw new Error("Float256 must contain 32 canonical bytes");
    appendU8(out, 32);
    appendBytes(out, value.Float256);
  } else if ("Decimal" in value) {
    appendU8(out, 17);
    appendDecimal(out, value.Decimal);
  } else if ("Text" in value) {
    appendU8(out, 18);
    appendText(out, value.Text);
  } else if ("Bytes" in value) {
    appendU8(out, 19);
    appendLengthPrefixed(out, value.Bytes);
  } else if ("Date" in value) {
    appendU8(out, 20);
    appendI32(out, value.Date);
  } else if ("Time" in value) {
    appendU8(out, 21);
    appendU64(out, toSafeBigInt(value.Time, "Time"));
  } else if ("LocalTime" in value) {
    appendU8(out, 22);
    appendU64(out, toSafeBigInt(value.LocalTime, "LocalTime"));
  } else if ("DateTime" in value) {
    appendU8(out, 23);
    appendI64(out, toSafeBigInt(value.DateTime.seconds, "DateTime.seconds"));
    appendU32(out, value.DateTime.nanos);
  } else if ("LocalDateTime" in value) {
    appendU8(out, 24);
    appendI64(out, toSafeBigInt(value.LocalDateTime.seconds, "LocalDateTime.seconds"));
    appendU32(out, value.LocalDateTime.nanos);
  } else if ("ZonedDateTime" in value) {
    appendU8(out, 25);
    appendI64(out, toSafeBigInt(value.ZonedDateTime.seconds, "ZonedDateTime.seconds"));
    appendU32(out, value.ZonedDateTime.nanos);
    appendI32(out, value.ZonedDateTime.offset_seconds);
  } else if ("ZonedTime" in value) {
    appendU8(out, 26);
    appendU64(out, toSafeBigInt(value.ZonedTime.nanos, "ZonedTime.nanos"));
    appendI32(out, value.ZonedTime.offset_seconds);
  } else if ("Duration" in value) {
    appendU8(out, 27);
    appendI32(out, value.Duration.months);
    appendI64(out, toSafeBigInt(value.Duration.nanos, "Duration.nanos"));
  } else if ("List" in value) {
    appendU8(out, 28);
    appendU32(out, value.List.length);
    for (const item of value.List) appendValue(out, item);
  } else if ("Path" in value) {
    appendU8(out, 29);
    appendPath(out, value.Path);
  } else if ("Record" in value) {
    appendU8(out, 30);
    const fields = Object.entries(value.Record);
    appendU32(out, fields.length);
    for (const [key, field] of fields) {
      appendText(out, key);
      appendValue(out, field);
    }
  } else if ("Principal" in value) {
    const principal =
      typeof value.Principal === "string" ? Principal.fromText(value.Principal) : value.Principal;
    const bytes = principal.toUint8Array();
    if (bytes.length > 255) throw new Error("Principal is too large");
    appendU8(out, 34);
    appendU8(out, bytes.length);
    appendBytes(out, bytes);
  } else {
    throw new Error("Unsupported ApiValue variant");
  }
}

/** Encode one ApiValue with the Rust GQL compact binary value contract. */
export function encodeCanonicalGqlValue(value: ApiValue): Uint8Array {
  const out: number[] = [];
  appendValue(out, value);
  return Uint8Array.from(out);
}

export function bytesToHex(bytes: Uint8Array): string {
  return Array.from(bytes, (byte) => byte.toString(16).padStart(2, "0")).join("");
}
