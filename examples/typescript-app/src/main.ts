//! Example application: `gleaph-codegen` output + the `@gleaph/sdk` client.
//!
//! End-to-end flow:
//!   1. `manifest.json` declares prepared operations.
//!   2. Codegen generates the typed adapter into `generated.ts`:
//!      `cargo run -p gleaph-codegen -- --manifest manifest.json --target typescript \
//!         --output generated.ts`
//!   3. This file builds a `GraphClient` with [`createIcGraphClient`] and wraps it with
//!      [`withPreparedQueries`], then runs typed prepared operations against the Gleaph Router.
//!
//! The entrypoints demonstrate the API surface:
//! - prepared reads returning real value types (`bigint`, `Temporal`, `GqlFloat16`, ...);
//! - exotic scalar decoding (`GqlDecimal`, `GqlFloat128`, `Temporal`, `Principal`);
//! - dynamic GQL for ad-hoc reads; and
//! - an idempotent mutation passing an explicit `clientMutationKey`.
//!
//! The Router principal and identity are placeholders; configure them from environment or
//! deployment configuration. `scripts/smoke.mjs` runs the same adapter against a mock client
//! without needing a live Router.

import { Temporal } from "@js-temporal/polyfill";
import { createIcGraphClient, GqlDecimal, GqlFloat128, toApiValue } from "@gleaph/sdk";
import { withPreparedQueries } from "../generated.ts";

const graph = await createIcGraphClient({
  canisterId: "rrkah-fqaaa-aaaaa-aaaaq-cai",
  host: "http://localhost:8000",
  fetchRootKey: true,
});
const prepared = withPreparedQueries(graph);

// Prepared read with a caller-selected sort key. Rows carry real values: `user_id` is a bigint,
// `joined_on` a `Temporal.PlainDate`, and `rating` a `GqlFloat16` (or null).
const users = await prepared["find-users"]({ term: "al" }, [
  { key: "user_name", direction: "asc" },
]);
for (const row of users.rows) {
  console.log(row.user_name, row.user_id, row.joined_on.toString(), row.rating?.toNumber());
}

// Prepared read returning exotic scalars: Int256, Decimal, Float128, and temporal values decode
// into their real JavaScript types without hand-written wire conversion.
const account = await prepared["user-account"]({ user_id: 42n });
const profile = account.rows[0];
if (profile !== undefined) {
  console.log(profile.balance.toString());
  console.log(profile.amount.toFixed(2));
  console.log(profile.score?.toNumber());
  console.log(profile.joined_at.epochMilliseconds);
  console.log(profile.zoned_at.epochNanoseconds);
  console.log(profile.elapsed.total("minutes"));
  console.log(profile.owner.toText());
  console.log(profile.tags);
}

// Dynamic GQL for ad-hoc reads that are not prepared. The dynamic path takes wire-encoded
// params, so wrap user values with `toApiValue` (generated adapters do this automatically).
const dynamic = await graph.execute({
  query: "MATCH (n:Person {user_id: $user_id}) RETURN n.name AS user_name",
  params: { user_id: toApiValue(42n, "Uint64") },
});

// Idempotent mutation. Reuse `clientMutationKey` only when retrying the same mutation; a new
// mutation needs a fresh key.
const created = await prepared["create-user"](
  {
    user_id: 43n,
    user_name: "ada",
    joined_on: new Temporal.PlainDate(2024, 1, 15),
    signup_fee: new GqlDecimal("12.50"),
    score: GqlFloat128.fromString("1.5"),
  },
  "create-user-ada-1",
);
console.log(created.row_count, dynamic.rows.length);
