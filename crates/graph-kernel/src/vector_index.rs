//! Shared vector-index types.
//!
//! Per [ADR 0031](design/adr/0031-vertex-embedding-store-and-derived-vector-index.md), this module
//! is the home for vector-index wire types. Slice 1 carried only the canonical embedding encoding.
//! Slice 2 adds the derived sync/mutation wire surface (`VectorIndexKind`, `VectorMetric`,
//! `VectorSubject`, `VectorEmbeddingSyncOp`, `IndexedEmbeddingCatalog`, `VectorIndexError`).
//! Search/cursor types are deliberately deferred to Slice 4+ (Router catalog + target resolution
//! is Slice 3; search/centroids are Slice 4+).
//!
//! # Version naming glossary
//!
//! Three distinct concepts that are never conflated in code or wire:
//!
//! - `index_version` (vector-index canister): physical index generation; page/partition head keys.
//! - `embedding_version` (graph canonical store): `StoredEmbedding.version`; carried on sync ops and
//!   the repair journal, and the *only* field consulted for sync/repair idempotence.
//! - `generation` (vector-index canister): slot/entity handle incarnation for append-and-tombstone.

use crate::federation::ShardId;
use candid::CandidType;
use serde::{Deserialize, Serialize};

/// Encoding of a stored vertex embedding.
///
/// Only fixed-dimension `F32` is supported in the first slice. New variants (`F16`, `I8`) must
/// update every exhaustive `match` on this enum, which is the intended compile-time gate before
/// an `UnsupportedEncoding`-style runtime branch is introduced.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorEncoding {
    /// IEEE-754 little-endian `f32` components; byte width is `dims * 4`.
    F32,
}

impl VectorEncoding {
    /// Byte width of one component for this encoding.
    pub const fn component_bytes(self) -> u32 {
        match self {
            Self::F32 => 4,
        }
    }

    /// Byte width (`stride`) of a full vector with `dims` components.
    pub const fn stride_bytes(self, dims: u16) -> u32 {
        self.component_bytes() * dims as u32
    }
}

/// Physical index structure for a derived vector index.
///
/// Slice 2 standardizes on `IvfFlat` operated in its degenerate form (`nlist = 1`,
/// `partition_id = 0`, no centroids). There is intentionally no separate `Flat` kind: the
/// baseline exact scan landed in Slice 4+ is `IvfFlat` with one partition.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorIndexKind {
    /// Inverted-file flat: centroid-pruned exact rerank. Degenerate `nlist = 1` in Slice 2.
    IvfFlat,
}

/// Distance metric for vector scoring.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorMetric {
    /// Squared Euclidean distance (no square root); smaller is nearer.
    L2Squared,
}

/// What a stored vector refers to.
///
/// Slice 2 supports only graph vertices. `shard_id` is carried inside the subject so the
/// subject-map key is `(index_id, subject)` with no separate `shard_id` field; the canister
/// validates `shard_id` against the caller's attached shard. `VectorSubject::Edge` is deferred.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorSubject {
    /// A graph vertex identified by its owning shard and shard-local id.
    Vertex { shard_id: ShardId, vertex_id: u32 },
}

impl VectorSubject {
    /// The owning shard of this subject.
    pub const fn shard_id(self) -> ShardId {
        match self {
            Self::Vertex { shard_id, .. } => shard_id,
        }
    }
}

/// Graph shard → vector-index canister: one derived embedding mutation.
///
/// `bytes` is REQUIRED for an upsert (`remove = false`) and EMPTY for a remove (`remove = true`);
/// idempotence is decided **only** by `embedding_version` against the retained subject clock and
/// never reads `bytes`. `encoding`/`dims` on a remove op are ignored by the canister.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorEmbeddingSyncOp {
    pub index_id: u32,
    /// Routing filter only until the Router catalog lands (Slice 3).
    pub embedding_name_id: u16,
    pub subject: VectorSubject,
    /// Canonical `StoredEmbedding.version` from the graph `VertexEmbeddingStore`.
    pub embedding_version: u64,
    pub encoding: VectorEncoding,
    pub dims: u16,
    /// REQUIRED for upsert; EMPTY for remove — never read for idempotence.
    pub bytes: Vec<u8>,
    pub remove: bool,
}

