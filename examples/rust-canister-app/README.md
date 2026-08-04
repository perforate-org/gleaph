# gleaph-example-rust-canister-app

Example application canister showing how `gleaph-codegen` output and the `gleaph-cdk` client
fit together.

## Flow

1. **Declare prepared operations** in [`manifest.json`](manifest.json). Each operation has a
   name, a kind (`Query` / `Update`), typed parameters, and a result schema.
2. **Generate the typed operations** into [`src/generated.rs`](src/generated.rs):

   ```sh
   cargo run -p gleaph-codegen -- --manifest manifest.json \
     --target rust-canister --format rust=never --output src/generated.rs
   ```

   The generated code declares `*Params` / `*Row` types, a `Prepared` marker, and a
   `PreparedExt` trait implemented for `GleaphClient<Prepared>`. Query operations wrap the
   Router's `prepared_query`; update operations wrap `prepared_mutate` and take an explicit
   `client_mutation_key`.

3. **Use the client**: create `GleaphClient::with_prepared::<Prepared>(router_id())`, import
   `PreparedExt`, and call the operations on the client.

## What the entrypoints show

| Entrypoint             | Pattern                                                                                   |
| ---------------------- | ----------------------------------------------------------------------------------------- |
| `find_users`           | Prepared read through the generated `PreparedExt` trait                                   |
| `user_names_by_prefix` | Dynamic GQL built with the `gql!` macro                                                   |
| `create_user_and_read` | Idempotent `gql_mutate` + read-your-writes via the returned token and `ReadMode::AtLeast` |

## Notes

- A plain `GleaphClient::new(router_id())` does **not** implement `PreparedExt`, so the
  compiler rejects calling generated operations on it; `with_prepared::<Prepared>` opts in.
- The Router principal in `router_id()` is a placeholder; set it from init args or environment
  configuration in production.
