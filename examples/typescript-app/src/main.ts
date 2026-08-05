//! Browser-style entrypoint: authenticate with `@icp-sdk/auth` and run typed prepared
//! operations against the Gleaph Router. Type-checked by `pnpm typecheck`; the smoke test
//! (`scripts/smoke.mjs`) runs the same adapter against a mock client without a Router or a
//! browser.

import { Temporal } from "@js-temporal/polyfill";
import { AuthClient } from "@icp-sdk/auth/client";
import { GqlDecimal, GqlFloat128, toApiValue } from "@gleaph/sdk";
import { createPreparedGleaphClient } from "../generated.ts";

// `AuthClient` persists a delegation in IndexedDB; the caller principal seen by the Router is
// `identity.getPrincipal()`. `signIn` opens the identity provider (Internet Identity / OpenID)
// in a popup; with a prior session it restores the stored delegation instead.
const authClient = new AuthClient();
const identity = authClient.isAuthenticated()
  ? await authClient.getIdentity()
  : await authClient.signIn();
console.log(identity.getPrincipal().toText());

// One generated factory: the returned `PreparedGleaphClient` carries the typed prepared
// operations and the full dynamic GQL surface on the same value.
const client = await createPreparedGleaphClient({
  canisterId: "rrkah-fqaaa-aaaaa-aaaaq-cai",
  host: "http://localhost:8000",
  identity,
  fetchRootKey: true,
});

// Prepared read with a caller-selected sort key. Rows carry real values: `user_id` is a bigint,
// `joined_on` a `Temporal.PlainDate`, and `rating` a `GqlFloat16` (or null).
const users = await client.findUsers({ term: "al" }, [{ key: "user_name", direction: "asc" }]);
for (const row of users.rows) {
  console.log(row.user_name, row.user_id, row.joined_on.toString(), row.rating?.toNumber());
}

// Prepared read returning exotic scalars: Int256, Decimal, Float128, and temporal values decode
// into their real JavaScript types without hand-written wire conversion.
const account = await client.userAccount({ user_id: 42n });
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
const dynamic = await client.gqlQuery({
  query: "MATCH (n:Person {user_id: $user_id}) RETURN n.name AS user_name",
  params: { user_id: toApiValue(42n, "Uint64") },
});

// Idempotent mutation. Reuse `clientMutationKey` only when retrying the same mutation; a new
// mutation needs a fresh key.
const created = await client.createUser(
  {
    user_id: 43n,
    user_name: "ada",
    joined_on: new Temporal.PlainDate(2024, 1, 15),
    signup_fee: new GqlDecimal("12.50"),
    score: GqlFloat128.fromString("1.5"),
  },
  "create-user-ada-1",
);

// Dynamic mutation for ad-hoc writes that are not prepared. Like prepared mutations, reuse
// `client_mutation_key` only when retrying the same mutation.
const dynamicCreated = await client.gqlMutate({
  query: "MATCH (n:Person {user_id: $user_id}) SET n.name = $name",
  params: {
    user_id: toApiValue(43n, "Uint64"),
    name: toApiValue("grace", "Text"),
  },
  client_mutation_key: "rename-user-43-1",
});
console.log(created.row_count, dynamic.rows.length, dynamicCreated.row_count);
