use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::federation::ShardId;
use ic_stable_structures::{
    Memory, StableCell,
    storable::{Bound, Storable},
};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Maximum UTF-8 byte length persisted for [`GraphBootstrapConfigV1::logical_graph_name`].
pub const MAX_LOGICAL_GRAPH_NAME_BYTES: usize = 256;

pub struct StableGraphMetadata<M: Memory>(StableCell<GraphMetadata, M>);

impl<M: Memory> StableGraphMetadata<M> {
    pub fn new(memory: M) -> Self {
        Self(StableCell::new(memory, GraphMetadata::default()))
    }

    pub fn init(memory: M, metadata: GraphMetadata) -> StableGraphMetadata<M> {
        Self(StableCell::init(memory, metadata))
    }

    pub fn get(&self) -> &GraphMetadata {
        self.0.get()
    }

    pub fn set(&mut self, metadata: GraphMetadata) -> Result<(), GraphMetadataError> {
        metadata.validate_for_store()?;
        self.0.set(metadata);
        Ok(())
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct FederationRouting {
    pub router_canister: Principal,
    pub shard_id: ShardId,
    pub index_canister: Principal,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphMetadataError {
    InvalidLogicalGraphName(String),
}

impl fmt::Display for GraphMetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphMetadataError::InvalidLogicalGraphName(error) => write!(f, "{}", error),
        }
    }
}

impl std::error::Error for GraphMetadataError {}

/// Bootstrap payload layout **revision 1** (logical graph label + optional index routing).
#[derive(CandidType, Serialize, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub struct GraphMetadataV1 {
    logical_graph_name: Option<String>,
    federation_routing: Option<FederationRouting>,
}

impl GraphMetadataV1 {
    pub(crate) fn validate_for_store(&self) -> Result<(), GraphMetadataError> {
        if let Some(name) = &self.logical_graph_name {
            GraphMetadataV1::validate_name(name)?;
        }
        Ok(())
    }

    pub fn validate_name(name: &str) -> Result<(), GraphMetadataError> {
        if name.len() > MAX_LOGICAL_GRAPH_NAME_BYTES {
            return Err(GraphMetadataError::InvalidLogicalGraphName(format!(
                "logical_graph_name exceeds {MAX_LOGICAL_GRAPH_NAME_BYTES} UTF-8 bytes"
            )));
        }
        Ok(())
    }
}

#[derive(CandidType, Serialize, Deserialize, Clone, Debug)]
pub enum GraphMetadata {
    V1(GraphMetadataV1),
}

impl GraphMetadata {
    pub fn validate_for_store(&self) -> Result<(), GraphMetadataError> {
        match self {
            GraphMetadata::V1(v) => v.validate_for_store(),
        }
    }

    pub fn validate_name(name: &str) -> Result<(), GraphMetadataError> {
        GraphMetadataV1::validate_name(name)
    }

    pub fn logical_graph_name(&self) -> Option<String> {
        match self {
            GraphMetadata::V1(v) => v.logical_graph_name.clone(),
        }
    }

    pub fn set_logical_graph_name(&mut self, name: Option<String>) {
        match self {
            GraphMetadata::V1(v) => v.logical_graph_name = name,
        }
    }

    pub fn federation_routing(&self) -> Option<FederationRouting> {
        match self {
            GraphMetadata::V1(v) => v.federation_routing.clone(),
        }
    }

    pub fn set_federation_routing(&mut self, federation_routing: Option<FederationRouting>) {
        match self {
            GraphMetadata::V1(v) => v.federation_routing = federation_routing,
        }
    }

    pub fn federation_configured(&self) -> bool {
        match self {
            GraphMetadata::V1(v) => v.federation_routing.is_some(),
        }
    }
}

impl Default for GraphMetadata {
    fn default() -> Self {
        Self::V1(GraphMetadataV1::default())
    }
}

impl Storable for GraphMetadata {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("failed to encode StoredMetadata"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("failed to encode StoredMetadata")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), GraphMetadata).expect("failed to decode StoredMetadata")
    }

    const BOUND: Bound = Bound::Unbounded;
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;

    #[test]
    fn validate_name_rejects_oversized_utf8() {
        let name = "x".repeat(MAX_LOGICAL_GRAPH_NAME_BYTES + 1);
        let err = GraphMetadata::validate_name(&name).unwrap_err();
        assert!(matches!(
            err,
            GraphMetadataError::InvalidLogicalGraphName(_)
        ));
    }

    #[test]
    fn validate_name_accepts_max_length() {
        let name = "x".repeat(MAX_LOGICAL_GRAPH_NAME_BYTES);
        GraphMetadata::validate_name(&name).expect("max length name should be valid");
    }

    #[test]
    fn metadata_round_trip_storable_encoding() {
        let mut metadata = GraphMetadata::default();
        metadata.set_logical_graph_name(Some("gleaph-test".into()));
        metadata.set_federation_routing(Some(FederationRouting {
            router_canister: Principal::management_canister(),
            shard_id: 7,
            index_canister: Principal::anonymous(),
        }));
        metadata.validate_for_store().expect("valid metadata");

        let bytes = metadata.to_bytes();
        let decoded = GraphMetadata::from_bytes(bytes);
        assert_eq!(decoded.logical_graph_name(), Some("gleaph-test".into()));
        assert!(decoded.federation_configured());
        assert_eq!(decoded.federation_routing().unwrap().shard_id, 7u32);
    }
}
