# 0062. Project configuration file (`gleaph.toml`) for CLI environment defaults

This ADR defines the project-scoped `gleaph.toml` configuration file: discovery rules, the setting
vocabulary (directory defaults, per-network deployment profiles, codegen/load defaults), precedence
over built-in defaults and environment variables, and path-resolution semantics. It complements the
ADR 0061 command surface by making the remote subcommands reproducible without repeating connection
flags on every invocation.

Date: 2026-08-05
Status: accepted
Last revised: 2026-08-05
Anchor timestamp: 2026-08-05 13:31:38 UTC +0000

## Context

Every remote CLI subcommand repeats the full connection flag set on each invocation:

- `migration status` / `migration apply` require `--canister` and accept `-n/--network`
  (default `ic`), `--identity`, `--fetch-root-key` (`RemoteMigrationArgs`).
- `prepared status` / `prepared apply` / `prepared drop` repeat the same shape
  (`RemotePreparedArgs`).
- `load` repeats it and additionally takes `--graph`, `--key`, `--state-file`.
- `codegen --canister` repeats it and requires `--graph`, `--target`, with optional `--output`.

Directory defaults are working-directory-relative: `MigrationDirArgs.dir` defaults to
`"migrations"` and `PreparedDirArgs.dir` to `"prepared"`, so command outcomes depend on where the
shell happens to be. The e2e registration script (`scripts/check-codegen-local-e2e.sh`) hardcodes
the whole flag set — `--dir`, `--canister`, `-n local`, `--identity` — and a long identity PEM
path derived from the `icp` CLI home.

The project already uses strict, `deny_unknown_fields` TOML artifacts (`migration.toml`, prepared
sidecars, ADR 0061 §2) and a project-scoped YAML configuration precedent (`icp.yaml` for the `icp`
CLI). There is no Gleaph CLI configuration file today; every option is clap-level with a hardcoded
default.

## Problem

Remote and directory settings are not pinned to a project. Reproducing a workflow (registration,
migrations, load, codegen) requires restating `--canister -n --identity` and `--dir` by hand or in
a script, and directory defaults silently depend on the current working directory. There is no
single place that fixes "this project talks to this deployment on this network, with this
identity".

## Existing architecture and ownership

- **gleaph-cli** owns clap argument parsing (`main.rs`), the IC-agent transport
  (`remote.rs` `RemoteTransport::connect(canister, network, identity, fetch_root_key)`), and the
  pure subcommand modules (`migration.rs`, `prepared.rs`, `load.rs`). `migration.rs` and
  `prepared.rs` already parse strict TOML with `serde(deny_unknown_fields)`.
- **gleaph-codegen** owns `CodegenArgs` and `run()`; the `gleaph` binary dispatches to it
  (`TopLevelCommand::Codegen`). The standalone `gleaph-codegen` binary parses the same args.
- **Network resolution** lives in `remote.rs` `resolve_network`: `"ic"` → `https://icp-api.io`
  (no root-key fetch), `"local"` → `http://localhost:8000` (always fetch root key), a custom
  `http(s)://` URL requires the effective `fetch_root_key` flag. `codegen/src/cli.rs` mirrors this
  resolution for `list_prepared` queries.
- **Remote commands** validate `--canister` as clap-required today; `codegen --target` is required
  and `--canister`/`--graph` must be given together (`IncompleteRemoteSource`),
  `--manifest`/`--canister` are mutually exclusive (`ConflictingManifestSources`).

## Decision

### 1. File location and discovery

A project places **`gleaph.toml`** at its root. Discovery walks up from the current working
directory and uses the first file found (the same convention as `dfx.json` and cargo
configuration). The `GLEAPH_CONFIG` environment variable selects an explicit path and disables
walk-up; the file need not exist relative to the current directory, and a value pointing to a
missing file is an error (no walk-up fallback). A `--config` global flag is intentionally not
added in v1; `GLEAPH_CONFIG` is the only explicit-path mechanism. No global (home-directory)
configuration in v1.

### 2. File format and strictness

Strict TOML with `serde(deny_unknown_fields)` on every table, matching the migration manifest and
prepared sidecar conventions:

- `format_version` is optional and defaults to `1`; any other value is rejected with a clear error
  (fail-closed, forward-compatible).
- Unknown top-level keys, unknown table keys, and unknown deployment network names are rejected
  rather than silently ignored (typo protection).
- The file is entirely optional: absence means built-in defaults.

### 3. Setting vocabulary

