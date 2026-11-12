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

    /// Diagnostic: raw route with the abstain threshold forced to zero, served as a Router composite
    /// query. The default fails closed for transports that are not calibration-capable.
    fn route_general_raw(&self, _text: &str) -> Result<GeneralRouteResult, String> {
        Err("raw general routing is not supported by this transport".to_owned())
    }

    /// Raw calibration batch over one threshold-zero context.
    fn route_general_raw_batch(
        &self,
        _texts: &[String],
    ) -> Result<Vec<GeneralRouteResult>, String> {
        Err("raw general batch routing is not supported by this transport".to_owned())
    }

    /// Read the graph's `(abstain_threshold_bps, policy_version)` profile.
    fn routing_profile(&self, _graph: &str) -> Result<(u32, u32), String> {
        Err("routing profile reads are not supported by this transport".to_owned())
    }

    /// Set the graph's profile. Authorization is enforced by the Router.
    fn set_routing_profile(&self, _graph: &str, _bps: u32, _policy: u32) -> Result<(), String> {
        Err("routing profile writes are not supported by this transport".to_owned())
    }

    /// Read the graph's declared parameter domains (ADR 0117).
    fn routing_domains(
        &self,
        _graph: &str,
    ) -> Result<Vec<(String, Vec<gleaph_routing_wire::GeneralDomainValue>)>, String> {
        Err("routing domain reads are not supported by this transport".to_owned())
    }

    /// Enable routing for a graph and return the deployment inference canister (ADR 0111).
    fn enable_graph_inference(&self, _graph: &str) -> Result<candid::Principal, String> {
        Err("graph inference enablement is not supported by this transport".to_owned())
    }

    /// Disable routing for a graph.
    fn disable_graph_inference(&self, _graph: &str) -> Result<(), String> {
        Err("graph inference disablement is not supported by this transport".to_owned())
    }

    /// Set or clear one parameter's declared domain (ADR 0117). Empty `values` clears it.
    fn set_routing_domain(
        &self,
        _graph: &str,
        _parameter: &str,
        _values: &[gleaph_routing_wire::GeneralDomainValue],
    ) -> Result<(), String> {
        Err("routing domain writes are not supported by this transport".to_owned())
    }
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

impl RouterTransport {
    fn query_general_batch(
        &self,
        method: &str,
        texts: &[String],
    ) -> Result<Vec<GeneralRouteResult>, String> {
        let argument = Encode!(&self.graph, &texts.to_vec())
            .map_err(|error| format!("encode {method}: {error}"))?;
        let reply = block_on(
            self.agent
                .query(&self.router, method)
                .with_arg(argument)
                .call(),
        )
        .map_err(|error| format!("{method} query: {error}"))?;
        let decoded: Result<Vec<GeneralRouteResult>, gleaph_router_wire::RouterError> = Decode!(
            &reply,
            Result<Vec<GeneralRouteResult>, gleaph_router_wire::RouterError>
        )
        .map_err(|error| format!("decode {method}: {error}"))?;
        decoded.map_err(|error| format!("Router {method} rejected: {error:?}"))
    }

    fn query_general(&self, method: &str, text: &str) -> Result<GeneralRouteResult, String> {
        let argument = Encode!(&self.graph, &text.to_owned())
            .map_err(|error| format!("encode {method}: {error}"))?;
        let reply = block_on(
            self.agent
                .query(&self.router, method)
                .with_arg(argument)
                .call(),
        )
        .map_err(|error| format!("{method} query: {error}"))?;
        let decoded: Result<GeneralRouteResult, gleaph_router_wire::RouterError> =
            Decode!(&reply, Result<GeneralRouteResult, gleaph_router_wire::RouterError>)
                .map_err(|error| format!("decode {method}: {error}"))?;
        decoded.map_err(|error| format!("Router {method} rejected: {error:?}"))
    }

    fn query_pair(&self, method: &str) -> Result<(u32, u32), String> {
        let argument = Encode!(&self.graph).map_err(|error| format!("encode {method}: {error}"))?;
        let reply = block_on(
            self.agent
                .query(&self.router, method)
                .with_arg(argument)
                .call(),
        )
        .map_err(|error| format!("{method} query: {error}"))?;
        let decoded: Result<(u32, u32), gleaph_router_wire::RouterError> =
            Decode!(&reply, Result<(u32, u32), gleaph_router_wire::RouterError>)
                .map_err(|error| format!("decode {method}: {error}"))?;
        decoded.map_err(|error| format!("Router {method} rejected: {error:?}"))
    }

