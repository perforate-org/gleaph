//! Stable keys and records for the degenerate `ivf_flat` derived vector index (ADR 0031 Slice 2).
//!
//! Keys are fixed-width big-endian so `BTreeMap` range order is index-major, then version/partition,
//! then page/slot — the order the canister scans for a single index generation. Records are
//! Candid-encoded (`Bound::Unbounded`) wire envelopes; the page blob holds the vector rows.
//!
//! # Version naming
//!
//! `index_version` is the physical index generation (defs/page keys). `embedding_version` is the
//! canonical [`StoredEmbedding`](gleaph_graph_kernel) version carried on sync ops and the subject
//! clock. `generation` is the slot incarnation for append-and-tombstone. These are never conflated.

use candid::{CandidType, Decode, Encode};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    VectorEncoding, VectorIndexKind, VectorMetric, VectorSubject,
};
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

const SUBJECT_TAG_VERTEX: u8 = 0;

/// `(index_id, subject)` key for `VECTOR_SUBJECT_TO_ID`.
///
/// `shard_id` lives inside the subject; there is intentionally no separate `shard_id` key field.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct SubjectKey {
    pub index_id: u32,
    pub subject: VectorSubject,
}

impl SubjectKey {
    pub const fn new(index_id: u32, subject: VectorSubject) -> Self {
        Self { index_id, subject }
    }

    /// Smallest `SubjectKey` for an `index_id`, used as the inclusive lower bound of a per-index
    /// range scan (`VECTOR_SUBJECT_TO_ID` is index-major). This must remain the minimum key for the
    /// `index_id` prefix even as `VectorSubject` grows variants: the encoding writes the subject
    /// tag at byte 4, and `SUBJECT_TAG_VERTEX == 0` is the lowest tag, so the all-zero subject body
    /// stays the prefix minimum. A scan starts here and stops when `key.index_id != index_id`.
    pub const fn index_lower(index_id: u32) -> Self {
        Self {
            index_id,
            subject: VectorSubject::Vertex {
                shard_id: ShardId::new(0),
                vertex_id: 0,
            },
        }
    }

    fn to_array(self) -> [u8; 13] {
        let mut out = [0u8; 13];
        out[0..4].copy_from_slice(&self.index_id.to_be_bytes());
        match self.subject {
            VectorSubject::Vertex {
                shard_id,
                vertex_id,
            } => {
                out[4] = SUBJECT_TAG_VERTEX;
                out[5..9].copy_from_slice(&shard_id.raw().to_be_bytes());
                out[9..13].copy_from_slice(&vertex_id.to_be_bytes());
            }
        }
        out
    }

    fn from_array(raw: [u8; 13]) -> Self {
        let index_id = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let tag = raw[4];
        assert_eq!(tag, SUBJECT_TAG_VERTEX, "unknown VectorSubject tag {tag}");
        let shard_id = u32::from_be_bytes([raw[5], raw[6], raw[7], raw[8]]);
        let vertex_id = u32::from_be_bytes([raw[9], raw[10], raw[11], raw[12]]);
        Self {
            index_id,
            subject: VectorSubject::Vertex {
                shard_id: shard_id.into(),
                vertex_id,
            },
        }
    }
}

impl Storable for SubjectKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 13,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_array()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_array())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0u8; 13];
        raw.copy_from_slice(bytes.as_ref());
        Self::from_array(raw)
    }
}

/// `(index_id, vector_id)` key for `VECTOR_ID_TO_SLOT`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct VectorIdKey {
    pub index_id: u32,
    pub vector_id: u64,
}

impl VectorIdKey {
    pub const fn new(index_id: u32, vector_id: u64) -> Self {
        Self {
            index_id,
            vector_id,
        }
    }

    fn to_array(self) -> [u8; 12] {
        let mut out = [0u8; 12];
        out[0..4].copy_from_slice(&self.index_id.to_be_bytes());
        out[4..12].copy_from_slice(&self.vector_id.to_be_bytes());
        out
    }
}

impl Storable for VectorIdKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 12,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_array()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_array())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0u8; 12];
        raw.copy_from_slice(bytes.as_ref());
        Self {
            index_id: u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]),
            vector_id: u64::from_be_bytes([
                raw[4], raw[5], raw[6], raw[7], raw[8], raw[9], raw[10], raw[11],
            ]),
        }
    }
}

/// `(index_id, index_version, partition_id)` key for `VECTOR_PARTITION_HEADS` and `IVF_CENTROIDS`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartitionKey {
    pub index_id: u32,
    pub index_version: u64,
    pub partition_id: u32,
}

impl PartitionKey {
    pub const fn new(index_id: u32, index_version: u64, partition_id: u32) -> Self {
        Self {
            index_id,
            index_version,
            partition_id,
        }
    }