```toml
format_version = 1

# Default for `-n/--network` when the flag is absent; built-in default remains "ic".
default_network = "local"

[dirs]
migrations = "migrations"   # default for `gleaph migration --dir`
prepared = "prepared"       # default for `gleaph prepared --dir`

[deployment.local]          # selected when the effective network is "local"
canister = "rrkah-fqaaa-aaaaa-aaaaq-cai"
identity = ".icp/keys/deployer.pem"
# fetch_root_key is not written for "ic"/"local": their root-key behavior is fixed.

[deployment.ic]             # selected when the effective network is "ic"
canister = "aaaaa-aa"
identity = ".icp/keys/ic-deployer.pem"

[deployment."https://example.com"]   # selected when the effective network is this URL
canister = "aaaaa-aa"
identity = ".icp/keys/staging.pem"
fetch_root_key = true

[codegen]
target = "typescript"       # default for `--target`
output = "sdk/client/js/src/generated.ts"
graph = "my_graph"          # default for `--graph` (remote source)

[load]
graph = "my_graph"
key = "initial-load-v1"
state_file = ".load-state.json"
```

- **`default_network`** (top-level): the effective network is `-n/--network` flag, else
  `GLEAPH_NETWORK`, else `default_network`, else `"ic"`. Any value the flag accepts (`ic`, `local`,
  or an `http(s)://` URL) is allowed; a URL default still requires the effective
  `fetch_root_key` via `resolve_network`.
- **`[dirs]`**: `migrations`, `prepared` — defaults for the corresponding `--dir` flags.
- **`[deployment.<network>]`**: `canister`, `identity`, and — for custom-URL networks only —
  `fetch_root_key`. The network name is the effective-network value: `ic`, `local`, or an exact
  `http(s)://` URL (quoted TOML key). Entries are self-contained — there is no base `[deployment]`
  table; a missing field falls back to the built-in default. Any other key shape is rejected
  (fail-closed).
- **`[codegen]`**: `target`, `output`, `graph`. `manifest` and `canister` are **not** configurable:
  the manifest source stays a CLI choice (§5).
- **`[load]`**: `graph`, `key`, `state_file`.

### 4. Precedence and environment variables

Effective value lookup order: **CLI flag > `GLEAPH_*` environment variable > `gleaph.toml` >
built-in default**.

Environment variables are scalar-only and cover the machine/CI-specific connection settings:

| Variable                | Overrides                   |
| ----------------------- | --------------------------- |
| `GLEAPH_CONFIG`         | config file discovery       |
| `GLEAPH_NETWORK`        | effective network           |
| `GLEAPH_CANISTER`       | deployment `canister`       |
| `GLEAPH_IDENTITY`       | deployment `identity`       |
| `GLEAPH_FETCH_ROOT_KEY` | deployment `fetch_root_key` |

`GLEAPH_FETCH_ROOT_KEY` accepts `true`/`false` and errors on any other value. Directory settings
are intentionally not environment-overridable: they are project-fixed and exist to be pinned.

### 5. Field-level merge, then existing validation

The CLI merges config values into the parsed clap args **per field**, then runs the existing
validation unchanged (codegen source exclusivity/completeness, network resolution, missing-argument
errors). This keeps one validation path for config-backed and flag-backed invocations.

Consequences of merge ordering:

- `--canister` ceases to be clap-required on `migration status/apply`, `prepared status/apply/drop`,
  and `load`; the same "canister required" error is produced after the merge when neither the flag,
  an environment variable, nor the selected deployment entry supplies it.
- `codegen --target` ceases to be clap-required; the existing missing-target error is produced
  after the merge.
- **Manifest source is never created by config.** `[codegen]` supplies `canister`-derived state
  only through the deployment profile, and only when the caller selected no source: if neither
  `--manifest` nor `--canister` is given and the selected deployment entry has `canister`, the
  remote source is used (`canister` + `graph` from config). If the caller passes `--manifest`, the
  deployment `canister` is ignored entirely, so config cannot create a
  `ConflictingManifestSources` failure.

### 6. Path resolution

Relative paths written in `gleaph.toml` resolve against the **config file's directory**
(`identity`, `[dirs]` values, `[codegen] output`, `[load] state_file`), so a project is
reproducible from any working directory. Relative paths given as CLI flags or environment
variables resolve against the current working directory, as today. There is no shell expansion and
no `~` expansion in v1.

### 7. `fetch_root_key` semantics

`fetch_root_key` is written **only in custom-URL deployment entries** (`[deployment."https://…"]`).
It is omitted for the named networks: under `[deployment.ic]` / `[deployment.local]` the key is a
**schema error** (fail-closed), because their root-key behavior is fixed by network resolution and
a present-but-inert setting would be misleading. The effective value is CLI `--fetch-root-key`
(explicitly passed), else `GLEAPH_FETCH_ROOT_KEY`, else the URL entry's value, else `false`.
Named-network resolution in `remote.rs` is unchanged: `local` always fetches the root key, `ic`
never does, and a custom URL requires the effective value to be `true` — so a URL entry that omits
`fetch_root_key` fails with the existing "custom network URL requires --fetch-root-key" error.

