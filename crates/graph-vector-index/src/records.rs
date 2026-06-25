//! Stable keys and records for the degenerate `ivf_flat` derived vector index (ADR 0031 Slice 2).
//!
//! Keys are fixed-width big-endian so `BTreeMap` range order is index-major, then version/partition,
//! then page/slot — the order the canister scans for a single index generation. Most records are
//! Candid-encoded (`Bound::Unbounded`) wire envelopes; vector row bytes live in the slab page store
//! (ADR 0032), keyed by [`PageKey`] in the `VECTOR_PAGE_META` directory.
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

/// Storable wrapper for a `VectorSubject` value in `VECTOR_ID_TO_SUBJECT` (ADR 0031 Slice 6).
///
/// `ic_stable_structures::BTreeMap` values must implement `Storable`; the kernel `VectorSubject`
/// only derives Candid/Serde. Wrapping it here keeps the ICP `Storable` impl a vector-index concern
/// (the reverse-map locator) rather than leaking into `graph-kernel`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct VectorSubjectRecord(pub VectorSubject);

impl Storable for VectorSubjectRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode VectorSubjectRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode VectorSubjectRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), VectorSubjectRecord).expect("decode VectorSubjectRecord")
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

/// `(index_id, index_version, partition_id, page_id)` key for `VECTOR_PAGE_META` (ADR 0032).
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
    /// `Some` when live; `None` once deleted. Points at the live slot in the **active** index
    /// version while no rebuild is collapsing this subject.
    pub slot: Option<SlotRef>,
    /// Live slot in a rebuild's shadow (target) index version, maintained by dual-write while a
    /// rebuild is `Building`/`ReadyToPublish` (ADR 0031 Slice 7). `#[serde(default)]` so any row
    /// predating the field decodes as `None`. Collapsed into `slot` by post-publish cleanup.
    #[serde(default)]
    pub shadow_slot: Option<SlotRef>,
    /// `Some` when live; `None` once deleted — `VectorId` is never reused.
    pub vector_id: Option<u64>,
}

impl SubjectMapEntry {
    /// Resolves the live slot for `active_index_version`: the active `slot` when it matches, else the
    /// `shadow_slot` when it matches (after an atomic publish flips the active version onto the
    /// rebuilt one), else `None`. Both search paths and the rebuild-aware mutation path resolve the
    /// live slot through this so freshness is never read off the wrong version (ADR 0031 Slice 7).
    pub fn current_slot_for(&self, active_index_version: u64) -> Option<SlotRef> {
        if let Some(slot) = self.slot
            && slot.index_version == active_index_version
        {
            return Some(slot);
        }
        if let Some(shadow) = self.shadow_slot
            && shadow.index_version == active_index_version
        {
            return Some(shadow);
        }
        None
    }
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

/// Durable per-index rebuild lifecycle (`VECTOR_REBUILD_STATE`, ADR 0031 Slice 7/8).
///
/// Every long-running phase carries a resume cursor (subject keys / page keys as `Storable` bytes)
/// so each `*_step` honors the bounded-execution contract. `Sampling.candidates` accumulates a
/// bounded distinct candidate pool, then `Training` refines `nlist` centroids from it with
/// deterministic k-means-lite before they are written to `IVF_CENTROIDS` on the transition to
/// `Building` (ADR 0031 Slice 8). The combined durable `Training` value (`candidates + centroids`)
/// is bounded by `MAX_REBUILD_STATE_BYTES`; the candidate pool is sized to reserve room for the
/// centroids and encoding overhead inside that envelope. `Cleaning`/`Aborting` carry the `nlist`
/// they must tear down because `publish` overwrites `def.nlist`.
#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum VectorRebuildStateRecord {
    #[default]
    Idle,
    Sampling {
        target_index_version: u64,
        nlist: u32,
        sample_limit: u32,
        cursor: Option<Vec<u8>>,
        subjects_scanned: u64,
        candidates: Vec<Vec<u8>>,
    },
    Training {
        target_index_version: u64,
        nlist: u32,
        sample_limit: u32,
        iteration: u32,
        candidates: Vec<Vec<u8>>,
        centroids: Vec<Vec<u8>>,
    },
    Building {
        target_index_version: u64,
        nlist: u32,
        cursor: Option<Vec<u8>>,
        subjects_processed: u64,
    },
    ReadyToPublish {
        target_index_version: u64,
        nlist: u32,
    },
    Cleaning {
        old_version: u64,
        old_nlist: u32,
        target_index_version: u64,
        subject_cursor: Option<Vec<u8>>,
        page_cursor: Option<Vec<u8>>,
    },
    Aborting {
        target_index_version: u64,
        target_nlist: u32,
        subject_cursor: Option<Vec<u8>>,
        page_cursor: Option<Vec<u8>>,
    },
    Failed {
        target_index_version: u64,
        reason: String,
    },
}

impl Storable for VectorRebuildStateRecord {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode VectorRebuildStateRecord"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode VectorRebuildStateRecord")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), VectorRebuildStateRecord).expect("decode VectorRebuildStateRecord")
    }
}

/// A pre-encoded [`VectorRebuildStateRecord`] stored verbatim in `VECTOR_REBUILD_STATE`.
///
/// The bytes are exactly `VectorRebuildStateRecord::into_bytes()` (Candid), so the on-disk format is
/// identical to storing the record directly. The wrapper lets the rebuild step's fail-closed
/// encoded-size guard and the persist share a single Candid encode: the step encodes once, checks the
/// length, and stores these bytes without re-encoding (ADR 0031 Slice 7/8). `rebuild_state_of` decodes
/// them back into a [`VectorRebuildStateRecord`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawRebuildState(pub Vec<u8>);

