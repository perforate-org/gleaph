//! Provisioned graph-creation flow shared by the public `register_graph` fold and the retained
//! L3 seam (ADR 0035 Slice 5/8, ADR 0056 §6).
//!
//! This module owns the Router-side orchestration of one provisioned graph admission:
//! build the resolved `ProvisionRequest` envelope from the intent, send it to the configured
//! Provision canister, and — on `Accepted` or `Replay` with created resources — commit the returned
//! graph shard canisters into the Router catalog. It is the single caller of the provisioning
//! request store and the outbound sender, keeping the `api` layer modules thin siblings that do
//! not call each other.

use candid::{Decode, Encode, Principal};

use crate::facade::store::RouterStore;
use crate::facade::store::provisioning::{InsertError, RouterProvisioningRequestStore};
use crate::provisioning::sender::{
    is_proven_pre_effect_rejection, send_accept_envelope, send_registration_ack,
};
use crate::state::RouterError;
use crate::types;
use crate::types::{RouterOutboundError, RouterProvisioningRequestState};
use gleaph_graph_kernel::provisioning::{LogicalResource, ProvisioningIntentKey};

use crate::types::{ProvisioningRequestKey, RouterProvisioningRequest};

/// Run the provisioned graph-admission flow for an intent. `caller` is the authorized admin.
///
/// Returns the ingress response mirror. On `Accepted` or `Replay` with non-empty
/// `created_resources`, it also registers the provisioned graph and its shards into the Router
/// catalog.
pub(crate) async fn provision_graph_flow(
    caller: Principal,
    args: types::ProvisionGraphArgs,
) -> Result<types::ProvisionGraphResponse, RouterError> {
    provision_graph_flow_with(
        caller,
        args,
        ic_cdk::api::canister_self(),
        send_accept_envelope,
        send_registration_ack,
    )
    .await
}

async fn provision_graph_flow_with<Accept, AcceptFuture, Ack, AckFuture>(
    caller: Principal,
    args: types::ProvisionGraphArgs,
    router_principal: Principal,
    send_accept: Accept,
    send_ack: Ack,
) -> Result<types::ProvisionGraphResponse, RouterError>
where
    Accept: FnOnce(Principal, Vec<u8>) -> AcceptFuture,
    AcceptFuture: std::future::Future<
            Output = Result<
                gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse,
                RouterOutboundError,
            >,
        >,
    Ack: FnOnce(
        Principal,
        gleaph_graph_kernel::provisioning::wire::RouterRegistrationAck,
    ) -> AckFuture,
    AckFuture: std::future::Future<
            Output = Result<
                gleaph_graph_kernel::provisioning::wire::RouterRegistrationAckResponse,
                RouterOutboundError,
            >,
        >,
{
    validate_graph_bootstrap_args(&args)?;
    let (response, request_key) =
        provision_resource_flow_inner_with(caller, &args, router_principal, send_accept).await?;
    if matches!(response, types::ProvisionGraphResponse::Completed) {
        return Ok(response);
    }

    let created_resources = match &response {
        types::ProvisionGraphResponse::Accepted {
            created_resources, ..
        }
        | types::ProvisionGraphResponse::Replay {
            created_resources, ..
        } => created_resources,
        types::ProvisionGraphResponse::Completed => unreachable!(),
    };
    let store = RouterProvisioningRequestStore::new();
    let record = store
        .get_by_request_id(&request_key)
        .ok_or_else(|| RouterError::InvalidState("pending Map 45 record disappeared".to_owned()))?;
    reconcile_provisioned_graph(&record, created_resources).await?;

    send_ack(
        record.provision_target,
        gleaph_graph_kernel::provisioning::wire::RouterRegistrationAck {
            deployment_id: request_key.deployment_id.clone(),
            request_id: request_key.request_id,
        },
    )
    .await
    .map_err(map_provision_outbound_error)?;
    store.complete_request(&request_key).map_err(|error| {
        RouterError::InvalidState(format!("registration completion: {error:?}"))
    })?;
    Ok(response)
}

/// Internal add-on admission retained for Property and Vector owners. It deliberately does not
/// reconcile Graph catalogs or send the Graph registration ACK.
pub(crate) async fn provision_resource_flow(
    caller: Principal,
    args: types::ProvisionGraphArgs,
) -> Result<types::ProvisionGraphResponse, RouterError> {
    provision_resource_flow_inner_with(
        caller,
        &args,
        ic_cdk::api::canister_self(),
        send_accept_envelope,
    )
    .await
    .map(|(response, _)| response)
}

async fn provision_resource_flow_inner_with<Send, SendFuture>(
    caller: Principal,
    args: &types::ProvisionGraphArgs,
    router_principal: Principal,
    send: Send,
) -> Result<(types::ProvisionGraphResponse, ProvisioningRequestKey), RouterError>
where
    Send: FnOnce(Principal, Vec<u8>) -> SendFuture,
    SendFuture: std::future::Future<
            Output = Result<
                gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse,
                RouterOutboundError,
            >,
        >,
{
    let provision_canister = crate::provisioning::config::get().ok_or_else(|| {
        RouterError::NotImplemented("provision_canister not configured".to_owned())
    })?;

    // Validate requested_resources non-empty and canonical intent present. The canonical intent is
    // the first resource; for a graph bootstrap that is a GraphShard, for an add-on provisioning it
    // is the requested index/vector resource.
    if args.requested_resources.is_empty() {
        return Err(RouterError::InvalidArgument(
            "requested_resources is empty".to_owned(),
        ));
    }
    let canonical = args
        .requested_resources
        .first()
        .ok_or_else(|| RouterError::InvalidArgument("requested_resources is empty".to_owned()))?;
    let intent_key = ProvisioningIntentKey::new(&args.deployment_id, canonical.logical_resource);

    // Seed the Router-side provisioning-request catalog before the outbound send so the
    // ack callback has a canonical record to advance. We need deployment_id for the key, so
    // clone it before moving fields into the ProvisionRequest wire struct.
    let deployment_id = args.deployment_id.clone();
    let request_id = gleaph_graph_kernel::provisioning::wire::provisioning_request_id(
        &args.graph_name,
        &args.requested_resources,
    );
    let request_key = ProvisioningRequestKey::new(&request_id, &deployment_id);
    let store = RouterProvisioningRequestStore::new();
    let install_args = build_install_args_with_router(args, router_principal);
    let graph_name_for_request = args.graph_name.clone();
    let requested_resources_for_request = args.requested_resources.clone();
    let request = gleaph_graph_kernel::provisioning::wire::ProvisionRequest {
        deployment_id: deployment_id.clone(),
        request_id,
        intent_key,
        reserved_graph_id: None,
        graph_name: graph_name_for_request,
        requested_resources: requested_resources_for_request,
        install_args,
        authorized_caller: args.authorized_caller,
        release_id: args.release_id.clone(),
    };
    let resolved_request_bytes = Encode!(&request)
        .map_err(|error| RouterError::ProvisionEncodingFailed(error.to_string()))?;
    let seed_record = RouterProvisioningRequest {
        request_id,
        caller,
        owner: args.owner,
        admins: args.admins.clone(),
        provision_target: provision_canister,
        resolved_request_bytes,
        state: RouterProvisioningRequestState::AwaitingAck,
        created_at_ns: crate::facade::store::ic_time_ns(),
    };
    let outcome = store
        .insert(&deployment_id, seed_record)
        .map_err(|err| match err {
            InsertError::IntentConflict => {
                RouterError::Conflict("provisioning intent already locked".to_owned())
            }
            InsertError::InvalidDuplicateIntent | InsertError::InvalidEnvelope => {
                RouterError::InvalidArgument("invalid provisioning envelope".to_owned())
            }
            InsertError::IdentityConflict => RouterError::Conflict(
                "same provisioning request key has different immutable identity".to_owned(),
            ),
        })?;

    let response = dispatch_provision_send(request_key.clone(), outcome, store, send).await?;
    Ok((response, request_key))
}

