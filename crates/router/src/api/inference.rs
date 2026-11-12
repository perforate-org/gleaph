//! Router-owned inference control plane (ADR 0111).
//!
//! One inference canister per Router deployment. The deployment config keeps its principal
//! (MemoryId 48); each graph opts in through `GraphRuntimeConfig.inference_enabled` (MemoryId 5).
//! Provisioning goes through the existing Provision flow with the singleton
//! `LogicalResource::Inference`; the Router is the governance controller of the created canister.

use std::collections::BTreeSet;

use candid::Principal;
use ic_cdk::api::msg_caller;
use ic_cdk_macros::update;

use crate::facade::stable::graph_catalog;
use crate::facade::stable::memory::GeneralRoutingProfile;
use crate::facade::stable::router_config;
use crate::general_routing;
use crate::rbac::authorize_graph_routing_authority;
use crate::state::RouterError;
use crate::types;
#[cfg(target_family = "wasm")]
use gleaph_routing_wire::MAX_GENERAL_CONTEXT_BYTES;
use gleaph_routing_wire::{
    GeneralRouteBatchRequest, GeneralRouteRequest, GeneralRouteResult, MAX_GENERAL_BATCH_QUESTIONS,
    MAX_GENERAL_BATCH_TEXT_BYTES, MAX_GENERAL_OPERATIONS,
};

/// Upper bound on question text forwarded to the inference canister. The canister enforces the same
/// wire cap; this keeps oversized questions out of the composite query before the hop.
const MAX_QUESTION_TEXT_BYTES: usize = gleaph_routing_wire::MAX_GENERAL_TEXT_BYTES;

/// Deployment-level inference changes are governance/controller-only in this slice.
fn require_controller() -> Result<Principal, RouterError> {
    let caller = msg_caller();
    if !ic_cdk::api::is_controller(&caller) {
        return Err(RouterError::NotAuthorized);
    }
    Ok(caller)
}

/// Bind or clear the deployment inference canister. A new binding is verified against the wire
/// contract before it is persisted.
#[update]
async fn admin_set_inference_canister(canister: Option<Principal>) -> Result<(), RouterError> {
    require_controller()?;
    if let Some(canister) = canister {
        if canister == Principal::anonymous() {
            return Err(RouterError::InvalidArgument(
                "inference canister cannot be anonymous".to_owned(),
            ));
        }
        verify_inference_canister(canister).await?;
    }
    router_config::update(|config| config.inference_canister = canister);
    Ok(())
}

/// Enable model routing for one graph. The first enable lazily provisions the deployment's
/// inference canister through the Provision flow; later enables reuse the binding.
#[update]
async fn admin_enable_graph_inference(graph_name: String) -> Result<Principal, RouterError> {
    let caller = require_controller()?;
    let graph_id = graph_catalog::lookup_graph_id(&graph_name)
        .ok_or_else(|| RouterError::NotFound(graph_name.clone()))?;

    let canister = match router_config::inference_canister() {
        Some(canister) => canister,
        None => {
            let canister = provision_inference_canister(caller).await?;
            verify_inference_canister(canister).await?;
            router_config::update(|config| config.inference_canister = Some(canister));
            canister
        }
    };
    graph_catalog::set_graph_inference_enabled(graph_id, true)?;
    Ok(canister)
}

/// Disable model routing for one graph. The deployment binding is kept for other graphs.
#[update]
fn admin_disable_graph_inference(graph_name: String) -> Result<(), RouterError> {
    require_controller()?;
    let graph_id =
        graph_catalog::lookup_graph_id(&graph_name).ok_or(RouterError::NotFound(graph_name))?;
    graph_catalog::set_graph_inference_enabled(graph_id, false)
}

