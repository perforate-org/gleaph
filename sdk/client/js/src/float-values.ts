//! `Float128` / `Float256` row bindings with pure-JavaScript `BigInt` conversion.
//!
//! JavaScript has no native 128/256-bit float, so these bindings hold the canonical little-endian
//! byte forms and convert to/from decimal strings and JavaScript numbers with `BigInt` arithmetic.
//! Both formats are dyadic rationals (`m * 2^e`), so `BigInt` covers the full range exactly:
//! `Float128` spans ±2^-16494..±2^16384 and `Float256` spans ±2^-262378..±2^262144. Each format is
//! converted independently (no `f128` <-> `f256` round-trip), so subnormal values never lose
//! precision through a widening/narrowing step.

import { GleaphSdkError } from "./errors.ts";

// ---------------------------------------------------------------------------
// IEEE 754 binary interchange formats
// ---------------------------------------------------------------------------

/** Format parameters for a fixed-width IEEE 754 binary interchange format. */
interface FloatFormat {
  /** Fraction bits after the implicit leading 1. */
  p: bigint;
  /** Exponent bias. */
  bias: bigint;
  /** Exponent field width in bits. */
  expBits: bigint;
  /** Smallest normal LSB exponent (`1 - bias - p`). */
  emin: bigint;
  /** Largest normal LSB exponent (`(2^expBits - 2) - bias - p`). */
  emax: bigint;
  /** Canonical little-endian byte length. */
  bytes: number;
}

const F128: FloatFormat = {
  p: 112n,
  bias: 16383n,
  expBits: 15n,
  emin: 1n - 16383n - 112n,
  emax: (1n << 15n) - 2n - 16383n - 112n,
  bytes: 16,
};

const F256: FloatFormat = {
  p: 236n,
  bias: 262143n,
  expBits: 19n,
  emin: 1n - 262143n - 236n,
  emax: (1n << 19n) - 2n - 262143n - 236n,
  bytes: 32,
};

/** IEEE 754 binary64 parameters used when approximating as a JavaScript number. */
const F64: FloatFormat = {
  p: 52n,
  bias: 1023n,
  expBits: 11n,
  emin: 1n - 1023n - 52n,
  emax: (1n << 11n) - 2n - 1023n - 52n,
  bytes: 8,
};

const LOG10_2 = 0.3010299956639812;
const LOG2_10 = 3.321928094887362;

// ---------------------------------------------------------------------------
// Bit-level helpers
// ---------------------------------------------------------------------------

function bitLength(value: bigint): number {
  return value === 0n ? 0 : value.toString(2).length;
}

function bytesToBigIntLe(bytes: Uint8Array): bigint {
  let value = 0n;
  for (let index = bytes.length - 1; index >= 0; index -= 1) {
    value = (value << 8n) | BigInt(bytes[index] ?? 0);
  }
  return value;
}

function bigIntToBytesLe(value: bigint, length: number): Uint8Array {
  const out = new Uint8Array(length);
  for (let index = 0; index < length; index += 1) {
    out[index] = Number(value & 0xffn);
    value >>= 8n;
  }
  return out;
}

function bytesEqual(a: Uint8Array, b: Uint8Array): boolean {
  if (a.byteLength !== b.byteLength) {
    return false;
  }
  for (let index = 0; index < a.byteLength; index += 1) {
    if (a[index] !== b[index]) {
      return false;
    }
  }
  return true;
}

interface UnpackedFloat {
  sign: bigint;
  exp: bigint;
  frac: bigint;
}

function expField(format: FloatFormat): bigint {
  return (1n << format.expBits) - 1n;
}

function nanFrac(format: FloatFormat): bigint {
  return 1n << (format.p - 1n);
}

function unpackFloat(format: FloatFormat, bytes: Uint8Array): UnpackedFloat {
  const bits = bytesToBigIntLe(bytes);
  return {
    sign: (bits >> BigInt(format.bytes * 8 - 1)) & 1n,
    exp: (bits >> format.p) & expField(format),
    frac: bits & ((1n << format.p) - 1n),
  };
}