    fn to_array(self) -> [u8; 16] {
        let mut out = [0u8; 16];
        out[0..4].copy_from_slice(&self.index_id.to_be_bytes());
        out[4..12].copy_from_slice(&self.index_version.to_be_bytes());
        out[12..16].copy_from_slice(&self.partition_id.to_be_bytes());
        out
    }
}

impl Storable for PartitionKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 16,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_array()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_array())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0u8; 16];
        raw.copy_from_slice(bytes.as_ref());
        Self {
            index_id: u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]),
            index_version: u64::from_be_bytes([
                raw[4], raw[5], raw[6], raw[7], raw[8], raw[9], raw[10], raw[11],
            ]),
            partition_id: u32::from_be_bytes([raw[12], raw[13], raw[14], raw[15]]),
        }
    }
}

/// `(index_id, index_version, partition_id, page_id)` key for `VECTOR_PAGE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageKey {
    pub index_id: u32,
    pub index_version: u64,
    pub partition_id: u32,
    pub page_id: u64,
}

impl PageKey {
    pub const fn new(index_id: u32, index_version: u64, partition_id: u32, page_id: u64) -> Self {
        Self {
            index_id,
            index_version,
            partition_id,
            page_id,
        }
    }

    fn to_array(self) -> [u8; 24] {
        let mut out = [0u8; 24];
        out[0..4].copy_from_slice(&self.index_id.to_be_bytes());
        out[4..12].copy_from_slice(&self.index_version.to_be_bytes());
        out[12..16].copy_from_slice(&self.partition_id.to_be_bytes());
        out[16..24].copy_from_slice(&self.page_id.to_be_bytes());
        out
    }
}

impl Storable for PageKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 24,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_array()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_array())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0u8; 24];
        raw.copy_from_slice(bytes.as_ref());
        Self {
            index_id: u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]),
            index_version: u64::from_be_bytes([
                raw[4], raw[5], raw[6], raw[7], raw[8], raw[9], raw[10], raw[11],
            ]),
            partition_id: u32::from_be_bytes([raw[12], raw[13], raw[14], raw[15]]),
            page_id: u64::from_be_bytes([
                raw[16], raw[17], raw[18], raw[19], raw[20], raw[21], raw[22], raw[23],
            ]),
        }
    }
}

/// Authoritative index definition + durable `VectorId` allocator (`VECTOR_INDEX_DEFS`).
///
/// `VECTOR_INDEX_DEFS` is the single source of truth for version/config; `IVF_CENTROID_META` never
/// restates `active_index_version`/`nlist`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorIndexDef {
    pub kind: VectorIndexKind,
    pub encoding: VectorEncoding,
    pub dims: u16,
    pub metric: VectorMetric,
    pub nlist: u32,
    pub active_index_version: u64,
    pub stride_bytes: u32,
    pub max_page_bytes: u32,
    pub slots_per_page: u32,
    pub next_vector_id: u64,
}

impl Storable for VectorIndexDef {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode VectorIndexDef"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode VectorIndexDef")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), VectorIndexDef).expect("decode VectorIndexDef")
    }
}

/// Centroid-only derived state (`IVF_CENTROID_META`). Degenerate in Slice 2 (`nlist=1`, not ready).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct IvfCentroidMeta {
    pub centroid_ready: bool,
    pub centroid_epoch: u64,
    /// Index version the centroids were trained against (staleness check only; defs win).
    pub trained_index_version: u64,
}

impl Storable for IvfCentroidMeta {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode IvfCentroidMeta"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode IvfCentroidMeta")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), IvfCentroidMeta).expect("decode IvfCentroidMeta")
    }
}

/// Location of one vector slot within a physical index generation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SlotRef {
    pub index_version: u64,
    pub partition_id: u32,
    pub page_id: u64,
    pub slot: u32,
    pub generation: u64,
}

impl Storable for SlotRef {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode SlotRef"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode SlotRef")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), SlotRef).expect("decode SlotRef")
    }
}

/// Subject map row — a durable clock that survives deletion (`VECTOR_SUBJECT_TO_ID`).
///
/// A removed subject keeps `(embedding_incarnation, stored_embedding_version)` so a stale replay
/// cannot resurrect it and a stale remove cannot tombstone a newer reinsert (ADR 0031 Slice 4). The
/// clock is the ordered pair: incarnation dominates, version breaks ties within an incarnation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SubjectMapEntry {
    /// Last applied delete-spanning incarnation (ADR 0031 Slice 4). `#[serde(default)]` so any row
    /// predating the field decodes as `0` (= unset); no such rows exist in production because vector
    /// dispatch was inert before activation.
    #[serde(default)]
    pub embedding_incarnation: u64,
    /// Last applied canonical version within `embedding_incarnation` (live OR tombstoned).
    pub stored_embedding_version: u64,
    /// True once removed; the row is retained as a tombstone clock.
    pub deleted: bool,
    /// `Some` when live; `None` once deleted.
    pub slot: Option<SlotRef>,
    /// `Some` when live; `None` once deleted — `VectorId` is never reused.
    pub vector_id: Option<u64>,
}

