//! Router provisioning-request catalog store (ADR 0035 Slice 1).
//!
//! Owns three stable-memory regions:
//! - `ROUTER_PROVISIONING_REQUESTS`: canonical `(request_id, deployment_id) → RouterProvisioningRequest`
//! - `ROUTER_PROVISIONING_BY_GRAPH`: derived `(deployment_id, graph_name, request_id) → ProvisioningRequestKey`
//! - `ROUTER_PROVISIONING_INTENT_LOCK`: canonical `(deployment_id, resource_kind, logical_resource_key) → IntentLockOwner`

// These pub(crate) items are exercised in unit tests and will be reached by Router ingress and
// callback paths in later slices; allow dead_code while they remain crate-internal in Slice 1.
#![allow(dead_code)]

use std::collections::HashSet;

use crate::facade::stable::{
    ROUTER_PROVISIONING_BY_GRAPH, ROUTER_PROVISIONING_INTENT_LOCK, ROUTER_PROVISIONING_REQUESTS,
};
use crate::types::{
    IntentLockOwner, ProvisioningByGraphKey, ProvisioningIntentKey, ProvisioningRequestKey,
    RouterProvisioningRequest, RouterProvisioningRequestState,
};

/// Failure modes for `RouterProvisioningRequestStore::insert`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum InsertError {
    /// Same `request_id` with a different `request_fingerprint`.
    Conflict,
    /// At least one requested intent is already locked by another non-terminal request.
    IntentConflict,
    /// The request contains duplicate `(kind, logical_resource_key)` resources.
    InvalidDuplicateIntent,
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

/// Failure modes for `RouterProvisioningRequestStore::commit_ack`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum AckCommitError {
    /// No canonical record exists for the supplied key.
    NotFound(String),
    /// The record is not in a state that allows an ack commit or replay.
    InvalidState(String),
    /// The record is already `Completed` with a different registry version.
    Conflict { stored: u64 },
}

/// Stateless facade over the Router provisioning-request catalog.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RouterProvisioningRequestStore;

