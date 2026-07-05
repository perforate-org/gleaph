//! Router provisioning-request catalog store (ADR 0035 Slice 1).
//!
//! Owns three stable-memory regions:
//! - `ROUTER_PROVISIONING_REQUESTS`: canonical `(request_id, deployment_id) → RouterProvisioningRequest`
//! - `ROUTER_PROVISIONING_BY_GRAPH`: derived `(deployment_id, graph_name, request_id) → ProvisioningRequestKey`
//! - `ROUTER_PROVISIONING_INTENT_LOCK`: canonical `(deployment_id, resource_kind, logical_resource_key) → IntentLockMarker`

// These pub(crate) items are exercised in unit tests and will be reached by Router ingress and
// callback paths in later slices; allow dead_code while they remain crate-internal in Slice 1.
#![allow(dead_code)]

use std::collections::HashSet;

use crate::facade::stable::{
    ROUTER_PROVISIONING_BY_GRAPH, ROUTER_PROVISIONING_INTENT_LOCK, ROUTER_PROVISIONING_REQUESTS,
};
use crate::types::{
    IntentLockMarker, ProvisioningByGraphKey, ProvisioningIntentKey, ProvisioningRequestKey,
    RouterProvisioningRequest,
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

/// Failure modes for `RouterProvisioningRequestStore::clear_request`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ClearError {
    /// No canonical record exists for the supplied key.
    NotFound,
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
    /// All validation and conflict checks happen before the first stable mutation, so an error
    /// leaves no partial state.
    pub(crate) fn insert(
        &self,
        deployment_id: &str,
        req: RouterProvisioningRequest,
    ) -> Result<RouterProvisioningRequest, InsertError> {
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
                return Ok(existing);
            }
            return Err(InsertError::Conflict);
        }

        // 3. Preflight every derived intent lock.
        let intent_keys: Vec<ProvisioningIntentKey> = req
            .requested_resources
            .iter()
            .map(|r| ProvisioningIntentKey::new(deployment_id, r.kind, &r.logical_resource_key))
            .collect();
        let any_locked = ROUTER_PROVISIONING_INTENT_LOCK
            .with_borrow(|locks| intent_keys.iter().any(|key| locks.contains_key(key)));
        if any_locked {
            return Err(InsertError::IntentConflict);
        }

        // 4. Write canonical record, secondary index, and all intent locks synchronously.
        let graph_key =
            ProvisioningByGraphKey::new(deployment_id, &req.graph_name, &req.request_id);
        ROUTER_PROVISIONING_REQUESTS.with_borrow_mut(|map| {
            map.insert(request_key.clone(), req.clone());
        });
        ROUTER_PROVISIONING_BY_GRAPH.with_borrow_mut(|map| {
            map.insert(graph_key, request_key.clone());
        });
        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow_mut(|locks| {
            for key in intent_keys {
                locks.insert(key, IntentLockMarker);
            }
        });

        Ok(req)
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

    pub(crate) fn intent_locked(&self, key: &ProvisioningIntentKey) -> bool {
        ROUTER_PROVISIONING_INTENT_LOCK.with_borrow(|locks| locks.contains_key(key))
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
}
