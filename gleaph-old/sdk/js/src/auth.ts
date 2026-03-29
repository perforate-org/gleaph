/**
 * Authentication and identity utilities for Gleaph.
 *
 * Everything exported here is **re-exported from `@icp-sdk/core` and
 * `@icp-sdk/auth`** — the types and classes are identical to those packages.
 * If your application already uses them for other IC canisters, the same
 * {@link Identity} instances work with
 * {@link import("./client.js").GleaphClient | GleaphClient}.
 *
 * @example
 * ```ts
 * // Backend: keypair auth (only @icp-sdk/core needed)
 * import { GleaphClient } from "@gleaph/sdk";
 * import { Ed25519KeyIdentity } from "@gleaph/sdk/auth";
 *
 * const identity = Ed25519KeyIdentity.generate();
 * const client = new GleaphClient({ identity });
 * ```
 *
 * @example
 * ```ts
 * // Browser: Internet Identity login (requires @icp-sdk/auth)
 * import { GleaphClient } from "@gleaph/sdk";
 * import { AuthClient } from "@gleaph/sdk/auth";
 *
 * const auth = await AuthClient.create();
 * await auth.login({
 *   identityProvider: "https://identity.ic0.app",
 *   onSuccess: () => {
 *     const client = new GleaphClient({
 *       identity: auth.getIdentity(),
 *     });
 *   },
 * });
 * ```
 *
 * @example
 * ```ts
 * // Or import from @icp-sdk/* directly — fully compatible
 * import { Ed25519KeyIdentity } from "@icp-sdk/core/identity";
 * import { AuthClient } from "@icp-sdk/auth/client";
 * ```
 *
 * @module
 */

// ── Identity types (from @icp-sdk/core/agent) ──────────────────────────
//
// Core identity interfaces used by all IC agents.
// Any object satisfying `Identity` can be passed to `GleaphClient`.

export type { Identity, PublicKey, KeyPair } from "@icp-sdk/core/agent";
export { SignIdentity, AnonymousIdentity } from "@icp-sdk/core/agent";

// ── Concrete identity implementations (from @icp-sdk/core/identity) ───
//
// Ready-to-use identity classes.  Pick the one that matches your auth flow:
//
// - `Ed25519KeyIdentity`  — keypair-based, for server-side or testing
// - `ECDSAKeyIdentity`    — ECDSA P-256 keypair
// - `DelegationIdentity`  — delegated from Internet Identity or similar
// - `WebAuthnIdentity`    — browser WebAuthn / passkey

export {
	Ed25519KeyIdentity,
	Ed25519PublicKey,
} from "@icp-sdk/core/identity";
export { ECDSAKeyIdentity } from "@icp-sdk/core/identity";
export {
	DelegationIdentity,
	DelegationChain,
	Delegation,
	isDelegationValid,
	type SignedDelegation,
	type DelegationValidChecks,
} from "@icp-sdk/core/identity";
export { WebAuthnIdentity } from "@icp-sdk/core/identity";

// ── Principal (from @icp-sdk/core/principal) ───────────────────────────
//
// IC principal identifier.  Used for canister IDs and caller identities.

export { Principal } from "@icp-sdk/core/principal";

// ── AuthClient (from @icp-sdk/auth/client) ─────────────────────────────
//
// Handles the full Internet Identity login flow: creates a session key
// pair, opens the identity provider popup, and returns a
// `DelegationIdentity` with a short-lived delegation chain.
//
// Requires `@icp-sdk/auth` to be installed (optional peer dependency).
// Server-side code that uses keypair-based identity does NOT need this.

export {
	AuthClient,
	type AuthClientCreateOptions,
	type AuthClientLoginOptions,
} from "@icp-sdk/auth/client";
