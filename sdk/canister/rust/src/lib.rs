//! Gleaph canister SDK.
//!
//! Small, opinionated helpers for application canisters that delegate fixed read scenarios to the
//! Gleaph Router via its `prepared_execute_query` interface. The API is intentionally generic: it
//! knows the Candid shape of a prepared query call `(String name, Vec<u8> params)` and how to make
//! a bounded-wait inter-canister call, but it does not know application-specific scenario names or
//! semantics.

use candid::{CandidType, Deserialize, Principal};

/// Logical GQL value shared by dynamic GQL, prepared operations, and procedures.
pub use gleaph_gql::Value as GqlValue;

/// Ordered GQL record representation.
///
/// GQL record order is retained because the compact wire representation preserves field order.
pub type GqlRecord = Vec<(String, GqlValue)>;

/// Named GQL parameters, usable by dynamic GQL and prepared operations.
pub type GqlParams = GqlRecord;

/// One GQL result row.
pub type GqlRow = GqlRecord;

/// Path element in a logical GQL value.
pub use gleaph_gql::types::PathElement as GqlPathElement;

/// Error returned when logical GQL parameters cannot be compact-binary encoded.
pub type GqlEncodingError = gleaph_gql::ValueBinaryError;

/// Encode ordered named GQL parameters for a dynamic GQL or prepared-query call.
pub fn encode_gql_params(params: GqlParams) -> Result<Vec<u8>, GqlEncodingError> {
    GqlValue::Record(params).to_binary_bytes()
}

/// Error returned when a prepared-query inter-canister call fails before yielding a typed result.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum PreparedCallError {
    /// The IC rejected the call (timeout, canister error, destination invalid, etc.).
    Reject {
        /// Reject code as a human-readable string. This is the textual form of
        /// [`ic_cdk::call::RejectCode`] when available, or the raw numeric code otherwise.
        code: String,
        /// Reject or transport-level error message.
        message: String,
    },
    /// The call succeeded at the transport layer but the response could not be Candid-decoded as `R`.
    Decode {
        /// Candid decode error message.
        message: String,
    },
}

impl core::fmt::Display for PreparedCallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PreparedCallError::Reject { code, message } => {
                write!(f, "prepared query rejected ({code}): {message}")
            }
            PreparedCallError::Decode { message } => {
                write!(f, "failed to decode prepared query response: {message}")
            }
        }
    }
}

/// Candid-encode the `(String, Vec<u8>)` argument tuple used by Router prepared queries.
///
/// # Panics
///
/// Panics only if the inputs somehow fail to encode, which cannot happen for the `(String, Vec<u8>)`
/// tuple under normal Candid operation.
pub fn encode_prepared_query_args(name: impl Into<String>, params: Vec<u8>) -> Vec<u8> {
    candid::utils::encode_args((name.into(), params)).expect("Candid encode (String, Vec<u8>)")
}

/// Make a bounded-wait call to `prepared_execute_query` on `canister_id`.
///
/// `name` is the registered prepared-query name; `params` is the compact-binary GQL parameter blob
/// produced by the caller (often via `gleaph-gql-ic`). On success the Router's return value is
/// Candid-decoded into `R`, which is typically `Result<T, RouterError>`.
pub async fn call_prepared_query<R>(
    canister_id: Principal,
    name: impl Into<String>,
    params: Vec<u8>,
) -> Result<R, PreparedCallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    let args = encode_prepared_query_args(name, params);
    call_prepared_method(canister_id, "prepared_execute_query", args).await
}

/// Make a bounded-wait call to the Router's dynamic `gql_query` endpoint.
pub async fn call_gql_query<R>(
    canister_id: Principal,
    query: impl Into<String>,
    params: GqlParams,
) -> Result<R, PreparedCallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    let params = encode_gql_params(params).map_err(|error| PreparedCallError::Decode {
        message: format!("failed to encode GQL params: {error:?}"),
    })?;
    let args = candid::utils::encode_args((query.into(), params))
        .expect("Candid encode GQL query arguments");
    call_prepared_method(canister_id, "gql_query", args).await
}

/// Make a bounded-wait call to `prepared_execute_update` on `canister_id`.
///
/// The returned type is normally `u64` for the current Router endpoint. The generic form keeps
/// the helper usable with a future result-wire contract without duplicating call error handling.
pub async fn call_prepared_update<R>(
    canister_id: Principal,
    name: impl Into<String>,
    params: Vec<u8>,
) -> Result<R, PreparedCallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    let args = encode_prepared_query_args(name, params);
    call_prepared_method(canister_id, "prepared_execute_update", args).await
}

