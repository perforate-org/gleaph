import type { GleaphError as GleaphErrorVariant } from "./types.js";

/**
 * Error thrown when a graph canister returns an error variant.
 *
 * @example
 * ```ts
 * try {
 *   await graph.query("INVALID GQL");
 * } catch (e) {
 *   if (e instanceof GleaphError) {
 *     console.error(e.code);   // "ParseError"
 *     console.error(e.detail); // "unexpected token ..."
 *   }
 * }
 * ```
 */
export class GleaphError extends Error {
	/** Error code matching the Candid variant key (e.g. "ParseError", "BudgetExhausted"). */
	readonly code: string;
	/** Additional detail from the canister (string, number, or null). */
	readonly detail: unknown;

	constructor(variant: GleaphErrorVariant) {
		const [code, detail] = Object.entries(variant)[0] as [string, unknown];
		const message =
			typeof detail === "string"
				? `${code}: ${detail}`
				: typeof detail === "number"
					? `${code}: ${detail}`
					: code;
		super(message);
		this.name = "GleaphError";
		this.code = code;
		this.detail = detail;
	}
}

/**
 * Unwrap a Candid `Result` variant, throwing {@link GleaphError} on `Err`.
 *
 * @throws {GleaphError} If the result is an `Err` variant.
 */
export function unwrap<T>(result: { Ok: T } | { Err: GleaphErrorVariant }): T {
	if ("Ok" in result) return result.Ok;
	throw new GleaphError(result.Err);
}
