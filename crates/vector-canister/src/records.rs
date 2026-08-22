//! Stable keys and records for the degenerate `ivf_flat` derived vector index (ADR 0031 Slice 2).
//!
//! Keys are fixed-width big-endian so `BTreeMap` range order is index-major, then version/partition,
//! then page/slot — the order the canister scans for a single index generation. Most records are
//! Candid-encoded (`Bound::Unbounded`) wire envelopes; vector row bytes live in the slab page store
//! (ADR 0032), keyed by [`PageKey`] in the `VECTOR_PAGE_META` directory.
//!
//! # Version naming
//!
//! `index_version` is the physical index generation (defs/page keys). `mutation_id` is the graph
//! per-shard ordering stamp carried on sync ops and the subject clock (ADR 0064 §5). `generation` is
//! the slot incarnation for append-and-tombstone. These are never conflated.

use candid::{CandidType, Decode, Encode};
use gleaph_graph_kernel::federation::ShardId;
use gleaph_graph_kernel::vector_index::{
    VectorEncoding, VectorIndexKind, VectorMetric, VectorSubject,
};
use ic_stable_linear_hash_map::{ScanCursor, ScanError, StableHashKey, StableMapValue};
use ic_stable_structures::storable::{Bound, Storable};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

const SUBJECT_TAG_VERTEX: u8 = 0;
const VECTOR_INDEX_DEF_BYTES: usize = 41;
const VECTOR_INDEX_DEF_STORAGE_ID: [u8; 16] = *b"GLEAPH-VECDEF-01";

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

impl StableHashKey for SubjectKey {
    const KEY_STORAGE_ID: [u8; 16] = *b"GLEAPH-SUBKEY-01";
    const KEY_ROUTING_ID: [u8; 16] = *b"GLEAPH-SUBRTE-01";
    type HashBytes<'a>
        = [u8; 13]
    where
        Self: 'a;

    fn stable_hash_bytes(&self) -> Self::HashBytes<'_> {
        self.to_array()
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

impl StableHashKey for PartitionKey {
    const KEY_STORAGE_ID: [u8; 16] = *b"GLEAPH-PARTKEY-0";
    const KEY_ROUTING_ID: [u8; 16] = *b"GLEAPH-PARTRTE-0";
    type HashBytes<'a>
        = [u8; 16]
    where
        Self: 'a;

