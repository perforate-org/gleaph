#![allow(dead_code)]

// This file is intentionally shaped like `gleaph-codegen --target rust-canister` output.
// Keep its direct dependencies limited to std and gleaph-cdk.

use std::future::Future;
use std::pin::Pin;
use gleaph_cdk::GqlParams;
use gleaph_cdk::GqlValue;
use gleaph_cdk::serde::de::DeserializeOwned;
use gleaph_cdk::serde::{Deserialize, Serialize};

pub const GLEAPH_GRAPH_ID: &str = "fixture-graph";

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(crate = "gleaph_cdk::serde")]
pub struct PreparedCanisterResponse<Row> {
    pub row_count: u64,
    pub rows: Vec<Row>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(crate = "gleaph_cdk::serde")]
pub enum PreparedPathElement {
    Vertex(Vec<u8>),
    Edge(Vec<u8>),
}

pub trait PreparedCanisterExecutor {
    type Error;

    fn encode_params<T: Serialize>(&self, params: &T) -> Result<Vec<u8>, Self::Error>;

    fn execute_gql<'a, Row>(
        &'a self,
        name: &'static str,
        params: GqlParams,
    ) -> Pin<Box<dyn Future<Output = Result<PreparedCanisterResponse<Row>, Self::Error>> + 'a>>
    where
        Row: DeserializeOwned + 'a;

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
    #[serde(rename = "signed")]
    pub signed: gleaph_cdk::GqlInt256,
    #[serde(rename = "unsigned")]
    pub unsigned: gleaph_cdk::GqlUint256,
    #[serde(rename = "price")]
    pub price: gleaph_cdk::GqlDecimal,
    #[serde(rename = "owner")]
    pub owner: gleaph_cdk::GqlPrincipal,
    #[serde(rename = "route")]
    pub route: Vec<PreparedPathElement>,
}

impl FindUsersParams {
    pub fn into_gql_params(self) -> GqlParams {
        vec![
            ("limit".to_string(), GqlValue::Uint64(self.limit)),
            ("signed".to_string(), GqlValue::Int256(self.signed.into_inner())),
            (
                "unsigned".to_string(),
                GqlValue::Uint256(self.unsigned.into_inner()),
            ),
            ("price".to_string(), GqlValue::Decimal(self.price.into_inner())),
            (
                "owner".to_string(),
                gleaph_cdk::gql_principal_value(self.owner),
            ),
            (
                "route".to_string(),
                GqlValue::Path(
                    self.route
                        .into_iter()
                        .map(|value| match value {
                            PreparedPathElement::Vertex(value) => {
                                gleaph_cdk::GqlPathElement::Vertex(value.into())
                            }
                            PreparedPathElement::Edge(value) => {
                                gleaph_cdk::GqlPathElement::Edge(value.into())
                            }
                        })
                        .collect(),
                ),
            ),
        ]
    }
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
        self.executor
            .execute_gql::<FindUsersRow>("find-users", params.into_gql_params())
            .await
    }
}

pub fn accepts_shared_gql_params(_: GqlParams) {}
