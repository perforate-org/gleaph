# `gleaph migration` — schema migrations

`gleaph migration` manages immutable, additive schema migrations for a Gleaph
deployment. Migrations are local, versioned packages that Router applies once
and records in a durable ledger; a migration is never edited after it is
published.

The CLI owns the local package and chain invariants; Router owns the durable
applied ledger and execution. Local validation (`plan`) never makes a remote
call.

## Migration packages

A migration is a directory named `NNNNNN_slug` (six digits, underscore, and a
lowercase slug) under the migration root (`--dir`, default `./migrations`),
containing `migration.toml` and exactly one payload:

| Entry | Purpose |
| --- | --- |
| `migration.toml` | Manifest (below) |
| `up.gql` | The additive payload as a single GQL file, **or** |
| `up/` | The additive payload as a directory of `*.gql` fragments, concatenated in sorted filename order |

`up.gql` and `up/` are mutually exclusive. Fragment names must be unique, must
end in `.gql`, and every fragment must be LF-terminated so the concatenation is
deterministic. No symlinks or extra files are allowed anywhere in the tree;
temporary entries prefixed `.gleaph-tmp-` are ignored by discovery. The
concatenated payload is limited to 65,536 bytes.

`gleaph migration new --up <PATH>` accepts either form: a single `.gql` file
scaffolds `up.gql`, a directory scaffolds the `up/` fragment form. Without
`--up`, a minimal graph-type template is created.

### Manifest (`migration.toml`)

```toml
format_version = 1
id = "000001_create_person"
parent = "000000_init"
description = "Add the Person graph type"
# graph = "my_graph"   # valid only for CREATE INDEX and CREATE VECTOR INDEX migrations
```

| Field | Notes |
| --- | --- |
| `format_version` | Must be `1` |
| `id` | Canonical id; must equal the directory name (`NNNNNN_slug`) |
| `parent` | Id of the predecessor; the unique chain root omits it |
| `graph` | Optional named graph selector; **valid only for `CREATE INDEX` and `CREATE VECTOR INDEX`** migrations; omitted → the default graph |
| `description` | Human metadata; excluded from the execution checksum |

### GQL dialect (`up.gql` / `up/`)

The payload is one or more additive statements chained with `NEXT` (the only
chain operator; each statement may carry a trailing semicolon), drawn from:

- `CREATE GRAPH TYPE <name> { ... }` with an explicit body;
- `CREATE GRAPH <name> TYPED <type>` with simple literal names;
- `CREATE INDEX ...` — the property-index DDL; several `NEXT`-chained
  statements may share one migration and drive sequential Router backfill
  builds;
- `CREATE VECTOR INDEX ...` — the vector-index DDL with its `OPTIONS`
  block; exactly one statement, always the only statement in its migration,
  which provisions and registers the vector index against the selected graph.

Forbidden in migrations: parameters, `SESSION` commands, transaction commands,
`INSERT`/`SET`/`DELETE` DML, `IF NOT EXISTS`, `OR REPLACE`, `COPY`, and
`DROP INDEX`. `CREATE INDEX` and `CREATE VECTOR INDEX` migrations must not mix
with catalog statements (`CREATE GRAPH TYPE` / `CREATE GRAPH`) in one payload.
The whole payload is limited to 65,536 bytes and 1,024 statements.

> **`CREATE TEXT INDEX` note:** the Router's migration apply path already
> drives the text-canister backfill lifecycle for a single-statement
> `CREATE TEXT INDEX` payload, but the CLI's local validation does not accept
> text-index payloads yet — author text indexes through the ad-hoc GQL DDL
> path (`CREATE TEXT INDEX ...`) until the migration lane grows the text
> parser.

The execution identity is a sha256 checksum over the id, parent, graph
selector, and exact payload bytes; `description` does not affect it.

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
| `apply` | A live progress line per pending migration (rewritten in place on a terminal; index-build targets named with row/percent detail), then a summary (`applied <n> migrations` / `applied <n> new, <m> replay` / `no migrations to apply`) |

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
  `migration apply` resumes a pending build (a progress bar tracks the
  per-target index build when the migration drives several);
- a `CREATE VECTOR INDEX` migration provisions and registers the vector
  canister target synchronously before the ledger record commits;
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

A vector-index migration targeting a named graph declares the selector in
the manifest; the statement carries the complete physical shape:

```toml
format_version = 1
id = "000003_document_embedding"
parent = "000002_add_person"
description = "Embedding index over Document.embedding"
graph = "my_graph"
```

```gql
CREATE VECTOR INDEX document_embedding
FOR (d:Document)
ON d.embedding
OPTIONS {
  dimensions: 768,
  similarity_function: "cosine",
  encoding: "i8",
  algorithm: "ivf_flat"
}
```
