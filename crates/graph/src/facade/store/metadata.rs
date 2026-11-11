//! GraphStore `metadata` implementation.

use super::super::stable::METADATA;
use super::super::stable::metadata::TextTargetEntry;
use super::super::{FederationRouting, GraphMetadata, GraphMetadataError};

use super::GraphStore;
use candid::Principal;

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

    /// Sets this shard's local derived vector-index target within its existing federation routing
    /// (ADR 0031 Slice 4). The router-guarded `admin_set_vector_canister` endpoint calls this
    /// as the first step of the vector attach handshake, before the Router attaches the shard to the
    /// vector canister and flips its durable readiness bit. Errors if the shard has no federation
    /// routing (a standalone graph cannot host a derived vector index).
    pub fn set_vector_canister(
        &self,
        vector_canister: Option<Principal>,
    ) -> Result<(), GraphMetadataError> {
        METADATA.with_borrow_mut(|m| {
            let mut metadata = m.get().clone();
            let mut routing = metadata
                .federation_routing()
                .ok_or(GraphMetadataError::MissingFederationRouting)?;
            routing.vector_canister = vector_canister;
            metadata.set_federation_routing(Some(routing));
            m.set(metadata)
        })
    }

    /// Sets this shard's local derived text-index target for one (label, property) definition
    /// within its existing federation routing (plan 0297 TEXT attach handshake). The
    /// router-guarded `admin_set_text_canister` endpoint calls this so the shard's DML/maintenance
    /// flushes deliver that index's pending document log to its own canister. Re-attaching the
    /// same definition replaces the entry; distinct definitions accumulate. Errors if the shard
    /// has no federation routing.
    pub fn set_text_target(
        &self,
        label_id: u16,
        property_id: u32,
        text_canister: Principal,
    ) -> Result<(), GraphMetadataError> {
        METADATA.with_borrow_mut(|m| {
            let mut metadata = m.get().clone();
            let mut routing = metadata
                .federation_routing()
                .ok_or(GraphMetadataError::MissingFederationRouting)?;
            routing
                .text_targets
                .retain(|target| target.label_id != label_id || target.property_id != property_id);
            routing.text_targets.push(TextTargetEntry {
                label_id,
                property_id,
                canister: text_canister,
            });
            metadata.set_federation_routing(Some(routing));
            m.set(metadata)
        })
    }

    /// Sets this shard's local property-index target within its existing federation routing
    /// (ADR 0054 retrofit). The router-guarded `admin_set_index_canister` endpoint calls this as
    /// the graph leg of the retrofit attach handshake: an indexless-bootstrap shard is installed
    /// with the anonymous index sentinel in its local routing, so the Router re-points it here
    /// before the shard's index flushes, the canonical-export default puller resolution, and the
    /// maintenance timer can use it. Errors if the shard has no federation routing (a standalone
    /// graph has no router to flush through).
    pub fn set_index_canister(&self, index_canister: Principal) -> Result<(), GraphMetadataError> {
        METADATA.with_borrow_mut(|m| {
            let mut metadata = m.get().clone();
            let mut routing = metadata
                .federation_routing()
                .ok_or(GraphMetadataError::MissingFederationRouting)?;
            routing.index_canister = index_canister;
            metadata.set_federation_routing(Some(routing));
            m.set(metadata)
        })
    }
}