    fn stable_hash_bytes(&self) -> Self::HashBytes<'_> {
        self.to_array()
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
    /// Logical stored width per component: `component_bytes × dims` (the wire `bytes` length).
    pub stride_bytes: u32,
    /// Page row width: 16-byte-aligned `pad_stride_bytes` (the stored `vector_bytes` stride).
    pub pad_stride_bytes: u32,
    /// Row-meta stride: `4 + aux_bytes` (4 | 8 | 12).
    pub meta_stride_bytes: u32,
    /// Run-table width: `min(owned_shards, MAX_RUNS)`, frozen at def creation.
    pub run_capacity: u32,
    pub max_page_bytes: u32,
    pub slots_per_page: u32,
}

impl Storable for VectorIndexDef {
    const BOUND: Bound = Bound::Bounded {
        max_size: VECTOR_INDEX_DEF_BYTES as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = [0u8; VECTOR_INDEX_DEF_BYTES];
        out[0] = self.kind.as_u8();
        out[1] = self.encoding.as_u8();
        out[2..4].copy_from_slice(&self.dims.to_le_bytes());
        out[4] = self.metric.as_u8();
        out[5..9].copy_from_slice(&self.nlist.to_le_bytes());
        out[9..17].copy_from_slice(&self.active_index_version.to_le_bytes());
        out[17..21].copy_from_slice(&self.stride_bytes.to_le_bytes());
        out[21..25].copy_from_slice(&self.pad_stride_bytes.to_le_bytes());
        out[25..29].copy_from_slice(&self.meta_stride_bytes.to_le_bytes());
        out[29..33].copy_from_slice(&self.run_capacity.to_le_bytes());
        out[33..37].copy_from_slice(&self.max_page_bytes.to_le_bytes());
        out[37..41].copy_from_slice(&self.slots_per_page.to_le_bytes());
        Cow::Owned(out.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        assert_eq!(
            b.len(),
            VECTOR_INDEX_DEF_BYTES,
            "VectorIndexDef expects exactly {VECTOR_INDEX_DEF_BYTES} bytes"
        );
        Self {
            kind: VectorIndexKind::from_u8(b[0]).expect("valid kind"),
            encoding: VectorEncoding::from_u8(b[1]).expect("valid encoding"),
            dims: u16::from_le_bytes(b[2..4].try_into().expect("dims")),
            metric: VectorMetric::from_u8(b[4]).expect("valid metric"),
            nlist: u32::from_le_bytes(b[5..9].try_into().expect("nlist")),
            active_index_version: u64::from_le_bytes(b[9..17].try_into().expect("version")),
            stride_bytes: u32::from_le_bytes(b[17..21].try_into().expect("stride")),
            pad_stride_bytes: u32::from_le_bytes(b[21..25].try_into().expect("pad")),
            meta_stride_bytes: u32::from_le_bytes(b[25..29].try_into().expect("meta")),
            run_capacity: u32::from_le_bytes(b[29..33].try_into().expect("run")),
            max_page_bytes: u32::from_le_bytes(b[33..37].try_into().expect("max_page")),
            slots_per_page: u32::from_le_bytes(b[37..41].try_into().expect("slots")),
        }
    }
}

impl StableMapValue for VectorIndexDef {
    const VALUE_STORAGE_ID: [u8; 16] = VECTOR_INDEX_DEF_STORAGE_ID;
}

/// Centroid-only derived state (`IVF_CENTROID_META`). Degenerate in Slice 2 (`nlist=1`, not ready).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct IvfCentroidMeta {
    pub centroid_ready: bool,
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
///
/// Positions are write-once (a slot's payload never changes; superseded rows are tombstoned), so
/// `SlotRef` carries no per-row generation: freshness is validated positionally by the caller
/// against `VECTOR_SUBJECT_TO_ID`, and the slot must be in range and non-tombstoned to read (ADR 0064
/// §7).
///
/// `index_version` and `page_id` are stored as `u32` to keep the subject-map value compact. Both are
/// small in practice (`index_version` is the physical index generation, incremented per rebuild;
/// `page_id` is bounded by the page count within a partition). Callers widen to `u64` when building
/// `PageKey`/`PartitionKey` or comparing against the `u64` active/target index version; this is a
/// fail-closed assumption that neither exceeds `u32::MAX`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SlotRef {
    pub index_version: u32,
    pub partition_id: u32,
    pub page_id: u32,
    pub slot: u32,
}

/// Fixed-size encoding of the subject-map row for the clustered-hash-map subject store.
///
/// The clustered hash map requires a fixed-size `Storable` value, so the row is stored with a fixed
/// 41-byte layout: `stamp` (8) + `flags` (1) + `slot` (16) + `shadow_slot` (16). The flags byte packs
/// `deleted` (bit 0), `slot` presence (bit 1), and `shadow_slot` presence (bit 2), disambiguating
/// `None` from a zero-valued [`SlotRef`] without a per-option tag byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FixedSubjectMapEntry {
    pub stamp: u64,
    pub deleted: bool,
    pub slot: Option<SlotRef>,
    pub shadow_slot: Option<SlotRef>,
}

impl FixedSubjectMapEntry {
    /// Resolves the live slot for `active_index_version`: the active `slot` when it matches, else the
    /// `shadow_slot` when it matches (after an atomic publish flips the active version onto the
    /// rebuilt one), else `None`. Both search paths and the rebuild-aware mutation path resolve the
    /// live slot through this so freshness is never read off the wrong version (ADR 0031 Slice 7).
    pub fn current_slot_for(&self, active_index_version: u64) -> Option<SlotRef> {
        if let Some(slot) = self.slot
            && slot.index_version as u64 == active_index_version
        {
            return Some(slot);
        }
        if let Some(shadow) = self.shadow_slot
            && shadow.index_version as u64 == active_index_version
        {
            return Some(shadow);
        }
        None
    }
}

