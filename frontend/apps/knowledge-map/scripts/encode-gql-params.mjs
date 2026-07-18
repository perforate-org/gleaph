//! Encode GQL named parameters into the compact binary blob used by
//! `gleaph_gql_ic::encode_gql_params_blob`.
//!
//! The Graph and Router canisters decode this with
//! `gleaph_gql_ic::decode_gql_params_blob`. This module only implements the
//! subset needed for social-demo seeding: Bool, Int64, and Text inside a top
//! level Record. The binary layout mirrors `crates/gql/src/value/impls.rs`.
//!
//! Parameter keys must match the names stored by the GQL parser, e.g. "$demo_id"
//! (the leading "$" is part of the key).

const TAG_NULL = 0;
const TAG_BOOL = 1;
const TAG_INT64 = 5;
const TAG_TEXT = 18;
const TAG_RECORD = 30;

function writeU32LE(value) {
  const buf = Buffer.allocUnsafe(4);
  buf.writeUInt32LE(value);
  return buf;
}

function writeI64LE(value) {
  const buf = Buffer.allocUnsafe(8);
  buf.writeBigInt64LE(BigInt(value));
  return buf;
}

function writeLenPrefixedBytes(bytes) {
  return Buffer.concat([writeU32LE(bytes.length), bytes]);
}

function encodeValue(value) {
  if (value === null || value === undefined) {
    return Buffer.from([TAG_NULL]);
  }
  if (typeof value === "boolean") {
    return Buffer.from([TAG_BOOL, value ? 1 : 0]);
  }
  if (typeof value === "number") {
    if (!Number.isInteger(value)) {
      throw new Error(`encode-gql-params: non-integer number not supported: ${value}`);
    }
    return Buffer.concat([Buffer.from([TAG_INT64]), writeI64LE(value)]);
  }
  if (typeof value === "bigint") {
    return Buffer.concat([Buffer.from([TAG_INT64]), writeI64LE(value)]);
  }
  if (typeof value === "string") {
    return Buffer.concat([Buffer.from([TAG_TEXT]), writeLenPrefixedBytes(Buffer.from(value, "utf8"))]);
  }
  throw new Error(`encode-gql-params: unsupported value type ${typeof value}: ${value}`);
}

/**
 * Encode a plain JS object as a GQL Value::Record binary blob.
 *
 * @param {Record<string, boolean | number | bigint | string | null>} params
 * @returns {Uint8Array}
 */
export function encodeGqlParamsBlob(params) {
  if (params === null || params === undefined || Object.keys(params).length === 0) {
    return new Uint8Array(0);
  }
  const entries = Object.entries(params);
  const chunks = [Buffer.from([TAG_RECORD]), writeU32LE(entries.length)];
  for (const [key, value] of entries) {
    chunks.push(writeLenPrefixedBytes(Buffer.from(key, "utf8")));
    chunks.push(encodeValue(value));
  }
  return new Uint8Array(Buffer.concat(chunks));
}

/**
 * Render a Uint8Array as a Candid `vec { ... }` text literal.
 *
 * @param {Uint8Array} bytes
 * @returns {string}
 */
export function candidVecBytes(bytes) {
  if (bytes.length === 0) {
    return "vec {}";
  }
  // Candid `blob` is a shorthand for `vec nat8` and is more compact and
  // reliably parsed by the `icp` CLI than `vec { 1; 2; ... }`.
  const hex = Array.from(bytes)
    .map((b) => "\\" + b.toString(16).padStart(2, "0"))
    .join("");
  return `blob "${hex}"`;
}
