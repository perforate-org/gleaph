//! Gleaph canister SDK.
//!
//! Small, opinionated helpers for application canisters that delegate GQL execution to the
//! Gleaph Router via bounded-wait inter-canister calls. The query-building layer ([`gql!`],
//! [`IntoGqlParam`], [`GqlQuery`]) is general-purpose and lives in `gleaph-gql-params`; this
//! crate adds the Router-bound client ([`GleaphClient`]), the wire types it needs, and the
//! typed-binding helpers used by `gleaph-codegen` output.

#![cfg_attr(feature = "nightly-f128", feature(f128))]

use candid::{CandidType, Deserialize, Principal};
use std::marker::PhantomData;

pub mod types;

/// Query-building helpers re-exported from `gleaph-gql-params`.
pub use gleaph_gql_params::{
    GqlParams, GqlQuery, IntoGqlParam, encode_gql_params, gql, gql_param_value,
};

/// Serde facade used by generated canister bindings.
pub use serde;

/// JSON facade used by generated canister bindings for open record values.
pub use serde_json;

/// Candid facade used by generated canister bindings for entrypoint arguments and results.
pub use candid;

pub use types::{
    AtomicInsertPropertyV1, AtomicInsertReceiptV1, AtomicInsertVertexV1, BulkLoadChunkReceiptV1,
    BulkLoadChunkV1, BulkLoadCommand, BulkLoadEdgeV1, BulkLoadEndpointV1,
    BulkLoadPropertyEndpointV1, BulkLoadPublicStateV1, BulkLoadResponse, BulkLoadStatusPage,
    EdgePathElementId, Float256, FromGqlRow, GqlDecimal, GqlFloat16, GqlFloat256, GqlInt256,
    GqlPrincipal, GqlQueryResult, GqlRecord, GqlRow, GqlUint256, GqlValue, GqlWireDecodeError,
    GqlWireRow, GqlWireRows, GqlWireValue, MutationLifecyclePhase, MutationToken,
    MutationTokenShard, PathElement, ReadMode, RouterError, VectorActivationBlockReason,
    VertexPathElementId, gql_principal_from_value, gql_record_to_json_map, gql_value_to_json,
    gql_wire_value_to_json, take_gql_row_field,
};

/// Binary128 row binding carrying the canonical little-endian wire form.
#[cfg(feature = "nightly-f128")]
pub use types::Float128;

/// Prepared-operation wire types shared with the Router.
pub use gleaph_prepared_api::{PreparedManifest, PreparedOperation, PreparedSortSpec};

/// Marker for a client without generated prepared operations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoPrepared;

/// Canister-id-bound client for dynamic GQL and prepared operations.
///
/// The `Prepared` type parameter marks whether generated prepared operations are available:
/// [`GleaphClient::new`] yields [`GleaphClient<NoPrepared>`](Self), while
/// [`GleaphClient::with_prepared`] enables the operations generated for `Prepared` (see the
/// `PreparedExt` trait emitted by `gleaph-codegen`).
pub struct GleaphClient<Prepared = NoPrepared> {
    canister_id: Principal,
    _prepared: PhantomData<Prepared>,
}

impl<Prepared> Clone for GleaphClient<Prepared> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<Prepared> Copy for GleaphClient<Prepared> {}

impl<Prepared> std::fmt::Debug for GleaphClient<Prepared> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GleaphClient")
            .field("canister_id", &self.canister_id)
            .finish()
    }
}

impl<Prepared> PartialEq for GleaphClient<Prepared> {
    fn eq(&self, other: &Self) -> bool {
        self.canister_id == other.canister_id
    }
}

impl<Prepared> Eq for GleaphClient<Prepared> {}

impl GleaphClient<NoPrepared> {
    /// Bind the client to a Router canister without generated prepared operations.
    pub const fn new(canister_id: Principal) -> Self {
        Self {
            canister_id,
            _prepared: PhantomData,
        }
    }

