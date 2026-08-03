# `gleaph codegen` — prepared-query client generation

`gleaph codegen` generates typed client and canister adapter code for Gleaph
prepared queries. It consumes a versioned `PreparedManifest`, validates it, and
renders one of the supported language profiles.

The command is the top-level CLI entrypoint for the same generator that the
standalone `gleaph-codegen` binary exposes; both parse the shared
`gleaph_codegen::CodegenArgs` and execute `gleaph_codegen::run`, so option
validation, manifest retrieval, and output behavior are identical. The
full manifest schema, supported targets, and library API are documented in
[`../codegen/README.md`](../../codegen/README.md); this page covers the CLI
surface.

## Manifest source

The manifest comes from exactly one of two sources:

| Source                       | Flags                                          |
| ---------------------------- | ---------------------------------------------- |
| Local JSON file              | `--manifest <PATH>`                            |
| Router `list_prepared` query | `--canister <PRINCIPAL>` with `--graph <NAME>` |

`--manifest` and `--canister` are mutually exclusive, and `--canister` must be
paired with `--graph` (a remote source is incomplete without both). A
manifest source is required.

## Command reference

```
gleaph codegen (--manifest <PATH> | --canister <PRINCIPAL> --graph <NAME>)
               --target <TARGET> [--output <PATH>]
               [--format rust=<MODE>] [-n <NETWORK>] [--identity <PATH>]
               [--fetch-root-key]
```

| Flag                       | Meaning                                                                                                             |
| -------------------------- | ------------------------------------------------------------------------------------------------------------------- |
| `--manifest <PATH>`        | Read the prepared manifest from a local JSON file                                                                   |
| `--canister <PRINCIPAL>`   | Query a Router canister for the graph's prepared manifest (requires `--graph`)                                      |
| `--graph <NAME>`           | Graph name used with `--canister`                                                                                   |
| `--target <TARGET>`        | Output profile; one of `typescript`/`ts`, `javascript`/`js`, `rust`/`rs`, `rust-canister`, `motoko`/`mo` (required) |
| `--output <PATH>`          | Write generated source to this path instead of stdout                                                               |
| `--format <LANGUAGE=MODE>` | Rust formatting policy: `rust=auto` (default), `rust=rustfmt`, or `rust=never`; one mode per language               |
| `-n, --network <NETWORK>`  | Network name (`ic`/`local`) or an HTTP(S) endpoint URL; default `ic`                                                |
| `--identity <PATH>`        | PEM file containing a Secp256k1 identity for Router queries                                                         |
| `--fetch-root-key`         | Fetch the network root key before querying a custom endpoint                                                        |

Target aliases (`ts`, `js`, `rs`, `mo`) select the same profile as the full
name. The Rust and Motoko canister profiles define transport boundaries; the
application provides the runtime executor that performs Router calls, response
decoding, and error conversion (see
[`../codegen/README.md`](../../codegen/README.md)).

## Exit codes

`gleaph codegen` exits 0 on success and 1 on any failure (conflicting or
missing manifest source, unknown target or format mode, manifest validation,
or generation).

## Examples

Generate TypeScript from a local manifest to stdout:

```sh
gleaph codegen --manifest prepared.json --target typescript
```

Generate a Rust client from a local manifest with deterministic formatting:

```sh
gleaph codegen --manifest prepared.json --target rust \
  --format rust=never --output src/generated.rs
```

Fetch the manifest from a Router on the local network and emit Motoko:

```sh
gleaph codegen --canister rrkah-fqaaa-aaaaa-aaaaq-cai --graph default \
  --target motoko -n local --output src/generated.mo
```

## Relationship to other commands

- Schema migrations are a separate workflow (`gleaph migration`); they do not
  change the prepared-query code-generation contract.
- Initial data loading is `gleaph load`.
