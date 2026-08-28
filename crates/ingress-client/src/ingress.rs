//! The generic any-canister/any-method IC ingress layer.
//!
//! This is the reuse seam for the operator's bootstrap-tier management-canister commands
//! and for the dev CLI's platform bring-up seeding.

use std::path::Path;

use candid::{CandidType, Decode, Encode};
use ic_agent::Agent;
use serde::de::DeserializeOwned;
use thiserror::Error;

use crate::net::resolve_network;

/// Failures of the IC ingress layer itself (never the canister's typed rejections).
#[derive(Debug, Error)]
pub enum IngressError {
    /// The agent rejected or failed the call (connectivity, replica reject, timeout).
    #[error("IC agent call failed: {0}")]
    Agent(String),
    /// Candid encoding of the request arguments failed.
    #[error("encode {method} arguments: {detail}")]
    Encode {
        /// Method whose arguments could not be encoded.
        method: String,
        /// Underlying candid error text.
        detail: String,
    },
    /// Candid decoding of the response bytes failed.
    #[error("decode {method} response: {detail}")]
    Decode {
        /// Method whose response could not be decoded.
        method: String,
        /// Underlying candid error text.
        detail: String,
    },
}

/// One connected IC endpoint able to call any destination canister and method.
///
/// The connection conventions (endpoint selection, identity handling, root-key fetch) are
/// shared between consumers via [`crate::net`].
pub struct IcIngress {
    agent: Agent,
    /// The principal this connection signs requests as (anonymous without `--identity`).
    sender: candid::Principal,
    /// True when this connection targets the mainnet (`ic`) selector. The standard
    /// `create_canister` management method applies there; provisional methods are
    /// unavailable. Local/PocketIC endpoints (which cannot route ingress-level
    /// `create_canister`) use `provisional_create_canister_with_cycles` instead.
    mainnet: bool,
    /// The local network's default effective canister id (a principal within the
    /// application subnet's canister ranges), read from the `/_/topology` endpoint.
    /// Required as the effective canister id for `provisional_create_canister_with_cycles`
    /// so its response certification passes. `None` on mainnet or when the endpoint is
    /// unavailable.
    default_effective_canister_id: Option<candid::Principal>,
}

impl IcIngress {
    /// Connect to `network` ("ic", "local", or an http(s) URL) signing with the PEM at
    /// `identity` when given (anonymous otherwise). The root key is fetched exactly when the
    /// network convention requires it ([`crate::net::resolve_network`]).
    pub async fn connect(network: &str, identity: Option<&Path>) -> Result<Self, String> {
        let (url, fetch_root_key) = resolve_network(network)?;
        let builder = Agent::builder().with_url(&url);
        let agent = match identity {
            Some(path) => {
                let identity = ic_agent::identity::Secp256k1Identity::from_pem_file(path)
                    .map_err(|error| format!("read identity {}: {error}", path.display()))?;
                builder.with_identity(identity)
            }
            None => builder,
        }
        .build()
        .map_err(|error| format!("create IC agent: {error}"))?;
        if fetch_root_key {
            agent
                .fetch_root_key()
                .await
                .map_err(|error| format!("fetch IC root key: {error}"))?;
        }
        let sender = agent
            .get_principal()
            .map_err(|error| format!("resolve caller principal: {error}"))?;
        let mainnet = network == "ic";
        let default_effective_canister_id = if mainnet {
            None
        } else {
            fetch_default_effective_canister_id(&url).await
        };
        Ok(Self {
            agent,
            sender,
            mainnet,
            default_effective_canister_id,
        })
    }

    /// The principal this connection signs as (the governance/recovery principal when a PEM
    /// was given). Bootstrap-tier deploy uses it as the created canister's controller.
    pub fn principal(&self) -> candid::Principal {
        self.sender
    }

    /// The local network's default effective canister id, used as the effective canister id
    /// for `provisional_create_canister_with_cycles` (see GAP-2026-08-24-006(a)).
    pub fn default_effective_canister_id(&self) -> Option<candid::Principal> {
        self.default_effective_canister_id
    }

    /// Whether this connection targets the mainnet (`ic`) selector, where the standard
    /// `create_canister` management method applies and provisional methods are unavailable.
    pub fn is_mainnet(&self) -> bool {
        self.mainnet
    }

