//! Shared provisioning protocol types used by both Router and Provision canisters.
//!
//! These types are intentionally owned by `gleaph-graph-kernel` — a neutral shared crate —
//! rather than by either canister's implementation, so cross-canister stable-memory and wire
//! encodings stay identical without forcing one canister to depend on the other's implementation.

use candid::CandidType;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

use crate::federation::{IndexClusterId, ShardId, VectorIndexId};

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
    // Future: TextIndex(...), Procedure(...)
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
        match self {
            LogicalResource::GraphShard(shard) => {
                out.push(0u8);
                out.extend_from_slice(&shard.to_le_bytes());
            }
            LogicalResource::PropertyIndex(cluster) => {
                out.push(1u8);
                out.extend_from_slice(&cluster.to_le_bytes());
            }
            LogicalResource::VectorIndex(vector) => {
                out.push(2u8);
                out.extend_from_slice(&vector.to_le_bytes());
            }
            LogicalResource::Router => {
                out.push(3u8);
                out.extend_from_slice(&[0u8; 4]);
            }
        }
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut raw = [0u8; 4];
        raw.copy_from_slice(&bytes[1..5]);
        match bytes[0] {
            0 => LogicalResource::GraphShard(ShardId::from_le_bytes(raw)),
            1 => LogicalResource::PropertyIndex(IndexClusterId::from_le_bytes(raw)),
            2 => LogicalResource::VectorIndex(VectorIndexId::from_le_bytes(raw)),
            3 => LogicalResource::Router,
            other => panic!("unknown LogicalResource variant {other}"),
        }
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
    pub deployment_id: String,
    pub logical_resource: LogicalResource,
}

impl ProvisioningIntentKey {
    pub fn new(deployment_id: &str, logical_resource: LogicalResource) -> Self {
        Self {
            deployment_id: deployment_id.to_owned(),
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
        let mut out = Vec::with_capacity(
            5 + self.deployment_id.len() + self.logical_resource.into_bytes().len(),
        );
        out.extend_from_slice(&(self.deployment_id.len() as u32).to_le_bytes());
        out.extend_from_slice(self.deployment_id.as_bytes());
        out.extend_from_slice(&self.logical_resource.into_bytes());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let bytes = bytes.as_ref();
        let mut offset = 0usize;
        let deployment_id_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("deployment_id len"),
        ) as usize;
        offset += 4;
        let deployment_id = String::from_utf8(bytes[offset..offset + deployment_id_len].to_vec())
            .expect("deployment_id utf8");
        offset += deployment_id_len;
        let logical_resource =
            LogicalResource::from_bytes(Cow::Borrowed(&bytes[offset..offset + 5]));
        Self {
            deployment_id,
            logical_resource,
        }
    }
}

#[cfg(test)]
mod tests;

pub mod init_args;
pub mod wire;
