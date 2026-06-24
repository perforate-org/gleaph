use candid::{CandidType, Decode, Encode, Principal};
use gleaph_graph_kernel::federation::ShardId;
use ic_stable_structures::{
    Memory, StableCell,
    storable::{Bound, Storable},
};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;
use std::fmt;

/// Maximum UTF-8 byte length persisted for [`GraphMetadataV1::logical_graph_name`].
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
    /// Derived vector-index canister (ADR 0031). `None` on shards with no vector index attached;
    /// the Router owns target selection (no `VectorSyncSpec` is persisted here). When `Some`, it
    /// must not be the anonymous principal.
    #[serde(default)]
    pub vector_index_canister: Option<Principal>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum GraphMetadataError {
    InvalidLogicalGraphName(String),
    /// A federation-routing principal (`router_canister` or `index_canister`) was the anonymous
    /// principal, which can never be a trusted federation identity.
    AnonymousFederationPrincipal(&'static str),
}

impl fmt::Display for GraphMetadataError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GraphMetadataError::InvalidLogicalGraphName(error) => write!(f, "{}", error),
            GraphMetadataError::AnonymousFederationPrincipal(field) => write!(
                f,
                "federation routing {field} must not be the anonymous principal"
            ),
        }
    }
}

impl std::error::Error for GraphMetadataError {}

/// Bootstrap payload layout revision 1 (logical graph label + optional index routing).
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
        if let Some(routing) = &self.federation_routing {
            if routing.router_canister == Principal::anonymous() {
                return Err(GraphMetadataError::AnonymousFederationPrincipal(
                    "router_canister",
                ));
            }
            if routing.index_canister == Principal::anonymous() {
                return Err(GraphMetadataError::AnonymousFederationPrincipal(
                    "index_canister",
                ));
            }
            if routing.vector_index_canister == Some(Principal::anonymous()) {
                return Err(GraphMetadataError::AnonymousFederationPrincipal(
                    "vector_index_canister",
                ));
            }
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

/// Versioned graph metadata for stable storage and upgrade-safe evolution.
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
    fn validate_rejects_anonymous_router_canister() {
        let mut metadata = GraphMetadata::default();
        metadata.set_federation_routing(Some(FederationRouting {
            router_canister: Principal::anonymous(),
            shard_id: ShardId::new(0),
            index_canister: Principal::from_slice(&[3; 29]),
            vector_index_canister: None,
        }));
        assert_eq!(
            metadata.validate_for_store(),
            Err(GraphMetadataError::AnonymousFederationPrincipal(
                "router_canister"
            ))
        );
    }

    #[test]
    fn validate_rejects_anonymous_index_canister() {
        let mut metadata = GraphMetadata::default();
        metadata.set_federation_routing(Some(FederationRouting {
            router_canister: Principal::management_canister(),
            shard_id: ShardId::new(0),
            index_canister: Principal::anonymous(),
            vector_index_canister: None,
        }));
        assert_eq!(
            metadata.validate_for_store(),
            Err(GraphMetadataError::AnonymousFederationPrincipal(
                "index_canister"
            ))
        );
    }

    #[test]
    fn validate_rejects_anonymous_vector_index_canister() {
        let mut metadata = GraphMetadata::default();
        metadata.set_federation_routing(Some(FederationRouting {
            router_canister: Principal::management_canister(),
            shard_id: ShardId::new(0),
            index_canister: Principal::from_slice(&[3; 29]),
            vector_index_canister: Some(Principal::anonymous()),
        }));
        assert_eq!(
            metadata.validate_for_store(),
            Err(GraphMetadataError::AnonymousFederationPrincipal(
                "vector_index_canister"
            ))
        );
    }

    #[test]
    fn store_set_rejects_anonymous_routing_and_leaves_state_unchanged() {
        use ic_stable_structures::DefaultMemoryImpl;
        let mut cell = StableGraphMetadata::new(DefaultMemoryImpl::default());
        assert!(cell.get().federation_routing().is_none());

        let mut metadata = GraphMetadata::default();
        metadata.set_federation_routing(Some(FederationRouting {
            router_canister: Principal::anonymous(),
            shard_id: ShardId::new(0),
            index_canister: Principal::from_slice(&[3; 29]),
            vector_index_canister: None,
        }));
        let err = cell
            .set(metadata)
            .expect_err("anonymous router must be rejected at the persistence boundary");
        assert_eq!(
            err,
            GraphMetadataError::AnonymousFederationPrincipal("router_canister")
        );
        assert!(
            cell.get().federation_routing().is_none(),
            "rejected routing must not be persisted"
        );
    }

    #[test]
    fn metadata_round_trip_storable_encoding() {
        let mut metadata = GraphMetadata::default();
        metadata.set_logical_graph_name(Some("gleaph-test".into()));
        metadata.set_federation_routing(Some(FederationRouting {
            router_canister: Principal::management_canister(),
            shard_id: ShardId::new(0),
            index_canister: Principal::from_slice(&[3; 29]),
            vector_index_canister: None,
        }));
        metadata.validate_for_store().expect("valid metadata");

        let bytes = metadata.to_bytes();
        let decoded = GraphMetadata::from_bytes(bytes);
        assert_eq!(decoded.logical_graph_name(), Some("gleaph-test".into()));
        assert!(decoded.federation_configured());
        assert_eq!(
            decoded.federation_routing().unwrap().shard_id,
            gleaph_graph_kernel::federation::ShardId::new(0)
        );
    }
}
