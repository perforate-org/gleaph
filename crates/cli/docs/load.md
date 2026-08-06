# `gleaph load` — initial data loading

`gleaph load` loads initial vertices and edges into an existing logical graph
through the durable Router `bulk_load` lifecycle (ADR 0057, ADR 0060 Decision 4).
It is the intended entry point for the initial data load of an application
graph; incremental edge loads can reference existing vertices by property
instead of by in-artifact ids.

The command never hardcodes a chunk size: each request is fitted to the
inter-canister payload bound with `gleaph-message-sizing`, chunk boundaries are
Router-owned, and the driver loops on the returned `next_offset` cursor.

## Artifact formats

The artifact is either one YAML/JSON file or two NDJSON files. The same row
schema applies to both families.

### YAML/JSON single file

A single document with `format_version: 1`, plus optional `vertices` and
`edges` arrays:

```yaml
format_version: 1
vertices:
  - source_id: alice
    labels: [Person]
    properties:
      name: { Text: Alice }
      joined: { DateTime: { seconds: 1700000000, nanos: 5 } }
edges:
  - source: alice
    target: bob
    label: KNOWS
    directed: true
```

The single-file family is bounded to 64 MiB (the cap also bounds YAML
alias-expansion work); larger data must use NDJSON.

### NDJSON files

Two newline-delimited files, one JSON object per line, with the same row
schema as above. Blank lines are ignored. Designate them as two positional
arguments (vertices first) or with `--vertices FILE` / `--edges FILE`:

```sh
gleaph load vertices.jsonl edges.jsonl --canister <PRINCIPAL>
gleaph load --vertices vertices.jsonl --edges edges.jsonl --canister <PRINCIPAL>
```

A single NDJSON positional argument is ambiguous and rejected; use
`--vertices` / `--edges`. A `--edges`-only load must use property-based
endpoints (below), because a `source_id` reference cannot resolve without
vertices in the same artifact.

### Row schema

Vertex row:

| Field        | Type             | Notes                                       |
| ------------ | ---------------- | ------------------------------------------- |
| `source_id`  | string           | Required; unique within the artifact        |
| `labels`     | array of strings | Required; non-empty, no empty entries       |
| `properties` | object           | Optional; duplicate property names rejected |

Edge row:

| Field              | Type     | Notes                              |
| ------------------ | -------- | ---------------------------------- |
| `source`, `target` | endpoint | See below                          |
| `label`            | string   | Required; non-empty                |
| `directed`         | bool     | Optional; default `true`           |
| `inline_value`     | value    | Optional edge inline property      |
| `properties`       | object   | Optional; duplicate names rejected |

An endpoint is either:

- a bare string naming a `source_id` loaded in the same artifact
  (`source: alice`), or
- a property-based reference to an existing vertex:
  `source: {label: Person, property: email, value: {Text: a@b.c}}`.

Property-based endpoints are resolved by the Router through a **converged
property index** on `(label, property)` before the chunk is admitted. The whole
candidate chunk is rejected without committing anything when any endpoint is
missing or resolves to more than one vertex, or when the required index does
not exist (reported as an operator action). Property endpoint values must be
sortable (indexable) GQL value types.

Property values use the canonical GQL value JSON forms, for example
`{"Text": "Alice"}`, `{"Int64": 30}`, `{"Float64": 1.5}`, `{"Bool": true}`,
and `{"DateTime": {"seconds": 1700000000, "nanos": 5}}`.

## Command reference

```
gleaph load [OPTIONS] [ARTIFACT]...
```

