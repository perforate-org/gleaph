//! Router provisioning-request catalog store (ADR 0035 Slice 1).
//!
//! Owns three stable-memory regions:
//! - `ROUTER_PROVISIONING_REQUESTS`: canonical `(request_id, deployment_id) → RouterProvisioningRequest`
//! - `ROUTER_PROVISIONING_BY_GRAPH`: derived `(deployment_id, graph_name, request_id) → ProvisioningRequestKey`
//! - `ROUTER_PROVISIONING_INTENT_LOCK`: canonical `(deployment_id, resource_kind, logical_resource_key) → IntentLockOwner`

// These pub(crate) items are exercised in unit tests and will be reached by Router ingress and
// callback paths in later slices; allow dead_code while they remain crate-internal in Slice 1.
#![allow(dead_code)]

use candid::Decode;
use std::collections::HashSet;

use crate::facade::stable::{
    ROUTER_PROVISIONING_BY_GRAPH, ROUTER_PROVISIONING_INTENT_LOCK, ROUTER_PROVISIONING_REQUESTS,
};
use crate::types::{
    IntentLockOwner, ProvisioningByGraphKey, ProvisioningIntentKey, ProvisioningRequestKey,
    RouterProvisioningRequest, RouterProvisioningRequestState,
};
use gleaph_graph_kernel::provisioning::wire::ProvisionRequest;

/// Failure modes for `RouterProvisioningRequestStore::insert`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertError {
    /// At least one requested intent is already locked by another non-terminal request.
    IntentConflict,
    /// The request contains duplicate `(kind, logical_resource_key)` resources.
    InvalidDuplicateIntent,
    /// The encoded envelope is invalid, oversized, or disagrees with the Map 45 key.
    InvalidEnvelope,
    /// The same request key was reused with different immutable semantic identity.
    IdentityConflict,
}

/// Ownership signal returned by `RouterProvisioningRequestStore::insert`.
///
/// Distinguishes a record created by the current invocation from one that already existed,
/// so callers can roll back only effects they actually created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum InsertionOutcome {
    Inserted(RouterProvisioningRequest),
    Existing(RouterProvisioningRequest),
}

/// Failure modes for `RouterProvisioningRequestStore::clear_request`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClearError {
    /// No canonical record exists for the supplied key.
    NotFound,
}

