# @gleaph/sdk

JavaScript/TypeScript SDK for application canisters that delegate read scenarios to the Gleaph
Router. The SDK mirrors the Router wire format (`IcWireValue`): generated bindings reference real
JavaScript types directly, and the SDK converts them to and from the wire form — users never write
wire conversions by hand.

## Values

| GQL type                      | JS type                                                   |
| ----------------------------- | --------------------------------------------------------- |
| Int8/16/32, Uint8/16/32       | `number`                                                  |
| Int64/128/256, Uint64/128/256 | `bigint` (Int256/Uint256 carry 32-byte forms on the wire) |
| Float16                       | `GqlFloat16`                                              |
| Float32/64                    | `number`                                                  |
| Float128                      | `GqlFloat128`                                             |
| Float256                      | `GqlFloat256`                                             |
| Decimal                       | `GqlDecimal` (decimal.js)                                 |
| Date                          | `Temporal.PlainDate`                                      |
| Time / LocalTime              | `Temporal.PlainTime`                                      |
| DateTime                      | `Temporal.Instant`                                        |
| LocalDateTime                 | `Temporal.PlainDateTime`                                  |
| ZonedDateTime                 | `Temporal.ZonedDateTime`                                  |
| ZonedTime                     | `GqlZonedTime`                                            |
| Duration                      | `Temporal.Duration`                                       |
| Bytes / Path                  | `Uint8Array`                                              |
| Principal                     | `Principal` (`@icp-sdk/core`)                             |

`GqlFloat128` / `GqlFloat256` hold the canonical little-endian wire bytes and convert to/from
decimal strings and JavaScript numbers in pure `BigInt` arithmetic, so they work in Node, browsers,
workers, and edge runtimes without any wasm or async initialization. `toString()` emits the
shortest decimal string that round-trips; `toNumber()` rounds to the nearest f64.

## Usage

```ts
import { createGleaphClient, makeQueryRequest, makeMutationRequest } from "@gleaph/sdk";

const client = await createGleaphClient({
  canisterId: "rrkah-fqaaa-aaaaa-aaaaq-cai",
});

// Dynamic read; `makeQueryRequest` converts user values to the wire `ApiValue` form.
const result = await client.gqlQuery(
  makeQueryRequest("MATCH (n:Person {id: $id}) RETURN n.name", { id: "alice" }),
);

// Idempotent dynamic mutation; reuse the key only when retrying the same mutation.
const mutated = await client.gqlMutate(
  makeMutationRequest(
    "MATCH (n:Person {id: $id}) SET n.name = $name",
    { id: "alice", name: "alicia" },
    "rename-alice-1",
  ),
);
```

Generated prepared-operation bindings (from `gleaph-codegen`) wrap `preparedQuery`/`preparedMutate`
and expose typed `*Params` / `*Row` shapes backed by the value types above.

## Dynamic GQL parameters

`gqlQuery` / `gqlMutate` (and the `makeQueryRequest` / `makeMutationRequest` builders) accept
plain JavaScript values or explicit `ApiValue` wire values per parameter. Plain values are
converted through the SDK's inference rules:

| JS value                                 | Inferred wire type                     |
| ---------------------------------------- | -------------------------------------- |
| `string`                                 | `Text`                                 |
| `boolean`                                | `Bool`                                 |
| `number` (integer)                       | `Int64`                                |
| `number` (non-integer)                   | `Float64`                              |
| `bigint`                                 | `Int64`                                |
| `Uint8Array`                             | `Bytes`                                |
| `null` / `undefined`                     | `Null`                                 |
| `Array`                                  | `List` (elements inferred recursively) |
| plain object                             | `Record` (values inferred recursively) |
| `Date` / `Temporal.*`                    | their respective date/time wire type   |
| `GqlDecimal` / `GqlFloat*` / `Principal` | their respective wire type             |

Inference only sees the JavaScript type, not the GQL parameter's declared type. When the
inferred wire type would differ from what the query expects — most notably `bigint` always
infers `Int64`, so `Uint64` / `Int128` / `Decimal` parameters need an explicit tag — pass an
`ApiValue` literal (or `toApiValue(value, hint)`):

```ts
await client.gqlQuery({
  query: "MATCH (n:Person {user_id: $user_id}) RETURN n.name AS user_name",
  params: {
    user_id: { Uint64: 42n }, // bigint would infer Int64; pin the wire type
    name: "grace", // string infers Text — no tag needed
  },
});
```

Values that cannot be converted throw a `GleaphSdkError` at runtime instead of silently
corrupting the wire form. Generated prepared-operation bindings never need this: they know
each parameter's type from the manifest and encode it exactly.

## Development

- `pnpm check` — format, lint, and type checks
- `pnpm test` — conformance tests against the shared GQL value vectors
- `pnpm build` — build the library with `vp pack`