/// Provision the singleton inference canister. Replay is safe: the intent key is
/// `(deployment_id, LogicalResource::Inference)`.
async fn provision_inference_canister(caller: Principal) -> Result<Principal, RouterError> {
    use gleaph_graph_kernel::provisioning::LogicalResource;
    use gleaph_graph_kernel::provisioning::wire::ProvisionableResource;

    let args = types::ProvisionGraphArgs {
        deployment_id: caller.to_text(),
        // The inference resource is deployment-scoped; the envelope has no graph.
        graph_name: String::new(),
        requested_resources: vec![ProvisionableResource {
            logical_resource: LogicalResource::Inference,
        }],
        authorized_caller: caller,
        release_id: "default".to_owned(),
        owner: caller,
        admins: BTreeSet::new(),
        text_analyzer_id: 0,
        text_kinds: None,
    };
    match crate::provisioning::graph::provision_resource_flow(caller, args).await? {
        types::ProvisionGraphResponse::Accepted {
            created_resources, ..
        }
        | types::ProvisionGraphResponse::Replay {
            created_resources, ..
        } => created_resources
            .into_iter()
            .find(|resource| resource.logical_resource == LogicalResource::Inference)
            .map(|resource| resource.canister_id)
            .ok_or_else(|| {
                RouterError::Internal(
                    "provisioned inference canister missing from created_resources".to_owned(),
                )
            }),
        types::ProvisionGraphResponse::Completed => {
            router_config::inference_canister().ok_or_else(|| {
                RouterError::Internal(
                    "inference provisioning completed but no binding was recorded".to_owned(),
                )
            })
        }
    }
}

/// Verify the wire contract version before a binding is persisted.
#[cfg(target_family = "wasm")]
async fn verify_inference_canister(canister: Principal) -> Result<(), RouterError> {
    use ic_cdk::call::Call;

    let version: u32 = Call::unbounded_wait(canister, "wire_version")
        .await
        .map_err(|error| RouterError::Internal(format!("wire_version call failed: {error}")))?
        .candid()
        .map_err(|error| RouterError::Internal(format!("wire_version decode failed: {error}")))?;
    if version != gleaph_routing_wire::WIRE_VERSION {
        return Err(RouterError::InvalidArgument(format!(
            "inference canister wire version {version} does not match {}",
            gleaph_routing_wire::WIRE_VERSION
        )));
    }
    Ok(())
}

#[cfg(not(target_family = "wasm"))]
async fn verify_inference_canister(_canister: Principal) -> Result<(), RouterError> {
    // Native unit tests have no management/inter-canister runtime; the PocketIC gate covers this.
    Ok(())
}

/// Read the per-graph general routing profile as `(abstain_threshold_bps, policy_version)`.
#[ic_cdk_macros::query]
fn general_routing_profile(graph_name: String) -> Result<(u32, u32), RouterError> {
    let graph_id =
        graph_catalog::lookup_graph_id(&graph_name).ok_or(RouterError::NotFound(graph_name))?;
    authorize_graph_routing_authority(&msg_caller(), graph_id)?;
    let profile = graph_catalog::general_routing_profile(graph_id)?;
    Ok((profile.abstain_threshold_bps, profile.policy_version))
}

/// Set the per-graph general routing profile. Authorized for a graph tenant or Router controller
/// (ADR 0115); thresholds are basis points in `0..=10_000`.
#[update]
fn admin_set_general_routing_profile(
    graph_name: String,
    abstain_threshold_bps: u32,
    policy_version: u32,
) -> Result<(), RouterError> {
    general_routing::validate_profile(abstain_threshold_bps, policy_version)?;
    let graph_id =
        graph_catalog::lookup_graph_id(&graph_name).ok_or(RouterError::NotFound(graph_name))?;
    authorize_graph_routing_authority(&msg_caller(), graph_id)?;
    graph_catalog::set_general_routing_profile(
        graph_id,
        GeneralRoutingProfile {
            abstain_threshold_bps,
            policy_version,
        },
    )
}