fn validate_graph_bootstrap_args(args: &types::ProvisionGraphArgs) -> Result<(), RouterError> {
    let exact = args.requested_resources.len() == 1
        && matches!(
            args.requested_resources[0].logical_resource,
            LogicalResource::GraphShard(shard)
                if shard == gleaph_graph_kernel::federation::ShardId::new(0)
        );
    if !exact {
        return Err(RouterError::InvalidArgument(
            "Graph bootstrap requires exactly [GraphShard(0)]".to_owned(),
        ));
    }
    Ok(())
}

/// ADR 0070: GQL `CREATE GRAPH` admission bridge.
///
/// A pre-registered graph name takes the binding-only catalog path (unchanged). An unregistered
/// name is provisioned through the shared admission flow — one indexless `GraphShard(0)` bootstrap
/// (ADR 0054), owned by the caller, whose registration marks it home when no home exists yet — so
/// the subsequent schema-binding write lands on a resolvable `GraphId`. Dev mode (no provisioner
/// configured) fails closed: shards must be registered explicitly via `register_graph`.
pub(crate) async fn create_graph_admission(
    caller: Principal,
    graph_name: &str,
) -> Result<(), RouterError> {
    let args = types::ProvisionGraphArgs {
        deployment_id: caller.to_text(),
        graph_name: graph_name.to_owned(),
        requested_resources: vec![
            gleaph_graph_kernel::provisioning::wire::ProvisionableResource {
                logical_resource: LogicalResource::GraphShard(
                    gleaph_graph_kernel::federation::ShardId::new(0),
                ),
            },
        ],
        authorized_caller: caller,
        release_id: "default".to_owned(),
        owner: caller,
        admins: std::collections::BTreeSet::new(),
    };
    create_graph_admission_with(
        caller,
        graph_name,
        args,
        ic_cdk::api::canister_self(),
        send_accept_envelope,
        send_registration_ack,
    )
    .await
}

/// The ordered CREATE GRAPH admission path. The runtime Router principal and two cross-canister
/// sends are injectable; pending lookup, catalog short-circuiting, shape validation,
/// reconciliation, and completion are the production owners used by both the public wrapper and
/// owner-layer tests.
async fn create_graph_admission_with<Accept, AcceptFuture, Ack, AckFuture>(
    caller: Principal,
    graph_name: &str,
    args: types::ProvisionGraphArgs,
    router_principal: Principal,
    send_accept: Accept,
    send_ack: Ack,
) -> Result<(), RouterError>
where
    Accept: FnOnce(Principal, Vec<u8>) -> AcceptFuture,
    AcceptFuture: std::future::Future<
            Output = Result<
                gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse,
                RouterOutboundError,
            >,
        >,
    Ack: FnOnce(
        Principal,
        gleaph_graph_kernel::provisioning::wire::RouterRegistrationAck,
    ) -> AckFuture,
    AckFuture: std::future::Future<
            Output = Result<
                gleaph_graph_kernel::provisioning::wire::RouterRegistrationAckResponse,
                RouterOutboundError,
            >,
        >,
{
    let pending = pending_create_graph_request(caller, graph_name)?;
    if pending.is_none()
        && crate::facade::stable::graph_catalog::lookup_graph_id(graph_name).is_some()
    {
        return Ok(());
    }
    if crate::provisioning::config::get().is_none() {
        return Err(RouterError::NotImplemented(
            "CREATE GRAPH for an unregistered graph requires a configured provision canister; register shards via register_graph in dev mode".to_owned(),
        ));
    }
    match provision_graph_flow_with(caller, args, router_principal, send_accept, send_ack).await? {
        types::ProvisionGraphResponse::Accepted { .. }
        | types::ProvisionGraphResponse::Replay { .. }
        | types::ProvisionGraphResponse::Completed => Ok(()),
    }
}

fn pending_create_graph_request(
    caller: Principal,
    graph_name: &str,
) -> Result<Option<RouterProvisioningRequest>, RouterError> {
    let pending = RouterProvisioningRequestStore::new()
        .pending_graph_bootstrap(&caller.to_text(), graph_name)
        .map_err(|error| {
            RouterError::InvalidState(format!("pending Graph bootstrap: {error:?}"))
        })?;
    if let Some(record) = pending.as_ref()
        && record.caller != caller
    {
        return Err(RouterError::Conflict(
            "pending Graph bootstrap belongs to a different caller".to_owned(),
        ));
    }
    Ok(pending)
}

/// Dispatches the outbound `accept_envelope` send according to the `InsertionOutcome`.
///
/// Four branches:
/// 1. `Inserted(AwaitingAck)` or `Existing(AwaitingAck)` → call `send`. On failure,
///    rollback ONLY if the current operation inserted the record.
/// 2. `Existing(Completed)` → do not resend; return the durable accepted version.
/// 3. `Existing(Pending | Submitted | Failed)` → reject as `InvalidState`.
async fn dispatch_provision_send<F, Fut>(
    request_key: types::ProvisioningRequestKey,
    outcome: crate::facade::store::provisioning::InsertionOutcome,
    store: crate::facade::store::provisioning::RouterProvisioningRequestStore,
    send: F,
) -> Result<types::ProvisionGraphResponse, RouterError>
where
    F: FnOnce(Principal, Vec<u8>) -> Fut,
    Fut: std::future::Future<
            Output = Result<
                gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse,
                RouterOutboundError,
            >,
        >,
{
    use crate::facade::store::provisioning::InsertionOutcome;

    let is_inserted = matches!(outcome, InsertionOutcome::Inserted(_));

    match &outcome {
        InsertionOutcome::Inserted(record) | InsertionOutcome::Existing(record)
            if matches!(record.state, RouterProvisioningRequestState::AwaitingAck) =>
        {
            let accept_response = match send(
                record.provision_target,
                record.resolved_request_bytes.clone(),
            )
            .await
            {
                Ok(response) => response,
                Err(e) => {
                    if is_inserted && is_proven_pre_effect_rejection(&e) {
                        // Invocation-owned rollback: only remove the record if the current
                        // operation inserted it AND it is still in AwaitingAck. Pre-existing
                        // records from any prior invocation must survive a transient send
                        // failure on a retry.
                        store.rollback_if_inserted_and_awaiting(&request_key, &outcome);
                    }
                    return Err(map_provision_outbound_error(e));
                }
            };
            Ok(build_provision_graph_response(accept_response))
        }
        InsertionOutcome::Existing(record)
            if matches!(record.state, RouterProvisioningRequestState::Completed) =>
        {
            Ok(types::ProvisionGraphResponse::Completed)
        }
        InsertionOutcome::Existing(record) => Err(RouterError::InvalidState(format!(
            "request in non-terminal state {:?}",
            record.state
        ))),
        // `Inserted` for a non-AwaitingAck state is impossible because `insert` always seeds
        // `AwaitingAck`; kept as a defensive match arm.
        InsertionOutcome::Inserted(record) => Err(RouterError::InvalidState(format!(
            "freshly inserted request in unexpected state {:?}",
            record.state
        ))),
    }
}

fn build_provision_graph_response(
    accept_response: gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse,
) -> types::ProvisionGraphResponse {
    match accept_response {
        gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse::Accepted {
            job_view,
            intent_lock_count,
            created_resources,
        } => types::ProvisionGraphResponse::Accepted {
            job_view,
            intent_lock_count,
            created_resources,
        },
        gleaph_graph_kernel::provisioning::wire::ProvisionAcceptResponse::Replay {
            job_view,
            intent_lock_count,
            created_resources,
        } => types::ProvisionGraphResponse::Replay {
            job_view,
            intent_lock_count,
            created_resources,
        },
    }
}