### 8. Scope boundaries

- The config file is read by the **`gleaph` binary only**. The standalone `gleaph-codegen` binary
  keeps flag-only behavior (documented); the `gleaph codegen` path receives already-merged args.
- Secrets are never stored in the file: `identity` is a PEM **path**; key material stays in the
  file system.
- No _named_ network registry in v1: deployment keys are `ic`, `local`, or an exact URL; a custom
  name mapped to a URL (for example `[deployment.staging]` with a URL field) is a future extension,
  and URL keys are matched exactly (no normalization).
- The config file is committed to the repository like any project artifact; per-machine overrides
  use the `GLEAPH_*` environment layer.

## Alternatives considered

| Alternative                                                         | Decision and reason                                                                                                                                                                                                                 |
| ------------------------------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `[router]` / `[graph]` table names                                  | Rejected: `router` leaks the internal canister architecture to users; `graph` collides with the Gleaph logical-graph vocabulary. `deployment` names the user-facing concept: this project's deployment on a network.                |
| One `[deployment]` table with a `network` key                       | Rejected: per-network nesting (`[deployment.local]`, `[deployment.ic]`) makes identity/root-key differences explicit per network and matches `dfx.json`/`icp.yaml` `networks` precedent; the flag value maps directly to the entry. |
| Environment variables instead of a file                             | Rejected: not project-pinnable and not reviewable; env vars remain a per-machine override layer (§4).                                                                                                                               |
| Piggyback on the existing `dfx.json` / `icp.yaml`                   | Rejected: those formats are owned by other tools' schemas and cannot carry the Gleaph-specific canister/identity vocabulary; the CLI stays independent of `icp`'s project format.                                                   |
| `fetch_root_key` allowed in any entry, inert for `ic`/`local`       | Rejected: a present-but-no-op setting is misleading; the key is a schema error under `ic`/`local` where network resolution already decides (§7).                                                                                    |
| Config supplies the codegen manifest source (`manifest`/`canister`) | Rejected: the source is a command decision; config filling it in would need conditional exclusivity logic and could surprise callers (§5).                                                                                          |
| Global (home) config file                                           | Rejected for v1: machine-specific values belong in `GLEAPH_*`; a home config adds discovery surface without a use case yet.                                                                                                         |
| `~` expansion / shell substitution in config paths                  | Rejected for v1: predictable absolute-or-config-relative resolution is sufficient; can be added later without a format change.                                                                                                      |

## Consequences

Positive:

- Remote commands become reproducible from any working directory: `gleaph prepared apply` and
  `gleaph codegen` run with zero flags when the project pins `default_network`, `[deployment]`,
  `[dirs]`, and `[codegen]`.
- One validation path for config-backed and flag-backed invocations (merge-then-validate).
- CI can override connection settings per machine without editing the committed file
  (`GLEAPH_*`).
- Fail-closed parsing protects against typos in table and network names.

Costs and limits:

- `--canister` and `codegen --target` lose their clap-required markers; requiredness is enforced
  after merge, so error messages must be preserved.
- Config discovery walks up from the current directory, so CLI unit tests that run from a
  repository root containing `gleaph.toml` must isolate discovery (inject a config root) to avoid
  picking up a real file.
- v1 has no base `[deployment]` table, no named-network registry, and no `~` expansion; these are
  additive later without a format change.
- The standalone `gleaph-codegen` binary does not read the config file; `gleaph codegen` does.

## Validation (planned)

- CLI unit tests: discovery (walk-up, `GLEAPH_CONFIG` override, missing file), strict parsing
  (unknown key/network/table, bad `format_version`, bad `GLEAPH_FETCH_ROOT_KEY`), precedence per
  field (flag > env > config > default), config-relative path resolution, and the codegen
  source-selection rule (`--manifest` suppresses config `canister`; no config-created source
  conflicts).
- `fetch_root_key` rules: a URL-keyed entry (`[deployment."https://…"]`) supplies `fetch_root_key =
true` without flags; a URL entry omitting it fails with the existing root-key-required error;
  writing `fetch_root_key` under `[deployment.ic]` or `[deployment.local]` is a schema error; URL
  keys are matched exactly (no normalization).
- E2E note: the e2e project's identity PEM lives outside the repository (under the `icp` CLI
  home), so the e2e `gleaph.toml` pins it via `GLEAPH_IDENTITY` or an absolute path.
- Merge tests per command: `migration status/apply`, `prepared status/apply/drop`, `load`, and
  `codegen` with canister/target supplied only by config; missing-required errors preserved.
