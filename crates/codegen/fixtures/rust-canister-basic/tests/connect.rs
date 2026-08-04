//! Connectivity proof: the generated `PreparedCanisterExecutor` boundary implemented on top of
//! the `gleaph-cdk` Router-aligned client.
//!
//! Hand-written; not part of the generated output. Compiles the executor contract types
//! (`GqlParams`, `PreparedCanisterResponse`, `PreparedCanisterFuture`) against the real client
//! and decodes a simulated Router `prepared_query` response into a generated row type.

use gleaph_cdk::serde::Serialize;
use gleaph_cdk::serde::de::DeserializeOwned;
use gleaph_cdk::{
    CallError, GleaphClient, GqlParams, GqlQueryResult, GqlWireRow, GqlWireRows, GqlWireValue,
    ReadMode,
};
use gleaph_codegen_rust_canister_fixture::{
    FindUsersRow, PreparedCanisterExecutor, PreparedCanisterFuture, PreparedCanisterQueries,
    PreparedCanisterResponse,
};

/// Runtime executor backed by the `gleaph-cdk` Router-aligned client.
struct CdkExecutor {
    client: GleaphClient,
}

impl PreparedCanisterExecutor for CdkExecutor {
    type Error = CallError;

    fn encode_params<T: Serialize>(&self, _params: &T) -> Result<Vec<u8>, Self::Error> {
        // Only used for operations whose parameters cannot be expressed as logical GQL values
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
            Ok(PreparedCanisterResponse {
                row_count: result.row_count,
                rows: decode_rows(&result)?,
            })
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
            Ok(PreparedCanisterResponse {
                row_count: result.row_count,
                rows: decode_rows(&result)?,
            })
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
            let result = self
                .client
                .prepared_mutate(name, params, "connectivity-test")
                .await?;
            Ok(PreparedCanisterResponse {
                row_count: result.row_count,
                rows: decode_rows(&result)?,
            })
        })
    }
}

/// Decode the Router response rows through the serde path used by generated row types.
fn decode_rows<Row: DeserializeOwned>(result: &GqlQueryResult) -> Result<Vec<Row>, CallError> {
    result
        .decode_serde_rows::<Row>()
        .map(|rows| rows.unwrap_or_default())
        .map_err(|error| CallError::Decode {
            message: error.to_string(),
        })
}

#[test]
fn generated_facade_binds_to_the_cdk_executor() {
    let executor = CdkExecutor {
        client: GleaphClient::new(candid::Principal::anonymous()),
    };
    let _queries = PreparedCanisterQueries::new(&executor);
}

#[test]
fn executor_decodes_generated_rows_from_router_envelope() {
    // Simulate the Router's `prepared_query` response blob and run the same decode path the
    // executor uses, without making an inter-canister call.
    let rows = GqlWireRows {
        rows: vec![GqlWireRow {
            columns: vec![("user_name".into(), GqlWireValue::Text("Ada".into()))],
        }],
    };
    let result = GqlQueryResult {
        row_count: 1,
        rows_blob: Some(candid::encode_one(&rows).expect("encode wire rows")),
        phase: None,
        token: None,
    };
    let decoded = decode_rows::<FindUsersRow>(&result).expect("decode rows");
    assert_eq!(decoded.len(), 1);
    assert_eq!(decoded[0].user_name, "Ada");
}
