//! Transport abstraction and the `ic-agent`-backed implementation.

use crate::types;
use candid::{CandidType, Decode, Principal};
use std::future::Future;
use std::pin::Pin;

/// A boxed future returned by a transport method.
///
/// On native targets the future must be `Send` so the client can be shared
/// across threads. On wasm ic-agent's wasm-bindgen backend is single-threaded
/// (`Rc`-based) and is neither `Send` nor `Sync`, so the `Send` bound is
/// dropped there.
#[cfg(not(target_family = "wasm"))]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
/// A boxed future returned by a transport method (wasm variant, without the
/// `Send` bound because ic-agent's wasm-bindgen backend is single-threaded).
#[cfg(target_family = "wasm")]
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;

/// A transport to the Gleaph Router.
///
/// A transport owns the call encoding/decoding and error policy, while `GleaphClient` owns the
/// typed, prepared-aware surface. Application code can provide a custom transport (e.g. for
/// testing or a different HTTP stack) and wrap it with `GleaphClient::new`.
#[cfg(not(target_family = "wasm"))]
pub trait GleaphTransport: Send + Sync {
    /// Execute a dynamic GQL query with explicit read consistency.
    fn gql_query<'a>(
        &'a self,
        query: String,
        params: Vec<u8>,
        read_mode: types::ReadMode,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>>;

    /// Execute an idempotent dynamic GQL mutation.
    fn gql_mutate<'a>(
        &'a self,
        query: String,
        params: Vec<u8>,
        client_mutation_key: String,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>>;

    /// Execute a named prepared query.
    fn prepared_query<'a>(
        &'a self,
        name: String,
        params: Vec<u8>,
        sort: Option<Vec<types::PreparedSortSpec>>,
        read_mode: types::ReadMode,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>>;

    /// Execute an idempotent named prepared mutation.
    fn prepared_mutate<'a>(
        &'a self,
        name: String,
        params: Vec<u8>,
        client_mutation_key: String,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>>;

    /// Execute one durable Router bulk-load command.
    fn bulk_load<'a>(
        &'a self,
        command: types::BulkLoadCommand,
    ) -> BoxFuture<'a, Result<types::BulkLoadResponse, crate::CallError>>;

    /// Read one bounded page of durable Router bulk-load status.
    fn bulk_load_status<'a>(
        &'a self,
        graph_name: Option<String>,
        client_bulk_key: String,
        receipt_cursor: Option<u32>,
        max_receipts: u32,
    ) -> BoxFuture<'a, Result<types::BulkLoadStatusPage, crate::CallError>>;

    /// Register or replace named prepared operations in one atomic batch.
    fn prepare<'a>(
        &'a self,
        operations: Vec<types::PreparedRegistration>,
    ) -> BoxFuture<'a, Result<(), crate::CallError>>;

    /// Remove one named prepared operation.
    fn drop_prepared<'a>(&'a self, name: String) -> BoxFuture<'a, Result<(), crate::CallError>>;

    /// The full prepared-operation manifest for one graph.
    fn list_prepared<'a>(
        &'a self,
        graph_name: Option<String>,
    ) -> BoxFuture<'a, Result<types::PreparedManifest, crate::CallError>>;

    /// The caller principal of the underlying identity, when known.
    fn caller(&self) -> Result<Principal, String> {
        Err("this transport does not expose a caller principal".to_string())
    }
}
/// A transport to the Gleaph Router (wasm variant).
///
/// Identical to the native [`GleaphTransport`] except the `Send + Sync`
/// supertrait and the `Send` future bound are dropped, because ic-agent's
/// wasm-bindgen backend is single-threaded.
#[cfg(target_family = "wasm")]
pub trait GleaphTransport {
    /// Execute a dynamic GQL query with explicit read consistency.
    fn gql_query<'a>(
        &'a self,
        query: String,
        params: Vec<u8>,
        read_mode: types::ReadMode,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>>;

    /// Execute an idempotent dynamic GQL mutation.
    fn gql_mutate<'a>(
        &'a self,
        query: String,
        params: Vec<u8>,
        client_mutation_key: String,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>>;

    /// Execute a named prepared query.
    fn prepared_query<'a>(
        &'a self,
        name: String,
        params: Vec<u8>,
        sort: Option<Vec<types::PreparedSortSpec>>,
        read_mode: types::ReadMode,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>>;

    /// Execute an idempotent named prepared mutation.
    fn prepared_mutate<'a>(
        &'a self,
        name: String,
        params: Vec<u8>,
        client_mutation_key: String,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>>;

