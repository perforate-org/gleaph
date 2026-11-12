//! Router deployment-wide runtime configuration (MemoryId 48).
//!
//! One durable singleton holds the bindings that belong to the Router deployment rather than to a
//! graph: the Provision canister (ADR 0035) and the inference canister (ADR 0111). The record is
//! the SSOT; a heap mirror is seeded by `init`/`post_upgrade` and updated by the admin setters.

use candid::{CandidType, Decode, Encode, Principal};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;

use crate::facade::stable::ROUTER_RUNTIME_CONFIG;

/// Durable Router runtime config stored in `ROUTER_RUNTIME_CONFIG` (MemoryId 48).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize, Default)]
pub(crate) struct RouterRuntimeConfig {
    /// Provision canister that issues this deployment's graph/index/inference canisters.
    pub provision_canister: Option<Principal>,
    /// Inference canister serving model routing for enabled graphs. One per Router deployment.
    pub inference_canister: Option<Principal>,
}

impl Storable for RouterRuntimeConfig {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&RouterRuntimeConfigStableRecord::V1(self.clone()))
                .expect("encode RouterRuntimeConfig"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&RouterRuntimeConfigStableRecord::V1(self)).expect("encode RouterRuntimeConfig")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), RouterRuntimeConfigStableRecord)
            .expect("decode RouterRuntimeConfig")
        {
            RouterRuntimeConfigStableRecord::V1(v1) => v1,
        }
    }
}

#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
pub(crate) enum RouterRuntimeConfigStableRecord {
    V1(RouterRuntimeConfig),
}

thread_local! {
    static ROUTER_RUNTIME_CONFIG_HEAP: RefCell<Option<RouterRuntimeConfig>> =
        const { RefCell::new(None) };
}

/// Runtime read of the deployment configuration. `None` until `init`/`post_upgrade` seeds it.
pub(crate) fn get() -> Option<RouterRuntimeConfig> {
    ROUTER_RUNTIME_CONFIG_HEAP.with_borrow(|cell| cell.clone())
}

/// The configured Provision canister, if any.
pub(crate) fn provision_canister() -> Option<Principal> {
    get().and_then(|config| config.provision_canister)
}

/// The configured inference canister, if any.
pub(crate) fn inference_canister() -> Option<Principal> {
    get().and_then(|config| config.inference_canister)
}

/// Seed the heap mirror. Called from `init` / `post_upgrade` only.
pub(crate) fn set(config: &RouterRuntimeConfig) {
    ROUTER_RUNTIME_CONFIG_HEAP.with_borrow_mut(|cell| {
        *cell = Some(config.clone());
    });
}

/// Read-modify-write the durable record and refresh the heap mirror.
pub(crate) fn update(change: impl FnOnce(&mut RouterRuntimeConfig)) {
    let mut config = load();
    change(&mut config);
    save(&config);
    set(&config);
}

pub(crate) fn save(config: &RouterRuntimeConfig) {
    ROUTER_RUNTIME_CONFIG.with_borrow_mut(|cell| {
        cell.set(config.clone());
    });
}

pub(crate) fn load() -> RouterRuntimeConfig {
    ROUTER_RUNTIME_CONFIG.with_borrow(|cell| cell.get().clone())
}