function encodeFloat(format: FloatFormat, sign: bigint, exp: bigint, frac: bigint): Uint8Array {
  const bits =
    (sign << BigInt(format.bytes * 8 - 1)) |
    ((exp & expField(format)) << format.p) |
    (frac & ((1n << format.p) - 1n));
  return bigIntToBytesLe(bits, format.bytes);
}

// ---------------------------------------------------------------------------
// Decimal parsing: string -> exact `m * 2^e` with round-to-nearest-even
// ---------------------------------------------------------------------------

const FLOAT_TEXT_RE = /^([+-]?)(?:(\d+)(?:\.(\d*))?|\.(\d+))(?:[eE]([+-]?\d+))?$/;

interface ParsedFloat {
  sign: bigint;
  /** Significant digits (0 means zero). */
  m: bigint;
  /** Decimal exponent: value = m × 10^e. */
  e: bigint;
}

type ParsedFloatText = ParsedFloat | "inf" | "-inf" | "nan";

function parseFloatText(text: string): ParsedFloatText {
  const trimmed = text.trim();
  if (trimmed === "Infinity" || trimmed === "+Infinity") return "inf";
  if (trimmed === "-Infinity") return "-inf";
  if (trimmed === "NaN") return "nan";
  const match = FLOAT_TEXT_RE.exec(trimmed);
  if (match === null) {
    throw new GleaphSdkError(`invalid Float value: ${text}`);
  }
  const sign = match[1] === "-" ? 1n : 0n;
  const intPart = match[2] ?? "";
  const fracPart = (match[3] ?? "") + (match[4] ?? "");
  const digits = intPart + fracPart;
  const stripped = digits.replace(/^0+/, "");
  if (stripped === "") {
    return { sign, m: 0n, e: 0n };
  }
  let D = BigInt(stripped);
  const scale = fracPart.length;
  let E = (match[5] === undefined ? 0 : Number(match[5])) - scale;
  const trailing = stripped.length - stripped.replace(/0+$/, "").length;
  if (trailing > 0) {
    D /= 10n ** BigInt(trailing);
    E += trailing;
  }
  return { sign, m: D, e: BigInt(E) };
}

/** Exact value `m * 2^e` plus an optional positive remainder `stickyNum / stickyDen < 2^e`. */
interface MantissaValue {
  m: bigint;
  e: bigint;
  stickyNum: bigint;
  stickyDen: bigint;
}

/** Convert `D × 10^E` into `m × 2^e` (plus a sticky remainder) without rounding. */
function decimalToMantissa(D: bigint, E: bigint, format: FloatFormat): MantissaValue {
  if (E >= 0n) {
    const V = D * 5n ** E;
    return { m: V << E, e: 0n, stickyNum: 0n, stickyDen: 0n };
  }
  // value = D / 10^k = D / (2^k × 5^k), k = -E. Scale up by 2^guard so the
  // quotient carries every bit needed to round unambiguously (p+1 bits for
  // normals, more for deep subnormals), keeping the remainder as a sticky bit.
  const k = -E;
  const fiveK = 5n ** k;
  const p = Number(format.p);
  const eTotalEst = Math.floor(bitLength(D) - 1 + Number(E) * LOG2_10);
  const needBits = p + 3 + Math.max(0, Number(format.emin) + p - 1 - eTotalEst);
  let guard = Math.max(needBits - (eTotalEst + Number(k)) + 1, bitLength(fiveK) - bitLength(D) + 1);
  for (;;) {
    const scaled = D << BigInt(guard);
    const m = scaled / fiveK;
    if (m !== 0n) {
      const r = scaled % fiveK;
      return { m, e: -(BigInt(k) + BigInt(guard)), stickyNum: r, stickyDen: fiveK };
    }
    guard *= 2;
  }
}

