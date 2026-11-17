//! Shared provisioning protocol types used by both Router and Provision canisters.
//!
//! These types are intentionally owned by `gleaph-graph-kernel` — a neutral shared crate —
//! rather than by either canister's implementation, so cross-canister stable-memory and wire
//! encodings stay identical without forcing one canister to depend on the other's implementation.

use candid::{CandidType, Principal};
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use crate::federation::{IndexClusterId, ShardId, TextIndexId, VectorIndexId};

/// A provisionable resource within a deployment. The enum variant is the discriminator (it
/// doubles as the resource kind); the inner type is a shared newtype so the stable encoding is
/// fixed-length.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub enum LogicalResource {
    GraphShard(ShardId),
    PropertyIndex(IndexClusterId),
    VectorIndex(VectorIndexId),
    /// The deployment's Router canister. A singleton per deployment (issued once during the
    /// bootstrap handover by the Account as trust subject); no payload id.
    Router,
    TextIndex(TextIndexId),
    /// The deployment's inference canister. A singleton per Router deployment, provisioned
    /// lazily when model routing is enabled; no payload id.
    Inference,
    // Future: Procedure(...)
}

/// Controller policy for a provisioned deployment. Omitted from the request means `SelfCustody`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum ControllerMode {
    /// The platform operator manages the canisters; the customer owns the data.
    Managed,
    /// The deployment owner manages the canisters.
    #[default]
    SelfCustody,
}

/// Stable-byte tag for [`LogicalResource`]. One table serves encode and decode.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LogicalResourceTag {
    GraphShard = 0,
    PropertyIndex = 1,
    VectorIndex = 2,
    Router = 3,
    TextIndex = 4,
    Inference = 5,
}

impl LogicalResourceTag {
    pub const fn as_u8(self) -> u8 {
        self as u8
    }

    pub const fn from_u8(tag: u8) -> Option<Self> {
        Some(match tag {
            0 => Self::GraphShard,
            1 => Self::PropertyIndex,
            2 => Self::VectorIndex,
            3 => Self::Router,
            4 => Self::TextIndex,
            5 => Self::Inference,
            _ => return None,
        })
    }
}

impl LogicalResource {
    pub const fn tag(self) -> LogicalResourceTag {
        match self {
            Self::GraphShard(_) => LogicalResourceTag::GraphShard,
            Self::PropertyIndex(_) => LogicalResourceTag::PropertyIndex,
            Self::VectorIndex(_) => LogicalResourceTag::VectorIndex,
            Self::Router => LogicalResourceTag::Router,
            Self::TextIndex(_) => LogicalResourceTag::TextIndex,
            Self::Inference => LogicalResourceTag::Inference,
        }
    }
}

impl Storable for LogicalResource {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: 5, // 1 variant tag + 4 bytes (ShardId or IndexClusterId)
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let val = *self;
        Cow::Owned(val.into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(5);
        out.push(self.tag().as_u8());
        match self {
            LogicalResource::GraphShard(shard) => out.extend_from_slice(&shard.to_le_bytes()),
            LogicalResource::PropertyIndex(cluster) => {
                out.extend_from_slice(&cluster.to_le_bytes())
            }
            LogicalResource::VectorIndex(vector) => out.extend_from_slice(&vector.to_le_bytes()),
            LogicalResource::Router | LogicalResource::Inference => {
                out.extend_from_slice(&[0u8; 4])
            }
            LogicalResource::TextIndex(text) => out.extend_from_slice(&text.to_le_bytes()),
        }
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&bytes[1..5]);
        match LogicalResourceTag::from_u8(bytes[0])
            .unwrap_or_else(|| panic!("unknown LogicalResource variant {}", bytes[0]))
        {
            LogicalResourceTag::GraphShard => {
                LogicalResource::GraphShard(ShardId::from_le_bytes(raw))
            }
            LogicalResourceTag::PropertyIndex => {
                LogicalResource::PropertyIndex(IndexClusterId::from_le_bytes(raw))
            }
            LogicalResourceTag::VectorIndex => {
                LogicalResource::VectorIndex(VectorIndexId::from_le_bytes(raw))
            }
            LogicalResourceTag::Router => LogicalResource::Router,
            LogicalResourceTag::TextIndex => {
                LogicalResource::TextIndex(TextIndexId::from_le_bytes(raw))
            }
            LogicalResourceTag::Inference => LogicalResource::Inference,
        }
    }
}

/// Canonical deployment identity. Every deployment is an Account: a Personal account principal or
/// an Org-generated account id. The type keeps a principal's bytes and canonical text from being
/// confused with an arbitrary string, and fixes the stable-key width.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct DeploymentId(Principal);

impl DeploymentId {
    pub const fn new(principal: Principal) -> Self {
        Self(principal)
    }

    pub const fn as_principal(self) -> Principal {
        self.0
    }

    pub fn to_text(self) -> String {
        self.0.to_text()
    }

    pub fn parse(text: &str) -> Result<Self, String> {
        Principal::from_text(text)
            .map(Self)
            .map_err(|error| format!("invalid deployment id: {error}"))
    }
}

impl From<Principal> for DeploymentId {
    fn from(principal: Principal) -> Self {
        Self(principal)
    }
}

impl From<DeploymentId> for Principal {
    fn from(id: DeploymentId) -> Self {
        id.0
    }
}

impl std::fmt::Display for DeploymentId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl std::str::FromStr for DeploymentId {
    type Err = String;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Self::parse(text)
    }
}

impl Storable for DeploymentId {
    const BOUND: StorableBound = StorableBound::Bounded {
        max_size: 30,
        is_fixed_size: false,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let bytes = self.0.as_slice();
        let mut out = Vec::with_capacity(1 + bytes.len());
        out.push(bytes.len() as u8);
        out.extend_from_slice(bytes);
        Cow::Owned(out)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let len = bytes[0] as usize;
        Self(Principal::from_slice(&bytes[1..1 + len]))
    }
}

/// Intent lock key for Map 47: (deployment_id, logical_resource) → marker.
///
/// This key is used by Router Map 47 and by Provision Maps 2/3. The stable byte encoding is
/// preserved exactly across both canisters.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub struct ProvisioningIntentKey {
    pub deployment_id: DeploymentId,
    pub logical_resource: LogicalResource,
}

impl ProvisioningIntentKey {
    pub fn new(deployment_id: DeploymentId, logical_resource: LogicalResource) -> Self {
        Self {
            deployment_id,
            logical_resource,
        }
    }
}

impl Storable for ProvisioningIntentKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(5 + 30 + self.logical_resource.into_bytes().len());
        out.extend_from_slice(&self.deployment_id.to_bytes());
        out.extend_from_slice(&self.logical_resource.into_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let deployment_id_len = 1 + bytes[0] as usize;
        let deployment_id = DeploymentId::from_bytes(Cow::Borrowed(&bytes[..deployment_id_len]));
        let logical_resource = LogicalResource::from_bytes(Cow::Borrowed(
            &bytes[deployment_id_len..deployment_id_len + 5],
        ));
        Self {
            deployment_id,
            logical_resource,
        }
    }
}

#[cfg(test)]
mod tests;

pub mod dictionary;
pub mod init_args;
pub mod wire;