    /// Bind the client and enable the generated prepared operations for `Prepared`.
    pub fn with_prepared<Prepared>(canister_id: Principal) -> GleaphClient<Prepared> {
        GleaphClient {
            canister_id,
            _prepared: PhantomData,
        }
    }
}

impl<Prepared> GleaphClient<Prepared> {
    /// Execute dynamic GQL through the configured Router canister.
    ///
    /// Reads use the default [`ReadMode::Eventual`] freshness contract; use
    /// [`gql_query_with_mode`](Self::gql_query_with_mode) for read-your-writes.
    pub async fn gql_query(&self, query: impl Into<GqlQuery>) -> Result<GqlQueryResult, CallError> {
        self.gql_query_with_mode(query, ReadMode::Eventual).await
    }

    /// Execute dynamic GQL with an explicit read freshness contract.
    pub async fn gql_query_with_mode(
        &self,
        query: impl Into<GqlQuery>,
        read_mode: ReadMode,
    ) -> Result<GqlQueryResult, CallError> {
        let query = query.into();
        let params = encode_gql_params(query.params).map_err(|error| CallError::Decode {
            message: format!("failed to encode GQL params: {error:?}"),
        })?;
        let args = candid::utils::encode_args((query.query, params, read_mode))
            .expect("Candid encode GQL query arguments");
        let result: Result<GqlQueryResult, RouterError> =
            call_router(self.canister_id, "gql_query", args).await?;
        result.map_err(CallError::Router)
    }

    /// Execute an idempotent dynamic GQL mutation.
    ///
    /// Reuse `client_mutation_key` only for retries of the same mutation. The returned
    /// [`GqlQueryResult`] carries the ADR 0029 federated mutation lifecycle `phase` and the
    /// read-your-writes [`MutationToken`].
    pub async fn gql_mutate(
        &self,
        query: impl Into<GqlQuery>,
        client_mutation_key: impl Into<String>,
    ) -> Result<GqlQueryResult, CallError> {
        let query = query.into();
        let params = encode_gql_params(query.params).map_err(|error| CallError::Decode {
            message: format!("failed to encode GQL params: {error:?}"),
        })?;
        let args = candid::utils::encode_args((query.query, params, client_mutation_key.into()))
            .expect("Candid encode GQL mutate arguments");
        let result: Result<GqlQueryResult, RouterError> =
            call_router(self.canister_id, "gql_mutate", args).await?;
        result.map_err(CallError::Router)
    }

    /// Execute a named prepared query through the configured Router canister.
    ///
    /// Mirrors the Router `prepared_query` signature: raw compact-binary `params` and an
    /// optional caller-selected sort.
    pub async fn prepared_query(
        &self,
        name: impl Into<String>,
        params: Vec<u8>,
        sort: Option<Vec<PreparedSortSpec>>,
        read_mode: ReadMode,
    ) -> Result<GqlQueryResult, CallError> {
        let args = candid::utils::encode_args((name.into(), params, sort, read_mode))
            .expect("Candid encode prepared query arguments");
        let result: Result<GqlQueryResult, RouterError> =
            call_router(self.canister_id, "prepared_query", args).await?;
        result.map_err(CallError::Router)
    }

    /// Execute an idempotent named prepared mutation.
    pub async fn prepared_mutate(
        &self,
        name: impl Into<String>,
        params: Vec<u8>,
        client_mutation_key: impl Into<String>,
    ) -> Result<GqlQueryResult, CallError> {
        let args = candid::utils::encode_args((name.into(), params, client_mutation_key.into()))
            .expect("Candid encode prepared mutation arguments");
        let result: Result<GqlQueryResult, RouterError> =
            call_router(self.canister_id, "prepared_mutate", args).await?;
        result.map_err(CallError::Router)
    }

    /// Execute one durable Router bulk-load command (ADR 0057).
    pub async fn bulk_load(&self, command: BulkLoadCommand) -> Result<BulkLoadResponse, CallError> {
        let args = candid::utils::encode_args((command,)).expect("Candid encode bulk-load command");
        let result: Result<BulkLoadResponse, RouterError> =
            call_router(self.canister_id, "bulk_load", args).await?;
        result.map_err(CallError::Router)
    }