interface RoundedFloat {
  /** Canonical significand: `[2^p, 2^(p+1))` for normal, `[1, 2^p)` for subnormal. */
  significand: bigint;
  /** LSB exponent (`format.emin` for subnormal). */
  e: bigint;
  overflow: boolean;
  zero: boolean;
}

/**
 * Round `m × 2^e` plus a positive remainder `stickyNum / stickyDen < 2^e` to the format with
 * round-to-nearest-even. The caller guarantees enough guard bits for an unambiguous decision.
 */
function roundMantissaToFormat(
  format: FloatFormat,
  m: bigint,
  e: bigint,
  stickyNum: bigint,
  stickyDen: bigint,
): RoundedFloat {
  const p = format.p;
  const p1 = p + 1n;
  if (m === 0n) {
    return { significand: 0n, e: 0n, overflow: false, zero: true };
  }
  const sticky = stickyNum !== 0n;
  const eTotal = BigInt(bitLength(m) - 1) + e;
  if (eTotal >= format.emin + p) {
    // Normal (or overflow): round once to p+1 significand bits.
    const bits = BigInt(bitLength(m));
    let significand: bigint;
    let eOut: bigint;
    if (bits <= p1) {
      // Pad to exactly p+1 bits; only the sticky remainder can round.
      significand = m << (p1 - bits);
      eOut = eTotal - p;
      if (
        2n * stickyNum > stickyDen ||
        (2n * stickyNum === stickyDen && (significand & 1n) === 1n)
      ) {
        significand += 1n;
        if (significand === 1n << p1) {
          significand >>= 1n;
          eOut += 1n;
        }
      }
    } else {
      const shift = bits - p1;
      const dropped = m & ((1n << shift) - 1n);
      const half = 1n << (shift - 1n);
      const base = m >> shift;
      const roundUp = dropped > half || (dropped === half && (sticky || (base & 1n) === 1n));
      significand = base + (roundUp ? 1n : 0n);
      eOut = eTotal - p;
      if (significand === 1n << p1) {
        significand >>= 1n;
        eOut += 1n;
      }
    }
    if (eOut > format.emax) {
      return { significand: 0n, e: 0n, overflow: true, zero: false };
    }
    return { significand, e: eOut, overflow: false, zero: false };
  }
  if (eTotal >= format.emin) {
    // Subnormal: significand = round(value / 2^emin), in [1, 2^p).
    const shift = format.emin - e;
    const dropped = m & ((1n << shift) - 1n);
    const half = 1n << (shift - 1n);
    const base = m >> shift;
    const roundUp = dropped > half || (dropped === half && (sticky || (base & 1n) === 1n));
    const significand = base + (roundUp ? 1n : 0n);
    if (significand >= 1n << p) {
      // Rounded up into the smallest normal.
      return { significand: 1n << p, e: format.emin, overflow: false, zero: false };
    }
    return { significand, e: format.emin, overflow: false, zero: false };
  }
  // Underflow: value < 2^emin; rounds to zero or the smallest subnormal.
  const shift = format.emin - e;
  const dropped = m & ((1n << shift) - 1n);
  const half = 1n << (shift - 1n);
  if (dropped > half || (dropped === half && sticky)) {
    return { significand: 1n, e: format.emin, overflow: false, zero: false };
  }
  return { significand: 0n, e: 0n, overflow: false, zero: true };
}