    /// Execute one durable Router bulk-load command.
    fn bulk_load<'a>(
        &'a self,
        command: types::BulkLoadCommand,
    ) -> BoxFuture<'a, Result<types::BulkLoadResponse, crate::CallError>>;

    /// Read one bounded page of durable Router bulk-load status.
    fn bulk_load_status<'a>(
        &'a self,
        graph_name: Option<String>,
        client_bulk_key: String,
        receipt_cursor: Option<u32>,
        max_receipts: u32,
    ) -> BoxFuture<'a, Result<types::BulkLoadStatusPage, crate::CallError>>;

    /// Register or replace named prepared operations in one atomic batch.
    fn prepare<'a>(
        &'a self,
        operations: Vec<types::PreparedRegistration>,
    ) -> BoxFuture<'a, Result<(), crate::CallError>>;

    /// Remove one named prepared operation.
    fn drop_prepared<'a>(&'a self, name: String) -> BoxFuture<'a, Result<(), crate::CallError>>;

    /// The full prepared-operation manifest for one graph.
    fn list_prepared<'a>(
        &'a self,
        graph_name: Option<String>,
    ) -> BoxFuture<'a, Result<types::PreparedManifest, crate::CallError>>;

    /// The caller principal of the underlying identity, when known.
    fn caller(&self) -> Result<Principal, String> {
        Err("this transport does not expose a caller principal".to_string())
    }
}

/// Options used to connect an [`IcAgentTransport`] to a Router canister.
pub struct GleaphClientOptions {
    /// The Router canister id.
    pub canister_id: Principal,
    /// The IC endpoint URL. Defaults to `https://icp-api.io` when `None`.
    pub url: Option<String>,
    /// The caller identity used to sign calls. Defaults to the anonymous identity when `None`.
    pub identity: Option<Box<dyn ic_agent::Identity>>,
    /// Fetch the network root key before issuing calls (required for local/custom endpoints).
    pub fetch_root_key: bool,
}

impl GleaphClientOptions {
    /// Connect to a Router canister at the IC mainnet endpoint.
    pub fn new(canister_id: Principal) -> Self {
        Self {
            canister_id,
            url: None,
            identity: None,
            fetch_root_key: false,
        }
    }
}

/// An [`ic-agent`] backed transport to the Gleaph Router.
pub struct IcAgentTransport {
    agent: ic_agent::Agent,
    canister_id: Principal,
}

impl IcAgentTransport {
    /// Wrap an existing `ic-agent` agent and Router canister id, so one agent (with its identity,
    /// URL, and transport) is shared across Gleaph and other canister calls.
    ///
    /// The identity is owned by the `Agent`; the transport never sets it separately, so there is
    /// no way to supply a conflicting identity through this constructor.
    pub fn from_agent(agent: ic_agent::Agent, canister_id: Principal) -> Self {
        Self { agent, canister_id }
    }

    /// Build an `ic-agent` transport from [`GleaphClientOptions`].
    pub fn connect(options: GleaphClientOptions) -> Result<Self, crate::CallError> {
        let url = options
            .url
            .unwrap_or_else(|| "https://icp-api.io".to_string());
        let mut builder = ic_agent::Agent::builder().with_url(url);
        if let Some(identity) = options.identity {
            builder = builder.with_boxed_identity(identity);
        }
        let agent = builder.build().map_err(|error| crate::CallError::Reject {
            code: "BuildAgent".to_string(),
            message: error.to_string(),
        })?;
        if options.fetch_root_key {
            pollster::block_on(agent.fetch_root_key()).map_err(|error| {
                crate::CallError::Reject {
                    code: "FetchRootKey".to_string(),
                    message: error.to_string(),
                }
            })?;
        }
        Ok(Self {
            agent,
            canister_id: options.canister_id,
        })
    }

    fn query<'a>(
        &'a self,
        method: &'a str,
        args: Vec<u8>,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>> {
        Box::pin(async move {
            let response = self
                .agent
                .query(&self.canister_id, method)
                .with_arg(args)
                .call()
                .await
                .map_err(|error| crate::CallError::Reject {
                    code: "AgentQuery".to_string(),
                    message: error.to_string(),
                })?;
            decode_result(&response)
        })
    }

    fn update<'a>(
        &'a self,
        method: &'a str,
        args: Vec<u8>,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>> {
        Box::pin(async move {
            let response = self
                .agent
                .update(&self.canister_id, method)
                .with_arg(args)
                .call_and_wait()
                .await
                .map_err(|error| crate::CallError::Reject {
                    code: "AgentUpdate".to_string(),
                    message: error.to_string(),
                })?;
            decode_result(&response)
        })
    }
}

/// Decode a Router `Result<T, RouterError>` envelope from an `ic-agent` response.
pub fn decode_result<T>(response: &[u8]) -> Result<T, crate::CallError>
where
    T: CandidType + for<'de> serde::Deserialize<'de>,
{
    candid::Decode!(response, Result<T, types::RouterError>)
        .map_err(|error| crate::CallError::Decode {
            message: error.to_string(),
        })?
        .map_err(crate::CallError::Router)
}

