#![allow(dead_code)]

// This file is intentionally shaped like `gleaph-codegen --target rust-canister` output.
// Keep its direct dependencies limited to std and gleaph-cdk.

use std::future::Future;
use std::pin::Pin;
use gleaph_cdk::GqlParams;
use gleaph_cdk::serde::de::DeserializeOwned;
use gleaph_cdk::serde::{Deserialize, Serialize};

pub const GLEAPH_GRAPH_ID: &str = "fixture-graph";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(crate = "gleaph_cdk::serde")]
pub struct PreparedCanisterResponse<Row> {
    pub row_count: u64,
    pub rows: Vec<Row>,
}

pub trait PreparedCanisterExecutor {
    type Error;

    fn encode_params<T: Serialize>(&self, params: &T) -> Result<Vec<u8>, Self::Error>;

    fn execute_query<'a, Row>(
        &'a self,
        name: &'static str,
        params: Vec<u8>,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedCanisterResponse<Row>, Self::Error>> + 'a>>
    where
        Row: DeserializeOwned + 'a;
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(crate = "gleaph_cdk::serde")]
pub struct FindUsersParams {
    #[serde(rename = "limit")]
    pub limit: u64,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(crate = "gleaph_cdk::serde")]
pub struct FindUsersRow {
    #[serde(rename = "user_name")]
    pub user_name: String,
    #[serde(rename = "score")]
    pub score: gleaph_cdk::GqlFloat256,
}

pub struct PreparedCanisterQueries<'a, E: PreparedCanisterExecutor> {
    executor: &'a E,
}

impl<'a, E: PreparedCanisterExecutor> PreparedCanisterQueries<'a, E> {
    pub fn new(executor: &'a E) -> Self {
        Self { executor }
    }

    pub async fn find_users(
        &self,
        params: FindUsersParams,
    ) -> Result<PreparedCanisterResponse<FindUsersRow>, E::Error> {
        let params = self.executor.encode_params(&params)?;
        self.executor.execute_query::<FindUsersRow>("find-users", params).await
    }
}

pub fn accepts_shared_gql_params(_: GqlParams) {}