function floatTextToBytes(text: string, format: FloatFormat): Uint8Array {
  const parsed = parseFloatText(text);
  if (parsed === "inf") return encodeFloat(format, 0n, expField(format), 0n);
  if (parsed === "-inf") return encodeFloat(format, 1n, expField(format), 0n);
  if (parsed === "nan") return encodeFloat(format, 0n, expField(format), nanFrac(format));
  if (parsed.m === 0n) {
    return encodeFloat(format, parsed.sign, 0n, 0n);
  }
  // Fast overflow/underflow rejection (±2 bits of slack absorbs the estimate error).
  const est = bitLength(parsed.m) - 1 + Number(parsed.e) * LOG2_10;
  const top = Number(format.emax) + Number(format.p);
  if (est > top + 2) {
    return encodeFloat(format, parsed.sign, expField(format), 0n);
  }
  if (est < Number(format.emin) - 3) {
    return encodeFloat(format, parsed.sign, 0n, 0n);
  }
  const value = decimalToMantissa(parsed.m, parsed.e, format);
  const rounded = roundMantissaToFormat(format, value.m, value.e, value.stickyNum, value.stickyDen);
  if (rounded.overflow) {
    return encodeFloat(format, parsed.sign, expField(format), 0n);
  }
  if (rounded.zero) {
    return encodeFloat(format, parsed.sign, 0n, 0n);
  }
  if (rounded.significand >= 1n << format.p) {
    return encodeFloat(
      format,
      parsed.sign,
      rounded.e + format.bias + format.p,
      rounded.significand - (1n << format.p),
    );
  }
  return encodeFloat(format, parsed.sign, 0n, rounded.significand);
}

// ---------------------------------------------------------------------------
// Shortest decimal formatting: bytes -> string (round-trips, JS conventions)
// ---------------------------------------------------------------------------

/**
 * Format the value `D × 10^j` (an n-digit significand) following `Number.prototype.toString`
 * conventions: exponential when the leading digit is at or above 1e21 or below 1e-6, fixed
 * otherwise.
 */
function formatShortest(sign: bigint, D: bigint, j: number, n: number): string {
  const digits = D.toString();
  const prefix = sign === 1n ? "-" : "";
  const exponent = j + n - 1;
  if (exponent >= 21 || exponent < -6) {
    const mantissa = digits.length === 1 ? digits : `${digits.slice(0, 1)}.${digits.slice(1)}`;
    return `${prefix}${mantissa}e${exponent >= 0 ? "+" : "-"}${Math.abs(exponent)}`;
  }
  if (j >= 0) {
    return `${prefix}${digits}${"0".repeat(j)}`;
  }
  const point = j + digits.length;
  if (point > 0) {
    return `${prefix}${digits.slice(0, point)}.${digits.slice(point)}`;
  }
  return `${prefix}0.${"0".repeat(-point)}${digits}`;
}