/// Make a bounded-wait call to `prepared_execute_update_idempotent` on `canister_id`.
pub async fn call_prepared_update_idempotent<R>(
    canister_id: Principal,
    name: impl Into<String>,
    params: Vec<u8>,
    client_mutation_key: impl Into<String>,
) -> Result<R, PreparedCallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    let args = candid::utils::encode_args((name.into(), params, client_mutation_key.into()))
        .expect("Candid encode prepared idempotent update arguments");
    call_prepared_method(canister_id, "prepared_execute_update_idempotent", args).await
}

async fn call_prepared_method<R>(
    canister_id: Principal,
    method: &str,
    args: Vec<u8>,
) -> Result<R, PreparedCallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    use ic_cdk::call::{CallFailed, Response};

    let call_result: Result<Response, CallFailed> =
        ic_cdk::call::Call::bounded_wait(canister_id, method)
            .with_raw_args(&args)
            .await;

    match call_result {
        Ok(response) => response.candid().map_err(|e| PreparedCallError::Decode {
            message: e.to_string(),
        }),
        Err(CallFailed::CallRejected(rejected)) => Err(PreparedCallError::Reject {
            code: rejected
                .reject_code()
                .map(|code| format!("{code:?}"))
                .unwrap_or_else(|_| rejected.raw_reject_code().to_string()),
            message: rejected.reject_message().to_string(),
        }),
        Err(other) => Err(PreparedCallError::Reject {
            code: "CallFailed".to_string(),
            message: other.to_string(),
        }),
    }
}

/// Thin canister-id-bound wrapper around [`call_prepared_query`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PreparedQueryClient {
    canister_id: Principal,
}

impl PreparedQueryClient {
    /// Bind the client to a Router canister.
    pub const fn new(canister_id: Principal) -> Self {
        Self { canister_id }
    }

    /// Execute a named prepared query through the configured Router canister.
    pub async fn execute<R>(
        &self,
        name: impl Into<String>,
        params: Vec<u8>,
    ) -> Result<R, PreparedCallError>
    where
        R: CandidType + for<'de> Deserialize<'de>,
    {
        call_prepared_query(self.canister_id, name, params).await
    }

    /// Execute a named prepared query from logical GQL parameters.
    pub async fn execute_gql_params<R>(
        &self,
        name: impl Into<String>,
        params: GqlParams,
    ) -> Result<R, PreparedCallError>
    where
        R: CandidType + for<'de> Deserialize<'de>,
    {
        let params = encode_gql_params(params).map_err(|error| PreparedCallError::Decode {
            message: format!("failed to encode GQL params: {error:?}"),
        })?;
        call_prepared_query(self.canister_id, name, params).await
    }

    /// Execute dynamic GQL through the configured Router canister.
    pub async fn gql_query<R>(
        &self,
        query: impl Into<String>,
        params: GqlParams,
    ) -> Result<R, PreparedCallError>
    where
        R: CandidType + for<'de> Deserialize<'de>,
    {
        call_gql_query(self.canister_id, query, params).await
    }

    /// Execute a named prepared update through the configured Router canister.
    pub async fn execute_update<R>(
        &self,
        name: impl Into<String>,
        params: Vec<u8>,
    ) -> Result<R, PreparedCallError>
    where
        R: CandidType + for<'de> Deserialize<'de>,
    {
        call_prepared_update(self.canister_id, name, params).await
    }

    /// Execute an idempotent named prepared update through the configured Router canister.
    pub async fn execute_update_idempotent<R>(
        &self,
        name: impl Into<String>,
        params: Vec<u8>,
        client_mutation_key: impl Into<String>,
    ) -> Result<R, PreparedCallError>
    where
        R: CandidType + for<'de> Deserialize<'de>,
    {
        call_prepared_update_idempotent(self.canister_id, name, params, client_mutation_key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_prepared_query_args_round_trips() {
        let name = "alice_home_feed";
        let params: Vec<u8> = vec![0, 1, 2, 255];
        let encoded = encode_prepared_query_args(name, params.clone());

        let (decoded_name, decoded_params): (String, Vec<u8>) =
            candid::utils::decode_args(&encoded).expect("decode args");

        assert_eq!(decoded_name, name);
        assert_eq!(decoded_params, params);
    }

    #[test]
    fn prepared_call_error_display_contains_context() {
        let err = PreparedCallError::Reject {
            code: "CanisterReject".to_string(),
            message: "no query".to_string(),
        };
        let text = format!("{err}");
        assert!(text.contains("no query"), "{text}");
        assert!(text.contains("CanisterReject"), "{text}");
    }

    #[test]
    fn encode_prepared_idempotent_update_args_round_trips() {
        let encoded = candid::utils::encode_args((
            "increment".to_string(),
            vec![1_u8, 2, 3],
            "request-1".to_string(),
        ))
        .expect("encode args");
        let decoded: (String, Vec<u8>, String) =
            candid::utils::decode_args(&encoded).expect("decode args");
        assert_eq!(
            decoded,
            ("increment".into(), vec![1, 2, 3], "request-1".into())
        );
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
}
