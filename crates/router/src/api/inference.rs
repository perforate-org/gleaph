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
use crate::facade::stable::router_config;
use crate::state::RouterError;
use crate::types;

/// Deployment-level inference changes are governance/controller-only in this slice.
fn require_controller() -> Result<Principal, RouterError> {
    let caller = msg_caller();
    if !ic_cdk::api::is_controller(&caller) {
        return Err(RouterError::NotAuthorized);
    }
    Ok(caller)
}

/// Set the Router-funded inference quota (ADR 0112). Controller-only.
#[update]
fn admin_set_inference_quota(
    window_secs: u32,
    units_per_window: u32,
    enabled: bool,
) -> Result<(), RouterError> {
    require_controller()?;
    if !(1..=3600).contains(&window_secs) {
        return Err(RouterError::InvalidArgument(format!(
            "quota window must be 1..=3600 seconds, got {window_secs}"
        )));
    }
    if !(1..=100_000).contains(&units_per_window) {
        return Err(RouterError::InvalidArgument(format!(
            "quota units must be 1..=100000, got {units_per_window}"
        )));
    }
    router_config::update(|config| {
        config.inference_quota = router_config::InferenceQuotaConfig {
            enabled,
            window_secs,
            units_per_window,
        };
    });
    Ok(())
}

/// Aggregate quota counters (consumed units, denied requests) for diagnostics.
#[ic_cdk_macros::query]
fn inference_quota_stats() -> (u64, u64) {
    crate::inference_quota::stats()
}

/// Bind or clear the deployment inference canister. A new binding is verified against the wire
/// contract before it is persisted; model weights are verified lazily when the proxy is used.
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
    let graph_id = graph_catalog::lookup_graph_id(&graph_name)
        .ok_or_else(|| RouterError::NotFound(graph_name))?;
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

/// One model-pack chunk uploaded through the Router to the deployment inference canister.
#[derive(candid::CandidType, serde::Deserialize, serde::Serialize, Clone, Debug)]
struct InferenceInstallChunk {
    offset: u64,
    bytes: Vec<u8>,
}

/// Ingress cap: the IC message limit is 2 MiB; the canister's install chunks are 1.9 MB.
const MAX_INFERENCE_PACK_CHUNK: usize = 1_900_000;

/// Forward one pack chunk to the inference canister. The Router is its governance controller.
#[update]
async fn admin_install_inference_pack_chunk(
    offset: u64,
    bytes: Vec<u8>,
) -> Result<(), RouterError> {
    require_controller()?;
    if bytes.len() > MAX_INFERENCE_PACK_CHUNK {
        return Err(RouterError::InvalidArgument(format!(
            "inference pack chunk is {} bytes; max {}",
            bytes.len(),
            MAX_INFERENCE_PACK_CHUNK
        )));
    }
    let canister = router_config::inference_canister().ok_or_else(|| {
        RouterError::NotImplemented("inference canister is not provisioned".to_owned())
    })?;
    forward_pack_chunk(canister, offset, bytes).await
}

/// Finish the pack upload and confirm the model is installed before the graph can route.
#[update]
async fn admin_install_inference_pack_finish(digest: String) -> Result<(), RouterError> {
    require_controller()?;
    let canister = router_config::inference_canister().ok_or_else(|| {
        RouterError::NotImplemented("inference canister is not provisioned".to_owned())
    })?;
    forward_pack_finish(canister, digest).await?;
    if !inference_weights_installed(canister).await? {
        return Err(RouterError::Internal(
            "inference pack finish succeeded but weights are not installed".to_owned(),
        ));
    }
    Ok(())
}

/// Proxy one routing request for an inference-enabled graph.
#[update]
async fn route_and_extract(
    graph_name: String,
    token_ids: Vec<u32>,
) -> Result<gleaph_routing_wire::RouteAndExtract, RouterError> {
    let graph_id = require_routing_graph(&graph_name)?;
    let caller = msg_caller();
    let quota = router_config::get()
        .map(|config| config.inference_quota)
        .unwrap_or_default();
    #[cfg(target_family = "wasm")]
    let exempt = ic_cdk::api::is_controller(&caller);
    #[cfg(not(target_family = "wasm"))]
    let exempt = false;
    crate::inference_quota::consume(
        caller,
        graph_id,
        token_ids.len(),
        quota,
        ic_cdk::api::time(),
        exempt,
    )?;
    let canister = router_config::inference_canister().ok_or_else(|| {
        RouterError::NotImplemented("inference canister is not provisioned".to_owned())
    })?;
    forward_route_and_extract(canister, token_ids).await
}

