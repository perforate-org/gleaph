# gleaph-example-rust-canister-app

Example application canister showing how `gleaph-codegen` output and the `gleaph-cdk` client
fit together.

## Flow

1. **Declare prepared operations** in [`manifest.json`](manifest.json). Each operation has a
   name, a kind (`Query` / `Update`), typed parameters, and a result schema.
2. **Generate the typed facade** into [`src/generated.rs`](src/generated.rs):

   ```sh
   cargo run -p gleaph-codegen -- --manifest manifest.json \
     --target rust-canister --format rust=rustfmt --output src/generated.rs
   ```

   The generated code is transport-neutral: it declares `*Params` / `*Row` types, a
   `PreparedCanisterExecutor` trait, and a `PreparedCanisterQueries` facade.

3. **Implement the executor** in [`src/lib.rs`](src/lib.rs) with `GleaphClient`. The executor
   performs Router calls (`prepared_query` / `prepared_mutate`) and decodes the response rows
   via `GqlQueryResult::decode_serde_rows`.
4. **Expose canister entrypoints** that delegate to the facade, or use the client directly for
   dynamic GQL and idempotent mutations.

## What the entrypoints show

| Entrypoint             | Pattern                                                                                   |
| ---------------------- | ----------------------------------------------------------------------------------------- |
| `find_users`           | Prepared read through the generated facade                                                |
| `user_names_by_prefix` | Dynamic GQL built with the `gql!` macro                                                   |
| `create_user_and_read` | Idempotent `gql_mutate` + read-your-writes via the returned token and `ReadMode::AtLeast` |

## Notes

- The generated facade routes prepared operations with logical GQL parameters through
  `PreparedCanisterExecutor::execute_gql`. For mutations, prefer the client-direct path
  (`gql_mutate` / `prepared_mutate`) with an explicit caller-supplied idempotency key, as shown
  in `create_user_and_read`.
- The Router principal in `router_id()` is a placeholder; set it from init args or environment
  configuration in production.
