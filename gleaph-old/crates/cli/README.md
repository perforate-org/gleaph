# gleaph CLI

Code generation tool for [Gleaph](https://github.com/nicksrandall/gleaph) graph database. Connects to a graph canister, reads registered prepared statements, and generates type-safe client code.

## Install

```bash
cargo install --path crates/cli
```

Or build without installing:

```bash
make cli   # builds release binary at target/release/gleaph
```

## Usage

### `gleaph codegen`

Generate client code from prepared statements registered on a canister.

```bash
# JS (default) — outputs .js + .d.ts pair
gleaph codegen -c bkyz2-fmaaa-aaaaa-qaaaq-cai

# JS with original GQL parameter names
gleaph codegen -c bkyz2-fmaaa-aaaaa-qaaaq-cai --js-param-style preserve

# TypeScript — single .ts file
gleaph codegen -c bkyz2-fmaaa-aaaaa-qaaaq-cai --lang ts

# Rust — single .rs file with GraphOps trait
gleaph codegen -c bkyz2-fmaaa-aaaaa-qaaaq-cai --lang rust

# Custom output path
gleaph codegen -c bkyz2-fmaaa-aaaaa-qaaaq-cai -o src/generated/gleaph.js

# Multiple languages to a directory
gleaph codegen -c bkyz2-fmaaa-aaaaa-qaaaq-cai --lang js --lang rust -o src/generated/

# Local replica
gleaph codegen -c bkyz2-fmaaa-aaaaa-qaaaq-cai --local
```

#### Options

| Flag | Description |
|------|-------------|
| `-c`, `--canister`, `--canister-id` | Graph canister principal (required) |
| `--lang <LANG>` | Target language: `js` (default), `ts`, `rust` |
| `--js-param-style <STYLE>` | JS/TS parameter names: `camel` (default) or `preserve` |
| `-o`, `--output <PATH>` | Output file or directory |
| `--local` | Use local replica (`http://127.0.0.1:4943`) |
| `--host <URL>` | Custom IC host URL |

#### Default output filenames

| Language | Filename |
|----------|----------|
| `js` | `gleaph.generated.js` + `gleaph.generated.d.ts` |
| `ts` | `gleaph.generated.ts` |
| `rust` | `gleaph_prepared.rs` |

### `gleaph list`

List prepared statements on a canister in a human-readable table.

```bash
gleaph list -c bkyz2-fmaaa-aaaaa-qaaaq-cai
```

```
NAME                     KIND       CALLER   PARAMETERS               COLUMNS                  SORTS                        DEFAULT SORT
----------------------------------------------------------------------------------------------------------------------------------------------------------------
get_user                 query      no       name                     name, age                name (u.name), age (u.age)  age DESC
follow                   mutation   yes      target                   (none)                   (none)                       (none)
my_posts                 query      yes      (none)                   title, body              created_at (p.created_at)    created_at DESC
```

## Generated code

### JavaScript (.js + .d.ts)

The `.js` file contains a plain factory function with JSDoc annotations. The `.d.ts` file provides full type information for IDE support.

```javascript
// gleaph.generated.js
/** @param {import("@gleaph/sdk").GraphClient} graph */
export function createPreparedClient(graph) {
  return {
    get_user: (params) =>
      graph.executePrepared("get_user", { user_name: params.userName }),
    follow: (params) =>
      graph.executePreparedMutation("follow", params),
  };
}
```

```typescript
// gleaph.generated.d.ts
import type {
  GraphClient,
  MutationResult,
  PreparedSortSpec,
  QueryResultWithContinuation,
} from "@gleaph/sdk";

export declare const GetUserSortKey: {
  /** wire key: "user_name", expr: "u.user_name" */
  readonly UserName: "user_name";
  /** wire key: "age", expr: "u.age" */
  readonly Age: "age";
};

export interface GetUserParams {
  /** wire param: "user_name" */
  userName: unknown;
}

export type GetUserSortKey =
  (typeof GetUserSortKey)[keyof typeof GetUserSortKey];

export type GetUserSortSpec = Omit<PreparedSortSpec, "key"> & {
  key: GetUserSortKey;
};

export interface GleaphPrepared {
  /**
   * Fetch one user by exact username.
   */
  get_user(
    params: GetUserParams,
    options?: { sort?: GetUserSortSpec[] },
  ): Promise<QueryResultWithContinuation>;
  follow(params: { target: unknown }): Promise<MutationResult>;
}

export declare function createPreparedClient(graph: GraphClient): GleaphPrepared;
```

Usage:

```typescript
import { GleaphClient } from "@gleaph/sdk";
import { GetUserSortKey, createPreparedClient } from "./gleaph.generated.js";

const client = new GleaphClient({ host: "https://icp-api.io" });
const graph = client.graph("bkyz2-fmaaa-aaaaa-qaaaq-cai");
const prepared = createPreparedClient(graph);

const result = await prepared.get_user(
  { userName: "Alice" },
  { sort: [{ key: GetUserSortKey.Age, descending: true }] },
);
```

By default, JS/TS codegen exposes `camelCase` parameter names while preserving the original GQL wire names internally. For a query that uses `$user_name`, the generated API takes `{ userName: ... }` and sends `{ user_name: ... }` to the canister. Use `--js-param-style preserve` if you want the generated API to keep the original names.

Parameterized queries use named params types, with inline docs for the original wire parameter names:

```ts
export interface FindUserParams {
  /** wire param: "user_name" */
  userName: unknown;
}
```

Parameters annotated as `:: ... | NULL` are generated as optional fields in JS/TS params types.

Sort key constants follow a stable export rule:

- the generated constant is `<PreparedName>SortKey` in `PascalCase`
- each sort key property is converted to `PascalCase`
- non-alphanumeric characters are treated as separators
- keys that start with a digit get a `K` prefix, for example `"1day"` becomes `K1day`
- if two keys normalize to the same property name, later ones get a numeric suffix such as `UserName2`

Examples:

```ts
FindUserSortKey.UserName   // "user_name"
FindUserSortKey.K1day      // "1day"
FindUserSortKey.UserName2  // "user-name"
```

Each generated sort key constant also carries inline docs for the original key and expression:

```ts
export declare const FindUserSortKey: {
  /** wire key: "user_name", expr: "u.user_name" */
  readonly UserName: "user_name";
};
```

### Rust (.rs)

The generated file defines a `GraphOps` trait and a `PreparedClient` wrapper. Implement the trait for your agent or inter-canister client.

```rust
// gleaph_prepared.rs
pub trait GraphOps {
    async fn execute_prepared(
        &self,
        name: &str,
        params: Vec<(&str, Value)>,
        sort: Option<Vec<PreparedSortSpec>>,
    ) -> Result<QueryResult, GleaphError>;
    async fn execute_prepared_mutation(&self, name: &str, params: Vec<(&str, Value)>) -> Result<MutationResult, GleaphError>;
}

pub struct PreparedClient<'a, C: GraphOps> { ... }

pub struct GetUserParams {
    /// wire param: "user_name"
    pub user_name: Value,
}

impl GetUserParams {
    pub fn new(user_name: impl Into<Value>) -> Self { ... }
    pub fn builder(user_name: impl Into<Value>) -> GetUserParamsBuilder { ... }
}

pub struct SearchParamsBuilder { ... }

impl SearchParamsBuilder {
    pub fn offset(self, value: impl Into<Value>) -> Self { ... }
    pub fn city(self, value: impl Into<Value>) -> Self { ... }
    pub fn build(self) -> SearchParams { ... }
}

pub enum GetUserSortKey {
    /// wire key: "user_name"
    /// expr: "u.user_name"
    UserName,
    /// wire key: "age"
    /// expr: "u.age"
    Age,
}

pub struct GetUserSortSpec {
    pub key: GetUserSortKey,
    pub descending: bool,
    pub nulls_first: Option<bool>,
}

impl GetUserSortSpec {
    pub fn asc(key: GetUserSortKey) -> Self { ... }
    pub fn desc(key: GetUserSortKey) -> Self { ... }
    pub fn with_nulls_first(self, nulls_first: bool) -> Self { ... }
}

impl<'a, C: GraphOps> PreparedClient<'a, C> {
    pub async fn get_user(
        &self,
        params: GetUserParams,
        sort: Option<Vec<GetUserSortSpec>>,
    ) -> Result<QueryResult, GleaphError> { ... }
}
```

Rust codegen emits named params structs and sort enums/spec structs, with doc comments for the original wire names and bound expressions.
Parameters annotated as `:: ... | NULL` are generated as `Option<Value>` fields and exposed via fluent builder setters.

That lets call sites stay compact:

```rust
let result = client
    .get_user(
        GetUserParams::new("alice"),
        Some(vec![GetUserSortSpec::desc(GetUserSortKey::Age)]),
    )
    .await?;

let search = SearchParams::builder("alice")
    .city("Tokyo")
    .build();
```

Prepared statements with dynamic sort metadata produce generated docs that include:

- allowed sort keys and the bound GQL expressions
- the default sort, when one is configured
- typed `sort` options for generated query helpers

## Prerequisites

The canister must have prepared statements registered via the `prepare` endpoint before running codegen. The CLI uses anonymous identity for the `list_prepared` query call — no wallet or identity file is needed unless the canister restricts read access.
