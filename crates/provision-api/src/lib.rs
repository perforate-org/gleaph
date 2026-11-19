//! Shared Provision control-plane wire types.
//!
//! These types are returned by `upsert_deployment_grant` and `artifact_audit_history`-adjacent
//! bootstrap authority surfaces. Provision, the ingress client, and the CLI must encode the
//! same field and variant names; defining them once removes the three hand-maintained mirrors.

use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};

/// What an issuer may name as the deployment in a `ProvisionRequest`.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum DeploymentGrantScope {
    /// The issuer is the deployment; `deployment_id` must equal the caller's principal text.
    #[default]
    OwnDeployment = 0,
    /// A trusted relay (Account) may name the account it issues for.
    Relay = 1,
}

/// Action recorded for every bootstrap-authority decision.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum BootstrapAuthAction {
    /// First authority seed at init.
    InitialSeed,
    /// A successful grant upsert.
    Upsert,
    /// A rejected upsert before the authority was seeded.
    RejectNotSeeded,
    /// A rejected upsert by a non-governance caller (or anonymous issuer).
    RejectUnauthorized,
}

/// One durable audit row in `PROVISION_BOOTSTRAP_AUDIT_LOG` (MemoryId 5).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct BootstrapAuthEntry {
    /// Caller that produced the decision.
    pub caller: Principal,
    /// Related deployment (= issuer principal text), if any.
    pub deployment_id: Option<String>,
    /// Recorded action.
    pub action: BootstrapAuthAction,
    /// Timestamp (IC NNS nanoseconds).
    pub timestamp_ns: u64,
}

/// Arguments for `upsert_deployment_grant`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct UpsertDeploymentGrantArgs {
    /// Whether the issuer may name a deployment other than itself.
    pub scope: DeploymentGrantScope,
    /// The principal authorized to request issuance.
    pub issuer: Principal,
}

/// Error returned by `upsert_deployment_grant`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum UpsertDeploymentGrantError {
    /// Bootstrap authority has not been seeded yet.
    NoBootstrapAuthority,
    /// Caller is not the governance authority (or the issuer is anonymous).
    Unauthorized,
}

#[cfg(feature = "stable")]
mod stable {
    use super::BootstrapAuthEntry;
    use candid::{Decode, Encode};
    use ic_stable_structures::storable::{Bound, Storable};
    use std::borrow::Cow;

    impl Storable for super::DeploymentGrantScope {
        const BOUND: Bound = Bound::Bounded {
            max_size: 1,
            is_fixed_size: true,
        };

        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Owned(vec![*self as u8])
        }

        fn into_bytes(self) -> Vec<u8> {
            vec![self as u8]
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            match bytes.first() {
                Some(0) => Self::OwnDeployment,
                Some(1) => Self::Relay,
                other => panic!("unknown DeploymentGrantScope tag: {other:?}"),
            }
        }
    }

    impl Storable for BootstrapAuthEntry {
        const BOUND: Bound = Bound::Unbounded;

        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Owned(Encode!(self).expect("encode BootstrapAuthEntry"))
        }

        fn into_bytes(self) -> Vec<u8> {
            Encode!(&self).expect("encode BootstrapAuthEntry")
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            Decode!(bytes.as_ref(), Self).expect("decode BootstrapAuthEntry")
        }
    }
}
