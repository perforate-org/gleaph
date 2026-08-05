# @gleaph/example-typescript-app

Example TypeScript application showing how `gleaph-codegen` output and the `@gleaph/sdk`
client fit together.

## Flow

1. **Declare prepared operations** in [`manifest.json`](manifest.json). Each operation has a
   name, a kind (`Query` / `Update`), typed parameters, and a result schema covering the full
   scalar vocabulary (integers, decimals, floats, temporals, principals, lists, paths).
2. **Generate the typed adapter** into [`generated.ts`](generated.ts):

   ```sh
   cargo run -p gleaph-codegen -- --manifest manifest.json \
     --target typescript --output generated.ts
   ```

   The generated code declares `*Params` / `*Row` interfaces backed by the SDK's real value
   types (`bigint`, `GqlDecimal`, `GqlFloat16`, `GqlFloat128`, `Temporal.*`, `GqlZonedTime`,
   `PrincipalLike`). Query operations wrap `executePrepared`; update operations wrap
   `executePreparedMutation` and take an explicit `clientMutationKey`.

3. **Use the client**: build a `GraphClient` with `createIcGraphClient`, wrap it with
   `withPreparedQueries`, and call the typed operations. See [`src/main.ts`](src/main.ts).

## What the example shows

| Operation      | Pattern                                                                               |
| -------------- | ------------------------------------------------------------------------------------- |
| `find-users`   | Prepared read with a caller-selected sort key; `Date` and `Float16` row decoding      |
| `user-account` | Exotic scalar decoding: `Int256`, `Decimal`, `Float128`, temporal, `Principal`, lists |
| `create-user`  | Idempotent mutation passing a `clientMutationKey` and exotic parameter encoding       |
| `main.ts`      | Browser entrypoint: `@icp-sdk/auth` sign-in, then typed prepared operations           |
| `main.ts`      | Dynamic GQL via `graph.execute(...)` for ad-hoc reads                                 |

## Authentication

`src/main.ts` authenticates with [`@icp-sdk/auth`](https://www.npmjs.com/package/@icp-sdk/auth)
and passes the resulting identity to `createIcGraphClient`:

```ts
import { AuthClient } from "@icp-sdk/auth/client";

const authClient = new AuthClient(); // persists the delegation in IndexedDB
const identity = authClient.isAuthenticated()
  ? await authClient.getIdentity() // restore a stored session
  : await authClient.signIn(); // open the identity provider (Internet Identity / OpenID)

const graph = await createIcGraphClient({ canisterId, identity });
```

The caller principal the Router sees is `identity.getPrincipal()`.

### Version note

`@icp-sdk/auth` (and `@icp-sdk/signer`) currently declare `@icp-sdk/core ^5`, while this
workspace standardizes on core 6. `pnpm-workspace.yaml` therefore allows the auth packages to
run against core 6 so the example keeps a single `@icp-sdk/core` version (mixing versions would
split the `Principal` class across two installs and break `instanceof` checks and types).

## Validation (no Router required)

```sh
pnpm typecheck   # tsc against the SDK dist (run `pnpm sdk:build` first)
pnpm smoke       # runs the adapter against a mock client, asserting wire encoding and decode
```

`scripts/smoke.mjs` feeds canned wire rows through the generated adapter and asserts that real
values round-trip through `toApiValue` / `fromApiValue` — no live Router or identity needed.

## Notes

- `Temporal` values come from `@js-temporal/polyfill` (or native Temporal when available) and
  `GqlDecimal` from decimal.js; these are ordinary user-side dependencies.
- The Router principal and identity in `src/main.ts` are placeholders; configure them from
  environment or deployment configuration.
- Regenerate `generated.ts` whenever `manifest.json` changes; the fixture check
  (`pnpm codegen:check-fixtures`) verifies it is in sync.