/// Encodes a `SlotRef` payload into `out[off..off+16]`, returning the next offset. Presence is
/// tracked in the entry's flags byte (not here); a `None` leaves the 16 bytes zero.
fn encode_slot_ref(slot: Option<SlotRef>, out: &mut [u8], off: usize) -> usize {
    if let Some(s) = slot {
        out[off..off + 4].copy_from_slice(&s.index_version.to_le_bytes());
        out[off + 4..off + 8].copy_from_slice(&s.partition_id.to_le_bytes());
        out[off + 8..off + 12].copy_from_slice(&s.page_id.to_le_bytes());
        out[off + 12..off + 16].copy_from_slice(&s.slot.to_le_bytes());
    }
    off + 16
}

/// Decodes a `SlotRef` payload from `bytes[off..off+16]`.
fn decode_slot_ref(bytes: &[u8], off: usize) -> SlotRef {
    SlotRef {
        index_version: u32::from_le_bytes(bytes[off..off + 4].try_into().unwrap()),
        partition_id: u32::from_le_bytes(bytes[off + 4..off + 8].try_into().unwrap()),
        page_id: u32::from_le_bytes(bytes[off + 8..off + 12].try_into().unwrap()),
        slot: u32::from_le_bytes(bytes[off + 12..off + 16].try_into().unwrap()),
    }
}

impl Storable for FixedSubjectMapEntry {
    const BOUND: Bound = Bound::Bounded {
        max_size: 41,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = [0u8; 41];
        out[0..8].copy_from_slice(&self.stamp.to_le_bytes());
        out[8] = (self.deleted as u8)
            | ((self.slot.is_some() as u8) << 1)
            | ((self.shadow_slot.is_some() as u8) << 2);
        encode_slot_ref(self.slot, &mut out, 9);
        encode_slot_ref(self.shadow_slot, &mut out, 25);
        Cow::Owned(out.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        let stamp = u64::from_le_bytes(b[0..8].try_into().unwrap());
        let flags = b[8];
        let deleted = flags & 0b0000_0001 != 0;
        let slot = (flags & 0b0000_0010 != 0).then(|| decode_slot_ref(b, 9));
        let shadow_slot = (flags & 0b0000_0100 != 0).then(|| decode_slot_ref(b, 25));
        Self {
            stamp,
            deleted,
            slot,
            shadow_slot,
        }
    }
}

impl StableMapValue for FixedSubjectMapEntry {
    const VALUE_STORAGE_ID: [u8; 16] = *b"GLEAPH-SUBVAL-01";
}

/// Key for the deleted-subjects list (`VECTOR_DELETED_SUBJECTS`): `(shard, tombstone stamp, subject)`.
///
/// The subject is part of the key so two subjects in the same shard tombstoned by the same DML
/// `mutation_id` (e.g. a vertex delete dispatching removes across multiple index ids) do not collide.
/// Big-endian so `BTreeMap` order groups by shard, then stamp, then subject — the order the GC walks
/// to stop at each shard's cutoff.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct DeletedSubjectKey {
    pub shard_id: ShardId,
    pub stamp: u64,
    pub subject: SubjectKey,
}

impl DeletedSubjectKey {
    pub const fn new(shard_id: ShardId, stamp: u64, subject: SubjectKey) -> Self {
        Self {
            shard_id,
            stamp,
            subject,
        }
    }

    fn to_array(self) -> [u8; 25] {
        let mut out = [0u8; 25];
        out[0..4].copy_from_slice(&self.shard_id.raw().to_be_bytes());
        out[4..12].copy_from_slice(&self.stamp.to_be_bytes());
        out[12..25].copy_from_slice(&self.subject.to_array());
        out
    }

    fn from_array(raw: [u8; 25]) -> Self {
        let shard_id = u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]);
        let stamp = u64::from_be_bytes([
            raw[4], raw[5], raw[6], raw[7], raw[8], raw[9], raw[10], raw[11],
        ]);
        let mut subject_raw = [0u8; 13];
        subject_raw.copy_from_slice(&raw[12..25]);
        Self {
            shard_id: shard_id.into(),
            stamp,
            subject: SubjectKey::from_array(subject_raw),
        }
    }
}

impl Storable for DeletedSubjectKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 25,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_array()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_array())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0u8; 25];
        raw.copy_from_slice(bytes.as_ref());
        Self::from_array(raw)
    }
}

