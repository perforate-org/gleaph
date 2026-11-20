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

/// One canister recorded for a deployment, with whether an active job still references it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DeploymentCanister {
    /// The canister id.
    pub canister: Principal,
    /// True when at least one non-Failed job for the deployment records this canister.
    pub active: bool,
}

/// Error returned by `upsert_deployment_grant`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum UpsertDeploymentGrantError {
    /// Bootstrap authority has not been seeded yet.
    NoBootstrapAuthority,
    /// Caller is not the governance authority (or the issuer is anonymous).
    Unauthorized,
}

/// One row of the deployment job list. This is an operator view, not the full job record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DeploymentJobSummary {
    /// Canonical request id.
    pub request_id: [u8; 32],
    /// Current state name.
    pub state_name: String,
    /// Index into `resources` currently being processed.
    pub active_resource_index: u32,
    /// Number of completed management-canister effects.
    pub completed_effect_count: u32,
    /// Resource count for this job.
    pub resource_count: u32,
    /// Graph name requested by the job.
    pub graph_name: String,
    /// Reserved graph id, if the job reserved one.
    pub reserved_graph_id: Option<u32>,
    /// Creation timestamp (IC NNS nanoseconds).
    pub created_at_ns: u64,
    /// Last transition timestamp (IC NNS nanoseconds).
    pub last_transition_ns: u64,
}

/// Errors from an explicit Managed -> SelfCustody controller handover.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum TakeOwnershipError {
    Unauthorized,
    UnknownDeployment,
    AlreadySelfCustody,
    ManagementCallFailed,
    RollbackFailed { canisters: Vec<Principal> },
    InvalidState,
}

/// Errors from a governance repair of a Managed deployment's owner.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum DeploymentOwnerUpdateError {
    Unauthorized,
    UnknownDeployment,
    AnonymousOwner,
    SelfCustody,
    InvalidState,
}

/// Errors from a governance transfer of a SelfCustody deployment's owner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum SelfCustodyTransferError {
    Unauthorized,
    UnknownDeployment,
    AnonymousOwner,
    Managed,
    SameOwner,
    ManagementCallFailed,
    RollbackFailed { canisters: Vec<Principal> },
    InvalidState,
}

/// Read view of one applied billing grant.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct BillingGrantView {
    pub authority: Principal,
    pub idempotency_key: [u8; 32],
    pub deployment_id: gleaph_graph_kernel::provisioning::DeploymentId,
    pub amount: u128,
    pub credited_at_ns: u64,
    pub reversed_at_ns: Option<u64>,
}

/// Read view of one provisioning job, exposed to the deployment read authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ProvisionJobView {
    pub request_id: [u8; 32],
    pub deployment_id: gleaph_graph_kernel::provisioning::DeploymentId,
    pub reserved_graph_id: Option<gleaph_graph_kernel::entry::GraphId>,
    pub graph_name: String,
    pub state_name: String,
    pub active_resource_index: u32,
    pub completed_effect_count: u32,
    pub resources: Vec<ResourceJobView>,
    pub is_authorized_caller: bool,
}

/// One resource recorded by a provisioning job.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct ResourceJobView {
    pub logical_resource: gleaph_graph_kernel::provisioning::LogicalResource,
    pub canister_id: Option<Principal>,
    pub artifact_hash: Option<[u8; 32]>,
}

/// Why a deployment was suspended. Free text is retained verbatim in the audit record.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct DeploymentTermination {
    pub deployment_id: gleaph_graph_kernel::provisioning::DeploymentId,
    pub initiator: Principal,
    pub reason: String,
    pub state: DeploymentTerminationState,
    pub requested_at_ns: u64,
    pub grace_until_ns: u64,
    pub cancelled_at_ns: Option<u64>,
    pub deleted_at_ns: Option<u64>,
    /// Set when the grace sweep marked this record; deletion still needs an explicit call.
    pub grace_expired_at_ns: Option<u64>,
    /// Every canister the suspension stopped (or will stop).
    pub canisters: Vec<Principal>,
    /// Remaining stop list while `SuspensionPending`.
    pub pending_canisters: Vec<Principal>,
}

/// Phase 1 termination state. Deletion is deliberately absent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum DeploymentTerminationState {
    /// Persisted before the first stop call; `pending_canisters` is the retry list.
    SuspensionPending,
    /// Every recorded canister is stopped; data is retained.
    Suspended,
    /// Suspension was cancelled and the canisters were started again.
    Cancelled,
    /// The grace period expired and the sweep recorded it; deletion is still explicit.
    GraceExpired,
    /// Deletion is in progress; `pending_canisters` is the retry list.
    Deleting,
    /// Every recorded canister was deleted; the record remains as audit history.
    Deleted,
}

/// Errors from suspension/cancellation.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum DeploymentTerminationError {
    Unauthorized,
    UnknownDeployment,
    /// A canonical job is not terminal; finish or repair it first.
    ActiveJob,
    AlreadySuspended,
    NotSuspended,
    /// The 30-day grace period has not expired and the caller is not governance-forcing.
    GraceNotExpired,
    /// A positive deployment credit remains; settle it before deleting.
    CreditNotSettled,
    /// The confirmation principal does not match the deployment.
    ConfirmationMismatch,
    /// Deletion already completed.
    AlreadyDeleted,
    /// One or more management calls failed; retry with the same request.
    ManagementCallFailed {
        canisters: Vec<Principal>,
    },
}

/// Outcome of deleting one failed-job canister during repair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum FailedCanisterRepair {
    /// The canister was stopped and deleted.
    Deleted,
    /// The canister no longer exists, so nothing was deleted.
    AlreadyAbsent,
    /// The canister became active again between the scan and the delete.
    SkippedActive,
    /// Stop/delete failed; the reason is retained for the operator.
    Failed { reason: String },
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

    impl Storable for super::DeploymentTermination {
        const BOUND: Bound = Bound::Unbounded;

        fn to_bytes(&self) -> Cow<'_, [u8]> {
            Cow::Owned(Encode!(self).expect("encode DeploymentTermination"))
        }

        fn into_bytes(self) -> Vec<u8> {
            Encode!(&self).expect("encode DeploymentTermination")
        }

        fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
            Decode!(bytes.as_ref(), Self).expect("decode DeploymentTermination")
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