| Argument / flag           | Meaning                                                                                    |
| ------------------------- | ------------------------------------------------------------------------------------------ |
| `ARTIFACT`                | One YAML/JSON file, or two NDJSON files (vertices, then edges)                             |
| `--canister <PRINCIPAL>`  | Router canister principal (required)                                                       |
| `--graph <NAME>`          | Logical graph; omitted → the caller's default (HOME) graph                                 |
| `-k, --key <KEY>`         | Durable bulk-load job key; default `initial-load-v1`                                       |
| `-n, --network <NETWORK>` | Network name (`ic`/`local`) or endpoint URL; default `ic`                                  |
| `--identity <PATH>`       | PEM file containing a Secp256k1 identity                                                   |
| `--fetch-root-key`        | Fetch the network root key before a custom endpoint                                        |
| `--format <FORMAT>`       | `yaml` / `json` / `jsonl`; inferred from the file extension when omitted                   |
| `--vertices <FILE>`       | NDJSON vertices file only (mutually exclusive with positional ARTIFACT)                    |
| `--edges <FILE>`          | NDJSON edges file only; requires property-based endpoints                                  |
| `--fresh`                 | Start a new job under a derived key `{key}.{nonce}` instead of resuming                    |
| `--state-file <PATH>`     | Record the effective key and artifact digest; skip-on-Completed requires a matching digest |

`--key` and `--graph` are limited to 1..=256 UTF-8 bytes.

## Lifecycle and durability

`gleaph load` drives the durable bulk-load job to `Completed`:

1. **Validate** the whole artifact before any remote call; NDJSON files are
   validated by a streaming pre-scan. A validation failure exits 2 without
   changing any remote state.
2. **Skip or resume**: if a job already exists under the effective key, a
   `Completed` job is skipped (exit 0) — with `--state-file`, only when the
   artifact digest matches; a `Failed` or `Aborted` job is an operator error.
   Otherwise the driver resumes from the `bulk_load_status` receipt
   boundaries.
3. **Start** the job, then append **vertex chunks** (in artifact order,
   collecting the allocated vertex ids), then **edge chunks** (endpoints
   resolved against those ids).
4. **Finalize** and poll until `Completed`.

Each Append commits a budget-fitting prefix and returns `next_offset`
(operations of the candidate batch committed); the driver loops
`offset = next_offset` until the batch is consumed. Chunk boundaries are
Router-owned, so a chunk never traps at the execution ceiling.

During the vertex and edge phases the command prints a live progress bar with
a row count and percentage (`loading vertices … 12,345 / 17,240 (72%)`). On a
terminal the line is rewritten in place; when the output is captured, a line is
printed only when the percentage advances.

Because durable bulk-load keys are single-use after a terminal state,
re-loading after a terminal job requires a new `--key` or `--fresh`.

## Streaming reads

NDJSON files are read as a row stream, so peak CLI memory is bounded by one
chunk plus the compact vertex-id index rather than the file size:

- a **pre-scan pass** validates every row and hashes the raw file bytes
  (the `--state-file` digest) without materializing rows;
- a **dispatch pass** re-reads the files and builds budget-fitted chunks.

The YAML/JSON single-file family is read in full but remains bounded by its
64 MiB cap.

## Exit codes

| Code | Meaning                                                                                                        |
| ---- | -------------------------------------------------------------------------------------------------------------- |
| 0    | Load completed, or skipped because the job was already `Completed`                                             |
| 1    | Operator action required (terminal `Failed`/`Aborted` job, artifact digest changed, finalize did not complete) |
| 2    | Input validation (usage or artifact error); nothing was loaded                                                 |
| 3    | Remote/auth failure (connection, Router rejection, unexpected response)                                        |

## Examples

Load a YAML artifact into the default graph:

```sh
gleaph load seed.yaml --canister rrkah-fqaaa-aaaaa-aaaaq-cai
```

Load a large NDJSON pair into a named graph with a resume identity:

```sh
gleaph load --vertices vertices.jsonl --edges edges.jsonl \
  --canister rrkah-fqaaa-aaaaa-aaaaq-cai \
  --graph my_graph --key initial-v2 --state-file .load-state.json
```

Add edges only, referencing existing vertices by their `email` property
(requires a converged index on `(Person, email)`):

```sh
gleaph load --edges follow-edges.jsonl --canister rrkah-fqaaa-aaaaa-aaaaq-cai
```