    /// Raw update call to any destination canister and method. Returns the raw reply bytes.
    ///
    /// This is the reuse seam for future bootstrap-tier management-canister commands.
    pub async fn update_raw(
        &self,
        target: candid::Principal,
        method: &str,
        encoded_args: Vec<u8>,
    ) -> Result<Vec<u8>, IngressError> {
        self.agent
            .update(&target, method)
            .with_arg(encoded_args)
            .call_and_wait()
            .await
            .map_err(|error| IngressError::Agent(error.to_string()))
    }

    /// Raw update call with an explicit effective canister ID. Needed for
    /// `provisional_create_canister_with_cycles`, whose response certification
    /// requires the effective canister ID to fall within the target subnet's
    /// canister ranges (the management canister `aaaaa-aa` does not).
    pub async fn update_raw_with_effective_canister_id(
        &self,
        target: candid::Principal,
        method: &str,
        encoded_args: Vec<u8>,
        effective_canister_id: candid::Principal,
    ) -> Result<Vec<u8>, IngressError> {
        self.agent
            .update(&target, method)
            .with_arg(encoded_args)
            .with_effective_canister_id(effective_canister_id)
            .call_and_wait()
            .await
            .map_err(|error| IngressError::Agent(error.to_string()))
    }

    /// Raw query call to any destination canister and method. Returns the raw reply bytes.
    pub async fn query_raw(
        &self,
        target: candid::Principal,
        method: &str,
        encoded_args: Vec<u8>,
    ) -> Result<Vec<u8>, IngressError> {
        self.agent
            .query(&target, method)
            .with_arg(encoded_args)
            .call()
            .await
            .map_err(|error| IngressError::Agent(error.to_string()))
    }

    /// Raw query call with an explicit effective canister ID. `canister_status` must route
    /// through the target canister's effective ID, not the management canister.
    pub async fn query_raw_with_effective_canister_id(
        &self,
        target: candid::Principal,
        method: &str,
        encoded_args: Vec<u8>,
        effective_canister_id: candid::Principal,
    ) -> Result<Vec<u8>, IngressError> {
        self.agent
            .query(&target, method)
            .with_arg(encoded_args)
            .with_effective_canister_id(effective_canister_id)
            .call()
            .await
            .map_err(|error| IngressError::Agent(error.to_string()))
    }

    /// Update any canister method taking one Candid argument and decode the candid
    /// `Result<T, E>` envelope.
    pub async fn update_result<A, T, E>(
        &self,
        target: candid::Principal,
        method: &str,
        args: &A,
    ) -> Result<Result<T, E>, IngressError>
    where
        A: CandidType,
        T: CandidType + DeserializeOwned,
        E: CandidType + DeserializeOwned,
    {
        let encoded = Encode!(args).map_err(|source| IngressError::Encode {
            method: method.to_owned(),
            detail: source.to_string(),
        })?;
        let response = self.update_raw(target, method, encoded).await?;
        decode_envelope(&response, method)
    }

    /// Query any canister method taking one Candid argument and decode the candid
    /// `Result<T, E>` envelope.
    pub async fn query_result<A, T, E>(
        &self,
        target: candid::Principal,
        method: &str,
        args: &A,
    ) -> Result<Result<T, E>, IngressError>
    where
        A: CandidType,
        T: CandidType + DeserializeOwned,
        E: CandidType + DeserializeOwned,
    {
        let encoded = Encode!(args).map_err(|source| IngressError::Encode {
            method: method.to_owned(),
            detail: source.to_string(),
        })?;
        let response = self.query_raw(target, method, encoded).await?;
        decode_envelope(&response, method)
    }

    /// Query any canister method returning a plain (non-`Result`) value.
    pub async fn query_value<A, T>(
        &self,
        target: candid::Principal,
        method: &str,
        args: &A,
    ) -> Result<T, IngressError>
    where
        A: CandidType,
        T: CandidType + DeserializeOwned,
    {
        let encoded = Encode!(args).map_err(|source| IngressError::Encode {
            method: method.to_owned(),
            detail: source.to_string(),
        })?;
        let response = self.query_raw(target, method, encoded).await?;
        Decode!(&response, T).map_err(|source| IngressError::Decode {
            method: method.to_owned(),
            detail: source.to_string(),
        })
    }

    /// Update any canister method returning a plain (non-`Result`) value.
    ///
    /// This is the reuse seam for management-canister calls whose replies are plain values
    /// (`create_canister` → `CanisterIdRecord`, `upload_chunk` → `chunk_hash`;
    /// bootstrap tier).
    pub async fn update_value<A, T>(
        &self,
        target: candid::Principal,
        method: &str,
        args: &A,
    ) -> Result<T, IngressError>
    where
        A: CandidType,
        T: CandidType + DeserializeOwned,
    {
        let encoded = Encode!(args).map_err(|source| IngressError::Encode {
            method: method.to_owned(),
            detail: source.to_string(),
        })?;
        let response = self.update_raw(target, method, encoded).await?;
        Decode!(&response, T).map_err(|source| IngressError::Decode {
            method: method.to_owned(),
            detail: source.to_string(),
        })
    }

