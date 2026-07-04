//! Gleaph canister SDK.
//!
//! Small, opinionated helpers for application canisters that delegate fixed read scenarios to the
//! Gleaph Router via its `prepared_execute_query` interface. The API is intentionally generic: it
//! knows the Candid shape of a prepared query call `(String name, Vec<u8> params)` and how to make
//! a bounded-wait inter-canister call, but it does not know application-specific scenario names or
//! semantics.

use candid::{CandidType, Deserialize, Principal};

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
    use ic_cdk::call::{CallFailed, Response};

    let args = encode_prepared_query_args(name, params);
    let call_result: Result<Response, CallFailed> =
        ic_cdk::call::Call::bounded_wait(canister_id, "prepared_execute_query")
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
}
