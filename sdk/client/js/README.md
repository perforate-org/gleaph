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
import { createIcGraphClient } from "@gleaph/sdk";

const client = createIcGraphClient({ canisterId: "rrkah-fqaaa-aaaaa-aaaaq-cai" });

const result = await client.execute({
  query: "MATCH (n:Person {id: $id}) RETURN n.name",
  params: { id: "alice" },
});
```

Generated prepared-operation bindings (from `gleaph-codegen`) wrap `execute`/`executePreparedQuery`
and expose typed `*Params` / `*Row` shapes backed by the value types above.

## Development

- `pnpm check` — format, lint, and type checks
- `pnpm test` — conformance tests against the shared GQL value vectors
- `pnpm build` — build the library with `vp pack`
