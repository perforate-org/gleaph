# Gleaph CLI

The `gleaph` command-line tool drives the Gleaph canister stack from a developer
machine: it generates typed clients for prepared queries, validates and applies
immutable schema migrations, and loads initial graph data through the durable
Router bulk-load protocol.

The binary lives in this crate (`crates/cli`); the shared wire contracts it
speaks are owned by `gleaph-bulk-load-api` and `gleaph-migration-api`, so the
CLI does not link the Router canister crate.

## Build

```sh
cargo build -p gleaph-cli --release
# binary: target/release/gleaph
```

## Subcommands

| Command            | Purpose                                                                            | Details                                  |
| ------------------ | ---------------------------------------------------------------------------------- | ---------------------------------------- |
| `gleaph codegen`   | Generate typed prepared-query clients and canister adapters from a Router manifest | [`docs/codegen.md`](docs/codegen.md)     |
| `gleaph migration` | Create, validate, plan, and apply immutable schema migrations                      | [`docs/migration.md`](docs/migration.md) |
| `gleaph prepared`  | Register prepared queries from local `.gql` files (plan/status/apply/drop)         | [`docs/prepared.md`](docs/prepared.md)   |
| `gleaph load`      | Load initial vertices and edges into an existing logical graph                     | [`docs/load.md`](docs/load.md)           |

### `gleaph migration`

Migrations are immutable packages: each is a directory named
`NNNNNN_slug` containing exactly `migration.toml` (manifest) and `up.gql`
(schema change), linked into a linear chain by parent ids.

| Subcommand                    | Description                                                                                         |
| ----------------------------- | --------------------------------------------------------------------------------------------------- |
| `gleaph migration new <slug>` | Create and atomically publish the next migration package (`--description`, `--up` for custom bytes) |
| `gleaph migration plan`       | Validate and print the local migration chain without remote calls                                   |
| `gleaph migration status`     | Compare the local chain with Router's durable migration ledger                                      |
| `gleaph migration apply`      | Apply pending migrations through Router in parent order                                             |

Local subcommands take `--dir` (default `./migrations`). Remote subcommands
(`status`, `apply`) additionally require `--canister` and accept the shared
connection flags below.

## Shared connection flags

Remote subcommands (`migration status`, `migration apply`, `load`, and
`codegen --canister`) share the same connection conventions:

| Flag                      | Meaning                                                                 |
| ------------------------- | ----------------------------------------------------------------------- |
| `--canister <PRINCIPAL>`  | Router canister principal                                               |
| `-n, --network <NETWORK>` | Network name (`ic` or `local`) or an HTTP(S) endpoint URL; default `ic` |
| `--identity <PATH>`       | PEM file containing a Secp256k1 identity                                |
| `--fetch-root-key`        | Fetch the network root key before querying a custom endpoint            |

## Exit codes

| Code | Meaning                                                                                                          |
| ---- | ---------------------------------------------------------------------------------------------------------------- |
| 0    | Command completed (or skipped as already done)                                                                   |
| 1    | Operator action required (for example a terminal bulk-load job, a digest mismatch, or a general command failure) |
| 2    | Input validation failure; nothing was changed remotely (`gleaph load` artifact errors)                           |
| 3    | Remote/auth failure (`gleaph load`)                                                                              |

`gleaph load` distinguishes 1/2/3 precisely; other subcommands currently report
0 on success and 1 on any failure.

## Detailed specifications

- [`docs/load.md`](docs/load.md) — `gleaph load` artifact schema, flags,
  lifecycle, resume/skip semantics, streaming reads, and exit codes.
- [`docs/migration.md`](docs/migration.md) — `gleaph migration` package
  format, GQL dialect, chain rules, and remote apply semantics.
- [`docs/codegen.md`](docs/codegen.md) — `gleaph codegen` manifest sources,
  targets, and flags; the generator itself is documented in
  [`gleaph-codegen`](../codegen/README.md).
