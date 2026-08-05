# `gleaph.toml` — project configuration

A `gleaph.toml` at the project root pins the CLI's environment defaults (directory locations and
per-network deployment profiles) so remote commands run without repeating connection flags. The
file is entirely optional; absence means the built-in defaults below. The contract is defined by
[ADR 0062](../../../design/adr/0062-gleaph-toml-project-configuration.md).

## Discovery

The CLI walks up from the current working directory and uses the first `gleaph.toml` found. Set
`GLEAPH_CONFIG` to an explicit path to disable walk-up; a missing explicit path is an error. There
is no `--config` flag and no global (home-directory) config.

## Example

```toml
format_version = 1
default_network = "local"

[dirs]
migrations = "migrations"
prepared = "prepared"

[deployment.local]
canister = "rrkah-fqaaa-aaaaa-aaaaq-cai"
identity = ".icp/keys/deployer.pem"

[deployment.ic]
canister = "aaaaa-aa"
identity = ".icp/keys/ic-deployer.pem"

[deployment."https://example.com"]
canister = "aaaaa-aa"
identity = ".icp/keys/staging.pem"
fetch_root_key = true

[codegen]
target = "typescript"
output = "sdk/client/js/src/generated.ts"
graph = "my_graph"

[load]
graph = "my_graph"
key = "initial-load-v1"
state_file = ".load-state.json"
```

## Setting tables

| Table / key | Supplies the default for |
| --- | --- |
| `default_network` | `-n/--network` (flag > `GLEAPH_NETWORK` > this > `"ic"`) |
| `[dirs] migrations` | `gleaph migration --dir` |
| `[dirs] prepared` | `gleaph prepared --dir` |
| `[deployment.<network>] canister` | `--canister` on `migration status/apply`, `prepared status/apply/drop`, `load`, and the codegen remote source |
| `[deployment.<network>] identity` | `--identity` |
| `[deployment.<network>] fetch_root_key` | `--fetch-root-key` (custom-URL entries only, see below) |
| `[codegen] target` | `gleaph codegen --target` |
| `[codegen] output` | `gleaph codegen --output` |
| `[codegen] graph` | `gleaph codegen --graph` (remote source) |
| `[load] graph` | `gleaph load --graph` |
| `[load] key` | `gleaph load --key` |
| `[load] state_file` | `gleaph load --state-file` |

## Precedence

**CLI flag > `GLEAPH_*` environment variable > `gleaph.toml` > built-in default.**

| Variable | Overrides |
| --- | --- |
| `GLEAPH_CONFIG` | config file discovery |
| `GLEAPH_NETWORK` | effective network |
| `GLEAPH_CANISTER` | deployment `canister` |
| `GLEAPH_IDENTITY` | deployment `identity` |
| `GLEAPH_FETCH_ROOT_KEY` | deployment `fetch_root_key` (`true`/`false` only) |

Directory settings are not environment-overridable: they exist to be pinned by the file.

## Networks and `fetch_root_key`

- Deployment keys are `ic`, `local`, or an exact `http(s)://` URL (quoted TOML key). Unknown key
  shapes are rejected.
- The effective network (`-n` > `GLEAPH_NETWORK` > `default_network` > `"ic"`) selects the entry.
- **`fetch_root_key` is written only under custom-URL entries**; under `[deployment.ic]` /
  `[deployment.local]` it is a schema error, because their root-key behavior is fixed (`local`
  always fetches, `ic` never does). A URL entry omitting it fails with
  `a custom network URL requires --fetch-root-key`.
- The manifest source of `gleaph codegen` is never created by config: `--manifest` suppresses the
  deployment `canister`, and `--canister`/`--graph` are still required together when a remote
  source is selected.

## Paths and strictness

Relative paths in the file (`identity`, `[dirs]`, `[codegen] output`, `[load] state_file`) resolve
against the config file's directory; relative paths given as flags or environment variables
resolve against the current working directory. There is no `~` or shell expansion.

Parsing is fail-closed: unknown keys, unknown deployment network shapes, a `format_version` other
than `1`, and a non-boolean `GLEAPH_FETCH_ROOT_KEY` are all errors.

The config file is read by the `gleaph` binary only; the standalone `gleaph-codegen` binary
remains flag-only.
