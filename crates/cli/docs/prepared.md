# `gleaph prepared` — prepared-query registration

`gleaph prepared` registers prepared queries from local `.gql` files. Each file is one operation;
the CLI validates the directory locally, compares it against Router storage, and applies the
operations through the Router's bounded batch API (ADR 0061). The Router completes parameter and
result metadata and remains the final validator.

The CLI owns the local artifact and validation rules; Router owns the prepared catalog,
authorization, and execution. Local validation (`plan`) never makes a remote call.

## Artifact format

A `prepared/` directory (default; `--dir` override) containing one operation per file:

| File | Purpose |
| --- | --- |
| `<name>.gql` | The prepared-query source; the operation name is the file stem |
| `<name>.toml` | Optional explicit metadata sidecar (below) |

No symlinks, subdirectories, or other files are allowed. Operation names must match
`[a-z][a-z0-9-]*` (kebab-case; codegen derives camelCase methods). A sidecar without its matching
source is rejected. The source is bounded at 65,536 bytes.

### Sidecar (`<name>.toml`)

```toml
description = "Find users by term."      # optional; `///` doc comments on the source apply otherwise
supports_consistency = false              # optional, default false
supports_idempotency = false              # optional, default false

[[allowed_sorts]]                          # optional sort keys accepted by the operation
key = "name"
label = "Name"
```

Unknown sidecar fields are rejected. Parameter types and the result schema are **not** authorable
here: the Router completes them from the program (`VALUE <name> TYPED ...` declarations and the
typed output) at registration time.

### Source conventions

- `///` doc-comment lines describe the operation; `/// @param <name> <text>` describes an input
  parameter (used when the sidecar leaves the description absent).
- `VALUE <name> TYPED <type> = $<name>` declares a typed input parameter.
- `USE GRAPH` selects a non-home graph; otherwise the operation registers against the caller's
  home graph.

## Command reference

```
gleaph prepared <SUBCOMMAND>
```

| Subcommand | Description |
| --- | --- |
| `gleaph prepared new <name>` | Scaffold `<name>.gql` (atomic; fails if it exists) |
| `gleaph prepared plan` | Validate and print the local prepared directory (no remote calls) |
| `gleaph prepared status` | Compare the local directory with Router storage |
| `gleaph prepared apply` | Register local operations through Router in bounded batches |
| `gleaph prepared drop <name>` | Remove one named operation from Router storage |

| Flag | Applies to | Meaning |
| --- | --- | --- |
| `--dir <PATH>` | all | Prepared directory; default `./prepared` |
| `--description <TEXT>` | `new` | Operation description emitted as the source doc comment |
| `--canister <PRINCIPAL>` | `status`, `apply`, `drop` | Router canister principal (required) |
| `-n, --network <NETWORK>` | `status`, `apply`, `drop` | Network name (`ic`/`local`) or endpoint URL; default `ic` |
| `--identity <PATH>` | `status`, `apply`, `drop` | PEM file containing a Secp256k1 identity |
| `--fetch-root-key` | `status`, `apply`, `drop` | Fetch the network root key before a custom endpoint |

## Outputs

| Subcommand | Output |
| --- | --- |
| `new` | `created <name> (<path>)` |
| `plan` | One `<name> Query|Update` line per operation, or `no prepared operations` |
| `status` | One `<name> missing|drift|remote-only` line per finding, then `up-to-date <n>/<total>` |
| `apply` | One `<name> registered` line per operation, or `no prepared operations` |
| `drop` | `dropped <name>` |

## Remote semantics

- `status` compares each local operation against `get_prepared(name)`: the query bytes and the
  explicitly authored sidecar fields (description, `allowed_sorts`, consistency/idempotency
  flags). Router-completed fields (parameters, result columns) are derived and never diffed.
  Operations absent from Router are `missing`; stored operations not in the local directory are
  `remote-only` (detected via `list_prepared`, which resolves the caller's default graph). Drift
  or missing operations make the command exit non-zero.
- `apply` validates the directory locally first, then registers in chunks of 32 through Router's
  all-or-nothing `prepare` batch. A chunk failure propagates the Router error (which names the
  failing operation); re-running converges because the upsert is idempotent. An empty or missing
  directory is a no-op.
- Registration always sends operation metadata (`metadata: Some(...)`), because `list_prepared` —
  the codegen input — surfaces metadata-bearing operations only.

## Exit codes

`gleaph prepared` exits 0 on success and 1 on any failure (validation, remote, or status drift).

## Examples

```sh
gleaph prepared new find-users --description "Find users by term"
gleaph prepared plan
gleaph prepared apply --canister rrkah-fqaaa-aaaaa-aaaaq-cai \
  -n local --identity ~/.config/dfx/identity/default/identity.pem
gleaph prepared status --canister rrkah-fqaaa-aaaaa-aaaaq-cai -n local
gleaph prepared drop find-users --canister rrkah-fqaaa-aaaaa-aaaaq-cai -n local
```