function floatBytesToShortestText(bytes: Uint8Array, format: FloatFormat): string {
  const { sign, exp, frac } = unpackFloat(format, bytes);
  if (exp === expField(format)) {
    if (frac === 0n) return sign === 1n ? "-Infinity" : "Infinity";
    return "NaN";
  }
  const p = format.p;
  const m = exp === 0n ? frac : (1n << p) + frac;
  const e = exp === 0n ? format.emin : exp - format.bias - p;
  if (m === 0n) {
    return sign === 1n ? "-0" : "0";
  }
  // Round-trip interval: (v - ulp/2, v + ulp/2) with v = m × 2^e.
  const twoM = 2n * m;
  const eMinusOne = e - 1n;
  const eTotal = BigInt(bitLength(m) - 1) + e;
  // Leading decimal exponent k = floor(log10 v).
  let k = Math.floor(Number(eTotal) * LOG10_2);
  const pow10Cache = new Map<number, bigint>();
  const pow10 = (j: number): bigint => {
    let cached = pow10Cache.get(j);
    if (cached === undefined) {
      cached = 10n ** BigInt(j);
      pow10Cache.set(j, cached);
    }
    return cached;
  };
  const gePow10 = (j: number): boolean => {
    if (j >= 0) {
      const ten = pow10(j);
      return e >= 0n ? m << e >= ten : m >= ten << -e;
    }
    // 10^j < 1: compare value × 10^-j against 1.
    const ten = pow10(-j);
    return e >= 0n ? true : m * ten >= 1n << -e;
  };
  while (gePow10(k + 1)) k += 1;
  while (!gePow10(k)) k -= 1;
  const ceilDiv = (a: bigint, b: bigint): bigint => (a + b - 1n) / b;
  for (let n = 1; n <= 100; n += 1) {
    const tenNMinus1 = pow10(n - 1);
    const tenN = pow10(n);
    for (const j of [k - n + 1, k - n + 2]) {
      // n-digit candidates D with D × 10^j inside the interval.
      let dMin: bigint;
      let dMax: bigint;
      if (j >= 0) {
        const tenJ = pow10(j);
        if (eMinusOne >= 0n) {
          dMin = ceilDiv((twoM - 1n) << eMinusOne, tenJ);
          dMax = ((twoM + 1n) << eMinusOne) / tenJ;
        } else {
          const den = tenJ << -eMinusOne;
          dMin = ceilDiv(twoM - 1n, den);
          dMax = (twoM + 1n) / den;
        }
      } else {
        const tenNegJ = pow10(-j);
        if (eMinusOne >= 0n) {
          dMin = ceilDiv(((twoM - 1n) << eMinusOne) * tenNegJ, 1n);
          dMax = ((twoM + 1n) << eMinusOne) * tenNegJ;
        } else {
          const den = 1n << -eMinusOne;
          dMin = ceilDiv((twoM - 1n) * tenNegJ, den);
          dMax = ((twoM + 1n) * tenNegJ) / den;
        }
      }
      if (dMin < tenNMinus1) dMin = tenNMinus1;
      if (dMax >= tenN) dMax = tenN - 1n;
      if (dMin > dMax) continue;
      // Every interior candidate round-trips; the parse-back check settles boundary ties.
      // The sign is part of the candidate text so negative values verify too.
      const prefix = sign === 1n ? "-" : "";
      let tried = 0;
      for (let D = dMin; D <= dMax && tried < 16; D += 1n, tried += 1) {
        if (bytesEqual(floatTextToBytes(`${prefix}${D}e${j}`, format), bytes)) {
          return formatShortest(sign, D, j, n);
        }
      }
    }
  }
  throw new GleaphSdkError("shortest decimal representation not found");
}

// ---------------------------------------------------------------------------
// f64 widening / narrowing
// ---------------------------------------------------------------------------

function f64ToFloatBytes(value: number, format: FloatFormat): Uint8Array {
  if (Number.isNaN(value)) {
    return encodeFloat(format, 0n, expField(format), nanFrac(format));
  }
  const sign = value < 0 || Object.is(value, -0) ? 1n : 0n;
  const abs = Math.abs(value);
  if (abs === Infinity) {
    return encodeFloat(format, sign, expField(format), 0n);
  }
  const buffer = new ArrayBuffer(8);
  const view = new DataView(buffer);
  view.setFloat64(0, abs, true);
  const bits = view.getBigUint64(0, true);
  const exp64 = (bits >> 52n) & 0x7ffn;
  const frac64 = bits & ((1n << 52n) - 1n);
  let m = exp64 === 0n ? frac64 : (1n << 52n) + frac64;
  let e = exp64 === 0n ? -1074n : exp64 - 1023n - 52n;
  if (m === 0n) {
    return encodeFloat(format, sign, 0n, 0n);
  }
  // Widen exactly. Every f64 value (including f64 subnormals, ≥ 2^-1074) lands in the
  // normal range of f128/f256, so only a significand left-shift is needed.
  const shift = format.p - 52n;
  m <<= shift;
  e -= shift;
  while (m < 1n << format.p) {
    m <<= 1n;
    e -= 1n;
  }
  return encodeFloat(format, sign, e + format.bias + format.p, m - (1n << format.p));
}

