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
const VECTOR_INDEX_DEF_BYTES: usize = 59;
const VECTOR_INDEX_DEF_STORAGE_ID: [u8; 16] = *b"GLEAPH-VECDEF-03";

/// `VectorIndexDef` shape tags (`levels` field). `levels = 1` is the flat behavior and the default
/// for lazily created defs; `levels = 2` activates the coarse/leaf hierarchy (ADR 0064 §9).
pub(crate) const LEVELS_FLAT: u8 = 1;
pub(crate) const LEVELS_TWO: u8 = 2;

/// `PartitionKey` level tag occupying the key's dedicated byte. Coarse keys (tag `0`) sort before
/// all leaf keys (tag `1`) within one `(index_id, index_version)` prefix, so a teardown can
/// range-delete both levels with a single `(index_id, version)` prefix scan.
///
/// Tags `2`/`3` (Phase-0 Slice 8) are **not generations**: they address companion records of the
/// leaf generation inside the same hash map — the per-partition sealed-page table (`2`) and the
/// slab free-list sentinel (`3`). All map access is exact-key, so the extra tags need no ordering
/// property beyond staying distinct from `0`/`1`.
pub(crate) const PARTITION_LEVEL_COARSE: u8 = 0;
pub(crate) const PARTITION_LEVEL_LEAF: u8 = 1;
/// Sealed-page-table chunk base: chunk `i` of a partition's table lives at level
/// `PARTITION_LEVEL_PAGE_TABLE_BASE + i` (`i < MAX_PAGE_TABLE_CHUNKS`).
pub(crate) const PARTITION_LEVEL_PAGE_TABLE_BASE: u8 = 2;
/// Maximum sealed-page-table chunks per partition. The chunk index is packed into the key's
/// `partition_id` high bits (`(partition << 16) | chunk`), giving 65,536 chunks x 3 entries =
/// 196,608 sealed pages per partition before the fail-closed cap.
pub(crate) const MAX_PAGE_TABLE_CHUNKS: u32 = 200;
/// Fixed entry capacity of one [`PageTableChunk`].
pub(crate) const ENTRIES_PER_TABLE_CHUNK: usize = 3;

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

/// `(index_id, index_version, level, partition_id)` key for `VECTOR_PARTITION_HEADS` and
/// `IVF_CENTROIDS`.
///
/// The `level` byte splits the key space of one index generation into coarse (`0`) and leaf (`1`)
/// ranges. Heads and pages exist for **leaves only**; coarse keys hold the level-0 centroid set of
/// a two-level index. Leaf `partition_id` packs the hierarchy path as `coarse_id * nlist_fine +
/// fine_id`, so a subtree is the contiguous id range `[c * f, (c + 1) * f)`.
///
/// The 17-byte layout (level byte between version and partition id) is a breaking change over the
/// retired 16-byte layout; reinstall required.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PartitionKey {
    pub index_id: u32,
    pub index_version: u64,
    /// Level tag: [`PARTITION_LEVEL_COARSE`], [`PARTITION_LEVEL_LEAF`], a sealed-table chunk tag
    /// (`PARTITION_LEVEL_PAGE_TABLE_BASE..`), or the free-list sentinel
    /// ([`PARTITION_LEVEL_FREE_LIST`]).
    pub level: u8,
    pub partition_id: u32,
}

impl PartitionKey {
    /// Leaf partition key — heads, pages, and flat/two-level leaf centroids. This is the historical
    /// `PartitionKey::new` shape, so every leaf-addressed call site is source-compatible.
    pub const fn new(index_id: u32, index_version: u64, partition_id: u32) -> Self {
        Self {
            index_id,
            index_version,
            level: PARTITION_LEVEL_LEAF,
            partition_id,
        }
    }

    /// Coarse (level-0) centroid key of a two-level index generation.
    pub const fn coarse(index_id: u32, index_version: u64, partition_id: u32) -> Self {
        Self {
            index_id,
            index_version,
            level: PARTITION_LEVEL_COARSE,
            partition_id,
        }
    }

    /// Sealed-page-table chunk key of one leaf partition (Slice 8): chunk `chunk` of the table.
    /// Same collection as the heads; chunks own the positional page id → slab block seq mapping
    /// of every sealed (non-mutable) page of the partition. Panics fail-closed when `chunk`
    /// exceeds [`MAX_PAGE_TABLE_CHUNKS`] or overflows the level byte range.
    /// Sealed-page-table chunk key (Slice 8). The chunk index packs into the `partition_id`
    /// field's high bits (`(real_partition << 16) | chunk`), so one level tag (`2`) addresses
    /// every chunk of every leaf partition while the level-tag space stays reserved for future
    /// companion kinds. Fail-closed when either half exceeds 16 bits.
    pub(crate) fn page_table_chunk(
        index_id: u32,
        index_version: u64,
        partition_id: u32,
        chunk: u32,
    ) -> Self {
        assert!(
            partition_id < (1 << 16) && chunk < (1 << 16),
            "page-table key overflow: partition {partition_id} chunk {chunk}"
        );
        Self {
            index_id,
            index_version,
            level: PARTITION_LEVEL_PAGE_TABLE_BASE,
            partition_id: (partition_id << 16) | chunk,
        }
    }

    fn to_array(self) -> [u8; 17] {
        let mut out = [0u8; 17];
        out[0..4].copy_from_slice(&self.index_id.to_be_bytes());
        out[4..12].copy_from_slice(&self.index_version.to_be_bytes());
        out[12] = self.level;
        out[13..17].copy_from_slice(&self.partition_id.to_be_bytes());
        out
    }
}

impl Storable for PartitionKey {
    const BOUND: Bound = Bound::Bounded {
        max_size: 17,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Vec::from(self.to_array()))
    }

    fn into_bytes(self) -> Vec<u8> {
        Vec::from(self.to_array())
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let mut raw = [0u8; 17];
        raw.copy_from_slice(bytes.as_ref());
        Self {
            index_id: u32::from_be_bytes([raw[0], raw[1], raw[2], raw[3]]),
            index_version: u64::from_be_bytes([
                raw[4], raw[5], raw[6], raw[7], raw[8], raw[9], raw[10], raw[11],
            ]),
            level: raw[12],
            partition_id: u32::from_be_bytes([raw[13], raw[14], raw[15], raw[16]]),
        }
    }
}

impl StableHashKey for PartitionKey {
    const KEY_STORAGE_ID: [u8; 16] = *b"GLEAPH-PARTKEY-1";
    const KEY_ROUTING_ID: [u8; 16] = *b"GLEAPH-PARTRTE-1";
    type HashBytes<'a>
        = [u8; 17]
    where
        Self: 'a;

    fn stable_hash_bytes(&self) -> Self::HashBytes<'_> {
        self.to_array()
    }
}

/// `(index_id, index_version, partition_id, page_id)` key for `VECTOR_PAGE_META` (ADR 0032).
/// Also persists as the typed resume cursor of the slab-compaction driver state (plan 0278).
#[derive(
    Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, CandidType, Serialize, Deserialize,
)]
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
    /// Coarse partition count (level-0 centroid count). For a flat index (`levels = 1`) this is
    /// also the leaf count.
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
    /// Hierarchy depth of the active generation: `1` = flat, `2` = coarse/leaf.
    pub levels: u8,
    /// Branching factor of the level-1 subtrees (`1` for flat). Leaves per coarse subtree form the
    /// contiguous id range `[c * nlist_fine, (c + 1) * nlist_fine)`.
    pub nlist_fine: u32,
    /// Two-tier precision code tier of the **active** generation (Slice 6): `false` = rows carry
    /// no code segment and search is exact; `true` = rows carry a 1-bit RaBitQ code segment used
    /// to accelerate the first-stage scan (the advertised result quality stays the original
    /// tier). Chosen at rebuild start; the public `VectorEncoding` never changes.
    pub code_tier: bool,
    /// Frozen per-row code-segment width of the active generation (`0` when
    /// [`Self::code_tier`] is off; otherwise `[code_aux 8B][codes ceil(P/64)*8B]` with
    /// `P = next_pow2(dims)` — see [`Self::canonical_code_stride_bytes`]). Frozen so stored pages
    /// stay decodable even if a future encoder changes the derivation.
    pub code_stride_bytes: u32,
    /// Rotation seed frozen at definition creation. The same seeded rotation (randomized
    /// Walsh–Hadamard + seeded sign flips over the zero-padded `P` domain) is applied at data
    /// write time and query time, so codes and queries always live in the same rotated space.
    pub rotation_seed: u64,
}

impl VectorIndexDef {
    /// Zero-padded rotation domain for `dims`: the randomized Walsh–Hadamard transform needs a
    /// power-of-two length. Distances are preserved exactly by zero-padding (the padded domain's
    /// orthogonal transform leaves `‖x − y‖` invariant), and every coordinate — real or padded —
    /// participates in the code.
    pub(crate) fn code_padded_dims(dims: u16) -> u32 {
        (dims as u32).max(1).next_power_of_two()
    }

