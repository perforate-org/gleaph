# Gleaph CLI

The `gleaph` command-line tool drives the Gleaph canister stack from a developer
machine. It spans the whole lifecycle: local platform bring-up and Account
registration (`network`, `identity`, `login`, `signup`), typed client
generation for prepared queries (`codegen`), immutable schema migrations
(`migration`), initial data load through the durable Router bulk-load protocol
(`load`), derived embedding ingestion (`embed`), declarative data-plane grant
policies (`grants`), and fleet-level vector dispatch controls (`vector`).

The binary lives in this crate (`crates/cli`); the shared wire contracts it
speaks are owned by `gleaph-bulk-load-api` and `gleaph-migration-api`, so the
CLI does not link the Router canister crate.

## Build

```sh
cargo build -p gleaph-cli --release
# binary: target/release/gleaph
```

## Subcommands

| Command           | Purpose                                                                                        | Details                              |
| ----------------- | ---------------------------------------------------------------------------------------------- | ------------------------------------ |
| `gleaph network`  | Start/stop/status a local network; deploys the Account and Provision canisters                  | below                                |
| `gleaph identity` | Manage identities: `new`, `import` (PEM or icp-cli), `list`                                     | below                                |
| `gleaph login`    | Resolve the caller's principal and store the active session (PEM or web identity flow)          | below                                |
| `gleaph signup`   | Register a Personal Account for the caller's principal                                          | below                                |
| `gleaph codegen`  | Generate typed prepared-query clients and canister adapters from a Router manifest              | [`docs/codegen.md`](docs/codegen.md)     |
| `gleaph migration`| Create, validate, plan, and apply immutable schema migrations                                   | [`docs/migration.md`](docs/migration.md) |
| `gleaph prepared` | Scaffold, register, publish, and run prepared queries (`new/plan/status/apply/drop/publish/unpublish/run`) | [`docs/prepared.md`](docs/prepared.md)   |
| `gleaph load`     | Load initial vertices and edges into an existing logical graph                                  | [`docs/load.md`](docs/load.md)           |
| `gleaph embed`    | Ingest deterministic vertex embeddings into a registered vector index after a bulk load         | below                                |
| `gleaph grants`   | Apply the declarative data-plane grant policy (`GRANT`/`REVOKE` policy files)                   | below                                |
| `gleaph vector`   | Global vector-dispatch kill-switch (`activate` / `deactivate`)                                  | below                                |

### `gleaph migration`

Migrations are immutable packages: each is a directory named
`NNNNNN_slug` containing `migration.toml` (manifest) and one additive payload —
a single `up.gql` or a directory of `up/*.gql` fragments concatenated in sorted
filename order — chained into a linear parent order.

| Subcommand                    | Description                                                                                         |
| ----------------------------- | --------------------------------------------------------------------------------------------------- |
| `gleaph migration new <slug>` | Create and atomically publish the next migration package (`--description`, `--up` for custom bytes) |
| `gleaph migration plan`       | Validate and print the local migration chain without remote calls                                   |
| `gleaph migration status`     | Compare the local chain with Router's durable migration ledger                                      |
| `gleaph migration apply`      | Apply pending migrations through Router in parent order                                             |

Local subcommands take `--dir` (default `./migrations`). Remote subcommands
(`status`, `apply`) additionally require `--canister` and accept the shared
connection flags below.

### Platform and session commands

`gleaph network start` launches a local network from the project's `icp.yaml`,
deploys the Account and Provision canisters, and writes the canister mapping
under `.gleaph/`. In a repository checkout it builds the platform wasm set from
source when stale; `--platform-wasm-dir` supplies prebuilt artifacts instead and
seeds the Provision artifact catalog so the Router can be issued lazily. A
detached launcher runs by default (`--foreground` attaches and blocks);
`network stop` / `network status` manage a background network. `network start`
auto-registers the session's Personal Account unless `--no-auto-register`.

`gleaph identity new|import|list` manages PEM identities in the Gleaph identity
store; `import --from-icp <name>` adopts an icp-cli identity. `gleaph login`
resolves the caller's principal — from a stored session, `--identity <PEM>`, or
the browser Internet Identity flow (`--web`) — and stores it as the active
session. `gleaph signup` registers a Personal Account for the caller's
principal against the deployed Account canister; it requires a project config
(`gleaph.toml`) and a completed `network start`.

