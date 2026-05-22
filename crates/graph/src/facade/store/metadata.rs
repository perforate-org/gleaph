//! GraphStore `metadata` implementation.

use super::super::stable::{METADATA, PEER_GRAPH_CANISTERS};
use super::super::{FederationRouting, GraphMetadata, GraphMetadataError};
use candid::Principal;

use super::GraphStore;

impl GraphStore {
    pub const fn new() -> Self {
        Self
    }

    pub fn set_metadata(&self, metadata: GraphMetadata) -> Result<(), GraphMetadataError> {
        METADATA.with_borrow_mut(|m| m.set(metadata))
    }

    pub fn logical_graph_name(&self) -> Option<String> {
        METADATA.with_borrow(|m| m.get().logical_graph_name())
    }

    pub fn set_logical_graph_name(&self, name: Option<String>) -> Result<(), GraphMetadataError> {
        if let Some(name) = &name {
            GraphMetadata::validate_name(name)?;
        }
        METADATA.with_borrow_mut(|m| {
            let mut metadata = m.get().clone();
            metadata.set_logical_graph_name(name);
            m.set(metadata)
        })
    }

    pub fn federation_routing(&self) -> Option<FederationRouting> {
        METADATA.with_borrow(|m| m.get().federation_routing())
    }

    pub fn set_federation_routing(
        &self,
        federation_routing: Option<FederationRouting>,
    ) -> Result<(), GraphMetadataError> {
        METADATA.with_borrow_mut(|m| {
            let mut metadata = m.get().clone();
            metadata.set_federation_routing(federation_routing);
            m.set(metadata)
        })
    }

    pub fn federation_configured(&self) -> bool {
        METADATA.with_borrow(|m| m.get().federation_configured())
    }

    pub fn is_peer_graph_canister(&self, principal: &Principal) -> bool {
        PEER_GRAPH_CANISTERS.with_borrow(|peers| peers.contains(principal))
    }

    pub fn bootstrap_peer_graph_canisters(&self, peers: &[Principal], self_canister: Principal) {
        PEER_GRAPH_CANISTERS.with_borrow_mut(|set| set.insert_many(peers, self_canister));
    }

    pub fn add_peer_graph_canister(&self, peer: Principal, self_canister: Principal) {
        if peer == self_canister {
            return;
        }
        PEER_GRAPH_CANISTERS.with_borrow_mut(|set| set.insert(peer));
    }

    pub fn remove_peer_graph_canister(&self, peer: &Principal) -> bool {
        PEER_GRAPH_CANISTERS.with_borrow_mut(|set| set.remove(peer))
    }
}
