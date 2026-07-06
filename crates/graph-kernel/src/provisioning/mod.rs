//! Shared provisioning protocol types used by both Router and Provision canisters.
//!
//! These types are intentionally owned by `gleaph-graph-kernel` — a neutral shared crate —
//! rather than by either canister's implementation, so cross-canister stable-memory and wire
//! encodings stay identical without forcing one canister to depend on the other's implementation.

use candid::CandidType;
use ic_stable_structures::storable::{Bound as StorableBound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Shared stable+wire resource kind. One-byte ordinal in stable memory.
#[repr(u8)]
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub enum ProvisionableResourceKind {
    GraphShard,
    PropertyIndex,
    VectorIndex,
}

impl Storable for ProvisionableResourceKind {
    const BOUND: StorableBound = StorableBound::Bounded {
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
        Self::from_ordinal(bytes.as_ref().first().copied())
    }
}

impl ProvisionableResourceKind {
    /// Decode a one-byte stable ordinal into a [`ProvisionableResourceKind`].
    ///
    /// Panics on unknown or missing ordinals with the same messages historically emitted by the
    /// two `Storable::from_bytes` paths, so existing stable bytes remain fail-closed.
    fn from_ordinal(ordinal: Option<u8>) -> Self {
        match ordinal {
            Some(0) => Self::GraphShard,
            Some(1) => Self::PropertyIndex,
            Some(2) => Self::VectorIndex,
            Some(b) => panic!("unknown ProvisionableResourceKind ordinal {b}"),
            None => panic!("missing ProvisionableResourceKind ordinal"),
        }
    }
}

/// Intent lock key for Map 47: (deployment_id, resource_kind, logical_resource_key) → marker.
///
/// This key is used by Router Map 47 and by Provision Maps 2/3. The stable byte encoding is
/// preserved exactly across both canisters.
#[derive(
    Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, CandidType,
)]
pub struct ProvisioningIntentKey {
    pub deployment_id: String,
    pub resource_kind: ProvisionableResourceKind,
    pub logical_resource_key: String,
}

impl ProvisioningIntentKey {
    pub fn new(
        deployment_id: &str,
        resource_kind: ProvisionableResourceKind,
        logical_resource_key: &str,
    ) -> Self {
        Self {
            deployment_id: deployment_id.to_owned(),
            resource_kind,
            logical_resource_key: logical_resource_key.to_owned(),
        }
    }
}

impl Storable for ProvisioningIntentKey {
    const BOUND: StorableBound = StorableBound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out =
            Vec::with_capacity(5 + self.deployment_id.len() + self.logical_resource_key.len());
        out.extend_from_slice(&(self.deployment_id.len() as u32).to_le_bytes());
        out.extend_from_slice(self.deployment_id.as_bytes());
        out.push(self.resource_kind as u8);
        out.extend_from_slice(&(self.logical_resource_key.len() as u32).to_le_bytes());
        out.extend_from_slice(self.logical_resource_key.as_bytes());
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
        let resource_kind = ProvisionableResourceKind::from_ordinal(bytes.get(offset).copied());
        offset += 1;
        let logical_resource_key_len = u32::from_le_bytes(
            bytes[offset..offset + 4]
                .try_into()
                .expect("resource_key len"),
        ) as usize;
        offset += 4;
        let logical_resource_key =
            String::from_utf8(bytes[offset..offset + logical_resource_key_len].to_vec())
                .expect("resource_key utf8");
        Self {
            deployment_id,
            resource_kind,
            logical_resource_key,
        }
    }
}

#[cfg(test)]
mod tests;

pub mod wire;
