//! Gleaph Rust client SDK for application clients that call the Gleaph Router from outside a
//! canister.
//!
//! This crate is the application-client counterpart to `gleaph-cdk` (the canister SDK). It uses
//! `ic-agent` for transport, mirrors the canister SDK's `GleaphClient<Prepared>` API surface, and
//! shares the Router data-plane wire contract via `gleaph-router-wire`.
//!
//! Caller identity is injected through the agent identity. Construct a client with
//! [`connect`] (accepts a [`GleaphClientOptions`] carrying an `ic_agent::Identity`) or with a
//! custom [`GleaphTransport`] via [`create_gleaph_client`].

#![warn(missing_docs)]

use candid::{CandidType, Deserialize, Principal};
use std::marker::PhantomData;

pub mod transport;

/// Router data-plane wire contract as a module, for path-qualified use in bindings.
pub mod types {
    pub use gleaph_prepared_api::{PreparedManifest, PreparedRegistration, PreparedSortSpec};
    pub use gleaph_router_wire::types::*;
pub use gleaph_router_wire::rows;

    /// GQL `Date` row binding used by generated client bindings.
    #[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
    pub use gleaph_gql_ic_wire::GqlDate;
    /// GQL `DateTime` row binding used by generated client bindings.
    #[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
    pub use gleaph_gql_ic_wire::GqlDateTime;
    /// GQL `Duration` row binding used by generated client bindings.
    #[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
    pub use gleaph_gql_ic_wire::GqlDuration;
    /// GQL `LocalDateTime` row binding used by generated client bindings.
    #[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
    pub use gleaph_gql_ic_wire::GqlLocalDateTime;
    /// GQL `LocalTime` row binding used by generated client bindings.
    #[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
    pub use gleaph_gql_ic_wire::GqlLocalTime;
    /// GQL `Time` row binding used by generated client bindings.
    #[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
    pub use gleaph_gql_ic_wire::GqlTime;
    /// GQL `ZonedDateTime` row binding used by generated client bindings.
    #[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
    pub use gleaph_gql_ic_wire::GqlZonedDateTime;
    /// GQL `ZonedTime` row binding used by generated client bindings.
    #[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
    pub use gleaph_gql_ic_wire::GqlZonedTime;
}

/// Query-building helpers re-exported from `gleaph-gql-params`.
pub use gleaph_gql_params::{
    GqlParams, GqlQuery, IntoGqlParam, encode_gql_params, gql, gql_param_value,
};

/// Serde facade used by generated client bindings.
pub use serde;

/// JSON facade used by generated client bindings for open record values.
pub use serde_json;

/// Candid facade used by generated client bindings for entrypoint arguments and results.
pub use candid;

/// Router data-plane wire contract (response envelope, read modes, mutation tokens,
/// `RouterError`, bulk-load family, row-decode helpers).
pub use gleaph_router_wire::types::*;

/// The Router's prepared-operation wire types.
pub use gleaph_prepared_api::{
    PreparedManifest, PreparedOperation, PreparedRegistration, PreparedSortSpec,
};

/// Error returned when a Router call fails before yielding a typed result.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum CallError {
    /// The IC rejected the call (timeout, canister error, destination invalid, etc.).
    Reject {
        /// Reject code as a human-readable string.
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

impl From<gleaph_router_wire::rows::RowSchemaError> for CallError {
    fn from(error: gleaph_router_wire::rows::RowSchemaError) -> Self {
        Self::Decode {
            message: error.to_string(),
        }
    }
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

/// Marker for a client without generated prepared operations.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct NoPrepared;

/// Canister-bound client for dynamic GQL and prepared operations.
///
/// The `Prepared` type parameter marks whether generated prepared operations are available:
/// [`GleaphClient::new`] yields [`GleaphClient<NoPrepared>`](Self), while
/// [`GleaphClient::with_prepared`] enables the operations generated for `Prepared` (see the
/// `PreparedExt` trait emitted by `gleaph-codegen`).
pub struct GleaphClient<Prepared = NoPrepared> {
    transport: std::sync::Arc<dyn transport::GleaphTransport>,
    _prepared: PhantomData<Prepared>,
}

impl<Prepared> Clone for GleaphClient<Prepared> {
    fn clone(&self) -> Self {
        Self {
            transport: self.transport.clone(),
            _prepared: PhantomData,
        }
    }
}

impl<Prepared> std::fmt::Debug for GleaphClient<Prepared> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("GleaphClient")
            .finish_non_exhaustive()
    }
}

