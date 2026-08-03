# 0058. Versioned additive schema migrations

Date: 2026-08-02
Status: implemented
Last revised: 2026-08-02
Anchor timestamp: 2026-08-02 23:10:19 UTC +0000

## Context

Router owns graph-type and graph-schema catalogs, while operators need a reproducible way to
apply catalog changes across environments. Ad-hoc GQL calls do not provide a durable execution
identity, a parent-ordered history, or a replay proof. Embedding migration metadata in catalog
records would also couple schema data to deployment tooling and would not cover the exact bytes
sent for execution.

The project is pre-release. Public Candid, CLI, and development stable-memory formats may change
without compatibility wrappers. This ADR records the first narrow migration contract; it is not a
general-purpose migration framework.

## Problem

Schema changes need all of the following at one boundary:

- immutable, reviewable artifacts in source control;
- deterministic local discovery and parent-chain validation;
- an execution checksum that excludes filesystem and human-description noise;
- Router-owned authorization, catalog mutation, and durable applied history;
- exact replay of an already-applied request without duplicate catalog effects; and
- an explicit boundary for operations that are not schema migrations.

## Existing architecture and ownership

The top-level `gleaph` CLI owns filesystem artifacts, strict TOML/GQL validation, chain ordering,
checksum calculation, and the IC-agent transport adapter. The shared wire records and checksum
function live in `crates/migration-api/src/lib.rs`; this crate has no filesystem or Router
storage dependency. The Router control API exposes `apply_schema_migration` (update) and
`list_schema_migrations` (query). Router store code owns authorization, statement validation,
catalog application, and the durable ledger in `ROUTER_SCHEMA_MIGRATIONS` (MemoryId 50).

This split keeps the Router ledger as the sole source of truth for applied migrations. The CLI
compares a local parent-ordered plan with the Router prefix before applying anything; it does not
write Router stable memory directly or reimplement catalog ownership.

## Decision

### Artifact format (v1)

Each migration is one directory named `<six-digit-sequence>_<lowercase-slug>` under the migration
root (`./migrations` by default). The directory must contain exactly two regular files:
`migration.toml` and `up.gql`; symlinks, extra files, and nested directories are rejected.

`migration.toml` is strict TOML (`deny_unknown_fields`) with these fields:

```toml
format_version = 1
id = "000001_init_graph"
description = "Initial graph schema"
# parent = "000000_previous"   # omitted for the unique root
```

`id` must match the directory name. `parent` is omitted for the root (TOML has no null literal) and
must name the immediate prior migration otherwise. Description text is human metadata and is not
part of execution identity. `up.gql` must be UTF-8, LF-terminated, and contain exactly one
statement.

The v1 dialect accepts only one additive catalog statement:

- `CREATE GRAPH TYPE <name> { ... }`; or
- `CREATE GRAPH <name> TYPED <type>` with literal names and a literal typed schema.

Parameters, `SESSION`, transaction control, `NEXT` chains, `IF NOT EXISTS`, `OR REPLACE`, and
`COPY` are rejected. `INDEX`, backfill, DML, stable-memory, and federation operations are not
migrations under v1.

### Chain and checksum

The local CLI and Router require one globally linear root-to-head chain with unique numeric
prefixes that strictly increase from the parent. Router rejects a second root, a missing parent,
a parent other than the current head, forks, cycles, disconnected records, and a ledger over
`MAX_SCHEMA_MIGRATIONS = 4096` records. `list_schema_migrations` returns canonical parent order
with an exclusive cursor and a limit from 1 through 16.

`gleaph_migration_api::schema_migration_checksum` is the checksum source of truth. It computes a
domain-separated SHA-256 over framed fields in this order:

The domain bytes are fed first and are not length-framed. Every `frame` is an unsigned 64-bit
big-endian byte length followed by the framed bytes. The checksum stream is:

1. `frame(1_u32.to_be_bytes())` (four-byte big-endian version payload);
2. `frame(migration_id UTF-8 bytes)`;
3. one raw parent-present marker byte (`0` or `1`), followed when present by
   `frame(parent UTF-8 bytes)`; and
4. `frame(raw up.gql UTF-8 bytes)`, including comments, whitespace, and its final LF.