impl Storable for RawRebuildState {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
    }
}

/// A pre-encoded [`VectorMaintenanceState`](gleaph_graph_kernel::vector_index::VectorMaintenanceState)
/// stored verbatim in `VECTOR_MAINTENANCE_STATE` (ADR 0031 Slice 10).
///
/// Mirrors [`RawRebuildState`]: the bytes are exactly the Candid encoding of the kernel
/// `VectorMaintenanceState`, so the on-disk format is identical to storing the type directly while
/// keeping the `Storable` impl local (the kernel type is foreign to this crate). The maintenance
/// step encodes once and persists these bytes; `maintenance_state_of` decodes them back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawMaintenanceState(pub Vec<u8>);

impl Storable for RawMaintenanceState {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Borrowed(&self.0)
    }

    fn into_bytes(self) -> Vec<u8> {
        self.0
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self(bytes.into_owned())
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
    fn vector_subject_record_storable_roundtrip() {
        let record = VectorSubjectRecord(VectorSubject::Vertex {
            shard_id: ShardId::new(3),
            vertex_id: 77,
        });
        assert_eq!(VectorSubjectRecord::from_bytes(record.to_bytes()), record);
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
            shadow_slot: Some(SlotRef {
                index_version: 2,
                partition_id: 3,
                page_id: 0,
                slot: 2,
                generation: 1,
            }),
            vector_id: Some(5),
        };
        assert_eq!(SubjectMapEntry::from_bytes(entry.to_bytes()), entry);
    }

    /// Pre-Slice-7 `SubjectMapEntry` (no `shadow_slot`) must still decode, defaulting to `None`.
    /// Encodes the legacy 5-field Candid record shape and asserts forward-compatible decode.
    #[test]
    fn subject_entry_pre_slice7_bytes_decode_with_no_shadow_slot() {
        #[derive(CandidType)]
        struct LegacySubjectMapEntry {
            embedding_incarnation: u64,
            stored_embedding_version: u64,
            deleted: bool,
            slot: Option<SlotRef>,
            vector_id: Option<u64>,
        }
        let legacy = LegacySubjectMapEntry {
            embedding_incarnation: 7,
            stored_embedding_version: 2,
            deleted: false,
            slot: Some(SlotRef {
                index_version: 1,
                partition_id: 0,
                page_id: 0,
                slot: 4,
                generation: 1,
            }),
            vector_id: Some(9),
        };
        let bytes = Encode!(&legacy).expect("encode legacy entry");
        let decoded = SubjectMapEntry::from_bytes(Cow::Owned(bytes));
        assert_eq!(decoded.embedding_incarnation, 7);
        assert_eq!(decoded.vector_id, Some(9));
        assert_eq!(decoded.shadow_slot, None, "missing field defaults to None");
    }

    #[test]
    fn rebuild_state_record_storable_roundtrip() {
        for state in [
            VectorRebuildStateRecord::Idle,
            VectorRebuildStateRecord::Sampling {
                target_index_version: 2,
                nlist: 8,
                sample_limit: 1024,
                cursor: Some(vec![1, 2, 3]),
                subjects_scanned: 17,
                candidates: vec![vec![0u8; 16], vec![1u8; 16]],
            },
            VectorRebuildStateRecord::Training {
                target_index_version: 2,
                nlist: 2,
                sample_limit: 1024,
                iteration: 3,
                candidates: vec![vec![0u8; 16], vec![1u8; 16], vec![2u8; 16]],
                centroids: vec![vec![0u8; 16], vec![1u8; 16]],
            },
            VectorRebuildStateRecord::Building {
                target_index_version: 2,
                nlist: 8,
                cursor: None,
                subjects_processed: 42,
            },
            VectorRebuildStateRecord::ReadyToPublish {
                target_index_version: 2,
                nlist: 8,
            },
            VectorRebuildStateRecord::Cleaning {
                old_version: 1,
                old_nlist: 1,
                target_index_version: 2,
                subject_cursor: Some(vec![9]),
                page_cursor: None,
            },
            VectorRebuildStateRecord::Aborting {
                target_index_version: 2,
                target_nlist: 8,
                subject_cursor: None,
                page_cursor: Some(vec![7]),
            },
            VectorRebuildStateRecord::Failed {
                target_index_version: 2,
                reason: "insufficient live vectors".to_string(),
            },
        ] {
            assert_eq!(
                VectorRebuildStateRecord::from_bytes(state.to_bytes()),
                state
            );
        }
    }

    #[test]
    fn current_slot_for_resolves_active_then_shadow() {
        let active = SlotRef {
            index_version: 1,
            partition_id: 0,
            page_id: 0,
            slot: 0,
            generation: 1,
        };
        let shadow = SlotRef {
            index_version: 2,
            partition_id: 5,
            page_id: 0,
            slot: 0,
            generation: 1,
        };
        let entry = SubjectMapEntry {
            embedding_incarnation: 1,
            stored_embedding_version: 1,
            deleted: false,
            slot: Some(active),
            shadow_slot: Some(shadow),
            vector_id: Some(1),
        };
        assert_eq!(entry.current_slot_for(1), Some(active));
        assert_eq!(entry.current_slot_for(2), Some(shadow));
        assert_eq!(entry.current_slot_for(3), None);
    }
}
