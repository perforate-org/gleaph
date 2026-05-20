# Gleaph

**Gleaph** is a graph database designed to run on the decentralized blockchain platform [Internet Computer](https://internetcomputer.org/).

Its internal storage model is based on LARA (Localized Adjacency Relocation Array), a graph representation derived from Compressed Sparse Row (CSR). LARA is specifically optimized for the execution environment of Internet Computer, allowing Gleaph to operate efficiently within the platform’s resource constraints.

## GQL (Graph Query Language)

Gleaph uses [GQL](https://www.gqlstandards.org/) (Graph Query Language, [ISO/IEC 39075](https://www.iso.org/standard/76120.html)) as its query language.

GQL is a standardized graph query language, comparable to SQL in relational databases, designed for querying, traversing, and analyzing graph data.

### `IC.PRINCIPAL` and `IC.MSG_CALLER()`

Gleaph extends GQL for Internet Computer by providing the `IC.PRINCIPAL` type and the `IC.MSG_CALLER()` function.

These extensions allow queries to directly access the caller’s Principal, making it easier to work with authentication and caller identity within Internet Computer applications.

## Prepared Query

**Prepared Query** is one of Gleaph’s core features.

It allows database administrators to pre-register queries that can be executed by unprivileged users, preventing arbitrary query execution and improving security.

This makes it possible for frontend applications to send queries directly to Gleaph safely, without requiring intermediary canisters or backend servers.

In combination with `IC.MSG_CALLER()`, Prepared Queries can also be used to implement access control patterns where users are only allowed to access data related to themselves.

## Access control (roles)

The **router** canister enforces a five-level role hierarchy (each level includes all lower levels): **Executor**, **Read**, **Write**, **Manager**, **Admin**.

Every **caller** is treated as **Executor** until a row exists in stable auth for that principal (for example after `admin_grant_role`). Administrators from router `init` (`issuing_principal` / `initial_admins`) are stored as **Admin**. Graph shards do not expose user GQL; they accept plan execution only from the router (or peer graph shards for federation).

- **Executor**: may execute **prepared** GQL only (including prepared updates).
- **Read**: may execute arbitrary **read-only** GQL programs (and prepared execution).
- **Write**: may execute programs that perform **data modification**, **catalog DDL** (`CREATE`/`DROP` graph and related statements), or contain a named `CALL` (treated conservatively as requiring write access until procedure semantics are modeled).
- **Manager**: same as Write, plus optional **capability bits** (for example `PREPARE_REGISTER`, future index DDL bits). Only **Admin** or a **Manager** with `PREPARE_REGISTER` may register or drop prepared queries; **Write** and below cannot.
- **Admin**: full access, including assigning roles via `admin_grant_role`.

Implementation overview:

- Crate [`crates/gleaph-auth`](crates/auth): stable RBAC types and [`AuthState`](crates/auth/src/lib.rs).
- [`gleaph_gql::program_modification`](crates/gql/src/program_modification.rs): static classification of a parsed program for read vs write paths.
- [`gleaph-router`](crates/router): stable auth map, `gql_*` / `prepared_*` entrypoints, and [`rbac`](crates/router/src/rbac.rs) checks in [`lib.rs`](crates/router/src/lib.rs).

**Internet Computer controllers** can still upgrade code or replace canister state; they are separate from in-canister roles.

## Design documentation

Architecture and GQL/federation design notes live in [`design/`](./design/). Start with [`design/README.md`](./design/README.md).

## License

This project is licensed under either of [Apache License, Version 2.0](./LICENSE-APACHE) or [MIT License](./LICENSE-MIT) at your option.
