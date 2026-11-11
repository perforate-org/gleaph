//! Transport to the inference canister's `route_and_extract` / `route_options` entry points.

use candid::{Decode, Encode, Principal};
use ic_agent::Agent;

use super::wire::RouteAndExtractWire;

/// How the routing client reaches the inference canister.
///
/// Implementations are deliberately synchronous: the SDK blocks on the IC call, and a test or
/// in-process transport can answer without an agent.
pub trait RoutingTransport {
    /// Run one forward pass: routing probabilities plus BIO slot spans.
    fn route_and_extract(&self, token_ids: &[u32]) -> Result<RouteAndExtractWire, String>;

    /// Ordered routing options (index = probability index).
    fn route_options(&self) -> Result<Vec<String>, String>;
}

/// Live transport over `ic-agent`.
pub struct CanisterTransport {
    agent: Agent,
    canister: Principal,
}

impl CanisterTransport {
    /// Wrap an already-configured agent (identity, network and root key are the caller's policy).
    pub fn new(agent: Agent, canister: Principal) -> Self {
        Self { agent, canister }
    }
}

impl RoutingTransport for CanisterTransport {
    fn route_and_extract(&self, token_ids: &[u32]) -> Result<RouteAndExtractWire, String> {
        let argument = Encode!(&token_ids.to_vec())
            .map_err(|error| format!("encode route_and_extract: {error}"))?;
        let reply = pollster::block_on(
            self.agent
                .update(&self.canister, "route_and_extract")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("route_and_extract call: {error}"))?;
        let decoded: Result<RouteAndExtractWire, String> =
            Decode!(&reply, Result<RouteAndExtractWire, String>)
                .map_err(|error| format!("decode route_and_extract: {error}"))?;
        decoded.map_err(|error| format!("inference canister: {error}"))
    }

    fn route_options(&self) -> Result<Vec<String>, String> {
        let reply = pollster::block_on(self.agent.query(&self.canister, "route_options").call())
            .map_err(|error| format!("route_options call: {error}"))?;
        let decoded: Result<Vec<String>, String> = Decode!(&reply, Result<Vec<String>, String>)
            .map_err(|error| format!("decode route_options: {error}"))?;
        decoded.map_err(|error| format!("inference canister: {error}"))
    }
}