    /// Read one bounded page of durable Router bulk-load status.
    pub async fn bulk_load_status(
        &self,
        graph_name: Option<String>,
        client_bulk_key: impl Into<String>,
        receipt_cursor: Option<u32>,
        max_receipts: u32,
    ) -> Result<BulkLoadStatusPage, CallError> {
        let args = candid::utils::encode_args((
            graph_name,
            client_bulk_key.into(),
            receipt_cursor,
            max_receipts,
        ))
        .expect("Candid encode bulk-load status arguments");
        let result: Result<BulkLoadStatusPage, RouterError> =
            call_router(self.canister_id, "bulk_load_status", args).await?;
        result.map_err(CallError::Router)
    }

    /// Register or replace one named prepared operation (idempotent upsert).
    pub async fn prepare(
        &self,
        name: impl Into<String>,
        query: impl Into<String>,
        metadata: Option<PreparedOperation>,
    ) -> Result<(), CallError> {
        let args = candid::utils::encode_args((name.into(), query.into(), metadata))
            .expect("Candid encode prepare arguments");
        let result: Result<(), RouterError> =
            call_router(self.canister_id, "prepare", args).await?;
        result.map_err(CallError::Router)
    }

    /// Remove one named prepared operation.
    pub async fn drop_prepared(&self, name: impl Into<String>) -> Result<(), CallError> {
        let args =
            candid::utils::encode_args((name.into(),)).expect("Candid encode drop-prepared args");
        let result: Result<(), RouterError> =
            call_router(self.canister_id, "drop_prepared", args).await?;
        result.map_err(CallError::Router)
    }

    /// The full prepared-operation manifest for one graph.
    pub async fn list_prepared(
        &self,
        graph_name: Option<String>,
    ) -> Result<PreparedManifest, CallError> {
        let args =
            candid::utils::encode_args((graph_name,)).expect("Candid encode list-prepared args");
        let result: Result<PreparedManifest, RouterError> =
            call_router(self.canister_id, "list_prepared", args).await?;
        result.map_err(CallError::Router)
    }
}

/// Error returned when a Router inter-canister call fails before yielding a typed result.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The IC rejected the call (timeout, canister error, destination invalid, etc.).
    Reject {
        /// Reject code as a human-readable string; the textual form of
        /// [`ic_cdk::call::RejectCode`] when available, or the raw numeric code otherwise.
        code: String,
        /// Reject or transport-level error message.
        message: String,
    },
    /// The call succeeded at the transport layer but the response could not be Candid-decoded.
    Decode {
        /// Candid decode error message.
        message: String,
    },
    /// The call succeeded and the Router rejected it with a structured error.
    Router(RouterError),
}

impl core::fmt::Display for CallError {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            Self::Reject { code, message } => {
                write!(formatter, "Router call rejected ({code}): {message}")
            }
            Self::Decode { message } => {
                write!(formatter, "failed to decode Router response: {message}")
            }
            Self::Router(error) => write!(formatter, "Router error: {error:?}"),
        }
    }
}

impl std::error::Error for CallError {}

impl From<GqlWireDecodeError> for CallError {
    fn from(error: GqlWireDecodeError) -> Self {
        CallError::Decode {
            message: error.to_string(),
        }
    }
}

