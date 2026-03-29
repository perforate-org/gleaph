# @gleaph/sdk

TypeScript SDK for [Gleaph](https://github.com/perforate-org/gleaph) — a multi-tenant graph database on the Internet Computer.

## Install

```bash
pnpm add @gleaph/sdk @icp-sdk/core

# For browser auth with Internet Identity (optional)
pnpm add @icp-sdk/auth
```

## Quick Start

```ts
import { GleaphClient } from "@gleaph/sdk";

const client = new GleaphClient();
const graph = client.graph("bkyz2-fmaaa-aaaaa-qaaaq-cai");

// GQL query
const result = await graph.query(
  `MATCH (u:User {name: $name})-[:FOLLOWS]->(f) RETURN f.name`,
  { name: "Alice" },
);
console.log(result.result.columns); // ["f.name"]
console.log(result.result.rows); // [[{ Text: "Bob" }], ...]

// GQL mutation
await graph.mutate(`CREATE (:User {name: $name, age: $age})`, {
  name: "Carol",
  age: 28,
});
```

## Authentication

All auth-related imports come from `@gleaph/sdk/auth`, which re-exports
types and classes from `@icp-sdk/core` and `@icp-sdk/auth`. If your
application already uses those packages, the same `Identity` instances
work with `GleaphClient`.

### Anonymous (no auth)

```ts
import { GleaphClient } from "@gleaph/sdk";
const client = new GleaphClient();
```

### Keypair (server-side / testing)

Requires only `@icp-sdk/core` (peer dependency).

```ts
import { GleaphClient } from "@gleaph/sdk";
import { Ed25519KeyIdentity } from "@gleaph/sdk/auth";

const identity = Ed25519KeyIdentity.generate();
const client = new GleaphClient({ identity });
```

### Internet Identity (browser)

Requires `@icp-sdk/auth` (optional peer dependency).

```ts
import { GleaphClient } from "@gleaph/sdk";
import { AuthClient } from "@gleaph/sdk/auth";

let client: GleaphClient | undefined;

const auth = await AuthClient.create();
await auth.login({
  identityProvider: "https://identity.ic0.app",
  onSuccess: () => {
    client = new GleaphClient({
      identity: auth.getIdentity(),
    });
  },
});
```

### Using `@icp-sdk/*` directly

Imports from `@gleaph/sdk/auth` are identical to those from `@icp-sdk/core`
and `@icp-sdk/auth`. You can use either interchangeably:

```ts
// These produce the exact same Identity type:
import { Ed25519KeyIdentity } from "@gleaph/sdk/auth";
import { Ed25519KeyIdentity } from "@icp-sdk/core/identity";
```

## Local Development

```ts
const client = new GleaphClient({
  host: "http://127.0.0.1:4943",
  fetchRootKey: true,
});
await client.ready();
```

## API

### `GleaphClient`

| Method                 | Description                                                |
| ---------------------- | ---------------------------------------------------------- |
| `graph(canisterId)`    | Returns a `GraphClient` for the given graph canister       |
| `registry(canisterId)` | Returns a `RegistryClient` for the given registry canister |
| `ready()`              | Resolves when root key is fetched (local dev only)         |

### `GraphClient`

#### GQL

| Method                   | Description                                            |
| ------------------------ | ------------------------------------------------------ |
| `query(gql, params?)`    | Execute a read-only GQL statement                      |
| `queryAll(gql, params?)` | Execute a query, auto-paginating all continuation pages |
| `mutate(gql, params?)`   | Execute a GQL mutation (returns continuation support)  |
| `batchMutate(gqls)`      | Execute multiple mutations in a single call            |

`mutate()` returns `MutationResultWithContinuation` — large DELETE operations
may include a continuation token for resuming via `mutate_continue`.

`batchMutate()` accepts strings or `[gql, params]` tuples:

```ts
await graph.batchMutate([
  "CREATE (:User {name: 'Alice'})",
  ["CREATE (:User {name: $n})", { n: "Bob" }],
]);
```

#### Prepared Statements

| Method                                   | Description                  |
| ---------------------------------------- | ---------------------------- |
| `prepare(name, gql, options?)`           | Register a prepared statement |
| `executePrepared(name, params?, sort?)`  | Execute a prepared query     |
| `executePreparedMutation(name, params?)` | Execute a prepared mutation  |
| `dropPrepared(name)`                     | Remove a prepared statement  |
| `listPrepared()`                         | List all prepared statements |

Dynamic sort example:

```ts
await graph.prepare(
  "list_users",
  "MATCH (u:User) RETURN u.name AS name, u.age AS age",
  {
    description: "List users with optional filtering and sortable results.",
    allowed_sorts: [
      { key: "name", expr: "u.name" },
      { key: "age", expr: "u.age" },
    ],
    default_sort: [{ key: "age", descending: true }],
  },
);

const result = await graph.executePrepared(
  "list_users",
  {},
  [{ key: "name", descending: false }],
);

const prepared = await graph.listPrepared();
console.log(prepared[0].parameters); // [{ name: "min_age", required: false }, ...]
console.log(prepared[0].description);
console.log(prepared[0].allowed_sorts);
console.log(prepared[0].default_sort);
```

#### Graph Algorithms

| Method                    | Description                             |
| ------------------------- | --------------------------------------- |
| `bfs(start, config?)`     | Breadth-first search                    |
| `sssp(start, config?)`    | Single-source shortest path             |
| `pagerank(config)`        | PageRank computation                    |
| `recommend(user, config)` | Collaborative filtering recommendations |

All algorithm methods return `WithContinuation` types — large results may
include a continuation token for pagination.

#### Low-Level Operations

| Method                              | Description              |
| ----------------------------------- | ------------------------ |
| `addVertex(vertex)`                 | Add a single vertex      |
| `addEdge(edge)`                     | Add a single edge        |
| `bulkInsertVertices(vertices)`      | Bulk insert vertices     |
| `bulkInsertEdges(edges)`            | Bulk insert edges        |
| `createIndex(entityType, property)` | Create an equality index |

#### Stats & Diagnostics

| Method                       | Description                   |
| ---------------------------- | ----------------------------- |
| `getStats()`                 | Graph statistics              |
| `getStatsCertified()`        | Certified graph statistics    |
| `getPlannerStats()`          | Query planner statistics      |
| `computeGraphStats()`        | Recompute planner statistics  |
| `getNeighbors(vertexId)`     | Get neighbors of a vertex     |
| `getPagerankCertified(hash)` | Get certified PageRank result |

### `RegistryClient`

| Method                                   | Description                                   |
| ---------------------------------------- | --------------------------------------------- |
| `createGraph(config)`                    | Create a new graph canister                   |
| `deleteGraph(id)`                        | Delete a graph                                |
| `listGraphs()`                           | List all graphs accessible to the caller      |
| `grantAccess(graphId, principal, level)` | Grant `"read"` / `"write"` / `"admin"` access |

## Subpath Exports

| Import path        | Contents                           | Required dependency |
| ------------------ | ---------------------------------- | ------------------- |
| `@gleaph/sdk`      | `GleaphClient`, `GraphClient`, types, `Identity` type | `@icp-sdk/core`    |
| `@gleaph/sdk/auth` | `AuthClient`, identity classes, `Principal` | `@icp-sdk/auth` (optional) |

## Parameterized Queries

Pass parameters as a plain object — the SDK converts JS values to Candid `Value` variants automatically:

| JS Type              | Candid Value |
| -------------------- | ------------ |
| `null` / `undefined` | `Null`       |
| `boolean`            | `Bool`       |
| `number`             | `Float`      |
| `bigint`             | `Int`        |
| `string`             | `Text`       |
| `Uint8Array`         | `Bytes`      |
| `Array`              | `List`       |

For explicit control, pass a `Value` variant directly:

```ts
await graph.query(`MATCH (e:Event) WHERE e.ts > $since RETURN e`, {
  since: { Timestamp: 1700000000000000n },
});
```

## Error Handling

All canister errors throw a `GleaphError` with a `code` property:

```ts
import { GleaphError } from "@gleaph/sdk";

try {
  await graph.query("INVALID GQL");
} catch (e) {
  if (e instanceof GleaphError) {
    console.error(e.code); // "ParseError", "ValidationError", etc.
    console.error(e.detail); // error details from the canister
  }
}
```

## Build

```bash
pnpm install
pnpm build   # tsc → dist/
pnpm lint    # biome check
pnpm check   # tsc --noEmit
```

## License

MIT