function floatBytesToF64Number(bytes: Uint8Array, format: FloatFormat): number {
  const { sign, exp, frac } = unpackFloat(format, bytes);
  if (exp === expField(format)) {
    if (frac === 0n) return sign === 1n ? -Infinity : Infinity;
    return NaN;
  }
  const p = format.p;
  const m = exp === 0n ? frac : (1n << p) + frac;
  const e = exp === 0n ? format.emin : exp - format.bias - p;
  if (m === 0n) {
    return sign === 1n ? -0 : 0;
  }
  const rounded = roundMantissaToFormat(F64, m, e, 0n, 0n);
  if (rounded.overflow) return sign === 1n ? -Infinity : Infinity;
  if (rounded.zero) return sign === 1n ? -0 : 0;
  // The 53-bit significand and the LSB exponent make this product an exact f64.
  const value = Number(rounded.significand) * 2 ** Number(rounded.e);
  return sign === 1n ? -value : value;
}

// ---------------------------------------------------------------------------
// Row bindings
// ---------------------------------------------------------------------------

/** Binary128 row binding carrying the canonical little-endian 16-byte wire form. */
export class GqlFloat128 {
  private readonly bytes: Uint8Array;

  private constructor(bytes: Uint8Array) {
    this.bytes = bytes;
  }

  /** Wrap the canonical little-endian 16-byte form. */
  static fromBytes(bytes: Uint8Array): GqlFloat128 {
    if (bytes.byteLength !== 16) {
      throw new GleaphSdkError("GqlFloat128 requires 16 bytes");
    }
    return new GqlFloat128(new Uint8Array(bytes));
  }

  /** Parse a decimal string into the nearest canonical form (round-to-nearest-even). */
  static fromString(value: string): GqlFloat128 {
    return GqlFloat128.fromBytes(floatTextToBytes(value, F128));
  }

  /** Widen a JavaScript number exactly. */
  static fromNumber(value: number): GqlFloat128 {
    return GqlFloat128.fromBytes(f64ToFloatBytes(value, F128));
  }

  /** Return the canonical little-endian 16-byte form. */
  toBytes(): Uint8Array {
    return new Uint8Array(this.bytes);
  }

  /** Format as the shortest decimal string that round-trips (JS number conventions). */
  toString(): string {
    return floatBytesToShortestText(this.bytes, F128);
  }

  /** Approximate as a JavaScript number (round-to-nearest-even). */
  toNumber(): number {
    return floatBytesToF64Number(this.bytes, F128);
  }

  equals(other: GqlFloat128): boolean {
    return bytesEqual(this.bytes, other.bytes);
  }
}

/** Binary256 row binding carrying the canonical little-endian 32-byte wire form. */
export class GqlFloat256 {
  private readonly bytes: Uint8Array;

  private constructor(bytes: Uint8Array) {
    this.bytes = bytes;
  }

  /** Wrap the canonical little-endian 32-byte form. */
  static fromBytes(bytes: Uint8Array): GqlFloat256 {
    if (bytes.byteLength !== 32) {
      throw new GleaphSdkError("GqlFloat256 requires 32 bytes");
    }
    return new GqlFloat256(new Uint8Array(bytes));
  }

  /** Parse a decimal string into the nearest canonical form (round-to-nearest-even). */
  static fromString(value: string): GqlFloat256 {
    return GqlFloat256.fromBytes(floatTextToBytes(value, F256));
  }

  /** Widen a JavaScript number exactly. */
  static fromNumber(value: number): GqlFloat256 {
    return GqlFloat256.fromBytes(f64ToFloatBytes(value, F256));
  }

  /** Return the canonical little-endian 32-byte form. */
  toBytes(): Uint8Array {
    return new Uint8Array(this.bytes);
  }

  /** Format as the shortest decimal string that round-trips (JS number conventions). */
  toString(): string {
    return floatBytesToShortestText(this.bytes, F256);
  }

  /** Approximate as a JavaScript number (round-to-nearest-even). */
  toNumber(): number {
    return floatBytesToF64Number(this.bytes, F256);
  }

  equals(other: GqlFloat256): boolean {
    return bytesEqual(this.bytes, other.bytes);
  }
}
