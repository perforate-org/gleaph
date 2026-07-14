# Social demo frontend

## GQL formatter WASM

The query panel uses the formatter owned by `gleaph-gql` through the thin adapter in
`wasm/`. The adapter intentionally enables only the `format` and `gleaph` features and
keeps JavaScript option/error conversion outside the portable GQL crate.

Regenerate the browser bindings after changing the adapter or formatter API with:

```sh
pnpm run build:gql-formatter
```

This uses `wasm-pack build --target web --release --mode no-install` and writes ignored,
reproducible output to `src/generated/gql_formatter/`. The pinned `wasm-bindgen` and
`js-sys` versions in `wasm/Cargo.toml` must match the installed wasm-pack toolchain.