/// One indexed embedding definition supplied ephemerally by the Router (Slice 3).
///
/// Slice 2 defines the type only; the graph never persists an indexed-embedding registry. A
/// dispatch with no installed catalog skips vector sync entirely (production), while tests inject
/// a catalog via the embedding catalog context.
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub struct IndexedEmbeddingSpec {
    pub embedding_name_id: u16,
    pub index_id: u32,
    pub kind: VectorIndexKind,
    pub metric: VectorMetric,
    pub encoding: VectorEncoding,
    pub dims: u16,
}

/// Router-sourced snapshot of which embedding names are indexed (mirrors `IndexedPropertyCatalog`).
#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct IndexedEmbeddingCatalog {
    pub embeddings: Vec<IndexedEmbeddingSpec>,
}

impl IndexedEmbeddingCatalog {
    pub fn is_empty(&self) -> bool {
        self.embeddings.is_empty()
    }

    /// The indexed spec for an embedding name, if registered.
    pub fn spec_for(&self, embedding_name_id: u16) -> Option<IndexedEmbeddingSpec> {
        self.embeddings
            .iter()
            .copied()
            .find(|spec| spec.embedding_name_id == embedding_name_id)
    }
}

/// Vector-index canister mutation/sync/admin failure.
///
/// Single error type for the canister: mutation endpoints return it over the wire; admin endpoints
/// map it to a `String` at the canister boundary (mirroring `graph-index`).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
pub enum VectorIndexError {
    /// Caller is not the authorized router (admin endpoints).
    Unauthorized,
    /// The configured router principal is the anonymous principal.
    AnonymousRouter,
    /// A shard-canister principal supplied to attach is anonymous/invalid.
    InvalidPrincipalInRegistry,
    /// `shard_id` or principal is already attached to a different counterpart.
    ShardCanisterAlreadyAttached,
    /// The canister is already bound to a different graph/group.
    GraphOwnershipMismatch,
    /// Invalid index group configuration (e.g. zero group size).
    InvalidIndexGroupConfig,
    /// `shard_id` is outside the attached index group range.
    ShardOutOfRangeForGroup,
    /// Caller is not an attached graph shard for the requested `shard_id`.
    ShardNotAttached,
    /// Caller is not the attached canister for `shard_id`.
    WrongShardCanister,
    /// `subject.shard_id` does not match the caller's attached shard.
    ShardMismatch,
    /// No index definition for `index_id`.
    UnknownIndex,
    /// `encoding`/`dims` on an upsert disagree with the index definition.
    DimensionMismatch,
    /// `bytes.len()` does not equal `dims * stride` for an upsert.
    ByteWidthMismatch,
    /// A same-`embedding_version` upsert arrived with a different payload on a live subject.
    EmbeddingVersionConflict,
    /// The op's `remove` flag disagrees with the invoked mutation endpoint (e.g. `vector_upsert`
    /// received `remove = true`).
    MutationKindMismatch,
    /// An index definition whose `slots_per_page` would be `< 1`.
    InvalidPageCapacity,
    /// Internal allocator exhausted (`u64` overflow); not reachable in practice.
    AllocatorOverflow,
}

