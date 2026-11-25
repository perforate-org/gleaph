//! Distributed graph federation identifiers and placement types.

mod backfill_shard_state;
mod bulk_ingest_finalize;
mod claim;
mod edge_backfill_shard_state;
mod edge_posting_backfill;
mod encoded;
mod expand;
mod global_edge_id;
mod graph_shard_key;
mod index_posting_purge;
mod index_sync_status;
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
pub use encoded::{
    ENCODED_EDGE_ID_BYTES, ENCODED_VERTEX_ID_BYTES, ElementIdEncodingKey, EncodedEdgeId,
    EncodedVertexId, decode_global_edge_id, decode_global_vertex_id, encode_global_edge_id,
    encode_global_vertex_id,
};
pub use expand::{
    FederatedExpandArgs, FederatedExpandDirection, FederatedExpandNeighbor,
    MAX_FEDERATED_EXPAND_INLINE_PROPERTY_BYTE_WIDTH,
};
pub use global_edge_id::{GLOBAL_EDGE_ID_BYTES, GlobalEdgeId};
pub use graph_shard_key::GraphShardKey;
pub use index_posting_purge::{
    IndexPostingPurgeCursor, IndexPostingPurgeStepResult, IndexPurgeKind,
};
pub use index_sync_status::IndexSyncStatus;
pub use peer_sync::{AddGraphPeerArgs, BootstrapGraphPeersArgs, RemoveGraphPeerArgs};
pub use posting_backfill::{
    PostingBackfillArgs, PostingBackfillResult, VertexPropertyBackfillRequest,
};
pub use router_error::{
    RouterError, UNIQUENESS_VIOLATION_WIRE_PREFIX, VectorActivationBlockReason,
};
pub use shard_detach::{ShardDetachCursor, ShardDetachPhase, ShardDetachStepResult};
pub use shard_id::{IndexClusterId, ShardId, TextIndexId, VectorIndexId};
pub use unique_effect::{
    EffectId, UniqueAcquireEvidence, UniqueAcquireProof, UniqueEffectOp, UniqueEffectReceipt,
};

use crate::entry::GraphId;

use candid::{CandidType, Decode, Encode, Principal};
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

/// Dense vertex index within a single graph shard (`VertexId` in LARA). One type so a shard-local
/// vertex ordinal cannot be interchanged with a shard, label, slot, or mutation ordinal.
#[derive(
    Clone,
    Copy,
    Debug,
    Default,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    CandidType,
    Serialize,
    Deserialize,
)]
#[repr(transparent)]
#[serde(transparent)]
pub struct LocalVertexId(u32);

impl LocalVertexId {
    pub const fn new(raw: u32) -> Self {
        Self(raw)
    }
    pub const fn raw(self) -> u32 {
        self.0
    }
    pub const fn to_le_bytes(self) -> [u8; 4] {
        self.0.to_le_bytes()
    }
    pub const fn from_le_bytes(bytes: [u8; 4]) -> Self {
        Self(u32::from_le_bytes(bytes))
    }
    pub fn checked_add(self, delta: u32) -> Option<Self> {
        self.0.checked_add(delta).map(Self)
    }
    pub fn saturating_add(self, delta: u32) -> Self {
        Self(self.0.saturating_add(delta))
    }
}

impl From<u32> for LocalVertexId {
    fn from(raw: u32) -> Self {
        Self(raw)
    }
}

impl From<LocalVertexId> for u32 {
    fn from(id: LocalVertexId) -> Self {
        id.0
    }
}

impl From<LocalVertexId> for u64 {
    fn from(id: LocalVertexId) -> Self {
        u64::from(id.0)
    }
}

impl From<LocalVertexId> for crate::entry::VertexId {
    fn from(id: LocalVertexId) -> Self {
        crate::entry::VertexId::from(id.0)
    }
}

impl From<crate::entry::VertexId> for LocalVertexId {
    fn from(id: crate::entry::VertexId) -> Self {
        Self(u32::from(id))
    }
}

impl PartialEq<u32> for LocalVertexId {
    fn eq(&self, other: &u32) -> bool {
        self.0 == *other
    }
}

impl PartialEq<LocalVertexId> for u32 {
    fn eq(&self, other: &LocalVertexId) -> bool {
        *self == other.0
    }
}

impl std::fmt::Display for LocalVertexId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// A list of [`LocalVertexId`]s in a Candid wire record: `vec nat32`, the same type and bytes as
/// `Vec<u32>`.
///
/// Candid encodes a slice of a primitive element type such as `u32` as one memory block, but it
/// decides that by the element's Rust type, so `Vec<LocalVertexId>` falls back to encoding one
/// element at a time (about 50 more instructions per id). This type views its ids as the `u32`
/// slice they are laid out as and takes the block path, without copying. Wire records hold their
/// id lists as this type; code that builds or reads them works with the slice it derefs to, or
/// with `Vec<LocalVertexId>` through `From`.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct LocalVertexIds(Vec<LocalVertexId>);

impl LocalVertexIds {
    pub const fn new() -> Self {
        Self(Vec::new())
    }

