//! Distributed graph federation identifiers and placement types.

mod backfill_shard_state;
mod bulk_ingest_finalize;
mod claim;
mod edge_backfill_shard_state;
mod edge_posting_backfill;
mod embedding_backfill;
mod encoded;
mod expand;
mod global_edge_id;
mod graph_shard_key;
mod index_posting_purge;
mod peer_sync;
mod posting_backfill;
mod router_error;
mod shard_detach;
mod shard_id;
mod unique_effect;

pub use backfill_shard_state::BackfillShardState;
pub use bulk_ingest_finalize::{
    BULK_INGEST_FINALIZE_MAX_DRAIN_RETRIES, BulkIngestFinalizeArgs, BulkIngestFinalizeResult,
    HOT_FORWARD_EDGE_INSERT_THRESHOLD, is_gleaph_finalize_procedure_name,
};
pub use claim::ClaimId;
pub use edge_backfill_shard_state::{EDGE_PROPERTY_KEY_BYTES, EdgeBackfillShardState};
pub use edge_posting_backfill::{
    EdgePostingBackfillArgs, EdgePostingBackfillResult, EdgePropertyBackfillRequest,
};
pub use embedding_backfill::{
    EmbeddingBackfillArgs, EmbeddingBackfillResult, VertexEmbeddingBackfillRequest,
};
pub use encoded::{
    ENCODED_EDGE_ID_BYTES, ENCODED_VERTEX_ID_BYTES, ElementIdEncodingKey, EncodedEdgeId,
    EncodedVertexId, decode_global_edge_id, decode_global_vertex_id, encode_global_edge_id,
    encode_global_vertex_id,
};
pub use expand::{
    FederatedExpandArgs, FederatedExpandDirection, FederatedExpandNeighbor,
    MAX_FEDERATED_EXPAND_INLINE_VALUE_BYTE_WIDTH,
};
pub use global_edge_id::GlobalEdgeId;
pub use graph_shard_key::GraphShardKey;
pub use index_posting_purge::{
    IndexPostingPurgeCursor, IndexPostingPurgeStepResult, IndexPurgeKind,
};
pub use peer_sync::{AddGraphPeerArgs, BootstrapGraphPeersArgs, RemoveGraphPeerArgs};
pub use posting_backfill::{
    PostingBackfillArgs, PostingBackfillResult, VertexPropertyBackfillRequest,
};
pub use router_error::{
    RouterError, UNIQUENESS_VIOLATION_WIRE_PREFIX, VectorActivationBlockReason,
};
pub use shard_detach::{ShardDetachCursor, ShardDetachPhase, ShardDetachStepResult};
pub use shard_id::ShardId;
pub use unique_effect::{
    EffectId, UniqueAcquireEvidence, UniqueAcquireProof, UniqueEffectOp, UniqueEffectReceipt,
};

use crate::entry::GraphId;

use candid::{CandidType, Decode, Encode, Principal};
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Dense vertex index within a single graph shard (`VertexId` in LARA).
pub type LocalVertexId = u32;

/// Canonical global vertex key (`shard_id`, `local_vertex_id`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct GlobalVertexId {
    pub shard_id: ShardId,
    pub local_vertex_id: LocalVertexId,
}

/// Deprecated alias retained for mechanical migration in router stable maps.
pub type PhysicalPlacementKey = GlobalVertexId;

impl GlobalVertexId {
    #[inline]
    pub const fn new(shard_id: ShardId, local_vertex_id: LocalVertexId) -> Self {
        Self {
            shard_id,
            local_vertex_id,
        }
    }

    #[inline]
    pub const fn from_posting_hit(shard_id: ShardId, vertex_id: u32) -> Self {
        Self::new(shard_id, vertex_id)
    }

    #[inline]
    pub fn to_le_bytes(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0..4].copy_from_slice(&self.shard_id.to_le_bytes());
        out[4..8].copy_from_slice(&self.local_vertex_id.to_le_bytes());
        out
    }

    #[inline]
    pub fn from_le_bytes(bytes: [u8; 8]) -> Self {
        let mut shard = [0; 4];
        let mut local = [0; 4];
        shard.copy_from_slice(&bytes[0..4]);
        local.copy_from_slice(&bytes[4..8]);
        Self::new(ShardId::from_le_bytes(shard), u32::from_le_bytes(local))
    }
}

impl Storable for GlobalVertexId {
    const BOUND: Bound = Bound::Bounded {
        max_size: 8,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_le_bytes()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_le_bytes())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0u8; 8];
        raw.copy_from_slice(bytes.as_ref());
        Self::from_le_bytes(raw)
    }
}