    /// Update any canister method returning a plain value, with an explicit effective
    /// canister ID. Management-canister calls that target a specific canister
    /// (`upload_chunk`, `install_chunked_code`, `stop_canister`, `start_canister`) must
    /// route through that canister's effective ID, not the management canister.
    pub async fn update_value_with_effective_canister_id<A, T>(
        &self,
        target: candid::Principal,
        method: &str,
        args: &A,
        effective_canister_id: candid::Principal,
    ) -> Result<T, IngressError>
    where
        A: CandidType,
        T: CandidType + DeserializeOwned,
    {
        let encoded = Encode!(args).map_err(|source| IngressError::Encode {
            method: method.to_owned(),
            detail: source.to_string(),
        })?;
        let response = self
            .update_raw_with_effective_canister_id(target, method, encoded, effective_canister_id)
            .await?;
        Decode!(&response, T).map_err(|source| IngressError::Decode {
            method: method.to_owned(),
            detail: source.to_string(),
        })
    }

    /// Query any canister method returning a plain value, with an explicit effective
    /// canister ID. `canister_status` must route through the target canister's effective ID.
    pub async fn query_value_with_effective_canister_id<A, T>(
        &self,
        target: candid::Principal,
        method: &str,
        args: &A,
        effective_canister_id: candid::Principal,
    ) -> Result<T, IngressError>
    where
        A: CandidType,
        T: CandidType + DeserializeOwned,
    {
        let encoded = Encode!(args).map_err(|source| IngressError::Encode {
            method: method.to_owned(),
            detail: source.to_string(),
        })?;
        let response = self
            .query_raw_with_effective_canister_id(target, method, encoded, effective_canister_id)
            .await?;
        Decode!(&response, T).map_err(|source| IngressError::Decode {
            method: method.to_owned(),
            detail: source.to_string(),
        })
    }
}

/// Decode one candid `Result<T, E>` payload, keeping the `Err` variant typed so callers can
/// distinguish domain errors from transport failures. Same construction as the dev CLI's
/// `decode_result` (`crates/cli/src/remote.rs`).
pub(crate) fn decode_envelope<T, E>(
    response: &[u8],
    method: &str,
) -> Result<Result<T, E>, IngressError>
where
    T: CandidType + DeserializeOwned,
    E: CandidType + DeserializeOwned,
{
    Decode!(response, Result<T, E>).map_err(|source| IngressError::Decode {
        method: method.to_owned(),
        detail: source.to_string(),
    })
}

/// Fetch the local network's default effective canister id from the `/_/topology` endpoint
/// (the same source dfx's `dfx info default-effective-canister-id` uses). Returns `None` when
/// the endpoint is unavailable or the field is absent.
async fn fetch_default_effective_canister_id(url: &str) -> Option<candid::Principal> {
    #[derive(serde::Deserialize)]
    struct Topology {
        default_effective_canister_id: Option<DefaultEffectiveCanisterId>,
    }
    #[derive(serde::Deserialize)]
    struct DefaultEffectiveCanisterId {
        canister_id: String,
    }
    let body: Topology = reqwest::get(format!("{url}/_/topology"))
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let b64 = body.default_effective_canister_id?.canister_id;
    decode_default_effective_canister_id(&b64)
}

/// Decode the base64-encoded principal bytes from the `/_/topology`
/// `default_effective_canister_id.canister_id` field.
fn decode_default_effective_canister_id(b64: &str) -> Option<candid::Principal> {
    let bytes = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64).ok()?;
    Some(candid::Principal::from_slice(&bytes))
}

#[cfg(test)]
mod tests {
    use super::decode_default_effective_canister_id;

    #[test]
    fn decodes_launcher_default_effective_canister_id() {
        // The launcher's `/_/topology` default_effective_canister_id.canister_id for the
        // application subnet, base64 of the 10-byte principal.
        let p = decode_default_effective_canister_id("f/////+gAAABAQ==").unwrap();
        assert_eq!(p.to_text(), "4mc4g-43777-77775-aaaaa-cai");
    }

    #[test]
    fn rejects_malformed_base64() {
        assert!(decode_default_effective_canister_id("not-base64!!").is_none());
    }
}