    /// Canonical v1 (1-bit RaBitQ) per-row code-segment width for `dims`:
    /// `[code_aux 8B][codes ceil(P/64)*8B]` over the power-of-two rotation domain
    /// (`P = next_pow2(dims)`), e.g. d1536 → P=2048 → 264 B, d768 → P=1024 → 136 B.
    pub(crate) fn canonical_code_stride_bytes(dims: u16) -> u32 {
        let words = Self::code_padded_dims(dims).div_ceil(64);
        8 + words * 8
    }

    /// Deterministic rotation seed derived from the index id (splitmix64 finalizer). Frozen into
    /// the def at creation so data writes and queries always share one rotated space without
    /// depending on any randomness source at runtime.
    pub(crate) fn rotation_seed_for(index_id: u32) -> u64 {
        let mut z = (index_id as u64).wrapping_add(0x9E37_79B9_7F4A_7C15);
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Whether scanned rows of this generation carry a code segment.
    pub fn has_code_tier(&self) -> bool {
        self.code_tier
    }
    /// Whether the active generation uses the two-level (coarse/leaf) hierarchy.
    pub fn is_two_level(&self) -> bool {
        self.levels == LEVELS_TWO
    }

    /// Total leaf partition count: `nlist * nlist_fine`. Heads and pages are indexed by leaf ids
    /// `0..leaf_count()` in both shapes.
    pub fn leaf_count(&self) -> u32 {
        self.nlist.saturating_mul(if self.nlist_fine == 0 {
            1
        } else {
            self.nlist_fine
        })
    }

    /// The exclusive leaf-id end of coarse subtree `c`: `[c * f, (c + 1) * f)`.
    pub fn subtree_range(&self, coarse: u32) -> std::ops::Range<u32> {
        let f = if self.nlist_fine == 0 {
            1
        } else {
            self.nlist_fine
        };
        let start = coarse.saturating_mul(f);
        start..start.saturating_add(f)
    }
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
        // Slice 5 (levels=2): shape fields extend the retired 41-byte layout to 46 bytes.
        out[41] = self.levels;
        out[42..46].copy_from_slice(&self.nlist_fine.to_le_bytes());
        // Slice 6 (two-tier precision): `code_tier` (1 B) + frozen code width (4 B LE) + the
        // rotation seed (8 B LE) extend the layout to 59 bytes. The storage id bumped to
        // GLEAPH-VECDEF-03; reinstall required.
        out[46] = self.code_tier as u8;
        out[47..51].copy_from_slice(&self.code_stride_bytes.to_le_bytes());
        out[51..59].copy_from_slice(&self.rotation_seed.to_le_bytes());
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
        let dims = u16::from_le_bytes(b[2..4].try_into().expect("dims"));
        Self {
            kind: VectorIndexKind::from_u8(b[0]).expect("valid kind"),
            encoding: VectorEncoding::from_u8(b[1]).expect("valid encoding"),
            dims,
            metric: VectorMetric::from_u8(b[4]).expect("valid metric"),
            nlist: u32::from_le_bytes(b[5..9].try_into().expect("nlist")),
            active_index_version: u64::from_le_bytes(b[9..17].try_into().expect("version")),
            stride_bytes: u32::from_le_bytes(b[17..21].try_into().expect("stride")),
            pad_stride_bytes: u32::from_le_bytes(b[21..25].try_into().expect("pad")),
            meta_stride_bytes: u32::from_le_bytes(b[25..29].try_into().expect("meta")),
            run_capacity: u32::from_le_bytes(b[29..33].try_into().expect("run")),
            max_page_bytes: u32::from_le_bytes(b[33..37].try_into().expect("max_page")),
            slots_per_page: u32::from_le_bytes(b[37..41].try_into().expect("slots")),
            levels: {
                let level = b[41];
                assert!(
                    level == LEVELS_FLAT || level == LEVELS_TWO,
                    "unknown def levels tag {level}"
                );
                level
            },
            nlist_fine: u32::from_le_bytes(b[42..46].try_into().expect("nlist_fine")),
            code_tier: {
                let flag = b[46];
                assert!(flag <= 1, "unknown def code_tier tag {flag}");
                flag == 1
            },
            code_stride_bytes: {
                // Fail-closed consistency: the frozen width must match the v1 derivation exactly
                // (canonical when the tier is on, zero when it is off). A future encoder changes
                // the derivation together with this validation, never silently.
                let canonical = Self::canonical_code_stride_bytes(dims);
                let stride = u32::from_le_bytes(b[47..51].try_into().expect("code_stride"));
                assert_eq!(
                    stride,
                    if b[46] == 1 { canonical } else { 0 },
                    "def code_stride_bytes disagrees with the v1 derivation"
                );
                stride
            },
            rotation_seed: u64::from_le_bytes(b[51..59].try_into().expect("rotation_seed")),
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
/// `graph_watermark` is the highest graph→vector acked stamp; `router_watermark` is the contiguous
/// Router frontier published for an exact attached shard. A deleted subject-map entry with
/// `stamp <= min(both)` for its shard is unreachable (no stale replay can arrive) and is eligible
/// for a bounded GC step. Graph-only lanes without a Router marker remain outside this liveness
/// contract.
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

/// Per-partition head: page chain bounds + per-partition counters (`VECTOR_PARTITION_HEADS`).
/// `live_len`/`page_count` serve the documented O(`nlist`) partition-health check without full
/// scans. `next_page_id` is the number of pages this partition has created (dense positional page
/// ids, so it always equals `page_count`); `mutable_page` is the positional id of the tail page.
///
/// **Slab addressing + hot-path mirrors (Phase-0 Slice 8).** The former MemoryId 10 page directory
/// is replaced by arithmetic addressing: every slab page occupies one uniform block
/// (`BLOCK_LEN`, see `page_store`) at `SLAB_HEADER_SIZE + seq × BLOCK_LEN`. This head owns its
/// partition's page chain: sealed pages live in the companion [`PageTable`] record
/// (`{seq, row_count, live_count, block_bound}` per positional id) while the single mutable tail
/// page's state is mirrored here as scalars — `mutable_rows/mutable_live/mutable_bound/
/// mutable_run_count/mutable_last_shard` plus the tail page's `mutable_seq` block number. An
/// append therefore performs exactly two durable map ops (head get + head insert): the roll
/// decision and the write layout derive from these scalars and from the generation def, never
/// from a stable read.
///
/// `block_bound` is the conservative scalar skip bound `M = max‖row‖` of a page (monotone:
/// tombstones never lower it; cosine generations still maintain it but search ignores it because
/// unit-normalized rows make the bound powerless). Fixed-width `Storable` (56 bytes; breaking
/// layout change, reinstall required).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct PartitionHead {
    pub mutable_page: u64,
    pub page_count: u64,
    pub live_len: u64,
    /// Number of pages created by this partition so far (== `page_count`).
    pub next_page_id: u64,
    /// Rows written into the mutable tail page (`0` ⇔ no mutable page ⇔ `page_count == 0`).
    pub mutable_rows: u32,
    /// Live rows in the mutable tail page.
    pub mutable_live: u32,
    /// Block bound `M = max‖row‖` over the mutable tail page's rows (0.0 when empty).
    pub mutable_bound: f32,
    /// Mirror of the mutable page's on-slab header `run_count`.
    pub mutable_run_count: u32,
    /// Shard of the mutable page's last run (run-extension decision without a slab read).
    pub mutable_last_shard: u32,
    /// Slab block sequence of the mutable tail page.
    pub mutable_seq: u32,
}

impl Eq for PartitionHead {}

/// The value type of `VECTOR_PARTITION_HEADS` (MemoryId 9, Slice 8): one collection carries the
/// leaf partition heads, their fixed-width sealed-page-table chunks, and the slab free-list
/// sentinel record, discriminated by this tag. The linear hash map requires a **fixed-width**
/// value, so every member is encoded into the common [`RECORD_PAYLOAD_WIDTH`] payload (zero
/// padded) behind a one-byte tag. The three key ranges are disjoint by construction; decoding a
/// tag that disagrees with its key's level is corruption and fails closed at the typed accessors.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum PartitionHeadRecord {
    Head(PartitionHead),
    Table(PageTableChunk),
}

impl PartitionHeadRecord {
    const TAG_HEAD: u8 = 0;
    const TAG_TABLE: u8 = 1;
    /// Common encoded payload width: the head is the widest member (the slab free list lives in
    /// intrusive block headers anchored by the compaction-state record, not here).
    const PAYLOAD_WIDTH: usize = PageTableChunk::ENCODED_LEN;

    // Invariant: `PageTableChunk::ENCODED_LEN >= 56` (the head width), so the head payload
    // zero-pads into the chunk-sized slot.

    fn tag(&self) -> u8 {
        match self {
            Self::Head(_) => Self::TAG_HEAD,
            Self::Table(_) => Self::TAG_TABLE,
        }
    }

    fn encode_payload(&self) -> Vec<u8> {
        let mut payload = match self {
            Self::Head(head) => head.to_bytes().into_owned(),
            Self::Table(table) => table.encode(),
        };
        assert!(payload.len() <= Self::PAYLOAD_WIDTH);
        payload.resize(Self::PAYLOAD_WIDTH, 0);
        payload
    }

