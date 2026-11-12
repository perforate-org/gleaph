//! Transports to the inference path: through the Router (preferred, ADR 0111) or directly to a
//! known inference canister (tests and operators).

use candid::{Decode, Encode, Principal};
use ic_agent::Agent;

use super::wire::RouteAndExtractWire;
use gleaph_routing_wire::{GeneralRouteResult, TextRouteAndExtract};

/// Block a synchronous transport call on an `ic-agent` future.
///
/// `ic-agent` futures drive tokio timers, so they need an active reactor even when the caller is a
/// plain synchronous function (the CLI's `ask` path). A bare `pollster::block_on` panics there with
/// "there is no reactor running"; when no runtime is active this helper runs the call on a private
/// current-thread runtime, and inside a multi-thread runtime it moves the wait off the worker.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    match tokio::runtime::Handle::try_current() {
        Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
            tokio::task::block_in_place(|| handle.block_on(future))
        }
        _ => tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("build blocking runtime for the routing transport")
            .block_on(future),
    }
}

/// How the routing client reaches the inference path.
///
/// Implementations are deliberately synchronous: the SDK blocks on the IC call, and a test or
/// in-process transport can answer without an agent.
pub trait RoutingTransport {
    /// Run one forward pass: routing probabilities plus BIO slot spans.
    fn route_and_extract(&self, token_ids: &[u32]) -> Result<RouteAndExtractWire, String>;

    /// Ordered routing options (index = probability index).
    fn route_options(&self) -> Result<Vec<String>, String>;

    /// Route question text when the transport can reach a Router-owned tokenizer (ADR 0113).
    /// The default fails closed so token-only transports keep working.
    fn route_and_extract_text(&self, _text: &str) -> Result<TextRouteAndExtract, String> {
        Err("text routing is not supported by this transport".to_owned())
    }

    /// Route question text over the graph's prepared catalog through the Router's additive
    /// general route (ADR 0114). The default fails closed for token-only transports.
    fn route_general(&self, _text: &str) -> Result<GeneralRouteResult, String> {
        Err("general routing is not supported by this transport".to_owned())
    }
}

/// Live transport that reaches the inference canister through the Router (ADR 0111).
///
/// The Router owns the deployment inference canister, checks that model routing is enabled for
/// `graph`, and proxies both calls. Applications no longer configure an inference principal.
pub struct RouterTransport {
    agent: Agent,
    router: Principal,
    graph: String,
}

impl RouterTransport {
    /// Wrap an already-configured agent and the inference-enabled graph.
    pub fn new(agent: Agent, router: Principal, graph: impl Into<String>) -> Self {
        Self {
            agent,
            router,
            graph: graph.into(),
        }
    }
}

impl RoutingTransport for RouterTransport {
    fn route_and_extract(&self, token_ids: &[u32]) -> Result<RouteAndExtractWire, String> {
        let argument = Encode!(&self.graph, &token_ids.to_vec())
            .map_err(|error| format!("encode Router route_and_extract: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, "route_and_extract")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("Router route_and_extract call: {error}"))?;
        let decoded: Result<RouteAndExtractWire, candid::types::value::IDLValue> = Decode!(
            &reply,
            Result<RouteAndExtractWire, candid::types::value::IDLValue>
        )
        .map_err(|error| format!("decode Router route_and_extract: {error}"))?;
        decoded.map_err(|error| format!("Router route_and_extract rejected: {error:?}"))
    }

    fn route_options(&self) -> Result<Vec<String>, String> {
        let argument = Encode!(&self.graph)
            .map_err(|error| format!("encode Router route_options: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, "route_options")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("Router route_options call: {error}"))?;
        let decoded: Result<Vec<String>, candid::types::value::IDLValue> =
            Decode!(&reply, Result<Vec<String>, candid::types::value::IDLValue>)
                .map_err(|error| format!("decode Router route_options: {error}"))?;
        decoded.map_err(|error| format!("Router route_options rejected: {error:?}"))
    }

    fn route_general(&self, text: &str) -> Result<GeneralRouteResult, String> {
        let argument = Encode!(&self.graph, &text.to_owned())
            .map_err(|error| format!("encode route_general: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, "route_general")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("route_general call: {error}"))?;
        let decoded: Result<GeneralRouteResult, candid::types::value::IDLValue> = Decode!(
            &reply,
            Result<GeneralRouteResult, candid::types::value::IDLValue>
        )
        .map_err(|error| format!("decode route_general: {error}"))?;
        decoded.map_err(|error| format!("Router route_general rejected: {error:?}"))
    }

    fn route_and_extract_text(&self, text: &str) -> Result<TextRouteAndExtract, String> {
        let argument = Encode!(&self.graph, &text.to_owned())
            .map_err(|error| format!("encode route_and_extract_text: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, "route_and_extract_text")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("route_and_extract_text call: {error}"))?;
        let decoded: Result<TextRouteAndExtract, candid::types::value::IDLValue> = Decode!(
            &reply,
            Result<TextRouteAndExtract, candid::types::value::IDLValue>
        )
        .map_err(|error| format!("decode route_and_extract_text: {error}"))?;
        decoded.map_err(|error| format!("Router route_and_extract_text rejected: {error:?}"))
    }
}

/// Live transport over `ic-agent` that calls a known inference canister directly.
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
        let reply = block_on(
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
        let reply = block_on(self.agent.query(&self.canister, "route_options").call())
            .map_err(|error| format!("route_options call: {error}"))?;
        let decoded: Result<Vec<String>, String> = Decode!(&reply, Result<Vec<String>, String>)
            .map_err(|error| format!("decode route_options: {error}"))?;
        decoded.map_err(|error| format!("inference canister: {error}"))
    }
}
