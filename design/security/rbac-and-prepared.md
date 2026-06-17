# RBAC and prepared queries

## Purpose

Document Gleaph’s **in-canister access model** and how Prepared Queries fit the threat model.

## Non-goals

- IC canister controller privileges (platform-level; separate from RBAC).
- Frontend auth UX.

## Role hierarchy

**Source:** root `README.md`, `crates/auth`, `crates/router/src/rbac.rs`

Five levels (each includes lower):

| Role | Ad-hoc GQL | Prepared | Catalog / admin |
|------|------------|----------|-----------------|
| **Executor** | Prepared only | Yes | No |
| **Read** | Read-only programs | Yes | No |
| **Write** | + data modification, GQL catalog DDL (`CREATE`/`DROP` graph type, graph), `CALL` (conservative) | Yes | No |
| **Manager** | Same as Write | Yes | Capability bits (e.g. `PREPARE_REGISTER`) |
| **Admin** | Full | Yes | Grant roles |

Default: unknown principals are **Executor** until `admin_grant_role`.

## Classification pipeline

```mermaid
flowchart LR
    A[parse] --> B["classify_program<br/>gleaph-gql"]
    B --> C["authorize_adhoc_gql<br/>router"]
    C --> D[build_plan]
    D --> E["verify has_dml()"]
    E --> F[dispatch]
```

Write detection must agree between static classification and planner DML detection (`router/src/gql.rs`).

## Catalog DDL authorization

GQL catalog statements set `has_catalog_modification` in [`ProgramModificationFlags`](../../crates/gql/src/program_modification.rs) (`CREATE`/`DROP` graph, graph type, schema). Router enforcement:

| DDL surface | Entry | Minimum role / gate |
|-------------|-------|---------------------|
| **Graph type catalog** (`CREATE`/`DROP GRAPH TYPE`, `CREATE`/`DROP GRAPH` in `gql_execute*`) | `authorize_adhoc_gql` after `classify_program` | **Write** (includes `has_catalog_modification`) |
| **Index DDL** (`CREATE INDEX` / `DROP INDEX` standalone parse path) | `authorize_index_ddl` | **Controller** or Manager with **`PREPARE_REGISTER`** |
| **Prepared plan registry** | `authorize_prepared_catalog_change` | Admin or Manager with **`PREPARE_REGISTER`** |
| **Federation graph registration** | `admin_register_graph` | Admin (Candid admin API; separate from GQL catalog DDL) |

Graph type catalog DDL runs on the main GQL path **before** ingress dispatch when the transaction block contains catalog statements ([ADR 0013](../adr/0013-gql-graph-type-catalog-on-router.md)). Catalog-only blocks return zero rows without dispatching DML/query ops.

**Note:** Index DDL is **stricter** than graph type catalog DDL — Write alone is insufficient for index create/drop.

## Graph shard exposure

Graph canisters **do not** serve arbitrary GQL to end users. They execute:

- `ExecutePlanArgs` from router (trusted)
- Cross-shard graph endpoints (`federated_expand`, peer ACL) are **removed** until a follow-up ADR (router `peer_sync` is a no-op).
- Migration APIs (controlled)

This shrinks the attack surface: compromise of a user principal does not bypass router policy without also forging router calls.

## Prepared queries

**Product goal (README):** Admins register queries; frontends invoke them with parameters only.

Benefits:

- No arbitrary parse/plan on hot path for untrusted callers
- Stable plans for auditing and caching
- Combined with `IC.MSG_CALLER()` for row-level patterns

**Registration:** Manager with `PREPARE_REGISTER` or Admin (`README`).

**Implementation touchpoints:**

- `crates/router/src/prepared.rs`
- `crates/graph-prepared` (if present in workspace)
- Plan blob storage on router stable memory (`ROUTER_PREPARED_PLANS`, MemoryId 8); records are versioned (`PreparedPlanRecord::V1`)

## IC caller identity

GQL extensions:

- `IC.MSG_CALLER()` evaluated at execution time on graph
- Used in filters and prepared-query access patterns

Document query patterns that enforce “users see only their rows” in application guides (future).

## Federation and security

- Cross-shard expand requires peer graph principals in ACL.
- Router remains the entry for user GQL; shards trust router + peers, not arbitrary users.

## Related documents

- [architecture/overview.md](../architecture/overview.md)
- [gql/layers.md](../gql/layers.md)
