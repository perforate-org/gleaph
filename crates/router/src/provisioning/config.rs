// Runtime provision-canister binding for the Router canister (ADR 0035 Slice 5).
//
// The binding is heap-only: it is set once at `init` and must be re-seeded in
// `post_upgrade` from the durable `ROUTER_PROVISION_CONFIG` stable region.

use candid::{CandidType, Decode, Encode, Principal};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::cell::RefCell;

/// Durable bootstrap config stored in `ROUTER_PROVISION_CONFIG` (MemoryId 48).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub(crate) struct ProvisionRuntimeConfig {
    pub provision_canister: Option<Principal>,
}

impl Storable for ProvisionRuntimeConfig {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&ProvisionRuntimeConfigStableRecord::V1(self.clone()))
                .expect("encode ProvisionRuntimeConfig"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&ProvisionRuntimeConfigStableRecord::V1(self))
            .expect("encode ProvisionRuntimeConfig")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), ProvisionRuntimeConfigStableRecord)
            .expect("decode ProvisionRuntimeConfig")
        {
            ProvisionRuntimeConfigStableRecord::V1(v1) => v1,
        }
    }
}

#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
pub(crate) enum ProvisionRuntimeConfigStableRecord {
    V1(ProvisionRuntimeConfig),
}

thread_local! {
    static PROVISION_CANISTER: RefCell<Option<Principal>> = const { RefCell::new(None) };
}

/// Runtime read of the configured provision canister. Returns `None` until `init` or
/// `post_upgrade` has seeded the binding.
pub fn get() -> Option<Principal> {
    PROVISION_CANISTER.with_borrow(|cell| *cell)
}

/// Set the provision-canister binding. Called only from `init` / `post_upgrade`.
pub fn set(principal: Option<Principal>) {
    PROVISION_CANISTER.with_borrow_mut(|cell| {
        *cell = principal;
    });
}