- E2E: update `scripts/check-codegen-local-e2e.sh` (or its successor) to place a `gleaph.toml` in
  the e2e project and drop the repeated `--canister -n local --identity` flags, then verify the
  codegen local E2E still passes.
- Docs: `crates/cli/README.md` and a new `crates/cli/docs/config.md` documenting the file, the
  setting tables, precedence, and path resolution.

## Amendment: Account, environment, and identity (planned)

**Status of this amendment: planned, not implemented.** The original ADR above is accepted and
implemented as written (canister-id in `[deployment.<network>]`, PEM-path identity). This section
records the changes agreed during the Account-canister design
([ADR 0068](0068-account-canister-and-per-developer-router-issuance.md)) that will be applied in a
later slice. Until implemented, the original sections remain authoritative.

### 9. Account is not stored

`gleaph.toml` carries **no account identifier**. The CLI resolves the account from the caller
principal at runtime via `Account.resolve_my_accounts()`. The `account` field is not added and the
`[project] account` proposal is rejected.

### 10. `[deployment.<environment>]` vs network

The environment/network concepts from `icp-cli` (Pitfall 7) are adopted:

- **network** = connection endpoint (URL).
- **environment** = network + identity + settings (the deploy target).
- `-e/--environment` (name reference) and `-n/--network` (direct endpoint) are separated.

`[deployment.<environment>]` replaces the current `[deployment.<network>]` meaning. Implicit
`local` and `ic` environments exist; custom environments (e.g. `staging`) are added. A Gleaph
environment is distinct from icp-cli's canister-centric environment; `icp.yaml` `environments:` are
not adopted.

### 11. Canister id removed from `gleaph.toml`

The `canister` field in `[deployment.<environment>]` is **removed**. Canister ids are supplied by
`.gleaph/` (see §13) and Account resolution. `GLEAPH_CANISTER` is **removed**; a direct id for
debugging is passed with `--canister <id>`.

### 12. Identity is a name, delegated to icp-cli

`identity` in `[deployment.<environment>]` becomes a **name**, not a PEM path. Identity resolution:

1. `--identity` / `GLEAPH_IDENTITY` (explicit) → resolve by name.
2. project has `icp.yaml` → delegate to icp-cli (`icp identity default` / `principal`).
3. otherwise → gleaph's own identity store (`~/.config/gleaph/identity/`).

Storage selection is `--storage keyring|password|plaintext` (keyring default), mirroring
`icp-cli`. The secret stays in the chosen store (icp-cli keyring when delegated); it is never
committed.

### 13. `.gleaph/` directory

Located in the same directory as `gleaph.toml`. Holds all canister-id state:

- `.gleaph/data/mappings/<env>.ids.json` — committed; platform-fixed ids (`account`, `provision`),
  public metadata.
- `.gleaph/cache/<env>.router.json` — gitignored; a user's own Router id, resolved from Account
  (SSOT) and cached per user.
- `.gleaph/cache/` — gitignored; ephemeral generated artifacts.

Generated by the CLI (e.g. `gleaph login` / a deploy flow) from Account resolution results.

### 14. Environment variables

| Variable             | Status | Notes                                              |
| -------------------- | ------ | -------------------------------------------------- |
| `GLEAPH_CONFIG`      | keep   | config file discovery                              |
| `GLEAPH_ENVIRONMENT` | new    | selects environment (`-e`)                         |
| `GLEAPH_NETWORK`     | keep   | network endpoint (`-n`)                            |
| `GLEAPH_IDENTITY`    | changed| now a **name**, not a PEM path                    |
| `GLEAPH_ROUTER`      | new    | logical router name for multi-router selection     |
| `GLEAPH_CANISTER`    | removed| mapping + Account resolution; `--canister` for direct id |
| `GLEAPH_FETCH_ROOT_KEY` | keep |                                                |

## References

- [ADR 0068 — Account canister and per-developer Router issuance](0068-account-canister-and-per-developer-router-issuance.md)
- [ADR 0061 — Prepared-query CLI registration and batch catalog API](0061-prepared-cli-registration-and-batch-catalog-api.md)
- [ADR 0058 — Versioned additive schema migrations](0058-versioned-additive-schema-migrations.md) (strict TOML artifact discipline)
- `crates/cli/src/main.rs`, `crates/cli/src/remote.rs`, `crates/cli/src/migration.rs`,
  `crates/cli/src/prepared.rs`, `crates/cli/src/load.rs`
- `crates/codegen/src/cli.rs` (`CodegenArgs`, `run`, `fetch_manifest`)
- `crates/codegen/e2e/icp.yaml` (project-scoped `networks` configuration precedent)
- `scripts/check-codegen-local-e2e.sh` (status quo repeated-flag workflow)