### Data-plane and vector operations

`gleaph grants apply` executes the declarative grant policy: every `*.gql` file
in the grants directory (default `grants/`; `--dir` or `gleaph.toml`
`[dirs] grants` override; `--file` for a single policy), in sorted filename
order. Each file must be authorization-only — `GRANT`/`REVOKE` statements; a
file mixing `MATCH`/`INSERT` with grants is rejected before any remote call —
and is applied idempotently: the mutation key derives from the file content, so
an unchanged re-apply replays the same scope. All files are validated before
any wire call, so a failing policy never applies partially.

`gleaph embed ingest` pushes deterministic vertex embeddings into a registered
vector index for a completed bulk load. It re-reads the load's vertices NDJSON
in the original row order, reconstructs the `source_id → vertex id` mapping
from the job's chunk receipts (the ground truth after budget-fitted commits),
and pages the ingestion through Router. `--vertices` and `--embeddings` are
required; `--key` selects the completed bulk-load job and `--graph` names the
logical graph holding the index.

`gleaph vector activate` / `deactivate` flips the Router's fleet-level vector
dispatch flag (default enabled; gated on the `MANAGE_FEDERATION`
administrative capability). It is incident-response state, not a setup step —
per-index dispatch readiness is the index lifecycle's job.

## Shared connection flags

Remote subcommands (`migration status`/`apply`, the remote `prepared`
subcommands, `load`, `embed ingest`, `vector activate`/`deactivate`, and
`grants apply`) share the same connection conventions:

| Flag                      | Meaning                                                                 |
| ------------------------- | ----------------------------------------------------------------------- |
| `--canister <PRINCIPAL>`  | Router canister principal                                               |
| `-n, --network <NETWORK>` | Network name (`ic` or `local`) or an HTTP(S) endpoint URL; default `ic` |
| `--identity <PATH>`       | PEM file containing a Secp256k1 identity                                |
| `--fetch-root-key`        | Fetch the network root key before querying a custom endpoint            |

`--canister` is required unless a config or `GLEAPH_CANISTER` supplies it.
When `--identity` is absent everywhere, the active session (from
`gleaph identity new` / `login` / `import`) signs the call.

These flags and the `--dir` defaults can be pinned per project in
[`gleaph.toml`](docs/config.md): per-network `[deployment.<network>]` profiles, `[dirs]`, and
`default_network`, with `GLEAPH_NETWORK` / `GLEAPH_CANISTER` / `GLEAPH_IDENTITY` /
`GLEAPH_FETCH_ROOT_KEY` / `GLEAPH_CONFIG` as per-machine overrides. The
standalone `gleaph-codegen` binary stays flag-only.

## Exit codes

| Code | Meaning                                                                                                          |
| ---- | ---------------------------------------------------------------------------------------------------------------- |
| 0    | Command completed (or skipped as already done)                                                                   |
| 1    | Operator action required (for example a terminal bulk-load job, a digest mismatch, or a general command failure) |
| 2    | Input validation failure; nothing was changed remotely (`gleaph load` artifact errors)                           |
| 3    | Remote/auth failure (`gleaph load`)                                                                              |

`gleaph load` and `gleaph embed` distinguish 1/2/3 precisely; other
subcommands currently report 0 on success and 1 on any failure.

## Detailed specifications

- [`docs/config.md`](docs/config.md) — `gleaph.toml` project configuration: discovery, setting
  tables, precedence, path resolution, and `fetch_root_key` rules.
- [`docs/load.md`](docs/load.md) — `gleaph load` artifact schema, flags,
  lifecycle, resume/skip semantics, streaming reads, and exit codes.
- [`docs/migration.md`](docs/migration.md) — `gleaph migration` package
  format, GQL dialect, chain rules, and remote apply semantics.
- [`docs/prepared.md`](docs/prepared.md) — `gleaph prepared` artifact format,
  registration lifecycle, publication, and the `run` surface.
- [`docs/codegen.md`](docs/codegen.md) — `gleaph codegen` manifest sources,
  targets, and flags; the generator itself is documented in
  [`gleaph-codegen`](../codegen/README.md).

The platform and session commands (`network`, `identity`, `login`, `signup`)
and the data-plane/vector commands (`grants`, `embed`, `vector`) are documented
in the sections above; there are no dedicated spec pages yet.