/// Maps a Provision outbound error to the Router ingress error.
fn map_provision_outbound_error(err: RouterOutboundError) -> RouterError {
    match err {
        RouterOutboundError::CallFailed(s) => RouterError::ProvisionCallFailed(s),
        RouterOutboundError::UnknownDeployment => {
            RouterError::UnknownDeployment("deployment not bound".to_owned())
        }
        RouterOutboundError::ProvenPreEffectRejection(message) => {
            RouterError::ProvisionRejected(message)
        }
        RouterOutboundError::Conflict => RouterError::ProvisionConflict("conflict".to_owned()),
        RouterOutboundError::IngressRejected(s) => RouterError::ProvisionRejected(s),
        RouterOutboundError::EncodingFailed(s) => RouterError::ProvisionEncodingFailed(s),
    }
}

/// Register a provisioned graph and its shards into the Router catalog from the canister ids the
/// Provision canister returned. The Router is the sole owner of logical topology; this commits
/// the graph registry entry and each graph shard (+ its index canister) so subsequent reads and
/// DML resolve correctly.
async fn reconcile_provisioned_graph(
    record: &RouterProvisioningRequest,
    created_resources: &[gleaph_graph_kernel::provisioning::wire::CreatedResource],
) -> Result<(), RouterError> {
    use gleaph_gql_ic::graph_registry::{GraphStatus, ProvisioningState};

    let request = Decode!(
        &record.resolved_request_bytes,
        gleaph_graph_kernel::provisioning::wire::ProvisionRequest
    )
    .map_err(|error| RouterError::InvalidState(format!("stored envelope decode: {error}")))?;
    if created_resources.len() != 1 {
        return Err(RouterError::InvalidState(
            "Graph bootstrap response must contain exactly one created resource".to_owned(),
        ));
    }
    let graph_shard = &created_resources[0];
    let LogicalResource::GraphShard(shard_id) = graph_shard.logical_resource else {
        return Err(RouterError::InvalidState(
            "Graph bootstrap response must contain GraphShard(0)".to_owned(),
        ));
    };
    if shard_id != gleaph_graph_kernel::federation::ShardId::new(0)
        || graph_shard.canister_id == Principal::anonymous()
    {
        return Err(RouterError::InvalidState(
            "Graph bootstrap response has invalid shard identity".to_owned(),
        ));
    }

    let store = RouterStore::new();
    let graph_canister = graph_shard.canister_id;
    let graph_name = &request.graph_name;

    if crate::facade::stable::graph_catalog::lookup_graph_id(graph_name).is_none() {
        let entry = gleaph_gql_ic::graph_registry::GraphRegistryEntry {
            graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(0),
            canister_id: graph_canister,
            owner: record.owner,
            admins: record.admins.clone(),
            status: GraphStatus::Active,
            version: 1,
            updated_at_ns: crate::facade::store::ic_time_ns(),
            provisioning_state: ProvisioningState::None,
            is_home: !crate::facade::store::any_home_graph_exists(),
        };
        store
            .admin_register_graph_with_random_key(record.caller, entry, graph_name)
            .await?;
    }

    let graph_id =
        crate::facade::stable::graph_catalog::lookup_graph_id(graph_name).ok_or_else(|| {
            RouterError::InvalidState("graph reconciliation did not persist name".into())
        })?;
    let graph_entry = crate::facade::stable::graph_catalog::graph_entry(graph_id)
        .ok_or_else(|| RouterError::Conflict("graph name exists without registry row".into()))?;
    let exact_graph = graph_entry.graph_id == graph_id
        && crate::facade::stable::graph_catalog::graph_name(graph_id).as_deref()
            == Some(graph_name.as_str())
        && graph_entry.canister_id == graph_canister
        && graph_entry.owner == record.owner
        && graph_entry.admins == record.admins
        && graph_entry.status == GraphStatus::Active
        && graph_entry.provisioning_state == ProvisioningState::None;
    if !exact_graph {
        return Err(RouterError::Conflict(
            "existing graph identity differs from provisioned Graph bootstrap".to_owned(),
        ));
    }

    if crate::facade::stable::graph_catalog::lookup_shard_entry(graph_id, shard_id).is_none() {
        store
            .admin_register_shard(
                record.caller,
                types::AdminRegisterShardArgs {
                    shard_id,
                    graph_canister,
                    index_canister: Principal::anonymous(),
                    logical_graph_name: graph_name.clone(),
                },
            )
            .await?;
    }
    let shard = crate::facade::stable::graph_catalog::lookup_shard_entry(graph_id, shard_id)
        .ok_or_else(|| {
            RouterError::InvalidState("shard reconciliation did not persist row".into())
        })?;
    if shard.graph_id != graph_id
        || shard.shard_id != shard_id
        || shard.graph_canister != graph_canister
        || shard.index_canister != Principal::anonymous()
        || !shard.index_attached
    {
        return Err(RouterError::Conflict(
            "existing shard identity differs from provisioned Graph bootstrap".to_owned(),
        ));
    }
    Ok(())
}

