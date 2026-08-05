# gleaph-codegen

`gleaph-codegen` generates typed client and canister adapter code for Gleaph
prepared queries.

The crate provides both a Rust library API and the `gleaph-codegen` command-line
binary. It consumes a versioned `PreparedManifest`, validates the manifest, and
renders one of the supported language profiles.

## Supported targets

| Target              | Output                                                        |
| ------------------- | ------------------------------------------------------------- |
| `typescript` / `ts` | TypeScript client helpers using `@gleaph/sdk`                 |
| `javascript` / `js` | JavaScript client helpers using `@gleaph/sdk`                 |
| `rust` / `rs`       | Rust application-client declarations and executor facade      |
| `rust-canister`     | Rust canister declarations and `gleaph-cdk` executor boundary |
| `motoko` / `mo`     | Motoko canister declarations and executor boundary            |

The generated Rust and Motoko canister profiles define transport boundaries;
the application must provide the runtime executor that performs Router calls,
response decoding, and error conversion.

## Command line

Generate from a local manifest:

```sh
cargo run -p gleaph-codegen -- \
  --manifest path/to/manifest.json \
  --target typescript \
  --output src/generated.ts
```

The same codegen command is available under the top-level `gleaph` CLI:

```sh
cargo run -p gleaph-cli -- codegen \
  --manifest path/to/manifest.json \
  --target typescript \
  --output src/generated.ts
```

Both entrypoints parse the same public `gleaph_codegen::CodegenArgs` clap
arguments and execute `gleaph_codegen::run`. This keeps option validation,
manifest retrieval, and generated output behavior identical between the
standalone and top-level commands.

Schema migrations are a separate top-level `gleaph migration` workflow owned by
`gleaph-cli`; they do not change the prepared-query code-generation contract.
Prepared-query registration is likewise a separate top-level `gleaph prepared` workflow
(`gleaph-cli`), which registers operations from local `.gql` files through the Router's batch
`prepare` API; the manifest consumed here is then retrieved from the Router via `list_prepared`.

The default output is stdout. Use `--output` to write a file instead.

The manifest can also be fetched from a Router using its graph-scoped
`list_prepared` query:

```sh
cargo run -p gleaph-codegen -- \
  --canister <router-principal> \
  --graph default \
  --target rust \
  --output src/generated.rs
```

Network selection follows the `icp-cli` convention used by the project:

- `-n ic` is the default and uses the IC mainnet endpoint.
- `-n local` uses `http://localhost:8000` and fetches the local root key.
- An `http://` or `https://` URL selects a custom endpoint; custom endpoints
  require `--fetch-root-key`.

For authenticated Router queries, pass `--identity <pem>` with a Secp256k1
identity file.

Rust output supports an explicit formatting policy:

```sh
--format rust=auto     # use rustfmt when available; otherwise built-in formatting
--format rust=rustfmt  # require rustfmt
--format rust=never    # use the generator's built-in Rust formatting
```

`rust=auto` is the default. The fixture checks use `rust=never` so that their
output is independent of the host rustfmt installation.

## Manifest

The manifest is JSON-serializable and is shared with the
`gleaph-prepared-api` contract crate. A minimal manifest looks like this:

```json
{
  "manifest_version": 1,
  "graph": { "id": "default", "name": null },
  "operations": [
    {
      "name": "find-users",
      "description": "Find users by their search term.",
      "kind": "Query",
      "parameters": [
        {
          "name": "term",
          "description": "Text to search for.",
          "required": true,
          "nullable": false,
          "type": "Text"
        }
      ],
      "result": {
        "columns": [{ "name": "user_name", "type": "Text", "nullable": false }]
      },
      "supports_consistency": false,
      "supports_idempotency": false,
      "allowed_sorts": []
    }
  ]
}
```

Operation and parameter descriptions are emitted as documentation comments in
the generated source. `allowed_sorts` declares the sort keys accepted by a
prepared operation; generated TypeScript and Rust client APIs expose sort
arguments for operations that declare them.

The manifest version is currently `1`. The Router metadata ABI remains under
construction and may change destructively before Gleaph release.

## Library API

The library exposes manifest parsing/validation and one generator per output
profile:

```rust
use gleaph_codegen::{generate_typescript, parse_manifest};

let manifest = parse_manifest(json_manifest)?;
let generated = generate_typescript(&manifest)?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

The normalized `CodegenIr` is shared by all language profiles. This keeps
manifest validation and semantic type handling consistent across generated
languages.

## Fixtures and local E2E

Regenerate and validate the checked-in language fixtures with:

```sh
pnpm codegen:check-fixtures
```

The local-network end-to-end test provisions a small Router/Graph/Index
topology, fetches a manifest, and executes generated JavaScript through the JS
SDK. It requires Docker:

```sh
pnpm codegen:e2e:local
```

See [`e2e/README.md`](e2e/README.md) for the topology and prerequisites.

## License

MIT OR Apache-2.0
