//! Example application canister: `gleaph-codegen` output + `gleaph-cdk`.
//!
//! End-to-end flow:
//!   1. `manifest.json` declares prepared operations.
//!   2. Codegen generates the typed operations into `generated.rs`:
//!      `cargo run -p gleaph-codegen -- --manifest manifest.json --target rust-canister \
//!         --format rust=rustfmt --output src/generated.rs`
//!   3. This crate creates a [`GleaphClient<Prepared>`] with
//!      [`GleaphClient::with_prepared`] and exposes canister entrypoints that delegate to the
//!      Gleaph Router.
//!
//! The three entrypoints demonstrate the API surface:
//! - [`find_users`]: prepared read through the generated [`PreparedExt`] trait.
//! - [`user_names_by_prefix`]: dynamic GQL built with the [`gql!`] macro.
//! - [`create_user_and_read`]: idempotent mutation plus read-your-writes via the returned
//!   [`MutationToken`](gleaph_cdk::MutationToken).
//!
//! Generated `*Row` structs derive [`CandidType`] and serde, so a canister can return them
//! directly over its candid interface without hand-written mapping types.

use candid::Principal;
use gleaph_cdk::{CallError, GleaphClient, GqlQueryResult, MutationToken, ReadMode, gql};
use ic_cdk_macros::{query, update};

pub mod generated;
use generated::{FindUsersParams, FindUsersRow, Prepared, PreparedExt, PreparedResponse};

/// Principal of the deployed Gleaph Router. In production, set this from canister init args or
/// environment configuration.
fn router_id() -> Principal {
    Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").expect("valid router principal")
}

/// Prepared read: find users whose name starts with `term`.
#[query(composite = true)]
async fn find_users(term: String) -> Result<PreparedResponse<FindUsersRow>, CallError> {
    let client = GleaphClient::with_prepared::<Prepared>(router_id());
    client.find_users(FindUsersParams { term }).await
}

/// Dynamic GQL: name-prefix search built with the [`gql!`] macro.
#[query(composite = true)]
async fn user_names_by_prefix(prefix: String) -> Result<Vec<String>, CallError> {
    let client = GleaphClient::new(router_id());
    let result = client
        .gql_query(gql!(
            "MATCH (n:Person) WHERE n.name STARTS WITH $prefix RETURN n.name AS user_name",
            prefix = prefix,
        ))
        .await?;
    let rows = result
        .decode_serde_rows::<FindUsersRow>()?
        .unwrap_or_default();
    Ok(rows.into_iter().map(|row| row.user_name).collect())
}

/// Idempotent mutation plus read-your-writes: create a person, then read it back through the
/// returned token with `ReadMode::AtLeast`. Reuse `client_mutation_key` only for retries of the
/// same mutation.
#[update]
async fn create_user_and_read(
    user_id: u64,
    user_name: String,
    client_mutation_key: String,
) -> Result<GqlQueryResult, CallError> {
    let client = GleaphClient::new(router_id());
    let write = client
        .gql_mutate(
            gql!(
                "CREATE (n:Person {user_id: $user_id, name: $user_name}) RETURN n.user_id AS user_id",
                user_id = user_id,
                user_name = user_name,
            ),
            client_mutation_key,
        )
        .await?;
    let token: MutationToken = write.token.ok_or_else(|| CallError::Decode {
        message: "mutation did not return a read-your-writes token".into(),
    })?;
    client
        .gql_query_with_mode(
            gql!(
                "MATCH (n:Person {user_id: $user_id}) RETURN n.name AS user_name",
                user_id = user_id,
            ),
            ReadMode::AtLeast(token),
        )
        .await
}