/// Per-shard watermark pair used for conservative tombstone GC (ADR 0064 §5).
///
/// `graph_watermark` is the highest graph→vector acked stamp; `router_watermark` is the highest
/// Router→vector acked stamp. A deleted subject-map entry with `stamp <= min(both)` for its shard is
/// unreachable (no stale replay can arrive) and is GC'd. The production Router watermark remains
/// zero, so tombstone deletion is currently paused.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct ShardWatermarks {
    pub graph_watermark: u64,
    pub router_watermark: u64,
}

impl ShardWatermarks {
    /// The GC cutoff: `min(graph_watermark, router_watermark)`. A deleted entry at or below this stamp
    /// is unreachable. `0` (no watermark yet) means nothing is GC-eligible.
    pub fn cutoff(&self) -> u64 {
        self.graph_watermark.min(self.router_watermark)
    }
}

impl Storable for ShardWatermarks {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode ShardWatermarks"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode ShardWatermarks")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), ShardWatermarks).expect("decode ShardWatermarks")
    }
}

/// Per-partition head: page chain bounds + durable `page_id` allocator (`VECTOR_PARTITION_HEADS`).
///
/// `live_len`/`page_count` serve the documented O(`nlist`) partition-health check without full
/// scans.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct PartitionHead {
    pub mutable_page: u64,
    pub page_count: u64,
    pub live_len: u64,
    /// Durable monotonic `page_id` allocator within this `(index_version, partition)`.
    pub next_page_id: u64,
}

impl Storable for PartitionHead {
    const BOUND: Bound = Bound::Bounded {
        max_size: 32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = [0u8; 32];
        out[0..8].copy_from_slice(&self.mutable_page.to_le_bytes());
        out[8..16].copy_from_slice(&self.page_count.to_le_bytes());
        out[16..24].copy_from_slice(&self.live_len.to_le_bytes());
        out[24..32].copy_from_slice(&self.next_page_id.to_le_bytes());
        Cow::Owned(out.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        assert_eq!(b.len(), 32, "PartitionHead expects exactly 32 bytes");
        Self {
            mutable_page: u64::from_le_bytes(b[0..8].try_into().expect("mutable_page")),
            page_count: u64::from_le_bytes(b[8..16].try_into().expect("page_count")),
            live_len: u64::from_le_bytes(b[16..24].try_into().expect("live_len")),
            next_page_id: u64::from_le_bytes(b[24..32].try_into().expect("next_page_id")),
        }
    }
}

impl StableMapValue for PartitionHead {
    const VALUE_STORAGE_ID: [u8; 16] = *b"GLEAPH-PARTVAL-0";
}

/// One frozen rebuild candidate: a live row's native stored bytes plus its row-meta aux (the `I8`
/// scale; zero for `F32`). The pool snapshots Sampling-time values and stays immutable into
/// `Training` even though dual-write mutations keep mutating live rows mid-rebuild.
#[derive(Clone, Debug, PartialEq, Eq, Hash, CandidType, Serialize, Deserialize)]
pub struct RebuildCandidate {
    /// The row's stored vector bytes (`stride_bytes` wide), verbatim from the page store.
    pub stored: Vec<u8>,
    /// The row's page-store aux (the `I8` quantization scale; zero for `F32`).
    pub aux: [u8; 8],
}

/// Durable per-index rebuild lifecycle (`VECTOR_REBUILD_STATE`, ADR 0031 Slice 7/8).
///
/// Every long-running phase carries a resume cursor (subject keys / page keys as `Storable` bytes)
/// so each `*_step` honors the bounded-execution contract. `Sampling.candidates` accumulates a
/// bounded distinct candidate pool of native stored rows ([`RebuildCandidate`]), then `Training`
/// refines `nlist` canonical-f32 centroids from it with deterministic k-means-lite before they are
/// written to `IVF_CENTROIDS` on the transition to `Building` (ADR 0031 Slice 8). The combined
/// durable `Training` value (`candidates + centroids`) is bounded by `MAX_REBUILD_STATE_BYTES`; the
/// candidate pool is sized to reserve room for the centroids and encoding overhead inside that
/// envelope. `Cleaning`/`Aborting` carry the `nlist` they must tear down because `publish`
/// overwrites `def.nlist`.
#[derive(Clone, Debug, Default, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum VectorRebuildStateRecord {
    #[default]
    Idle,
    Sampling {
        target_index_version: u64,
        nlist: u32,
        sample_limit: u32,
        cursor: Option<SubjectScanCursor>,
        subjects_scanned: u64,
        candidates: Vec<RebuildCandidate>,
    },
    Training {
        target_index_version: u64,
        nlist: u32,
        sample_limit: u32,
        iteration: u32,
        candidates: Vec<RebuildCandidate>,
        centroids: Vec<Vec<u8>>,
    },
    Building {
        target_index_version: u64,
        nlist: u32,
        cursor: Option<SubjectScanCursor>,
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
        subject_cursor: Option<SubjectScanCursor>,
        page_cursor: Option<Vec<u8>>,
    },
    Aborting {
        target_index_version: u64,
        target_nlist: u32,
        subject_cursor: Option<SubjectScanCursor>,
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

/// Consumer scope bound to a durable physical subject-map scan cursor.
///
/// A cursor from one consumer must never be replayed by another consumer: the scope is persisted
/// beside the exact bytes emitted by the LHM owner and is checked before any physical slot read.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum SubjectScanScope {
    Detach {
        shard_id: u32,
    },
    Sampling {
        index_id: u32,
        target_index_version: u64,
    },
    Building {
        index_id: u32,
        target_index_version: u64,
    },
    Cleaning {
        index_id: u32,
        target_index_version: u64,
    },
    Aborting {
        index_id: u32,
        target_index_version: u64,
    },
}

/// Versioned, scope-bound envelope for the LHM's upgrade-stable [`ScanCursor`] bytes.
#[derive(Clone, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub struct SubjectScanCursor {
    version: u8,
    scope: SubjectScanScope,
    cursor: Vec<u8>,
    done: bool,
}