    fn decode_payload(tag: u8, payload: &[u8]) -> Self {
        match tag {
            Self::TAG_HEAD => Self::Head(PartitionHead::from_bytes(Cow::Owned(
                payload[..56].to_vec(),
            ))),
            Self::TAG_TABLE => Self::Table(PageTableChunk::decode(payload)),
            other => panic!("PartitionHeadRecord: unknown tag {other}"),
        }
    }
}

impl Storable for PartitionHeadRecord {
    const BOUND: Bound = Bound::Bounded {
        max_size: (1 + PartitionHeadRecord::PAYLOAD_WIDTH) as u32,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = Vec::with_capacity(1 + Self::PAYLOAD_WIDTH);
        out.push(self.tag());
        out.extend_from_slice(&self.encode_payload());
        Cow::Owned(out)
    }

    fn into_bytes(self) -> Vec<u8> {
        let mut out = Vec::with_capacity(1 + Self::PAYLOAD_WIDTH);
        out.push(self.tag());
        out.extend_from_slice(&self.encode_payload());
        out
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        assert!(
            bytes.len() == 1 + Self::PAYLOAD_WIDTH,
            "PartitionHeadRecord: corrupt width {}",
            bytes.len()
        );
        let (tag, payload) = bytes.as_ref().split_first().expect("empty record");
        Self::decode_payload(*tag, payload)
    }
}

impl StableMapValue for PartitionHeadRecord {
    const VALUE_STORAGE_ID: [u8; 16] = *b"GLEAPH-PARTVAL-1";
}

impl Storable for PartitionHead {
    const BOUND: Bound = Bound::Bounded {
        max_size: 56,
        is_fixed_size: true,
    };

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        let mut out = [0u8; 56];
        out[0..8].copy_from_slice(&self.mutable_page.to_le_bytes());
        out[8..16].copy_from_slice(&self.page_count.to_le_bytes());
        out[16..24].copy_from_slice(&self.live_len.to_le_bytes());
        out[24..32].copy_from_slice(&self.next_page_id.to_le_bytes());
        out[32..36].copy_from_slice(&self.mutable_rows.to_le_bytes());
        out[36..40].copy_from_slice(&self.mutable_live.to_le_bytes());
        out[40..44].copy_from_slice(&self.mutable_bound.to_bits().to_le_bytes());
        out[44..48].copy_from_slice(&self.mutable_run_count.to_le_bytes());
        out[48..52].copy_from_slice(&self.mutable_last_shard.to_le_bytes());
        out[52..56].copy_from_slice(&self.mutable_seq.to_le_bytes());
        Cow::Owned(out.to_vec())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.to_bytes().into_owned()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        let b = bytes.as_ref();
        assert_eq!(b.len(), 56, "PartitionHead expects exactly 56 bytes");
        Self {
            mutable_page: u64::from_le_bytes(b[0..8].try_into().expect("mutable_page")),
            page_count: u64::from_le_bytes(b[8..16].try_into().expect("page_count")),
            live_len: u64::from_le_bytes(b[16..24].try_into().expect("live_len")),
            next_page_id: u64::from_le_bytes(b[24..32].try_into().expect("next_page_id")),
            mutable_rows: u32::from_le_bytes(b[32..36].try_into().expect("mutable_rows")),
            mutable_live: u32::from_le_bytes(b[36..40].try_into().expect("mutable_live")),
            mutable_bound: f32::from_bits(u32::from_le_bytes(
                b[40..44].try_into().expect("mutable_bound"),
            )),
            mutable_run_count: u32::from_le_bytes(b[44..48].try_into().expect("mutable_run_count")),
            mutable_last_shard: u32::from_le_bytes(
                b[48..52].try_into().expect("mutable_last_shard"),
            ),
            mutable_seq: u32::from_le_bytes(b[52..56].try_into().expect("mutable_seq")),
        }
    }
}

/// One sealed (non-mutable) page in a partition's [`PageTableChunk`] (Slice 8). `positional page
/// id` equals the global entry index across the partition's chunks, so
/// [`crate::records::SlotRef`] addressing is unchanged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PageTableEntry {
    /// Slab block sequence: physical base is `SLAB_HEADER_SIZE + seq × BLOCK_LEN`.
    pub seq: u32,
    /// Written rows (sealed pages may be short when a run-table roll closed them early).
    pub row_count: u32,
    /// Live (non-tombstoned) rows; tombstones are `row_count − live_count`.
    pub live_count: u32,
    /// Scalar skip bound `M = max‖row‖` over all written rows (tombstones included).
    pub block_bound: f32,
}

impl PageTableEntry {
    const ENCODED_LEN: usize = 16;

    fn to_array(self) -> [u8; Self::ENCODED_LEN] {
        let mut out = [0u8; Self::ENCODED_LEN];
        out[0..4].copy_from_slice(&self.seq.to_le_bytes());
        out[4..8].copy_from_slice(&self.row_count.to_le_bytes());
        out[8..12].copy_from_slice(&self.live_count.to_le_bytes());
        out[12..16].copy_from_slice(&self.block_bound.to_bits().to_le_bytes());
        out
    }

    fn from_array(raw: &[u8]) -> Self {
        let bound = f32::from_bits(u32::from_le_bytes(raw[12..16].try_into().expect("bound")));
        if !bound.is_finite() || bound < 0.0 {
            panic!("PageTableEntry: corrupt block bound {bound}");
        }
        Self {
            seq: u32::from_le_bytes(raw[0..4].try_into().expect("seq")),
            row_count: u32::from_le_bytes(raw[4..8].try_into().expect("row_count")),
            live_count: u32::from_le_bytes(raw[8..12].try_into().expect("live_count")),
            block_bound: bound,
        }
    }
}

/// One fixed-capacity chunk of a leaf partition's sealed-page table (Phase-0 Slice 8), stored
/// under the `PARTITION_LEVEL_PAGE_TABLE_BASE + chunk_index` key of `VECTOR_PARTITION_HEADS`.
/// The linear hash map requires fixed-width values, so a partition's table is a list of up to
/// [`MAX_PAGE_TABLE_CHUNKS`] chunks of [`ENTRIES_PER_TABLE_CHUNK`] entries each (6,400 sealed
/// pages per partition; growth past the cap fails closed). Global positional page id =
/// `chunk_index * ENTRIES_PER_TABLE_CHUNK + slot`. Versioned fixed-width codec.
#[derive(Clone, Debug, PartialEq, Default)]
pub(crate) struct PageTableChunk {
    /// Populated entries (`len <= ENTRIES_PER_TABLE_CHUNK`; only the last chunk may be partial).
    pub entries: Vec<PageTableEntry>,
}

impl PageTableChunk {
    const MAGIC: [u8; 3] = *b"PTC";
    const VERSION: u8 = 1;
    /// Natural encoded width: magic + version + len + `ENTRIES_PER_TABLE_CHUNK` × 16 B entries.
    const ENCODED_LEN: usize = 8 + ENTRIES_PER_TABLE_CHUNK * PageTableEntry::ENCODED_LEN;

    fn encode(&self) -> Vec<u8> {
        assert!(self.entries.len() <= ENTRIES_PER_TABLE_CHUNK);
        let mut out = vec![0u8; Self::ENCODED_LEN];
        out[..3].copy_from_slice(&Self::MAGIC);
        out[3] = Self::VERSION;
        out[4..8].copy_from_slice(&(self.entries.len() as u32).to_le_bytes());
        for (i, entry) in self.entries.iter().enumerate() {
            out[8 + i * 16..8 + (i + 1) * 16].copy_from_slice(&entry.to_array());
        }
        out
    }

    fn decode(bytes: &[u8]) -> Self {
        assert!(
            bytes.len() >= Self::ENCODED_LEN && bytes[..3] == Self::MAGIC,
            "PageTableChunk: bad magic/width"
        );
        assert_eq!(bytes[3], Self::VERSION, "PageTableChunk: unknown version");
        let len = u32::from_le_bytes(bytes[4..8].try_into().expect("len")) as usize;
        assert!(
            len <= ENTRIES_PER_TABLE_CHUNK,
            "PageTableChunk: entry count {len} exceeds chunk capacity"
        );
        let entries = bytes[8..8 + len * PageTableEntry::ENCODED_LEN]
            .as_chunks::<{ PageTableEntry::ENCODED_LEN }>()
            .0
            .iter()
            .map(|raw| PageTableEntry::from_array(raw))
            .collect();
        Self { entries }
    }
}

/// One frozen rebuild candidate: a live row's native stored bytes plus its row-meta aux (the `I8`
/// scale; zero for `F32`). The pool snapshots Sampling-time values and stays immutable into
/// `Training` even though dual-write mutations keep mutating live rows mid-rebuild.
///
/// This is a transient heap value and the fixed-width row format of the durable rebuild-pool
/// region (`facade/stable/rebuild_pool.rs`); it is **not** part of the durable lifecycle record —
/// that record carries only a `pool_len` scalar.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct RebuildCandidate {
    /// The row's stored vector bytes (pad-stride wide, trailing pad zeroed), verbatim from the
    /// page store.
    pub stored: Vec<u8>,
    /// The row's page-store aux (the `I8` quantization scale; zero for `F32`).
    pub aux: [u8; 8],
}

