//! Transport to the Router `interpret`/`ask` ingress.

use candid::{Decode, Encode, Principal};
use ic_agent::Agent;

use gleaph_router_wire::{Answer, Interpretation, ReadMode};
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

/// How the ask client reaches the Router. Implementations are synchronous: the SDK blocks on the
/// IC call, and a test transport can answer without an agent.
pub trait AskTransport {
    /// Interpret question text over the graph's prepared catalog without executing anything.
    fn interpret(&self, text: &str) -> Result<Interpretation, String>;

    /// Interpret and execute the selected read-only prepared query.
    fn ask(&self, text: &str, read_mode: ReadMode) -> Result<Answer, String>;

    /// Diagnostic: raw interpretation with the abstain threshold forced to zero.
    fn debug_interpret(&self, _text: &str) -> Result<GeneralRouteResult, String> {
        Err("debug interpretation is not supported by this transport".to_owned())
    }

    /// Diagnostic batch over one threshold-zero context.
    fn debug_interpret_batch(&self, _texts: &[String]) -> Result<Vec<GeneralRouteResult>, String> {
        Err("debug interpretation batches are not supported by this transport".to_owned())
    }

    /// Read the graph's `(abstain_threshold_bps, policy_version)` profile.
    fn ask_config(&self, _graph: &str) -> Result<(u32, u32), String> {
        Err("routing profile reads are not supported by this transport".to_owned())
    }

    /// Set the graph's profile. Authorization is enforced by the Router.
    fn set_ask_config(&self, _graph: &str, _bps: u32, _policy: u32) -> Result<(), String> {
        Err("routing profile writes are not supported by this transport".to_owned())
    }

    /// Read the graph's declared parameter domains (ADR 0117).
    fn parameter_values(
        &self,
        _graph: &str,
    ) -> Result<Vec<(String, Vec<gleaph_routing_wire::GeneralDomainValue>)>, String> {
        Err("routing domain reads are not supported by this transport".to_owned())
    }

    /// Enable routing for a graph and return the deployment inference canister (ADR 0111).
    fn enable_ask(&self, _graph: &str) -> Result<candid::Principal, String> {
        Err("graph inference enablement is not supported by this transport".to_owned())
    }

    /// Disable routing for a graph.
    fn disable_ask(&self, _graph: &str) -> Result<(), String> {
        Err("graph inference disablement is not supported by this transport".to_owned())
    }

    /// Set or clear one parameter's declared domain (ADR 0117). Empty `values` clears it.
    fn set_parameter_values(
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

    fn query_interpret(&self, text: &str) -> Result<Interpretation, String> {
        let argument = Encode!(&self.graph, &text.to_owned())
            .map_err(|error| format!("encode interpret: {error}"))?;
        let reply = block_on(
            self.agent
                .query(&self.router, "interpret")
                .with_arg(argument)
                .call(),
        )
        .map_err(|error| format!("interpret query: {error}"))?;
        let decoded: Result<Interpretation, gleaph_router_wire::RouterError> =
            Decode!(&reply, Result<Interpretation, gleaph_router_wire::RouterError>)
                .map_err(|error| format!("decode interpret: {error}"))?;
        decoded.map_err(|error| format!("Router interpret rejected: {error:?}"))
    }

    fn query_ask(&self, text: &str, read_mode: ReadMode) -> Result<Answer, String> {
        let argument = Encode!(&self.graph, &text.to_owned(), &read_mode)
            .map_err(|error| format!("encode ask: {error}"))?;
        let reply = block_on(
            self.agent
                .query(&self.router, "ask")
                .with_arg(argument)
                .call(),
        )
        .map_err(|error| format!("ask query: {error}"))?;
        let decoded: Result<Answer, gleaph_router_wire::RouterError> =
            Decode!(&reply, Result<Answer, gleaph_router_wire::RouterError>)
                .map_err(|error| format!("decode ask: {error}"))?;
        decoded.map_err(|error| format!("Router ask rejected: {error:?}"))
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

impl AskTransport for RouterTransport {
    fn interpret(&self, text: &str) -> Result<Interpretation, String> {
        self.query_interpret(text)
    }

    fn ask(&self, text: &str, read_mode: ReadMode) -> Result<Answer, String> {
        self.query_ask(text, read_mode)
    }

    fn debug_interpret(&self, text: &str) -> Result<GeneralRouteResult, String> {
        self.query_general("debug_interpret", text)
    }

    fn debug_interpret_batch(&self, texts: &[String]) -> Result<Vec<GeneralRouteResult>, String> {
        self.query_general_batch("debug_interpret_batch", texts)
    }

    fn ask_config(&self, _graph: &str) -> Result<(u32, u32), String> {
        self.query_pair("general_ask_config")
    }

    fn set_ask_config(&self, _graph: &str, bps: u32, policy: u32) -> Result<(), String> {
        self.update_pair("admin_set_general_ask_config", bps, policy)
    }

    fn parameter_values(
        &self,
        _graph: &str,
    ) -> Result<Vec<(String, Vec<gleaph_routing_wire::GeneralDomainValue>)>, String> {
        self.query_domains("parameter_values")
    }

    fn enable_ask(&self, _graph: &str) -> Result<Principal, String> {
        let argument =
            Encode!(&self.graph).map_err(|error| format!("encode enable_ask: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, "enable_ask")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("Router enable_ask call: {error}"))?;
        let result: Result<Principal, gleaph_router_wire::RouterError> = Decode!(
            &reply,
            Result<Principal, gleaph_router_wire::RouterError>
        )
        .map_err(|error| format!("decode enable_ask: {error}"))?;
        result.map_err(|error| format!("Router rejected enable_ask: {error:?}"))
    }

    fn disable_ask(&self, _graph: &str) -> Result<(), String> {
        let argument =
            Encode!(&self.graph).map_err(|error| format!("encode disable_ask: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, "disable_ask")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("Router disable_ask call: {error}"))?;
        let result: Result<(), gleaph_router_wire::RouterError> =
            Decode!(&reply, Result<(), gleaph_router_wire::RouterError>)
                .map_err(|error| format!("decode disable_ask: {error}"))?;
        result.map_err(|error| format!("Router rejected disable_ask: {error:?}"))
    }

    fn set_parameter_values(
        &self,
        _graph: &str,
        parameter: &str,
        values: &[gleaph_routing_wire::GeneralDomainValue],
    ) -> Result<(), String> {
        let argument = Encode!(&self.graph, &parameter.to_owned(), &values.to_vec())
            .map_err(|error| format!("encode set_parameter_values: {error}"))?;
        let reply = block_on(
            self.agent
                .update(&self.router, "set_parameter_values")
                .with_arg(argument)
                .call_and_wait(),
        )
        .map_err(|error| format!("Router set_parameter_values call: {error}"))?;
        let result: Result<(), gleaph_router_wire::RouterError> =
            Decode!(&reply, Result<(), gleaph_router_wire::RouterError>)
                .map_err(|error| format!("decode set_parameter_values: {error}"))?;
        result.map_err(|error| format!("Router rejected set_parameter_values: {error:?}"))
    }
}