/// Make a bounded-wait call to `method` on `canister_id` and Candid-decode the response as `R`.
async fn call_router<R>(canister_id: Principal, method: &str, args: Vec<u8>) -> Result<R, CallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    use ic_cdk::call::{CallFailed, Response};

    let call_result: Result<Response, CallFailed> =
        ic_cdk::call::Call::bounded_wait(canister_id, method)
            .with_raw_args(&args)
            .await;

    match call_result {
        Ok(response) => response.candid().map_err(|error| CallError::Decode {
            message: error.to_string(),
        }),
        Err(CallFailed::CallRejected(rejected)) => Err(CallError::Reject {
            code: rejected
                .reject_code()
                .map(|code| format!("{code:?}"))
                .unwrap_or_else(|_| rejected.raw_reject_code().to_string()),
            message: rejected.reject_message().to_string(),
        }),
        Err(other) => Err(CallError::Reject {
            code: "CallFailed".to_string(),
            message: other.to_string(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql_ic_wire::{GqlWireRow, GqlWireValue};

    #[test]
    fn call_error_display_contains_context() {
        let err = CallError::Reject {
            code: "CanisterReject".to_string(),
            message: "no query".to_string(),
        };
        let text = format!("{err}");
        assert!(text.contains("no query"), "{text}");
        assert!(text.contains("CanisterReject"), "{text}");
    }

    #[test]
    fn call_error_converts_wire_decode_errors() {
        let error = GqlWireDecodeError::MissingField("name".into());
        let call_error = CallError::from(error);
        assert!(matches!(call_error, CallError::Decode { message } if message.contains("name")));
    }

    #[test]
    fn logical_gql_params_round_trip_preserves_order() {
        let params = vec![
            ("first".to_string(), GqlValue::Int8(1)),
            ("second".to_string(), GqlValue::Text("two".to_string())),
        ];
        let encoded = encode_gql_params(params.clone()).expect("encode GQL params");
        let decoded = GqlValue::from_binary_bytes(&encoded).expect("decode GQL params");
        assert_eq!(decoded, GqlValue::Record(params));
    }

    #[test]
    fn gql_query_result_envelope_round_trips() {
        let result = GqlQueryResult {
            row_count: 2,
            rows_blob: Some(vec![1, 2, 3]),
            phase: None,
            token: None,
        };
        let bytes = candid::encode_one(&result).expect("encode result");
        let decoded: GqlQueryResult = candid::decode_one(&bytes).expect("decode result");
        assert_eq!(decoded, result);
        assert_eq!(decoded.rows_blob(), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn gql_query_result_decodes_from_router_result_envelope() {
        // The Router responds with `Result<GqlQueryResult, RouterError>`; the CDK must decode
        // through the Candid Ok/Err variant, matching the wire shape exactly.
        let ok = Ok::<GqlQueryResult, RouterError>(GqlQueryResult {
            row_count: 1,
            rows_blob: None,
            phase: Some(MutationLifecyclePhase::Completed),
            token: None,
        });
        let bytes = candid::encode_one(&ok).expect("encode ok envelope");
        let decoded: Result<GqlQueryResult, RouterError> =
            candid::decode_one(&bytes).expect("decode ok envelope");
        assert_eq!(decoded, ok);

        let err = Err::<GqlQueryResult, RouterError>(RouterError::ProjectionLag {
            shard_id: 3,
            watermark: "label_stats".into(),
            required: 10,
            current: 7,
        });
        let bytes = candid::encode_one(&err).expect("encode err envelope");
        let decoded: Result<GqlQueryResult, RouterError> =
            candid::decode_one(&bytes).expect("decode err envelope");
        assert_eq!(decoded, err);
    }

    #[test]
    fn router_error_mirror_round_trips_candid() {
        let errors = [
            RouterError::NotAuthorized,
            RouterError::NotFound("graph".into()),
            RouterError::Busy {
                operation: "bulk_load.append".into(),
            },
            RouterError::UnsupportedMultiDmlBundle {
                dml_statements: 2,
                shard_count: 4,
            },
            RouterError::VectorDispatchActivationBlocked(
                VectorActivationBlockReason::DispatchNotActivated,
            ),
            RouterError::AckConflict { stored: 7 },
            RouterError::Internal("boom".into()),
        ];
        for error in errors {
            let bytes = candid::encode_one(&error).expect("encode router error");
            let decoded: RouterError = candid::decode_one(&bytes).expect("decode router error");
            assert_eq!(decoded, error);
        }
    }

    #[test]
    fn mutation_lifecycle_phase_round_trips_candid() {
        let phases = [
            MutationLifecyclePhase::Routing,
            MutationLifecyclePhase::CanonicalPending,
            MutationLifecyclePhase::CanonicalCommitted,
            MutationLifecyclePhase::ProjectionPending,
            MutationLifecyclePhase::Completed,
            MutationLifecyclePhase::Failed,
        ];
        for phase in phases {
            let bytes = candid::encode_one(phase).expect("encode phase");
            let decoded: MutationLifecyclePhase = candid::decode_one(&bytes).expect("decode phase");
            assert_eq!(decoded, phase);
        }
    }

    #[test]
    fn prepared_query_args_round_trip() {
        let args = candid::utils::encode_args((
            "alice_home_feed".to_string(),
            vec![0_u8, 1, 2],
            Some(vec![PreparedSortSpec {
                key: "created_at".into(),
                direction: "desc".into(),
            }]),
            ReadMode::Eventual,
        ))
        .expect("encode prepared query args");
        let decoded: (String, Vec<u8>, Option<Vec<PreparedSortSpec>>, ReadMode) =
            candid::utils::decode_args(&args).expect("decode prepared query args");
        assert_eq!(
            decoded,
            (
                "alice_home_feed".into(),
                vec![0, 1, 2],
                Some(vec![PreparedSortSpec {
                    key: "created_at".into(),
                    direction: "desc".into(),
                }]),
                ReadMode::Eventual
            )
        );
    }

    #[test]
    fn bulk_load_command_round_trips_candid() {
        let command = BulkLoadCommand::Append {
            graph_name: Some("tenant.main".into()),
            client_bulk_key: "bulk-1".into(),
            chunk_index: 3,
            chunk: BulkLoadChunkV1::Edges(vec![BulkLoadEdgeV1 {
                source: BulkLoadEndpointV1::ByProperty(BulkLoadPropertyEndpointV1 {
                    vertex_label: "Person".into(),
                    property_name: "email".into(),
                    value: vec![1, 2, 3],
                }),
                target: BulkLoadEndpointV1::Existing(vec![9; 16]),
                directed: true,
                edge_label_name: Some("follows".into()),
                inline_property: Some(vec![7, 8]),
                initial_edge_properties: vec![AtomicInsertPropertyV1 {
                    property_name: "weight".into(),
                    value: vec![9],
                }],
            }]),
        };
        let bytes = candid::encode_one(&command).expect("encode bulk-load command");
        let decoded: BulkLoadCommand =
            candid::decode_one(&bytes).expect("decode bulk-load command");
        assert_eq!(decoded, command);
    }

    #[test]
    fn bulk_load_status_args_round_trip() {
        let args = candid::utils::encode_args((
            Some("tenant.main".to_string()),
            "bulk-1".to_string(),
            Some(8_u32),
            16_u32,
        ))
        .expect("encode bulk-load status args");
        let decoded: (Option<String>, String, Option<u32>, u32) =
            candid::utils::decode_args(&args).expect("decode bulk-load status args");
        assert_eq!(
            decoded,
            (Some("tenant.main".into()), "bulk-1".into(), Some(8), 16)
        );
    }

    #[test]
    fn gql_mutate_args_round_trip() {
        let args = candid::utils::encode_args((
            "MATCH (n) RETURN n".to_string(),
            vec![1_u8, 2, 3],
            "request-1".to_string(),
        ))
        .expect("encode gql mutate args");
        let decoded: (String, Vec<u8>, String) =
            candid::utils::decode_args(&args).expect("decode gql mutate args");
        assert_eq!(
            decoded,
            (
                "MATCH (n) RETURN n".into(),
                vec![1, 2, 3],
                "request-1".into()
            )
        );
    }

    #[test]
    fn gql_response_decodes_wire_rows() {
        let rows = GqlWireRows {
            rows: vec![GqlWireRow {
                columns: vec![("name".into(), GqlWireValue::Text("Ada".into()))],
            }],
        };
        let response = GqlQueryResult {
            row_count: 1,
            rows_blob: Some(candid::encode_one(&rows).expect("encode rows")),
            phase: None,
            token: None,
        };
        assert_eq!(response.decode_rows().unwrap(), Some(rows));
    }

    #[derive(Debug, PartialEq)]
    struct TypedRow {
        name: String,
    }

    impl FromGqlRow for TypedRow {
        fn from_gql_row(mut row: GqlRow) -> Result<Self, GqlWireDecodeError> {
            let value = take_gql_row_field(&mut row, "name")?;
            match value {
                GqlValue::Text(name) => Ok(Self { name }),
                _ => Err(GqlWireDecodeError::TypeMismatch("Text")),
            }
        }
    }

    #[derive(serde::Deserialize, Debug, PartialEq)]
    struct SerdeRow {
        #[serde(rename = "name")]
        name: String,
    }

    #[test]
    fn gql_query_result_decodes_serde_rows() {
        let rows = GqlWireRows {
            rows: vec![GqlWireRow {
                columns: vec![("name".into(), GqlWireValue::Text("Ada".into()))],
            }],
        };
        let response = GqlQueryResult {
            row_count: 1,
            rows_blob: Some(candid::encode_one(&rows).expect("encode rows")),
            phase: None,
            token: None,
        };
        assert_eq!(
            response.decode_serde_rows::<SerdeRow>().unwrap(),
            Some(vec![SerdeRow { name: "Ada".into() }])
        );
    }

    #[test]
    fn gql_response_decodes_typed_rows() {
        let rows = GqlWireRows {
            rows: vec![GqlWireRow {
                columns: vec![("name".into(), GqlWireValue::Text("Ada".into()))],
            }],
        };
        let response = GqlQueryResult {
            row_count: 1,
            rows_blob: Some(candid::encode_one(&rows).expect("encode rows")),
            phase: None,
            token: None,
        };
        assert_eq!(
            response.decode_typed_rows::<TypedRow>().unwrap(),
            Some(vec![TypedRow { name: "Ada".into() }])
        );
    }

    #[test]
    fn duplicate_typed_row_fields_are_rejected() {
        let mut row = vec![
            ("name".into(), GqlValue::Text("Ada".into())),
            ("name".into(), GqlValue::Text("Grace".into())),
        ];
        assert_eq!(
            take_gql_row_field(&mut row, "name"),
            Err(GqlWireDecodeError::DuplicateField("name".into()))
        );
    }

    #[test]
    fn logical_gql_records_project_to_open_json_maps() {
        let value = GqlValue::Record(vec![
            ("name".into(), GqlValue::Text("Ada".into())),
            ("active".into(), GqlValue::Bool(true)),
            (
                "nested".into(),
                GqlValue::List(vec![GqlValue::Int8(1), GqlValue::Int8(2)]),
            ),
            ("bytes".into(), GqlValue::Bytes(vec![1, 2, 3])),
        ]);

        let projected = gql_record_to_json_map(value).expect("project record");
        assert_eq!(projected["name"], serde_json::json!("Ada"));
        assert_eq!(projected["active"], serde_json::json!(true));
        assert_eq!(projected["nested"], serde_json::json!([1, 2]));
        assert_eq!(projected["bytes"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn logical_gql_records_reject_duplicate_fields() {
        let value = GqlValue::Record(vec![
            ("name".into(), GqlValue::Text("Ada".into())),
            ("name".into(), GqlValue::Text("Grace".into())),
        ]);

        assert_eq!(
            gql_record_to_json_map(value),
            Err(GqlWireDecodeError::DuplicateField("name".into()))
        );
    }

    #[test]
    fn value_bindings_build_logical_values_from_real_types() {
        assert_eq!(
            GqlValue::from(GqlFloat16::from(half::f16::from_bits(0x3c00)).into_inner()),
            GqlValue::Float16(half::f16::from_bits(0x3c00))
        );
        assert!(matches!(
            GqlValue::from(GqlInt256::from(ethnum::I256::from(-7)).into_inner()),
            GqlValue::Int256(_)
        ));
        assert!(matches!(
            GqlValue::from(GqlUint256::from(ethnum::U256::from(7u64)).into_inner()),
            GqlValue::Uint256(_)
        ));
        assert!(matches!(
            GqlValue::from(
                GqlDecimal::from("123.4500".parse::<rust_decimal::Decimal>().unwrap()).into_inner()
            ),
            GqlValue::Decimal(_)
        ));
        assert!(matches!(
            GqlValue::from(f256::f256::from(1.25_f64)),
            GqlValue::Float256(_)
        ));
    }

    #[test]
    fn row_binding_wrappers_round_trip_serde_and_candid() {
        let value = GqlInt256::from(ethnum::I256::from(-42));
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(json, "\"-42\"");
        assert_eq!(serde_json::from_str::<GqlInt256>(&json).unwrap(), value);

        let value = GqlDecimal::from("1.25".parse::<rust_decimal::Decimal>().unwrap());
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(serde_json::from_str::<GqlDecimal>(&json).unwrap(), value);

        let value = GqlFloat16::from(half::f16::from_bits(0x3c00));
        let json = serde_json::to_string(&value).unwrap();
        assert_eq!(json, "15360");
        assert_eq!(serde_json::from_str::<GqlFloat16>(&json).unwrap(), value);

        let bytes = [0xabu8; 32];
        let value = Float256::from_le_bytes(bytes);
        let encoded = candid::encode_one(value).unwrap();
        assert_eq!(candid::decode_one::<Float256>(&encoded).unwrap(), value);
    }

    #[test]
    fn principal_value_wraps_the_shared_extension() {
        let anonymous = Principal::anonymous();
        let value = GqlValue::Extension(Box::new(GqlPrincipal::from_inner(anonymous)));
        let parsed = gql_principal_from_value(value).expect("parse principal extension");
        assert_eq!(parsed.into_inner(), anonymous);
    }

    #[test]
    fn wire_value_projection_matches_generated_row_shapes() {
        let rows = GqlWireRows {
            rows: vec![GqlWireRow {
                columns: vec![
                    ("name".into(), GqlWireValue::Text("Ada".into())),
                    (
                        "big".into(),
                        GqlWireValue::Int256(ethnum::I256::from(42).to_le_bytes().to_vec()),
                    ),
                    ("wide".into(), GqlWireValue::Int128(i128::from(i64::MAX))),
                    (
                        "price".into(),
                        GqlWireValue::Decimal(
                            "123.45"
                                .parse::<rust_decimal::Decimal>()
                                .unwrap()
                                .serialize()
                                .to_vec(),
                        ),
                    ),
                    ("bits".into(), GqlWireValue::Float16(0x3c00)),
                    ("raw".into(), GqlWireValue::Float256(vec![0xabu8; 32])),
                    ("data".into(), GqlWireValue::Bytes(vec![1, 2, 3])),
                    (
                        "owner".into(),
                        GqlWireValue::Principal(Principal::anonymous().as_slice().to_vec()),
                    ),
                ],
            }],
        };

        #[derive(serde::Deserialize, Debug, PartialEq)]
        struct WireRow {
            name: String,
            big: GqlInt256,
            wide: i128,
            price: GqlDecimal,
            bits: GqlFloat16,
            raw: Float256,
            data: Vec<u8>,
            owner: Principal,
        }

        let response = GqlQueryResult {
            row_count: 1,
            rows_blob: Some(candid::encode_one(&rows).expect("encode rows")),
            phase: None,
            token: None,
        };
        assert_eq!(
            response.decode_serde_rows::<WireRow>().unwrap(),
            Some(vec![WireRow {
                name: "Ada".into(),
                big: GqlInt256::from(ethnum::I256::from(42)),
                wide: i128::from(i64::MAX),
                price: GqlDecimal::from("123.45".parse::<rust_decimal::Decimal>().unwrap()),
                bits: GqlFloat16::from(half::f16::from_bits(0x3c00)),
                raw: Float256::from_le_bytes([0xabu8; 32]),
                data: vec![1, 2, 3],
                owner: Principal::anonymous(),
            }])
        );
    }
}