TOML formatting, description, directory paths, and parsed AST formatting are excluded. Router
recomputes this digest for new ids before any catalog or ledger mutation; existing ids are first
classified as exact replay or payload conflict so a reused id cannot be masked by a malformed
replacement checksum.

### Router apply and replay

`apply_schema_migration` is admin-only and rejects the anonymous principal. It validates the v1
request envelope, parses and profiles the statement, verifies the checksum, validates the chain,
and applies the catalog statement. The immutable `SchemaMigrationRecord::V1` stores `id`,
`parent`, `checksum`, `actor`, `applied_at`, exact `statement`, and the derived statement profile.

Catalog mutation and ledger insertion are one synchronous Router update boundary. An unexpected
catalog error traps so the IC message rolls back both writes; there is no inter-canister `await`
between them. Reapplying the same id with the same parent, checksum, and exact statement returns
`Replay` with the existing record and performs no catalog mutation. Reusing an id with a different
payload returns a conflict.

`list_schema_migrations` reconstructs root-to-head order from the canonical parent links on each
bounded operation. The ledger map, not a denormalized head or child index, is the persistence
source of truth.

### CLI commands

The implemented command surface is:

```text
gleaph migration new <SLUG> [--dir PATH] [--description TEXT] [--up PATH]
gleaph migration plan [--dir PATH]
gleaph migration status [--dir PATH] --canister PRINCIPAL [-n ic|local|URL] [--identity PEM] [--fetch-root-key]
gleaph migration apply  [--dir PATH] --canister PRINCIPAL [-n ic|local|URL] [--identity PEM] [--fetch-root-key]
```

`new` creates the next package through a same-filesystem temporary directory and rename. `plan`
performs local validation only. `status` lists the Router ledger page by page and verifies that
it is a checksum-matching prefix of the local chain. `apply` performs the same preflight, then
submits only the remaining parent-ordered artifacts. `-n ic` is the default; `local` fetches the
local root key; custom HTTP(S) endpoints require `--fetch-root-key`. `status` only reads the
Router ledger and does not require an admin identity; `apply` requires an admin-capable caller.
The CLI uses an optional Secp256k1 PEM identity and never constructs or mutates Router stable
structures itself.

## Alternatives considered

| Alternative | Decision and reason |
| --- | --- |
| Embed migration metadata in graph-type catalog rows | Rejected: couples deployment history to schema records and cannot commit an exact artifact checksum or actor independently. |
| Accept arbitrary GQL or a generic step list | Rejected for v1: broadens authorization and replay semantics to DML, indexes, and maintenance without a bounded invariant. |
| Store a local-only file and call ordinary GQL | Rejected: no Router-owned applied ledger, authorization boundary, or atomic catalog/history co-write. |
| Separate `migration.toml` plus pure `up.gql` with a Router ledger | Chosen: reviewable source artifacts, exact-byte identity, narrow parser contract, and clear CLI/Router ownership. |

## Consequences

Positive:

- Applied schema history is durable and auditable in Router stable memory.
- Exact replay is safe and deterministic, including after a lost client response.
- Local validation is testable without an IC agent, while Router remains the catalog and invariant
  owner.
- The narrow dialect prevents accidental use of migration tooling for data movement or physical
  layout changes.

Costs and limits:

- v1 supports only one additive catalog statement per artifact and one global linear chain.
- There is no `down`/rollback command. Reversal requires a new forward migration or a future ADR.
- `status` requires Router transport; `apply` additionally requires an admin-capable identity. No
  offline remote mode is implied.
- Development stable-memory compatibility is not provided when the Router layout changes; the
  MemoryId 50 addition follows ADR 0007's layout policy.

## Future work (planned)

- Add focused PocketIC coverage for authorization, apply/replay/conflict, pagination, upgrade
  reopen, and catalog/ledger rollback behavior; this is validation work, not an unimplemented
  migration feature.
- Define a separate ADR before introducing destructive schema changes, multi-step transactions,
  data backfills, index migrations, stable-memory migrations, federation changes, or a `down`
  workflow.

## References

- [ADR 0007 — Stable-memory layout policy](0007-stable-memory-layout.md)
- [Stable-memory inventory](../storage/stable-memory-inventory.md)
- `crates/migration-api/src/lib.rs`
- `crates/cli/src/migration.rs`
- `crates/router/src/facade/store/schema_migration.rs`