/// Durable per-index rebuild lifecycle (`VECTOR_REBUILD_STATE`, ADR 0031 Slice 7/8).
///
/// Every long-running phase carries a resume cursor (subject keys / page keys as `Storable` bytes)
/// so each `*_step` honors the bounded-execution contract. `Sampling` accumulates a bounded
/// distinct candidate pool of native stored rows ([`RebuildCandidate`]) in the dedicated raw
/// rebuild-pool region (`VECTOR_REBUILD_POOL`, MemoryId 18; ADR 0033 implementation) — this record
/// carries only the `pool_len` scalar — then `Training` refines `nlist` canonical-f32 centroids
/// from that pool with deterministic k-means-lite before they are written to `IVF_CENTROIDS` on
/// the transition to `Building` (ADR 0031 Slice 8). The pool rows and the `Training` centroid work
/// area never enter this durable record; every step reads only the rows it needs from the pool
/// region. `Cleaning`/`Aborting` carry the `nlist` they must tear down because `publish`
/// overwrites `def.nlist`; their key teardown is a `(index_id, version)` prefix range deletion, so
/// they need no shape fields.
///
/// **Two-level shape (Slice 5).** The flat lifecycle is unchanged: `Sampling → Training →
/// Building`. When a rebuild starts with `fine_nlist = Some(f)` the target generation has
/// `levels = 2` and training splits into `TrainCoarse` (k-means over the whole pool at the coarse
/// count `nlist`, one iteration per step exactly like flat `Training`) followed by `TrainFine` —
/// one bounded k-means-lite iteration per step over the current subtree's members, advancing
/// `coarse_cursor` after each subtree converges and its leaf centroids are written. All phase
/// variants that decide or publish the target shape carry `levels` + `nlist_fine`; the variants
/// that write or publish shadow **pages** (`Building`/`ReadyToPublish`) additionally carry
/// `code_tier`, because the code tier changes the shadow generation's row geometry while the
/// training phases touch only original-tier pool rows.
///
/// **Durable row codec (Phase-0 Slice 7).** The row is a versioned custom binary format (see
/// [`VectorRebuildStateRecord::decode_rebuild_state`]), not Candid: the ingest hot path decodes this
/// record once per op while a rebuild row exists (`rebuild_mutation_mode`), and Candid's
/// self-describing type table for this wide enum made that decode cost ~425K instructions per op.
/// Unknown magic/version/tag fails closed; there is no migration reader (fresh install).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum VectorRebuildStateRecord {
    #[default]
    Idle,
    Sampling {
        target_index_version: u64,
        nlist: u32,
        sample_limit: u32,
        cursor: Option<SubjectScanCursor>,
        subjects_scanned: u64,
        /// Distinct candidates accumulated in the durable pool region so far.
        pool_len: u32,
        levels: u8,
        nlist_fine: u32,
    },
    Training {
        target_index_version: u64,
        nlist: u32,
        sample_limit: u32,
        iteration: u32,
        /// Frozen candidate count inherited from `Sampling` (validated against the pool region
        /// header on resume).
        pool_len: u32,
        levels: u8,
        nlist_fine: u32,
    },
    /// Two-level only: k-means over the whole candidate pool at the coarse count (`nlist`),
    /// identical convergence rule and one-iteration-per-step budget as flat `Training`.
    TrainCoarse {
        target_index_version: u64,
        nlist: u32,
        nlist_fine: u32,
        sample_limit: u32,
        iteration: u32,
        pool_len: u32,
    },
    /// Two-level only: per-subtree fine k-means jobs in coarse-id order. Each step performs at
    /// most one k-means-lite iteration over the current subtree's members; on convergence the
    /// subtree's `nlist_fine` leaf centroids are written to `IVF_CENTROIDS` and `coarse_cursor`
    /// advances.
    TrainFine {
        target_index_version: u64,
        nlist: u32,
        nlist_fine: u32,
        sample_limit: u32,
        /// Next coarse subtree id to train (`0..nlist`).
        coarse_cursor: u32,
        /// Iteration counter within the current subtree's job.
        iteration: u32,
        pool_len: u32,
    },
    Building {
        target_index_version: u64,
        nlist: u32,
        cursor: Option<SubjectScanCursor>,
        subjects_processed: u64,
        levels: u8,
        nlist_fine: u32,
        /// Code tier of the shadow generation being built (Slice 6): shadow page appends and the
        /// publish flip derive their row geometry from this flag.
        code_tier: bool,
    },
    ReadyToPublish {
        target_index_version: u64,
        nlist: u32,
        levels: u8,
        nlist_fine: u32,
        /// Code tier to flip into the definition on publish (Slice 6).
        code_tier: bool,
    },
    Cleaning {
        old_version: u64,
        old_nlist: u32,
        /// Shape of the OLD generation being torn down (`levels`/`nlist_fine` as of publish).
        old_levels: u8,
        old_nlist_fine: u32,
        target_index_version: u64,
        subject_cursor: Option<SubjectScanCursor>,
        page_cursor: Option<Vec<u8>>,
    },
    Aborting {
        target_index_version: u64,
        target_nlist: u32,
        /// Shape of the shadow generation being torn down.
        target_levels: u8,
        target_nlist_fine: u32,
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
        Cow::Owned(self.encode_rebuild_state())
    }

    fn into_bytes(self) -> Vec<u8> {
        self.encode_rebuild_state()
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Self::decode_rebuild_state(bytes.as_ref()).expect("decode VectorRebuildStateRecord")
    }
}

/// Fail-closed decode errors for the [`VectorRebuildStateRecord`] binary codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RebuildStateCodecError {
    /// Leading magic byte is not the rebuild-state codec marker.
    Magic,
    /// Unsupported format version (fail-closed; fresh install, never migrated).
    Version,
    /// Unknown record variant tag or cursor scope tag.
    Tag,
    /// Buffer ended before the record was complete.
    UnexpectedEof,
    /// Bytes remain after a complete record.
    TrailingBytes,
    /// A flag byte was not 0/1, or the `Failed` reason was not UTF-8.
    Payload,
}

/// Cursor scope tags, following [`SubjectScanScope`] declaration order.
const CURSOR_SCOPE_DETACH: u8 = 0;
const CURSOR_SCOPE_SAMPLING: u8 = 1;
const CURSOR_SCOPE_BUILDING: u8 = 2;
const CURSOR_SCOPE_CLEANING: u8 = 3;
const CURSOR_SCOPE_ABORTING: u8 = 4;

fn put_u32(out: &mut Vec<u8>, value: u32) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_u64(out: &mut Vec<u8>, value: u64) {
    out.extend_from_slice(&value.to_le_bytes());
}

fn put_flag(out: &mut Vec<u8>, value: bool) {
    out.push(u8::from(value));
}

fn put_blob(out: &mut Vec<u8>, bytes: &[u8]) {
    let len = u32::try_from(bytes.len()).expect("blob length fits u32");
    put_u32(out, len);
    out.extend_from_slice(bytes);
}

/// Encodes the scope-bound cursor envelope structurally. The LHM owner bytes stay opaque here —
/// they are validated by `ScanCursor::decode` when the consumer resumes, exactly as with Candid.
fn put_cursor(out: &mut Vec<u8>, cursor: &SubjectScanCursor) {
    out.push(cursor.version);
    match &cursor.scope {
        SubjectScanScope::Detach { shard_id } => {
            out.push(CURSOR_SCOPE_DETACH);
            put_u32(out, *shard_id);
        }
        SubjectScanScope::Sampling {
            index_id,
            target_index_version,
        } => {
            out.push(CURSOR_SCOPE_SAMPLING);
            put_u32(out, *index_id);
            put_u64(out, *target_index_version);
        }
        SubjectScanScope::Building {
            index_id,
            target_index_version,
        } => {
            out.push(CURSOR_SCOPE_BUILDING);
            put_u32(out, *index_id);
            put_u64(out, *target_index_version);
        }
        SubjectScanScope::Cleaning {
            index_id,
            target_index_version,
        } => {
            out.push(CURSOR_SCOPE_CLEANING);
            put_u32(out, *index_id);
            put_u64(out, *target_index_version);
        }
        SubjectScanScope::Aborting {
            index_id,
            target_index_version,
        } => {
            out.push(CURSOR_SCOPE_ABORTING);
            put_u32(out, *index_id);
            put_u64(out, *target_index_version);
        }
    }
    put_blob(out, &cursor.cursor);
    put_flag(out, cursor.done);
}

/// Straight-line little-endian reader over one encoded rebuild-state row. Every short read,
/// invalid flag, or leftover byte fails closed instead of being guessed away.
struct RebuildStateReader<'a> {
    src: &'a [u8],
    pos: usize,
}