/// Set or clear one parameter's declared value domain (ADR 0117).
///
/// Empty `values` clears the domain. Values and aliases are trimmed, deduplicated and sorted so the
/// stored domain (and therefore the routing-context fingerprint) is canonical. Authorized for a
/// graph tenant or Router controller, like the routing profile (ADR 0115).
#[update]
fn set_routing_domain(
    graph_name: String,
    parameter: String,
    values: Vec<gleaph_routing_wire::GeneralDomainValue>,
) -> Result<(), RouterError> {
    use gleaph_routing_wire::GeneralDomainValue;

    let graph_id = graph_catalog::lookup_graph_id(&graph_name)
        .ok_or_else(|| RouterError::NotFound(graph_name.clone()))?;
    authorize_graph_routing_authority(&msg_caller(), graph_id)?;
    let mut canonical: Vec<GeneralDomainValue> = values
        .into_iter()
        .map(|entry| {
            let mut aliases: Vec<String> = entry
                .aliases
                .iter()
                .map(|alias| alias.trim().to_owned())
                .filter(|alias| !alias.is_empty())
                .collect();
            aliases.sort();
            aliases.dedup();
            GeneralDomainValue {
                value: entry.value.trim().to_owned(),
                aliases,
            }
        })
        .filter(|entry| !entry.value.is_empty())
        .collect();
    canonical.sort_by(|left, right| left.value.cmp(&right.value));
    let mut domains = crate::facade::stable::graph_catalog::routing_domains(graph_id)?;
    if canonical.is_empty() {
        domains.remove(&parameter);
        general_routing::validate_domains(&domains)?;
        return crate::facade::stable::graph_catalog::clear_routing_domain(graph_id, &parameter);
    }
    domains.insert(parameter.clone(), canonical.clone());
    general_routing::validate_domains(&domains)?;
    crate::facade::stable::graph_catalog::set_routing_domain(graph_id, parameter, canonical)
}

/// Read the graph's declared parameter domains, sorted by parameter name.
#[ic_cdk_macros::query]
fn routing_domains(
    graph_name: String,
) -> Result<Vec<(String, Vec<gleaph_routing_wire::GeneralDomainValue>)>, RouterError> {
    let graph_id = graph_catalog::lookup_graph_id(&graph_name)
        .ok_or_else(|| RouterError::NotFound(graph_name.clone()))?;
    authorize_graph_routing_authority(&msg_caller(), graph_id)?;
    let domains = crate::facade::stable::graph_catalog::routing_domains(graph_id)?;
    Ok(domains.into_iter().collect())
}

/// Route one question over the graph's prepared-query catalog with the general route (ADR 0114).
/// This is a **composite query**: the Router builds the caller-visible context, forwards it to the
/// inference canister's query route, and validates the returned operation. It does not consume
/// quota, mutate state or execute a prepared query; execution remains a separate update.
#[ic_cdk_macros::query(composite = true)]
async fn route_general(
    graph_name: String,
    text: String,
) -> Result<GeneralRouteResult, RouterError> {
    let graph_id = require_routing_graph(&graph_name)?;
    if text.len() > MAX_QUESTION_TEXT_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "question text is {} bytes; max {}",
            text.len(),
            MAX_QUESTION_TEXT_BYTES
        )));
    }
    let caller = msg_caller();
    let context = general_routing::context_for_graph(graph_id, caller)?;
    if context.operations.len() > MAX_GENERAL_OPERATIONS {
        return Err(RouterError::InvalidArgument(format!(
            "routing context has {} operations; max {MAX_GENERAL_OPERATIONS}",
            context.operations.len()
        )));
    }
    let canister = router_config::inference_canister().ok_or_else(|| {
        RouterError::NotImplemented("inference canister is not provisioned".to_owned())
    })?;
    let expected_fingerprint = general_routing::context_fingerprint(&context);
    let context_operation_ids: BTreeSet<String> = context
        .operations
        .iter()
        .map(|operation| operation.id.clone())
        .collect();
    let request = GeneralRouteRequest { text, context };
    let result = forward_route_general(canister, request).await?;
    if result.context_fingerprint.is_empty() {
        return Err(RouterError::Internal(
            "general route returned an empty context fingerprint".to_owned(),
        ));
    }
    let Some(operation) = &result.operation else {
        if !result.abstain {
            return Err(RouterError::Internal(
                "general route returned no operation without abstaining".to_owned(),
            ));
        }
        return Ok(result);
    };
    if !result
        .context_fingerprint
        .iter()
        .eq(expected_fingerprint.iter())
    {
        return Err(RouterError::Internal(
            "general route context fingerprint mismatch".to_owned(),
        ));
    }
    if !context_operation_ids.contains(operation) {
        return Err(RouterError::Internal(
            "general route returned an operation outside the caller-visible context".to_owned(),
        ));
    }
    Ok(result)
}