/// Shard registration record returned by the router (`resolve_shard`).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ShardRegistryEntry {
    pub shard_id: ShardId,
    pub graph_canister: Principal,
    pub index_canister: Principal,
    pub graph_id: GraphId,
    pub registered_at_ns: u64,
    /// `false` while router awaits index `admin_attach_shard_canister`; excluded from dispatch/index fan-out.
    pub index_attached: bool,
    /// Derived vector-index canister wired to this shard (ADR 0031 Slice 4). `None` until the
    /// vector attach handshake sets the graph shard's local routing. The Router owns target
    /// selection (one target per graph). Decodes as `None` for pre-Slice-4 (V1) records.
    #[serde(default)]
    pub vector_index_canister: Option<Principal>,
    /// `true` once the vector attach handshake has set the shard's **local** `FederationRouting`
    /// target *and* attached the shard to the vector canister (ADR 0031 Slice 4). Mirrors
    /// `index_attached`; a faithful proxy for graph-local vector readiness. Decodes as `false` for
    /// pre-Slice-4 (V1) records.
    #[serde(default)]
    pub vector_index_attached: bool,
}

/// Pre-Slice-4 record shape, retained only to decode old `V1` stable bytes (ADR 0031 Slice 4).
/// New writes use [`ShardRegistryStableRecord::V2`]; never construct this for writes.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
struct ShardRegistryEntryV1 {
    shard_id: ShardId,
    graph_canister: Principal,
    index_canister: Principal,
    graph_id: GraphId,
    registered_at_ns: u64,
    index_attached: bool,
}

/// Stable-memory wire envelope for [`ShardRegistryEntry`]. `V2` adds the vector-index fields; old
/// `V1` bytes decode with `vector_index_canister = None` / `vector_index_attached = false`.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum ShardRegistryStableRecord {
    V1(ShardRegistryEntryV1),
    V2(ShardRegistryEntry),
}

impl Storable for ShardRegistryEntry {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(
            Encode!(&ShardRegistryStableRecord::V2(self.clone()))
                .expect("encode ShardRegistryEntry"),
        )
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&ShardRegistryStableRecord::V2(self)).expect("encode ShardRegistryEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), ShardRegistryStableRecord).expect("decode ShardRegistryEntry")
        {
            ShardRegistryStableRecord::V1(v1) => ShardRegistryEntry {
                shard_id: v1.shard_id,
                graph_canister: v1.graph_canister,
                index_canister: v1.index_canister,
                graph_id: v1.graph_id,
                registered_at_ns: v1.registered_at_ns,
                index_attached: v1.index_attached,
                vector_index_canister: None,
                vector_index_attached: false,
            },
            ShardRegistryStableRecord::V2(v2) => v2,
        }
    }

    const BOUND: Bound = Bound::Unbounded;
}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::Principal;
    use ic_stable_structures::Storable;

    #[test]
    fn global_vertex_id_le_bytes_roundtrip() {
        let key = GlobalVertexId::new(ShardId::new(0), 42);
        assert_eq!(key, GlobalVertexId::from_le_bytes(key.to_le_bytes()));
        assert_eq!(key, GlobalVertexId::from_posting_hit(ShardId::new(0), 42));
    }

    #[test]
    fn global_vertex_id_storable_roundtrip() {
        let key = GlobalVertexId::new(ShardId::new(1), 99);
        let bytes = key.to_bytes();
        assert_eq!(key, GlobalVertexId::from_bytes(bytes));
    }

    #[test]
    fn shard_registry_entry_storable_roundtrip() {
        let entry = ShardRegistryEntry {
            shard_id: ShardId::new(1),
            graph_canister: Principal::anonymous(),
            index_canister: Principal::management_canister(),
            graph_id: GraphId::from_raw(1),
            registered_at_ns: 123,
            index_attached: true,
            vector_index_canister: Some(Principal::management_canister()),
            vector_index_attached: true,
        };
        let bytes = entry.to_bytes();
        assert_eq!(entry, ShardRegistryEntry::from_bytes(bytes));
    }

    #[test]
    fn shard_registry_entry_decodes_old_v1_bytes() {
        // A pre-Slice-4 record persisted as the V1 envelope must still decode after upgrade, with
        // the new vector fields defaulting to "no vector index" (ADR 0031 Slice 4).
        let v1 = ShardRegistryEntryV1 {
            shard_id: ShardId::new(2),
            graph_canister: Principal::management_canister(),
            index_canister: Principal::management_canister(),
            graph_id: GraphId::from_raw(7),
            registered_at_ns: 999,
            index_attached: true,
        };
        let old_bytes = Encode!(&ShardRegistryStableRecord::V1(v1)).expect("encode V1");
        let decoded = ShardRegistryEntry::from_bytes(Cow::Owned(old_bytes));
        assert_eq!(decoded.shard_id, ShardId::new(2));
        assert_eq!(decoded.graph_id, GraphId::from_raw(7));
        assert!(decoded.index_attached);
        assert_eq!(decoded.vector_index_canister, None);
        assert!(!decoded.vector_index_attached);
    }
}