    pub fn into_vec(self) -> Vec<LocalVertexId> {
        self.0
    }

    pub fn as_slice(&self) -> &[LocalVertexId] {
        &self.0
    }
}

impl CandidType for LocalVertexIds {
    fn _ty() -> candid::types::Type {
        <Vec<u32>>::ty()
    }

    fn idl_serialize<S: candid::types::Serializer>(&self, serializer: S) -> Result<(), S::Error> {
        // SAFETY: `LocalVertexId` is `#[repr(transparent)]` over `u32`, so a slice of it has the
        // layout, alignment and validity of a `u32` slice of the same length, and the borrow keeps
        // the elements alive for the returned slice.
        let raw: &[u32] =
            unsafe { std::slice::from_raw_parts(self.0.as_ptr().cast::<u32>(), self.0.len()) };
        raw.idl_serialize(serializer)
    }
}

impl std::ops::Deref for LocalVertexIds {
    type Target = [LocalVertexId];

    fn deref(&self) -> &[LocalVertexId] {
        &self.0
    }
}

impl std::ops::DerefMut for LocalVertexIds {
    fn deref_mut(&mut self) -> &mut [LocalVertexId] {
        &mut self.0
    }
}

impl From<Vec<LocalVertexId>> for LocalVertexIds {
    fn from(ids: Vec<LocalVertexId>) -> Self {
        Self(ids)
    }
}

impl From<LocalVertexIds> for Vec<LocalVertexId> {
    fn from(ids: LocalVertexIds) -> Self {
        ids.0
    }
}

impl FromIterator<LocalVertexId> for LocalVertexIds {
    fn from_iter<I: IntoIterator<Item = LocalVertexId>>(iter: I) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for LocalVertexIds {
    type Item = LocalVertexId;
    type IntoIter = std::vec::IntoIter<LocalVertexId>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl<'a> IntoIterator for &'a LocalVertexIds {
    type Item = &'a LocalVertexId;
    type IntoIter = std::slice::Iter<'a, LocalVertexId>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.iter()
    }
}

impl<T> PartialEq<Vec<T>> for LocalVertexIds
where
    LocalVertexId: PartialEq<T>,
{
    fn eq(&self, other: &Vec<T>) -> bool {
        self.0 == *other
    }
}

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
        Self::new(shard_id, LocalVertexId::new(vertex_id))
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
        Self::new(
            ShardId::from_le_bytes(shard),
            LocalVertexId::from_le_bytes(local),
        )
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
    /// Derived vector canister wired to this shard (ADR 0031 Slice 4). `None` until the
    /// vector attach handshake sets the graph shard's local routing. The Router owns target
    /// selection (one target per graph).
    pub vector_canister: Option<Principal>,
    /// `true` once the vector attach handshake has set the shard's **local** `FederationRouting`
    /// target *and* attached the shard to the vector canister (ADR 0031 Slice 4). Mirrors
    /// `index_attached`; a faithful proxy for graph-local vector readiness.
    pub vector_index_attached: bool,
}

/// Stable-memory wire envelope for [`ShardRegistryEntry`]. The current layout is `V1`; unknown
/// variants fail closed at decode.
#[derive(Clone, Debug, CandidType, Serialize, Deserialize)]
enum ShardRegistryStableRecord {
    V1(ShardRegistryEntry),
}

impl Storable for ShardRegistryEntry {
    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(self.clone().into_bytes())
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&ShardRegistryStableRecord::V1(self)).expect("encode ShardRegistryEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        match Decode!(bytes.as_ref(), ShardRegistryStableRecord).expect("decode ShardRegistryEntry")
        {
            ShardRegistryStableRecord::V1(entry) => entry,
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
        let key = GlobalVertexId::new(ShardId::new(0), 42.into());
        assert_eq!(key, GlobalVertexId::from_le_bytes(key.to_le_bytes()));
        assert_eq!(key, GlobalVertexId::from_posting_hit(ShardId::new(0), 42));
    }

    #[test]
    fn global_vertex_id_storable_roundtrip() {
        let key = GlobalVertexId::new(ShardId::new(1), 99.into());
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
            vector_canister: Some(Principal::management_canister()),
            vector_index_attached: true,
        };
        let bytes = entry.to_bytes();
        assert_eq!(entry, ShardRegistryEntry::from_bytes(bytes));
    }
}

#[cfg(test)]
mod local_vertex_ids_wire_tests {
    use super::{LocalVertexId, LocalVertexIds};
    use candid::{CandidType, Decode, Encode};

    #[test]
    fn wire_type_and_bytes_match_vec_u32_and_round_trip() {
        let raw = vec![0_u32, 7, 42, u32::MAX];
        let ids: LocalVertexIds = raw.iter().copied().map(LocalVertexId::new).collect();

        assert_eq!(LocalVertexIds::ty(), <Vec<u32>>::ty());
        let encoded = Encode!(&ids).expect("encode ids");
        assert_eq!(encoded, Encode!(&raw).expect("encode raw"));
        let decoded = Decode!(&encoded, LocalVertexIds).expect("decode ids");
        assert_eq!(decoded, ids);
        assert_eq!(decoded, raw);
    }
}