/// Diagnostic: run the general route with the abstain threshold forced to zero, returning the raw
/// top operation and probabilities. Calibration uses this as a composite query to sweep thresholds
/// without changing the stored graph profile; it never executes a prepared query.
#[ic_cdk_macros::query(composite = true)]
async fn admin_general_route_raw(
    graph_name: String,
    text: String,
) -> Result<GeneralRouteResult, RouterError> {
    if text.len() > MAX_QUESTION_TEXT_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "question text is {} bytes; max {}",
            text.len(),
            MAX_QUESTION_TEXT_BYTES
        )));
    }
    let graph_id =
        graph_catalog::lookup_graph_id(&graph_name).ok_or(RouterError::NotFound(graph_name))?;
    let caller = msg_caller();
    authorize_graph_routing_authority(&caller, graph_id)?;
    if !graph_catalog::graph_inference_enabled(graph_id)? {
        return Err(RouterError::Forbidden);
    }
    let mut context = general_routing::context_for_graph(graph_id, caller)?;
    context.abstain_threshold = 0.0;
    context.context_fingerprint = general_routing::context_fingerprint(&context);
    let canister = router_config::inference_canister().ok_or_else(|| {
        RouterError::NotImplemented("inference canister is not provisioned".to_owned())
    })?;
    let request = GeneralRouteRequest { text, context };
    let result = forward_route_general(canister, request).await?;
    if result.context_fingerprint.len() != 32 {
        return Err(RouterError::Internal(
            "general raw route returned an invalid context fingerprint".to_owned(),
        ));
    }
    Ok(result)
}

/// Raw calibration batch: score up to `MAX_GENERAL_BATCH_QUESTIONS` questions against one shared
/// threshold-zero context in a single composite query. Used by `gleaph ask --calibrate`; it never
/// executes and has no production quota.
#[ic_cdk_macros::query(composite = true)]
async fn admin_general_route_raw_batch(
    graph_name: String,
    texts: Vec<String>,
) -> Result<Vec<GeneralRouteResult>, RouterError> {
    if texts.is_empty() || texts.len() > MAX_GENERAL_BATCH_QUESTIONS {
        return Err(RouterError::InvalidArgument(format!(
            "batch must contain 1..={MAX_GENERAL_BATCH_QUESTIONS} questions"
        )));
    }
    let total_bytes: usize = texts.iter().map(String::len).sum();
    if total_bytes > MAX_GENERAL_BATCH_TEXT_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "batch texts are {total_bytes} bytes; max {MAX_GENERAL_BATCH_TEXT_BYTES}"
        )));
    }
    if texts
        .iter()
        .any(|text| text.len() > MAX_QUESTION_TEXT_BYTES)
    {
        return Err(RouterError::InvalidArgument(format!(
            "a question exceeds {MAX_QUESTION_TEXT_BYTES} bytes"
        )));
    }
    let graph_id =
        graph_catalog::lookup_graph_id(&graph_name).ok_or(RouterError::NotFound(graph_name))?;
    let caller = msg_caller();
    authorize_graph_routing_authority(&caller, graph_id)?;
    if !graph_catalog::graph_inference_enabled(graph_id)? {
        return Err(RouterError::Forbidden);
    }
    let mut context = general_routing::context_for_graph(graph_id, caller)?;
    context.abstain_threshold = 0.0;
    context.context_fingerprint = general_routing::context_fingerprint(&context);
    let canister = router_config::inference_canister().ok_or_else(|| {
        RouterError::NotImplemented("inference canister is not provisioned".to_owned())
    })?;
    let expected_fingerprint = general_routing::context_fingerprint(&context);
    let context_operation_ids: BTreeSet<String> = context
        .operations
        .iter()
        .map(|operation| operation.id.clone())
        .collect();
    let question_count = texts.len();
    let request = GeneralRouteBatchRequest { texts, context };
    let results = forward_route_general_batch(canister, request).await?;
    if results.len() != question_count {
        return Err(RouterError::Internal(
            "general raw batch reply length does not match the request".to_owned(),
        ));
    }
    for result in &results {
        if result.context_fingerprint.len() != 32 {
            return Err(RouterError::Internal(
                "general raw batch returned an invalid context fingerprint".to_owned(),
            ));
        }
        if !result
            .context_fingerprint
            .iter()
            .eq(expected_fingerprint.iter())
        {
            return Err(RouterError::Internal(
                "general raw batch context fingerprint mismatch".to_owned(),
            ));
        }
        if let Some(operation) = &result.operation
            && !context_operation_ids.contains(operation)
        {
            return Err(RouterError::Internal(
                "general raw batch returned an operation outside the caller-visible context"
                    .to_owned(),
            ));
        }
    }
    Ok(results)
}