impl GleaphTransport for IcAgentTransport {
    fn gql_query<'a>(
        &'a self,
        query: String,
        params: Vec<u8>,
        read_mode: types::ReadMode,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>> {
        let args = candid::utils::encode_args((query, params, read_mode))
            .expect("Candid encode gql_query arguments");
        self.query("gql_query", args)
    }

    fn gql_mutate<'a>(
        &'a self,
        query: String,
        params: Vec<u8>,
        client_mutation_key: String,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>> {
        let args = candid::utils::encode_args((query, params, client_mutation_key))
            .expect("Candid encode gql_mutate arguments");
        self.update("gql_mutate", args)
    }

    fn prepared_query<'a>(
        &'a self,
        name: String,
        params: Vec<u8>,
        sort: Option<Vec<types::PreparedSortSpec>>,
        read_mode: types::ReadMode,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>> {
        let args = candid::utils::encode_args((name, params, sort, read_mode))
            .expect("Candid encode prepared_query arguments");
        self.query("prepared_query", args)
    }

    fn prepared_mutate<'a>(
        &'a self,
        name: String,
        params: Vec<u8>,
        client_mutation_key: String,
    ) -> BoxFuture<'a, Result<types::GqlQueryResult, crate::CallError>> {
        let args = candid::utils::encode_args((name, params, client_mutation_key))
            .expect("Candid encode prepared_mutate arguments");
        self.update("prepared_mutate", args)
    }

    fn bulk_load<'a>(
        &'a self,
        command: types::BulkLoadCommand,
    ) -> BoxFuture<'a, Result<types::BulkLoadResponse, crate::CallError>> {
        Box::pin(async move {
            let args =
                candid::utils::encode_args((command,)).expect("Candid encode bulk_load arguments");
            let response = self
                .agent
                .update(&self.canister_id, "bulk_load")
                .with_arg(args)
                .call_and_wait()
                .await
                .map_err(|error| crate::CallError::Reject {
                    code: "AgentUpdate".to_string(),
                    message: error.to_string(),
                })?;
            decode_result(&response)
        })
    }

    fn bulk_load_status<'a>(
        &'a self,
        graph_name: Option<String>,
        client_bulk_key: String,
        receipt_cursor: Option<u32>,
        max_receipts: u32,
    ) -> BoxFuture<'a, Result<types::BulkLoadStatusPage, crate::CallError>> {
        Box::pin(async move {
            let args = candid::utils::encode_args((
                graph_name,
                client_bulk_key,
                receipt_cursor,
                max_receipts,
            ))
            .expect("Candid encode bulk_load_status arguments");
            let response = self
                .agent
                .query(&self.canister_id, "bulk_load_status")
                .with_arg(args)
                .call()
                .await
                .map_err(|error| crate::CallError::Reject {
                    code: "AgentQuery".to_string(),
                    message: error.to_string(),
                })?;
            decode_result(&response)
        })
    }

    fn prepare<'a>(
        &'a self,
        operations: Vec<types::PreparedRegistration>,
    ) -> BoxFuture<'a, Result<(), crate::CallError>> {
        Box::pin(async move {
            let args =
                candid::utils::encode_args((operations,)).expect("Candid encode prepare arguments");
            let response = self
                .agent
                .update(&self.canister_id, "prepare")
                .with_arg(args)
                .call_and_wait()
                .await
                .map_err(|error| crate::CallError::Reject {
                    code: "AgentUpdate".to_string(),
                    message: error.to_string(),
                })?;
            decode_result(&response)
        })
    }

    fn drop_prepared<'a>(&'a self, name: String) -> BoxFuture<'a, Result<(), crate::CallError>> {
        Box::pin(async move {
            let args =
                candid::utils::encode_args((name,)).expect("Candid encode drop_prepared arguments");
            let response = self
                .agent
                .update(&self.canister_id, "drop_prepared")
                .with_arg(args)
                .call_and_wait()
                .await
                .map_err(|error| crate::CallError::Reject {
                    code: "AgentUpdate".to_string(),
                    message: error.to_string(),
                })?;
            decode_result(&response)
        })
    }

    fn list_prepared<'a>(
        &'a self,
        graph_name: Option<String>,
    ) -> BoxFuture<'a, Result<types::PreparedManifest, crate::CallError>> {
        Box::pin(async move {
            let args = candid::utils::encode_args((graph_name,))
                .expect("Candid encode list_prepared arguments");
            let response = self
                .agent
                .query(&self.canister_id, "list_prepared")
                .with_arg(args)
                .call()
                .await
                .map_err(|error| crate::CallError::Reject {
                    code: "AgentQuery".to_string(),
                    message: error.to_string(),
                })?;
            decode_result(&response)
        })
    }

    fn caller(&self) -> Result<Principal, String> {
        self.agent.get_principal()
    }
}
