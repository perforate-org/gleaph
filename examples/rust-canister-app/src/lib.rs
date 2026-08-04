//! Example application canister: `gleaph-codegen` output + `gleaph-cdk`.
//!
//! End-to-end flow:
//!   1. `manifest.json` declares prepared operations.
//!   2. Codegen generates the typed facade into `generated.rs`:
//!      `cargo run -p gleaph-codegen -- --manifest manifest.json --target rust-canister \
//!         --format rust=never --output src/generated.rs`
//!   3. This crate implements the generated [`PreparedCanisterExecutor`] with
//!      [`GleaphClient`] and exposes canister entrypoints that delegate to the Gleaph Router.
//!
//! The three entrypoints demonstrate the API surface:
//! - [`find_users`]: prepared read through the generated facade.
//! - [`user_names_by_prefix`]: dynamic GQL built with the [`gql!`] macro.
//! - [`create_user_and_read`]: idempotent mutation plus read-your-writes via the returned
//!   [`MutationToken`](gleaph_cdk::MutationToken).

use candid::Principal;
use gleaph_cdk::serde::Serialize;
use gleaph_cdk::serde::de::DeserializeOwned;
use gleaph_cdk::{
    CallError, GleaphClient, GqlParams, GqlQueryResult, MutationToken, ReadMode, gql,
};
use ic_cdk_macros::{query, update};

pub mod generated;
use generated::{
    FindUsersParams, FindUsersRow, PreparedCanisterExecutor, PreparedCanisterFuture,
    PreparedCanisterQueries, PreparedCanisterResponse,
};

/// Principal of the deployed Gleaph Router. In production, set this from canister init args or
/// environment configuration.
fn router_id() -> Principal {
    Principal::from_text("rrkah-fqaaa-aaaaa-aaaaq-cai").expect("valid router principal")
}

/// Executor bridging the generated prepared-operation facade to the Router.
struct Executor {
    client: GleaphClient,
}

impl PreparedCanisterExecutor for Executor {
    type Error = CallError;

    fn encode_params<T: Serialize>(&self, _params: &T) -> Result<Vec<u8>, Self::Error> {
        // Only used by operations whose parameters cannot be expressed as logical GQL values
        // (e.g. records); application code serializes its own compact-binary blob here.
        Err(CallError::Decode {
            message: "encode_params is application-specific".into(),
        })
    }

    fn execute_gql<'a, Row>(
        &'a self,
        name: &'static str,
        params: GqlParams,
    ) -> PreparedCanisterFuture<'a, Row, Self::Error>
    where
        Row: DeserializeOwned + 'a,
    {
        Box::pin(async move {
            let params =
                gleaph_cdk::encode_gql_params(params).map_err(|error| CallError::Decode {
                    message: error.to_string(),
                })?;
            let result = self
                .client
                .prepared_query(name, params, None, ReadMode::Eventual)
                .await?;
            response(result)
        })
    }

    fn execute_query<'a, Row>(
        &'a self,
        name: &'static str,
        params: Vec<u8>,
    ) -> PreparedCanisterFuture<'a, Row, Self::Error>
    where
        Row: DeserializeOwned + 'a,
    {
        Box::pin(async move {
            let result = self
                .client
                .prepared_query(name, params, None, ReadMode::Eventual)
                .await?;
            response(result)
        })
    }

    fn execute_update<'a, Row>(
        &'a self,
        name: &'static str,
        params: Vec<u8>,
    ) -> PreparedCanisterFuture<'a, Row, Self::Error>
    where
        Row: DeserializeOwned + 'a,
    {
        Box::pin(async move {
            // Key derivation is application-specific; idempotency requires the same caller +
            // params to produce the same key. Hash params for compact keys in production.
            let key = format!("{}:{params:?}", ic_cdk::api::msg_caller());
            let result = self.client.prepared_mutate(name, params, key).await?;
            response(result)
        })
    }
}

/// Decode the Router response rows into a generated row type.
fn response<Row: DeserializeOwned>(
    result: GqlQueryResult,
) -> Result<PreparedCanisterResponse<Row>, CallError> {
    Ok(PreparedCanisterResponse {
        row_count: result.row_count,
        rows: result.decode_serde_rows::<Row>()?.unwrap_or_default(),
    })
}

/// Prepared read: find users whose name starts with `term`.
#[query(composite = true)]
async fn find_users(term: String) -> Result<PreparedCanisterResponse<FindUsersRow>, CallError> {
    let executor = Executor {
        client: GleaphClient::new(router_id()),
    };
    let queries = PreparedCanisterQueries::new(&executor);
    queries.find_users(FindUsersParams { term }).await
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