impl<'a> RebuildStateReader<'a> {
    fn new(src: &'a [u8]) -> Self {
        Self { src, pos: 0 }
    }

    fn take(&mut self, len: usize) -> Result<&'a [u8], RebuildStateCodecError> {
        let end = self
            .pos
            .checked_add(len)
            .filter(|&end| end <= self.src.len())
            .ok_or(RebuildStateCodecError::UnexpectedEof)?;
        let slice = &self.src[self.pos..end];
        self.pos = end;
        Ok(slice)
    }

    fn le<const N: usize>(&mut self) -> Result<[u8; N], RebuildStateCodecError> {
        let mut bytes = [0u8; N];
        bytes.copy_from_slice(self.take(N)?);
        Ok(bytes)
    }

    fn u8(&mut self) -> Result<u8, RebuildStateCodecError> {
        Ok(self.le::<1>()?[0])
    }

    fn u32(&mut self) -> Result<u32, RebuildStateCodecError> {
        Ok(u32::from_le_bytes(self.le()?))
    }

    fn u64(&mut self) -> Result<u64, RebuildStateCodecError> {
        Ok(u64::from_le_bytes(self.le()?))
    }

    fn flag(&mut self) -> Result<bool, RebuildStateCodecError> {
        match self.u8()? {
            0 => Ok(false),
            1 => Ok(true),
            _ => Err(RebuildStateCodecError::Payload),
        }
    }

    fn blob(&mut self) -> Result<&'a [u8], RebuildStateCodecError> {
        let len = self.u32()? as usize;
        self.take(len)
    }

    fn opt_blob(&mut self) -> Result<Option<Vec<u8>>, RebuildStateCodecError> {
        if self.flag()? {
            Ok(Some(self.blob()?.to_vec()))
        } else {
            Ok(None)
        }
    }

    fn opt_cursor(&mut self) -> Result<Option<SubjectScanCursor>, RebuildStateCodecError> {
        if !self.flag()? {
            return Ok(None);
        }
        let version = self.u8()?;
        if version != SubjectScanCursor::VERSION {
            return Err(RebuildStateCodecError::Version);
        }
        let scope = match self.u8()? {
            CURSOR_SCOPE_DETACH => SubjectScanScope::Detach {
                shard_id: self.u32()?,
            },
            CURSOR_SCOPE_SAMPLING => SubjectScanScope::Sampling {
                index_id: self.u32()?,
                target_index_version: self.u64()?,
            },
            CURSOR_SCOPE_BUILDING => SubjectScanScope::Building {
                index_id: self.u32()?,
                target_index_version: self.u64()?,
            },
            CURSOR_SCOPE_CLEANING => SubjectScanScope::Cleaning {
                index_id: self.u32()?,
                target_index_version: self.u64()?,
            },
            CURSOR_SCOPE_ABORTING => SubjectScanScope::Aborting {
                index_id: self.u32()?,
                target_index_version: self.u64()?,
            },
            _ => return Err(RebuildStateCodecError::Tag),
        };
        let cursor = self.blob()?.to_vec();
        let done = self.flag()?;
        Ok(Some(SubjectScanCursor {
            version,
            scope,
            cursor,
            done,
        }))
    }

    fn finish(&self) -> Result<(), RebuildStateCodecError> {
        if self.pos == self.src.len() {
            Ok(())
        } else {
            Err(RebuildStateCodecError::TrailingBytes)
        }
    }
}

impl VectorRebuildStateRecord {
    const CODEC_MAGIC: u8 = b'R';
    const CODEC_VERSION: u8 = 1;

    // Variant tags follow enum declaration order.
    const TAG_IDLE: u8 = 0;
    const TAG_SAMPLING: u8 = 1;
    const TAG_TRAINING: u8 = 2;
    const TAG_TRAIN_COARSE: u8 = 3;
    const TAG_TRAIN_FINE: u8 = 4;
    const TAG_BUILDING: u8 = 5;
    const TAG_READY_TO_PUBLISH: u8 = 6;
    const TAG_CLEANING: u8 = 7;
    const TAG_ABORTING: u8 = 8;
    const TAG_FAILED: u8 = 9;

    /// Durable row layout (`VECTOR_REBUILD_STATE`, MemoryId 12), Phase-0 Slice 7:
    ///
    /// ```text
    /// [magic b'R'][format version u8 = 1][variant tag u8][variant payload…]
    /// ```
    ///
    /// Variant payload fields are written in declaration order as fixed-width little-endian
    /// scalars; `Option<_>` and `bool` are 0/1 flag bytes; variable parts (`Vec<u8>`, `String`)
    /// are u32-length-prefixed byte strings. The embedded `SubjectScanCursor` re-encodes
    /// structurally (version + scope tag + fixed-width scope fields + length-prefixed opaque LHM
    /// owner bytes + done flag). Unknown magic/version/tags, truncation, trailing bytes, non-0/1
    /// flags, or non-UTF-8 reasons fail closed. Candid was replaced because its self-describing
    /// type table (~500B per ~15B payload) taxed every hot-path op read ~425K instructions while a
    /// rebuild row exists (see design/index/vector-index.md design principle 5); fresh install,
    /// no migration reader.
    fn encode_rebuild_state(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(48);
        out.push(Self::CODEC_MAGIC);
        out.push(Self::CODEC_VERSION);
        match self {
            Self::Idle => out.push(Self::TAG_IDLE),
            Self::Sampling {
                target_index_version,
                nlist,
                sample_limit,
                cursor,
                subjects_scanned,
                pool_len,
                levels,
                nlist_fine,
            } => {
                out.push(Self::TAG_SAMPLING);
                put_u64(&mut out, *target_index_version);
                put_u32(&mut out, *nlist);
                put_u32(&mut out, *sample_limit);
                put_flag(&mut out, cursor.is_some());
                if let Some(cursor) = cursor {
                    put_cursor(&mut out, cursor);
                }
                put_u64(&mut out, *subjects_scanned);
                put_u32(&mut out, *pool_len);
                out.push(*levels);
                put_u32(&mut out, *nlist_fine);
            }
            Self::Training {
                target_index_version,
                nlist,
                sample_limit,
                iteration,
                pool_len,
                levels,
                nlist_fine,
            } => {
                out.push(Self::TAG_TRAINING);
                put_u64(&mut out, *target_index_version);
                put_u32(&mut out, *nlist);
                put_u32(&mut out, *sample_limit);
                put_u32(&mut out, *iteration);
                put_u32(&mut out, *pool_len);
                out.push(*levels);
                put_u32(&mut out, *nlist_fine);
            }
            Self::TrainCoarse {
                target_index_version,
                nlist,
                nlist_fine,
                sample_limit,
                iteration,
                pool_len,
            } => {
                out.push(Self::TAG_TRAIN_COARSE);
                put_u64(&mut out, *target_index_version);
                put_u32(&mut out, *nlist);
                put_u32(&mut out, *nlist_fine);
                put_u32(&mut out, *sample_limit);
                put_u32(&mut out, *iteration);
                put_u32(&mut out, *pool_len);
            }
            Self::TrainFine {
                target_index_version,
                nlist,
                nlist_fine,
                sample_limit,
                coarse_cursor,
                iteration,
                pool_len,
            } => {
                out.push(Self::TAG_TRAIN_FINE);
                put_u64(&mut out, *target_index_version);
                put_u32(&mut out, *nlist);
                put_u32(&mut out, *nlist_fine);
                put_u32(&mut out, *sample_limit);
                put_u32(&mut out, *coarse_cursor);
                put_u32(&mut out, *iteration);
                put_u32(&mut out, *pool_len);
            }
            Self::Building {
                target_index_version,
                nlist,
                cursor,
                subjects_processed,
                levels,
                nlist_fine,
                code_tier,
            } => {
                out.push(Self::TAG_BUILDING);
                put_u64(&mut out, *target_index_version);
                put_u32(&mut out, *nlist);
                put_flag(&mut out, cursor.is_some());
                if let Some(cursor) = cursor {
                    put_cursor(&mut out, cursor);
                }
                put_u64(&mut out, *subjects_processed);
                out.push(*levels);
                put_u32(&mut out, *nlist_fine);
                put_flag(&mut out, *code_tier);
            }
            Self::ReadyToPublish {
                target_index_version,
                nlist,
                levels,
                nlist_fine,
                code_tier,
            } => {
                out.push(Self::TAG_READY_TO_PUBLISH);
                put_u64(&mut out, *target_index_version);
                put_u32(&mut out, *nlist);
                out.push(*levels);
                put_u32(&mut out, *nlist_fine);
                put_flag(&mut out, *code_tier);
            }
            Self::Cleaning {
                old_version,
                old_nlist,
                old_levels,
                old_nlist_fine,
                target_index_version,
                subject_cursor,
                page_cursor,
            } => {
                out.push(Self::TAG_CLEANING);
                put_u64(&mut out, *old_version);
                put_u32(&mut out, *old_nlist);
                out.push(*old_levels);
                put_u32(&mut out, *old_nlist_fine);
                put_u64(&mut out, *target_index_version);
                put_flag(&mut out, subject_cursor.is_some());
                if let Some(cursor) = subject_cursor {
                    put_cursor(&mut out, cursor);
                }
                put_flag(&mut out, page_cursor.is_some());
                if let Some(page_key) = page_cursor {
                    put_blob(&mut out, page_key);
                }
            }
            Self::Aborting {
                target_index_version,
                target_nlist,
                target_levels,
                target_nlist_fine,
                subject_cursor,
                page_cursor,
            } => {
                out.push(Self::TAG_ABORTING);
                put_u64(&mut out, *target_index_version);
                put_u32(&mut out, *target_nlist);
                out.push(*target_levels);
                put_u32(&mut out, *target_nlist_fine);
                put_flag(&mut out, subject_cursor.is_some());
                if let Some(cursor) = subject_cursor {
                    put_cursor(&mut out, cursor);
                }
                put_flag(&mut out, page_cursor.is_some());
                if let Some(page_key) = page_cursor {
                    put_blob(&mut out, page_key);
                }
            }
            Self::Failed {
                target_index_version,
                reason,
            } => {
                out.push(Self::TAG_FAILED);
                put_u64(&mut out, *target_index_version);
                put_blob(&mut out, reason.as_bytes());
            }
        }
        out
    }

