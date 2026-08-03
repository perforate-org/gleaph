# `gleaph migration` — schema migrations

`gleaph migration` manages immutable, additive schema migrations for a Gleaph
deployment. Migrations are local, versioned packages that Router applies once
and records in a durable ledger; a migration is never edited after it is
published (ADR 0058).

The CLI owns the local package and chain invariants; Router owns the durable
applied ledger and execution. Local validation (`plan`) never makes a remote
call.

## Migration packages

A migration is a directory named `NNNNNN_slug` (six digits, underscore, and a
lowercase slug) under the migration root (`--dir`, default `./migrations`),
containing exactly two regular files:

| File | Purpose |
| --- | --- |
| `migration.toml` | Manifest (below) |
| `up.gql` | The single additive catalog statement |

No symlinks or extra files are allowed anywhere in the tree; temporary entries
prefixed `.gleaph-tmp-` are ignored by discovery.

### Manifest (`migration.toml`)

```toml
format_version = 1
id = "000001_create_person"
parent = "000000_init"
description = "Add the Person graph type"
# graph = "my_graph"   # valid only for CREATE INDEX migrations
```

| Field | Notes |
| --- | --- |
| `format_version` | Must be `1` |
| `id` | Canonical id; must equal the directory name (`NNNNNN_slug`) |
| `parent` | Id of the predecessor; the unique chain root omits it |
| `graph` | Optional named graph selector; **valid only for `CREATE INDEX`** migrations; omitted → the default graph |
| `description` | Human metadata; excluded from the execution checksum |

### GQL dialect (`up.gql`)

A migration contains exactly **one** additive catalog statement, one of:

- `CREATE GRAPH TYPE <name> { ... }` with an explicit body;
- `CREATE GRAPH <name> TYPED <type>` with simple literal names;
- `CREATE INDEX ...` (the gleaph index DDL, which starts a separate Router
  backfill lifecycle).

Forbidden in migrations: parameters, `SESSION` commands, transaction commands,
`NEXT`-chained statements, `IF NOT EXISTS`, `OR REPLACE`, `COPY`, and
`DROP INDEX`. The statement is limited to 65,536 bytes.

The execution identity is a sha256 checksum over the id, parent, graph
selector, and statement bytes; `description` does not affect it.

## Command reference

```
gleaph migration <SUBCOMMAND>
```

| Subcommand | Description |
| --- | --- |
| `gleaph migration new <slug>` | Create and atomically publish the next migration package |
| `gleaph migration plan` | Validate and print the local migration chain (no remote calls) |
| `gleaph migration status` | Compare the local chain with Router's durable ledger |
| `gleaph migration apply` | Apply pending migrations through Router in parent order |

| Flag | Applies to | Meaning |
| --- | --- | --- |
| `--dir <PATH>` | all | Migration root; default `./migrations` |
| `--description <TEXT>` | `new` | Human-readable rationale |
| `--up <PATH>` | `new` | Read `up.gql` bytes from this path; default is a minimal `CREATE GRAPH TYPE <slug> {}` template |
| `--canister <PRINCIPAL>` | `status`, `apply` | Router canister principal (required) |
| `-n, --network <NETWORK>` | `status`, `apply` | Network name (`ic`/`local`) or endpoint URL; default `ic` |
| `--identity <PATH>` | `status`, `apply` | PEM file containing a Secp256k1 identity |
| `--fetch-root-key` | `status`, `apply` | Fetch the network root key before a custom endpoint |

## Chain rules

Discovery validates the whole tree before any remote call:

- exactly one chain root (no parent);
- a linear parent-to-child sequence — each parent has at most one child, every
  parent link resolves, and no cycles exist;
- a child's numeric prefix must be greater than its parent's;
- numeric prefixes are unique and never reused (exhausted at 999,999);
- at most 4,096 migrations.

## Outputs

| Subcommand | Output |
| --- | --- |
| `new` | `created <id> (<path>)` |
| `plan` | One `<id> <checksum-hex>` line per migration, or `no migrations` |
| `status` | `applied <n>/<total>` |
| `apply` | One status line per migration (`Applied`, `Replay`, `Progress(...)`) |

## Remote semantics

`status` pages Router's `list_schema_migrations` ledger (16 records per page)
and compares every remote record against the local chain by id, parent, graph
selector, and checksum. Any drift, a remote migration absent from the local
chain, or a remote `Failed` record is reported as an error.

`apply` first performs the same local/remote preflight, then applies pending
migrations in parent order. Each migration is sent with the exact local
envelope:

- an ambiguous response is resolved by **exact replay** of the same envelope;
- a `CREATE INDEX` migration returns `Progress(...)` and remains durable and
  resumable: the command polls bounded rounds until `Applied`, and re-running
  `migration apply` resumes a pending index migration;
- a deterministic `Failed` status stops the run.

## Exit codes

`gleaph migration` exits 0 on success and 1 on any failure (validation,
chain, or remote).

## Examples

Create the first migration, then a follow-up:

```sh
gleaph migration new init --description "Baseline graph types"
gleaph migration new add_person --description "Add the Person type"
```

Validate the chain locally:

```sh
gleaph migration plan
```

Apply pending migrations through a Router:

```sh
gleaph migration apply --canister rrkah-fqaaa-aaaaa-aaaaq-cai \
  -n local --identity ~/.config/dfx/identity/default/identity.pem
```

A `CREATE INDEX` migration targeting a named graph declares the selector in
the manifest and starts a resumable Router backfill lifecycle:

```toml
format_version = 1
id = "000003_index_person_email"
parent = "000002_add_person"
description = "Index Person.email"
graph = "my_graph"
```