impl SubjectScanCursor {
    pub const VERSION: u8 = 1;

    pub fn from_lhm(scope: SubjectScanScope, cursor: ScanCursor) -> Self {
        Self {
            version: Self::VERSION,
            scope,
            cursor: cursor.encode().to_vec(),
            done: false,
        }
    }

    /// Durable marker used by teardown phases after the subject scan reaches EOF.
    pub fn done(scope: SubjectScanScope) -> Self {
        Self {
            version: Self::VERSION,
            scope,
            cursor: Vec::new(),
            done: true,
        }
    }

    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Encodes this envelope for the detach cursor's opaque `resume_key` field.
    pub fn encode_bytes(&self) -> Vec<u8> {
        Encode!(&self).expect("encode SubjectScanCursor")
    }

    /// Decodes and validates a detach cursor before the owner reads any slots.
    pub fn decode_bytes(
        expected_scope: SubjectScanScope,
        bytes: &[u8],
    ) -> Result<Self, SubjectScanCursorError> {
        let cursor: Self = Decode!(bytes, Self).map_err(|_| SubjectScanCursorError::Malformed)?;
        cursor.validate(expected_scope)?;
        Ok(cursor)
    }

    pub fn validate(&self, expected_scope: SubjectScanScope) -> Result<(), SubjectScanCursorError> {
        if self.version != Self::VERSION {
            return Err(SubjectScanCursorError::VersionMismatch);
        }
        if self.scope != expected_scope {
            return Err(SubjectScanCursorError::ScopeMismatch);
        }
        if self.done {
            return Ok(());
        }
        // Keep the owner cursor opaque here. The final V1 owner accepts only its exact cursor
        // encoding; malformed or stale bytes fail before any physical slot read.
        ScanCursor::decode(&self.cursor).map_err(SubjectScanCursorError::from)?;
        Ok(())
    }

