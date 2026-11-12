//! Transport to the Router general-route ingress.

use candid::{Decode, Encode, Principal};
use ic_agent::Agent;

use gleaph_routing_wire::GeneralRouteResult;

/// Block a synchronous transport call on an `ic-agent` future.
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

/// How the routing client reaches the Router general route. Implementations are synchronous: the
/// SDK blocks on the IC call, and a test transport can answer without an agent.
pub trait RoutingTransport {
    /// Route question text over the graph's prepared catalog.
    fn route_general(&self, text: &str) -> Result<GeneralRouteResult, String>;
}

/// Live transport that reaches the Router general route through `ic-agent`.
pub struct RouterTransport {
    agent: Agent,
    router: Principal,
    graph: String,
}

impl RouterTransport {
    /// Wrap an already-configured agent, the Router principal and the inference-enabled graph.
    pub fn new(agent: Agent, router: Principal, graph: impl Into<String>) -> Self {
        Self {
            agent,
            router,
            graph: graph.into(),
        }
    }
}

impl RoutingTransport for RouterTransport {
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
}