impl Storable for SubjectMapEntry {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode SubjectMapEntry"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode SubjectMapEntry")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), SubjectMapEntry).expect("decode SubjectMapEntry")
    }
}

/// Per-partition head: page chain bounds + durable `page_id` allocator (`VECTOR_PARTITION_HEADS`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct PartitionHead {
    pub first_page: u64,
    pub mutable_page: u64,
    pub page_count: u64,
    pub live_len: u64,
    /// Durable monotonic `page_id` allocator within this `(index_version, partition)`.
    pub next_page_id: u64,
}

impl Storable for PartitionHead {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode PartitionHead"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode PartitionHead")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), PartitionHead).expect("decode PartitionHead")
    }
}

/// One vector row within a page.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct PageRow {
    pub vector_id: u64,
    pub generation: u64,
    pub tombstoned: bool,
    /// `stride_bytes` of encoded vector components.
    pub bytes: Vec<u8>,
}

/// A fixed-capacity page of vector rows (`VECTOR_PAGE`).
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorPage {
    pub rows: Vec<PageRow>,
}

impl VectorPage {
    pub fn empty() -> Self {
        Self { rows: Vec::new() }
    }
}

impl Storable for VectorPage {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode VectorPage"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode VectorPage")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), VectorPage).expect("decode VectorPage")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_graph_kernel::federation::ShardId;

    #[test]
    fn subject_key_roundtrip_and_order() {
        let key = SubjectKey::new(
            7,
            VectorSubject::Vertex {
                shard_id: ShardId::new(2),
                vertex_id: 42,
            },
        );
        let bytes = key.to_bytes();
        assert_eq!(SubjectKey::from_bytes(bytes), key);
        // index-major ordering
        let lower = SubjectKey::new(
            6,
            VectorSubject::Vertex {
                shard_id: ShardId::new(9),
                vertex_id: 9,
            },
        );
        assert!(lower.to_array() < key.to_array());
    }

    #[test]
    fn index_lower_is_prefix_minimum() {
        let id = 7;
        let lower = SubjectKey::index_lower(id);
        // Equals the smallest concrete subject of this index today.
        assert_eq!(
            lower,
            SubjectKey::new(
                id,
                VectorSubject::Vertex {
                    shard_id: ShardId::new(0),
                    vertex_id: 0,
                },
            )
        );
        // <= any subject in the same index (prefix minimum), regardless of subject body.
        let some = SubjectKey::new(
            id,
            VectorSubject::Vertex {
                shard_id: ShardId::new(u32::MAX),
                vertex_id: u32::MAX,
            },
        );
        assert!(lower.to_array() <= some.to_array());
        // Strictly greater than every key of the previous index_id, and strictly less than the next
        // index's lower bound — so a `range(index_lower(id)..)` that breaks on `index_id != id`
        // sees exactly this index's rows.
        let prev_max = SubjectKey::new(
            id - 1,
            VectorSubject::Vertex {
                shard_id: ShardId::new(u32::MAX),
                vertex_id: u32::MAX,
            },
        );
        assert!(lower.to_array() > prev_max.to_array());
        assert!(lower.to_array() < SubjectKey::index_lower(id + 1).to_array());
    }

    #[test]
    fn page_key_roundtrip_and_order() {
        let a = PageKey::new(1, 1, 0, 0);
        let b = PageKey::new(1, 1, 0, 1);
        assert!(a.to_array() < b.to_array());
        assert_eq!(PageKey::from_bytes(a.to_bytes()), a);
    }

    #[test]
    fn def_storable_roundtrip() {
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::L2Squared,
            nlist: 1,
            active_index_version: 1,
            stride_bytes: 16,
            max_page_bytes: 65536,
            slots_per_page: 4000,
            next_vector_id: 1,
        };
        assert_eq!(VectorIndexDef::from_bytes(def.to_bytes()), def);
    }

    #[test]
    fn subject_entry_storable_roundtrip() {
        let entry = SubjectMapEntry {
            embedding_incarnation: 4,
            stored_embedding_version: 3,
            deleted: false,
            slot: Some(SlotRef {
                index_version: 1,
                partition_id: 0,
                page_id: 0,
                slot: 2,
                generation: 1,
            }),
            vector_id: Some(5),
        };
        assert_eq!(SubjectMapEntry::from_bytes(entry.to_bytes()), entry);
    }
}