/// Resolve a graph and enforce both access and the per-graph inference opt-in.
fn require_routing_graph(
    graph_name: &str,
) -> Result<gleaph_graph_kernel::entry::GraphId, RouterError> {
    let graph_id = graph_catalog::lookup_graph_id(graph_name)
        .ok_or_else(|| RouterError::NotFound(graph_name.to_owned()))?;
    let caller = msg_caller();
    if !crate::facade::store::RouterStore::new().caller_may_access_graph(graph_id, caller) {
        return Err(RouterError::NotFound(graph_name.to_owned()));
    }
    if !graph_catalog::graph_inference_enabled(graph_id)? {
        return Err(RouterError::Forbidden);
    }
    Ok(graph_id)
}

#[cfg(target_family = "wasm")]
async fn forward_route_general_batch(
    canister: Principal,
    request: GeneralRouteBatchRequest,
) -> Result<Vec<GeneralRouteResult>, RouterError> {
    use candid::Encode;
    use ic_cdk::call::Call;
    let args = Encode!(&request)
        .map_err(|error| RouterError::Internal(format!("general batch encode: {error}")))?;
    if args.len() > MAX_GENERAL_CONTEXT_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "general batch request is {} bytes; max {}",
            args.len(),
            MAX_GENERAL_CONTEXT_BYTES
        )));
    }
    Call::bounded_wait(canister, "route_general_batch")
        .with_raw_args(&args)
        .await
        .map_err(|error| RouterError::Internal(format!("general batch call failed: {error}")))?
        .candid::<Result<Vec<GeneralRouteResult>, String>>()
        .map_err(|error| RouterError::Internal(format!("general batch decode: {error}")))?
        .map_err(RouterError::Internal)
}

#[cfg(not(target_family = "wasm"))]
async fn forward_route_general_batch(
    _: Principal,
    _: GeneralRouteBatchRequest,
) -> Result<Vec<GeneralRouteResult>, RouterError> {
    Err(RouterError::NotImplemented(
        "general batch routing requires a wasm runtime".to_owned(),
    ))
}

#[cfg(target_family = "wasm")]
async fn forward_route_general(
    canister: Principal,
    request: GeneralRouteRequest,
) -> Result<GeneralRouteResult, RouterError> {
    use candid::Encode;
    use ic_cdk::call::Call;
    let args = Encode!(&request)
        .map_err(|error| RouterError::Internal(format!("general route encode: {error}")))?;
    if args.len() > MAX_GENERAL_CONTEXT_BYTES {
        return Err(RouterError::InvalidArgument(format!(
            "general route request is {} bytes; max {}",
            args.len(),
            MAX_GENERAL_CONTEXT_BYTES
        )));
    }
    Call::bounded_wait(canister, "route_general")
        .with_raw_args(&args)
        .await
        .map_err(|error| RouterError::Internal(format!("general route call failed: {error}")))?
        .candid::<Result<GeneralRouteResult, String>>()
        .map_err(|error| RouterError::Internal(format!("general route decode: {error}")))?
        .map_err(RouterError::Internal)
}

#[cfg(not(target_family = "wasm"))]
async fn forward_route_general(
    _: Principal,
    _: GeneralRouteRequest,
) -> Result<GeneralRouteResult, RouterError> {
    Err(RouterError::NotImplemented(
        "general routing requires a wasm runtime".to_owned(),
    ))
}