    /// Decodes a durable rebuild-state row, failing closed on any unknown or malformed input
    /// (see the layout contract on [`Self::encode_rebuild_state`]).
    fn decode_rebuild_state(bytes: &[u8]) -> Result<Self, RebuildStateCodecError> {
        let mut reader = RebuildStateReader::new(bytes);
        if reader.u8()? != Self::CODEC_MAGIC {
            return Err(RebuildStateCodecError::Magic);
        }
        if reader.u8()? != Self::CODEC_VERSION {
            return Err(RebuildStateCodecError::Version);
        }
        let state = match reader.u8()? {
            Self::TAG_IDLE => Self::Idle,
            Self::TAG_SAMPLING => Self::Sampling {
                target_index_version: reader.u64()?,
                nlist: reader.u32()?,
                sample_limit: reader.u32()?,
                cursor: reader.opt_cursor()?,
                subjects_scanned: reader.u64()?,
                pool_len: reader.u32()?,
                levels: reader.u8()?,
                nlist_fine: reader.u32()?,
            },
            Self::TAG_TRAINING => Self::Training {
                target_index_version: reader.u64()?,
                nlist: reader.u32()?,
                sample_limit: reader.u32()?,
                iteration: reader.u32()?,
                pool_len: reader.u32()?,
                levels: reader.u8()?,
                nlist_fine: reader.u32()?,
            },
            Self::TAG_TRAIN_COARSE => Self::TrainCoarse {
                target_index_version: reader.u64()?,
                nlist: reader.u32()?,
                nlist_fine: reader.u32()?,
                sample_limit: reader.u32()?,
                iteration: reader.u32()?,
                pool_len: reader.u32()?,
            },
            Self::TAG_TRAIN_FINE => Self::TrainFine {
                target_index_version: reader.u64()?,
                nlist: reader.u32()?,
                nlist_fine: reader.u32()?,
                sample_limit: reader.u32()?,
                coarse_cursor: reader.u32()?,
                iteration: reader.u32()?,
                pool_len: reader.u32()?,
            },
            Self::TAG_BUILDING => Self::Building {
                target_index_version: reader.u64()?,
                nlist: reader.u32()?,
                cursor: reader.opt_cursor()?,
                subjects_processed: reader.u64()?,
                levels: reader.u8()?,
                nlist_fine: reader.u32()?,
                code_tier: reader.flag()?,
            },
            Self::TAG_READY_TO_PUBLISH => Self::ReadyToPublish {
                target_index_version: reader.u64()?,
                nlist: reader.u32()?,
                levels: reader.u8()?,
                nlist_fine: reader.u32()?,
                code_tier: reader.flag()?,
            },
            Self::TAG_CLEANING => Self::Cleaning {
                old_version: reader.u64()?,
                old_nlist: reader.u32()?,
                old_levels: reader.u8()?,
                old_nlist_fine: reader.u32()?,
                target_index_version: reader.u64()?,
                subject_cursor: reader.opt_cursor()?,
                page_cursor: reader.opt_blob()?,
            },
            Self::TAG_ABORTING => Self::Aborting {
                target_index_version: reader.u64()?,
                target_nlist: reader.u32()?,
                target_levels: reader.u8()?,
                target_nlist_fine: reader.u32()?,
                subject_cursor: reader.opt_cursor()?,
                page_cursor: reader.opt_blob()?,
            },
            Self::TAG_FAILED => Self::Failed {
                target_index_version: reader.u64()?,
                reason: std::str::from_utf8(reader.blob()?)
                    .map_err(|_| RebuildStateCodecError::Payload)?
                    .to_string(),
            },
            _ => return Err(RebuildStateCodecError::Tag),
        };
        reader.finish()?;
        Ok(state)
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
/// The bytes are exactly `VectorRebuildStateRecord::into_bytes()` (the versioned custom binary
/// codec, not Candid), so the on-disk format is identical to storing the record directly. The
/// wrapper lets the step persist share a single encode with the store call (ADR 0031 Slice 7/8).
/// `rebuild_state_of` decodes them back into a [`VectorRebuildStateRecord`].
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
/// Follows the same verbatim-bytes wrapper pattern as [`RawRebuildState`]: these bytes are exactly
/// the Candid encoding of the kernel `VectorMaintenanceState`, so the on-disk format is identical to
/// storing the type directly while keeping the `Storable` impl local (the kernel type is foreign to
/// this crate). The maintenance step encodes once and persists these bytes; `maintenance_state_of`
/// decodes them back. Unlike the rebuild-state row this type stays Candid — it is not read once per
/// ingest op.
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

/// Durable global slab-compaction lifecycle (`VECTOR_SLAB_COMPACTION_STATE`, MemoryId 11; plan
/// 0278).
///
/// The `VECTOR_ROW_SLAB` is one global allocation domain, so compaction state is a single canister
/// record, not a per-index map. `Compacting` carries exactly three facts, all reopen-visible so an
/// interrupted driver resumes fail-closed from its persisted cursors:
///
/// - `write_cursor` — the next free byte of the dense prefix (copy destination);
/// - `range_end` — exclusive upper bound of the snapshot source range (`occupied_tail` at start);
///   pages appended after that land above it and are never touched;
/// - `scan_cursor` — last examined `PageKey` of the current meta-map lap (`None` restarts the lap),
///   so each bounded step resumes without rescanning.
///
/// `pages_moved` is cumulative progress bookkeeping for the status surface only. The record is
/// cleared (`Idle`) when finalize rewinds `occupied_tail` once.
#[derive(Clone, Copy, Debug, PartialEq, Eq, CandidType, Serialize, Deserialize)]
pub enum VectorSlabCompactionState {
    Idle {
        /// Intrusive LIFO free-block chain anchor (Slice 8): each free block begins with a
        /// `u32` next-pointer in its own first bytes; `None` = no reusable holes below the tail.
        free_head: Option<u32>,
    },
    Compacting {
        write_cursor: u64,
        range_end: u64,
        scan_cursor: Option<PageKey>,
        pages_moved: u64,
        /// Same anchor, carried through compaction so finalize can sanitize stale nodes.
        free_head: Option<u32>,
    },
}

impl Default for VectorSlabCompactionState {
    fn default() -> Self {
        Self::Idle { free_head: None }
    }
}

impl Storable for VectorSlabCompactionState {
    const BOUND: Bound = Bound::Unbounded;

    fn to_bytes(&self) -> Cow<'_, [u8]> {
        Cow::Owned(Encode!(self).expect("encode VectorSlabCompactionState"))
    }

    fn into_bytes(self) -> Vec<u8> {
        Encode!(&self).expect("encode VectorSlabCompactionState")
    }

    fn from_bytes(bytes: Cow<'_, [u8]>) -> Self {
        Decode!(bytes.as_ref(), VectorSlabCompactionState)
            .expect("decode VectorSlabCompactionState")
    }
}

/// Shared definition fixtures for test modules across the crate (math units, store tests, bench
/// seeding). Production construction paths stay in `mutation.rs`; this module only assembles
/// in-memory defs and never touches stable state.
#[cfg(any(test, feature = "canbench"))]
pub(crate) mod test_support {
    use super::*;