    pub fn lhm_cursor(&self) -> Result<ScanCursor, SubjectScanCursorError> {
        ScanCursor::decode(&self.cursor).map_err(SubjectScanCursorError::from)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubjectScanCursorError {
    Malformed,
    VersionMismatch,
    ScopeMismatch,
    Lhm(ScanError),
}

impl From<ScanError> for SubjectScanCursorError {
    fn from(error: ScanError) -> Self {
        Self::Lhm(error)
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
    use ic_stable_linear_hash_map::StableLinearHashMap;
    use ic_stable_structures::VectorMemory;

    fn subject_scan_cursor_fixture(scope: SubjectScanScope) -> SubjectScanCursor {
        let subjects = StableLinearHashMap::<SubjectKey, FixedSubjectMapEntry, VectorMemory>::new_with_hash_seed(
            VectorMemory::default(),
            0x5eed,
        )
        .expect("fixture subject map");
        SubjectScanCursor::from_lhm(scope, subjects.scan_start().expect("fixture scan cursor"))
    }

    #[test]
    fn subject_key_exact_bytes_schema_and_roundtrip() {
        let key = SubjectKey::new(
            0x0102_0304,
            VectorSubject::Vertex {
                shard_id: ShardId::new(0x0506_0708),
                vertex_id: 0x090a_0b0c,
            },
        );
        let expected = [
            0x01,
            0x02,
            0x03,
            0x04,
            SUBJECT_TAG_VERTEX,
            0x05,
            0x06,
            0x07,
            0x08,
            0x09,
            0x0a,
            0x0b,
            0x0c,
        ];
        let bytes = key.to_bytes();

        assert_eq!(SubjectKey::BOUND.max_size(), expected.len() as u32);
        assert!(SubjectKey::BOUND.is_fixed_size());
        assert_eq!(SubjectKey::KEY_STORAGE_ID, *b"GLEAPH-SUBKEY-01");
        assert_eq!(SubjectKey::KEY_ROUTING_ID, *b"GLEAPH-SUBRTE-01");
        assert_eq!(bytes.as_ref(), expected);
        assert_eq!(key.stable_hash_bytes().as_ref(), expected);
        assert_eq!(SubjectKey::from_bytes(Cow::Borrowed(&expected)), key);

        let lower_index = SubjectKey::new(
            key.index_id - 1,
            VectorSubject::Vertex {
                shard_id: ShardId::new(u32::MAX),
                vertex_id: u32::MAX,
            },
        );
        assert!(lower_index.to_array() < key.to_array());
    }

    #[test]
    fn page_key_storable_roundtrip_and_order() {
        let a = PageKey::new(1, 1, 0, 0);
        let b = PageKey::new(1, 1, 0, 1);
        assert!(a.to_array() < b.to_array());
        assert_eq!(PageKey::from_bytes(a.to_bytes()), a);
    }

    #[test]
    fn def_storable_exact_asymmetric_bytes_and_roundtrip() {
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding: VectorEncoding::I8,
            dims: 0x0201,
            metric: VectorMetric::Cosine,
            nlist: 0x0605_0403,
            active_index_version: 0x0e0d_0c0b_0a09_0807,
            stride_bytes: 0x1211_100f,
            pad_stride_bytes: 0x1615_1413,
            meta_stride_bytes: 0x1a19_1817,
            run_capacity: 0x1e1d_1c1b,
            max_page_bytes: 0x2221_201f,
            slots_per_page: 0x2625_2423,
        };
        let expected = [
            0x00, 0x01, 0x01, 0x02, 0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
            0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19,
            0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26,
        ];

        assert_eq!(
            VectorIndexDef::BOUND.max_size(),
            VECTOR_INDEX_DEF_BYTES as u32
        );
        assert!(VectorIndexDef::BOUND.is_fixed_size());
        assert_eq!(VectorIndexDef::VALUE_STORAGE_ID, *b"GLEAPH-VECDEF-01");
        assert_eq!(def.to_bytes().as_ref(), expected);
        assert_eq!(VectorIndexDef::from_bytes(Cow::Borrowed(&expected)), def);
    }

    #[test]
    fn fixed_subject_entry_exact_bytes_schema_and_roundtrip() {
        let exact_entry = FixedSubjectMapEntry {
            stamp: 0x0807_0605_0403_0201,
            deleted: true,
            slot: Some(SlotRef {
                index_version: 0x0c0b_0a09,
                partition_id: 0x100f_0e0d,
                page_id: 0x1413_1211,
                slot: 0x1817_1615,
            }),
            shadow_slot: Some(SlotRef {
                index_version: 0x1c1b_1a19,
                partition_id: 0x201f_1e1d,
                page_id: 0x2423_2221,
                slot: 0x2827_2625,
            }),
        };
        let expected = [
            0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x07, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19, 0x1a, 0x1b,
            0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26, 0x27, 0x28,
        ];

        assert_eq!(
            FixedSubjectMapEntry::BOUND.max_size(),
            expected.len() as u32
        );
        assert!(FixedSubjectMapEntry::BOUND.is_fixed_size());
        assert_eq!(FixedSubjectMapEntry::VALUE_STORAGE_ID, *b"GLEAPH-SUBVAL-01");
        assert_eq!(exact_entry.to_bytes().as_ref(), expected);
        assert_eq!(
            FixedSubjectMapEntry::from_bytes(Cow::Borrowed(&expected)),
            exact_entry
        );

        // Flags, including zero-valued Some payloads, remain byte-identical to the existing layout.
        for entry in [
            // A zero-valued SlotRef must round-trip as Some (the flags byte, not the payload,
            // records presence).
            FixedSubjectMapEntry {
                stamp: 1,
                deleted: false,
                slot: Some(SlotRef {
                    index_version: 0,
                    partition_id: 0,
                    page_id: 0,
                    slot: 0,
                }),
                shadow_slot: Some(SlotRef {
                    index_version: 0,
                    partition_id: 0,
                    page_id: 0,
                    slot: 0,
                }),
            },
            FixedSubjectMapEntry {
                stamp: 0,
                deleted: true,
                slot: None,
                shadow_slot: None,
            },
            FixedSubjectMapEntry {
                stamp: u64::MAX,
                deleted: false,
                slot: Some(SlotRef {
                    index_version: u32::MAX,
                    partition_id: u32::MAX,
                    page_id: u32::MAX,
                    slot: u32::MAX,
                }),
                shadow_slot: None,
            },
        ] {
            let bytes = entry.to_bytes();
            assert_eq!(bytes.len(), 41, "encoded subject entry must be 41 bytes");
            assert_eq!(FixedSubjectMapEntry::from_bytes(bytes), entry);
        }
    }

    #[test]
    fn deleted_subject_key_storable_roundtrip_and_order() {
        let subject = SubjectKey::new(
            7,
            VectorSubject::Vertex {
                shard_id: ShardId::new(2),
                vertex_id: 42,
            },
        );
        let key = DeletedSubjectKey::new(ShardId::new(1), 9, subject);
        assert_eq!(DeletedSubjectKey::from_bytes(key.to_bytes()), key);
        // Ordering groups by shard, then stamp, then subject.
        let same_shard_lower_stamp = DeletedSubjectKey::new(ShardId::new(1), 8, subject);
        let higher_shard = DeletedSubjectKey::new(ShardId::new(2), 0, subject);
        assert!(same_shard_lower_stamp.to_array() < key.to_array());
        assert!(key.to_array() < higher_shard.to_array());
    }

    #[test]
    fn rebuild_state_record_storable_roundtrip() {
        for state in [
            VectorRebuildStateRecord::Idle,
            VectorRebuildStateRecord::Sampling {
                target_index_version: 2,
                nlist: 8,
                sample_limit: 1024,
                cursor: Some(subject_scan_cursor_fixture(SubjectScanScope::Sampling {
                    index_id: 1,
                    target_index_version: 2,
                })),
                subjects_scanned: 17,
                candidates: vec![
                    RebuildCandidate {
                        stored: vec![0u8; 16],
                        aux: [0u8; 8],
                    },
                    RebuildCandidate {
                        stored: vec![1u8; 16],
                        aux: [7u8; 8],
                    },
                ],
            },
            VectorRebuildStateRecord::Training {
                target_index_version: 2,
                nlist: 2,
                sample_limit: 1024,
                iteration: 3,
                candidates: vec![
                    RebuildCandidate {
                        stored: vec![0u8; 16],
                        aux: [0u8; 8],
                    },
                    RebuildCandidate {
                        stored: vec![1u8; 16],
                        aux: [7u8; 8],
                    },
                    RebuildCandidate {
                        stored: vec![2u8; 16],
                        aux: [9u8; 8],
                    },
                ],
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
                subject_cursor: Some(subject_scan_cursor_fixture(SubjectScanScope::Cleaning {
                    index_id: 1,
                    target_index_version: 2,
                })),
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
        };
        let shadow = SlotRef {
            index_version: 2,
            partition_id: 5,
            page_id: 0,
            slot: 0,
        };
        let entry = FixedSubjectMapEntry {
            stamp: 1,
            deleted: false,
            slot: Some(active),
            shadow_slot: Some(shadow),
        };
        assert_eq!(entry.current_slot_for(1), Some(active));
        assert_eq!(entry.current_slot_for(2), Some(shadow));
        assert_eq!(entry.current_slot_for(3), None);
    }
}
