//! Provisioned graph-creation flow shared by the public `register_graph` fold and the retained
//! L3 seam (ADR 0035 Slice 5/8, ADR 0056 §6).
//!
//! This module owns the Router-side orchestration of one provisioned graph admission:
//! build the resolved `ProvisionRequest` envelope from the intent, send it to the configured
//! Provision canister, and — on a fresh `Accepted` with created resources — commit the returned
//! graph shard canisters into the Router catalog. It is the single caller of the provisioning
//! request store and the outbound sender, keeping the `api` layer modules thin siblings that do
//! not call each other.

use candid::Principal;

use crate::facade::store::RouterStore;
use crate::facade::store::provisioning::{InsertError, RouterProvisioningRequestStore};
use crate::provisioning::sender::send_accept_envelope;
use crate::state::RouterError;
use crate::types;
use crate::types::{RouterOutboundError, RouterProvisioningRequestState};
use gleaph_graph_kernel::provisioning::{LogicalResource, ProvisioningIntentKey};

use crate::types::{ProvisioningRequestKey, RouterProvisioningRequest};

/// Run the provisioned graph-admission flow for an intent. `caller` is the authorized admin.
///
/// Returns the ingress response mirror. On a fresh `Accepted` with non-empty `created_resources`
/// it also registers the provisioned graph and its shards into the Router catalog.
pub(crate) async fn provision_graph_flow(
    caller: Principal,
    args: types::ProvisionGraphArgs,
) -> Result<types::ProvisionGraphResponse, RouterError> {
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
    let seed_record = RouterProvisioningRequest {
        request_id,
        caller,
        graph_name: args.graph_name.clone(),
        reserved_graph_id: None,
        requested_resources: args.requested_resources.clone(),
        state: RouterProvisioningRequestState::AwaitingAck,
        provision_receipt: None,
        accepted_registry_version: None,
        created_at_ns: crate::facade::store::ic_time_ns(),
    };
    let outcome = store
        .insert(&deployment_id, seed_record)
        .map_err(|err| match err {
            InsertError::IntentConflict => {
                RouterError::Conflict("provisioning intent already locked".to_owned())
            }
            InsertError::InvalidDuplicateIntent => {
                RouterError::InvalidArgument("duplicate requested resources".to_owned())
            }
        })?;

    let install_args = build_install_args(&args);
    let graph_name_for_request = args.graph_name.clone();
    let requested_resources_for_request = args.requested_resources.clone();
    let request = gleaph_graph_kernel::provisioning::wire::ProvisionRequest {
        deployment_id,
        request_id,
        intent_key,
        reserved_graph_id: None,
        graph_name: graph_name_for_request,
        requested_resources: requested_resources_for_request,
        install_args,
        authorized_caller: args.authorized_caller,
        release_id: args.release_id.clone(),
        // Sender will overwrite this with ic_cdk::api::canister_self() before encoding.
        router_callback_principal: Principal::anonymous(),
    };

    let response = dispatch_provision_send(request_key, outcome, store, || {
        send_accept_envelope(provision_canister, request)
    })
    .await?;

    // Register the provisioned graph and its shards into the Router catalog from the created
    // canisters. Only a fresh `Accepted` with created resources carries canister ids; a `Replay`
    // returns whatever was already recorded and a `Completed` returns the acked version, neither
    // of which triggers a new registration.
    if let types::ProvisionGraphResponse::Accepted {
        created_resources, ..
    } = &response
        && !created_resources.is_empty()
        && created_resources
            .iter()
            .any(|r| matches!(r.logical_resource, LogicalResource::GraphShard(_)))
    {
        // A graph bootstrap (or a batch that created a shard) registers the graph and its shards.
        // An add-on provision (e.g. a VectorIndex for an existing graph) carries no GraphShard; the
        // graph is already registered and needs no graph/shard registration here.
        register_provisioned_graph(caller, &args, created_resources).await?;
    }

    Ok(response)
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
    F: FnOnce() -> Fut,
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
            let accept_response = match send().await {
                Ok(response) => response,
                Err(e) => {
                    if is_inserted {
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
            let version = record.accepted_registry_version.ok_or_else(|| {
                RouterError::InvalidState(
                    "completed record missing accepted_registry_version".to_owned(),
                )
            })?;
            Ok(types::ProvisionGraphResponse::Completed {
                accepted_registry_version: version,
            })
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
        RouterOutboundError::Conflict => RouterError::ProvisionConflict("conflict".to_owned()),
        RouterOutboundError::IngressRejected(s) => RouterError::ProvisionRejected(s),
        RouterOutboundError::EncodingFailed(s) => RouterError::ProvisionEncodingFailed(s),
    }
}

/// Register a provisioned graph and its shards into the Router catalog from the canister ids the
/// Provision canister returned. The Router is the sole owner of logical topology; this commits
/// the graph registry entry and each graph shard (+ its index canister) so subsequent reads and
/// DML resolve correctly.
async fn register_provisioned_graph(
    caller: Principal,
    args: &types::ProvisionGraphArgs,
    created_resources: &[gleaph_graph_kernel::provisioning::wire::CreatedResource],
) -> Result<(), RouterError> {
    use gleaph_gql_ic::graph_registry::{GraphStatus, ProvisioningState};

    let store = RouterStore::new();

    // A property index canister may be requested without a graph shard in the same batch only if
    // the graph already exists. For the initial graph bootstrap the graph shard is mandatory, so
    // reject a batch that contains an index but no graph shard (the graph cannot be placed).
    let graph_shard = created_resources
        .iter()
        .find(|r| matches!(r.logical_resource, LogicalResource::GraphShard(_)))
        .ok_or_else(|| {
            RouterError::InvalidArgument(
                "provisioned graph registration requires at least one GraphShard resource"
                    .to_owned(),
            )
        })?;
    let graph_canister = graph_shard.canister_id;

    let entry = gleaph_gql_ic::graph_registry::GraphRegistryEntry {
        graph_id: gleaph_graph_kernel::entry::GraphId::from_raw(0), // store assigns
        graph_name: args.graph_name.clone(),
        canister_id: graph_canister,
        owner: args.owner,
        admins: args.admins.clone(),
        status: GraphStatus::Active,
        version: 1,
        updated_at_ns: crate::facade::store::ic_time_ns(),
        provisioning_state: ProvisioningState::None,
        is_home: false,
    };
    store
        .admin_register_graph_with_random_key(caller, entry)
        .await?;

    // Register each created graph shard, pairing it with the index canister from the same
    // `IndexClusterId` group when one was requested. Indexless shards (ADR 0054) pass an
    // anonymous index target, which the registry now accepts.
    for resource in created_resources
        .iter()
        .filter(|r| matches!(r.logical_resource, LogicalResource::GraphShard(_)))
    {
        let LogicalResource::GraphShard(shard_id) = resource.logical_resource else {
            continue;
        };
        let index_canister = created_resources
            .iter()
            .find(|r| matches!(r.logical_resource, LogicalResource::PropertyIndex(_)))
            .map(|r| r.canister_id)
            .unwrap_or(Principal::anonymous());
        store
            .admin_register_shard(
                caller,
                types::AdminRegisterShardArgs {
                    shard_id,
                    graph_canister: resource.canister_id,
                    index_canister,
                    logical_graph_name: args.graph_name.clone(),
                },
            )
            .await?;
    }
    Ok(())
}

/// Build Candid-encoded init args for each requested resource, in `requested_resources` order.
/// The Router owns logical topology and constructs these; Provision installs them verbatim.
fn build_install_args(args: &types::ProvisionGraphArgs) -> Vec<Vec<u8>> {
    use candid::Encode;
    use gleaph_graph_kernel::provisioning::init_args::{
        DEFAULT_DEFINITION_MAP_SEED, DEFAULT_SUBJECT_MAP_SEED, GraphInitArgs, IndexInitArgs,
        VectorCanisterInitArgs,
    };
    let router_principal = ic_cdk::api::canister_self();
    args.requested_resources
        .iter()
        .map(|resource| match resource.logical_resource {
            LogicalResource::GraphShard(shard_id) => {
                let init = GraphInitArgs {
                    logical_graph_name: Some(args.graph_name.clone()),
                    router_canister: Some(router_principal),
                    shard_id: Some(shard_id),
                    index_canister: None,
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
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;
    use gleaph_graph_kernel::federation::ShardId;
    use gleaph_graph_kernel::provisioning::LogicalResource;
    use gleaph_graph_kernel::provisioning::wire::{ProvisionAcceptResponse, ProvisionJobSummary};

    use crate::facade::store::provisioning::{InsertionOutcome, RouterProvisioningRequestStore};
    use crate::state::RouterError;
    use crate::types::{
        ProvisionableResource, ProvisioningRequestKey, RouterOutboundError,
        RouterProvisioningRequest, RouterProvisioningRequestState,
    };

    fn sample_record(
        request_id: &str,
        _deployment_id: &str,
        _fingerprint: &str,
        state: RouterProvisioningRequestState,
        version: Option<u64>,
    ) -> RouterProvisioningRequest {
        RouterProvisioningRequest {
            request_id: test_request_id(request_id),
            caller: Principal::anonymous(),
            graph_name: "tenant.main".to_owned(),
            reserved_graph_id: None,
            requested_resources: vec![ProvisionableResource {
                logical_resource: LogicalResource::GraphShard(ShardId::new(0)),
            }],
            state,
            provision_receipt: None,
            accepted_registry_version: version,
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
            accepted_registry_version: None,
        }
    }

    fn store() -> RouterProvisioningRequestStore {
        RouterProvisioningRequestStore::new()
    }

    #[test]
    fn existing_completed_does_not_resend_and_returns_version() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-completed";
            let request_id = "req-completed";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-completed",
                RouterProvisioningRequestState::Completed,
                Some(7),
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
                || async { panic!("send must not be called for Completed record") },
            )
            .await
            .expect("completed returns ok");

            assert_eq!(
                result,
                crate::types::ProvisionGraphResponse::Completed {
                    accepted_registry_version: 7
                }
            );
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
                None,
            );
            s.insert(deployment_id, record.clone())
                .expect("insert awaiting");

            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);
            let outcome = InsertionOutcome::Existing(record);

            let result = dispatch_provision_send(request_key.clone(), outcome, s, || async {
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
                None,
            );
            s.insert(deployment_id, record).expect("insert pending");

            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);
            let outcome = InsertionOutcome::Existing(s.get_by_request_id(&request_key).unwrap());

            let result = dispatch_provision_send(request_key, outcome, s, || async {
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
                None,
            );
            s.insert(deployment_id, record.clone())
                .expect("insert failed");

            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);
            let outcome = InsertionOutcome::Existing(record);

            let result = dispatch_provision_send(request_key, outcome, s, || async {
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
    fn inserted_awaiting_ack_rolls_back_on_send_failure() {
        futures::executor::block_on(async {
            let deployment_id = "deploy-fresh-awaiting";
            let request_id = "req-fresh-awaiting";
            let s = store();
            let record = sample_record(
                request_id,
                deployment_id,
                "fp-fresh-awaiting",
                RouterProvisioningRequestState::AwaitingAck,
                None,
            );
            let outcome = InsertionOutcome::Inserted(record);
            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);

            let result = dispatch_provision_send(request_key.clone(), outcome, s, || async {
                Err(RouterOutboundError::CallFailed("simulated".to_owned()))
            })
            .await;

            assert!(
                matches!(result, Err(RouterError::ProvisionCallFailed(_))),
                "expected ProvisionCallFailed, got {result:?}"
            );
            assert!(store().get_by_request_id(&request_key).is_none());
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
                None,
            );
            let outcome = InsertionOutcome::Inserted(record);
            let request_key =
                ProvisioningRequestKey::new(&test_request_id(request_id), deployment_id);

            let result = dispatch_provision_send(request_key, outcome, s, || async {
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
    fn register_provisioned_graph_indexless_commits_graph_and_shard() {
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

            register_provisioned_graph(admin, &args, &created)
                .await
                .expect("register provisioned graph");

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
}