    /// A minimal valid `ivf_flat` def with the code tier **on** at the canonical v1 shape:
    /// `code_stride_bytes = 8 + ceil(P/64)*8` for `P = next_pow2(dims)` and the deterministic
    /// `rotation_seed_for(index_id)` seed. Geometry fields (`slots_per_page`) reflect the tier-on
    /// page capacity so fixtures that exercise page layout stay consistent with the real
    /// derivation.
    pub(crate) fn tier_def(dims: u16, encoding: VectorEncoding) -> VectorIndexDef {
        let stride_bytes = match encoding {
            VectorEncoding::F32 => u32::from(dims) * 4,
            VectorEncoding::I8 => u32::from(dims),
        };
        let pad_stride_bytes = stride_bytes.div_ceil(16) * 16;
        let code_stride_bytes = VectorIndexDef::canonical_code_stride_bytes(dims);
        let slots_per_page = ic_stable_vector_page_store::layout::PageLayout::max_capacity_for(
            64 * 1024,
            pad_stride_bytes,
            4,
            1,
            code_stride_bytes,
        )
        .expect("tier fixture fits a page");
        VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding,
            dims,
            metric: VectorMetric::L2Squared,
            nlist: 1,
            active_index_version: 1,
            stride_bytes,
            pad_stride_bytes,
            meta_stride_bytes: 4,
            run_capacity: 1,
            max_page_bytes: 64 * 1024,
            slots_per_page,
            levels: LEVELS_FLAT,
            nlist_fine: 1,
            code_tier: true,
            code_stride_bytes,
            rotation_seed: VectorIndexDef::rotation_seed_for(1),
        }
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
            levels: LEVELS_TWO,
            nlist_fine: 0x2a29_2827,
            code_tier: true,
            code_stride_bytes: VectorIndexDef::canonical_code_stride_bytes(0x0201),
            rotation_seed: 0x3231_2f2e_2d2c_2b2a,
        };
        // The retired 46-byte prefix is unchanged; `code_tier` (1 B) + `code_stride_bytes` (4 B
        // LE) + `rotation_seed` (8 B LE) extend the layout to 59 bytes under GLEAPH-VECDEF-03.
        let mut expected = vec![
            0x00, 0x01, 0x01, 0x02, 0x01, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b,
            0x0c, 0x0d, 0x0e, 0x0f, 0x10, 0x11, 0x12, 0x13, 0x14, 0x15, 0x16, 0x17, 0x18, 0x19,
            0x1a, 0x1b, 0x1c, 0x1d, 0x1e, 0x1f, 0x20, 0x21, 0x22, 0x23, 0x24, 0x25, 0x26,
        ];
        expected.push(LEVELS_TWO);
        expected.extend_from_slice(&0x2a29_2827u32.to_le_bytes());
        expected.push(1);
        expected
            .extend_from_slice(&VectorIndexDef::canonical_code_stride_bytes(0x0201).to_le_bytes());
        expected.extend_from_slice(&0x3231_2f2e_2d2c_2b2au64.to_le_bytes());