/// Proxy the pack's routing option names for an inference-enabled graph.
#[update]
async fn route_options(graph_name: String) -> Result<Vec<String>, RouterError> {
    require_routing_graph(&graph_name)?;
    let canister = router_config::inference_canister().ok_or_else(|| {
        RouterError::NotImplemented("inference canister is not provisioned".to_owned())
    })?;
    forward_route_options(canister).await
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
async fn forward_pack_chunk(
    canister: Principal,
    offset: u64,
    bytes: Vec<u8>,
) -> Result<(), RouterError> {
    use candid::Encode;
    use ic_cdk::call::Call;
    let chunk = InferenceInstallChunk { offset, bytes };
    let args = Encode!(&chunk).map_err(|error| {
        RouterError::Internal(format!("inference chunk encode failed: {error}"))
    })?;
    Call::unbounded_wait(canister, "admin_install_chunk")
        .with_raw_args(&args)
        .await
        .map_err(|error| {
            RouterError::Internal(format!("inference pack chunk call failed: {error}"))
        })?
        .candid::<Result<u64, String>>()
        .map_err(|error| {
            RouterError::Internal(format!("inference pack chunk decode failed: {error}"))
        })?
        .map(|_installed_bytes| ())
        .map_err(RouterError::Internal)
}

#[cfg(not(target_family = "wasm"))]
async fn forward_pack_chunk(_: Principal, _: u64, _: Vec<u8>) -> Result<(), RouterError> {
    Ok(())
}

#[cfg(target_family = "wasm")]
async fn forward_pack_finish(canister: Principal, digest: String) -> Result<(), RouterError> {
    use candid::Encode;
    use ic_cdk::call::Call;
    let args = Encode!(&digest).map_err(|error| {
        RouterError::Internal(format!("inference finish encode failed: {error}"))
    })?;
    Call::unbounded_wait(canister, "admin_install_finish")
        .with_raw_args(&args)
        .await
        .map_err(|error| {
            RouterError::Internal(format!("inference pack finish call failed: {error}"))
        })?
        .candid::<Result<u64, String>>()
        .map_err(|error| {
            RouterError::Internal(format!("inference pack finish decode failed: {error}"))
        })?
        .map(|_installed_bytes| ())
        .map_err(RouterError::Internal)
}

#[cfg(not(target_family = "wasm"))]
async fn forward_pack_finish(_: Principal, _: String) -> Result<(), RouterError> {
    Ok(())
}

#[cfg(target_family = "wasm")]
async fn inference_weights_installed(canister: Principal) -> Result<bool, RouterError> {
    use ic_cdk::call::Call;
    Call::unbounded_wait(canister, "weights_installed")
        .await
        .map_err(|error| RouterError::Internal(format!("weights_installed call failed: {error}")))?
        .candid::<bool>()
        .map_err(|error| RouterError::Internal(format!("weights_installed decode failed: {error}")))
}

#[cfg(not(target_family = "wasm"))]
async fn inference_weights_installed(_: Principal) -> Result<bool, RouterError> {
    Ok(true)
}

#[cfg(target_family = "wasm")]
async fn forward_route_and_extract(
    canister: Principal,
    token_ids: Vec<u32>,
) -> Result<gleaph_routing_wire::RouteAndExtract, RouterError> {
    use candid::Encode;
    use ic_cdk::call::Call;
    let args = Encode!(&token_ids).map_err(|error| {
        RouterError::Internal(format!("inference route encode failed: {error}"))
    })?;
    Call::unbounded_wait(canister, "route_and_extract")
        .with_raw_args(&args)
        .await
        .map_err(|error| RouterError::Internal(format!("inference call failed: {error}")))?
        .candid::<Result<gleaph_routing_wire::RouteAndExtract, String>>()
        .map_err(|error| RouterError::Internal(format!("inference decode failed: {error}")))?
        .map_err(RouterError::Internal)
}

#[cfg(not(target_family = "wasm"))]
async fn forward_route_and_extract(
    _: Principal,
    _: Vec<u32>,
) -> Result<gleaph_routing_wire::RouteAndExtract, RouterError> {
    Err(RouterError::NotImplemented(
        "inference routing requires a wasm runtime".to_owned(),
    ))
}

#[cfg(target_family = "wasm")]
async fn forward_route_options(canister: Principal) -> Result<Vec<String>, RouterError> {
    use ic_cdk::call::Call;
    Call::unbounded_wait(canister, "route_options")
        .await
        .map_err(|error| RouterError::Internal(format!("route_options call failed: {error}")))?
        .candid::<Result<Vec<String>, String>>()
        .map_err(|error| RouterError::Internal(format!("route_options decode failed: {error}")))?
        .map_err(RouterError::Internal)
}

#[cfg(not(target_family = "wasm"))]
async fn forward_route_options(_: Principal) -> Result<Vec<String>, RouterError> {
    Err(RouterError::NotImplemented(
        "inference routing requires a wasm runtime".to_owned(),
    ))
}

#[cfg(test)]
mod tests {
    use candid::{Decode, Encode};

    use super::InferenceInstallChunk;

    #[derive(candid::CandidType, serde::Deserialize)]
    struct CanisterInstallChunk {
        offset: u64,
        bytes: Vec<u8>,
    }

    #[test]
    fn inference_chunk_encodes_as_the_canister_record() {
        let chunk = InferenceInstallChunk {
            offset: 7,
            bytes: vec![1, 2, 3],
        };
        let encoded = Encode!(&chunk).expect("encode router chunk");
        let one = candid::encode_one(&chunk).expect("encode_one router chunk");
        assert_eq!(encoded, one, "Encode! and encode_one must agree");
        let decoded: CanisterInstallChunk =
            Decode!(&encoded, CanisterInstallChunk).expect("decode canister chunk");
        assert_eq!(decoded.offset, 7);
        assert_eq!(decoded.bytes, vec![1, 2, 3]);
    }
}
