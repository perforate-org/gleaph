//! Shared IC-agent transport for Router subcommands.
//!
//! Owns network/identity setup and the candid `Result` decode only. Subcommand logic stays in
//! the pure modules (`migration`, `load`); this module is the single place that touches the
//! agent, so the same conventions apply to every Router-facing command.

use candid::{CandidType, Decode, Encode, IDLArgs, IDLValue};
use ic_agent::Agent;
use std::path::Path;

pub const DEFAULT_IC_URL: &str = "https://icp-api.io";
pub const DEFAULT_LOCAL_URL: &str = "http://localhost:8000";

/// One connected Router endpoint.
pub struct RemoteTransport {
    agent: Agent,
    canister: candid::Principal,
    runtime: tokio::runtime::Runtime,
}

impl RemoteTransport {
    /// Build a transport using the same network and identity conventions as `gleaph migration`.
    pub fn connect(
        canister: &str,
        network: &str,
        identity: Option<&Path>,
        fetch_root_key: bool,
    ) -> Result<Self, String> {
        let (url, should_fetch_root_key) = resolve_network(network, fetch_root_key)?;
        let canister = candid::Principal::from_text(canister)
            .map_err(|error| format!("invalid canister principal: {error}"))?;
        let agent = if let Some(identity) = identity {
            let identity = ic_agent::identity::Secp256k1Identity::from_pem_file(identity)
                .map_err(|error| format!("read identity {}: {error}", identity.display()))?;
            Agent::builder()
                .with_url(url)
                .with_identity(identity)
                .build()
                .map_err(|error| format!("create IC agent: {error}"))?
        } else {
            Agent::builder()
                .with_url(url)
                .build()
                .map_err(|error| format!("create IC agent: {error}"))?
        };
        let runtime = tokio::runtime::Runtime::new()
            .map_err(|error| format!("create async runtime: {error}"))?;
        if should_fetch_root_key {
            runtime
                .block_on(agent.fetch_root_key())
                .map_err(|error| format!("fetch IC root key: {error}"))?;
        }
        Ok(Self {
            agent,
            canister,
            runtime,
        })
    }

    fn block_on<T>(&self, future: impl std::future::Future<Output = T>) -> T {
        self.runtime.block_on(future)
    }

    /// Query one Router method and decode the candid `Result<T, E>` envelope.
    pub fn query<T, E>(&self, method: &str, args: &impl CandidType) -> Result<Result<T, E>, String>
    where
        T: CandidType + for<'de> serde::Deserialize<'de>,
        E: CandidType + for<'de> serde::Deserialize<'de>,
    {
        let encoded = Encode!(args).map_err(|error| format!("encode {method} args: {error}"))?;
        let response = self
            .block_on(
                self.agent
                    .query(&self.canister, method)
                    .with_arg(encoded)
                    .call(),
            )
            .map_err(|error| format!("query {method}: {error}"))?;
        decode_result(&response, method)
    }

    /// Update one Router method and decode the candid `Result<T, E>` envelope.
    pub fn update<T, E>(&self, method: &str, args: &impl CandidType) -> Result<Result<T, E>, String>
    where
        T: CandidType + for<'de> serde::Deserialize<'de>,
        E: CandidType + for<'de> serde::Deserialize<'de>,
    {
        let encoded = Encode!(args).map_err(|error| format!("encode {method} args: {error}"))?;
        let response = self
            .block_on(
                self.agent
                    .update(&self.canister, method)
                    .with_arg(encoded)
                    .call_and_wait(),
            )
            .map_err(|error| format!("update {method}: {error}"))?;
        decode_result(&response, method)
    }
}

fn resolve_network(network: &str, fetch_root_key: bool) -> Result<(&str, bool), String> {
    match network {
        "ic" => Ok((DEFAULT_IC_URL, false)),
        "local" => Ok((DEFAULT_LOCAL_URL, true)),
        url if url.starts_with("http://") || url.starts_with("https://") => {
            if !fetch_root_key {
                return Err("a custom network URL requires --fetch-root-key".to_string());
            }
            Ok((url, true))
        }
        other => Err(format!(
            "unknown network {other:?}; expected \"ic\", \"local\", or an http(s) URL"
        )),
    }
}

/// Decode one candid `Result<T, E>` payload, keeping the `Err` variant typed so callers can
/// distinguish domain errors (e.g. `NotFound`) from transport failures.
fn decode_result<T, E>(response: &[u8], method: &str) -> Result<Result<T, E>, String>
where
    T: CandidType + for<'de> serde::Deserialize<'de>,
    E: CandidType + for<'de> serde::Deserialize<'de>,
{
    let args = IDLArgs::from_bytes(response)
        .map_err(|error| format!("decode {method} response: {error}"))?;
    let Some(IDLValue::Variant(result)) = args.args.first() else {
        return Err(format!("decode {method} response: expected Result variant"));
    };
    let value = &result.0.val;
    let payload = IDLArgs::new(std::slice::from_ref(value))
        .to_bytes()
        .map_err(|error| format!("decode {method} payload: {error}"))?;
    if result.0.id.get_id() == candid::idl_hash("Ok") {
        Decode!(&payload, T)
            .map(Ok)
            .map_err(|error| format!("decode {method} Ok payload: {error}"))
    } else {
        Decode!(&payload, E)
            .map(Err)
            .map_err(|error| format!("decode {method} Err payload: {error}"))
    }
}