impl RouterProvisioningRequestStore {
    pub(crate) const fn new() -> Self {
        Self
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
        // 1. Reject duplicate resource intents inside the same request.
        let mut seen = HashSet::new();
        for resource in &req.requested_resources {
            if !seen.insert((resource.kind, resource.logical_resource_key.clone())) {
                return Err(InsertError::InvalidDuplicateIntent);
            }
        }

        let request_key = ProvisioningRequestKey::new(&req.request_id, deployment_id);

        // 2. Idempotency / conflict check on the canonical record.
        let existing = ROUTER_PROVISIONING_REQUESTS.with_borrow(|map| map.get(&request_key));
        if let Some(existing) = existing {
            if existing.request_fingerprint == req.request_fingerprint {
                return Ok(InsertionOutcome::Existing(existing));
            }
            return Err(InsertError::Conflict);
        }

        // 3. Preflight every derived intent lock.
        let intent_keys: Vec<ProvisioningIntentKey> = req
            .requested_resources
            .iter()
            .map(|r| ProvisioningIntentKey::new(deployment_id, r.kind, &r.logical_resource_key))
            .collect();
        let new_owner = IntentLockOwner::new(request_key.clone(), req.request_fingerprint.clone());
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
            ProvisioningByGraphKey::new(deployment_id, &req.graph_name, &req.request_id);
        let lock_owner = IntentLockOwner::new(request_key.clone(), req.request_fingerprint.clone());
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
        let start = ProvisioningByGraphKey::new(deployment_id, graph_name, "");
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

        let deployment_id = request_key.deployment_id.clone();
        let graph_key =
            ProvisioningByGraphKey::new(&deployment_id, &record.graph_name, &record.request_id);

        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow_mut(|locks| {
            for resource in &record.requested_resources {
                let key = ProvisioningIntentKey::new(
                    &deployment_id,
                    resource.kind,
                    &resource.logical_resource_key,
                );
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
    /// another request are left untouched. Used by `commit_ack` after advancing a record to
    /// terminal `Completed` state so the same resource can be re-provisioned later (symmetric
    /// with the Provision-side `clear_intent_locks_for_record`).
    pub(crate) fn release_intent_locks_owned_by(
        &self,
        deployment_id: &str,
        record: &RouterProvisioningRequest,
    ) {
        let expected_owner = IntentLockOwner::new(
            ProvisioningRequestKey::new(&record.request_id, deployment_id),
            record.request_fingerprint.clone(),
        );
        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow_mut(|locks| {
            for resource in &record.requested_resources {
                let key = ProvisioningIntentKey::new(
                    deployment_id,
                    resource.kind,
                    &resource.logical_resource_key,
                );
                if locks
                    .get(&key)
                    .is_some_and(|stored| stored == expected_owner)
                {
                    locks.remove(&key);
                }
            }
        });
    }

    /// Commit the Router-side ack and advance the provisioning request to terminal `Completed`.
    ///
    /// Performs the state machine atomically with respect to the caller-visible `Result`:
    /// 1. Read the canonical record and every intent-lock owner.
    /// 2. Validate state (`Completed` replay/conflict, `AwaitingAck` preflight).
    /// 3. Build the updated `Completed` record with `accepted_registry_version`.
    /// 4. Apply all mutations (record update + owner-scoped lock release) in order.
    /// 5. Return `Ok` only after **all** mutations succeed.
    ///
    /// No mutation may be followed by a fallible operation that returns `Err`. If any step
    /// fails before mutations, this function returns `Err` without writing. If a mutation
    /// itself could fail, the function must trap, because the IC does not roll back a regular
    /// `Result::Err`.
    ///
    /// State machine:
    /// - `Completed` + matching version  -> Ok(record) (idempotent replay)
    /// - `Completed` + differing version -> Err(AckCommitError::Conflict { stored })
    /// - `Completed` + no version        -> Err(AckCommitError::InvalidState)
    /// - `AwaitingAck` + all locks owned -> write Completed + version, release locks, Ok(record)
    /// - `AwaitingAck` + missing/wrong-owner locks -> Err(AckCommitError::InvalidState)
    /// - any other state                 -> Err(AckCommitError::InvalidState)
    pub(crate) fn commit_ack(
        &self,
        key: &ProvisioningRequestKey,
        accepted_registry_version: u64,
    ) -> Result<RouterProvisioningRequest, AckCommitError> {
        let maybe_record = ROUTER_PROVISIONING_REQUESTS.with_borrow(|map| map.get(key));
        let Some(record) = maybe_record else {
            return Err(AckCommitError::NotFound(format!(
                "no provisioning request for {}/{}",
                key.deployment_id, key.request_id
            )));
        };

        // Replay / conflict branches for already-Completed records.
        if record.state == RouterProvisioningRequestState::Completed {
            match record.accepted_registry_version {
                Some(stored) if stored == accepted_registry_version => return Ok(record),
                Some(stored) => {
                    return Err(AckCommitError::Conflict { stored });
                }
                None => {
                    return Err(AckCommitError::InvalidState(
                        "completed record missing accepted_registry_version".to_owned(),
                    ));
                }
            }
        }

        if record.state != RouterProvisioningRequestState::AwaitingAck {
            return Err(AckCommitError::InvalidState(format!(
                "expected AwaitingAck, got {:?}",
                record.state
            )));
        }

        // Preflight: every intent lock derived from this record must still be held AND owned
        // by this record.
        let intent_keys: Vec<ProvisioningIntentKey> = record
            .requested_resources
            .iter()
            .map(|r| {
                ProvisioningIntentKey::new(&key.deployment_id, r.kind, &r.logical_resource_key)
            })
            .collect();
        let expected_owner = IntentLockOwner::new(key.clone(), record.request_fingerprint.clone());
        let all_owned = ROUTER_PROVISIONING_INTENT_LOCK.with_borrow(|locks| {
            intent_keys.iter().all(|k| {
                locks
                    .get(k)
                    .is_some_and(|stored| stored == expected_owner.clone())
            })
        });
        if !all_owned {
            return Err(AckCommitError::InvalidState(
                "AwaitingAck record missing or not owning intent locks".to_owned(),
            ));
        }

        // Atomic write: update the canonical record to Completed + accepted version, then
        // release the now-unnecessary intent locks. Both happen in the same message execution.
        let mut updated = record.clone();
        updated.state = RouterProvisioningRequestState::Completed;
        updated.accepted_registry_version = Some(accepted_registry_version);
        ROUTER_PROVISIONING_REQUESTS.with_borrow_mut(|map| {
            map.insert(key.clone(), updated.clone());
        });
        self.release_intent_locks_owned_by(&key.deployment_id, &updated);

        Ok(updated)
    }
}
