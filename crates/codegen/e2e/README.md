# codegen local-network E2E

This is a deliberately separate `icp` project for the code-generation black-box test.
It does not reuse the repository root deployment project because the root project contains
application/demo canisters and uses a dynamically mapped local gateway.

The test topology is:

```text
Router <- Graph shard 0 -> Graph index
```

Run it from the repository root:

```sh
pnpm codegen:e2e:local
```

The script starts the managed `local` network on `localhost:8000` when necessary, builds and
installs the three canisters, registers one empty prepared query with metadata, retrieves the
manifest with `gleaph-codegen --network local`, and executes the generated JavaScript through
`@gleaph/sdk`.

The test requires Docker because `icp.yaml` uses the managed `icp-cli-network-launcher` image.
The canisters and CLI state are kept under `.icp/codegen-e2e-*`; set the corresponding
`ICP_*` variables to use another location.

This first slice intentionally verifies the JavaScript SDK runtime. Rust canister wrapper
execution and Motoko runtime execution are separate follow-up slices; their generated fixtures
continue to be checked by `pnpm codegen:check-fixtures`.