impl GleaphClient<NoPrepared> {
    /// Bind the client to a transport without generated prepared operations.
    pub fn new(transport: std::sync::Arc<dyn transport::GleaphTransport>) -> Self {
        Self {
            transport,
            _prepared: PhantomData,
        }
    }

    /// Bind the client and enable the generated prepared operations for `Prepared`.
    pub fn with_prepared<Prepared>(
        transport: std::sync::Arc<dyn transport::GleaphTransport>,
    ) -> GleaphClient<Prepared> {
        GleaphClient {
            transport,
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
        self.transport
            .gql_query(query.query, params, read_mode)
            .await
    }

    /// Execute an idempotent dynamic GQL mutation.
    ///
    /// Reuse `client_mutation_key` only for retries of the same mutation. The returned
    /// [`GqlQueryResult`] carries the federated mutation lifecycle `phase` and the
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
        self.transport
            .gql_mutate(query.query, params, client_mutation_key.into())
            .await
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
        self.transport
            .prepared_query(name.into(), params, sort, read_mode)
            .await
    }

    /// Execute an idempotent named prepared mutation.
    pub async fn prepared_mutate(
        &self,
        name: impl Into<String>,
        params: Vec<u8>,
        client_mutation_key: impl Into<String>,
    ) -> Result<GqlQueryResult, CallError> {
        self.transport
            .prepared_mutate(name.into(), params, client_mutation_key.into())
            .await
    }

    /// Execute one durable Router bulk-load command.
    pub async fn bulk_load(&self, command: BulkLoadCommand) -> Result<BulkLoadResponse, CallError> {
        self.transport.bulk_load(command).await
    }

    /// Read one bounded page of durable Router bulk-load status.
    pub async fn bulk_load_status(
        &self,
        graph_name: Option<String>,
        client_bulk_key: impl Into<String>,
        receipt_cursor: Option<u32>,
        max_receipts: u32,
    ) -> Result<BulkLoadStatusPage, CallError> {
        self.transport
            .bulk_load_status(
                graph_name,
                client_bulk_key.into(),
                receipt_cursor,
                max_receipts,
            )
            .await
    }

    /// Register or replace named prepared operations in one atomic batch (idempotent upsert).
    /// Per-operation `metadata` is optional.
    pub async fn prepare(&self, operations: Vec<PreparedRegistration>) -> Result<(), CallError> {
        self.transport.prepare(operations).await
    }

    /// Remove one named prepared operation.
    pub async fn drop_prepared(&self, name: impl Into<String>) -> Result<(), CallError> {
        self.transport.drop_prepared(name.into()).await
    }

    /// The full prepared-operation manifest for one graph.
    pub async fn list_prepared(
        &self,
        graph_name: Option<String>,
    ) -> Result<PreparedManifest, CallError> {
        self.transport.list_prepared(graph_name).await
    }

    /// The caller principal of the underlying transport identity, when known.
    ///
    /// Equivalent to the canister-side `IC.MSG_CALLER()` context for debugging client calls that
    /// exercise caller-dependent prepared queries.
    pub fn caller(&self) -> Result<Principal, String> {
        self.transport.caller()
    }
}

/// Connect a [`GleaphClient<NoPrepared>`] to a Router canister over the IC.
pub fn connect(options: transport::GleaphClientOptions) -> Result<GleaphClient, CallError> {
    let transport = std::sync::Arc::new(transport::IcAgentTransport::connect(options)?);
    Ok(GleaphClient::new(transport))
}

/// Wrap an arbitrary transport as a [`GleaphClient<NoPrepared>`].
pub fn create_gleaph_client(
    transport: std::sync::Arc<dyn transport::GleaphTransport>,
) -> GleaphClient {
    GleaphClient::new(transport)
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
