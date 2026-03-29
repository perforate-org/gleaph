import { Principal } from "@icp-sdk/core/principal";
import type { PropertyMap, Value } from "./types.js";

/**
 * Convert a plain JS object to a Candid-compatible {@link PropertyMap}.
 *
 * Each value is converted to a {@link Value} variant using {@link toValue}.
 *
 * @example
 * ```ts
 * toPropertyMap({ name: "Alice", age: 30 })
 * // → [["name", { Text: "Alice" }], ["age", { Int32: 30 }]]
 * ```
 */
export function toPropertyMap(params: Record<string, unknown>): PropertyMap {
	return Object.entries(params).map(([key, val]) => [key, toValue(val)]);
}

/**
 * Convert a plain JS value to a Candid {@link Value} variant.
 *
 * | JS Type        | Candid Value |
 * |----------------|-------------|
 * | `null`/`undefined` | `Null`  |
 * | `boolean`      | `Bool`      |
 * | `bigint`       | `Int64`     |
 * | `number`       | `Float64`   |
 * | `string`       | `Text`      |
 * | `Uint8Array`   | `Bytes`     |
 * | `Principal`    | `Principal` |
 * | `Array`        | `List`      |
 *
 * You can also pass an already-formed `Value` variant (e.g. `{ Timestamp: 123n }`).
 *
 * @throws {Error} If the value cannot be converted.
 */
export function toValue(val: unknown): Value {
	if (val === null || val === undefined) return { Null: null };
	if (typeof val === "boolean") return { Bool: val };
	if (typeof val === "bigint") return { Int64: val };
	if (typeof val === "number") {
		if (Number.isInteger(val)) return { Int32: val };
		return { Float64: val };
	}
	if (typeof val === "string") return { Text: val };
	if (val instanceof Uint8Array) return { Bytes: val };
	if (val instanceof Principal) return { Principal: val };
	if (Array.isArray(val)) return { List: val.map(toValue) };
	// Already a Value variant (has exactly one key matching a Value tag)
	if (typeof val === "object" && val !== null) {
		const keys = Object.keys(val);
		if (keys.length === 1 && isValueTag(keys[0])) return val as Value;
	}
	throw new Error(`Cannot convert to Value: ${typeof val}`);
}

const VALUE_TAGS = new Set([
	"Null",
	"Bool",
	"Int8",
	"Int16",
	"Int32",
	"Int64",
	"Int128",
	"Int256",
	"Uint8",
	"Uint16",
	"Uint32",
	"Uint64",
	"Uint128",
	"Uint256",
	"Float32",
	"Float64",
	"Text",
	"Timestamp",
	"List",
	"Path",
	"Bytes",
	"Date",
	"Time",
	"DateTime",
	"Duration",
	"Principal",
	"Decimal",
]);

function isValueTag(key: string): boolean {
	return VALUE_TAGS.has(key);
}