        assert_eq!(
            VectorIndexDef::BOUND.max_size(),
            VECTOR_INDEX_DEF_BYTES as u32
        );
        assert!(VectorIndexDef::BOUND.is_fixed_size());
        assert_eq!(VectorIndexDef::VALUE_STORAGE_ID, *b"GLEAPH-VECDEF-03");
        assert_eq!(def.to_bytes().as_ref(), expected.as_slice());
        assert_eq!(VectorIndexDef::from_bytes(Cow::Borrowed(&expected)), def);
    }

    #[test]
    fn canonical_code_stride_matches_v1_shape() {
        // The WHT rotation needs a power-of-two domain, so `P = next_pow2(dims)`.
        // d1536: P = 2048, 32 words -> aux 8 + 256 = 264. d768 -> P = 1024 -> 136.
        // A non-power-of-two dims pads up: d17 -> P = 32, one word -> 16. d1 -> one word -> 16.
        assert_eq!(
            VectorIndexDef::canonical_code_stride_bytes(1536),
            8 + 32 * 8
        );
        assert_eq!(VectorIndexDef::canonical_code_stride_bytes(768), 8 + 16 * 8);
        assert_eq!(VectorIndexDef::canonical_code_stride_bytes(17), 8 + 8);
        assert_eq!(VectorIndexDef::canonical_code_stride_bytes(1), 8 + 8);
    }

    #[test]
    fn partition_key_level_layout_and_order() {
        let leaf = PartitionKey::new(0x0102_0304, 0x0506_0708_090a_0b0c, 0x0d0e_0f10);
        assert_eq!(leaf.level, PARTITION_LEVEL_LEAF);
        let coarse = PartitionKey::coarse(leaf.index_id, leaf.index_version, leaf.partition_id);
        assert_eq!(coarse.level, PARTITION_LEVEL_COARSE);

        let expected = [
            0x01,
            0x02,
            0x03,
            0x04,
            0x05,
            0x06,
            0x07,
            0x08,
            0x09,
            0x0a,
            0x0b,
            0x0c,
            PARTITION_LEVEL_LEAF,
            0x0d,
            0x0e,
            0x0f,
            0x10,
        ];
        assert_eq!(PartitionKey::BOUND.max_size(), 17);
        assert!(PartitionKey::BOUND.is_fixed_size());
        assert_eq!(PartitionKey::KEY_STORAGE_ID, *b"GLEAPH-PARTKEY-1");
        assert_eq!(PartitionKey::KEY_ROUTING_ID, *b"GLEAPH-PARTRTE-1");
        assert_eq!(leaf.to_array(), expected);
        assert_eq!(PartitionKey::from_bytes(Cow::Borrowed(&expected)), leaf);

        // Coarse keys sort before all leaf keys of the same generation; both share the
        // `(index_id, version)` prefix a teardown range-deletes.
        assert!(coarse.to_array() < leaf.to_array());
        let next_version_leaf = PartitionKey::new(leaf.index_id, leaf.index_version + 1, 0);
        assert!(leaf.to_array() < next_version_leaf.to_array());

        assert!(def_is_two_level_shape());
    }

    fn def_is_two_level_shape() -> bool {
        let def = VectorIndexDef {
            kind: VectorIndexKind::IvfFlat,
            encoding: VectorEncoding::F32,
            dims: 4,
            metric: VectorMetric::L2Squared,
            nlist: 16,
            active_index_version: 2,
            stride_bytes: 16,
            pad_stride_bytes: 16,
            meta_stride_bytes: 4,
            run_capacity: 1,
            max_page_bytes: 64 * 1024,
            slots_per_page: 1024,
            levels: LEVELS_TWO,
            nlist_fine: 16,
            code_tier: false,
            code_stride_bytes: 0,
            rotation_seed: 0,
        };
        let flat = VectorIndexDef {
            levels: LEVELS_FLAT,
            nlist_fine: 1,
            ..def
        };
        assert_eq!(flat.leaf_count(), 16);
        assert_eq!(def.leaf_count(), 256);
        assert_eq!(def.subtree_range(3), 48..64);
        def.is_two_level() && !flat.is_two_level()
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
                pool_len: 2,
                levels: LEVELS_FLAT,
                nlist_fine: 1,
            },
            VectorRebuildStateRecord::Training {
                target_index_version: 2,
                nlist: 8,
                sample_limit: 1024,
                iteration: 3,
                pool_len: 5,
                levels: LEVELS_FLAT,
                nlist_fine: 1,
            },
            VectorRebuildStateRecord::TrainCoarse {
                target_index_version: 2,
                nlist: 8,
                nlist_fine: 4,
                sample_limit: 1024,
                iteration: 1,
                pool_len: 5,
            },
            VectorRebuildStateRecord::TrainFine {
                target_index_version: 2,
                nlist: 8,
                nlist_fine: 4,
                sample_limit: 1024,
                coarse_cursor: 3,
                iteration: 2,
                pool_len: 5,
            },
            VectorRebuildStateRecord::Building {
                target_index_version: 2,
                nlist: 8,
                cursor: None,
                subjects_processed: 42,
                levels: LEVELS_TWO,
                nlist_fine: 4,
                code_tier: true,
            },
            VectorRebuildStateRecord::ReadyToPublish {
                target_index_version: 2,
                nlist: 8,
                levels: LEVELS_TWO,
                nlist_fine: 4,
                code_tier: true,
            },
            VectorRebuildStateRecord::Cleaning {
                old_version: 1,
                old_nlist: 1,
                old_levels: LEVELS_FLAT,
                old_nlist_fine: 1,
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
                target_levels: LEVELS_TWO,
                target_nlist_fine: 4,
                subject_cursor: None,
                page_cursor: Some(vec![7]),
            },
            VectorRebuildStateRecord::Failed {
                target_index_version: 2,
                reason: "insufficient live vectors".to_string(),
            },
            // Extended codec coverage (Phase-0 Slice 7): absent/present cursor combinations,
            // done-marker cursors with empty owner bytes, and boundary scalar widths.
            VectorRebuildStateRecord::Sampling {
                target_index_version: u64::MAX,
                nlist: 65_536,
                sample_limit: u32::MAX,
                cursor: None,
                subjects_scanned: u64::MAX,
                pool_len: u32::MAX,
                levels: LEVELS_TWO,
                nlist_fine: 16,
            },
            VectorRebuildStateRecord::Building {
                target_index_version: 9,
                nlist: 64,
                cursor: Some(subject_scan_cursor_fixture(SubjectScanScope::Building {
                    index_id: 3,
                    target_index_version: 9,
                })),
                subjects_processed: u64::MAX,
                levels: LEVELS_FLAT,
                nlist_fine: 1,
                code_tier: false,
            },
            VectorRebuildStateRecord::Cleaning {
                old_version: u64::MAX,
                old_nlist: 256,
                old_levels: LEVELS_TWO,
                old_nlist_fine: 16,
                target_index_version: 7,
                subject_cursor: Some(SubjectScanCursor::done(SubjectScanScope::Cleaning {
                    index_id: 3,
                    target_index_version: 7,
                })),
                page_cursor: Some(vec![0xAB; 24]),
            },
            VectorRebuildStateRecord::Aborting {
                target_index_version: 11,
                target_nlist: 16,
                target_levels: LEVELS_FLAT,
                target_nlist_fine: 1,
                subject_cursor: Some(subject_scan_cursor_fixture(SubjectScanScope::Aborting {
                    index_id: 3,
                    target_index_version: 11,
                })),
                page_cursor: None,
            },
            VectorRebuildStateRecord::Failed {
                target_index_version: 13,
                reason: String::new(),
            },
        ] {
            assert_eq!(
                VectorRebuildStateRecord::from_bytes(state.to_bytes()),
                state
            );
        }
    }

    #[test]
    fn rebuild_state_codec_pins_v1_layout_bytes() {
        // Idle is exactly header (magic + format version) + variant tag.
        assert_eq!(
            VectorRebuildStateRecord::Idle.to_bytes().as_ref(),
            &[b'R', 1, VectorRebuildStateRecord::TAG_IDLE]
        );

        // Field order/width pin for one multi-scalar variant: declaration order, little-endian.
        let training = VectorRebuildStateRecord::Training {
            target_index_version: 2,
            nlist: 8,
            sample_limit: 1024,
            iteration: 3,
            pool_len: 5,
            levels: LEVELS_FLAT,
            nlist_fine: 1,
        };
        let mut expected = vec![b'R', 1, VectorRebuildStateRecord::TAG_TRAINING];
        expected.extend_from_slice(&2u64.to_le_bytes());
        expected.extend_from_slice(&8u32.to_le_bytes());
        expected.extend_from_slice(&1024u32.to_le_bytes());
        expected.extend_from_slice(&3u32.to_le_bytes());
        expected.extend_from_slice(&5u32.to_le_bytes());
        expected.push(LEVELS_FLAT);
        expected.extend_from_slice(&1u32.to_le_bytes());
        assert_eq!(training.to_bytes().as_ref(), &expected);
    }

    #[test]
    fn rebuild_state_codec_fails_closed_on_unknown_or_malformed_rows() {
        let decode_err =
            |bytes: &[u8]| VectorRebuildStateRecord::decode_rebuild_state(bytes).unwrap_err();

        // Unknown magic / format version / variant tag.
        assert_eq!(decode_err(&[b'X', 1, 0]), RebuildStateCodecError::Magic);
        assert_eq!(decode_err(&[b'R', 2, 0]), RebuildStateCodecError::Version);
        assert_eq!(decode_err(&[b'R', 1, 99]), RebuildStateCodecError::Tag);

        // Truncation of a complete row fails closed.
        let training = VectorRebuildStateRecord::Training {
            target_index_version: 2,
            nlist: 8,
            sample_limit: 1024,
            iteration: 3,
            pool_len: 5,
            levels: LEVELS_FLAT,
            nlist_fine: 1,
        }
        .to_bytes()
        .to_vec();
        assert_eq!(
            decode_err(&training[..training.len() - 1]),
            RebuildStateCodecError::UnexpectedEof
        );

        // Trailing bytes after a complete record fail closed.
        let mut trailing = VectorRebuildStateRecord::Idle.to_bytes().to_vec();
        trailing.push(7);
        assert_eq!(decode_err(&trailing), RebuildStateCodecError::TrailingBytes);

        // Non-0/1 flag byte in a bool field (`ReadyToPublish.code_tier`).
        let mut bad_flag = vec![b'R', 1, VectorRebuildStateRecord::TAG_READY_TO_PUBLISH];
        bad_flag.extend_from_slice(&1u64.to_le_bytes());
        bad_flag.extend_from_slice(&1u32.to_le_bytes());
        bad_flag.push(LEVELS_TWO);
        bad_flag.extend_from_slice(&1u32.to_le_bytes());
        bad_flag.push(2);
        assert_eq!(decode_err(&bad_flag), RebuildStateCodecError::Payload);

        // Non-UTF-8 failure reason.
        let mut bad_reason = vec![b'R', 1, VectorRebuildStateRecord::TAG_FAILED];
        bad_reason.extend_from_slice(&1u64.to_le_bytes());
        bad_reason.extend_from_slice(&2u32.to_le_bytes());
        bad_reason.push(0xFF);
        bad_reason.push(0xFE);
        assert_eq!(decode_err(&bad_reason), RebuildStateCodecError::Payload);

        // Embedded cursor envelope: unsupported envelope version and unknown scope tag.
        let mut sampling_head = vec![b'R', 1, VectorRebuildStateRecord::TAG_SAMPLING];
        sampling_head.extend_from_slice(&1u64.to_le_bytes());
        sampling_head.extend_from_slice(&8u32.to_le_bytes());
        sampling_head.extend_from_slice(&16u32.to_le_bytes());
        sampling_head.push(1); // cursor present

        let mut bad_cursor_version = sampling_head.clone();
        bad_cursor_version.push(3); // not SubjectScanCursor::VERSION
        assert_eq!(
            decode_err(&bad_cursor_version),
            RebuildStateCodecError::Version
        );

        let mut bad_scope_tag = sampling_head;
        bad_scope_tag.push(SubjectScanCursor::VERSION);
        bad_scope_tag.push(9); // not a SubjectScanScope tag
        assert_eq!(decode_err(&bad_scope_tag), RebuildStateCodecError::Tag);
    }

    #[test]
    fn rebuild_state_codec_hot_path_rows_stay_bounded() {
        // Native unit tests cannot count wasm instructions; the deterministic cost proxy is the
        // encoded size. Decoding is straight-line parsing over these few hundred bytes with no
        // self-describing type table — orders of magnitude below the ≤5K-instruction budget for
        // `rebuild_state_decode`. The live instruction figures are reported by the focused
        // canbench scopes (`rebuild_state_get` / `rebuild_state_decode`) in Phase-0 verification.
        //
        // Bound composition: ~30B fixed scalars/header, one embedded cursor whose dominant part is
        // the opaque LHM owner bytes (~88B today), and a page-key allowance. The Candid row this
        // codec replaced was 527B for a ~15B payload.
        const HOT_PATH_ROW_BOUND: usize = 256;

        assert_eq!(VectorRebuildStateRecord::Idle.to_bytes().len(), 3);
        let hot_path_rows = [
            // The variants decoded once per ingest op (`rebuild_mutation_mode`): Building /
            // ReadyToPublish / Cleaning. Their only variable parts are system-owned cursors and
            // the page-key bytes, so this bound is data-independent.
            VectorRebuildStateRecord::Building {
                target_index_version: u64::MAX,
                nlist: u32::MAX,
                cursor: Some(subject_scan_cursor_fixture(SubjectScanScope::Building {
                    index_id: u32::MAX,
                    target_index_version: u64::MAX,
                })),
                subjects_processed: u64::MAX,
                levels: LEVELS_TWO,
                nlist_fine: u32::MAX,
                code_tier: true,
            },
            VectorRebuildStateRecord::ReadyToPublish {
                target_index_version: u64::MAX,
                nlist: u32::MAX,
                levels: LEVELS_TWO,
                nlist_fine: u32::MAX,
                code_tier: true,
            },
            VectorRebuildStateRecord::Cleaning {
                old_version: u64::MAX,
                old_nlist: u32::MAX,
                old_levels: LEVELS_TWO,
                old_nlist_fine: u32::MAX,
                target_index_version: u64::MAX,
                subject_cursor: Some(subject_scan_cursor_fixture(SubjectScanScope::Cleaning {
                    index_id: u32::MAX,
                    target_index_version: u64::MAX,
                })),
                page_cursor: Some(vec![0u8; 64]),
            },
        ];
        for state in hot_path_rows {
            assert!(
                state.to_bytes().len() <= HOT_PATH_ROW_BOUND,
                "hot-path rebuild-state row grew beyond the codec bound: {state:?}"
            );
        }
    }

    #[test]
    fn slab_compaction_state_record_storable_roundtrip() {
        for state in [
            VectorSlabCompactionState::Idle { free_head: Some(7) },
            VectorSlabCompactionState::Idle { free_head: None },
            VectorSlabCompactionState::Compacting {
                write_cursor: 0xdead_beef_cafe_f00d,
                range_end: 0x0123_4567_89ab_cdef,
                scan_cursor: Some(PageKey::new(7, 3, 2, 41)),
                pages_moved: 19,
                free_head: None,
            },
            VectorSlabCompactionState::Compacting {
                write_cursor: 32,
                range_end: 32,
                scan_cursor: None,
                pages_moved: 0,
                free_head: Some(3),
            },
        ] {
            assert_eq!(
                VectorSlabCompactionState::from_bytes(state.to_bytes()),
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