    fn query_domains(
        &self,
        method: &str,
    ) -> Result<Vec<(String, Vec<gleaph_routing_wire::GeneralDomainValue>)>, String> {
        let argument = Encode!(&self.graph).map_err(|error| format!("encode {method}: {error}"))?;
        let reply = block_on(
            self.agent
                .query(&self.router, method)
                .with_arg(argument)
                .call(),
        )
        .map_err(|error| format!("{method} query: {error}"))?;
        let decoded: Result<
            Vec<(String, Vec<gleaph_routing_wire::GeneralDomainValue>)>,
            gleaph_router_wire::RouterError,
        > = Decode!(
            &reply,
            Result<
                Vec<(String, Vec<gleaph_routing_wire::GeneralDomainValue>)>,
                gleaph_router_wire::RouterError,
            >
        )
        .map_err(|error| format!("decode {method}: {error}"))?;
        decoded.map_err(|error| format!("Router {method} rejected: {error:?}"))
    }

    fn update_pair(&self, method: &str, bps: u32, policy: u32) -> Result<(), String> {
        let argument = Encode!(&self.graph, &bps, &policy)
            .map_err(|error| format!("encode {method}: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, method)
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("{method} call: {error}"))?;
        let decoded: Result<(), gleaph_router_wire::RouterError> =
            Decode!(&reply, Result<(), gleaph_router_wire::RouterError>)
                .map_err(|error| format!("decode {method}: {error}"))?;
        decoded.map_err(|error| format!("Router {method} rejected: {error:?}"))
    }
}

impl RoutingTransport for RouterTransport {
    fn route_general(&self, text: &str) -> Result<GeneralRouteResult, String> {
        self.query_general("route_general", text)
    }

    fn route_general_raw(&self, text: &str) -> Result<GeneralRouteResult, String> {
        self.query_general("admin_general_route_raw", text)
    }

    fn route_general_raw_batch(&self, texts: &[String]) -> Result<Vec<GeneralRouteResult>, String> {
        self.query_general_batch("admin_general_route_raw_batch", texts)
    }

    fn routing_profile(&self, _graph: &str) -> Result<(u32, u32), String> {
        self.query_pair("general_routing_profile")
    }

    fn set_routing_profile(&self, _graph: &str, bps: u32, policy: u32) -> Result<(), String> {
        self.update_pair("admin_set_general_routing_profile", bps, policy)
    }

    fn routing_domains(
        &self,
        _graph: &str,
    ) -> Result<Vec<(String, Vec<gleaph_routing_wire::GeneralDomainValue>)>, String> {
        self.query_domains("routing_domains")
    }

    fn enable_graph_inference(&self, _graph: &str) -> Result<Principal, String> {
        let argument = Encode!(&self.graph)
            .map_err(|error| format!("encode admin_enable_graph_inference: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, "admin_enable_graph_inference")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("Router admin_enable_graph_inference call: {error}"))?;
        let result: Result<Principal, gleaph_router_wire::RouterError> = Decode!(
            &reply,
            Result<Principal, gleaph_router_wire::RouterError>
        )
        .map_err(|error| format!("decode admin_enable_graph_inference: {error}"))?;
        result.map_err(|error| format!("Router rejected admin_enable_graph_inference: {error:?}"))
    }

    fn disable_graph_inference(&self, _graph: &str) -> Result<(), String> {
        let argument = Encode!(&self.graph)
            .map_err(|error| format!("encode admin_disable_graph_inference: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, "admin_disable_graph_inference")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("Router admin_disable_graph_inference call: {error}"))?;
        let result: Result<(), gleaph_router_wire::RouterError> =
            Decode!(&reply, Result<(), gleaph_router_wire::RouterError>)
                .map_err(|error| format!("decode admin_disable_graph_inference: {error}"))?;
        result.map_err(|error| format!("Router rejected admin_disable_graph_inference: {error:?}"))
    }

    fn set_routing_domain(
        &self,
        _graph: &str,
        parameter: &str,
        values: &[gleaph_routing_wire::GeneralDomainValue],
    ) -> Result<(), String> {
        let argument = Encode!(&self.graph, &parameter.to_owned(), &values.to_vec())
            .map_err(|error| format!("encode set_routing_domain: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, "set_routing_domain")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("Router set_routing_domain call: {error}"))?;
        let result: Result<(), gleaph_router_wire::RouterError> =
            Decode!(&reply, Result<(), gleaph_router_wire::RouterError>)
                .map_err(|error| format!("decode set_routing_domain: {error}"))?;
        result.map_err(|error| format!("Router rejected set_routing_domain: {error:?}"))
    }
}