/// Failure modes for `RouterProvisioningRequestStore::complete_request`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum CompletionError {
    /// No canonical record exists for the supplied key.
    NotFound(String),
    /// The record is not in a state that allows an ack commit or replay.
    InvalidState(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum PendingGraphLookupError {
    InconsistentDerivedState,
    InvalidEnvelope,
}

fn decode_and_validate_envelope(
    record: &RouterProvisioningRequest,
    deployment_id: &str,
) -> Result<ProvisionRequest, InsertError> {
    if record.resolved_request_bytes.len()
        > gleaph_message_sizing::MAX_SAFE_INTER_CANISTER_REQUEST_PAYLOAD_BYTES
        || record.provision_target == candid::Principal::anonymous()
    {
        return Err(InsertError::InvalidEnvelope);
    }
    let envelope = Decode!(&record.resolved_request_bytes, ProvisionRequest)
        .map_err(|_| InsertError::InvalidEnvelope)?;
    if envelope.request_id != record.request_id
        || envelope.deployment_id != deployment_id
        || envelope.intent_key.deployment_id != deployment_id
        || !envelope
            .requested_resources
            .iter()
            .any(|resource| resource.logical_resource == envelope.intent_key.logical_resource)
    {
        return Err(InsertError::InvalidEnvelope);
    }
    Ok(envelope)
}

fn immutable_identity_matches(
    existing: &RouterProvisioningRequest,
    candidate: &RouterProvisioningRequest,
) -> bool {
    existing.request_id == candidate.request_id
        && existing.caller == candidate.caller
        && existing.owner == candidate.owner
        && existing.admins == candidate.admins
        && existing.provision_target == candidate.provision_target
        && existing.resolved_request_bytes == candidate.resolved_request_bytes
}

/// Stateless facade over the Router provisioning-request catalog.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RouterProvisioningRequestStore;

impl RouterProvisioningRequestStore {
    pub(crate) const fn new() -> Self {
        Self
    }

    #[cfg(test)]
    pub(crate) fn map_lengths_for_test(&self) -> (u64, u64, u64) {
        let requests = ROUTER_PROVISIONING_REQUESTS.with_borrow(|map| map.len());
        let by_graph = ROUTER_PROVISIONING_BY_GRAPH.with_borrow(|map| map.len());
        let intent_locks = ROUTER_PROVISIONING_INTENT_LOCK.with_borrow(|map| map.len());
        (requests, by_graph, intent_locks)
    }

    /// Insert or idempotently return an existing request.
    ///
    /// Returns `InsertionOutcome::Inserted(record)` when this invocation created the record,
    /// and `InsertionOutcome::Existing(record)` when a matching record was already present.
    /// Callers that need to roll back on a later failure must use the ownership signal to undo
    /// only effects created by the current operation.
    ///
    /// All validation and conflict checks happen before the first stable mutation, so an error
    /// leaves no partial state.
    pub(crate) fn insert(
        &self,
        deployment_id: &str,
        req: RouterProvisioningRequest,
    ) -> Result<InsertionOutcome, InsertError> {
        let envelope = decode_and_validate_envelope(&req, deployment_id)?;

        // 1. Reject duplicate resource intents inside the same request.
        let mut seen = HashSet::new();
        for resource in &envelope.requested_resources {
            if !seen.insert(resource.logical_resource) {
                return Err(InsertError::InvalidDuplicateIntent);
            }
        }

        let request_key = ProvisioningRequestKey::new(&req.request_id, deployment_id);

        // 2. Same-key replay must preserve the complete immutable semantic identity.
        let existing = ROUTER_PROVISIONING_REQUESTS.with_borrow(|map| map.get(&request_key));
        if let Some(existing) = existing {
            if immutable_identity_matches(&existing, &req) {
                return Ok(InsertionOutcome::Existing(existing));
            }
            return Err(InsertError::IdentityConflict);
        }

        // 3. Preflight every derived intent lock.
        let intent_keys: Vec<ProvisioningIntentKey> = envelope
            .requested_resources
            .iter()
            .map(|r| ProvisioningIntentKey::new(deployment_id, r.logical_resource))
            .collect();
        let new_owner = IntentLockOwner::new(request_key.clone());
        let conflicting_lock = ROUTER_PROVISIONING_INTENT_LOCK.with_borrow(|locks| {
            intent_keys
                .iter()
                .find(|key| locks.get(key).is_some_and(|stored| stored != new_owner))
        });
        if conflicting_lock.is_some() {
            return Err(InsertError::IntentConflict);
        }

        // 4. Write canonical record, secondary index, and all intent locks synchronously.
        let graph_key =
            ProvisioningByGraphKey::new(deployment_id, &envelope.graph_name, &req.request_id);
        let lock_owner = IntentLockOwner::new(request_key.clone());
        ROUTER_PROVISIONING_REQUESTS.with_borrow_mut(|map| {
            map.insert(request_key.clone(), req.clone());
        });
        ROUTER_PROVISIONING_BY_GRAPH.with_borrow_mut(|map| {
            map.insert(graph_key, request_key.clone());
        });
        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow_mut(|locks| {
            for key in intent_keys {
                locks.insert(key, lock_owner.clone());
            }
        });

        Ok(InsertionOutcome::Inserted(req))
    }

    pub(crate) fn get_by_request_id(
        &self,
        key: &ProvisioningRequestKey,
    ) -> Option<RouterProvisioningRequest> {
        ROUTER_PROVISIONING_REQUESTS.with_borrow(|map| map.get(key))
    }

    /// List all requests for a given `(deployment_id, graph_name)` via the derived index.
    pub(crate) fn list_by_graph(
        &self,
        deployment_id: &str,
        graph_name: &str,
    ) -> Vec<RouterProvisioningRequest> {
        let start = ProvisioningByGraphKey::new(deployment_id, graph_name, &[0u8; 32]);
        let keys: Vec<ProvisioningRequestKey> = ROUTER_PROVISIONING_BY_GRAPH.with_borrow(|map| {
            map.range(start..)
                .take_while(|entry| {
                    entry.key().deployment_id == deployment_id
                        && entry.key().graph_name == graph_name
                })
                .map(|entry| entry.value())
                .collect()
        });
        ROUTER_PROVISIONING_REQUESTS
            .with_borrow(|map| keys.into_iter().filter_map(|k| map.get(&k)).collect())
    }

    /// Resolve an in-flight exact GraphShard(0) request through both derived Maps 46 and 47.
    /// Map 47 absence means no pending owner (Completed requests intentionally release it).
    pub(crate) fn pending_graph_bootstrap(
        &self,
        deployment_id: &str,
        graph_name: &str,
    ) -> Result<Option<RouterProvisioningRequest>, PendingGraphLookupError> {
        let intent_key = ProvisioningIntentKey::new(
            deployment_id,
            gleaph_graph_kernel::provisioning::LogicalResource::GraphShard(
                gleaph_graph_kernel::federation::ShardId::new(0),
            ),
        );
        let Some(owner) =
            ROUTER_PROVISIONING_INTENT_LOCK.with_borrow(|locks| locks.get(&intent_key))
        else {
            return Ok(None);
        };
        if owner.request_key.deployment_id != deployment_id {
            return Err(PendingGraphLookupError::InconsistentDerivedState);
        }

        let graph_key =
            ProvisioningByGraphKey::new(deployment_id, graph_name, &owner.request_key.request_id);
        let indexed = ROUTER_PROVISIONING_BY_GRAPH.with_borrow(|map| map.get(&graph_key));
        if indexed.as_ref() != Some(&owner.request_key) {
            return Err(PendingGraphLookupError::InconsistentDerivedState);
        }
        let record = ROUTER_PROVISIONING_REQUESTS
            .with_borrow(|map| map.get(&owner.request_key))
            .ok_or(PendingGraphLookupError::InconsistentDerivedState)?;
        let envelope = Decode!(&record.resolved_request_bytes, ProvisionRequest)
            .map_err(|_| PendingGraphLookupError::InvalidEnvelope)?;
        let exact_shape = envelope.graph_name == graph_name
            && envelope.deployment_id == deployment_id
            && envelope.requested_resources.len() == 1
            && envelope.install_args.len() == 1
            && matches!(
                envelope.requested_resources[0].logical_resource,
                gleaph_graph_kernel::provisioning::LogicalResource::GraphShard(shard)
                    if shard == gleaph_graph_kernel::federation::ShardId::new(0)
            );
        if !exact_shape || record.state != RouterProvisioningRequestState::AwaitingAck {
            return Err(PendingGraphLookupError::InconsistentDerivedState);
        }
        Ok(Some(record))
    }

    pub(crate) fn intent_locked(
        &self,
        key: &ProvisioningIntentKey,
        owner: &IntentLockOwner,
    ) -> bool {
        ROUTER_PROVISIONING_INTENT_LOCK
            .with_borrow(|locks| locks.get(key).is_some_and(|stored| stored == owner.clone()))
    }

    /// Clears the canonical record, graph index, and every intent lock derived from the stored
    /// request. Returns `Err(ClearError::NotFound)` if the request key is not present in the
    /// canonical store.
    pub(crate) fn clear_request(
        &self,
        request_key: &ProvisioningRequestKey,
    ) -> Result<(), ClearError> {
        let maybe_record = ROUTER_PROVISIONING_REQUESTS.with_borrow(|map| map.get(request_key));
        let Some(record) = maybe_record else {
            return Err(ClearError::NotFound);
        };
        let envelope = Decode!(&record.resolved_request_bytes, ProvisionRequest)
            .expect("stored Map 45 envelope must decode");

        let deployment_id = request_key.deployment_id.clone();
        let graph_key =
            ProvisioningByGraphKey::new(&deployment_id, &envelope.graph_name, &record.request_id);

        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow_mut(|locks| {
            for resource in &envelope.requested_resources {
                let key = ProvisioningIntentKey::new(&deployment_id, resource.logical_resource);
                locks.remove(&key);
            }
        });
        ROUTER_PROVISIONING_BY_GRAPH.with_borrow_mut(|map| {
            map.remove(&graph_key);
        });
        ROUTER_PROVISIONING_REQUESTS.with_borrow_mut(|map| {
            map.remove(request_key);
        });

        Ok(())
    }

    /// Invocation-owned rollback used when `provision_graph`'s outbound
    /// `send_accept_envelope` fails.
    ///
    /// Removes the record and its intent locks **only** if the current operation can prove it
    /// created the record (`InsertionOutcome::Inserted`) AND the record is still in
    /// `AwaitingAck` state. Pre-existing records from any prior invocation — whether
    /// `AwaitingAck`, `Completed`, or any other state — are preserved, preventing a retry with
    /// a transient send failure from deleting durable state owned by an earlier call.
    pub(crate) fn rollback_if_inserted_and_awaiting(
        &self,
        request_key: &ProvisioningRequestKey,
        outcome: &InsertionOutcome,
    ) {
        let InsertionOutcome::Inserted(_) = outcome else {
            return;
        };
        if let Some(record) = self.get_by_request_id(request_key)
            && record.state == RouterProvisioningRequestState::AwaitingAck
        {
            let _ = self.clear_request(request_key);
        }
    }

    /// Release every intent lock that is owned by the supplied request.
    ///
    /// Only removes locks whose stored owner matches the record's owner identity. Locks held by
    /// another request are left untouched. Used by `complete_request` after advancing a record to
    /// terminal `Completed` state so the same resource can be re-provisioned later (symmetric
    /// with the Provision-side `clear_intent_locks_for_record`).
    pub(crate) fn release_intent_locks_owned_by(
        &self,
        deployment_id: &str,
        record: &RouterProvisioningRequest,
    ) {
        let envelope = Decode!(&record.resolved_request_bytes, ProvisionRequest)
            .expect("stored Map 45 envelope must decode");
        let expected_owner = IntentLockOwner::new(ProvisioningRequestKey::new(
            &record.request_id,
            deployment_id,
        ));
        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow_mut(|locks| {
            for resource in &envelope.requested_resources {
                let key = ProvisioningIntentKey::new(deployment_id, resource.logical_resource);
                if locks
                    .get(&key)
                    .is_some_and(|stored| stored == expected_owner)
                {
                    locks.remove(&key);
                }
            }
        });
    }

    /// Complete the Router orchestration record after Provision applied or replayed the
    /// registration ACK. All lock-owner checks precede the first mutation.
    pub(crate) fn complete_request(
        &self,
        key: &ProvisioningRequestKey,
    ) -> Result<RouterProvisioningRequest, CompletionError> {
        let maybe_record = ROUTER_PROVISIONING_REQUESTS.with_borrow(|map| map.get(key));
        let Some(record) = maybe_record else {
            return Err(CompletionError::NotFound(format!(
                "no provisioning request for {}/{:02x?}",
                key.deployment_id, key.request_id
            )));
        };

        if record.state == RouterProvisioningRequestState::Completed {
            return Ok(record);
        }

        if record.state != RouterProvisioningRequestState::AwaitingAck {
            return Err(CompletionError::InvalidState(format!(
                "expected AwaitingAck, got {:?}",
                record.state
            )));
        }

        let envelope = Decode!(&record.resolved_request_bytes, ProvisionRequest)
            .map_err(|_| CompletionError::InvalidState("invalid stored envelope".to_owned()))?;
        let intent_keys: Vec<ProvisioningIntentKey> = envelope
            .requested_resources
            .iter()
            .map(|r| ProvisioningIntentKey::new(&key.deployment_id, r.logical_resource))
            .collect();
        let expected_owner = IntentLockOwner::new(key.clone());
        let all_owned = ROUTER_PROVISIONING_INTENT_LOCK.with_borrow(|locks| {
            intent_keys.iter().all(|k| {
                locks
                    .get(k)
                    .is_some_and(|stored| stored == expected_owner.clone())
            })
        });
        if !all_owned {
            return Err(CompletionError::InvalidState(
                "AwaitingAck record missing or not owning intent locks".to_owned(),
            ));
        }

        let mut updated = record.clone();
        updated.state = RouterProvisioningRequestState::Completed;
        ROUTER_PROVISIONING_REQUESTS.with_borrow_mut(|map| {
            map.insert(key.clone(), updated.clone());
        });
        self.release_intent_locks_owned_by(&key.deployment_id, &updated);

        Ok(updated)
    }
}