fn build_install_args_with_router(
    args: &types::ProvisionGraphArgs,
    router_principal: Principal,
) -> Vec<Vec<u8>> {
    use candid::Encode;
    use gleaph_graph_kernel::provisioning::init_args::{
        DEFAULT_DEFINITION_MAP_SEED, DEFAULT_SUBJECT_MAP_SEED, GraphInitArgs, IndexInitArgs,
        VectorCanisterInitArgs,
    };
    args.requested_resources
        .iter()
        .map(|resource| match resource.logical_resource {
            LogicalResource::GraphShard(shard_id) => {
                let init = GraphInitArgs {
                    logical_graph_name: Some(args.graph_name.clone()),
                    router_canister: Some(router_principal),
                    shard_id: Some(shard_id),
                    // ADR 0054: an indexless bootstrap shard carries the anonymous index target.
                    // The graph canister's wasm init requires router/shard/index to be set
                    // together, so absence of an index canister is expressed as `anonymous`,
                    // matching the shard registry's indexless representation.
                    index_canister: Some(Principal::anonymous()),
                };
                Encode!(&init).expect("encode GraphInitArgs")
            }
            LogicalResource::PropertyIndex(_) => {
                let init = IndexInitArgs {
                    router_canister: router_principal,
                };
                Encode!(&init).expect("encode IndexInitArgs")
            }
            LogicalResource::VectorIndex(_) => {
                let init = VectorCanisterInitArgs {
                    router_canister: router_principal,
                    definition_map_seed: DEFAULT_DEFINITION_MAP_SEED,
                    subject_map_seed: DEFAULT_SUBJECT_MAP_SEED,
                };
                Encode!(&init).expect("encode VectorCanisterInitArgs")
            }
            // A Router is a singleton issued once per deployment by the Account during the
            // bootstrap handover; an already-issued Router never requests another Router.
            LogicalResource::Router => {
                unreachable!("Router cannot issue a Router resource")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::provisioning::LogicalResource;
    use gleaph_graph_kernel::provisioning::wire::{
        ProvisionAcceptResponse, ProvisionJobSummary, RouterRegistrationAckResponse,
    };
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;

    use crate::facade::store::provisioning::{InsertionOutcome, RouterProvisioningRequestStore};
    use crate::state::RouterError;
    use crate::types::{
        ProvisionableResource, ProvisioningRequestKey, RouterOutboundError,
        RouterProvisioningRequest, RouterProvisioningRequestState,
    };

    fn sample_record(
        request_id: &str,
        deployment_id: &str,
        fingerprint: &str,
        state: RouterProvisioningRequestState,
    ) -> RouterProvisioningRequest {
        let request_id = test_request_id(request_id);
        let request = gleaph_graph_kernel::provisioning::wire::ProvisionRequest {
            deployment_id: deployment_id.to_owned(),
            request_id,
            intent_key: ProvisioningIntentKey::new(
                deployment_id,
                LogicalResource::GraphShard(ShardId::new(0)),
            ),
            reserved_graph_id: None,
            graph_name: "tenant.main".to_owned(),
            requested_resources: vec![ProvisionableResource {
                logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
            }],
            install_args: vec![vec![]],
            authorized_caller: Principal::anonymous(),
            release_id: fingerprint.to_owned(),
        };
        RouterProvisioningRequest {
            request_id,
            caller: Principal::anonymous(),
            owner: Principal::anonymous(),
            admins: Default::default(),
            provision_target: Principal::from_slice(&[3; 29]),
            resolved_request_bytes: Encode!(&request).expect("encode sample request"),
            state,
            created_at_ns: 0,
        }
    }

    fn test_request_id(label: &str) -> [u8; 32] {
        let mut id = [0u8; 32];
        let bytes = label.as_bytes();
        let n = bytes.len().min(32);
        id[..n].copy_from_slice(&bytes[..n]);
        id
    }

    fn job_view() -> ProvisionJobSummary {
        ProvisionJobSummary {
            request_id: test_request_id("req"),
            deployment_id: "deploy".to_owned(),
            state: "AwaitingAck".to_owned(),
            active_resource_index: 0,
            completed_effect_count: 0,
        }
    }

    fn store() -> RouterProvisioningRequestStore {
        RouterProvisioningRequestStore::new()
    }

    fn graph_args(
        deployment_id: &str,
        graph_name: &str,
        resources: Vec<ProvisionableResource>,
    ) -> crate::types::ProvisionGraphArgs {
        crate::types::ProvisionGraphArgs {
            deployment_id: deployment_id.to_owned(),
            graph_name: graph_name.to_owned(),
            requested_resources: resources,
            authorized_caller: Principal::from_slice(&[1; 29]),
            release_id: "rel-1".to_owned(),
            owner: Principal::from_slice(&[1; 29]),
            admins: Default::default(),
        }
    }

    fn record_for_args(
        caller: Principal,
        args: &crate::types::ProvisionGraphArgs,
    ) -> RouterProvisioningRequest {
        let request_id = gleaph_graph_kernel::provisioning::wire::provisioning_request_id(
            &args.graph_name,
            &args.requested_resources,
        );
        let request = gleaph_graph_kernel::provisioning::wire::ProvisionRequest {
            deployment_id: args.deployment_id.clone(),
            request_id,
            intent_key: ProvisioningIntentKey::new(
                &args.deployment_id,
                args.requested_resources[0].logical_resource,
            ),
            reserved_graph_id: None,
            graph_name: args.graph_name.clone(),
            requested_resources: args.requested_resources.clone(),
            install_args: build_install_args_with_router(args, caller),
            authorized_caller: args.authorized_caller,
            release_id: args.release_id.clone(),
        };
        RouterProvisioningRequest {
            request_id,
            caller,
            owner: args.owner,
            admins: args.admins.clone(),
            provision_target: Principal::from_slice(&[0x40; 29]),
            resolved_request_bytes: Encode!(&request).unwrap(),
            state: RouterProvisioningRequestState::AwaitingAck,
            created_at_ns: 1,
        }
    }

    #[test]
    fn graph_bootstrap_accepts_only_exact_graph_shard_zero_before_effects() {
        futures::executor::block_on(async {
            let router_store = crate::facade::store::RouterStore::new();
            router_store.init_from_args(&crate::facade::store::tests::test_init_args());
            let caller = Principal::from_slice(&[0x31; 29]);
            crate::facade::auth::grant_admins(&[caller]);
            crate::provisioning::config::set(Some(Principal::from_slice(&[0x32; 29])));
            let graph_zero = ProvisionableResource {
                logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
            };
            let property = ProvisionableResource {
                logical_resource: LogicalResource::PropertyIndex(
                    gleaph_graph_kernel::federation::IndexClusterId::new(0),
                ),
            };
            let vector = ProvisionableResource {
                logical_resource: LogicalResource::VectorIndex(
                    gleaph_graph_kernel::federation::VectorIndexId::new(0),
                ),
            };
            let router = ProvisionableResource {
                logical_resource: LogicalResource::Router,
            };
            let invalid_shapes = [
                vec![],
                vec![ProvisionableResource {
                    logical_resource: LogicalResource::GraphShard(ShardId::new(1)),
                }],
                vec![graph_zero.clone(), graph_zero.clone()],
                vec![graph_zero.clone(), property.clone()],
                vec![graph_zero.clone(), vector.clone()],
                vec![graph_zero, router.clone()],
                vec![property],
                vec![vector],
                vec![router],
            ];
            let send_count = Rc::new(Cell::new(0usize));
            for (ordinal, resources) in invalid_shapes.into_iter().enumerate() {
                let graph_name = format!("invalid-{ordinal}");
                let args = graph_args(&caller.to_text(), &graph_name, resources);
                let send_count_for_call = Rc::clone(&send_count);
                let error = create_graph_admission_with(
                    caller,
                    &graph_name,
                    args,
                    caller,
                    move |_, _| {
                        send_count_for_call.set(send_count_for_call.get() + 1);
                        async { unreachable!("invalid shape must not dispatch") }
                    },
                    |_, _| async { unreachable!("invalid shape must not ACK") },
                )
                .await
                .expect_err("unsupported Graph-bootstrap shape must reject");
                assert!(
                    matches!(error, RouterError::InvalidArgument(ref message) if message == "Graph bootstrap requires exactly [GraphShard(0)]"),
                    "unexpected rejection for shape {ordinal}: {error:?}"
                );
                assert_eq!(store().map_lengths_for_test(), (0, 0, 0));
                assert_eq!(send_count.get(), 0);
            }
            crate::provisioning::config::set(None);
        });
    }

    #[test]
    fn existing_completed_does_not_resend_and_returns_unit() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-completed";
            let request_id = "req-completed";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-completed",
                RouterProvisioningRequestState::Completed,
            );
            s.insert(deployment_id, record.clone())
                .expect("insert completed");

            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);
            let outcome = InsertionOutcome::Existing(record);

            let result = dispatch_provision_send(
                request_key.clone(),
                outcome,
                s,
                // Sender must not be called for a Completed record.
                |_, _| async { panic!("send must not be called for Completed record") },
            )
            .await
            .expect("completed returns ok");

            assert_eq!(result, crate::types::ProvisionGraphResponse::Completed);
            let stored = store()
                .get_by_request_id(&request_key)
                .expect("record survives");
            assert_eq!(stored.state, RouterProvisioningRequestState::Completed);
        });
    }

    #[test]
    fn existing_awaiting_ack_keeps_record_on_send_failure() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-existing-awaiting";
            let request_id = "req-existing-awaiting";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-existing-awaiting",
                RouterProvisioningRequestState::AwaitingAck,
            );
            s.insert(deployment_id, record.clone())
                .expect("insert awaiting");

            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);
            let outcome = InsertionOutcome::Existing(record);

            let result = dispatch_provision_send(request_key.clone(), outcome, s, |_, _| async {
                Err(RouterOutboundError::CallFailed("simulated".to_owned()))
            })
            .await;

            assert!(
                matches!(result, Err(RouterError::ProvisionCallFailed(_))),
                "expected ProvisionCallFailed, got {result:?}"
            );
            let stored = store()
                .get_by_request_id(&request_key)
                .expect("record survives");
            assert_eq!(stored.state, RouterProvisioningRequestState::AwaitingAck);
        });
    }

    #[test]
    fn retry_compares_full_identity_and_sends_only_stored_target_and_bytes() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-stored-dispatch";
            let request_id = "req-stored-dispatch";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-stored-dispatch",
                RouterProvisioningRequestState::AwaitingAck,
            );
            let expected_target = record.provision_target;
            let expected_bytes = record.resolved_request_bytes.clone();
            assert!(matches!(
                s.insert(deployment_id, record.clone()).unwrap(),
                InsertionOutcome::Inserted(_)
            ));
            let outcome = s.insert(deployment_id, record.clone()).unwrap();
            assert!(matches!(outcome, InsertionOutcome::Existing(_)));
            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);
            let result =
                dispatch_provision_send(request_key.clone(), outcome, s, |target, bytes| {
                    assert_eq!(target, expected_target);
                    assert_eq!(bytes, expected_bytes);
                    async {
                        Ok(ProvisionAcceptResponse::Replay {
                            job_view: job_view(),
                            intent_lock_count: 1,
                            created_resources: vec![],
                        })
                    }
                })
                .await
                .unwrap();
            assert!(matches!(
                result,
                crate::types::ProvisionGraphResponse::Replay { .. }
            ));

            let mut altered_owner = record.clone();
            altered_owner.owner = Principal::from_slice(&[7; 29]);
            let mut altered_caller = record.clone();
            altered_caller.caller = Principal::from_slice(&[6; 29]);
            let mut altered_admins = record.clone();
            altered_admins
                .admins
                .insert(Principal::from_slice(&[8; 29]));
            let mut altered_target = record.clone();
            altered_target.provision_target = Principal::from_slice(&[9; 29]);
            let mut altered_bytes = record.clone();
            let mut altered_envelope = Decode!(
                &altered_bytes.resolved_request_bytes,
                gleaph_graph_kernel::provisioning::wire::ProvisionRequest
            )
            .unwrap();
            altered_envelope.release_id = "different-release".to_owned();
            altered_bytes.resolved_request_bytes = Encode!(&altered_envelope).unwrap();
            for altered in [
                altered_caller,
                altered_owner,
                altered_admins,
                altered_target,
                altered_bytes,
            ] {
                assert_eq!(
                    s.insert(deployment_id, altered),
                    Err(InsertError::IdentityConflict)
                );
            }
            assert_eq!(s.get_by_request_id(&request_key), Some(record));
        });
    }

    #[test]
    fn existing_pending_returns_invalid_state() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-pending";
            let request_id = "req-pending";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-pending",
                RouterProvisioningRequestState::Pending,
            );
            s.insert(deployment_id, record).expect("insert pending");

            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);
            let outcome = InsertionOutcome::Existing(s.get_by_request_id(&request_key).unwrap());

            let result = dispatch_provision_send(request_key, outcome, s, |_, _| async {
                panic!("send must not be called for non-terminal record")
            })
            .await;

            assert!(
                matches!(result, Err(RouterError::InvalidState(_))),
                "expected InvalidState, got {result:?}"
            );
        });
    }

    #[test]
    fn existing_failed_returns_invalid_state() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-failed";
            let request_id = "req-failed";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-failed",
                RouterProvisioningRequestState::Failed {
                    reason: "boom".to_owned(),
                },
            );
            s.insert(deployment_id, record.clone())
                .expect("insert failed");

            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);
            let outcome = InsertionOutcome::Existing(record);

            let result = dispatch_provision_send(request_key, outcome, s, |_, _| async {
                panic!("send must not be called for non-terminal record")
            })
            .await;

            assert!(
                matches!(result, Err(RouterError::InvalidState(_))),
                "expected InvalidState, got {result:?}"
            );
        });
    }

    #[test]
    fn post_dispatch_transport_or_decode_failure_preserves_maps45_46_47() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-fresh-awaiting";
            let request_id = "req-fresh-awaiting";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-fresh-awaiting",
                RouterProvisioningRequestState::AwaitingAck,
            );
            let outcome = s.insert(deployment_id, record).unwrap();
            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);

            let result = dispatch_provision_send(request_key.clone(), outcome, s, |_, _| async {
                Err(RouterOutboundError::CallFailed("simulated".to_owned()))
            })
            .await;

            assert!(
                matches!(result, Err(RouterError::ProvisionCallFailed(_))),
                "expected ProvisionCallFailed, got {result:?}"
            );
            assert!(store().get_by_request_id(&request_key).is_some());
            assert_eq!(store().list_by_graph(deployment_id, "tenant.main").len(), 1);
            assert!(store().intent_locked(
                &ProvisioningIntentKey::new(
                    deployment_id,
                    LogicalResource::GraphShard(ShardId::new(0)),
                ),
                &crate::types::IntentLockOwner::new(request_key),
            ));

            let decode_request_id = "req-decode-failure";
            let decode_deployment_id = "deploy-decode-failure";
            let decode_record = sample_record(
                decode_request_id,
                decode_deployment_id,
                "fp-decode-failure",
                RouterProvisioningRequestState::AwaitingAck,
            );
            let decode_outcome = s.insert(decode_deployment_id, decode_record).unwrap();
            let decode_key = ProvisioningRequestKey::new(
                &test_request_id(decode_request_id),
                decode_deployment_id,
            );
            let result =
                dispatch_provision_send(decode_key.clone(), decode_outcome, s, |_, _| async {
                    Err(RouterOutboundError::EncodingFailed("decode".to_owned()))
                })
                .await;
            assert!(matches!(
                result,
                Err(RouterError::ProvisionEncodingFailed(_))
            ));
            assert!(s.get_by_request_id(&decode_key).is_some());
            assert_eq!(
                s.list_by_graph(decode_deployment_id, "tenant.main").len(),
                1
            );
        });
    }

    #[test]
    fn typed_pre_effect_rejection_rolls_back_only_inserted_request() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-pre-effect";
            let request_id = "req-pre-effect";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-pre-effect",
                RouterProvisioningRequestState::AwaitingAck,
            );
            let outcome = s.insert(deployment_id, record).unwrap();
            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);

            let result = dispatch_provision_send(request_key.clone(), outcome, s, |_, _| async {
                Err(RouterOutboundError::UnknownDeployment)
            })
            .await;
            assert!(matches!(result, Err(RouterError::UnknownDeployment(_))));
            assert!(s.get_by_request_id(&request_key).is_none());

            let existing = sample_record(
                request_id,
                deployment_id,
                "fp-pre-effect",
                RouterProvisioningRequestState::AwaitingAck,
            );
            let inserted = s.insert(deployment_id, existing.clone()).unwrap();
            assert!(matches!(inserted, InsertionOutcome::Inserted(_)));
            let existing_outcome = s.insert(deployment_id, existing).unwrap();
            assert!(matches!(existing_outcome, InsertionOutcome::Existing(_)));
            let result =
                dispatch_provision_send(request_key.clone(), existing_outcome, s, |_, _| async {
                    Err(RouterOutboundError::UnknownDeployment)
                })
                .await;
            assert!(matches!(result, Err(RouterError::UnknownDeployment(_))));
            assert!(s.get_by_request_id(&request_key).is_some());
        });
    }

    #[test]
    fn lost_ack_response_keeps_router_pending_then_replay_completes() {
        futures::executor::block_on(async {
            let router_store = crate::facade::store::RouterStore::new();
            router_store.init_from_args(&crate::facade::store::tests::test_init_args());
            let caller = Principal::from_slice(&[1; 29]);
            crate::facade::auth::grant_admins(&[caller]);
            let provision_target = Principal::from_slice(&[0x40; 29]);
            crate::provisioning::config::set(Some(provision_target));
            let args = graph_args(
                &caller.to_text(),
                "tenant.lost-ack",
                vec![ProvisionableResource {
                    logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                }],
            );
            let request_id = gleaph_graph_kernel::provisioning::wire::provisioning_request_id(
                &args.graph_name,
                &args.requested_resources,
            );
            let key = ProvisioningRequestKey::new(&request_id, &args.deployment_id);
            let graph_canister = Principal::from_slice(&[0x71; 29]);
            let created = vec![gleaph_graph_kernel::provisioning::wire::CreatedResource {
                logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                canister_id: graph_canister,
                artifact_hash: [0xAB; 32],
            }];
            let accepted_bytes = Rc::new(RefCell::new(None::<Vec<u8>>));
            let provision_effect_count = Rc::new(Cell::new(0u32));
            let provision_completed = Rc::new(Cell::new(false));
            let ack_order_checks = Rc::new(Cell::new(0u32));

            let first_bytes = Rc::clone(&accepted_bytes);
            let first_effect_count = Rc::clone(&provision_effect_count);
            let first_created = created.clone();
            let first_ack_completed = Rc::clone(&provision_completed);
            let first_ack_order_checks = Rc::clone(&ack_order_checks);
            let first = create_graph_admission_with(
                caller,
                &args.graph_name,
                args.clone(),
                caller,
                move |target, bytes| {
                    assert_eq!(target, provision_target);
                    assert!(first_bytes.borrow().is_none());
                    first_bytes.replace(Some(bytes));
                    first_effect_count.set(first_effect_count.get() + 1);
                    async move {
                        Ok(ProvisionAcceptResponse::Accepted {
                            job_view: ProvisionJobSummary {
                                request_id,
                                deployment_id: caller.to_text(),
                                state: "RouterRegistrationPending".to_owned(),
                                active_resource_index: 0,
                                completed_effect_count: 1,
                            },
                            intent_lock_count: 1,
                            created_resources: first_created,
                        })
                    }
                },
                move |target, ack| {
                    assert_eq!(target, provision_target);
                    assert_eq!(ack.request_id, request_id);
                    assert_eq!(ack.deployment_id, caller.to_text());
                    let graph_id =
                        crate::facade::stable::graph_catalog::lookup_graph_id("tenant.lost-ack")
                            .expect("graph must commit before ACK");
                    let shard = crate::facade::stable::graph_catalog::lookup_shard_entry(
                        graph_id,
                        ShardId::new(0),
                    )
                    .expect("shard must commit before ACK");
                    assert_eq!(shard.graph_canister, graph_canister);
                    first_ack_order_checks.set(first_ack_order_checks.get() + 1);
                    first_ack_completed.set(true);
                    async {
                        Err(RouterOutboundError::CallFailed(
                            "registration ACK response lost".to_owned(),
                        ))
                    }
                },
            )
            .await;
            assert!(matches!(first, Err(RouterError::ProvisionCallFailed(_))));

            let s = store();
            let pending = s.get_by_request_id(&key).expect("Map 45 remains pending");
            assert_eq!(pending.state, RouterProvisioningRequestState::AwaitingAck);
            assert!(s.intent_locked(
                &ProvisioningIntentKey::new(
                    &args.deployment_id,
                    LogicalResource::GraphShard(ShardId::new(0)),
                ),
                &crate::types::IntentLockOwner::new(key.clone()),
            ));
            assert_eq!(s.map_lengths_for_test(), (1, 1, 1));
            assert_eq!(ack_order_checks.get(), 1);
            assert!(provision_completed.get());
            let effects_before_retry = provision_effect_count.get();

            crate::facade::stable::reopen_provisioning_regions_for_test();
            let reopened = store();
            let reopened_record = reopened
                .get_by_request_id(&key)
                .expect("Map 45 must reopen from the same stable memory");
            let stored_bytes = accepted_bytes
                .borrow()
                .clone()
                .expect("first accepted envelope captured");
            assert_eq!(reopened_record.resolved_request_bytes, stored_bytes);
            assert_eq!(reopened.map_lengths_for_test(), (1, 1, 1));

            let replay_bytes = stored_bytes.clone();
            let replay_effect_count = Rc::clone(&provision_effect_count);
            let replay_completed = Rc::clone(&provision_completed);
            let replay_created = created;
            let replay_ack_order_checks = Rc::clone(&ack_order_checks);
            let key_for_replay_ack = key.clone();
            create_graph_admission_with(
                caller,
                &args.graph_name,
                args.clone(),
                caller,
                move |target, bytes| {
                    assert_eq!(target, provision_target);
                    assert_eq!(bytes, replay_bytes);
                    assert!(replay_completed.get());
                    let unchanged_effect_count = replay_effect_count.get();
                    async move {
                        assert_eq!(unchanged_effect_count, effects_before_retry);
                        Ok(ProvisionAcceptResponse::Replay {
                            job_view: ProvisionJobSummary {
                                request_id,
                                deployment_id: caller.to_text(),
                                state: "Completed".to_owned(),
                                active_resource_index: 0,
                                completed_effect_count: unchanged_effect_count,
                            },
                            intent_lock_count: 0,
                            created_resources: replay_created,
                        })
                    }
                },
                move |target, ack| {
                    assert_eq!(target, provision_target);
                    assert_eq!(ack.request_id, request_id);
                    let graph_id =
                        crate::facade::stable::graph_catalog::lookup_graph_id("tenant.lost-ack")
                            .expect("replayed graph remains committed before ACK");
                    assert!(
                        crate::facade::stable::graph_catalog::lookup_shard_entry(
                            graph_id,
                            ShardId::new(0),
                        )
                        .is_some(),
                        "replayed shard remains committed before ACK"
                    );
                    assert_eq!(
                        store()
                            .get_by_request_id(&key_for_replay_ack)
                            .unwrap()
                            .state,
                        RouterProvisioningRequestState::AwaitingAck,
                        "Router completion must occur after the ACK response"
                    );
                    replay_ack_order_checks.set(replay_ack_order_checks.get() + 1);
                    async { Ok(RouterRegistrationAckResponse::Replay) }
                },
            )
            .await
            .expect("Provision ACK Replay converges Router state");

            assert_eq!(provision_effect_count.get(), effects_before_retry);
            assert_eq!(ack_order_checks.get(), 2);
            assert_eq!(
                reopened.get_by_request_id(&key).unwrap().state,
                RouterProvisioningRequestState::Completed
            );
            assert!(!reopened.intent_locked(
                &ProvisioningIntentKey::new(
                    &args.deployment_id,
                    LogicalResource::GraphShard(ShardId::new(0)),
                ),
                &crate::types::IntentLockOwner::new(key),
            ));
            assert_eq!(reopened.map_lengths_for_test(), (1, 1, 0));
            crate::provisioning::config::set(None);
        });
    }

    #[test]
    fn reopen_retry_uses_byte_exact_stored_envelope() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-reopen";
            let request_id = "req-reopen";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-reopen",
                RouterProvisioningRequestState::AwaitingAck,
            );
            let expected_target = record.provision_target;
            let expected_bytes = record.resolved_request_bytes.clone();
            s.insert(deployment_id, record).unwrap();

            crate::facade::stable::reopen_provisioning_regions_for_test();
            let reopened = RouterProvisioningRequestStore::new();
            let key = ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);
            let reopened_record = reopened.get_by_request_id(&key).unwrap();
            assert_eq!(reopened_record.resolved_request_bytes, expected_bytes);
            dispatch_provision_send(
                key,
                InsertionOutcome::Existing(reopened_record),
                reopened,
                |target, bytes| {
                    assert_eq!(target, expected_target);
                    assert_eq!(bytes, expected_bytes);
                    async {
                        Ok(ProvisionAcceptResponse::Replay {
                            job_view: job_view(),
                            intent_lock_count: 1,
                            created_resources: vec![],
                        })
                    }
                },
            )
            .await
            .unwrap();
        });
    }

    #[test]
    fn inserted_awaiting_ack_returns_accepted_on_send_success() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-fresh-success";
            let request_id = "req-fresh-success";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-fresh-success",
                RouterProvisioningRequestState::AwaitingAck,
            );
            let outcome = s.insert(deployment_id, record).unwrap();
            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);

            let result = dispatch_provision_send(request_key, outcome, s, |_, _| async {
                Ok(ProvisionAcceptResponse::Accepted {
                    job_view: job_view(),
                    intent_lock_count: 1,
                    created_resources: vec![],
                })
            })
            .await
            .expect("fresh send succeeds");

            assert!(
                matches!(
                    result,
                    crate::types::ProvisionGraphResponse::Accepted { .. }
                ),
                "expected Accepted, got {result:?}"
            );
        });
    }

    #[test]
    fn admission_short_circuits_registered_name_without_provisioner() {
        futures::executor::block_on(async {
            let store = crate::facade::store::RouterStore::new();
            store.init_from_args(&crate::facade::store::tests::test_init_args());
            let caller = Principal::from_slice(&[7; 29]);
            crate::facade::auth::grant_admins(&[caller]);
            crate::facade::store::tests::register_test_graph(&store, caller, "tenant.existing");

            // A pre-registered name must take the binding-only path: no provisioner configured,
            // yet the bridge succeeds because no provisioning is needed.
            crate::provisioning::graph::create_graph_admission(caller, "tenant.existing")
                .await
                .expect("registered name skips provisioning");
        });
    }

    #[test]
    fn admission_fails_closed_for_unregistered_name_without_provisioner() {
        futures::executor::block_on(async {
            let store = crate::facade::store::RouterStore::new();
            store.init_from_args(&crate::facade::store::tests::test_init_args());
            let caller = Principal::from_slice(&[8; 29]);
            crate::facade::auth::grant_admins(&[caller]);

            let error =
                crate::provisioning::graph::create_graph_admission(caller, "tenant.unregistered")
                    .await
                    .expect_err("dev mode must fail closed");
            assert!(
                matches!(error, RouterError::NotImplemented(ref message) if message.contains("provision canister")),
                "expected NotImplemented provisioner error, got {error:?}"
            );
            assert!(
                crate::facade::stable::graph_catalog::lookup_graph_id("tenant.unregistered")
                    .is_none(),
                "failed admission must not leave a registered graph"
            );
        });
    }

    #[test]
    fn accepted_and_replay_share_graph_reconciliation() {
        futures::executor::block_on(async {
            use gleaph_graph_kernel::provisioning::wire::CreatedResource;
            use std::collections::BTreeSet;

            let store = crate::facade::store::RouterStore::new();
            store.init_from_args(&crate::facade::store::tests::test_init_args());
            let admin = Principal::from_slice(&[1; 29]);
            crate::facade::auth::grant_admins(&[admin]);

            let args = crate::types::ProvisionGraphArgs {
                deployment_id: "dep-1".to_owned(),
                graph_name: "tenant.provisioned".to_owned(),
                requested_resources: vec![ProvisionableResource {
                    logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                }],
                authorized_caller: admin,
                release_id: "rel-1".to_owned(),
                owner: admin,
                admins: BTreeSet::new(),
            };
            let graph_canister = Principal::from_slice(&[0x50; 29]);
            let created = vec![CreatedResource {
                logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                canister_id: graph_canister,
                artifact_hash: [0xAB; 32],
            }];
            let request_id = gleaph_graph_kernel::provisioning::wire::provisioning_request_id(
                &args.graph_name,
                &args.requested_resources,
            );
            let request = gleaph_graph_kernel::provisioning::wire::ProvisionRequest {
                deployment_id: args.deployment_id.clone(),
                request_id,
                intent_key: ProvisioningIntentKey::new(
                    &args.deployment_id,
                    LogicalResource::GraphShard(ShardId::new(0)),
                ),
                reserved_graph_id: None,
                graph_name: args.graph_name.clone(),
                requested_resources: args.requested_resources.clone(),
                install_args: build_install_args_with_router(&args, admin),
                authorized_caller: args.authorized_caller,
                release_id: args.release_id.clone(),
            };
            let record = RouterProvisioningRequest {
                request_id,
                caller: admin,
                owner: args.owner,
                admins: args.admins.clone(),
                provision_target: Principal::from_slice(&[0x40; 29]),
                resolved_request_bytes: Encode!(&request).unwrap(),
                state: RouterProvisioningRequestState::AwaitingAck,
                created_at_ns: 1,
            };

            reconcile_provisioned_graph(&record, &created)
                .await
                .expect("fresh reconcile");
            reconcile_provisioned_graph(&record, &created)
                .await
                .expect("replay reconcile");

            // ADR 0070: the first graph bootstrap claims the single global home slot.
            assert!(
                crate::facade::store::any_home_graph_exists(),
                "first provisioned registration must set is_home"
            );

            let graph_id =
                crate::facade::stable::graph_catalog::lookup_graph_id("tenant.provisioned")
                    .expect("graph id");
            assert_ne!(graph_id, gleaph_graph_kernel::entry::GraphId::from_raw(0));
            // The graph shard is registered and indexless (anonymous index target).
            let shard = store
                .resolve_shard(graph_id, ShardId::new(0))
                .expect("shard resolved");
            assert_eq!(shard.graph_canister, graph_canister);
            assert_eq!(shard.index_canister, Principal::anonymous());
        });
    }

    #[test]
    fn partial_graph_row_retry_adds_missing_shard_before_ack() {
        futures::executor::block_on(async {
            use gleaph_gql_ic::graph_registry::{
                GraphRegistryEntry, GraphStatus, ProvisioningState,
            };
            use gleaph_graph_kernel::provisioning::wire::CreatedResource;

            let store = crate::facade::store::RouterStore::new();
            store.init_from_args(&crate::facade::store::tests::test_init_args());
            let admin = Principal::from_slice(&[1; 29]);
            crate::facade::auth::grant_admins(&[admin]);
            let args = graph_args(
                "dep-partial",
                "tenant.partial-retry",
                vec![ProvisionableResource {
                    logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                }],
            );
            let graph_canister = Principal::from_slice(&[0x61; 29]);
            store
                .admin_register_graph_with_random_key(
                    admin,
                    GraphRegistryEntry {
                        graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(0),
                        canister_id: graph_canister,
                        owner: args.owner,
                        admins: args.admins.clone(),
                        status: GraphStatus::Active,
                        version: 1,
                        updated_at_ns: 1,
                        provisioning_state: ProvisioningState::None,
                        is_home: false,
                    },
                    &args.graph_name,
                )
                .await
                .unwrap();
            let graph_id =
                crate::facade::stable::graph_catalog::lookup_graph_id(&args.graph_name).unwrap();
            assert!(
                crate::facade::stable::graph_catalog::lookup_shard_entry(
                    graph_id,
                    ShardId::new(0),
                )
                .is_none()
            );

            let record = record_for_args(admin, &args);
            reconcile_provisioned_graph(
                &record,
                &[CreatedResource {
                    logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                    canister_id: graph_canister,
                    artifact_hash: [0xAB; 32],
                }],
            )
            .await
            .unwrap();
            let shard =
                crate::facade::stable::graph_catalog::lookup_shard_entry(graph_id, ShardId::new(0))
                    .unwrap();
            assert_eq!(shard.graph_canister, graph_canister);
            assert!(shard.index_attached);
        });
    }

    #[test]
    fn create_graph_resumes_pending_maps46_47_to_45_before_registered_early_return() {
        futures::executor::block_on(async {
            use gleaph_gql_ic::graph_registry::{
                GraphRegistryEntry, GraphStatus, ProvisioningState,
            };

            let router_store = crate::facade::store::RouterStore::new();
            router_store.init_from_args(&crate::facade::store::tests::test_init_args());
            let caller = Principal::from_slice(&[1; 29]);
            crate::facade::auth::grant_admins(&[caller]);
            let args = graph_args(
                &caller.to_text(),
                "tenant.pending-before-shortcut",
                vec![ProvisionableResource {
                    logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                }],
            );
            let record = record_for_args(caller, &args);
            let key = ProvisioningRequestKey::new(&record.request_id, &args.deployment_id);
            let request_id = key.request_id;
            let provision_target = record.provision_target;
            let expected_bytes = record.resolved_request_bytes.clone();
            crate::provisioning::config::set(Some(provision_target));
            RouterProvisioningRequestStore::new()
                .insert(&args.deployment_id, record.clone())
                .unwrap();
            let graph_canister = Principal::from_slice(&[0x70; 29]);
            router_store
                .admin_register_graph_with_random_key(
                    caller,
                    GraphRegistryEntry {
                        graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(0),
                        canister_id: graph_canister,
                        owner: args.owner,
                        admins: args.admins.clone(),
                        status: GraphStatus::Active,
                        version: 1,
                        updated_at_ns: 1,
                        provisioning_state: ProvisioningState::None,
                        is_home: false,
                    },
                    &args.graph_name,
                )
                .await
                .unwrap();

            let graph_id =
                crate::facade::stable::graph_catalog::lookup_graph_id(&args.graph_name).unwrap();
            assert!(
                crate::facade::stable::graph_catalog::lookup_shard_entry(
                    graph_id,
                    ShardId::new(0),
                )
                .is_none(),
                "partial graph row must not be mistaken for completed admission"
            );

            let send_count = Rc::new(Cell::new(0u32));
            let ack_count = Rc::new(Cell::new(0u32));
            let send_count_for_call = Rc::clone(&send_count);
            let ack_count_for_call = Rc::clone(&ack_count);
            let graph_name_for_ack = args.graph_name.clone();
            let key_for_ack = key.clone();
            let created = vec![gleaph_graph_kernel::provisioning::wire::CreatedResource {
                logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                canister_id: graph_canister,
                artifact_hash: [0xCD; 32],
            }];
            create_graph_admission_with(
                caller,
                &args.graph_name,
                args.clone(),
                caller,
                move |target, bytes| {
                    assert_eq!(target, provision_target);
                    assert_eq!(bytes, expected_bytes);
                    send_count_for_call.set(send_count_for_call.get() + 1);
                    async move {
                        Ok(ProvisionAcceptResponse::Replay {
                            job_view: ProvisionJobSummary {
                                request_id,
                                deployment_id: caller.to_text(),
                                state: "RouterRegistrationPending".to_owned(),
                                active_resource_index: 0,
                                completed_effect_count: 1,
                            },
                            intent_lock_count: 1,
                            created_resources: created,
                        })
                    }
                },
                move |target, ack| {
                    assert_eq!(target, provision_target);
                    assert_eq!(ack.request_id, key_for_ack.request_id);
                    let graph_id =
                        crate::facade::stable::graph_catalog::lookup_graph_id(&graph_name_for_ack)
                            .expect("partial graph remains registered");
                    let shard = crate::facade::stable::graph_catalog::lookup_shard_entry(
                        graph_id,
                        ShardId::new(0),
                    )
                    .expect("public retry must add the missing shard before ACK");
                    assert_eq!(shard.graph_canister, graph_canister);
                    assert_eq!(
                        store().get_by_request_id(&key_for_ack).unwrap().state,
                        RouterProvisioningRequestState::AwaitingAck
                    );
                    ack_count_for_call.set(ack_count_for_call.get() + 1);
                    async { Ok(RouterRegistrationAckResponse::Replay) }
                },
            )
            .await
            .expect("public CREATE GRAPH retry must bypass the catalog shortcut and converge");

            assert_eq!(send_count.get(), 1);
            assert_eq!(ack_count.get(), 1);
            let completed = store().get_by_request_id(&key).unwrap();
            assert_eq!(completed.state, RouterProvisioningRequestState::Completed);
            assert!(
                crate::facade::stable::graph_catalog::lookup_shard_entry(
                    graph_id,
                    ShardId::new(0),
                )
                .is_some()
            );
            assert_eq!(store().map_lengths_for_test(), (1, 1, 0));
            crate::provisioning::config::set(None);
        });
    }

    #[test]
    fn reconcile_rejects_wrong_graph_or_shard_identity_without_overwrite() {
        futures::executor::block_on(async {
            use gleaph_gql_ic::graph_registry::{
                GraphRegistryEntry, GraphStatus, ProvisioningState,
            };
            use gleaph_graph_kernel::provisioning::wire::CreatedResource;

            let store = crate::facade::store::RouterStore::new();
            store.init_from_args(&crate::facade::store::tests::test_init_args());
            let admin = Principal::from_slice(&[1; 29]);
            crate::facade::auth::grant_admins(&[admin]);

            let wrong_graph_args = graph_args(
                "dep-wrong-graph",
                "tenant.wrong-graph",
                vec![ProvisionableResource {
                    logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                }],
            );
            let expected_graph_canister = Principal::from_slice(&[0x62; 29]);
            let existing_graph_canister = Principal::from_slice(&[0x63; 29]);
            store
                .admin_register_graph_with_random_key(
                    admin,
                    GraphRegistryEntry {
                        graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(0),
                        canister_id: existing_graph_canister,
                        owner: Principal::from_slice(&[0x64; 29]),
                        admins: Default::default(),
                        status: GraphStatus::Active,
                        version: 1,
                        updated_at_ns: 1,
                        provisioning_state: ProvisioningState::None,
                        is_home: false,
                    },
                    &wrong_graph_args.graph_name,
                )
                .await
                .unwrap();
            let error = reconcile_provisioned_graph(
                &record_for_args(admin, &wrong_graph_args),
                &[CreatedResource {
                    logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                    canister_id: expected_graph_canister,
                    artifact_hash: [0xAB; 32],
                }],
            )
            .await
            .unwrap_err();
            assert!(matches!(error, RouterError::Conflict(_)));
            let graph_id =
                crate::facade::stable::graph_catalog::lookup_graph_id(&wrong_graph_args.graph_name)
                    .unwrap();
            assert_eq!(
                crate::facade::stable::graph_catalog::graph_entry(graph_id)
                    .unwrap()
                    .canister_id,
                existing_graph_canister
            );
            assert!(
                crate::facade::stable::graph_catalog::lookup_shard_entry(
                    graph_id,
                    ShardId::new(0),
                )
                .is_none()
            );

            let wrong_shard_args = graph_args(
                "dep-wrong-shard",
                "tenant.wrong-shard",
                vec![ProvisionableResource {
                    logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                }],
            );
            let correct_graph_canister = Principal::from_slice(&[0x65; 29]);
            let wrong_shard_canister = Principal::from_slice(&[0x66; 29]);
            store
                .admin_register_graph_with_random_key(
                    admin,
                    GraphRegistryEntry {
                        graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(0),
                        canister_id: correct_graph_canister,
                        owner: wrong_shard_args.owner,
                        admins: wrong_shard_args.admins.clone(),
                        status: GraphStatus::Active,
                        version: 1,
                        updated_at_ns: 1,
                        provisioning_state: ProvisioningState::None,
                        is_home: false,
                    },
                    &wrong_shard_args.graph_name,
                )
                .await
                .unwrap();
            store
                .admin_register_shard(
                    admin,
                    crate::types::AdminRegisterShardArgs {
                        shard_id: ShardId::new(0),
                        graph_canister: wrong_shard_canister,
                        index_canister: Principal::anonymous(),
                        logical_graph_name: wrong_shard_args.graph_name.clone(),
                    },
                )
                .await
                .unwrap();
            let error = reconcile_provisioned_graph(
                &record_for_args(admin, &wrong_shard_args),
                &[CreatedResource {
                    logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
                    canister_id: correct_graph_canister,
                    artifact_hash: [0xCD; 32],
                }],
            )
            .await
            .unwrap_err();
            assert!(matches!(error, RouterError::Conflict(_)));
            let graph_id =
                crate::facade::stable::graph_catalog::lookup_graph_id(&wrong_shard_args.graph_name)
                    .unwrap();
            assert_eq!(
                crate::facade::stable::graph_catalog::lookup_shard_entry(
                    graph_id,
                    ShardId::new(0),
                )
                .unwrap()
                .graph_canister,
                wrong_shard_canister
            );
        });
    }
}