impl std::fmt::Display for VectorIndexError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let text = match self {
            Self::Unauthorized => "caller is not the authorized router",
            Self::AnonymousRouter => "router principal must not be the anonymous principal",
            Self::InvalidPrincipalInRegistry => "invalid principal in shard registry",
            Self::ShardCanisterAlreadyAttached => {
                "shard/canister attachment already exists with a different counterpart"
            }
            Self::GraphOwnershipMismatch => {
                "vector index canister is already bound to a different graph/group"
            }
            Self::InvalidIndexGroupConfig => "invalid index group configuration",
            Self::ShardOutOfRangeForGroup => "shard id is outside the attached index group range",
            Self::ShardNotAttached => "caller is not an attached graph shard",
            Self::WrongShardCanister => "caller is not the attached canister for this shard",
            Self::ShardMismatch => "subject shard does not match attached shard",
            Self::UnknownIndex => "unknown vector index id",
            Self::DimensionMismatch => "embedding encoding/dims disagree with the index definition",
            Self::ByteWidthMismatch => "embedding byte width does not match dims * stride",
            Self::EmbeddingVersionConflict => {
                "same embedding_version upsert with a different payload"
            }
            Self::MutationKindMismatch => {
                "sync op remove flag disagrees with the mutation endpoint"
            }
            Self::InvalidPageCapacity => "index page capacity yields fewer than one slot per page",
            Self::AllocatorOverflow => "vector index allocator overflow",
        };
        f.write_str(text)
    }
}

impl std::error::Error for VectorIndexError {}

#[cfg(test)]
mod tests {
    use super::*;
    use candid::{Decode, Encode};

    #[test]
    fn encoding_stride_bytes() {
        assert_eq!(VectorEncoding::F32.component_bytes(), 4);
        assert_eq!(VectorEncoding::F32.stride_bytes(8), 32);
    }

    #[test]
    fn sync_op_candid_roundtrip() {
        let op = VectorEmbeddingSyncOp {
            index_id: 7,
            embedding_name_id: 3,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(2),
                vertex_id: 42,
            },
            embedding_version: 9,
            encoding: VectorEncoding::F32,
            dims: 4,
            bytes: vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16],
            remove: false,
        };
        let bytes = Encode!(&op).expect("encode");
        assert_eq!(Decode!(&bytes, VectorEmbeddingSyncOp).expect("decode"), op);
    }

    #[test]
    fn remove_op_carries_empty_bytes() {
        let op = VectorEmbeddingSyncOp {
            index_id: 1,
            embedding_name_id: 0,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 1,
            },
            embedding_version: 2,
            encoding: VectorEncoding::F32,
            dims: 4,
            bytes: Vec::new(),
            remove: true,
        };
        let bytes = Encode!(&op).expect("encode");
        let decoded = Decode!(&bytes, VectorEmbeddingSyncOp).expect("decode");
        assert!(decoded.remove);
        assert!(decoded.bytes.is_empty());
    }

    #[test]
    fn catalog_lookup_and_candid_roundtrip() {
        let catalog = IndexedEmbeddingCatalog {
            embeddings: vec![IndexedEmbeddingSpec {
                embedding_name_id: 5,
                index_id: 11,
                kind: VectorIndexKind::IvfFlat,
                metric: VectorMetric::L2Squared,
                encoding: VectorEncoding::F32,
                dims: 16,
            }],
        };
        assert!(!catalog.is_empty());
        assert_eq!(catalog.spec_for(5).expect("spec").index_id, 11);
        assert!(catalog.spec_for(6).is_none());
        let bytes = Encode!(&catalog).expect("encode");
        assert_eq!(
            Decode!(&bytes, IndexedEmbeddingCatalog).expect("decode"),
            catalog
        );
        assert!(IndexedEmbeddingCatalog::default().is_empty());
    }

    #[test]
    fn error_candid_roundtrip() {
        let err = VectorIndexError::EmbeddingVersionConflict;
        let bytes = Encode!(&err).expect("encode");
        assert_eq!(Decode!(&bytes, VectorIndexError).expect("decode"), err);
    }

    #[test]
    fn subject_shard_accessor() {
        let subject = VectorSubject::Vertex {
            shard_id: ShardId::new(4),
            vertex_id: 9,
        };
        assert_eq!(subject.shard_id(), ShardId::new(4));
    }
}
