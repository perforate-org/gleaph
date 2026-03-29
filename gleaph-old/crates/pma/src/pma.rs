use crate::property_store::{AbpPropertyStore, AbpSecondaryEqIndex, encode_value};
use crate::vertex_meta_table::{VertexMeta, VertexMetaTable};
use crate::{
    label_index::LabelIndex,
    layout,
    math::{ceil_div, ceil_log2},
    memory::{Memory, MemoryError},
    region_manager::{self, ReservedPersistMeta, ReservedRegionsMeta},
    segment_log::SegmentLog,
};
use candid::CandidType;
use gleaph_algo::GraphView;
use gleaph_types::{
    EdgeEntry, EdgeRecord, EntityType, GleaphError, GraphStats, IndexType, LogEntry, PlannerStats,
    PropertyIndex, PropertyMap, STABLE_MAGIC, STABLE_VERSION, StableHeader, TimestampRange,
    VertexEntry, VertexIdSet, pack_label_and_flags, unpack_edge_flags, unpack_edge_label_id,
};
use rapidhash::RapidHashSet;
use rapidhash::fast::RapidHashMap;
use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::collections::HashSet;
use std::collections::{BTreeMap, BTreeSet};

/// Rich reverse-index entry caching label and edge IDs to avoid redundant lookups.
#[derive(Clone, Copy, Debug)]
pub struct RevEntry {
    pub src: u32,
    pub weight: f32,
    pub timestamp: u64,
    /// Low 24 bits: label identifier from `LabelIndex` (0 = "no label").
    /// High 8 bits: edge flags.
    pub label_and_flags: u32,
    /// PMA edge_id (0 if overlay-only or log-backed).
    pub edge_id: u32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IndexedEdgeMatch {
    pub src: u32,
    pub dst: u32,
    pub label_id: u32,
    pub edge_id: u32,
}

impl RevEntry {
    #[inline]
    pub const fn label_id(&self) -> u32 {
        unpack_edge_label_id(self.label_and_flags)
    }

    #[inline]
    pub const fn flags(&self) -> u8 {
        unpack_edge_flags(self.label_and_flags)
    }

    #[inline]
    pub const fn is_tombstoned(&self) -> bool {
        self.flags() & gleaph_types::EDGE_FLAG_TOMBSTONED != 0
    }

    #[inline]
    pub fn set_label_id(&mut self, label_id: u32) {
        self.label_and_flags = pack_label_and_flags(label_id, self.flags());
    }

    #[inline]
    pub fn set_tombstoned(&mut self, tombstoned: bool) {
        let mut flags = self.flags();
        if tombstoned {
            flags |= gleaph_types::EDGE_FLAG_TOMBSTONED;
        } else {
            flags &= !gleaph_types::EDGE_FLAG_TOMBSTONED;
        }
        self.label_and_flags = pack_label_and_flags(self.label_id(), flags);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum EdgeLocator {
    PmaSlot { slot: u64 },
    LogSlot { seg_id: u32, slot: u32 },
}

#[derive(Clone, Debug, Default, PartialEq)]
struct EdgePropsOverlay {
    src: u32,
    dst: u32,
    label: String,
    props: PropertyMap,
}

#[derive(Clone, Debug, Default, CandidType, Serialize, Deserialize, PartialEq)]
pub struct EdgePropsSnapshot {
    pub edge_id: u32,
    pub src: u32,
    pub dst: u32,
    pub label: String,
    pub props: PropertyMap,
}

pub const UP_H: f64 = 0.75;
pub const UP_0: f64 = 1.0;
pub const LOW_H: f64 = 0.50;
pub const LOW_0: f64 = 0.25;

/// Number of entities to sample when re-estimating selectivity for non-indexed properties.
const SELECTIVITY_SAMPLE_SIZE: usize = 1024;

/// Minimum number of samples containing a property before we trust the estimate.
/// Below this threshold the old estimate is retained.
const MIN_PROPERTY_SAMPLE: usize = 8;

/// Per-property dirty ratio threshold that triggers re-estimation.
/// 10% means: if 10% of entities with this property have been mutated
/// since the last estimation, re-sample.
const DIRTY_RATIO_THRESHOLD: f64 = 0.10;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DebugReadCounters {
    pub edge_label_calls: u64,
    pub edge_record_calls: u64,
    pub is_edge_tombstoned_calls: u64,
}

thread_local! {
    static EDGE_LABEL_CALLS: Cell<u64> = const { Cell::new(0) };
    static EDGE_RECORD_CALLS: Cell<u64> = const { Cell::new(0) };
    static IS_EDGE_TOMBSTONED_CALLS: Cell<u64> = const { Cell::new(0) };
}

#[inline]
fn incr_edge_label_calls() {
    EDGE_LABEL_CALLS.with(|c| c.set(c.get().saturating_add(1)));
}

#[inline]
fn incr_edge_record_calls() {
    EDGE_RECORD_CALLS.with(|c| c.set(c.get().saturating_add(1)));
}

#[inline]
fn incr_is_edge_tombstoned_calls() {
    IS_EDGE_TOMBSTONED_CALLS.with(|c| c.set(c.get().saturating_add(1)));
}

pub fn reset_debug_read_counters() {
    EDGE_LABEL_CALLS.with(|c| c.set(0));
    EDGE_RECORD_CALLS.with(|c| c.set(0));
    IS_EDGE_TOMBSTONED_CALLS.with(|c| c.set(0));
}

pub fn snapshot_debug_read_counters() -> DebugReadCounters {
    DebugReadCounters {
        edge_label_calls: EDGE_LABEL_CALLS.with(Cell::get),
        edge_record_calls: EDGE_RECORD_CALLS.with(Cell::get),
        is_edge_tombstoned_calls: IS_EDGE_TOMBSTONED_CALLS.with(Cell::get),
    }
}

/// Lightweight PRNG for sampling-based selectivity estimation.
/// Uses splitmix64 — same mixing function as `timeline_hash()` in the benchmark suite.
/// Not cryptographically secure.
#[derive(Clone, Debug)]
pub struct Prng(u64);

impl Prng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// Returns the internal state (for snapshot persistence).
    fn state(&self) -> u64 {
        self.0
    }

    /// Advance state and return a uniformly distributed u64.
    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    /// Return a uniform random value in `[0, n)` using Lemire's nearly divisionless method.
    /// Caller must ensure `n > 0`.
    fn next_bounded(&mut self, n: u64) -> u64 {
        debug_assert!(n > 0, "next_bounded requires n > 0");
        let mut x = self.next_u64();
        let mut m = (x as u128) * (n as u128);
        let mut l = m as u64;
        if l < n {
            let t = n.wrapping_neg() % n;
            while l < t {
                x = self.next_u64();
                m = (x as u128) * (n as u128);
                l = m as u64;
            }
        }
        (m >> 64) as u64
    }
}

impl Default for Prng {
    fn default() -> Self {
        Self::new(0xDEAD_BEEF_CAFE_BABE)
    }
}

/// Interned property key identifier (e.g. `"vertex:name"` → `PropKeyId`).
type PropKeyId = u16;

/// Bidirectional intern table for selectivity property keys.
/// Eliminates per-mutation `format!()` heap allocations.
#[derive(Clone, Debug, Default)]
struct PropKeyIntern {
    to_id: RapidHashMap<String, PropKeyId>,
    to_str: Vec<String>,
}

impl PropKeyIntern {
    fn intern(&mut self, key: &str) -> PropKeyId {
        if let Some(&id) = self.to_id.get(key) {
            return id;
        }
        let id = self.to_str.len() as PropKeyId;
        self.to_str.push(key.to_string());
        self.to_id.insert(key.to_string(), id);
        id
    }

    fn resolve(&self, id: PropKeyId) -> &str {
        &self.to_str[id as usize]
    }

    #[allow(dead_code)]
    fn len(&self) -> usize {
        self.to_str.len()
    }

    fn iter(&self) -> impl Iterator<Item = (PropKeyId, &str)> {
        self.to_str
            .iter()
            .enumerate()
            .map(|(i, s)| (i as PropKeyId, s.as_str()))
    }

    fn from_pairs(pairs: impl IntoIterator<Item = (PropKeyId, String)>) -> Self {
        let mut sorted: Vec<(PropKeyId, String)> = pairs.into_iter().collect();
        sorted.sort_by_key(|(id, _)| *id);
        let mut intern = Self::default();
        for (id, s) in sorted {
            debug_assert_eq!(id as usize, intern.to_str.len());
            intern.to_str.push(s.clone());
            intern.to_id.insert(s, id);
        }
        intern
    }
}

/// Compute selectivity from distinct and total counts.
fn selectivity_from_counts(distinct: u64, total: u64) -> f64 {
    if total == 0 {
        1.0
    } else {
        (distinct as f64 / total as f64).min(1.0)
    }
}

/// Hash a property value for distinct-counting in selectivity estimation.
/// Uses a discriminant tag per variant. Float uses `to_bits()`.
/// List/Path hash length only (same as the former `value_selectivity_key`).
fn hash_property_value(v: &gleaph_types::Value) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = rapidhash::fast::RapidHasher::default();
    match v {
        gleaph_types::Value::Null => 0u8.hash(&mut h),
        gleaph_types::Value::Bool(b) => {
            1u8.hash(&mut h);
            b.hash(&mut h);
        }
        gleaph_types::Value::Int64(i) => {
            2u8.hash(&mut h);
            i.hash(&mut h);
        }
        gleaph_types::Value::Float64(f) => {
            3u8.hash(&mut h);
            f.to_bits().hash(&mut h);
        }
        gleaph_types::Value::Text(s) => {
            4u8.hash(&mut h);
            s.hash(&mut h);
        }
        gleaph_types::Value::Timestamp(t) => {
            5u8.hash(&mut h);
            t.hash(&mut h);
        }
        gleaph_types::Value::List(l) => {
            6u8.hash(&mut h);
            l.len().hash(&mut h);
        }
        gleaph_types::Value::Path(p) => {
            7u8.hash(&mut h);
            p.len().hash(&mut h);
        }
        gleaph_types::Value::Bytes(b) => {
            8u8.hash(&mut h);
            b.hash(&mut h);
        }
        gleaph_types::Value::Date(d) => {
            9u8.hash(&mut h);
            d.hash(&mut h);
        }
        gleaph_types::Value::Time(t) => {
            10u8.hash(&mut h);
            t.hash(&mut h);
        }
        gleaph_types::Value::DateTime(s, n) => {
            11u8.hash(&mut h);
            s.hash(&mut h);
            n.hash(&mut h);
        }
        gleaph_types::Value::Duration(m, n) => {
            12u8.hash(&mut h);
            m.hash(&mut h);
            n.hash(&mut h);
        }
        gleaph_types::Value::Principal(p) => {
            13u8.hash(&mut h);
            p.as_slice().hash(&mut h);
        }
        gleaph_types::Value::Decimal(d) => {
            14u8.hash(&mut h);
            d.normalize().0.hash(&mut h);
        }
        gleaph_types::Value::Uint64(u) => {
            15u8.hash(&mut h);
            u.hash(&mut h);
        }
        gleaph_types::Value::Int8(i) => {
            16u8.hash(&mut h);
            i.hash(&mut h);
        }
        gleaph_types::Value::Int16(i) => {
            17u8.hash(&mut h);
            i.hash(&mut h);
        }
        gleaph_types::Value::Int32(i) => {
            18u8.hash(&mut h);
            i.hash(&mut h);
        }
        gleaph_types::Value::Int128(i) => {
            19u8.hash(&mut h);
            i.hash(&mut h);
        }
        gleaph_types::Value::Int256(i) => {
            20u8.hash(&mut h);
            i.0.to_le_bytes().hash(&mut h);
        }
        gleaph_types::Value::Uint8(u) => {
            21u8.hash(&mut h);
            u.hash(&mut h);
        }
        gleaph_types::Value::Uint16(u) => {
            22u8.hash(&mut h);
            u.hash(&mut h);
        }
        gleaph_types::Value::Uint32(u) => {
            23u8.hash(&mut h);
            u.hash(&mut h);
        }
        gleaph_types::Value::Uint128(u) => {
            24u8.hash(&mut h);
            u.hash(&mut h);
        }
        gleaph_types::Value::Uint256(u) => {
            25u8.hash(&mut h);
            u.0.to_le_bytes().hash(&mut h);
        }
        gleaph_types::Value::Float32(f) => {
            26u8.hash(&mut h);
            f.to_bits().hash(&mut h);
        }
    }
    h.finish()
}

/// Fixed-size reservoir maintained incrementally via Algorithm R.
const RESERVOIR_SIZE: usize = 1024;

/// A single reservoir sample entry.
#[derive(Clone, Debug)]
struct ReservoirEntry {
    entity_id: u32,         // vertex_id, or src for edges
    prop_key_id: PropKeyId, // interned property key
    value_hash: u64,        // hash of property value at mutation time
}

/// Result of a bulk edge insertion.
#[derive(Clone, Debug, Default)]
pub struct BulkInsertResult {
    /// Number of edges successfully inserted into the PMA.
    pub inserted: u64,
    /// Number of edges skipped (duplicates).
    pub skipped: u64,
    /// Per-input-edge outcome: `Some(edge_id)` if inserted, `None` if skipped.
    pub edge_ids: Vec<Option<u32>>,
}

/// Input for high-level bulk edge creation with labels and properties.
#[derive(Clone, Debug)]
pub struct BulkEdgeInput {
    pub src: u32,
    pub dst: u32,
    pub label: Option<String>,
    pub props: PropertyMap,
    pub weight: f32,
    pub timestamp: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// Derived PMA layout parameters for a given graph size and edge estimate.
pub struct PmaParams {
    pub segment_size: u32,
    pub segment_count: u32,
    pub elem_capacity: u64,
    pub tree_height: u32,
}

/// Computes PMA segmenting and capacity parameters for a graph shape.
pub fn compute_capacity(num_vertices: u32, num_edges: u64) -> PmaParams {
    let v = u64::from(num_vertices.max(1));
    let segment_size = ceil_log2(v).max(1);
    let raw_count = ceil_div(v, segment_size as u64).max(1);
    let segment_count = raw_count.next_power_of_two().max(1) as u32;
    let elem_capacity = num_edges.max(v).saturating_mul(4).max(16);
    let tree_height = ceil_log2(segment_count as u64).max(1);
    PmaParams {
        segment_size,
        segment_count,
        elem_capacity,
        tree_height,
    }
}

/// Packed-memory-array graph implementation backed by an abstract memory backend.
pub struct PmaGraph<M: Memory> {
    pub mem: M,
    pub num_vertices: u64,
    pub num_edges: u64,
    pub elem_capacity: u64,
    pub segment_size: u32,
    pub segment_count: u32,
    pub tree_height: u32,
    pub edge_array_base: u64,
    pub seg_tree_base: u64,
    pub seg_log_base: u64,
    pub seg_log_idx_base: u64,
    /// Monotonically increasing counter used to assign unique `edge_id` values.
    pub next_edge_id: u32,
    pub label_index: LabelIndex,
    vertex_labels: RapidHashMap<u32, Vec<u32>>,
    /// Incremental label-cardinality counters maintained during mutations.
    /// Avoids an O(vertices) scan in every `query()` call.
    label_live_count: BTreeMap<String, u64>,
    vertex_props: RapidHashMap<u32, PropertyMap>,
    vertex_prop_eq_index: RapidHashMap<(String, Vec<u8>), VertexIdSet>,
    /// In-memory range index for vertex properties (order-preserving encoding).
    /// Uses BTreeMap for ordered traversal during range scans.
    vertex_prop_range_index: BTreeMap<(String, Vec<u8>), VertexIdSet>,
    edge_props: RapidHashMap<u32, EdgePropsOverlay>,
    #[allow(clippy::type_complexity)]
    edge_prop_eq_index: RapidHashMap<(String, Vec<u8>), BTreeSet<u32>>,
    #[allow(clippy::type_complexity)]
    edge_prop_eq_by_src: RapidHashMap<(String, Vec<u8>, u32), Vec<u32>>,
    #[allow(clippy::type_complexity)]
    edge_prop_eq_by_dst: RapidHashMap<(String, Vec<u8>, u32), Vec<u32>>,
    property_indexes: BTreeSet<PropertyIndex>,
    tombstoned_vertices: VertexIdSet,
    next_created_vertex_id: u32,
    /// When non-zero, `reserve_vertices` has pre-expanded the vertex array up to this ID.
    /// `create_vertex` skips the bump-to-num_vertices when `next_created_vertex_id` is below this.
    vertex_reservation_end: u64,
    out_index: RapidHashMap<u32, RapidHashMap<u32, Vec<EdgeEntry>>>,
    rev_index: RapidHashMap<u32, RapidHashMap<u32, Vec<RevEntry>>>,
    /// Cached property-selectivity estimates (keyed by `vertex:<prop_name>` or `edge:<prop_name>`).
    /// Updated by `compute_property_selectivity()` or `compute_selectivity_for_properties()`.
    /// Persisted in overlay snapshot.
    property_selectivity: BTreeMap<String, f64>,
    /// Intern table for selectivity property keys (avoids per-mutation `format!()` allocations).
    prop_key_intern: PropKeyIntern,
    /// Per-property mutation count since last selectivity refresh (keyed by `PropKeyId`).
    selectivity_dirty_counts: RapidHashMap<PropKeyId, u32>,
    /// Per-property entity count at last refresh (for computing dirty ratio).
    selectivity_baselines: RapidHashMap<PropKeyId, u32>,
    /// PRNG for sampling-based selectivity estimation (splitmix64).
    rng: Prng,
    /// Fixed-size reservoir of recent property mutation events (Algorithm R).
    reservoir: Vec<ReservoirEntry>,
    /// Total property mutation events observed (for reservoir sampling probability).
    reservoir_total_seen: u64,
    /// Live stable-memory equality index handle.  When `Some`, base mutation methods
    /// (create_vertex, set_vertex_prop, etc.) incrementally maintain the ABP B-tree
    /// alongside the in-memory `vertex_prop_eq_index`.
    live_eq_index: Option<AbpSecondaryEqIndex<M>>,
}

#[derive(Clone, Debug, Default, CandidType, Serialize, Deserialize, PartialEq)]
pub struct GraphOverlaySnapshot {
    pub vertex_labels: Vec<(u32, Vec<String>)>,
    pub vertex_props: Vec<(u32, PropertyMap)>,
    pub edge_props: Vec<EdgePropsSnapshot>,
    pub property_indexes: Vec<PropertyIndex>,
    pub tombstoned_vertices: Vec<u32>,
    pub next_created_vertex_id: u32,
    /// Cached property-selectivity estimates.  Absent in older snapshots (defaults to empty map).
    #[serde(default)]
    pub property_selectivity: BTreeMap<String, f64>,
    /// Authoritative label-id ↔ name mapping.  When present, `restore_overlay_snapshot`
    /// pre-seeds the `LabelIndex` so that IDs match those baked into `EdgeEntry.label_id`
    /// in stable memory.  Absent in older snapshots (defaults to empty vec).
    #[serde(default)]
    pub label_id_map: Vec<(u32, String)>,
    /// Per-property mutation counts since last selectivity refresh (PropKeyId, count).
    #[serde(default)]
    pub selectivity_dirty_counts: Vec<(u16, u32)>,
    /// Per-property entity count at last refresh (PropKeyId, entity_count).
    #[serde(default)]
    pub selectivity_baselines: Vec<(u16, u32)>,
    /// PRNG state for sampling-based selectivity estimation.  0 = absent (old snapshot).
    #[serde(default)]
    pub selectivity_rng_state: u64,
    /// Intern table for property key ids ↔ string mapping.
    #[serde(default)]
    pub prop_key_intern_table: Vec<(u16, String)>,
    /// Reservoir entries: (entity_id, prop_key_id, value_hash).
    #[serde(default)]
    pub selectivity_reservoir: Vec<(u32, u16, u64)>,
    /// Total mutations observed by the reservoir (for Algorithm R).
    #[serde(default)]
    pub reservoir_total_seen: u64,
}

impl<M: Memory> PmaGraph<M> {
    #[inline]
    fn out_bucket_mut(&mut self, src: u32, label_id: u32) -> Option<&mut Vec<EdgeEntry>> {
        self.out_index.get_mut(&src)?.get_mut(&label_id)
    }

    #[inline]
    fn push_out_entry(&mut self, src: u32, entry: EdgeEntry) {
        self.out_index
            .entry(src)
            .or_default()
            .entry(entry.label_id())
            .or_default()
            .push(entry);
    }

    fn out_find_entry_mut_by_edge_id(
        &mut self,
        src: u32,
        label_id: u32,
        edge_id: u32,
    ) -> Option<&mut EdgeEntry> {
        self.out_bucket_mut(src, label_id)?
            .iter_mut()
            .find(|edge| edge.edge_id == edge_id)
    }

    fn out_find_entry_mut_by_dst_in_bucket(
        &mut self,
        src: u32,
        label_id: u32,
        dst: u32,
    ) -> Option<&mut EdgeEntry> {
        self.out_bucket_mut(src, label_id)?
            .iter_mut()
            .find(|edge| edge.target == dst)
    }

    fn out_find_entry_mut_by_dst_any_label(&mut self, src: u32, dst: u32) -> Option<&mut EdgeEntry> {
        let buckets = self.out_index.get_mut(&src)?;
        for entries in buckets.values_mut() {
            if let Some(edge) = entries.iter_mut().find(|edge| edge.target == dst) {
                return Some(edge);
            }
        }
        None
    }

    fn out_remove_entry_by_edge_id(&mut self, src: u32, label_id: u32, edge_id: u32) {
        let Some(buckets) = self.out_index.get_mut(&src) else {
            return;
        };
        if let Some(entries) = buckets.get_mut(&label_id) {
            if let Some(pos) = entries.iter().position(|edge| edge.edge_id == edge_id) {
                entries.swap_remove(pos);
            }
            if entries.is_empty() {
                buckets.remove(&label_id);
            }
        }
        if buckets.is_empty() {
            self.out_index.remove(&src);
        }
    }

    fn out_move_entry_label_by_edge_id(
        &mut self,
        src: u32,
        old_label_id: u32,
        new_label_id: u32,
        edge_id: u32,
    ) {
        if old_label_id == new_label_id {
            return;
        }
        let mut moved: Option<EdgeEntry> = None;
        let mut remove_outer = false;
        if let Some(buckets) = self.out_index.get_mut(&src) {
            if let Some(entries) = buckets.get_mut(&old_label_id)
                && let Some(idx) = entries.iter().position(|edge| edge.edge_id == edge_id)
            {
                let mut edge = entries.swap_remove(idx);
                edge.set_label_id(new_label_id);
                moved = Some(edge);
                if entries.is_empty() {
                    buckets.remove(&old_label_id);
                }
            }
            remove_outer = buckets.is_empty();
        }
        if remove_outer {
            self.out_index.remove(&src);
        }
        if let Some(edge) = moved {
            self.push_out_entry(src, edge);
        }
    }

    #[inline]
    fn rev_bucket_mut(&mut self, dst: u32, label_id: u32) -> Option<&mut Vec<RevEntry>> {
        self.rev_index.get_mut(&dst)?.get_mut(&label_id)
    }

    #[inline]
    fn push_rev_entry(&mut self, dst: u32, entry: RevEntry) {
        self.rev_index
            .entry(dst)
            .or_default()
            .entry(entry.label_id())
            .or_default()
            .push(entry);
    }

    fn rev_find_entry_mut_by_edge_id(
        &mut self,
        dst: u32,
        label_id: u32,
        edge_id: u32,
    ) -> Option<&mut RevEntry> {
        self.rev_bucket_mut(dst, label_id)?
            .iter_mut()
            .find(|entry| entry.edge_id == edge_id)
    }

    fn rev_find_entry_mut_by_src_in_bucket(
        &mut self,
        dst: u32,
        label_id: u32,
        src: u32,
    ) -> Option<&mut RevEntry> {
        self.rev_bucket_mut(dst, label_id)?
            .iter_mut()
            .find(|entry| entry.src == src)
    }

    fn rev_find_entry_mut_by_src_any_label(&mut self, dst: u32, src: u32) -> Option<&mut RevEntry> {
        let buckets = self.rev_index.get_mut(&dst)?;
        for entries in buckets.values_mut() {
            if let Some(entry) = entries.iter_mut().find(|entry| entry.src == src) {
                return Some(entry);
            }
        }
        None
    }

    fn rev_move_entry_label_by_edge_id(
        &mut self,
        dst: u32,
        old_label_id: u32,
        new_label_id: u32,
        edge_id: u32,
        tombstoned: Option<bool>,
    ) {
        if old_label_id == new_label_id {
            if let Some(entry) = self.rev_find_entry_mut_by_edge_id(dst, old_label_id, edge_id) {
                if let Some(tombstoned) = tombstoned {
                    entry.set_tombstoned(tombstoned);
                }
            }
            return;
        }

        let mut moved: Option<RevEntry> = None;
        let mut remove_outer = false;
        if let Some(buckets) = self.rev_index.get_mut(&dst) {
            if let Some(entries) = buckets.get_mut(&old_label_id)
                && let Some(idx) = entries.iter().position(|entry| entry.edge_id == edge_id)
            {
                let mut entry = entries.swap_remove(idx);
                entry.set_label_id(new_label_id);
                if let Some(tombstoned) = tombstoned {
                    entry.set_tombstoned(tombstoned);
                }
                moved = Some(entry);
            }
            if buckets.get(&old_label_id).is_some_and(|entries| entries.is_empty()) {
                buckets.remove(&old_label_id);
            }
            remove_outer = buckets.is_empty();
        }
        if remove_outer {
            self.rev_index.remove(&dst);
        }
        if let Some(entry) = moved {
            self.push_rev_entry(dst, entry);
        }
    }

    fn edge_payload_for_pair(&self, src: u32, dst: u32) -> Option<EdgeEntry> {
        self.collect_neighbors(src)
            .ok()?
            .into_iter()
            .find(|edge| edge.target == dst)
    }

    fn resolve_edge_id(&self, src: u32, dst: u32, label: Option<&str>) -> Option<u32> {
        let label_name = label.unwrap_or_default().to_string();
        let label_id = self.resolve_edge_label_id(Some(label_name.as_str()));
        self.find_edge_locator(src, dst, label_id)
            .and_then(|locator| self.read_edge_at_locator(locator))
            .map(|edge| edge.edge_id)
    }

    fn resolve_edge_overlay_key(
        &self,
        src: u32,
        dst: u32,
        label: Option<&str>,
    ) -> Option<(u32, String)> {
        let label_name = label.unwrap_or_default().to_string();
        let label_id = self.resolve_edge_label_id(Some(label_name.as_str()));
        self.find_edge_locator(src, dst, label_id)
            .and_then(|locator| self.read_edge_at_locator(locator))
            .filter(|edge| !edge.is_tombstoned())
            .map(|edge| (edge.edge_id, label_name))
    }

    fn find_edge_locator(&self, src: u32, dst: u32, label_id: u32) -> Option<EdgeLocator> {
        if src as u64 >= self.num_vertices {
            return None;
        }
        let v = layout::read_vertex(&self.mem, src);
        let on_seg = self.on_seg_edge_count_with_vertex(src, &v) as u64;
        for i in 0..on_seg {
            let slot = v.edge_index + i;
            if slot >= self.elem_capacity {
                continue;
            }
            let edge = layout::read_edge(&self.mem, self.edge_array_base, slot);
            if edge.target == dst && edge.label_id() == label_id {
                return Some(EdgeLocator::PmaSlot { slot });
            }
        }
        if v.log_offset >= 0 {
            let seg_id = self.get_segment_id(src);
            let log = SegmentLog::for_segment(self.seg_log_base, seg_id, self.seg_log_idx_base);
            let mut cur = v.log_offset;
            while cur >= 0 {
                let slot = cur as u32;
                let entry = log.read_entry(&self.mem, slot)?;
                if entry.src == src
                    && entry.dst == dst
                    && unpack_edge_label_id(entry.label_and_flags) == label_id
                {
                    return Some(EdgeLocator::LogSlot { seg_id, slot });
                }
                cur = entry.prev_offset;
            }
        }
        None
    }

    fn read_edge_at_locator(&self, locator: EdgeLocator) -> Option<EdgeEntry> {
        match locator {
            EdgeLocator::PmaSlot { slot } => {
                Some(layout::read_edge(&self.mem, self.edge_array_base, slot))
            }
            EdgeLocator::LogSlot { seg_id, slot } => {
                let log = SegmentLog::for_segment(self.seg_log_base, seg_id, self.seg_log_idx_base);
                let entry = log.read_entry(&self.mem, slot)?;
                Some(EdgeEntry {
                    target: entry.dst,
                    weight: entry.weight,
                    timestamp: entry.timestamp,
                    label_and_flags: entry.label_and_flags,
                    edge_id: entry.edge_id,
                })
            }
        }
    }

    fn set_edge_tombstoned_at(
        &mut self,
        locator: EdgeLocator,
        tombstoned: bool,
    ) -> Result<(), GleaphError> {
        match locator {
            EdgeLocator::PmaSlot { slot } => {
                let mut edge = layout::read_edge(&self.mem, self.edge_array_base, slot);
                let mut flags = edge.flags();
                if tombstoned {
                    flags |= gleaph_types::EDGE_FLAG_TOMBSTONED;
                } else {
                    flags &= !gleaph_types::EDGE_FLAG_TOMBSTONED;
                }
                edge.set_flags(flags);
                layout::write_edge(&mut self.mem, self.edge_array_base, slot, &edge);
            }
            EdgeLocator::LogSlot { seg_id, slot } => {
                let log = SegmentLog::for_segment(self.seg_log_base, seg_id, self.seg_log_idx_base);
                let mut entry = log.read_entry(&self.mem, slot).ok_or_else(|| {
                    GleaphError::ExecutionError("log edge not found for tombstone update".into())
                })?;
                let flags = gleaph_types::unpack_edge_flags(entry.label_and_flags);
                let new_flags = if tombstoned {
                    flags | gleaph_types::EDGE_FLAG_TOMBSTONED
                } else {
                    flags & !gleaph_types::EDGE_FLAG_TOMBSTONED
                };
                entry.label_and_flags =
                    pack_label_and_flags(unpack_edge_label_id(entry.label_and_flags), new_flags);
                if !log.overwrite(&mut self.mem, slot, entry) {
                    return Err(GleaphError::ExecutionError(
                        "failed to overwrite log edge tombstone".into(),
                    ));
                }
            }
        }
        Ok(())
    }

    fn resolve_edge_label_id(&self, label: Option<&str>) -> u32 {
        match label {
            Some(name) if !name.is_empty() => self.label_index.label_id(name).unwrap_or(0),
            _ => 0,
        }
    }

    fn vertex_label_ids_to_names(&self, label_ids: &[u32]) -> Vec<String> {
        let mut names: Vec<String> = label_ids
            .iter()
            .filter_map(|label_id| self.label_index.label_name(*label_id).map(str::to_string))
            .collect();
        names.sort();
        names
    }

    /// Creates a graph with default initial edge capacity derived from `initial_vertex_capacity`.
    pub fn new(mem: M, initial_vertex_capacity: u32) -> Result<Self, GleaphError> {
        Self::new_with_initial_edge_capacity(mem, initial_vertex_capacity, 0)
    }

    /// Creates a graph with an explicit minimum initial edge capacity.
    pub fn new_with_initial_edge_capacity(
        mut mem: M,
        initial_vertex_capacity: u32,
        initial_edge_capacity: u64,
    ) -> Result<Self, GleaphError> {
        let mut params = compute_capacity(initial_vertex_capacity, 0);
        params.elem_capacity = params.elem_capacity.max(initial_edge_capacity.max(16));
        let required = layout::total_memory_needed(
            initial_vertex_capacity as u64,
            params.elem_capacity,
            params.segment_count as u64,
        );
        ensure_mem_size(&mut mem, required)?;

        // When reusing an existing stable-memory region after an invalid header, stale PMA metadata
        // can remain. Reset segment counters and logs explicitly for a clean init.
        let seg_tree_base =
            layout::seg_tree_base(initial_vertex_capacity as u64, params.elem_capacity);
        let seg_log_base = layout::seg_log_base(
            initial_vertex_capacity as u64,
            params.elem_capacity,
            params.segment_count as u64,
        );
        let seg_log_idx_base = layout::seg_log_idx_base(
            initial_vertex_capacity as u64,
            params.elem_capacity,
            params.segment_count as u64,
        );
        clear_segments_and_logs(
            &mut mem,
            seg_tree_base,
            seg_log_base,
            seg_log_idx_base,
            params.segment_count,
        );

        for vid in 0..initial_vertex_capacity {
            let v = VertexEntry {
                edge_index: 0,
                degree: 0,
                log_offset: -1,
            };
            layout::write_vertex(&mut mem, vid, &v);
        }

        let mut graph = Self {
            mem,
            num_vertices: initial_vertex_capacity as u64,
            num_edges: 0,
            elem_capacity: params.elem_capacity,
            segment_size: params.segment_size,
            segment_count: params.segment_count,
            tree_height: params.tree_height,
            edge_array_base: layout::edge_array_base(initial_vertex_capacity as u64),
            seg_tree_base: layout::seg_tree_base(
                initial_vertex_capacity as u64,
                params.elem_capacity,
            ),
            seg_log_base: layout::seg_log_base(
                initial_vertex_capacity as u64,
                params.elem_capacity,
                params.segment_count as u64,
            ),
            seg_log_idx_base: layout::seg_log_idx_base(
                initial_vertex_capacity as u64,
                params.elem_capacity,
                params.segment_count as u64,
            ),
            next_edge_id: 1,
            label_index: LabelIndex::default(),
            vertex_labels: RapidHashMap::default(),
            label_live_count: BTreeMap::new(),
            vertex_props: RapidHashMap::default(),
            vertex_prop_eq_index: RapidHashMap::default(),
            vertex_prop_range_index: BTreeMap::new(),
            edge_props: RapidHashMap::default(),
            edge_prop_eq_index: RapidHashMap::default(),
            edge_prop_eq_by_src: RapidHashMap::default(),
            edge_prop_eq_by_dst: RapidHashMap::default(),
            property_indexes: BTreeSet::new(),
            tombstoned_vertices: VertexIdSet::new(),
            next_created_vertex_id: initial_vertex_capacity,
            vertex_reservation_end: 0,
            out_index: RapidHashMap::default(),
            rev_index: RapidHashMap::default(),
            property_selectivity: BTreeMap::new(),
            prop_key_intern: PropKeyIntern::default(),
            selectivity_dirty_counts: RapidHashMap::default(),
            selectivity_baselines: RapidHashMap::default(),
            rng: Prng::default(),
            reservoir: Vec::new(),
            reservoir_total_seen: 0,
            live_eq_index: None,
        };
        graph.rebuild_vertex_offsets()?;
        graph.write_header()?;
        Ok(graph)
    }

    /// Restores a graph from an existing stable-memory header and regions.
    pub fn from_stable_memory(mem: M) -> Result<Self, GleaphError> {
        if mem.size_bytes() < layout::HEADER_SIZE {
            return Err(GleaphError::InvalidHeader);
        }
        let h = layout::read_header(&mem);
        if h.magic != STABLE_MAGIC {
            return Err(GleaphError::InvalidHeader);
        }
        if h.version != STABLE_VERSION {
            return Err(GleaphError::InvalidHeader);
        }
        let mut graph = Self {
            mem,
            num_vertices: h.num_vertices,
            num_edges: h.num_edges,
            elem_capacity: h.elem_capacity,
            segment_size: h.segment_size,
            segment_count: h.segment_count,
            tree_height: h.tree_height,
            edge_array_base: h.edge_array_base,
            seg_tree_base: h.seg_tree_base,
            seg_log_base: h.seg_log_base,
            seg_log_idx_base: h.seg_log_idx_base,
            next_edge_id: h.next_edge_id,
            label_index: LabelIndex::default(),
            vertex_labels: RapidHashMap::default(),
            label_live_count: BTreeMap::new(),
            vertex_props: RapidHashMap::default(),
            vertex_prop_eq_index: RapidHashMap::default(),
            vertex_prop_range_index: BTreeMap::new(),
            edge_props: RapidHashMap::default(),
            edge_prop_eq_index: RapidHashMap::default(),
            edge_prop_eq_by_src: RapidHashMap::default(),
            edge_prop_eq_by_dst: RapidHashMap::default(),
            property_indexes: BTreeSet::new(),
            tombstoned_vertices: VertexIdSet::new(),
            next_created_vertex_id: h.num_vertices.min(u32::MAX as u64) as u32,
            vertex_reservation_end: 0,
            out_index: RapidHashMap::default(),
            rev_index: RapidHashMap::default(),
            property_selectivity: BTreeMap::new(),
            prop_key_intern: PropKeyIntern::default(),
            selectivity_dirty_counts: RapidHashMap::default(),
            selectivity_baselines: RapidHashMap::default(),
            rng: Prng::default(),
            reservoir: Vec::new(),
            reservoir_total_seen: 0,
            live_eq_index: None,
        };
        graph.build_forward_index();
        graph.build_reverse_index();
        Ok(graph)
    }

    fn build_forward_index(&mut self) {
        self.out_index.clear();
        for src in 0..self.num_vertices as u32 {
            let Ok(edges) = self.collect_neighbors_filtered(src) else {
                continue;
            };
            for edge in edges {
                self.push_out_entry(src, edge);
            }
        }
    }

    /// Rebuilds the reverse neighbor index by scanning all PMA edges.
    /// Called once after loading from stable memory; thereafter maintained incrementally.
    fn build_reverse_index(&mut self) {
        self.rev_index.clear();
        for src in 0..self.num_vertices as u32 {
            let Ok(edges) = self.collect_neighbors_filtered(src) else {
                continue;
            };
            for e in self.normalize_algo_neighbors(src, edges) {
                self.push_rev_entry(e.target, RevEntry {
                    src,
                    weight: e.weight,
                    timestamp: e.timestamp,
                    label_and_flags: pack_label_and_flags(e.label_id(), e.flags()),
                    edge_id: e.edge_id,
                });
            }
        }
    }

    /// Fixes packed label/flag state in every `RevEntry` by rereading the backing edge payload.
    pub fn rebuild_rev_index_labels(&mut self) {
        self.build_reverse_index();
    }

    /// Collapses duplicate `(src, dst)` PMA entries into a single logical edge for algorithms.
    ///
    /// Upgraded canisters can contain historical PMA artefacts that duplicate endpoints. The
    /// algorithm endpoints reject true parallel-edge graphs separately, so this helper only
    /// provides a stable logical view by choosing one representative entry per destination.
    fn normalize_algo_neighbors(&self, _src: u32, edges: Vec<EdgeEntry>) -> Vec<EdgeEntry> {
        let mut out = Vec::<EdgeEntry>::new();
        let mut by_dst_index: RapidHashMap<u32, usize> = RapidHashMap::default();
        for edge in edges {
            let is_zero_payload_phantom = edge.label_id() == 0 && edge.weight == 0.0 && edge.timestamp == 0;
            match by_dst_index.get(&edge.target).copied() {
                Some(idx) => {
                    let existing = &mut out[idx];
                    let existing_is_zero_payload_phantom =
                        existing.label_id() == 0 && existing.weight == 0.0 && existing.timestamp == 0;
                    if existing_is_zero_payload_phantom && !is_zero_payload_phantom {
                        *existing = edge;
                    }
                }
                None => {
                    by_dst_index.insert(edge.target, out.len());
                    out.push(edge);
                }
            }
        }
        out
    }

    /// Writes the current in-memory PMA metadata into the stable header.
    pub fn write_header(&mut self) -> Result<(), GleaphError> {
        let mut reserved = [0u8; 4008];
        if self.mem.size_bytes() >= layout::HEADER_SIZE {
            reserved = layout::read_header(&self.mem)._reserved;
        }
        let h = StableHeader {
            magic: STABLE_MAGIC,
            version: STABLE_VERSION,
            num_vertices: self.num_vertices,
            num_edges: self.num_edges,
            elem_capacity: self.elem_capacity,
            segment_size: self.segment_size,
            segment_count: self.segment_count,
            tree_height: self.tree_height,
            next_edge_id: self.next_edge_id,
            vertex_array_base: layout::VERTEX_ARRAY_BASE,
            edge_array_base: self.edge_array_base,
            seg_tree_base: self.seg_tree_base,
            seg_log_base: self.seg_log_base,
            seg_log_idx_base: self.seg_log_idx_base,
            _reserved: reserved,
            ..StableHeader::default()
        };
        layout::write_header(&mut self.mem, &h);
        Ok(())
    }

    /// Recomputes weighted spacing by rebuilding vertex offsets.
    pub fn spread_weighted(&mut self) -> Result<(), GleaphError> {
        self.rebuild_vertex_offsets()
    }

    /// Returns the segment id that owns the given vertex.
    pub fn get_segment_id(&self, vertex_id: u32) -> u32 {
        (vertex_id / self.segment_size.max(1)).min(self.segment_count.saturating_sub(1))
    }

    /// Computes the current density of a segment (`actual / total`).
    pub fn seg_density(&self, seg_id: u32) -> f64 {
        let actual = layout::read_seg_actual(&self.mem, self.seg_tree_base, seg_id) as f64;
        let total =
            layout::read_seg_total(&self.mem, self.seg_tree_base, self.segment_count, seg_id)
                as f64;
        if total <= 0.0 { 0.0 } else { actual / total }
    }

    /// Increments the tracked actual edge count for a segment.
    pub fn increment_seg_actual(&mut self, seg_id: u32) {
        let v = layout::read_seg_actual(&self.mem, self.seg_tree_base, seg_id);
        layout::write_seg_actual(&mut self.mem, self.seg_tree_base, seg_id, v + 1);
    }

    /// Decrements the tracked actual edge count for a segment.
    pub fn decrement_seg_actual(&mut self, seg_id: u32) {
        let v = layout::read_seg_actual(&self.mem, self.seg_tree_base, seg_id);
        layout::write_seg_actual(
            &mut self.mem,
            self.seg_tree_base,
            seg_id,
            v.saturating_sub(1),
        );
    }

    /// Recounts per-segment total slot weights across the current vertex layout.
    pub fn recount_seg_total(&mut self, _start_v: u32, _end_v: u32) {
        for seg in 0..self.segment_count {
            let seg_start = (seg * self.segment_size) as u64;
            if seg_start >= self.num_vertices {
                layout::write_seg_total(
                    &mut self.mem,
                    self.seg_tree_base,
                    self.segment_count,
                    seg,
                    0,
                );
                continue;
            }
            let seg_end = ((seg + 1) * self.segment_size) as u64;
            let end = seg_end.min(self.num_vertices);
            let mut total = 0u64;
            for vid in seg_start..end {
                let v = layout::read_vertex(&self.mem, vid as u32);
                total = total.saturating_add(v.degree as u64 + 1);
            }
            layout::write_seg_total(
                &mut self.mem,
                self.seg_tree_base,
                self.segment_count,
                seg,
                total,
            );
        }
    }

    /// Returns whether the candidate on-segment slot stays within the source vertex range.
    pub fn have_space_onseg(&self, src: u32, loc: u64) -> bool {
        let upper_bound = if u64::from(src) + 1 < self.num_vertices {
            layout::read_vertex(&self.mem, src + 1).edge_index
        } else {
            self.elem_capacity
        };
        loc < upper_bound
    }

    fn on_seg_edge_count_with_vertex(&self, vertex_id: u32, v: &VertexEntry) -> u32 {
        let available = self.elem_capacity.saturating_sub(v.edge_index);
        let mut log_count = 0u32;
        if v.log_offset >= 0 {
            let seg = self.get_segment_id(vertex_id);
            let log = SegmentLog::for_segment(self.seg_log_base, seg, self.seg_log_idx_base);
            let mut cur = v.log_offset;
            while cur >= 0 {
                if let Some(entry) = log.read_entry(&self.mem, cur as u32) {
                    if entry.src == vertex_id {
                        log_count = log_count.saturating_add(1);
                    }
                    cur = entry.prev_offset;
                } else {
                    break;
                }
            }
        }
        v.degree
            .saturating_sub(log_count)
            .min(available.min(u32::MAX as u64) as u32)
    }

    /// Returns the number of edges for `vertex_id` already stored in the PMA region (excluding log-only entries).
    pub fn on_seg_edge_count(&self, vertex_id: u32) -> u32 {
        let v = layout::read_vertex(&self.mem, vertex_id);
        self.on_seg_edge_count_with_vertex(vertex_id, &v)
    }

    /// Appends an edge to the segment overflow log for `src`.
    pub fn insert_into_log(
        &mut self,
        seg_id: u32,
        src: u32,
        dst: u32,
        label_id: u32,
        weight: f32,
        timestamp: u64,
        edge_id: u32,
    ) -> Result<(), GleaphError> {
        let mut v = layout::read_vertex(&self.mem, src);
        let log = SegmentLog::for_segment(self.seg_log_base, seg_id, self.seg_log_idx_base);
        let prev = v.log_offset;
        let slot = log
            .append(
                &mut self.mem,
                LogEntry {
                    src,
                    dst,
                    weight,
                    timestamp,
                    prev_offset: prev,
                    label_and_flags: pack_label_and_flags(label_id, 0),
                    edge_id,
                },
            )
            .ok_or(GleaphError::OutOfCapacity)?;
        v.log_offset = slot as i32;
        v.degree = v.degree.saturating_add(1);
        layout::write_vertex(&mut self.mem, src, &v);
        self.num_edges += 1;
        self.increment_seg_actual(seg_id);
        Ok(())
    }

    /// Inserts an edge, using the overflow log and rebalance/resize paths as needed.
    pub fn insert(
        &mut self,
        src: u32,
        dst: u32,
        label_id: u32,
        weight: f32,
        timestamp: u64,
    ) -> Result<(), GleaphError> {
        self.ensure_vertex(src)?;
        self.ensure_vertex(dst)?;
        if self.num_edges >= self.elem_capacity {
            self.resize()?;
        }

        let mut v = layout::read_vertex(&self.mem, src);
        let slot = v.edge_index + u64::from(v.degree);
        let seg_id = self.get_segment_id(src);
        let rev_edge_id;
        if self.have_space_onseg(src, slot) {
            rev_edge_id = self.next_edge_id;
            let edge = EdgeEntry {
                target: dst,
                weight,
                timestamp,
                label_and_flags: pack_label_and_flags(label_id, 0),
                edge_id: self.next_edge_id,
            };
            layout::write_edge(&mut self.mem, self.edge_array_base, slot, &edge);
            self.next_edge_id = self.next_edge_id.saturating_add(1);
            v.degree = v.degree.saturating_add(1);
            layout::write_vertex(&mut self.mem, src, &v);
            self.num_edges += 1;
            self.increment_seg_actual(seg_id);
        } else {
            rev_edge_id = self.next_edge_id;
            self.next_edge_id = self.next_edge_id.saturating_add(1);
            let log = SegmentLog::for_segment(self.seg_log_base, seg_id, self.seg_log_idx_base);
            if log.is_full(&self.mem) {
                self.rebalance_wrapper(src)?;
                if self.num_edges >= self.elem_capacity {
                    self.resize()?;
                }
            }
            self.insert_into_log(seg_id, src, dst, label_id, weight, timestamp, rev_edge_id)?;
        }
        // The edge is already committed at this point. A best-effort post-insert rebalance is an
        // optimization. On wasm, `rebalance_wrapper` can fail after partially rewriting PMA state
        // when the instruction budget is exceeded, so do not run it in a fire-and-forget mode.
        #[cfg(not(target_arch = "wasm32"))]
        let _ = self.rebalance_wrapper(src);
        self.push_out_entry(src, EdgeEntry {
            target: dst,
            weight,
            timestamp,
            label_and_flags: pack_label_and_flags(label_id, 0),
            edge_id: rev_edge_id,
        });
        self.push_rev_entry(dst, RevEntry {
            src,
            weight,
            timestamp,
            label_and_flags: pack_label_and_flags(label_id, 0),
            edge_id: rev_edge_id,
        });
        Ok(())
    }

    /// Attempts to rebalance a window around `src`, resizing if no window satisfies density bounds.
    pub fn rebalance_wrapper(&mut self, src: u32) -> Result<(), GleaphError> {
        let leaf = self.get_segment_id(src);
        let mut height = 0u32;

        while height <= self.tree_height {
            let span_segments = (1u32 << height).min(self.segment_count.max(1));
            let window_start_seg = (leaf / span_segments) * span_segments;
            let window_end_seg = (window_start_seg + span_segments).min(self.segment_count);

            let mut actual = 0u64;
            let mut total = 0u64;
            for seg in window_start_seg..window_end_seg {
                actual = actual.saturating_add(layout::read_seg_actual(
                    &self.mem,
                    self.seg_tree_base,
                    seg,
                ));
                total = total.saturating_add(layout::read_seg_total(
                    &self.mem,
                    self.seg_tree_base,
                    self.segment_count,
                    seg,
                ));
            }

            let density = if total == 0 {
                0.0
            } else {
                actual as f64 / total as f64
            };
            if density <= self.upper_threshold(height) {
                let start_v = (window_start_seg * self.segment_size).min(self.num_vertices as u32);
                let end_v = ((window_end_seg * self.segment_size).min(self.num_vertices as u32))
                    .max(start_v);
                return self.rebalance_weighted(start_v, end_v);
            }

            if window_start_seg == 0 && window_end_seg == self.segment_count {
                break;
            }
            height += 1;
        }

        self.resize()
    }

    /// Calculates weighted edge-array positions for vertices in `[start_v, end_v)`.
    pub fn calculate_positions(&self, start_v: u32, end_v: u32) -> Vec<u64> {
        if start_v >= end_v {
            return Vec::new();
        }

        let window_start = layout::read_vertex(&self.mem, start_v).edge_index;
        let window_end = if (end_v as u64) < self.num_vertices {
            layout::read_vertex(&self.mem, end_v).edge_index
        } else {
            self.elem_capacity
        };
        let window_capacity = window_end.saturating_sub(window_start);

        let mut degrees = Vec::with_capacity((end_v - start_v) as usize);
        let mut base_slots = 0u64;
        let mut weight_sum = 0u64;
        for vid in start_v..end_v {
            let d = layout::read_vertex(&self.mem, vid).degree as u64;
            degrees.push(d);
            base_slots = base_slots.saturating_add(d + 1);
            weight_sum = weight_sum.saturating_add(d + 1);
        }

        let extra = window_capacity.saturating_sub(base_slots);
        let mut pos = Vec::with_capacity(degrees.len());
        let mut cur = window_start;
        let mut prefix_weight = 0u64;

        for degree in degrees {
            pos.push(cur);
            let w = degree + 1;
            let before = if weight_sum == 0 {
                0
            } else {
                (prefix_weight.saturating_mul(extra)) / weight_sum
            };
            let after = if weight_sum == 0 {
                0
            } else {
                ((prefix_weight + w).saturating_mul(extra)) / weight_sum
            };
            let weighted_gap = after.saturating_sub(before);
            cur = cur.saturating_add(degree + 1 + weighted_gap);
            prefix_weight = prefix_weight.saturating_add(w);
        }

        pos
    }

    /// Rebalances a vertex window by redistributing on-segment and logged edges.
    pub fn rebalance_weighted(&mut self, start_v: u32, end_v: u32) -> Result<(), GleaphError> {
        if start_v >= end_v {
            return Ok(());
        }

        let instr_start = ic_instruction_counter();

        let positions = self.calculate_positions(start_v, end_v);
        let old_positions: Vec<u64> = (start_v..end_v)
            .map(|vid| layout::read_vertex(&self.mem, vid).edge_index)
            .collect();
        let on_seg_counts: Vec<u32> = (start_v..end_v)
            .map(|vid| self.on_seg_edge_count(vid))
            .collect();
        let mut per_vertex_log_edges: Vec<Vec<EdgeEntry>> =
            Vec::with_capacity((end_v - start_v) as usize);
        for vid in start_v..end_v {
            per_vertex_log_edges.push(self.collect_log_neighbors(vid));
        }
        let pivot = rebalance_pivot_index(&old_positions, &positions);
        let ctx = RebalanceWriteCtx {
            start_v,
            instr_start,
        };

        for i in 0..pivot {
            self.rewrite_vertex_after_rebalance(
                &ctx,
                i,
                old_positions[i],
                positions[i],
                on_seg_counts[i],
                &per_vertex_log_edges[i],
            )?;
        }
        for i in (pivot..positions.len()).rev() {
            self.rewrite_vertex_after_rebalance(
                &ctx,
                i,
                old_positions[i],
                positions[i],
                on_seg_counts[i],
                &per_vertex_log_edges[i],
            )?;
        }

        // Clear logs in segments touched by this rebalance window after merging their payloads.
        let start_seg = if start_v < self.num_vertices as u32 {
            self.get_segment_id(start_v)
        } else {
            0
        };
        let end_seg = if end_v == 0 {
            0
        } else {
            self.get_segment_id(end_v.saturating_sub(1))
        };
        for seg in start_seg..=end_seg {
            ensure_rebalance_budget_or_abort(instr_start)?;
            let log = SegmentLog::for_segment(self.seg_log_base, seg, self.seg_log_idx_base);
            let _ = log.drain(&mut self.mem);
        }

        self.recount_seg_total(start_v, end_v);
        Ok(())
    }

    /// Doubles PMA edge capacity and rewrites edges into the expanded layout.
    pub fn resize(&mut self) -> Result<(), GleaphError> {
        let old_positions: Vec<u64> = (0..self.num_vertices as u32)
            .map(|vid| layout::read_vertex(&self.mem, vid).edge_index)
            .collect();
        let on_seg_counts: Vec<u32> = (0..self.num_vertices as u32)
            .map(|vid| self.on_seg_edge_count(vid))
            .collect();
        let mut per_vertex_log_edges: Vec<Vec<EdgeEntry>> =
            Vec::with_capacity(self.num_vertices as usize);
        for vid in 0..self.num_vertices as u32 {
            per_vertex_log_edges.push(self.collect_log_neighbors(vid));
        }
        // Drain old log regions before metadata/base offsets move.
        for seg in 0..self.segment_count {
            let log = SegmentLog::for_segment(self.seg_log_base, seg, self.seg_log_idx_base);
            let _ = log.drain(&mut self.mem);
        }

        self.elem_capacity = self.elem_capacity.saturating_mul(2).max(16);
        let required = layout::total_memory_needed(
            self.num_vertices,
            self.elem_capacity,
            self.segment_count as u64,
        );
        relocate_reserved_non_pma_regions_for_pma_growth(&mut self.mem, required)?;
        // PMA growth can relocate the stable secondary-index region, invalidating any cached
        // live handle that still points at the pre-relocation offset.
        self.live_eq_index = None;
        ensure_mem_size(&mut self.mem, required)?;
        self.seg_tree_base = layout::seg_tree_base(self.num_vertices, self.elem_capacity);
        self.seg_log_base = layout::seg_log_base(
            self.num_vertices,
            self.elem_capacity,
            self.segment_count as u64,
        );
        self.seg_log_idx_base = layout::seg_log_idx_base(
            self.num_vertices,
            self.elem_capacity,
            self.segment_count as u64,
        );

        let positions = self.calculate_positions(0, self.num_vertices as u32);
        for i in (0..positions.len()).rev() {
            let pos = positions[i];
            let vid = i as u32;
            let old_pos = old_positions[i];
            let on_seg = on_seg_counts[i] as u64;

            if pos > old_pos {
                for j in (0..on_seg).rev() {
                    let e = layout::read_edge(&self.mem, self.edge_array_base, old_pos + j);
                    layout::write_edge(&mut self.mem, self.edge_array_base, pos + j, &e);
                }
            } else {
                for j in 0..on_seg {
                    let e = layout::read_edge(&self.mem, self.edge_array_base, old_pos + j);
                    layout::write_edge(&mut self.mem, self.edge_array_base, pos + j, &e);
                }
            }
            for (j, edge) in per_vertex_log_edges[i].iter().enumerate() {
                layout::write_edge(
                    &mut self.mem,
                    self.edge_array_base,
                    pos + on_seg + j as u64,
                    edge,
                );
            }

            let mut v = layout::read_vertex(&self.mem, vid);
            v.edge_index = pos;
            v.log_offset = -1;
            layout::write_vertex(&mut self.mem, vid, &v);
        }

        for seg in 0..self.segment_count {
            // `all_edges` already captured both on-segment and overflow-log edges from the old
            // layout, and the graph was fully rewritten into the new edge array above. The new
            // log/index regions may overlap old bytes, so reading fill counts before explicitly
            // zeroing them can interpret stale data as a huge log length and trigger OOB reads.
            layout::write_seg_log_fill(&mut self.mem, self.seg_log_idx_base, seg, 0);
            let mut actual = 0u64;
            let seg_start = (seg * self.segment_size) as u64;
            let seg_end = ((seg + 1) * self.segment_size) as u64;
            for vid in seg_start..seg_end.min(self.num_vertices) {
                let v = layout::read_vertex(&self.mem, vid as u32);
                actual = actual.saturating_add(v.degree as u64);
            }
            layout::write_seg_actual(&mut self.mem, self.seg_tree_base, seg, actual);
        }
        self.recount_seg_total(0, self.num_vertices as u32);
        self.write_header()?;
        Ok(())
    }

    /// Collects all outgoing neighbors for a vertex from the PMA region and overflow log.
    ///
    /// Uses a single-pass log walk (merged count + collect) and bulk PMA reads to minimize
    /// memory operations.
    pub fn collect_neighbors(&self, vertex_id: u32) -> Result<Vec<EdgeEntry>, GleaphError> {
        if vertex_id as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(vertex_id));
        }
        let v = layout::read_vertex(&self.mem, vertex_id);

        // Single-pass: walk log chain to both count and collect log edges (avoids double walk).
        let mut log_edges = Vec::new();
        let mut log_count = 0u32;
        if v.log_offset >= 0 {
            let seg = self.get_segment_id(vertex_id);
            let log = SegmentLog::for_segment(self.seg_log_base, seg, self.seg_log_idx_base);
            let mut cur = v.log_offset;
            while cur >= 0 {
                if let Some(entry) = log.read_entry(&self.mem, cur as u32) {
                    if entry.src == vertex_id {
                        log_count += 1;
                        log_edges.push(EdgeEntry {
                            target: entry.dst,
                            weight: entry.weight,
                            timestamp: entry.timestamp,
                            label_and_flags: entry.label_and_flags,
                            edge_id: entry.edge_id,
                        });
                    }
                    cur = entry.prev_offset;
                } else {
                    break;
                }
            }
        }

        // Compute on-seg count from degree - log_count (no second log walk needed).
        let available = self.elem_capacity.saturating_sub(v.edge_index);
        let on_seg = v
            .degree
            .saturating_sub(log_count)
            .min(available.min(u32::MAX as u64) as u32) as u64;

        // Bulk read all on-seg edges in a single memory operation.
        let clamped = on_seg.min(self.elem_capacity.saturating_sub(v.edge_index));
        let mut out =
            layout::read_edges_bulk(&self.mem, self.edge_array_base, v.edge_index, clamped);
        out.extend(log_edges);
        Ok(out)
    }

    /// Collects outgoing neighbors, skipping edges outside the given timestamp range.
    ///
    /// Returns `(edges, total_read)` where `total_read` is the number of raw PMA/log entries
    /// examined (for stats tracking), and `edges` contains only those within `ts_range`.
    ///
    /// Uses a single-pass log walk (merged count + collect) and bulk PMA reads.
    pub fn collect_neighbors_ts_filtered(
        &self,
        vertex_id: u32,
        ts_range: &TimestampRange,
    ) -> Result<(Vec<EdgeEntry>, u64), GleaphError> {
        if vertex_id as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(vertex_id));
        }
        let v = layout::read_vertex(&self.mem, vertex_id);
        let mut out = Vec::new();
        let mut total_read: u64 = 0;
        let early_cutoff = ts_range.start;

        // Log chain: already newest→oldest (via prev_offset).
        // Early-terminate when timestamp drops below ts_range.start.
        let mut log_count = 0u32;
        if v.log_offset >= 0 {
            let seg = self.get_segment_id(vertex_id);
            let log = SegmentLog::for_segment(self.seg_log_base, seg, self.seg_log_idx_base);
            let mut cur = v.log_offset;
            while cur >= 0 {
                if let Some(entry) = log.read_entry(&self.mem, cur as u32) {
                    if entry.src == vertex_id {
                        log_count += 1;
                        total_read += 1;
                        if let Some(start) = early_cutoff
                            && entry.timestamp < start
                        {
                            break;
                        }
                        if ts_matches(ts_range, entry.timestamp) {
                            out.push(EdgeEntry {
                                target: entry.dst,
                                weight: entry.weight,
                                timestamp: entry.timestamp,
                                label_and_flags: entry.label_and_flags,
                                edge_id: entry.edge_id,
                            });
                        }
                    }
                    cur = entry.prev_offset;
                } else {
                    break;
                }
            }
        }

        // Compute on-seg count from degree - log_count (no second log walk needed).
        let available = self.elem_capacity.saturating_sub(v.edge_index);
        let on_seg = v
            .degree
            .saturating_sub(log_count)
            .min(available.min(u32::MAX as u64) as u32) as u64;

        // On-segment edges: iterate in reverse (newest→oldest) for early termination
        // when timestamps are monotonically increasing (production invariant).
        let clamped = on_seg.min(self.elem_capacity.saturating_sub(v.edge_index));
        let bulk = layout::read_edges_bulk(&self.mem, self.edge_array_base, v.edge_index, clamped);
        total_read += clamped;
        for edge in bulk.into_iter().rev() {
            if let Some(start) = early_cutoff
                && edge.timestamp < start
            {
                break;
            }
            if ts_matches(ts_range, edge.timestamp) {
                out.push(edge);
            }
        }
        Ok((out, total_read))
    }

    /// Iterates over outgoing neighbors for a vertex, calling `f` for each
    /// matching edge.
    ///
    /// Optional filters:
    /// - `label_filter`: skip edges whose inline `label_id()` does not match.
    /// - `ts_range`: skip edges outside the timestamp range.
    ///
    /// Returns `total_read` — the number of raw PMA/log entries examined
    /// (for stats tracking).
    pub fn for_each_neighbor_filtered<F>(
        &self,
        vertex_id: u32,
        label_filter: Option<u32>,
        ts_range: Option<&TimestampRange>,
        f: &mut F,
    ) -> Result<u64, GleaphError>
    where
        F: FnMut(EdgeEntry) -> Result<(), GleaphError>,
    {
        if vertex_id as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(vertex_id));
        }
        if let Some(label_id) = label_filter
            && let Some(label_buckets) = self.out_index.get(&vertex_id)
            && let Some(entries) = label_buckets.get(&label_id)
        {
            let mut total_read: u64 = 0;
            for &edge in entries {
                total_read = total_read.saturating_add(1);
                if ts_range.is_none_or(|r| ts_matches(r, edge.timestamp)) {
                    f(edge)?;
                }
            }
            return Ok(total_read);
        }
        let v = layout::read_vertex(&self.mem, vertex_id);
        let mut total_read: u64 = 0;

        // Walk log chain, calling f directly (no log_edges Vec).
        let mut log_count = 0u32;
        if v.log_offset >= 0 {
            let seg = self.get_segment_id(vertex_id);
            let log = SegmentLog::for_segment(self.seg_log_base, seg, self.seg_log_idx_base);
            let mut cur = v.log_offset;
            while cur >= 0 {
                if let Some(entry) = log.read_entry(&self.mem, cur as u32) {
                    if entry.src == vertex_id {
                        log_count += 1;
                        total_read += 1;
                        if label_filter.is_none_or(|label_id| {
                            unpack_edge_label_id(entry.label_and_flags) == label_id
                        }) && ts_range.is_none_or(|r| ts_matches(r, entry.timestamp))
                        {
                            let edge = EdgeEntry {
                                target: entry.dst,
                                weight: entry.weight,
                                timestamp: entry.timestamp,
                                label_and_flags: entry.label_and_flags,
                                edge_id: entry.edge_id,
                            };
                            f(edge)?;
                        }
                    }
                    cur = entry.prev_offset;
                } else {
                    break;
                }
            }
        }

        // Bulk read on-segment edges (single mem.read), then filter + callback.
        let available = self.elem_capacity.saturating_sub(v.edge_index);
        let on_seg = v
            .degree
            .saturating_sub(log_count)
            .min(available.min(u32::MAX as u64) as u32) as u64;
        let clamped = on_seg.min(self.elem_capacity.saturating_sub(v.edge_index));
        let bulk = layout::read_edges_bulk(&self.mem, self.edge_array_base, v.edge_index, clamped);
        total_read += clamped;
        for edge in bulk {
            if label_filter.is_none_or(|label_id| edge.label_id() == label_id)
                && ts_range.is_none_or(|r| ts_matches(r, edge.timestamp))
            {
                f(edge)?;
            }
        }
        Ok(total_read)
    }

    pub fn for_each_neighbor<F>(
        &self,
        vertex_id: u32,
        ts_range: Option<&TimestampRange>,
        f: &mut F,
    ) -> Result<u64, GleaphError>
    where
        F: FnMut(EdgeEntry) -> Result<(), GleaphError>,
    {
        self.for_each_neighbor_filtered(vertex_id, None, ts_range, f)
    }

    /// Collects outgoing neighbors while filtering tombstoned vertices/edges.
    pub fn collect_neighbors_filtered(
        &self,
        vertex_id: u32,
    ) -> Result<Vec<EdgeEntry>, GleaphError> {
        if self.is_vertex_tombstoned(vertex_id) {
            return Ok(Vec::new());
        }
        let neighbors = self.collect_neighbors(vertex_id)?;
        Ok(neighbors
            .into_iter()
            .filter(|edge| {
                !self.tombstoned_vertices.contains(edge.target)
                    && !edge.is_tombstoned()
            })
            .collect())
    }

    /// Returns true when multiple PMA edges share the same `(src, dst)` endpoints.
    ///
    /// Phase-2 GQL overlay metadata is keyed by endpoint pair, so such graphs are rejected by the
    /// GQL bridge to avoid ambiguous label/tombstone/property semantics on upgraded canisters.
    pub fn has_parallel_edges_by_endpoints(&self) -> bool {
        for src in 0..(self.vertex_count() as u32) {
            let Ok(neighbors) = self.collect_neighbors(src) else {
                continue;
            };
            let mut seen = HashSet::with_capacity(neighbors.len());
            for edge in neighbors {
                // Skip tombstoned edges: a deleted-then-re-added (src, dst) pair leaves a raw
                // duplicate in the PMA array and segment log, but only one live edge exists.
                let label = self.edge_label(src, edge.target);
                if self.is_edge_tombstoned(src, edge.target, label.as_deref()) {
                    continue;
                }
                if seen.insert(edge.target) {
                    // First time we see this (src, dst) target – not a parallel edge yet.
                    continue;
                }
                // Second or later occurrence of the same (src, dst) target.  Determine whether
                // this is a genuine raw-insert parallel edge or a PMA storage artefact:
                //
                // 1. Entries where the GQL overlay already tracks a live `(src, dst)` label are
                //    structural duplicates caused by the PMA rebalance overlap bug; the overlay
                //    guarantees at most one live logical edge per endpoint pair.
                // 2. Zero-payload slots (weight=0 AND timestamp=0) that have no overlay label are
                //    PMA over-count phantoms: `on_seg_edge_count_with_vertex` can read beyond a
                //    vertex's true boundary into zero-initialised array slots during rebalancing,
                //    producing spurious edge entries with the default-zeroed fields.
                //
                // NOTE: A legacy raw-insert parallel edge with weight=0 and timestamp=0 cannot be
                // distinguished from a phantom by payload alone.  Such edges are not creatable via
                // the GQL API, and the vast majority of practical graphs will never contain them.
                // If a future use-case requires detecting that specific sentinel pair, the PMA
                // over-count bug must be fixed at the source so that `collect_neighbors` never
                // returns phantom entries.
                let overlay_backed = label.is_some();
                let is_zero_payload = edge.weight == 0.0 && edge.timestamp == 0;
                if overlay_backed || is_zero_payload {
                    // PMA-level artefact – not a true parallel edge in the logical graph.
                    continue;
                }
                return true;
            }
        }
        false
    }

    /// Ensures the vertex id is representable, expanding the vertex array when required.
    pub fn ensure_vertex(&mut self, vertex_id: u32) -> Result<(), GleaphError> {
        if vertex_id as u64 >= self.num_vertices {
            self.expand_vertices(vertex_id as u64 + 1)?;
        }
        Ok(())
    }

    /// Pre-expand the vertex array to accommodate `additional` new vertices beyond
    /// the current allocation point. Call this before a batch of `create_vertex`
    /// calls to avoid repeated O(V+E) `expand_vertices` rebuilds.
    pub fn reserve_vertices(&mut self, additional: u32) -> Result<(), GleaphError> {
        if additional == 0 {
            return Ok(());
        }
        // Determine where create_vertex will start assigning IDs.
        let start = std::cmp::max(self.next_created_vertex_id as u64, self.num_vertices);
        let end = start + additional as u64;
        // Expand the vertex array to hold IDs [start, end).
        if end > self.num_vertices {
            self.expand_vertices(end)?;
        }
        // Pin next_created_vertex_id so create_vertex uses the reserved range
        // instead of bumping to num_vertices.
        self.next_created_vertex_id = start.try_into().map_err(|_| GleaphError::OutOfCapacity)?;
        self.vertex_reservation_end = end;
        Ok(())
    }

    /// Returns the current vertex count.
    pub fn vertex_count(&self) -> u64 {
        self.num_vertices
    }
    /// Returns the current edge count.
    pub fn edge_count(&self) -> u64 {
        self.num_edges
    }

    /// Returns a compact stats snapshot of the graph.
    pub fn stats(&self) -> GraphStats {
        GraphStats {
            num_vertices: self.num_vertices,
            num_edges: self.num_edges,
            elem_capacity: self.elem_capacity,
            segment_size: self.segment_size,
            segment_count: self.segment_count,
            avg_degree: if self.num_vertices == 0 {
                0.0
            } else {
                self.num_edges as f64 / self.num_vertices as f64
            },
        }
    }

    pub fn has_overlay_properties(&self) -> bool {
        !self.vertex_props.is_empty() || !self.edge_props.is_empty()
    }

    // ── Dirty tracking helpers for incremental selectivity ──────────────

    /// Intern a vertex property name as `"vertex:{prop_name}"`.
    fn intern_vertex_prop_key(&mut self, prop_name: &str) -> PropKeyId {
        // Avoid format!() allocation when the key already exists.
        let prefix = "vertex:";
        // Check if already interned by scanning existing entries.
        for (id, s) in self.prop_key_intern.iter() {
            if s.starts_with(prefix) && &s[prefix.len()..] == prop_name {
                return id;
            }
        }
        let key = format!("vertex:{prop_name}");
        self.prop_key_intern.intern(&key)
    }

    /// Intern an edge property name as `"edge:{prop_name}"`.
    fn intern_edge_prop_key(&mut self, prop_name: &str) -> PropKeyId {
        let prefix = "edge:";
        for (id, s) in self.prop_key_intern.iter() {
            if s.starts_with(prefix) && &s[prefix.len()..] == prop_name {
                return id;
            }
        }
        let key = format!("edge:{prop_name}");
        self.prop_key_intern.intern(&key)
    }

    /// Increment the dirty counter for an interned property key.
    fn mark_property_dirty_by_id(&mut self, id: PropKeyId) {
        *self.selectivity_dirty_counts.entry(id).or_insert(0) += 1;
    }

    /// Algorithm R reservoir sampling: observe a property mutation event.
    fn reservoir_observe(&mut self, prop_key_id: PropKeyId, entity_id: u32, value_hash: u64) {
        self.reservoir_total_seen += 1;
        if self.reservoir.len() < RESERVOIR_SIZE {
            self.reservoir.push(ReservoirEntry {
                entity_id,
                prop_key_id,
                value_hash,
            });
        } else {
            let j = self.rng.next_bounded(self.reservoir_total_seen);
            if j < RESERVOIR_SIZE as u64 {
                self.reservoir[j as usize] = ReservoirEntry {
                    entity_id,
                    prop_key_id,
                    value_hash,
                };
            }
        }
    }

    /// Estimate selectivity from reservoir for a given property key.
    /// Returns `Some((distinct, count))` if enough samples, else `None`.
    fn estimate_from_reservoir(&self, prop_key_id: PropKeyId) -> Option<(u64, u64)> {
        let mut distinct = HashSet::new();
        let mut count = 0u64;
        for entry in &self.reservoir {
            if entry.prop_key_id == prop_key_id {
                distinct.insert(entry.value_hash);
                count += 1;
            }
        }
        if count >= MIN_PROPERTY_SAMPLE as u64 {
            Some((distinct.len() as u64, count))
        } else {
            None
        }
    }

    /// Seed the reservoir from current overlay state (for cold start).
    fn seed_reservoir_from_overlay(&mut self) {
        // Collect vertex property events.
        let vertex_entries: Vec<(u32, Vec<(String, gleaph_types::Value)>)> = self
            .vertex_props
            .iter()
            .filter(|(vid, _)| !self.tombstoned_vertices.contains(**vid))
            .map(|(&vid, props)| (vid, props.clone()))
            .collect();
        for (vid, props) in vertex_entries {
            for (k, v) in &props {
                let kid = self.intern_vertex_prop_key(k);
                let vh = hash_property_value(v);
                self.reservoir_observe(kid, vid, vh);
            }
        }
        // Collect edge property events.
        let edge_entries: Vec<(u32, EdgePropsOverlay)> = self
            .edge_props
            .iter()
            .filter(|(_, overlay)| {
                !self.is_edge_tombstoned(overlay.src, overlay.dst, Some(overlay.label.as_str()))
            })
            .map(|(&edge_id, overlay)| (edge_id, overlay.clone()))
            .collect();
        for (edge_id, overlay) in edge_entries {
            for (k, v) in &overlay.props {
                let kid = self.intern_edge_prop_key(k);
                let vh = hash_property_value(v);
                self.reservoir_observe(kid, edge_id, vh);
            }
        }
    }

    /// Mark all properties of a vertex as dirty and observe for reservoir.
    fn mark_vertex_props_dirty(&mut self, vertex_id: u32) {
        if let Some(props) = self.vertex_props.get(&vertex_id) {
            let entries: Vec<(String, gleaph_types::Value)> = props.clone();
            for (prop_name, value) in entries {
                let id = self.intern_vertex_prop_key(&prop_name);
                self.mark_property_dirty_by_id(id);
                self.reservoir_observe(id, vertex_id, hash_property_value(&value));
            }
        }
    }

    /// Mark all properties of an edge as dirty and observe for reservoir.
    fn mark_edge_props_dirty_by_id(&mut self, edge_id: u32) {
        if let Some(overlay) = self.edge_props.get(&edge_id) {
            let entries: Vec<(String, gleaph_types::Value)> = overlay.props.clone();
            for (prop_name, value) in entries {
                let id = self.intern_edge_prop_key(&prop_name);
                self.mark_property_dirty_by_id(id);
                self.reservoir_observe(id, edge_id, hash_property_value(&value));
            }
        }
    }

    pub fn create_vertex(
        &mut self,
        labels: Vec<String>,
        props: PropertyMap,
    ) -> Result<u32, GleaphError> {
        if (self.next_created_vertex_id as u64) < self.num_vertices
            && (self.next_created_vertex_id as u64) >= self.vertex_reservation_end
        {
            self.next_created_vertex_id = self
                .num_vertices
                .try_into()
                .map_err(|_| GleaphError::OutOfCapacity)?;
        }
        let vertex_id = self.next_created_vertex_id;
        self.ensure_vertex(vertex_id)?;
        self.next_created_vertex_id = vertex_id.saturating_add(1);
        self.tombstoned_vertices.remove(vertex_id);
        for label in labels {
            let label_id = self.label_index.ensure_label_id(&label);
            self.label_index.add_vertex_label_id(vertex_id, label_id);
            *self.label_live_count.entry(label.clone()).or_insert(0) += 1;
            let labels = self.vertex_labels.entry(vertex_id).or_default();
            if let Err(pos) = labels.binary_search(&label_id) {
                labels.insert(pos, label_id);
            }
        }
        if !props.is_empty() {
            self.index_vertex_props(vertex_id, &props);
            if let Some(mut idx) = self.live_eq_index.take() {
                let res = self.apply_vertex_props_to_abp_secondary_eq_index(
                    &mut idx, vertex_id, &props, true,
                );
                self.live_eq_index = Some(idx);
                res?;
            }
            self.vertex_props.insert(vertex_id, props);
        }
        self.mark_vertex_props_dirty(vertex_id);
        Ok(vertex_id)
    }

    /// Convenience wrapper that updates graph state and a stable-memory equality index together
    /// for vertex creation/property materialization.
    pub fn create_vertex_with_abp_eq_index<N: Memory>(
        &mut self,
        idx: &mut AbpSecondaryEqIndex<N>,
        labels: Vec<String>,
        props: PropertyMap,
    ) -> Result<u32, GleaphError> {
        let vertex_id = self.create_vertex(labels, props.clone())?;
        self.apply_vertex_props_to_abp_secondary_eq_index(idx, vertex_id, &props, true)?;
        Ok(vertex_id)
    }

    /// Convenience wrapper that updates graph state and a stable property-store snapshot together
    /// for vertex creation/property materialization.
    pub fn create_vertex_with_abp_property_store<N: Memory>(
        &mut self,
        store: &mut AbpPropertyStore<N>,
        labels: Vec<String>,
        props: PropertyMap,
    ) -> Result<u32, GleaphError> {
        let vertex_id = self.create_vertex(labels, props.clone())?;
        self.apply_vertex_props_to_abp_property_store(store, vertex_id, &props, true)?;
        Ok(vertex_id)
    }

    pub fn create_edge(
        &mut self,
        src: u32,
        dst: u32,
        label: Option<String>,
        props: PropertyMap,
        weight: f32,
        timestamp: u64,
    ) -> Result<(), GleaphError> {
        let label_name = label.unwrap_or_default();
        let resolved_label_id = if label_name.is_empty() {
            0
        } else {
            self.label_index.ensure_label_id(&label_name)
        };

        // Check for existing edge with same (src, dst, label) — not just (src, dst).
        let neighbors = self.collect_neighbors(src)?;
        let has_same_label_edge = neighbors
            .iter()
            .any(|edge| edge.target == dst && edge.label_id() == resolved_label_id);

        if has_same_label_edge {
            // Check if the existing edge is tombstoned — if so, revive it.
            if self.is_edge_tombstoned(
                src,
                dst,
                Some(if label_name.is_empty() {
                    ""
                } else {
                    &label_name
                }),
            ) {
                let locator = self.find_edge_locator(src, dst, resolved_label_id).ok_or_else(|| {
                    GleaphError::ExecutionError("failed to locate tombstoned edge".into())
                })?;
                self.set_edge_tombstoned_at(locator, false)?;
                let edge_id = self
                    .read_edge_at_locator(locator)
                    .map(|edge| edge.edge_id)
                    .ok_or_else(|| {
                        GleaphError::ExecutionError("failed to read revived edge".into())
                    })?;
                self.edge_props.remove(&edge_id);
                if !props.is_empty() {
                    self.index_edge_props(edge_id, src, dst, &props);
                    self.edge_props.insert(
                        edge_id,
                        EdgePropsOverlay {
                            src,
                            dst,
                            label: label_name.clone(),
                            props,
                        },
                    );
                }
                if !self.update_edge_payload_by_endpoints_labeled(
                    src,
                    dst,
                    resolved_label_id,
                    weight,
                    timestamp,
                )? {
                    return Err(GleaphError::ExecutionError(
                        "failed to refresh payload for revived edge".into(),
                    ));
                }
                self.mark_edge_props_dirty_by_id(edge_id);
                return Ok(());
            }
            return Err(GleaphError::UnsupportedFeature(
                "duplicate edge with same label between the same vertices".into(),
            ));
        }

        // Insert new edge with label_id baked into EdgeEntry.
        let edge_id = self.next_edge_id;
        self.insert(src, dst, resolved_label_id, weight, timestamp)?;
        if !props.is_empty() {
            self.index_edge_props(edge_id, src, dst, &props);
            self.edge_props.insert(
                edge_id,
                EdgePropsOverlay {
                    src,
                    dst,
                    label: label_name.clone(),
                    props,
                },
            );
        }
        self.mark_edge_props_dirty_by_id(edge_id);
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    pub fn create_edge_with_abp_property_store<N: Memory>(
        &mut self,
        store: &mut AbpPropertyStore<N>,
        src: u32,
        dst: u32,
        label: Option<String>,
        props: PropertyMap,
        weight: f32,
        timestamp: u64,
    ) -> Result<(), GleaphError> {
        let label_name = label.clone().unwrap_or_default();
        self.create_edge(src, dst, label, props.clone(), weight, timestamp)?;
        self.apply_edge_props_to_abp_property_store(store, src, dst, &label_name, &props, true)
    }

    /// Legacy API compatibility: revive a logically deleted edge by endpoints while preserving
    /// any existing overlay metadata (label/properties).
    pub fn revive_edge_by_endpoints(&mut self, src: u32, dst: u32) -> Result<bool, GleaphError> {
        let neighbors = self.collect_neighbors(src)?;
        let Some(edge) = neighbors.into_iter().find(|edge| edge.target == dst) else {
            return Ok(false);
        };
        let label_id = edge.label_id();
        if let Some(locator) = self.find_edge_locator(src, dst, label_id)
            && self
                .read_edge_at_locator(locator)
                .is_some_and(|edge| edge.is_tombstoned())
        {
            self.set_edge_tombstoned_at(locator, false)?;
            if let Some(edge) = self.read_edge_at_locator(locator) {
                self.push_out_entry(src, edge);
            }
            if let Some(entry) = self.rev_find_entry_mut_by_edge_id(dst, label_id, edge.edge_id) {
                entry.set_tombstoned(false);
            }
            return Ok(true);
        }
        Ok(false)
    }

    /// Updates the stored PMA/log payload for an existing edge identified by endpoints.
    pub fn update_edge_payload_by_endpoints(
        &mut self,
        src: u32,
        dst: u32,
        weight: f32,
        timestamp: u64,
    ) -> Result<bool, GleaphError> {
        if src as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(src));
        }
        let v = layout::read_vertex(&self.mem, src);
        let on_seg = self.on_seg_edge_count_with_vertex(src, &v) as u64;
        for i in 0..on_seg {
            let slot = v.edge_index + i;
            if slot >= self.elem_capacity {
                continue;
            }
            let mut edge = layout::read_edge(&self.mem, self.edge_array_base, slot);
            if edge.target == dst {
                edge.weight = weight;
                edge.timestamp = timestamp;
                layout::write_edge(&mut self.mem, self.edge_array_base, slot, &edge);
                if let Some(e) = self.out_find_entry_mut_by_dst_any_label(src, dst) {
                    e.weight = weight;
                    e.timestamp = timestamp;
                }
                if let Some(e) = self.rev_find_entry_mut_by_src_any_label(dst, src) {
                    e.weight = weight;
                    e.timestamp = timestamp;
                }
                return Ok(true);
            }
        }

        if v.log_offset >= 0 {
            let seg = self.get_segment_id(src);
            let log = SegmentLog::for_segment(self.seg_log_base, seg, self.seg_log_idx_base);
            let mut cur = v.log_offset;
            while cur >= 0 {
                if let Some(mut entry) = log.read_entry(&self.mem, cur as u32) {
                    if entry.src == src && entry.dst == dst {
                        entry.weight = weight;
                        entry.timestamp = timestamp;
                        layout::write_log_entry(
                            &mut self.mem,
                            self.seg_log_base,
                            seg,
                            cur as u32,
                            &entry,
                        );
                        if let Some(e) = self.out_find_entry_mut_by_dst_any_label(src, dst) {
                            e.weight = weight;
                            e.timestamp = timestamp;
                        }
                        if let Some(e) = self.rev_find_entry_mut_by_src_any_label(dst, src) {
                            e.weight = weight;
                            e.timestamp = timestamp;
                        }
                        return Ok(true);
                    }
                    cur = entry.prev_offset;
                } else {
                    break;
                }
            }
        }

        Ok(false)
    }

    /// Like `update_edge_payload_by_endpoints` but matches on `(src, dst, label_id)`.
    pub fn update_edge_payload_by_endpoints_labeled(
        &mut self,
        src: u32,
        dst: u32,
        label_id: u32,
        weight: f32,
        timestamp: u64,
    ) -> Result<bool, GleaphError> {
        if src as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(src));
        }
        let v = layout::read_vertex(&self.mem, src);
        let on_seg = self.on_seg_edge_count_with_vertex(src, &v) as u64;
        for i in 0..on_seg {
            let slot = v.edge_index + i;
            if slot >= self.elem_capacity {
                continue;
            }
            let mut edge = layout::read_edge(&self.mem, self.edge_array_base, slot);
            if edge.target == dst && edge.label_id() == label_id {
                edge.weight = weight;
                edge.timestamp = timestamp;
                layout::write_edge(&mut self.mem, self.edge_array_base, slot, &edge);
                if let Some(e) = self.out_find_entry_mut_by_dst_in_bucket(src, label_id, dst) {
                    e.weight = weight;
                    e.timestamp = timestamp;
                }
                if let Some(e) = self.rev_find_entry_mut_by_src_in_bucket(dst, label_id, src) {
                    e.weight = weight;
                    e.timestamp = timestamp;
                }
                return Ok(true);
            }
        }

        if v.log_offset >= 0 {
            let seg = self.get_segment_id(src);
            let log = SegmentLog::for_segment(self.seg_log_base, seg, self.seg_log_idx_base);
            let mut cur = v.log_offset;
            while cur >= 0 {
                if let Some(mut entry) = log.read_entry(&self.mem, cur as u32) {
                    if entry.src == src
                        && entry.dst == dst
                        && unpack_edge_label_id(entry.label_and_flags) == label_id
                    {
                        entry.weight = weight;
                        entry.timestamp = timestamp;
                        layout::write_log_entry(
                            &mut self.mem,
                            self.seg_log_base,
                            seg,
                            cur as u32,
                            &entry,
                        );
                        if let Some(e) = self.out_find_entry_mut_by_dst_in_bucket(src, label_id, dst) {
                            e.weight = weight;
                            e.timestamp = timestamp;
                        }
                        if let Some(e) = self.rev_find_entry_mut_by_src_in_bucket(dst, label_id, src) {
                            e.weight = weight;
                            e.timestamp = timestamp;
                        }
                        return Ok(true);
                    }
                    cur = entry.prev_offset;
                } else {
                    break;
                }
            }
        }

        Ok(false)
    }

    pub fn delete_vertex(&mut self, vertex_id: u32) -> Result<(), GleaphError> {
        if vertex_id as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(vertex_id));
        }
        // Remove indexed properties from live ABP before tombstoning.
        if let Some(mut idx) = self.live_eq_index.take() {
            if let Some(props) = self.vertex_props.get(&vertex_id) {
                let res = self.apply_vertex_props_to_abp_secondary_eq_index(
                    &mut idx, vertex_id, props, false,
                );
                self.live_eq_index = Some(idx);
                res?;
            } else {
                self.live_eq_index = Some(idx);
            }
        }
        self.mark_vertex_props_dirty(vertex_id);
        self.tombstoned_vertices.insert(vertex_id);
        if let Some(labels) = self.vertex_labels.get(&vertex_id).cloned() {
            for label_id in labels {
                self.label_index.remove_vertex_label_id(vertex_id, label_id);
                if let Some(label) = self.label_index.label_name(label_id)
                    && let Some(c) = self.label_live_count.get_mut(label)
                {
                    *c = c.saturating_sub(1);
                }
            }
        }
        Ok(())
    }

    pub fn delete_vertex_with_abp_eq_index<N: Memory>(
        &mut self,
        idx: &mut AbpSecondaryEqIndex<N>,
        vertex_id: u32,
    ) -> Result<(), GleaphError> {
        let props = self.get_vertex_props(vertex_id).unwrap_or_default();
        self.delete_vertex(vertex_id)?;
        self.apply_vertex_props_to_abp_secondary_eq_index(idx, vertex_id, &props, false)
    }

    /// Ensures a vertex exists and clears a logical tombstone if present.
    pub fn revive_vertex(&mut self, vertex_id: u32) -> Result<(), GleaphError> {
        self.revive_vertex_changed(vertex_id).map(|_| ())
    }

    /// Ensures a vertex exists and reports whether graph-visible state changed.
    pub fn revive_vertex_changed(&mut self, vertex_id: u32) -> Result<bool, GleaphError> {
        let existed_before = (vertex_id as u64) < self.num_vertices;
        self.ensure_vertex(vertex_id)?;
        let was_tombstoned = self.tombstoned_vertices.remove(vertex_id);
        if was_tombstoned && let Some(labels) = self.vertex_labels.get(&vertex_id).cloned() {
            for label_id in labels {
                self.label_index.add_vertex_label_id(vertex_id, label_id);
            }
        }
        Ok(!existed_before || was_tombstoned)
    }

    pub fn delete_edge(
        &mut self,
        src: u32,
        dst: u32,
        label: Option<&str>,
    ) -> Result<(), GleaphError> {
        let label_id = self.resolve_edge_label_id(label);
        let locator = self.find_edge_locator(src, dst, label_id).ok_or_else(|| {
            GleaphError::ExecutionError("edge not found for tombstone".into())
        })?;
        if let Some(edge_id) = self.read_edge_at_locator(locator).map(|edge| edge.edge_id) {
            if let Some(old_props) = self.edge_props.get(&edge_id).map(|overlay| overlay.props.clone()) {
                self.deindex_edge_props(edge_id, src, dst, &old_props);
            }
            self.mark_edge_props_dirty_by_id(edge_id);
        }
        self.set_edge_tombstoned_at(locator, true)?;
        if let Some(edge_id) = self.read_edge_at_locator(locator).map(|edge| edge.edge_id)
        {
            self.out_remove_entry_by_edge_id(src, label_id, edge_id);
        }
        if let Some(edge_id) = self.read_edge_at_locator(locator).map(|edge| edge.edge_id)
            && let Some(entry) = self.rev_find_entry_mut_by_edge_id(dst, label_id, edge_id)
        {
            entry.set_tombstoned(true);
        }
        Ok(())
    }

    pub fn get_vertex_props(&self, vertex_id: u32) -> Option<PropertyMap> {
        self.vertex_props.get(&vertex_id).cloned()
    }

    /// Returns a single property value for a vertex without cloning the entire PropertyMap.
    pub fn get_single_vertex_property(
        &self,
        vertex_id: u32,
        key: &str,
    ) -> Option<gleaph_types::Value> {
        self.vertex_props.get(&vertex_id).and_then(|props| {
            props
                .iter()
                .find_map(|(k, v)| if k == key { Some(v.clone()) } else { None })
        })
    }

    pub fn set_vertex_props(
        &mut self,
        vertex_id: u32,
        props: PropertyMap,
    ) -> Result<(), GleaphError> {
        if vertex_id as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(vertex_id));
        }
        let old = self.vertex_props.get(&vertex_id).cloned();
        if let Some(ref old) = old {
            self.deindex_vertex_props(vertex_id, old);
            // Mark old property keys dirty (they may have been removed or changed).
            for (k, _) in old {
                let id = self.intern_vertex_prop_key(k);
                self.mark_property_dirty_by_id(id);
                self.reservoir_observe(
                    id,
                    vertex_id,
                    hash_property_value(&gleaph_types::Value::Null),
                );
            }
        }
        self.index_vertex_props(vertex_id, &props);
        // Mark new property keys dirty.
        for (k, v) in &props {
            let id = self.intern_vertex_prop_key(k);
            self.mark_property_dirty_by_id(id);
            self.reservoir_observe(id, vertex_id, hash_property_value(v));
        }
        if let Some(mut idx) = self.live_eq_index.take() {
            if let Some(ref old) = old {
                let _ = self
                    .apply_vertex_props_to_abp_secondary_eq_index(&mut idx, vertex_id, old, false);
            }
            let res = self
                .apply_vertex_props_to_abp_secondary_eq_index(&mut idx, vertex_id, &props, true);
            self.live_eq_index = Some(idx);
            res?;
        }
        self.vertex_props.insert(vertex_id, props);
        Ok(())
    }

    pub fn set_vertex_props_with_abp_eq_index<N: Memory>(
        &mut self,
        idx: &mut AbpSecondaryEqIndex<N>,
        vertex_id: u32,
        props: PropertyMap,
    ) -> Result<(), GleaphError> {
        let old = self.get_vertex_props(vertex_id).unwrap_or_default();
        self.set_vertex_props(vertex_id, props.clone())?;
        self.apply_vertex_props_to_abp_secondary_eq_index(idx, vertex_id, &old, false)?;
        self.apply_vertex_props_to_abp_secondary_eq_index(idx, vertex_id, &props, true)
    }

    pub fn set_vertex_props_with_abp_property_store<N: Memory>(
        &mut self,
        store: &mut AbpPropertyStore<N>,
        vertex_id: u32,
        props: PropertyMap,
    ) -> Result<(), GleaphError> {
        let old = self.get_vertex_props(vertex_id).unwrap_or_default();
        self.set_vertex_props(vertex_id, props.clone())?;
        self.apply_vertex_props_to_abp_property_store(store, vertex_id, &old, false)?;
        self.apply_vertex_props_to_abp_property_store(store, vertex_id, &props, true)
    }

    pub fn set_vertex_prop(
        &mut self,
        vertex_id: u32,
        key: String,
        value: gleaph_types::Value,
    ) -> Result<(), GleaphError> {
        if vertex_id as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(vertex_id));
        }
        let old_value = self.vertex_props.get(&vertex_id).and_then(|props| {
            props
                .iter()
                .find(|(k, _)| k == &key)
                .map(|(_, v)| v.clone())
        });
        if let Some(old) = old_value.as_ref() {
            self.deindex_vertex_prop(vertex_id, &key, old);
        }
        {
            let props = self.vertex_props.entry(vertex_id).or_default();
            if let Some((_, v)) = props.iter_mut().find(|(k, _)| k == &key) {
                *v = value.clone();
            } else {
                props.push((key.clone(), value.clone()));
            }
        }
        self.index_vertex_prop(vertex_id, &key, &value);
        if let Some(mut idx) = self.live_eq_index.take() {
            let res = self.apply_vertex_prop_delta_to_abp_secondary_eq_index(
                &mut idx,
                vertex_id,
                &key,
                old_value.as_ref(),
                Some(&value),
            );
            self.live_eq_index = Some(idx);
            res?;
        }
        let kid = self.intern_vertex_prop_key(&key);
        self.mark_property_dirty_by_id(kid);
        self.reservoir_observe(kid, vertex_id, hash_property_value(&value));
        Ok(())
    }

    pub fn set_vertex_prop_with_abp_eq_index<N: Memory>(
        &mut self,
        idx: &mut AbpSecondaryEqIndex<N>,
        vertex_id: u32,
        key: String,
        value: gleaph_types::Value,
    ) -> Result<(), GleaphError> {
        let old_value = self.vertex_props.get(&vertex_id).and_then(|props| {
            props
                .iter()
                .find(|(k, _)| k == &key)
                .map(|(_, v)| v.clone())
        });
        self.set_vertex_prop(vertex_id, key.clone(), value.clone())?;
        self.apply_vertex_prop_delta_to_abp_secondary_eq_index(
            idx,
            vertex_id,
            &key,
            old_value.as_ref(),
            Some(&value),
        )
    }

    pub fn set_vertex_prop_with_abp_property_store<N: Memory>(
        &mut self,
        store: &mut AbpPropertyStore<N>,
        vertex_id: u32,
        key: String,
        value: gleaph_types::Value,
    ) -> Result<(), GleaphError> {
        self.set_vertex_prop(vertex_id, key.clone(), value.clone())?;
        self.apply_vertex_prop_delta_to_abp_property_store(store, vertex_id, &key, Some(&value))
    }

    pub fn add_vertex_label(&mut self, vertex_id: u32, label: String) -> Result<(), GleaphError> {
        if vertex_id as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(vertex_id));
        }
        let label_id = self.label_index.ensure_label_id(&label);
        let labels = self.vertex_labels.entry(vertex_id).or_default();
        let inserted = match labels.binary_search(&label_id) {
            Ok(_) => false,
            Err(pos) => {
                labels.insert(pos, label_id);
                true
            }
        };
        if inserted && !self.tombstoned_vertices.contains(vertex_id) {
            self.label_index.add_vertex_label_id(vertex_id, label_id);
            *self.label_live_count.entry(label).or_insert(0) += 1;
        }
        Ok(())
    }

    pub fn remove_vertex_label(&mut self, vertex_id: u32, label: &str) -> Result<(), GleaphError> {
        if vertex_id as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(vertex_id));
        }
        if let Some(labels) = self.vertex_labels.get_mut(&vertex_id)
            && let Some(label_id) = self.label_index.label_id(label)
            && let Ok(pos) = labels.binary_search(&label_id)
        {
            labels.remove(pos);
            self.label_index.remove_vertex_label_id(vertex_id, label_id);
            if !self.tombstoned_vertices.contains(vertex_id)
                && let Some(c) = self.label_live_count.get_mut(label)
            {
                *c = c.saturating_sub(1);
            }
        }
        Ok(())
    }

    pub fn set_edge_prop(
        &mut self,
        src: u32,
        dst: u32,
        label: Option<&str>,
        key: String,
        value: gleaph_types::Value,
    ) -> Result<(), GleaphError> {
        if self.edge_record(src, dst, label).is_none() {
            return Err(GleaphError::ValidationError(
                "edge binding does not resolve to a live edge".into(),
            ));
        }
        let (edge_id, label_name) = self
            .resolve_edge_overlay_key(src, dst, label)
            .ok_or_else(|| {
                GleaphError::ValidationError("edge binding does not resolve to a live edge".into())
            })?;
        // Deindex the old value (if any) before mutating.
        if let Some(overlay) = self.edge_props.get(&edge_id)
            && let Some((_, old_v)) = overlay.props.iter().find(|(k, _)| k == &key)
        {
            self.deindex_edge_prop(edge_id, src, dst, &key, &old_v.clone());
        }
        let vh = hash_property_value(&value);
        self.index_edge_prop(edge_id, src, dst, &key, &value);
        let overlay = self.edge_props.entry(edge_id).or_insert_with(|| EdgePropsOverlay {
            src,
            dst,
            label: label_name,
            props: Vec::new(),
        });
        if let Some((_, v)) = overlay.props.iter_mut().find(|(k, _)| k == &key) {
            *v = value;
        } else {
            overlay.props.push((key.clone(), value));
        }
        let kid = self.intern_edge_prop_key(&key);
        self.mark_property_dirty_by_id(kid);
        self.reservoir_observe(kid, edge_id, vh);
        Ok(())
    }

    pub fn set_edge_prop_with_abp_property_store<N: Memory>(
        &mut self,
        store: &mut AbpPropertyStore<N>,
        src: u32,
        dst: u32,
        label: Option<&str>,
        key: String,
        value: gleaph_types::Value,
    ) -> Result<(), GleaphError> {
        self.set_edge_prop(src, dst, label, key.clone(), value.clone())?;
        self.apply_edge_prop_delta_to_abp_property_store(store, src, dst, label, &key, Some(&value))
    }

    pub fn delete_vertex_prop(&mut self, vertex_id: u32, key: &str) -> Result<(), GleaphError> {
        if vertex_id as u64 >= self.num_vertices {
            return Err(GleaphError::VertexNotFound(vertex_id));
        }
        let old_value = self
            .vertex_props
            .get(&vertex_id)
            .and_then(|props| props.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()));
        if let Some(old) = old_value.as_ref() {
            self.deindex_vertex_prop(vertex_id, key, old);
        }
        if let Some(props) = self.vertex_props.get_mut(&vertex_id) {
            props.retain(|(k, _)| k != key);
        }
        if let Some(mut idx) = self.live_eq_index.take() {
            let res = self.apply_vertex_prop_delta_to_abp_secondary_eq_index(
                &mut idx,
                vertex_id,
                key,
                old_value.as_ref(),
                None,
            );
            self.live_eq_index = Some(idx);
            res?;
        }
        let kid = self.intern_vertex_prop_key(key);
        self.mark_property_dirty_by_id(kid);
        self.reservoir_observe(
            kid,
            vertex_id,
            hash_property_value(&gleaph_types::Value::Null),
        );
        Ok(())
    }

    pub fn delete_vertex_prop_with_abp_eq_index<N: Memory>(
        &mut self,
        idx: &mut AbpSecondaryEqIndex<N>,
        vertex_id: u32,
        key: &str,
    ) -> Result<(), GleaphError> {
        let old_value = self
            .vertex_props
            .get(&vertex_id)
            .and_then(|props| props.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone()));
        self.delete_vertex_prop(vertex_id, key)?;
        self.apply_vertex_prop_delta_to_abp_secondary_eq_index(
            idx,
            vertex_id,
            key,
            old_value.as_ref(),
            None,
        )
    }

    pub fn delete_vertex_prop_with_abp_property_store<N: Memory>(
        &mut self,
        store: &mut AbpPropertyStore<N>,
        vertex_id: u32,
        key: &str,
    ) -> Result<(), GleaphError> {
        self.delete_vertex_prop(vertex_id, key)?;
        self.apply_vertex_prop_delta_to_abp_property_store(store, vertex_id, key, None)
    }

    pub fn scan_vertices_by_property_eq(
        &self,
        key: &str,
        value: &gleaph_types::Value,
    ) -> VertexIdSet {
        if !self.has_vertex_equality_index(key) {
            return VertexIdSet::new();
        }
        let Some(enc) = Self::encode_index_value(value) else {
            return VertexIdSet::new();
        };
        let set = self
            .vertex_prop_eq_index
            .get(&(key.to_string(), enc))
            .cloned()
            .unwrap_or_default();
        self.minus_tombstoned(set)
    }

    /// In-memory range scan using the BTreeMap range index.
    ///
    /// Returns vertices whose property value satisfies the given comparison operator
    /// against `bound_value`. Returns an empty Vec if no range index exists for the property.
    pub fn scan_vertices_by_property_range(
        &self,
        key: &str,
        bound_value: &gleaph_types::Value,
        cmp_op: crate::property_store::RangeOp,
    ) -> VertexIdSet {
        use crate::property_store::RangeOp;
        use std::ops::Bound;

        if !self.has_vertex_range_index(key) {
            return VertexIdSet::new();
        }
        let Ok(enc) = crate::property_store::encode_value_ordered(bound_value) else {
            return VertexIdSet::new();
        };
        let prop_key = key.to_string();

        let range_iter = |start, end| {
            self.vertex_prop_range_index
                .range::<(String, Vec<u8>), _>((start, end))
                .take_while(|((pn, _), _)| pn == &prop_key)
                .flat_map(|(_, ids)| ids.iter())
        };

        let out: VertexIdSet = match cmp_op {
            RangeOp::Ge => {
                range_iter(Bound::Included((prop_key.clone(), enc)), Bound::Unbounded).collect()
            }
            RangeOp::Gt => {
                range_iter(Bound::Excluded((prop_key.clone(), enc)), Bound::Unbounded).collect()
            }
            RangeOp::Le => range_iter(
                Bound::Included((prop_key.clone(), Vec::new())),
                Bound::Included((prop_key.clone(), enc)),
            )
            .collect(),
            RangeOp::Lt => range_iter(
                Bound::Included((prop_key.clone(), Vec::new())),
                Bound::Excluded((prop_key.clone(), enc)),
            )
            .collect(),
        };
        self.minus_tombstoned(out)
    }

    /// In-memory compound range scan using the BTreeMap range index.
    ///
    /// Returns vertices whose property value falls between `lower_value` and `upper_value`
    /// (inclusive/exclusive determined by the operators).
    pub fn scan_vertices_by_property_range_between(
        &self,
        key: &str,
        lower_value: &gleaph_types::Value,
        lower_op: crate::property_store::RangeOp,
        upper_value: &gleaph_types::Value,
        upper_op: crate::property_store::RangeOp,
    ) -> VertexIdSet {
        use crate::property_store::RangeOp;
        use std::ops::Bound;

        if !self.has_vertex_range_index(key) {
            return VertexIdSet::new();
        }
        let Ok(lower_enc) = crate::property_store::encode_value_ordered(lower_value) else {
            return VertexIdSet::new();
        };
        let Ok(upper_enc) = crate::property_store::encode_value_ordered(upper_value) else {
            return VertexIdSet::new();
        };
        let prop_key = key.to_string();

        let start = match lower_op {
            RangeOp::Ge => Bound::Included((prop_key.clone(), lower_enc)),
            RangeOp::Gt => {
                let mut after = lower_enc;
                after.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
                Bound::Excluded((prop_key.clone(), after))
            }
            _ => Bound::Included((prop_key.clone(), Vec::new())),
        };
        let end = match upper_op {
            RangeOp::Le => {
                let mut end = upper_enc;
                end.extend_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
                Bound::Included((prop_key.clone(), end))
            }
            RangeOp::Lt => Bound::Excluded((prop_key.clone(), upper_enc)),
            _ => Bound::Excluded((prop_key.clone(), {
                let mut p = prop_key.as_bytes().to_vec();
                p.push(0xFF);
                p
            })),
        };

        let out: VertexIdSet = self
            .vertex_prop_range_index
            .range((start, end))
            .take_while(|((pn, _), _)| pn == &prop_key)
            .flat_map(|(_, ids)| ids.iter())
            .collect();
        self.minus_tombstoned(out)
    }

    /// Best-effort compound range scan: tries ABP B+ tree first, falls back to in-memory.
    pub fn scan_vertices_by_property_range_between_auto(
        &self,
        key: &str,
        lower_value: &gleaph_types::Value,
        lower_op: crate::property_store::RangeOp,
        upper_value: &gleaph_types::Value,
        upper_op: crate::property_store::RangeOp,
    ) -> Result<VertexIdSet, GleaphError> {
        // Try live ABP handle first.
        if let Some(ref idx) = self.live_eq_index {
            if self.has_vertex_range_index(key) {
                // ABP doesn't have a native between scan, so do two scans and intersect.
                let lower_ids = idx
                    .scan_vertices_range(key, lower_value, lower_op)
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
                let upper_ids = idx
                    .scan_vertices_range(key, upper_value, upper_op)
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
                let lower_set: VertexIdSet = lower_ids.into_iter().collect();
                let upper_set: VertexIdSet = upper_ids.into_iter().collect();
                return Ok(self.minus_tombstoned(&lower_set & &upper_set));
            }
        }
        // Fallback: in-memory BTreeMap index.
        Ok(self.scan_vertices_by_property_range_between(
            key,
            lower_value,
            lower_op,
            upper_value,
            upper_op,
        ))
    }

    /// Best-effort range scan: tries ABP B+ tree first, falls back to in-memory.
    pub fn scan_vertices_by_property_range_auto(
        &self,
        key: &str,
        bound_value: &gleaph_types::Value,
        cmp_op: crate::property_store::RangeOp,
    ) -> Result<VertexIdSet, GleaphError> {
        // Try live ABP handle first.
        if let Some(ref idx) = self.live_eq_index {
            if self.has_vertex_range_index(key) {
                let ids = idx
                    .scan_vertices_range(key, bound_value, cmp_op)
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
                let set: VertexIdSet = ids.into_iter().collect();
                return Ok(self.minus_tombstoned(set));
            }
        }
        // Fallback: in-memory BTreeMap index.
        Ok(self.scan_vertices_by_property_range(key, bound_value, cmp_op))
    }

    /// Lightweight property equality lookup that does **not** require `M: Clone`.
    ///
    /// Tries the live ABP handle first, then falls back to the in-memory HashMap index.
    /// Returns `None` only when the property is not indexed at all.
    pub fn scan_vertices_by_property_eq_live(
        &self,
        key: &str,
        value: &gleaph_types::Value,
    ) -> Option<VertexIdSet> {
        if !self.has_vertex_equality_index(key) {
            return None;
        }
        // Try live ABP handle first.
        if let Some(idx) = self.live_eq_index.as_ref()
            && let Ok(ids) = idx.scan_vertices_eq(key, value)
        {
            let set: VertexIdSet = ids.into_iter().collect();
            return Some(self.minus_tombstoned(set));
        }
        // Fallback: in-memory HashMap index (always available when property is indexed).
        Some(self.scan_vertices_by_property_eq(key, value))
    }

    /// Best-effort property equality lookup for planner/executor index scans.
    ///
    /// If a reserved stable `(a,b)+ tree` secondary-index region is present and readable, query it;
    /// otherwise fall back to the host-side in-memory equality index.
    pub fn scan_vertices_by_property_eq_auto(
        &self,
        key: &str,
        value: &gleaph_types::Value,
    ) -> Result<VertexIdSet, GleaphError>
    where
        M: Clone,
    {
        // Prefer the live handle (avoids re-opening ABP from stable memory each query).
        if let Some(ref idx) = self.live_eq_index {
            if !self.has_vertex_equality_index(key) {
                return Ok(VertexIdSet::new());
            }
            let ids = idx
                .scan_vertices_eq(key, value)
                .map_err(|e| GleaphError::Memory(e.to_string()))?;
            let set: VertexIdSet = ids.into_iter().collect();
            return Ok(self.minus_tombstoned(set));
        }
        // Fallback: open ABP from region metadata if available.
        if let (_persist, Some(regions)) = read_reserved_metas(&self.mem)
            && regions.secondary_index_offset > 0
            && regions.secondary_index_len > 0
            && crate::abp_tree::AbpStoreHeader::read_from(&self.mem, regions.secondary_index_offset)
                .is_some()
        {
            return self.scan_vertices_by_property_eq_abp(
                self.mem.clone(),
                regions.secondary_index_offset,
                key,
                value,
            );
        }
        Ok(self.scan_vertices_by_property_eq(key, value))
    }

    /// Builds a stable-memory `(a,b)+ tree` property-store snapshot for currently visible
    /// overlay-backed vertex/edge properties.
    pub fn build_abp_property_store(
        &self,
        mem: M,
        region_start: u64,
    ) -> Result<AbpPropertyStore<M>, GleaphError>
    where
        M: Clone,
    {
        let mut store = AbpPropertyStore::new(mem, region_start)
            .map_err(|e| GleaphError::Memory(e.to_string()))?;
        for (vertex_id, props) in &self.vertex_props {
            if self.tombstoned_vertices.contains(*vertex_id) {
                continue;
            }
            for (prop_name, prop_value) in props {
                store
                    .set_vertex_prop(*vertex_id, prop_name, prop_value.clone())
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
            }
        }
        for (&edge_id, overlay) in &self.edge_props {
            if self.is_edge_tombstoned(overlay.src, overlay.dst, Some(overlay.label.as_str())) {
                continue;
            }
            for (prop_name, prop_value) in &overlay.props {
                store
                    .set_edge_prop_by_id(edge_id, prop_name, prop_value.clone())
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
            }
        }
        Ok(store)
    }

    /// Builds a stable `VertexMetaTable` snapshot from the in-memory `vertex_labels` map.
    ///
    /// Creates a fresh ABP B+ tree at `region_start` and populates it with all
    /// vertex label entries.
    pub fn build_abp_vertex_meta_snapshot(
        &self,
        mem: M,
        region_start: u64,
    ) -> Result<VertexMetaTable<M>, GleaphError>
    where
        M: Clone,
    {
        let mut table = VertexMetaTable::create(mem, region_start)
            .map_err(|e| GleaphError::Memory(e.to_string()))?;

        for (vertex_id, labels) in &self.vertex_labels {
            if labels.is_empty() {
                continue;
            }
            let meta = VertexMeta {
                labels: self.vertex_label_ids_to_names(labels),
            };
            table
                .set_vertex_meta(*vertex_id, &meta)
                .map_err(|e| GleaphError::Memory(e.to_string()))?;
        }
        Ok(table)
    }

    /// Restores in-memory `vertex_labels`, `label_index`, and `label_live_count`
    /// from a stable `VertexMetaTable`.
    ///
    /// Must be called after tombstone restore so that `label_live_count` is correct.
    /// Note: only vertex labels in the label_index are cleared; edge labels are preserved.
    pub fn restore_from_abp_vertex_meta<N: Memory>(&mut self, table: &VertexMetaTable<N>) {
        // Clear vertex labels from the label index (edge labels are left intact).
        // We do this by removing each vertex's labels from the index, then clearing vertex_labels.
        for (vertex_id, labels) in &self.vertex_labels {
            for label_id in labels {
                self.label_index
                    .remove_vertex_label_id(*vertex_id, *label_id);
            }
        }
        self.vertex_labels.clear();
        self.label_live_count.clear();

        for (vertex_id, meta) in table.iter_all() {
            let mut label_ids = Vec::new();
            for label in &meta.labels {
                let label_id = self.label_index.ensure_label_id(label);
                self.label_index.add_vertex_label_id(vertex_id, label_id);
                if let Err(pos) = label_ids.binary_search(&label_id) {
                    label_ids.insert(pos, label_id);
                }
            }
            self.vertex_labels.insert(vertex_id, label_ids);
        }

        // Rebuild label_live_count from vertex_labels excluding tombstoned vertices.
        for (vertex_id, labels) in &self.vertex_labels {
            if !self.tombstoned_vertices.contains(*vertex_id) {
                for label_id in labels {
                    if let Some(label) = self.label_index.label_name(*label_id) {
                        *self.label_live_count.entry(label.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    /// Applies a single-property delta to a stable `(a,b)+ tree` property-store snapshot.
    ///
    /// This is a helper for future incremental sync wiring; callers are responsible for ensuring
    /// the graph mutation has already been applied to the overlay state.
    pub fn apply_vertex_prop_delta_to_abp_property_store<N: Memory>(
        &self,
        store: &mut AbpPropertyStore<N>,
        vertex_id: u32,
        key: &str,
        new_value: Option<&gleaph_types::Value>,
    ) -> Result<(), GleaphError> {
        if self.tombstoned_vertices.contains(vertex_id) || (vertex_id as u64) >= self.num_vertices {
            let _ = store.delete_vertex_prop(vertex_id, key);
            return Ok(());
        }
        match new_value {
            Some(v) => store
                .set_vertex_prop(vertex_id, key, v.clone())
                .map_err(|e| GleaphError::Memory(e.to_string())),
            None => store
                .delete_vertex_prop(vertex_id, key)
                .map_err(|e| GleaphError::Memory(e.to_string())),
        }
    }

    /// Applies a full-property-map add/remove operation for one vertex to a stable property-store
    /// snapshot. `present=true` inserts/updates all props; `present=false` deletes them.
    pub fn apply_vertex_props_to_abp_property_store<N: Memory>(
        &self,
        store: &mut AbpPropertyStore<N>,
        vertex_id: u32,
        props: &PropertyMap,
        present: bool,
    ) -> Result<(), GleaphError> {
        for (key, value) in props {
            if present {
                store.set_vertex_prop(vertex_id, key, value.clone())
            } else {
                store.delete_vertex_prop(vertex_id, key)
            }
            .map_err(|e| GleaphError::Memory(e.to_string()))?;
        }
        Ok(())
    }

    /// Applies a full edge-property-map add/remove operation for one edge to a stable property-store
    /// snapshot. `present=true` inserts/updates all props; `present=false` deletes them.
    pub fn apply_edge_props_to_abp_property_store<N: Memory>(
        &self,
        store: &mut AbpPropertyStore<N>,
        src: u32,
        dst: u32,
        label: &str,
        props: &PropertyMap,
        present: bool,
    ) -> Result<(), GleaphError> {
        let edge_id = self
            .resolve_edge_overlay_key(src, dst, Some(label))
            .map(|(edge_id, _)| edge_id)
            .ok_or_else(|| {
                GleaphError::ExecutionError("edge binding does not resolve to a live edge".into())
            })?;
        for (key, value) in props {
            if present {
                store.set_edge_prop_by_id(edge_id, key, value.clone())
            } else {
                store.delete_edge_prop_by_id(edge_id, key)
            }
            .map_err(|e| GleaphError::Memory(e.to_string()))?;
        }
        Ok(())
    }

    /// Applies a single edge-property delta to a stable `(a,b)+ tree` property-store snapshot.
    pub fn apply_edge_prop_delta_to_abp_property_store<N: Memory>(
        &self,
        store: &mut AbpPropertyStore<N>,
        src: u32,
        dst: u32,
        label: Option<&str>,
        key: &str,
        new_value: Option<&gleaph_types::Value>,
    ) -> Result<(), GleaphError> {
        let edge_label = label.unwrap_or_default();
        let Some(edge_id) = self.resolve_edge_id(src, dst, Some(edge_label)) else {
            return Ok(());
        };
        match new_value {
            Some(v) => store
                .set_edge_prop_by_id(edge_id, key, v.clone())
                .map_err(|e| GleaphError::Memory(e.to_string())),
            None => store
                .delete_edge_prop_by_id(edge_id, key)
                .map_err(|e| GleaphError::Memory(e.to_string())),
        }
    }

    /// Attaches a live stable-memory equality index at the given region offset.
    ///
    /// After this call, all base mutation methods (`create_vertex`, `set_vertex_prop`, etc.)
    /// will incrementally maintain the ABP B-tree alongside the in-memory equality index.
    pub fn attach_live_eq_index(&mut self, region_offset: u64) -> Result<(), GleaphError>
    where
        M: Clone,
    {
        let idx = AbpSecondaryEqIndex::from_memory(self.mem.clone(), region_offset)
            .map_err(|e| GleaphError::Memory(e.to_string()))?;
        self.live_eq_index = Some(idx);
        Ok(())
    }

    /// Detaches and returns the live equality index handle, if any.
    pub fn detach_live_eq_index(&mut self) -> Option<AbpSecondaryEqIndex<M>> {
        self.live_eq_index.take()
    }

    /// Returns `true` if a live stable-memory equality index is attached.
    pub fn has_live_eq_index(&self) -> bool {
        self.live_eq_index.is_some()
    }

    /// Clears the in-memory range index (for testing: proves ABP tree provides the data).
    pub fn clear_in_memory_range_index(&mut self) {
        self.vertex_prop_range_index.clear();
    }

    /// Builds a stable-memory `(a,b)+ tree` equality index snapshot for currently registered
    /// vertex equality indexes using the graph's overlay-backed property view.
    pub fn build_abp_secondary_index(
        &self,
        mem: M,
        region_start: u64,
    ) -> Result<AbpSecondaryEqIndex<M>, GleaphError>
    where
        M: Clone,
    {
        let mut idx = AbpSecondaryEqIndex::new(mem, region_start)
            .map_err(|e| GleaphError::Memory(e.to_string()))?;
        for (vertex_id, props) in &self.vertex_props {
            if self.tombstoned_vertices.contains(*vertex_id) {
                continue;
            }
            for (prop_name, prop_value) in props {
                let eq_desc = PropertyIndex {
                    entity_type: EntityType::Vertex,
                    property_name: prop_name.clone(),
                    index_type: IndexType::Equality,
                };
                if self.property_indexes.contains(&eq_desc) {
                    idx.add_vertex_eq(prop_name, prop_value, *vertex_id)
                        .map_err(|e| GleaphError::Memory(e.to_string()))?;
                }
                let range_desc = PropertyIndex {
                    entity_type: EntityType::Vertex,
                    property_name: prop_name.clone(),
                    index_type: IndexType::Range,
                };
                if self.property_indexes.contains(&range_desc) {
                    idx.add_vertex_range(prop_name, prop_value, *vertex_id)
                        .map_err(|e| GleaphError::Memory(e.to_string()))?;
                }
            }
        }
        Ok(idx)
    }

    /// Applies a full-property-map add/remove operation for a single vertex to a stable-memory
    /// `(a,b)+ tree` equality index, restricted to registered vertex equality indexes.
    ///
    /// `present=true` adds postings for the provided props; `present=false` removes them.
    pub fn apply_vertex_props_to_abp_secondary_eq_index<N: Memory>(
        &self,
        idx: &mut AbpSecondaryEqIndex<N>,
        vertex_id: u32,
        props: &PropertyMap,
        present: bool,
    ) -> Result<(), GleaphError> {
        for (prop_name, prop_value) in props {
            // Equality index maintenance.
            let eq_desc = PropertyIndex {
                entity_type: EntityType::Vertex,
                property_name: prop_name.clone(),
                index_type: IndexType::Equality,
            };
            if self.property_indexes.contains(&eq_desc) {
                if present {
                    idx.add_vertex_eq(prop_name, prop_value, vertex_id)
                        .map_err(|e| GleaphError::Memory(e.to_string()))?;
                } else {
                    idx.remove_vertex_eq(prop_name, prop_value, vertex_id)
                        .map_err(|e| GleaphError::Memory(e.to_string()))?;
                }
            }
            // Range index maintenance.
            let range_desc = PropertyIndex {
                entity_type: EntityType::Vertex,
                property_name: prop_name.clone(),
                index_type: IndexType::Range,
            };
            if self.property_indexes.contains(&range_desc) {
                if present {
                    idx.add_vertex_range(prop_name, prop_value, vertex_id)
                        .map_err(|e| GleaphError::Memory(e.to_string()))?;
                } else {
                    idx.remove_vertex_range(prop_name, prop_value, vertex_id)
                        .map_err(|e| GleaphError::Memory(e.to_string()))?;
                }
            }
        }
        Ok(())
    }

    /// Applies a single-property mutation delta (`SET`/`REMOVE`) for one vertex to a stable
    /// `(a,b)+ tree` equality index, restricted to registered vertex equality indexes.
    pub fn apply_vertex_prop_delta_to_abp_secondary_eq_index<N: Memory>(
        &self,
        idx: &mut AbpSecondaryEqIndex<N>,
        vertex_id: u32,
        key: &str,
        old_value: Option<&gleaph_types::Value>,
        new_value: Option<&gleaph_types::Value>,
    ) -> Result<(), GleaphError> {
        // Equality index delta.
        let eq_desc = PropertyIndex {
            entity_type: EntityType::Vertex,
            property_name: key.to_string(),
            index_type: IndexType::Equality,
        };
        if self.property_indexes.contains(&eq_desc) {
            if let Some(old) = old_value {
                idx.remove_vertex_eq(key, old, vertex_id)
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
            }
            if let Some(newv) = new_value {
                idx.add_vertex_eq(key, newv, vertex_id)
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
            }
        }
        // Range index delta.
        let range_desc = PropertyIndex {
            entity_type: EntityType::Vertex,
            property_name: key.to_string(),
            index_type: IndexType::Range,
        };
        if self.property_indexes.contains(&range_desc) {
            if let Some(old) = old_value {
                idx.remove_vertex_range(key, old, vertex_id)
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
            }
            if let Some(newv) = new_value {
                idx.add_vertex_range(key, newv, vertex_id)
                    .map_err(|e| GleaphError::Memory(e.to_string()))?;
            }
        }
        Ok(())
    }

    /// Queries a stable-memory `(a,b)+ tree` equality index region for a registered vertex
    /// equality index definition.
    pub fn scan_vertices_by_property_eq_abp(
        &self,
        mem: M,
        region_start: u64,
        key: &str,
        value: &gleaph_types::Value,
    ) -> Result<VertexIdSet, GleaphError>
    where
        M: Clone,
    {
        if !self.has_vertex_equality_index(key) {
            return Ok(VertexIdSet::new());
        }
        let idx = AbpSecondaryEqIndex::from_memory(mem, region_start)
            .map_err(|e| GleaphError::Memory(e.to_string()))?;
        let ids = idx
            .scan_vertices_eq(key, value)
            .map_err(|e| GleaphError::Memory(e.to_string()))?;
        let set: VertexIdSet = ids.into_iter().collect();
        Ok(self.minus_tombstoned(set))
    }

    pub fn create_index(
        &mut self,
        entity_type: EntityType,
        property_name: String,
        index_type: IndexType,
    ) -> Result<(), GleaphError> {
        match (entity_type, index_type) {
            (EntityType::Vertex, IndexType::Equality) => {
                let created = self.property_indexes.insert(PropertyIndex {
                    entity_type,
                    property_name: property_name.clone(),
                    index_type,
                });
                if created {
                    self.backfill_vertex_equality_index(&property_name);
                }
                Ok(())
            }
            (EntityType::Edge, IndexType::Equality) => {
                let created = self.property_indexes.insert(PropertyIndex {
                    entity_type,
                    property_name: property_name.clone(),
                    index_type,
                });
                if created {
                    self.backfill_edge_equality_index(&property_name);
                }
                Ok(())
            }
            (EntityType::Vertex, IndexType::Range) => {
                let created = self.property_indexes.insert(PropertyIndex {
                    entity_type,
                    property_name: property_name.clone(),
                    index_type,
                });
                if created {
                    self.backfill_vertex_range_index(&property_name);
                }
                Ok(())
            }
            (EntityType::Edge, IndexType::Range) => Err(GleaphError::ExecutionError(
                "range indexes on edges are not yet supported".into(),
            )),
        }
    }

    pub fn list_property_indexes(&self) -> Vec<PropertyIndex> {
        self.property_indexes.iter().cloned().collect()
    }

    pub fn drop_index(
        &mut self,
        entity_type: EntityType,
        property_name: String,
        index_type: IndexType,
    ) -> Result<(), GleaphError> {
        let idx = PropertyIndex {
            entity_type,
            property_name: property_name.clone(),
            index_type,
        };
        if !self.property_indexes.remove(&idx) {
            return Err(GleaphError::ExecutionError(format!(
                "index on {entity_type:?}({property_name}) does not exist"
            )));
        }
        // Remove in-memory index entries for this property.
        match (entity_type, index_type) {
            (EntityType::Vertex, IndexType::Equality) => {
                self.vertex_prop_eq_index
                    .retain(|(pn, _), _| pn != &property_name);
            }
            (EntityType::Vertex, IndexType::Range) => {
                self.vertex_prop_range_index
                    .retain(|(pn, _), _| pn != &property_name);
            }
            (EntityType::Edge, IndexType::Equality) => {
                self.edge_prop_eq_index
                    .retain(|(pn, _), _| pn != &property_name);
                self.edge_prop_eq_by_src
                    .retain(|(pn, _, _), _| pn != &property_name);
                self.edge_prop_eq_by_dst
                    .retain(|(pn, _, _), _| pn != &property_name);
            }
            (EntityType::Edge, IndexType::Range) => {}
        }
        Ok(())
    }

    pub fn delete_edge_prop(
        &mut self,
        src: u32,
        dst: u32,
        label: Option<&str>,
        key: &str,
    ) -> Result<(), GleaphError> {
        if self.edge_record(src, dst, label).is_none() {
            return Err(GleaphError::ValidationError(
                "edge binding does not resolve to a live edge".into(),
            ));
        }
        let (edge_id, _) = self
            .resolve_edge_overlay_key(src, dst, label)
            .ok_or_else(|| {
                GleaphError::ValidationError("edge binding does not resolve to a live edge".into())
            })?;
        // Deindex old value before removing.
        if let Some(overlay) = self.edge_props.get(&edge_id)
            && let Some((_, old_v)) = overlay.props.iter().find(|(k, _)| k == key)
        {
            self.deindex_edge_prop(edge_id, src, dst, key, &old_v.clone());
        }
        if let Some(overlay) = self.edge_props.get_mut(&edge_id) {
            overlay.props.retain(|(k, _)| k != key);
        }
        let kid = self.intern_edge_prop_key(key);
        self.mark_property_dirty_by_id(kid);
        self.reservoir_observe(kid, edge_id, hash_property_value(&gleaph_types::Value::Null));
        Ok(())
    }

    pub fn delete_edge_prop_with_abp_property_store<N: Memory>(
        &mut self,
        store: &mut AbpPropertyStore<N>,
        src: u32,
        dst: u32,
        label: Option<&str>,
        key: &str,
    ) -> Result<(), GleaphError> {
        self.delete_edge_prop(src, dst, label, key)?;
        self.apply_edge_prop_delta_to_abp_property_store(store, src, dst, label, key, None)
    }

    pub fn scan_vertices_by_label(&self, label: &str) -> VertexIdSet {
        let set = self.label_index.scan_vertices_by_label(label);
        self.minus_tombstoned(set)
    }

    /// Returns the count of active (non-tombstoned) vertices for each label.
    ///
    /// O(1) — counters are maintained incrementally during mutations.
    /// Used by the query planner to populate [`gleaph_gql::stats::TableStats::label_cardinality`].
    pub fn label_cardinalities(&self) -> BTreeMap<String, u64> {
        self.label_live_count.clone()
    }

    /// Scan `vertex_prop_eq_index` for a single property. Returns (distinct_values, total_live).
    fn scan_vertex_prop_indexed(&self, prop_name: &str) -> (u64, u64) {
        let mut distinct: u64 = 0;
        let mut total: u64 = 0;
        for ((pn, _), vertex_ids) in &self.vertex_prop_eq_index {
            if pn != prop_name {
                continue;
            }
            let live = vertex_ids
                .iter()
                .filter(|v| !self.tombstoned_vertices.contains(*v))
                .count() as u64;
            if live > 0 {
                distinct += 1;
                total += live;
            }
        }
        (distinct, total)
    }

    /// Scan edge property index for a single property. Returns (distinct_values, total_live).
    fn scan_edge_prop_indexed(&self, prop_name: &str) -> (u64, u64) {
        let mut distinct: u64 = 0;
        let mut total: u64 = 0;
        for ((pn, _), edge_ids) in &self.edge_prop_eq_index {
            if pn != prop_name {
                continue;
            }
            let value_live: u64 =
                edge_ids.iter().filter(|edge_id| self.edge_props.contains_key(edge_id)).count() as u64;
            if value_live > 0 {
                distinct += 1;
                total += value_live;
            }
        }
        (distinct, total)
    }

    /// Recomputes property selectivity estimates and resets the mutation counter.
    ///
    /// For **indexed** properties the in-memory equality index (`vertex_prop_eq_index`) provides
    /// exact distinct / total counts — no sampling needed.
    /// For **non-indexed** properties a sampling fallback (up to 1 000 vertices) is used.
    ///
    /// Selectivity is defined as `distinct_values / total_count` (clamped to `[0, 1]`).
    /// A value near 1.0 means high cardinality (every value unique, good for index).
    /// A value near 0.0 means low cardinality (single repeated value, index less useful).
    pub fn compute_property_selectivity(&mut self) {
        let mut selectivity = BTreeMap::new();

        // ── Vertex properties ──
        let indexed_props = self.collect_indexed_props(EntityType::Vertex);
        self.compute_indexed_selectivity(
            &indexed_props,
            "vertex",
            |s, name| s.scan_vertex_prop_indexed(name),
            &mut selectivity,
        );
        let non_indexed_props = self.collect_non_indexed_vertex_props(&indexed_props);
        if !non_indexed_props.is_empty() {
            let sampled_keys = self.sample_keys_from_vertex_props(SELECTIVITY_SAMPLE_SIZE);
            let total_live = self
                .vertex_props
                .len()
                .saturating_sub(self.tombstoned_vertices.len() as usize);
            for prop_name in &non_indexed_props {
                let key = format!("vertex:{prop_name}");
                let kid = self.prop_key_intern.intern(&key);
                if self.try_reservoir_selectivity(kid, total_live, &key, &mut selectivity) {
                    continue;
                }
                let (distinct, sampled) = self.sample_vertex_prop(&sampled_keys, prop_name);
                self.record_sampled_selectivity(
                    distinct,
                    sampled,
                    &key,
                    kid,
                    total_live,
                    sampled_keys.len(),
                    &mut selectivity,
                );
            }
        }

        // ── Edge properties ──
        let edge_indexed_props = self.collect_indexed_props(EntityType::Edge);
        self.compute_indexed_selectivity(
            &edge_indexed_props,
            "edge",
            |s, name| s.scan_edge_prop_indexed(name),
            &mut selectivity,
        );
        let non_indexed_edge_props = self.collect_non_indexed_edge_props(&edge_indexed_props);
        if !non_indexed_edge_props.is_empty() {
            let sampled_edge_keys = self.sample_keys_from_edge_props(SELECTIVITY_SAMPLE_SIZE);
            let total_live_edges = self.edge_props.len();
            for prop_name in &non_indexed_edge_props {
                let key = format!("edge:{prop_name}");
                let kid = self.prop_key_intern.intern(&key);
                if self.try_reservoir_selectivity(kid, total_live_edges, &key, &mut selectivity) {
                    continue;
                }
                let (distinct, sampled) = self.sample_edge_prop(&sampled_edge_keys, prop_name);
                self.record_sampled_selectivity(
                    distinct,
                    sampled,
                    &key,
                    kid,
                    total_live_edges,
                    sampled_edge_keys.len(),
                    &mut selectivity,
                );
            }
        }

        self.property_selectivity = selectivity;
        self.selectivity_dirty_counts.clear();

        if self.reservoir.is_empty() {
            self.seed_reservoir_from_overlay();
        }
    }

    fn collect_indexed_props(&self, entity: EntityType) -> BTreeSet<String> {
        self.property_indexes
            .iter()
            .filter(|idx| idx.entity_type == entity && idx.index_type == IndexType::Equality)
            .map(|idx| idx.property_name.clone())
            .collect()
    }

    fn compute_indexed_selectivity(
        &mut self,
        indexed_props: &BTreeSet<String>,
        prefix: &str,
        scan_fn: impl Fn(&Self, &str) -> (u64, u64),
        selectivity: &mut BTreeMap<String, f64>,
    ) {
        for prop_name in indexed_props {
            let (distinct, total) = scan_fn(self, prop_name);
            let sel = selectivity_from_counts(distinct, total);
            let key = format!("{prefix}:{prop_name}");
            let kid = self.prop_key_intern.intern(&key);
            selectivity.insert(key, sel);
            self.selectivity_baselines.insert(kid, total as u32);
        }
    }

    fn collect_non_indexed_vertex_props(&self, indexed: &BTreeSet<String>) -> BTreeSet<String> {
        let mut result = BTreeSet::new();
        for (vid, props) in &self.vertex_props {
            if !self.tombstoned_vertices.contains(*vid) {
                for (k, _) in props {
                    if !indexed.contains(k) {
                        result.insert(k.clone());
                    }
                }
            }
        }
        result
    }

    fn collect_non_indexed_edge_props(&self, indexed: &BTreeSet<String>) -> BTreeSet<String> {
        let mut result = BTreeSet::new();
        for overlay in self.edge_props.values() {
            if !self.is_edge_tombstoned(overlay.src, overlay.dst, Some(overlay.label.as_str())) {
                for (k, _) in &overlay.props {
                    if !indexed.contains(k) {
                        result.insert(k.clone());
                    }
                }
            }
        }
        result
    }

    fn try_reservoir_selectivity(
        &mut self,
        kid: PropKeyId,
        total_entities: usize,
        key: &str,
        selectivity: &mut BTreeMap<String, f64>,
    ) -> bool {
        if let Some((res_distinct, res_count)) = self.estimate_from_reservoir(kid) {
            let sel = selectivity_from_counts(res_distinct, res_count);
            let estimated =
                (res_count * total_entities as u64 / self.reservoir.len().max(1) as u64) as u32;
            selectivity.insert(key.to_string(), sel);
            self.selectivity_baselines.insert(kid, estimated);
            true
        } else {
            false
        }
    }

    fn sample_vertex_prop(
        &self,
        sampled_keys: &[u32],
        prop_name: &str,
    ) -> (RapidHashSet<u64>, usize) {
        let mut distinct: RapidHashSet<u64> = RapidHashSet::default();
        let mut sampled = 0usize;
        for &vid in sampled_keys {
            if self.tombstoned_vertices.contains(vid) {
                continue;
            }
            if let Some(props) = self.vertex_props.get(&vid)
                && let Some((_, v)) = props.iter().find(|(k, _)| k == prop_name)
            {
                distinct.insert(hash_property_value(v));
                sampled += 1;
            }
        }
        (distinct, sampled)
    }

    fn sample_edge_prop(
        &self,
        sampled_keys: &[u32],
        prop_name: &str,
    ) -> (RapidHashSet<u64>, usize) {
        let mut distinct: RapidHashSet<u64> = RapidHashSet::default();
        let mut sampled = 0usize;
        for edge_id in sampled_keys {
            let Some(overlay) = self.edge_props.get(edge_id) else {
                continue;
            };
            if self.is_edge_tombstoned(overlay.src, overlay.dst, Some(overlay.label.as_str())) {
                continue;
            }
            if let Some((_, v)) = overlay.props.iter().find(|(k, _)| k == prop_name)
            {
                distinct.insert(hash_property_value(v));
                sampled += 1;
            }
        }
        (distinct, sampled)
    }

    #[allow(clippy::too_many_arguments)]
    fn record_sampled_selectivity(
        &mut self,
        distinct: RapidHashSet<u64>,
        sampled: usize,
        key: &str,
        kid: PropKeyId,
        total_live: usize,
        sample_pool_size: usize,
        selectivity: &mut BTreeMap<String, f64>,
    ) {
        if sampled < MIN_PROPERTY_SAMPLE {
            if let Some(&old) = self.property_selectivity.get(key) {
                selectivity.insert(key.to_string(), old);
                return;
            }
            if sampled == 0 {
                return;
            }
        }
        let sel = (distinct.len() as f64 / sampled as f64).min(1.0);
        selectivity.insert(key.to_string(), sel);
        let estimated = if sample_pool_size == 0 {
            0
        } else {
            (sampled as u64 * total_live as u64 / sample_pool_size as u64) as u32
        };
        self.selectivity_baselines.insert(kid, estimated);
    }

    /// Fisher-Yates partial shuffle to pick up to `n` random keys from `vertex_props`.
    fn sample_keys_from_vertex_props(&mut self, n: usize) -> Vec<u32> {
        let mut keys: Vec<u32> = self.vertex_props.keys().copied().collect();
        let len = keys.len();
        let sample_count = n.min(len);
        for i in 0..sample_count {
            let j = i + self.rng.next_bounded((len - i) as u64) as usize;
            keys.swap(i, j);
        }
        keys.truncate(sample_count);
        keys
    }

    /// Fisher-Yates partial shuffle to pick up to `n` random keys from `edge_props`.
    fn sample_keys_from_edge_props(&mut self, n: usize) -> Vec<u32> {
        let mut keys: Vec<u32> = self.edge_props.keys().copied().collect();
        let len = keys.len();
        let sample_count = n.min(len);
        for i in 0..sample_count {
            let j = i + self.rng.next_bounded((len - i) as u64) as usize;
            keys.swap(i, j);
        }
        keys.truncate(sample_count);
        keys
    }

    /// Selective re-estimation for specific property keys.
    /// Partitions keys into vertex/edge × indexed/non-indexed and updates only those.
    pub fn compute_selectivity_for_properties(&mut self, dirty_keys: &[String]) {
        let vertex_indexed: BTreeSet<String> = self
            .property_indexes
            .iter()
            .filter(|idx| {
                idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality
            })
            .map(|idx| idx.property_name.clone())
            .collect();
        let edge_indexed: BTreeSet<String> = self
            .property_indexes
            .iter()
            .filter(|idx| {
                idx.entity_type == EntityType::Edge && idx.index_type == IndexType::Equality
            })
            .map(|idx| idx.property_name.clone())
            .collect();

        // Collect which raw property names need re-estimation, partitioned by entity type.
        let mut vertex_indexed_refresh = Vec::new();
        let mut vertex_sampled_refresh = Vec::new();
        let mut edge_indexed_refresh = Vec::new();
        let mut edge_sampled_refresh = Vec::new();
        for key in dirty_keys {
            if let Some(prop) = key.strip_prefix("vertex:") {
                if vertex_indexed.contains(prop) {
                    vertex_indexed_refresh.push(prop.to_string());
                } else {
                    vertex_sampled_refresh.push(prop.to_string());
                }
            } else if let Some(prop) = key.strip_prefix("edge:") {
                if edge_indexed.contains(prop) {
                    edge_indexed_refresh.push(prop.to_string());
                } else {
                    edge_sampled_refresh.push(prop.to_string());
                }
            }
        }

        // ── Vertex indexed ──
        for prop_name in &vertex_indexed_refresh {
            let (distinct, total) = self.scan_vertex_prop_indexed(prop_name);
            let sel = selectivity_from_counts(distinct, total);
            let key = format!("vertex:{prop_name}");
            let kid = self.prop_key_intern.intern(&key);
            self.property_selectivity.insert(key, sel);
            self.selectivity_baselines.insert(kid, total as u32);
        }

        // ── Vertex sampled ──
        if !vertex_sampled_refresh.is_empty() {
            let total_live = self
                .vertex_props
                .len()
                .saturating_sub(self.tombstoned_vertices.len() as usize);
            // Try reservoir first for each property; collect those that need Fisher-Yates.
            let mut need_sample: Vec<String> = Vec::new();
            for prop_name in &vertex_sampled_refresh {
                let key = format!("vertex:{prop_name}");
                let kid = self.prop_key_intern.intern(&key);
                if let Some((res_distinct, res_count)) = self.estimate_from_reservoir(kid) {
                    let sel = selectivity_from_counts(res_distinct, res_count);
                    let estimated =
                        (res_count * total_live as u64 / self.reservoir.len().max(1) as u64) as u32;
                    self.property_selectivity.insert(key, sel);
                    self.selectivity_baselines.insert(kid, estimated);
                } else {
                    need_sample.push(prop_name.clone());
                }
            }
            if !need_sample.is_empty() {
                let sampled_keys = self.sample_keys_from_vertex_props(SELECTIVITY_SAMPLE_SIZE);
                for prop_name in &need_sample {
                    let mut distinct: RapidHashSet<u64> = RapidHashSet::default();
                    let mut sampled = 0usize;
                    for &vid in &sampled_keys {
                        if self.tombstoned_vertices.contains(vid) {
                            continue;
                        }
                        if let Some(props) = self.vertex_props.get(&vid)
                            && let Some((_, v)) = props.iter().find(|(k, _)| k == prop_name)
                        {
                            distinct.insert(hash_property_value(v));
                            sampled += 1;
                        }
                    }
                    if sampled < MIN_PROPERTY_SAMPLE {
                        continue;
                    }
                    let sel = (distinct.len() as f64 / sampled as f64).min(1.0);
                    let key = format!("vertex:{prop_name}");
                    let kid = self.prop_key_intern.intern(&key);
                    self.property_selectivity.insert(key, sel);
                    let estimated = if sampled_keys.is_empty() {
                        0
                    } else {
                        (sampled as u64 * total_live as u64 / sampled_keys.len() as u64) as u32
                    };
                    self.selectivity_baselines.insert(kid, estimated);
                }
            }
        }

        // ── Edge indexed ──
        for prop_name in &edge_indexed_refresh {
            let (distinct, total) = self.scan_edge_prop_indexed(prop_name);
            let sel = selectivity_from_counts(distinct, total);
            let key = format!("edge:{prop_name}");
            let kid = self.prop_key_intern.intern(&key);
            self.property_selectivity.insert(key, sel);
            self.selectivity_baselines.insert(kid, total as u32);
        }

        // ── Edge sampled ──
        if !edge_sampled_refresh.is_empty() {
            let total_live_edges = self.edge_props.len();
            let mut need_sample: Vec<String> = Vec::new();
            for prop_name in &edge_sampled_refresh {
                let key = format!("edge:{prop_name}");
                let kid = self.prop_key_intern.intern(&key);
                if let Some((res_distinct, res_count)) = self.estimate_from_reservoir(kid) {
                    let sel = selectivity_from_counts(res_distinct, res_count);
                    let estimated = (res_count * total_live_edges as u64
                        / self.reservoir.len().max(1) as u64)
                        as u32;
                    self.property_selectivity.insert(key, sel);
                    self.selectivity_baselines.insert(kid, estimated);
                } else {
                    need_sample.push(prop_name.clone());
                }
            }
            if !need_sample.is_empty() {
                let sampled_edge_keys = self.sample_keys_from_edge_props(SELECTIVITY_SAMPLE_SIZE);
                for prop_name in &need_sample {
                    let mut distinct: RapidHashSet<u64> = RapidHashSet::default();
                    let mut sampled = 0usize;
                    for edge_id in &sampled_edge_keys {
                        let Some(overlay) = self.edge_props.get(edge_id) else {
                            continue;
                        };
                        if self.is_edge_tombstoned(
                            overlay.src,
                            overlay.dst,
                            Some(overlay.label.as_str()),
                        ) {
                            continue;
                        }
                        if let Some((_, v)) = overlay.props.iter().find(|(k, _)| k == prop_name)
                        {
                            distinct.insert(hash_property_value(v));
                            sampled += 1;
                        }
                    }
                    if sampled < MIN_PROPERTY_SAMPLE {
                        continue;
                    }
                    let sel = (distinct.len() as f64 / sampled as f64).min(1.0);
                    let key = format!("edge:{prop_name}");
                    let kid = self.prop_key_intern.intern(&key);
                    self.property_selectivity.insert(key, sel);
                    let estimated = if sampled_edge_keys.is_empty() {
                        0
                    } else {
                        (sampled as u64 * total_live_edges as u64 / sampled_edge_keys.len() as u64)
                            as u32
                    };
                    self.selectivity_baselines.insert(kid, estimated);
                }
            }
        }
    }

    /// Recomputes property selectivity if stale.
    ///
    /// - If `property_selectivity` is empty and the graph has overlay properties, performs a
    ///   full `compute_property_selectivity()`.
    /// - Otherwise, checks per-property dirty ratios against `DIRTY_RATIO_THRESHOLD` and
    ///   selectively re-estimates only exceeded properties.
    pub fn refresh_selectivity_if_stale(&mut self) {
        let _ = self.refresh_selectivity_if_stale_with_flag();
    }

    /// Like [`Self::refresh_selectivity_if_stale`] but returns whether recomputation ran.
    pub fn refresh_selectivity_if_stale_with_flag(&mut self) -> bool {
        // Initial computation: first time with data but no selectivity yet.
        if self.property_selectivity.is_empty() && self.has_overlay_properties() {
            self.compute_property_selectivity();
            return true;
        }

        if self.selectivity_dirty_counts.is_empty() {
            return false;
        }

        // Per-property dirty ratio check.
        let num_vertices = self.num_vertices as u32;
        let num_edges = self.num_edges as u32;
        let mut refresh_ids: Vec<PropKeyId> = Vec::new();
        for (&kid, &dirty_count) in &self.selectivity_dirty_counts {
            let baseline = self
                .selectivity_baselines
                .get(&kid)
                .copied()
                .unwrap_or_else(|| {
                    let key_str = self.prop_key_intern.resolve(kid);
                    if key_str.starts_with("vertex:") {
                        num_vertices
                    } else {
                        num_edges
                    }
                });
            let dirty_ratio = dirty_count as f64 / baseline.max(1) as f64;
            if dirty_ratio >= DIRTY_RATIO_THRESHOLD {
                refresh_ids.push(kid);
            }
        }

        if refresh_ids.is_empty() {
            return false;
        }

        // Resolve interned ids to string keys for compute_selectivity_for_properties.
        let refresh_keys: Vec<String> = refresh_ids
            .iter()
            .map(|&kid| self.prop_key_intern.resolve(kid).to_string())
            .collect();
        self.compute_selectivity_for_properties(&refresh_keys);
        // Clear dirty counts only for refreshed properties.
        for kid in &refresh_ids {
            self.selectivity_dirty_counts.remove(kid);
        }
        true
    }

    /// Seeds the PRNG with the given value.
    ///
    /// Call this once after graph creation (not restoration) to provide entropy.
    /// On the IC, pass `ic_cdk::api::time()` (nanosecond timestamp).
    /// In tests, the default fixed seed (`0xDEAD_BEEF_CAFE_BABE`) provides reproducibility.
    pub fn seed_rng(&mut self, seed: u64) {
        self.rng = Prng::new(seed);
    }

    /// Returns the cached property-selectivity map.
    /// Call `compute_property_selectivity()` first to populate it.
    pub fn get_property_selectivity(&self) -> &BTreeMap<String, f64> {
        &self.property_selectivity
    }

    #[cfg(test)]
    pub fn selectivity_dirty_count_for_test(&self, key: &str) -> u32 {
        // Look up the interned id for `key`, then fetch the dirty count.
        if let Some(&id) = self.prop_key_intern.to_id.get(key) {
            self.selectivity_dirty_counts.get(&id).copied().unwrap_or(0)
        } else {
            0
        }
    }

    #[cfg(test)]
    pub fn selectivity_baseline_for_test(&self, key: &str) -> u32 {
        if let Some(&id) = self.prop_key_intern.to_id.get(key) {
            self.selectivity_baselines.get(&id).copied().unwrap_or(0)
        } else {
            0
        }
    }

    #[cfg(test)]
    pub fn rng_state_for_test(&self) -> u64 {
        self.rng.state()
    }

    #[cfg(test)]
    pub fn prop_key_intern_len_for_test(&self) -> usize {
        self.prop_key_intern.len()
    }

    #[cfg(test)]
    pub fn reservoir_len_for_test(&self) -> usize {
        self.reservoir.len()
    }

    #[cfg(test)]
    pub fn reservoir_total_seen_for_test(&self) -> u64 {
        self.reservoir_total_seen
    }

    /// Builds a [`PlannerStats`] snapshot from current in-memory state.
    pub fn planner_stats(&self) -> PlannerStats {
        let vertex_count = self.vertex_count();
        let edge_count = self.edge_count();
        PlannerStats {
            label_cardinality: self
                .label_live_count
                .iter()
                .map(|(k, &v)| (k.clone(), v))
                .collect(),
            avg_degree: if vertex_count == 0 {
                0.0
            } else {
                edge_count as f64 / vertex_count as f64
            },
            property_selectivity: self
                .property_selectivity
                .iter()
                .map(|(k, &v)| (k.clone(), v))
                .collect(),
            indexed_vertex_properties: self
                .property_indexes
                .iter()
                .filter(|idx| {
                    idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Equality
                })
                .map(|idx| idx.property_name.clone())
                .collect(),
            range_indexed_vertex_properties: self
                .property_indexes
                .iter()
                .filter(|idx| {
                    idx.entity_type == EntityType::Vertex && idx.index_type == IndexType::Range
                })
                .map(|idx| idx.property_name.clone())
                .collect(),
            vertex_count,
            edge_count,
        }
    }

    /// Ensures the property-store region is allocated in the stable-memory header.
    ///
    /// If the region is already allocated (non-zero offset and length) this is a no-op.
    /// Otherwise it places the region immediately after all other non-PMA regions and writes the
    /// updated [`PmaReservedRegionsMeta`] back to the header.
    ///
    /// Returns the region start offset.
    pub fn ensure_property_store_region(&mut self, min_len: u64) -> Result<u64, GleaphError> {
        use crate::abp_tree::{ABP_PAGE_SIZE, ABP_STORE_HEADER_LEN};
        let min_len = min_len.max(ABP_STORE_HEADER_LEN + u64::from(ABP_PAGE_SIZE));

        let (persist_opt, regions_opt) = read_reserved_metas(&self.mem);
        let mut regions = regions_opt.unwrap_or_else(ReservedRegionsMeta::new_valid);

        if regions.property_store_offset > 0 && regions.property_store_len >= min_len {
            return Ok(regions.property_store_offset);
        }

        let pma_end = layout::total_memory_needed(
            self.num_vertices,
            self.elem_capacity,
            u64::from(self.segment_count),
        );
        if regions.non_pma_base == 0 {
            let overlay_len = persist_opt
                .map(|p| u64::from(p.overlay_alloc_len.max(p.overlay_len)))
                .unwrap_or(0);
            let overlay_off = persist_opt.map(|p| p.overlay_offset).unwrap_or(0);
            regions.infer_non_pma_base_if_missing(if overlay_len > 0 {
                Some((overlay_off, overlay_len))
            } else {
                None
            });
            if regions.non_pma_base == 0 {
                regions.non_pma_base = pma_end;
            }
        }

        // Compute next free offset after all existing non-PMA regions.
        let mut next_free = regions.non_pma_base;
        let overlay_end = persist_opt
            .map(|p| p.overlay_offset + u64::from(p.overlay_alloc_len.max(p.overlay_len)))
            .unwrap_or(0);
        next_free = next_free.max(overlay_end);
        if regions.secondary_index_offset > 0 && regions.secondary_index_len > 0 {
            next_free = next_free.max(regions.secondary_index_offset + regions.secondary_index_len);
        }

        if regions.property_store_len == 0 {
            regions.property_store_offset = next_free;
        }
        regions.property_store_len = regions.property_store_len.max(min_len);

        let required_end = regions.property_store_offset + regions.property_store_len;
        ensure_mem_size(&mut self.mem, required_end)?;
        write_reserved_metas(&mut self.mem, None, Some(regions))?;
        Ok(regions.property_store_offset)
    }

    /// Ensures the secondary-index region is allocated in the stable-memory header.
    ///
    /// Places the region after the property-store region (if any) and after all other non-PMA
    /// regions.  Returns the region start offset.
    pub fn ensure_secondary_index_region(&mut self, min_len: u64) -> Result<u64, GleaphError> {
        use crate::abp_tree::{ABP_PAGE_SIZE, ABP_STORE_HEADER_LEN};
        let min_len = min_len.max(ABP_STORE_HEADER_LEN + u64::from(ABP_PAGE_SIZE));

        let (persist_opt, regions_opt) = read_reserved_metas(&self.mem);
        let mut regions = regions_opt.unwrap_or_else(ReservedRegionsMeta::new_valid);

        if regions.secondary_index_offset > 0 && regions.secondary_index_len >= min_len {
            return Ok(regions.secondary_index_offset);
        }

        let pma_end = layout::total_memory_needed(
            self.num_vertices,
            self.elem_capacity,
            u64::from(self.segment_count),
        );
        if regions.non_pma_base == 0 {
            let overlay_len = persist_opt
                .map(|p| u64::from(p.overlay_alloc_len.max(p.overlay_len)))
                .unwrap_or(0);
            let overlay_off = persist_opt.map(|p| p.overlay_offset).unwrap_or(0);
            regions.infer_non_pma_base_if_missing(if overlay_len > 0 {
                Some((overlay_off, overlay_len))
            } else {
                None
            });
            if regions.non_pma_base == 0 {
                regions.non_pma_base = pma_end;
            }
        }

        let mut next_free = regions.non_pma_base;
        let overlay_end = persist_opt
            .map(|p| p.overlay_offset + u64::from(p.overlay_alloc_len.max(p.overlay_len)))
            .unwrap_or(0);
        next_free = next_free.max(overlay_end);
        if regions.property_store_offset > 0 && regions.property_store_len > 0 {
            next_free = next_free.max(regions.property_store_offset + regions.property_store_len);
        }

        if regions.secondary_index_len == 0 {
            regions.secondary_index_offset = next_free;
        }
        regions.secondary_index_len = regions.secondary_index_len.max(min_len);

        let required_end = regions.secondary_index_offset + regions.secondary_index_len;
        ensure_mem_size(&mut self.mem, required_end)?;
        write_reserved_metas(&mut self.mem, None, Some(regions))?;
        Ok(regions.secondary_index_offset)
    }

    pub fn vertex_has_label(&self, vertex_id: u32, label: &str) -> bool {
        self.vertex_labels
            .get(&vertex_id)
            .and_then(|labels| {
                self.label_index
                    .label_id(label)
                    .map(|label_id| (labels, label_id))
            })
            .is_some_and(|(labels, label_id)| labels.binary_search(&label_id).is_ok())
            && !self.tombstoned_vertices.contains(vertex_id)
    }

    /// Like `vertex_has_label` but skips the tombstone check.
    /// Use when the caller has already verified the vertex is not tombstoned.
    #[inline]
    pub fn vertex_has_label_unchecked(&self, vertex_id: u32, label: &str) -> bool {
        self.vertex_labels
            .get(&vertex_id)
            .and_then(|labels| {
                self.label_index
                    .label_id(label)
                    .map(|label_id| (labels, label_id))
            })
            .is_some_and(|(labels, label_id)| labels.binary_search(&label_id).is_ok())
    }

    #[inline]
    pub fn vertex_has_label_id(&self, vertex_id: u32, label_id: u32) -> bool {
        self.vertex_labels
            .get(&vertex_id)
            .is_some_and(|labels| labels.binary_search(&label_id).is_ok())
            && !self.tombstoned_vertices.contains(vertex_id)
    }

    #[inline]
    pub fn vertex_has_label_id_unchecked(&self, vertex_id: u32, label_id: u32) -> bool {
        self.vertex_labels
            .get(&vertex_id)
            .is_some_and(|labels| labels.binary_search(&label_id).is_ok())
    }

    pub fn edge_matches_label(&self, src: u32, dst: u32, expected_label: &str) -> bool {
        let label_id = self.resolve_edge_label_id(Some(expected_label));
        self.find_edge_locator(src, dst, label_id)
            .and_then(|locator| self.read_edge_at_locator(locator))
            .is_some_and(|edge| !edge.is_tombstoned())
    }

    pub fn edge_label(&self, src: u32, dst: u32) -> Option<String> {
        incr_edge_label_calls();
        self.edge_payload_for_pair(src, dst)
            .and_then(|edge| self.label_index.label_name(edge.label_id()))
            .map(str::to_string)
    }

    /// Returns the edge label as a borrowed `&str`, avoiding heap allocation.
    pub fn edge_label_ref(&self, src: u32, dst: u32) -> Option<&str> {
        self.edge_payload_for_pair(src, dst)
            .and_then(|edge| self.label_index.label_name(edge.label_id()))
    }

    pub fn edge_id_for_labeled(&self, src: u32, dst: u32, label: Option<&str>) -> u32 {
        self.resolve_edge_id(src, dst, label).unwrap_or(0)
    }

    /// Returns rich reverse-neighbor entries for `target`, with tombstone filtering
    /// and dedup already applied. The returned `RevEntry` values contain packed
    /// label/flag state and `edge_id`, eliminating the need for separate lookups.
    pub fn reverse_neighbors_rich(&self, target: u32) -> Vec<RevEntry> {
        if self.is_vertex_tombstoned(target) {
            return Vec::new();
        }
        let Some(label_buckets) = self.rev_index.get(&target) else {
            return Vec::new();
        };
        let mut out = Vec::<RevEntry>::new();
        // Dedup by (src, label_id) — a single source vertex can have edges
        // with different labels (e.g., Bought AND Favorited to the same target).
        // Deduplicating only on `src` would silently drop valid edges.
        let mut seen = RapidHashMap::<(u32, u32), usize>::default();
        for entries in label_buckets.values() {
            for entry in entries {
                if entry.is_tombstoned() || self.is_vertex_tombstoned(entry.src) {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(v) =
                    seen.entry((entry.src, entry.label_id()))
                {
                    v.insert(out.len());
                    out.push(*entry);
                }
            }
        }
        out
    }

    /// Iterates over incoming edges for a vertex, calling `f` for each pre-filtered entry.
    ///
    /// Optional filters applied before tombstone/dedup processing:
    /// - `label_filter`: skip entries whose `label_id()` ≠ the given id (u32 comparison).
    /// - `ts_range`: skip entries whose timestamp is outside the range (u64 comparison).
    ///
    /// Both filters are placed before tombstone checks, label-name lookups, and dedup,
    /// so they skip all downstream processing for non-matching entries.
    ///
    /// Unlike `reverse_neighbors_rich`, avoids heap allocation for the result set.
    pub fn for_each_reverse_neighbor<F>(
        &self,
        target: u32,
        label_filter: Option<u32>,
        ts_range: Option<&TimestampRange>,
        f: &mut F,
    ) -> Result<(), GleaphError>
    where
        F: FnMut(RevEntry) -> Result<(), GleaphError>,
    {
        if self.is_vertex_tombstoned(target) {
            return Ok(());
        }
        let Some(label_buckets) = self.rev_index.get(&target) else {
            return Ok(());
        };
        if let Some(filter_id) = label_filter {
            let mut seen = RapidHashMap::<u32, ()>::default();
            let Some(entries) = label_buckets.get(&filter_id) else {
                return Ok(());
            };
            for entry in entries {
                if let Some(r) = ts_range
                    && !ts_matches(r, entry.timestamp)
                {
                    continue;
                }
                if entry.is_tombstoned() || self.is_vertex_tombstoned(entry.src) {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(v) = seen.entry(entry.src) {
                    v.insert(());
                    f(*entry)?;
                }
            }
            return Ok(());
        }

        // Dedup by (src, label_id) — same source can have edges with different labels.
        let mut seen = RapidHashMap::<(u32, u32), ()>::default();
        for entries in label_buckets.values() {
            for entry in entries {
                // Timestamp filter — u64 comparison, before expensive tombstone/dedup.
                if let Some(r) = ts_range
                    && !ts_matches(r, entry.timestamp)
                {
                    continue;
                }
                if entry.is_tombstoned() || self.is_vertex_tombstoned(entry.src) {
                    continue;
                }
                if let std::collections::hash_map::Entry::Vacant(v) =
                    seen.entry((entry.src, entry.label_id()))
                {
                    v.insert(());
                    f(*entry)?;
                }
            }
        }
        Ok(())
    }

    pub fn is_edge_tombstoned(&self, src: u32, dst: u32, label: Option<&str>) -> bool {
        incr_is_edge_tombstoned_calls();
        let label_id = self.resolve_edge_label_id(label);
        self.find_edge_locator(src, dst, label_id)
            .and_then(|locator| self.read_edge_at_locator(locator))
            .is_some_and(|edge| edge.is_tombstoned())
    }

    #[inline]
    pub fn has_tombstoned_edges(&self) -> bool {
        for src in 0..(self.vertex_count() as u32) {
            let Ok(edges) = self.collect_neighbors(src) else {
                continue;
            };
            if edges.into_iter().any(|edge| edge.is_tombstoned()) {
                return true;
            }
        }
        false
    }

    #[inline]
    pub fn is_vertex_tombstoned(&self, vertex_id: u32) -> bool {
        !self.tombstoned_vertices.is_empty() && self.tombstoned_vertices.contains(vertex_id)
    }

    /// Subtracts tombstoned vertices from a set. No-op when tombstones are empty.
    #[inline]
    fn minus_tombstoned(&self, set: VertexIdSet) -> VertexIdSet {
        if self.tombstoned_vertices.is_empty() {
            set
        } else {
            &set - &self.tombstoned_vertices
        }
    }

    /// Returns a reference to the set of tombstoned vertex IDs.
    pub fn tombstoned_vertex_set(&self) -> &VertexIdSet {
        &self.tombstoned_vertices
    }

    /// Replaces the in-memory tombstoned vertex set from a stable-memory bitset.
    ///
    /// This clears the existing set and repopulates it, updating `label_live_count`
    /// accordingly.
    pub fn restore_tombstoned_vertices_from_set(&mut self, set: VertexIdSet) {
        // Un-count labels of previously tombstoned vertices that are being un-tombstoned.
        // Then clear and repopulate. We rebuild label_live_count entirely for correctness.
        self.tombstoned_vertices = set;
        // Rebuild label_live_count from vertex_labels excluding tombstoned.
        self.label_live_count.clear();
        for (vertex_id, labels) in &self.vertex_labels {
            if !self.tombstoned_vertices.contains(*vertex_id) {
                for label_id in labels {
                    if let Some(label) = self.label_index.label_name(*label_id) {
                        *self.label_live_count.entry(label.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
    }

    pub fn edge_record(&self, src: u32, dst: u32, label: Option<&str>) -> Option<EdgeRecord> {
        incr_edge_record_calls();
        let label_name = label.unwrap_or_default().to_string();
        if self.is_edge_tombstoned(src, dst, Some(label_name.as_str())) {
            return None;
        }
        let label_id = self.resolve_edge_label_id(Some(label_name.as_str()));
        let payload = self
            .find_edge_locator(src, dst, label_id)
            .and_then(|locator| self.read_edge_at_locator(locator))?;
        let props = self
            .edge_props
            .get(&payload.edge_id)
            .map(|overlay| overlay.props.clone())
            .unwrap_or_default();
        Some(EdgeRecord {
            src,
            dst,
            label: if label_name.is_empty() {
                None
            } else {
                Some(label_name)
            },
            weight: Some(payload.weight),
            timestamp: Some(payload.timestamp),
            props,
        })
    }

    pub fn overlay_snapshot(&self) -> GraphOverlaySnapshot {
        let mut vertex_labels = self
            .vertex_labels
            .iter()
            .map(|(k, v)| (*k, self.vertex_label_ids_to_names(v)))
            .collect::<Vec<_>>();
        vertex_labels.sort_by_key(|(id, _)| *id);

        let mut vertex_props = self
            .vertex_props
            .iter()
            .map(|(k, v)| (*k, v.clone()))
            .collect::<Vec<_>>();
        vertex_props.sort_by_key(|(id, _)| *id);

        let mut edge_props = self
            .edge_props
            .iter()
            .map(|(&edge_id, overlay)| EdgePropsSnapshot {
                edge_id,
                src: overlay.src,
                dst: overlay.dst,
                label: overlay.label.clone(),
                props: overlay.props.clone(),
            })
            .collect::<Vec<_>>();
        edge_props.sort_by_key(|entry| entry.edge_id);
        let property_indexes = self.property_indexes.iter().cloned().collect::<Vec<_>>();

        let mut tombstoned_vertices = self.tombstoned_vertices.iter().collect::<Vec<_>>();
        tombstoned_vertices.sort_unstable();

        let mut dirty_counts: Vec<(u16, u32)> = self
            .selectivity_dirty_counts
            .iter()
            .map(|(&k, &v)| (k, v))
            .collect();
        dirty_counts.sort_by_key(|(k, _)| *k);
        let mut baselines: Vec<(u16, u32)> = self
            .selectivity_baselines
            .iter()
            .map(|(&k, &v)| (k, v))
            .collect();
        baselines.sort_by_key(|(k, _)| *k);
        let mut intern_table: Vec<(u16, String)> = self
            .prop_key_intern
            .iter()
            .map(|(id, s)| (id, s.to_string()))
            .collect();
        intern_table.sort_by_key(|(id, _)| *id);

        GraphOverlaySnapshot {
            vertex_labels,
            vertex_props,
            edge_props,
            property_indexes,
            tombstoned_vertices,
            next_created_vertex_id: self.next_created_vertex_id,
            property_selectivity: self.property_selectivity.clone(),
            label_id_map: self.label_index.label_id_map_snapshot(),
            selectivity_dirty_counts: dirty_counts,
            selectivity_baselines: baselines,
            selectivity_rng_state: self.rng.state(),
            prop_key_intern_table: intern_table,
            selectivity_reservoir: self
                .reservoir
                .iter()
                .map(|e| (e.entity_id, e.prop_key_id, e.value_hash))
                .collect(),
            reservoir_total_seen: self.reservoir_total_seen,
        }
    }

    pub fn restore_overlay_snapshot(
        &mut self,
        snapshot: GraphOverlaySnapshot,
    ) -> Result<(), GleaphError> {
        self.label_index = LabelIndex::default();
        // Pre-seed label_id ↔ name mapping so that IDs match those baked into
        // EdgeEntry.label_id in stable memory.  Without this, `ensure_label_id`
        // would assign new sequential IDs in restore order (alphabetical) which
        // differ from the creation-time order.
        if !snapshot.label_id_map.is_empty() {
            self.label_index
                .restore_from_label_id_map(snapshot.label_id_map);
        }
        self.vertex_labels.clear();
        self.label_live_count.clear();
        self.vertex_props.clear();
        self.vertex_prop_eq_index.clear();
        self.edge_props.clear();
        self.edge_prop_eq_index.clear();
        self.edge_prop_eq_by_src.clear();
        self.edge_prop_eq_by_dst.clear();
        self.property_indexes.clear();
        self.tombstoned_vertices.clear();

        for (vertex_id, labels) in snapshot.vertex_labels {
            let mut label_ids = Vec::new();
            for label in labels {
                let label_id = self.label_index.ensure_label_id(&label);
                self.label_index.add_vertex_label_id(vertex_id, label_id);
                if let Err(pos) = label_ids.binary_search(&label_id) {
                    label_ids.insert(pos, label_id);
                }
            }
            self.vertex_labels.insert(vertex_id, label_ids);
        }
        for idx in snapshot.property_indexes {
            self.property_indexes.insert(idx);
        }
        for (vertex_id, props) in snapshot.vertex_props {
            self.index_vertex_props(vertex_id, &props);
            self.vertex_props.insert(vertex_id, props);
        }
        for entry in snapshot.edge_props {
            let EdgePropsSnapshot {
                edge_id,
                src,
                dst,
                label,
                props,
            } = entry;
            let live = !self.is_edge_tombstoned(src, dst, Some(label.as_str()));
            // Ensure next_edge_id stays ahead of any restored edge id.
            if edge_id >= self.next_edge_id {
                self.next_edge_id = edge_id.saturating_add(1);
            }
            self.edge_props.insert(
                edge_id,
                EdgePropsOverlay {
                    src,
                    dst,
                    label,
                    props,
                },
            );
            if live && let Some(overlay) = self.edge_props.get(&edge_id).cloned() {
                self.index_edge_props(edge_id, overlay.src, overlay.dst, &overlay.props);
            }
        }
        for v in snapshot.tombstoned_vertices {
            self.tombstoned_vertices.insert(v);
        }
        // Recompute label_live_count from vertex_labels excluding tombstoned vertices.
        for (vertex_id, labels) in &self.vertex_labels {
            if !self.tombstoned_vertices.contains(*vertex_id) {
                for label_id in labels {
                    if let Some(label) = self.label_index.label_name(*label_id) {
                        *self.label_live_count.entry(label.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
        self.next_created_vertex_id = snapshot.next_created_vertex_id;
        self.property_selectivity = snapshot.property_selectivity;
        // Restore intern table first so dirty counts / baselines resolve correctly.
        self.prop_key_intern = PropKeyIntern::from_pairs(snapshot.prop_key_intern_table);
        // Restore incremental selectivity tracking state.
        self.selectivity_dirty_counts = snapshot.selectivity_dirty_counts.into_iter().collect();
        self.selectivity_baselines = snapshot.selectivity_baselines.into_iter().collect();
        if snapshot.selectivity_rng_state != 0 {
            self.rng = Prng::new(snapshot.selectivity_rng_state);
        }
        // Restore reservoir.
        self.reservoir = snapshot
            .selectivity_reservoir
            .into_iter()
            .map(|(entity_id, prop_key_id, value_hash)| ReservoirEntry {
                entity_id,
                prop_key_id,
                value_hash,
            })
            .collect();
        self.reservoir_total_seen = snapshot.reservoir_total_seen;
        // Backfill edge equality indexes from restored edge_props.
        let edge_idx_props: Vec<String> = self
            .property_indexes
            .iter()
            .filter(|idx| {
                idx.entity_type == EntityType::Edge && idx.index_type == IndexType::Equality
            })
            .map(|idx| idx.property_name.clone())
            .collect();
        for prop_name in edge_idx_props {
            self.backfill_edge_equality_index(&prop_name);
        }
        // Fix rev_index label_ids (built before overlay restored labels).
        self.rebuild_rev_index_labels();
        Ok(())
    }

    fn encode_index_value(value: &gleaph_types::Value) -> Option<Vec<u8>> {
        encode_value(value).ok()
    }

    fn index_vertex_props(&mut self, vertex_id: u32, props: &PropertyMap) {
        for (k, v) in props {
            self.index_vertex_prop(vertex_id, k, v);
            self.index_vertex_range_prop(vertex_id, k, v);
        }
    }

    fn deindex_vertex_props(&mut self, vertex_id: u32, props: &PropertyMap) {
        for (k, v) in props {
            self.deindex_vertex_prop(vertex_id, k, v);
            self.deindex_vertex_range_prop(vertex_id, k, v);
        }
    }

    fn index_vertex_prop(&mut self, vertex_id: u32, key: &str, value: &gleaph_types::Value) {
        if !self.has_vertex_equality_index(key) {
            return;
        }
        let Some(enc) = Self::encode_index_value(value) else {
            return;
        };
        self.vertex_prop_eq_index
            .entry((key.to_string(), enc))
            .or_default()
            .insert(vertex_id);
    }

    fn deindex_vertex_prop(&mut self, vertex_id: u32, key: &str, value: &gleaph_types::Value) {
        if !self.has_vertex_equality_index(key) {
            return;
        }
        let Some(enc) = Self::encode_index_value(value) else {
            return;
        };
        let idx_key = (key.to_string(), enc.clone());
        if let Some(ids) = self.vertex_prop_eq_index.get_mut(&idx_key) {
            ids.remove(vertex_id);
            if ids.is_empty() {
                self.vertex_prop_eq_index.remove(&idx_key);
            }
        }
    }

    fn has_vertex_equality_index(&self, key: &str) -> bool {
        self.property_indexes.contains(&PropertyIndex {
            entity_type: EntityType::Vertex,
            property_name: key.to_string(),
            index_type: IndexType::Equality,
        })
    }

    fn backfill_vertex_equality_index(&mut self, key: &str) {
        let rows = self
            .vertex_props
            .iter()
            .filter_map(|(vertex_id, props)| {
                props
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| (*vertex_id, v.clone()))
            })
            .collect::<Vec<_>>();
        for (vertex_id, value) in rows {
            self.index_vertex_prop(vertex_id, key, &value);
        }
    }

    // ── Vertex property range index helpers ─────────────────────────────

    fn has_vertex_range_index(&self, key: &str) -> bool {
        self.property_indexes.contains(&PropertyIndex {
            entity_type: EntityType::Vertex,
            property_name: key.to_string(),
            index_type: IndexType::Range,
        })
    }

    fn index_vertex_range_prop(&mut self, vertex_id: u32, key: &str, value: &gleaph_types::Value) {
        if !self.has_vertex_range_index(key) {
            return;
        }
        let Ok(enc) = crate::property_store::encode_value_ordered(value) else {
            return;
        };
        self.vertex_prop_range_index
            .entry((key.to_string(), enc))
            .or_default()
            .insert(vertex_id);
    }

    fn deindex_vertex_range_prop(
        &mut self,
        vertex_id: u32,
        key: &str,
        value: &gleaph_types::Value,
    ) {
        if !self.has_vertex_range_index(key) {
            return;
        }
        let Ok(enc) = crate::property_store::encode_value_ordered(value) else {
            return;
        };
        let idx_key = (key.to_string(), enc.clone());
        if let Some(ids) = self.vertex_prop_range_index.get_mut(&idx_key) {
            ids.remove(vertex_id);
            if ids.is_empty() {
                self.vertex_prop_range_index.remove(&idx_key);
            }
        }
    }

    fn backfill_vertex_range_index(&mut self, key: &str) {
        let rows = self
            .vertex_props
            .iter()
            .filter_map(|(vertex_id, props)| {
                props
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| (*vertex_id, v.clone()))
            })
            .collect::<Vec<_>>();
        for (vertex_id, value) in rows {
            self.index_vertex_range_prop(vertex_id, key, &value);
        }
    }

    // ── Edge property equality index helpers ───────────────────────────

    fn has_edge_equality_index(&self, key: &str) -> bool {
        self.property_indexes.contains(&PropertyIndex {
            entity_type: EntityType::Edge,
            property_name: key.to_string(),
            index_type: IndexType::Equality,
        })
    }

    fn index_edge_prop(
        &mut self,
        edge_id: u32,
        src: u32,
        dst: u32,
        key: &str,
        value: &gleaph_types::Value,
    ) {
        if !self.has_edge_equality_index(key) {
            return;
        }
        let Some(enc) = Self::encode_index_value(value) else {
            return;
        };
        let key_name = key.to_string();
        let idx_key = (key_name.clone(), enc.clone());
        self.edge_prop_eq_index.entry(idx_key).or_default().insert(edge_id);
        self.edge_prop_eq_by_src
            .entry((key_name.clone(), enc.clone(), src))
            .or_default()
            .push(edge_id);
        self.edge_prop_eq_by_dst
            .entry((key_name, enc, dst))
            .or_default()
            .push(edge_id);
    }

    fn deindex_edge_prop(
        &mut self,
        edge_id: u32,
        src: u32,
        dst: u32,
        key: &str,
        value: &gleaph_types::Value,
    ) {
        if !self.has_edge_equality_index(key) {
            return;
        }
        let Some(enc) = Self::encode_index_value(value) else {
            return;
        };
        let idx_key = (key.to_string(), enc.clone());
        if let Some(ids) = self.edge_prop_eq_index.get_mut(&idx_key) {
            ids.remove(&edge_id);
            if ids.is_empty() {
                self.edge_prop_eq_index.remove(&idx_key);
            }
        }
        let by_src_key = (key.to_string(), enc.clone(), src);
        if let Some(ids) = self.edge_prop_eq_by_src.get_mut(&by_src_key) {
            ids.retain(|stored| *stored != edge_id);
            if ids.is_empty() {
                self.edge_prop_eq_by_src.remove(&by_src_key);
            }
        }
        let by_dst_key = (key.to_string(), enc, dst);
        if let Some(ids) = self.edge_prop_eq_by_dst.get_mut(&by_dst_key) {
            ids.retain(|stored| *stored != edge_id);
            if ids.is_empty() {
                self.edge_prop_eq_by_dst.remove(&by_dst_key);
            }
        }
    }

    fn index_edge_props(&mut self, edge_id: u32, src: u32, dst: u32, props: &PropertyMap) {
        for (k, v) in props {
            self.index_edge_prop(edge_id, src, dst, k, v);
        }
    }

    fn deindex_edge_props(&mut self, edge_id: u32, src: u32, dst: u32, props: &PropertyMap) {
        for (k, v) in props {
            self.deindex_edge_prop(edge_id, src, dst, k, v);
        }
    }

    fn backfill_edge_equality_index(&mut self, key: &str) {
        let rows = self
            .edge_props
            .iter()
            .filter_map(|(&edge_id, overlay)| {
                overlay
                    .props
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| (edge_id, overlay.src, overlay.dst, v.clone()))
            })
            .collect::<Vec<_>>();
        for (edge_id, src, dst, value) in rows {
            self.index_edge_prop(edge_id, src, dst, key, &value);
        }
    }

    /// Scan edges matching a property equality predicate using the in-memory index.
    pub fn scan_edges_by_property_eq(
        &self,
        key: &str,
        value: &gleaph_types::Value,
    ) -> Vec<(u32, u32)> {
        self.scan_edges_by_property_eq_rich(key, value)
            .into_iter()
            .map(|edge| (edge.src, edge.dst))
            .collect()
    }

    /// Scan edges matching a property equality predicate using the in-memory index,
    /// preserving cached edge identity and label information from the overlay.
    pub fn scan_edges_by_property_eq_rich(
        &self,
        key: &str,
        value: &gleaph_types::Value,
    ) -> Vec<IndexedEdgeMatch> {
        let Some(enc) = Self::encode_index_value(value) else {
            return Vec::new();
        };
        self.edge_prop_eq_index
            .get(&(key.to_string(), enc))
            .cloned()
            .unwrap_or_default()
            .into_iter()
            .filter_map(|edge_id| {
                self.edge_props.get(&edge_id).map(|overlay| IndexedEdgeMatch {
                    src: overlay.src,
                    dst: overlay.dst,
                    label_id: self.resolve_edge_label_id(Some(overlay.label.as_str())),
                    edge_id,
                })
            })
            .collect()
    }

    /// Returns the set of target vertex IDs reachable from `src` via edges matching
    /// the given property equality, using the in-memory edge property index.
    /// Returns `None` if no edge property index exists for `key`.
    /// O(1) lookup into the by-src index.
    pub fn edge_index_targets_for_src(
        &self,
        key: &str,
        value: &gleaph_types::Value,
        src: u32,
    ) -> Option<Vec<u32>> {
        if !self.has_edge_equality_index(key) {
            return None;
        }
        let enc = Self::encode_index_value(value)?;
        let targets = self
            .edge_prop_eq_by_src
            .get(&(key.to_string(), enc, src))
            .map(|edge_ids| {
                edge_ids
                    .iter()
                    .filter_map(|edge_id| self.edge_props.get(edge_id).map(|overlay| overlay.dst))
                    .collect()
            })
            .unwrap_or_default();
        Some(targets)
    }

    /// Returns the set of source vertex IDs that have edges to `dst` matching
    /// the given property equality, using the in-memory edge property index.
    /// Returns `None` if no edge property index exists for `key`.
    /// O(1) lookup into the by-dst index.
    pub fn edge_index_sources_for_dst(
        &self,
        key: &str,
        value: &gleaph_types::Value,
        dst: u32,
    ) -> Option<Vec<u32>> {
        if !self.has_edge_equality_index(key) {
            return None;
        }
        let enc = Self::encode_index_value(value)?;
        let sources = self
            .edge_prop_eq_by_dst
            .get(&(key.to_string(), enc, dst))
            .map(|edge_ids| {
                edge_ids
                    .iter()
                    .filter_map(|edge_id| self.edge_props.get(edge_id).map(|overlay| overlay.src))
                    .collect()
            })
            .unwrap_or_default();
        Some(sources)
    }

    fn rebuild_vertex_offsets(&mut self) -> Result<(), GleaphError> {
        let mut cursor = 0u64;
        for vid in 0..self.num_vertices as u32 {
            let mut v = layout::read_vertex(&self.mem, vid);
            v.edge_index = cursor;
            if v.log_offset == 0 && v.degree == 0 {
                v.log_offset = -1;
            }
            layout::write_vertex(&mut self.mem, vid, &v);
            cursor = cursor.saturating_add(v.degree as u64 + 1);
        }
        self.recount_seg_total(0, self.num_vertices as u32);
        Ok(())
    }

    fn expand_vertices(&mut self, new_num_vertices: u64) -> Result<(), GleaphError> {
        if new_num_vertices <= self.num_vertices {
            return Ok(());
        }
        if new_num_vertices > u32::MAX as u64 {
            return Err(GleaphError::Unsupported(
                "vertex expansion beyond u32::MAX is not supported".to_string(),
            ));
        }

        let old_num_vertices = self.num_vertices;
        let mut all_edges: Vec<Vec<EdgeEntry>> = Vec::with_capacity(old_num_vertices as usize);
        for vid in 0..old_num_vertices as u32 {
            all_edges.push(self.collect_neighbors(vid)?);
        }

        let capped_vertices = new_num_vertices.min(u32::MAX as u64) as u32;
        let params = compute_capacity(capped_vertices, self.num_edges);

        let new_elem_capacity = self.elem_capacity.max(params.elem_capacity);
        let required = layout::total_memory_needed(
            new_num_vertices,
            new_elem_capacity,
            params.segment_count as u64,
        );
        relocate_reserved_non_pma_regions_for_pma_growth(&mut self.mem, required)?;
        // PMA growth can relocate the stable secondary-index region, invalidating any cached
        // live handle that still points at the pre-relocation offset.
        self.live_eq_index = None;

        self.num_vertices = new_num_vertices;
        self.segment_size = params.segment_size;
        self.segment_count = params.segment_count;
        self.tree_height = params.tree_height;
        self.elem_capacity = new_elem_capacity;
        self.edge_array_base = layout::edge_array_base(self.num_vertices);
        self.seg_tree_base = layout::seg_tree_base(self.num_vertices, self.elem_capacity);
        self.seg_log_base = layout::seg_log_base(
            self.num_vertices,
            self.elem_capacity,
            self.segment_count as u64,
        );
        self.seg_log_idx_base = layout::seg_log_idx_base(
            self.num_vertices,
            self.elem_capacity,
            self.segment_count as u64,
        );

        ensure_mem_size(&mut self.mem, required)?;

        for vid in 0..self.num_vertices as u32 {
            let degree = if (vid as u64) < old_num_vertices {
                all_edges[vid as usize].len() as u32
            } else {
                0
            };
            layout::write_vertex(
                &mut self.mem,
                vid,
                &VertexEntry {
                    edge_index: 0,
                    degree,
                    log_offset: -1,
                },
            );
        }

        let positions = self.calculate_positions(0, self.num_vertices as u32);
        for (vid, pos) in positions.into_iter().enumerate() {
            let mut v = layout::read_vertex(&self.mem, vid as u32);
            v.edge_index = pos;
            layout::write_vertex(&mut self.mem, vid as u32, &v);
            if (vid as u64) < old_num_vertices {
                for (j, edge) in all_edges[vid].iter().enumerate() {
                    layout::write_edge(&mut self.mem, self.edge_array_base, pos + j as u64, edge);
                }
            }
        }

        for seg in 0..self.segment_count {
            // The memory tail may contain non-PMA bytes (e.g. canister-managed overlay snapshots)
            // from a previous layout. After vertex expansion, the new log index region can overlap
            // those stale bytes, so read-before-zero may interpret a garbage fill count and cause
            // OOB log reads while draining.
            layout::write_seg_log_fill(&mut self.mem, self.seg_log_idx_base, seg, 0);
            let mut actual = 0u64;
            let seg_start = (seg * self.segment_size) as u64;
            let seg_end = ((seg + 1) * self.segment_size) as u64;
            for vid in seg_start..seg_end.min(self.num_vertices) {
                actual =
                    actual.saturating_add(layout::read_vertex(&self.mem, vid as u32).degree as u64);
            }
            layout::write_seg_actual(&mut self.mem, self.seg_tree_base, seg, actual);
        }
        self.recount_seg_total(0, self.num_vertices as u32);
        self.write_header()?;
        Ok(())
    }

    fn upper_threshold(&self, height: u32) -> f64 {
        if self.tree_height == 0 {
            return UP_H;
        }
        let h = height.min(self.tree_height) as f64;
        let th = self.tree_height as f64;
        UP_0 - h * ((UP_0 - UP_H) / th)
    }

    fn rewrite_vertex_after_rebalance(
        &mut self,
        ctx: &RebalanceWriteCtx,
        rel_idx: usize,
        old_pos: u64,
        new_pos: u64,
        on_seg_count: u32,
        log_edges: &[EdgeEntry],
    ) -> Result<(), GleaphError> {
        ensure_rebalance_budget_or_abort(ctx.instr_start)?;
        let vid = ctx.start_v + rel_idx as u32;

        if on_seg_count > 0 {
            if new_pos > old_pos {
                for j in (0..on_seg_count as u64).rev() {
                    let e = layout::read_edge(&self.mem, self.edge_array_base, old_pos + j);
                    layout::write_edge(&mut self.mem, self.edge_array_base, new_pos + j, &e);
                }
            } else {
                for j in 0..on_seg_count as u64 {
                    let e = layout::read_edge(&self.mem, self.edge_array_base, old_pos + j);
                    layout::write_edge(&mut self.mem, self.edge_array_base, new_pos + j, &e);
                }
            }
        }
        for (j, edge) in log_edges.iter().enumerate() {
            layout::write_edge(
                &mut self.mem,
                self.edge_array_base,
                new_pos + on_seg_count as u64 + j as u64,
                edge,
            );
        }

        let mut v = layout::read_vertex(&self.mem, vid);
        v.edge_index = new_pos;
        v.log_offset = -1;
        layout::write_vertex(&mut self.mem, vid, &v);
        Ok(())
    }

    fn collect_log_neighbors(&self, vertex_id: u32) -> Vec<EdgeEntry> {
        let v = layout::read_vertex(&self.mem, vertex_id);
        if v.log_offset < 0 {
            return Vec::new();
        }
        let seg = self.get_segment_id(vertex_id);
        let log = SegmentLog::for_segment(self.seg_log_base, seg, self.seg_log_idx_base);
        let mut out = Vec::new();
        let mut cur = v.log_offset;
        while cur >= 0 {
            if let Some(entry) = log.read_entry(&self.mem, cur as u32) {
                if entry.src == vertex_id {
                    out.push(EdgeEntry {
                        target: entry.dst,
                        weight: entry.weight,
                        timestamp: entry.timestamp,
                        label_and_flags: entry.label_and_flags,
                        edge_id: entry.edge_id,
                    });
                }
                cur = entry.prev_offset;
            } else {
                break;
            }
        }
        out
    }
}

impl<M: Memory> GraphView for PmaGraph<M> {
    fn vertex_count(&self) -> u64 {
        self.vertex_count()
    }

    fn edge_count(&self) -> u64 {
        self.edge_count()
    }

    fn neighbors(&self, vertex_id: u32) -> Vec<(u32, f32, u64)> {
        let edges = self
            .collect_neighbors_filtered(vertex_id)
            .unwrap_or_default();
        self.normalize_algo_neighbors(vertex_id, edges)
            .into_iter()
            .map(|e| (e.target, e.weight, e.timestamp))
            .collect()
    }

    fn neighbors_filtered(
        &self,
        vertex_id: u32,
        ts_range: Option<TimestampRange>,
    ) -> Vec<(u32, f32, u64)> {
        let edges = self
            .collect_neighbors_filtered(vertex_id)
            .unwrap_or_default();
        self.normalize_algo_neighbors(vertex_id, edges)
            .into_iter()
            .filter(|e| {
                let Some(range) = &ts_range else {
                    return true;
                };
                if let Some(start) = range.start
                    && e.timestamp < start
                {
                    return false;
                }
                if let Some(end) = range.end
                    && e.timestamp > end
                {
                    return false;
                }
                true
            })
            .map(|e| (e.target, e.weight, e.timestamp))
            .collect()
    }

    fn reverse_neighbors(&self, target: u32) -> Vec<(u32, f32, u64)> {
        self.reverse_neighbors_rich(target)
            .into_iter()
            .map(|r| (r.src, r.weight, r.timestamp))
            .collect()
    }

    fn is_vertex_active(&self, vertex_id: u32) -> bool {
        (vertex_id as u64) < self.vertex_count() && !self.is_vertex_tombstoned(vertex_id)
    }

    fn vertex_has_label(&self, vertex_id: u32, label: &str) -> bool {
        self.vertex_has_label(vertex_id, label)
    }

    fn edge_has_label(&self, src: u32, dst: u32, label: &str) -> bool {
        self.edge_matches_label(src, dst, label)
    }

    fn edge_label_ref(&self, src: u32, dst: u32) -> Option<&str> {
        PmaGraph::edge_label_ref(self, src, dst)
    }

    fn label_name_by_id(&self, label_id: u32) -> Option<&str> {
        self.label_index.label_name(label_id)
    }

    fn all_vertices(&self) -> Vec<u32> {
        let n = self.vertex_count() as u32;
        let mut all = VertexIdSet::from_sorted_iter(0..n).unwrap_or_default();
        if !self.tombstoned_vertices.is_empty() {
            all -= &self.tombstoned_vertices;
        }
        all.iter().collect()
    }
}

/// Inline timestamp range check for PMA-level pre-filtering.
#[inline]
fn ts_matches(range: &TimestampRange, timestamp: u64) -> bool {
    if let Some(start) = range.start
        && timestamp < start
    {
        return false;
    }
    if let Some(end) = range.end
        && timestamp > end
    {
        return false;
    }
    true
}

/// Clears per-segment counters and zeroes all overflow log storage.
fn clear_segments_and_logs<M: Memory>(
    mem: &mut M,
    seg_tree_base: u64,
    seg_log_base: u64,
    seg_log_idx_base: u64,
    segment_count: u32,
) {
    for seg in 0..segment_count {
        layout::write_seg_actual(mem, seg_tree_base, seg, 0);
        layout::write_seg_total(mem, seg_tree_base, segment_count, seg, 0);
        for slot in 0..layout::MAX_LOG_ENTRIES_PER_SEGMENT as u32 {
            layout::write_log_entry(mem, seg_log_base, seg, slot, &LogEntry::default());
        }
        layout::write_seg_log_fill(mem, seg_log_idx_base, seg, 0);
    }
}

/// Context shared while rewriting a rebalance window.
struct RebalanceWriteCtx {
    start_v: u32,
    instr_start: u64,
}

/// Grows the backing memory to at least `required` bytes.
fn ensure_mem_size<M: Memory>(mem: &mut M, required: u64) -> Result<(), GleaphError> {
    let size = mem.size_bytes();
    if required > size {
        mem.grow(required - size).map_err(mem_err)?;
    }
    Ok(())
}

fn read_reserved_metas<M: Memory>(
    mem: &M,
) -> (Option<ReservedPersistMeta>, Option<ReservedRegionsMeta>) {
    region_manager::read_metas(mem)
}

fn write_reserved_metas<M: Memory>(
    mem: &mut M,
    persist: Option<ReservedPersistMeta>,
    regions: Option<ReservedRegionsMeta>,
) -> Result<(), GleaphError> {
    region_manager::write_metas(mem, persist, regions)
}

fn relocate_reserved_non_pma_regions_for_pma_growth<M: Memory>(
    mem: &mut M,
    new_pma_end: u64,
) -> Result<(), GleaphError> {
    let (persist, regions_opt) = region_manager::read_metas(mem);
    if persist.is_none() && regions_opt.is_none() {
        return Ok(());
    }
    let mut regions = regions_opt.unwrap_or_default();
    region_manager::refresh_reserved_abp_region_lengths_from_headers(mem, &mut regions)?;
    write_reserved_metas(mem, persist, Some(regions))?;
    // Relocate even when persist (overlay snapshot) is absent — the 5 named
    // reserved regions (tombstone, vertex_meta, etc.) still need to move.
    region_manager::relocate_non_pma_regions(mem, new_pma_end, persist, regions)?;
    Ok(())
}

/// Converts a memory backend error into the shared graph error type.
fn mem_err(e: MemoryError) -> GleaphError {
    GleaphError::Memory(e.to_string())
}

/// Returns the first index whose new position moves to the right.
fn rebalance_pivot_index(old_positions: &[u64], new_positions: &[u64]) -> usize {
    let n = old_positions.len().min(new_positions.len());
    for i in 0..n {
        if new_positions[i] > old_positions[i] {
            return i;
        }
    }
    n
}

/// Returns the current IC instruction counter (or zero on non-wasm targets).
fn ic_instruction_counter() -> u64 {
    #[cfg(target_arch = "wasm32")]
    {
        ic_cdk::api::performance_counter(0)
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        0
    }
}

/// Returns whether the soft rebalance instruction budget has been exceeded.
fn instruction_budget_exceeded(start: u64) -> bool {
    #[cfg(target_arch = "wasm32")]
    {
        // Soft guard under the practical IC per-message limit.
        let used = ic_instruction_counter().saturating_sub(start);
        used > 18_000_000_000
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        let _ = start;
        false
    }
}

/// Aborts rebalancing when the instruction budget is exceeded.
fn ensure_rebalance_budget_or_abort(start: u64) -> Result<(), GleaphError> {
    if !instruction_budget_exceeded(start) {
        return Ok(());
    }

    #[cfg(target_arch = "wasm32")]
    {
        // Trap so the IC rolls back the partially-updated PMA state in this message.
        ic_cdk::trap("rebalance exceeded instruction budget");
    }

    #[cfg(not(target_arch = "wasm32"))]
    {
        Err(GleaphError::Unsupported(
            "rebalance exceeded instruction budget".to_string(),
        ))
    }
}

// ── Bulk insert helpers & core ────────────────────────────────────────────────

/// Degree threshold per-vertex above which we skip building the in-memory
/// existing-edge set and fall back to per-edge `collect_neighbors` checks.
const BULK_DEGREE_THRESHOLD: u32 = 10_000;

impl<M: Memory> PmaGraph<M> {
    // ── Phase 1 helpers ──────────────────────────────────────────────────

    /// Expands the vertex array once so that `max_vertex_id` is representable.
    fn ensure_vertices_batch(&mut self, max_vertex_id: u32) -> Result<(), GleaphError> {
        if max_vertex_id as u64 >= self.num_vertices {
            self.expand_vertices(max_vertex_id as u64 + 1)?;
        }
        Ok(())
    }

    /// Pre-allocates capacity so that `additional_edges` new edges can be inserted without
    /// triggering a mid-batch resize.
    fn ensure_capacity_for_batch(&mut self, additional_edges: u64) -> Result<(), GleaphError> {
        let needed = self.num_edges.saturating_add(additional_edges);
        while needed >= self.elem_capacity {
            self.resize()?;
        }
        Ok(())
    }

    /// Builds a `HashSet<(src, dst)>` of existing live edges for the given source vertices.
    /// Vertices whose degree exceeds `BULK_DEGREE_THRESHOLD` are excluded from the set;
    /// their edges will be checked individually via `collect_neighbors`.
    #[allow(clippy::type_complexity)]
    pub fn build_existing_edge_set(
        &self,
        src_vertices: &HashSet<u32>,
    ) -> Result<(HashSet<(u32, u32)>, HashSet<u32>), GleaphError> {
        let mut edge_set = HashSet::new();
        let mut high_degree_vertices = HashSet::new();
        for &src in src_vertices {
            if src as u64 >= self.num_vertices {
                continue;
            }
            let v = layout::read_vertex(&self.mem, src);
            if v.degree > BULK_DEGREE_THRESHOLD {
                high_degree_vertices.insert(src);
                continue;
            }
            for edge in self.collect_neighbors(src)? {
                edge_set.insert((src, edge.target));
            }
        }
        Ok((edge_set, high_degree_vertices))
    }

    // ── Phase 3 helper ───────────────────────────────────────────────────

    /// Rebalances all segments in `dirty_segments`. Consecutive dirty segments are merged
    /// into a single `rebalance_weighted` window to reduce overhead.
    fn global_rebalance_dirty_segments(
        &mut self,
        dirty_segments: &BTreeSet<u32>,
    ) -> Result<(), GleaphError> {
        if dirty_segments.is_empty() {
            return Ok(());
        }

        // Build consecutive runs of dirty segments.
        let mut runs: Vec<(u32, u32)> = Vec::new();
        let segs: Vec<u32> = dirty_segments.iter().copied().collect();
        if segs.is_empty() {
            return Ok(());
        }
        let mut i = 0;
        while i < segs.len() {
            let start = segs[i];
            let mut end = start;
            while i + 1 < segs.len() && segs[i + 1] == end + 1 {
                i += 1;
                end = segs[i];
            }
            runs.push((start, end));
            i += 1;
        }

        for (start_seg, end_seg) in runs {
            let start_v = (start_seg * self.segment_size).min(self.num_vertices as u32);
            let end_v = (((end_seg + 1) * self.segment_size) as u64).min(self.num_vertices) as u32;
            if start_v < end_v {
                self.rebalance_weighted(start_v, end_v)?;
            }
        }
        Ok(())
    }

    // ── Core bulk insert (raw PMA level) ─────────────────────────────────

    /// Inserts raw edges into the PMA in bulk, bypassing per-edge duplicate checks.
    ///
    /// The caller is responsible for ensuring no duplicates are present in `edges`.
    /// Tuple: `(src, dst, label_id, weight, timestamp)`.
    /// Returns a `BulkInsertResult` with per-input-edge edge IDs.
    pub fn bulk_insert_raw(
        &mut self,
        edges: &[(u32, u32, u32, f32, u64)],
    ) -> Result<BulkInsertResult, GleaphError> {
        if edges.is_empty() {
            return Ok(BulkInsertResult::default());
        }

        // Phase 1: Prepare
        let max_vid = edges
            .iter()
            .map(|&(s, d, _, _, _)| s.max(d))
            .max()
            .unwrap_or(0);
        self.ensure_vertices_batch(max_vid)?;
        self.ensure_capacity_for_batch(edges.len() as u64)?;

        // Build sorted index array for segment locality.
        let mut indices: Vec<usize> = (0..edges.len()).collect();
        indices.sort_by(|&a, &b| {
            let (sa, da, _, _, _) = edges[a];
            let (sb, db, _, _, _) = edges[b];
            (sa, da).cmp(&(sb, db))
        });

        // Phase 2: Insert
        let mut result = BulkInsertResult {
            inserted: 0,
            skipped: 0,
            edge_ids: vec![None; edges.len()],
        };
        let mut dirty_segments = BTreeSet::new();
        let mut out_batch: Vec<(u32, EdgeEntry)> = Vec::with_capacity(edges.len());
        let mut rev_batch: Vec<(u32, u32, u32, f32, u64, u32)> = Vec::with_capacity(edges.len());

        for &orig_idx in &indices {
            let (src, dst, label_id, weight, timestamp) = edges[orig_idx];
            let seg_id = self.get_segment_id(src);
            let mut v = layout::read_vertex(&self.mem, src);
            let slot = v.edge_index + u64::from(v.degree);

            if self.have_space_onseg(src, slot) {
                // Fast path: write directly into the on-segment slot.
                let edge_id = self.next_edge_id;
                let edge = EdgeEntry {
                    target: dst,
                    weight,
                    timestamp,
                    label_and_flags: pack_label_and_flags(label_id, 0),
                    edge_id,
                };
                layout::write_edge(&mut self.mem, self.edge_array_base, slot, &edge);
                self.next_edge_id = self.next_edge_id.saturating_add(1);
                v.degree = v.degree.saturating_add(1);
                layout::write_vertex(&mut self.mem, src, &v);
                self.num_edges += 1;
                self.increment_seg_actual(seg_id);
                result.edge_ids[orig_idx] = Some(edge_id);
            } else {
                // Slow path: use segment log.
                let log = SegmentLog::for_segment(self.seg_log_base, seg_id, self.seg_log_idx_base);
                if log.is_full(&self.mem) {
                    self.rebalance_wrapper(src)?;
                    // After rebalance, capacity may have changed. Re-check and retry.
                    if self.num_edges >= self.elem_capacity {
                        self.resize()?;
                    }
                    // Retry: re-read vertex after rebalance.
                    let v2 = layout::read_vertex(&self.mem, src);
                    let slot2 = v2.edge_index + u64::from(v2.degree);
                    if self.have_space_onseg(src, slot2) {
                        let edge_id = self.next_edge_id;
                        let edge = EdgeEntry {
                            target: dst,
                            weight,
                            timestamp,
                            label_and_flags: pack_label_and_flags(label_id, 0),
                            edge_id,
                        };
                        layout::write_edge(&mut self.mem, self.edge_array_base, slot2, &edge);
                        self.next_edge_id = self.next_edge_id.saturating_add(1);
                        let mut v2 = layout::read_vertex(&self.mem, src);
                        v2.degree = v2.degree.saturating_add(1);
                        layout::write_vertex(&mut self.mem, src, &v2);
                        self.num_edges += 1;
                        self.increment_seg_actual(seg_id);
                        result.edge_ids[orig_idx] = Some(edge_id);
                    } else {
                        self.insert_into_log(
                            seg_id,
                            src,
                            dst,
                            label_id,
                            weight,
                            timestamp,
                            self.next_edge_id,
                        )?;
                        self.next_edge_id = self.next_edge_id.saturating_add(1);
                        dirty_segments.insert(seg_id);
                        result.edge_ids[orig_idx] = Some(self.next_edge_id.saturating_sub(1));
                    }
                } else {
                    let edge_id = self.next_edge_id;
                    self.insert_into_log(
                        seg_id,
                        src,
                        dst,
                        label_id,
                        weight,
                        timestamp,
                        edge_id,
                    )?;
                    self.next_edge_id = self.next_edge_id.saturating_add(1);
                    dirty_segments.insert(seg_id);
                    result.edge_ids[orig_idx] = Some(edge_id);
                }
            }
            result.inserted += 1;
            out_batch.push((
                src,
                EdgeEntry {
                    target: dst,
                    weight,
                    timestamp,
                    label_and_flags: pack_label_and_flags(label_id, 0),
                    edge_id: result.edge_ids[orig_idx].unwrap_or_default(),
                },
            ));
            rev_batch.push((
                dst,
                src,
                label_id,
                weight,
                timestamp,
                result.edge_ids[orig_idx].unwrap_or_default(),
            ));
        }

        // Phase 3: Finalize
        // On native (test) builds, rebalance all dirty segments.
        #[cfg(not(target_arch = "wasm32"))]
        self.global_rebalance_dirty_segments(&dirty_segments)?;

        // Update forward/reverse indexes in batch.
        for (src, edge) in out_batch {
            self.push_out_entry(src, edge);
        }
        for (dst, src, label_id, weight, timestamp, edge_id) in rev_batch {
            self.push_rev_entry(dst, RevEntry {
                src,
                weight,
                timestamp,
                label_and_flags: pack_label_and_flags(label_id, 0),
                edge_id,
            });
        }

        self.write_header()?;
        Ok(result)
    }

    // ── High-level bulk create with dedup/labels/properties ──────────────

    /// Creates edges in bulk with full duplicate detection, tombstone revival,
    /// label assignment, and property indexing.
    pub fn bulk_create_edges(
        &mut self,
        edges: &[BulkEdgeInput],
    ) -> Result<BulkInsertResult, GleaphError> {
        if edges.is_empty() {
            return Ok(BulkInsertResult::default());
        }

        // 1. Collect unique src vertices.
        let src_set: HashSet<u32> = edges.iter().map(|e| e.src).collect();

        // Ensure all vertices exist before building the edge set.
        let max_vid = edges.iter().map(|e| e.src.max(e.dst)).max().unwrap_or(0);
        self.ensure_vertices_batch(max_vid)?;

        // 2. Build existing edge set for dedup.
        let (existing_edges, high_degree_vertices) = self.build_existing_edge_set(&src_set)?;

        // 3. Classify each input edge.
        let mut batch_seen: HashSet<(u32, u32)> = HashSet::with_capacity(edges.len());
        let mut insertable: Vec<(usize, u32, u32, u32, f32, u64)> = Vec::with_capacity(edges.len());
        let mut result = BulkInsertResult {
            inserted: 0,
            skipped: 0,
            edge_ids: vec![None; edges.len()],
        };

        for (i, edge) in edges.iter().enumerate() {
            let pair = (edge.src, edge.dst);

            // Batch-internal dedup.
            if !batch_seen.insert(pair) {
                result.skipped += 1;
                continue;
            }

            // Check existing edges.
            let exists = if high_degree_vertices.contains(&edge.src) {
                // Fallback: scan neighbors for high-degree vertex.
                self.collect_neighbors(edge.src)?
                    .iter()
                    .any(|e| e.target == edge.dst)
            } else {
                existing_edges.contains(&pair)
            };

            if exists {
                // Check if tombstoned → revive.
                let existing_label = self.edge_label(edge.src, edge.dst).unwrap_or_default();
                if self.is_edge_tombstoned(edge.src, edge.dst, Some(existing_label.as_str())) {
                    // Revive the tombstoned edge.
                    let label_name = edge.label.clone().unwrap_or_default();
                    let locator = self
                        .find_edge_locator(
                            edge.src,
                            edge.dst,
                            if existing_label.is_empty() {
                                0
                            } else {
                                self.label_index.label_id(&existing_label).unwrap_or(0)
                            },
                        )
                        .ok_or_else(|| {
                            GleaphError::ExecutionError(
                                "failed to locate tombstoned edge during bulk revive".into(),
                            )
                        })?;
                    self.set_edge_tombstoned_at(locator, false)?;
                    let edge_id = self
                        .read_edge_at_locator(locator)
                        .map(|entry| entry.edge_id)
                        .ok_or_else(|| {
                            GleaphError::ExecutionError(
                                "failed to read revived edge during bulk revive".into(),
                            )
                        })?;
                    self.edge_props.remove(&edge_id);
                    if !edge.props.is_empty() {
                        self.index_edge_props(edge_id, edge.src, edge.dst, &edge.props);
                        self.edge_props.insert(
                            edge_id,
                            EdgePropsOverlay {
                                src: edge.src,
                                dst: edge.dst,
                                label: label_name.clone(),
                                props: edge.props.clone(),
                            },
                        );
                    }
                    let _ = self.update_edge_payload_by_endpoints(
                        edge.src,
                        edge.dst,
                        edge.weight,
                        edge.timestamp,
                    );
                    // Update rev_index label_id after revival label change.
                    let old_lid = if existing_label.is_empty() {
                        0
                    } else {
                        self.label_index.label_id(&existing_label).unwrap_or(0)
                    };
                    let new_lid = self.resolve_edge_label_id(Some(label_name.as_str()));
                    self.rev_move_entry_label_by_edge_id(
                        edge.dst,
                        old_lid,
                        new_lid,
                        edge_id,
                        Some(false),
                    );
                    self.out_move_entry_label_by_edge_id(edge.src, old_lid, new_lid, edge_id);
                    if let Some(out) =
                        self.out_find_entry_mut_by_edge_id(edge.src, new_lid, edge_id)
                    {
                        out.weight = edge.weight;
                        out.timestamp = edge.timestamp;
                    }
                    result.inserted += 1;
                    result.edge_ids[i] = Some(edge_id);
                } else {
                    // Live duplicate → skip.
                    result.skipped += 1;
                }
                continue;
            }

            // New edge → queue for bulk_insert_raw.
            let lid = if let Some(ref l) = edge.label {
                if l.is_empty() {
                    0
                } else {
                    self.label_index.ensure_label_id(l)
                }
            } else {
                0
            };
            insertable.push((i, edge.src, edge.dst, lid, edge.weight, edge.timestamp));
        }

        // 4. Bulk-insert all new edges.
        if !insertable.is_empty() {
            let raw_edges: Vec<(u32, u32, u32, f32, u64)> = insertable
                .iter()
                .map(|&(_, s, d, l, w, t)| (s, d, l, w, t))
                .collect();
            let raw_result = self.bulk_insert_raw(&raw_edges)?;

            // Map raw results back to input indices.
            for (j, &(orig_idx, _, _, _, _, _)) in insertable.iter().enumerate() {
                result.edge_ids[orig_idx] = raw_result.edge_ids[j];
            }
            result.inserted += raw_result.inserted;
        }

        // 5. Apply labels and properties for successfully inserted new edges.
        for &(orig_idx, src, dst, _lid, _, _) in &insertable {
            if result.edge_ids[orig_idx].is_none() {
                continue;
            }
            let edge = &edges[orig_idx];
            let label_name = edge.label.clone().unwrap_or_default();
            if !edge.props.is_empty() {
                let edge_id = result.edge_ids[orig_idx].unwrap_or(0);
                if edge_id != 0 {
                    self.index_edge_props(edge_id, src, dst, &edge.props);
                    self.edge_props.insert(
                        edge_id,
                        EdgePropsOverlay {
                            src,
                            dst,
                            label: label_name,
                            props: edge.props.clone(),
                        },
                    );
                }
            }
        }

        // Mark dirty only for edges that were actually inserted or revived (not skipped).
        for (i, edge) in edges.iter().enumerate() {
            if result.edge_ids[i].is_some() {
                for (k, v) in &edge.props {
                    let kid = self.intern_edge_prop_key(k);
                    self.mark_property_dirty_by_id(kid);
                    self.reservoir_observe(
                        kid,
                        result.edge_ids[i].unwrap_or_default(),
                        hash_property_value(v),
                    );
                }
            }
        }
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::abp_tree::{ABP_PAGE_SIZE, ABP_STORE_HEADER_LEN};
    use crate::memory::{Memory, VecMemory};
    use gleaph_types::Value;

    #[test]
    fn compute_capacity_is_sane() {
        let p = compute_capacity(1000, 500);
        assert!(p.segment_size >= 1);
        assert!(p.segment_count >= 1);
        assert!(p.elem_capacity >= 16);
        assert!(p.segment_count.is_power_of_two());
    }

    #[test]
    fn compute_capacity_segment_grid_covers_vertices() {
        for n in 1..=10_000u32 {
            let p = compute_capacity(n, 0);
            let covered = u64::from(p.segment_size) * u64::from(p.segment_count);
            assert!(
                covered >= u64::from(n),
                "n={n} segment_size={} segment_count={}",
                p.segment_size,
                p.segment_count
            );
        }
    }

    #[test]
    fn new_can_honor_initial_edge_capacity() {
        let mem = VecMemory::default();
        let g = PmaGraph::new_with_initial_edge_capacity(mem, 4, 1_000).unwrap();
        assert!(g.elem_capacity >= 1_000);
    }

    #[test]
    fn create_delete_vertex_and_label_scan_filters_tombstones() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 0).unwrap();
        let v1 = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("A".into()))],
            )
            .unwrap();
        let v2 = g.create_vertex(vec!["User".into()], Vec::new()).unwrap();
        assert_eq!(
            g.scan_vertices_by_label("User"),
            VertexIdSet::from_iter([v1, v2])
        );
        g.delete_vertex(v1).unwrap();
        assert_eq!(
            g.scan_vertices_by_label("User"),
            VertexIdSet::from_iter([v2])
        );
        assert!(g.is_vertex_tombstoned(v1));
    }

    #[test]
    fn revive_vertex_restores_label_scan_and_props() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 0).unwrap();
        let v = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("A".into()))],
            )
            .unwrap();
        g.delete_vertex(v).unwrap();
        assert!(g.scan_vertices_by_label("User").is_empty());
        assert_eq!(
            g.get_vertex_props(v),
            Some(vec![("name".into(), Value::Text("A".into()))])
        );

        g.revive_vertex(v).unwrap();
        assert_eq!(
            g.scan_vertices_by_label("User"),
            VertexIdSet::from_iter([v])
        );
        assert_eq!(
            g.get_vertex_props(v),
            Some(vec![("name".into(), Value::Text("A".into()))])
        );
    }

    #[test]
    fn create_delete_edge_and_neighbor_filtering() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
            .unwrap();
        g.create_edge(0, 2, Some("LIKES".into()), Vec::new(), 1.0, 1)
            .unwrap();
        assert!(g.edge_matches_label(0, 1, "KNOWS"));
        g.delete_edge(0, 1, Some("KNOWS")).unwrap();
        let filtered = g.collect_neighbors_filtered(0).unwrap();
        assert!(filtered.iter().all(|e| e.target != 1));
    }

    #[test]
    fn collect_neighbors_filtered_hides_tombstoned_source_vertex() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
            .unwrap();
        g.delete_vertex(0).unwrap();
        let filtered = g.collect_neighbors_filtered(0).unwrap();
        assert!(filtered.is_empty());
    }

    #[test]
    fn recreate_deleted_edge_revives_overlay_without_duplicate_insert() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
            .unwrap();
        g.delete_edge(0, 1, Some("KNOWS")).unwrap();

        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
            .unwrap();

        let filtered = g.collect_neighbors_filtered(0).unwrap();
        assert_eq!(filtered.iter().filter(|e| e.target == 1).count(), 1);
        assert!(!g.is_edge_tombstoned(0, 1, Some("KNOWS")));
    }

    #[test]
    fn revive_edge_by_endpoints_preserves_label_and_props() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_edge(
            0,
            1,
            Some("KNOWS".into()),
            vec![("since".into(), Value::Int64(2020))],
            1.0,
            1,
        )
        .unwrap();
        g.delete_edge(0, 1, Some("KNOWS")).unwrap();

        assert!(g.revive_edge_by_endpoints(0, 1).unwrap());
        assert!(g.edge_matches_label(0, 1, "KNOWS"));
        let edge = g
            .edge_record(0, 1, Some("KNOWS"))
            .expect("edge should exist");
        assert_eq!(edge.props, vec![("since".into(), Value::Int64(2020))]);
    }

    #[test]
    fn recreate_deleted_edge_clears_stale_props_when_new_props_empty() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_edge(
            0,
            1,
            Some("KNOWS".into()),
            vec![("since".into(), Value::Int64(2020))],
            1.0,
            1,
        )
        .unwrap();
        g.delete_edge(0, 1, Some("KNOWS")).unwrap();

        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 2)
            .unwrap();

        let edge = g
            .edge_record(0, 1, Some("KNOWS"))
            .expect("edge should exist");
        assert!(edge.props.is_empty());
    }

    #[test]
    fn recreate_deleted_edge_updates_payload() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
            .unwrap();
        g.delete_edge(0, 1, Some("KNOWS")).unwrap();

        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 7.5, 1234)
            .unwrap();

        let edge = g
            .edge_record(0, 1, Some("KNOWS"))
            .expect("edge should exist");
        assert_eq!(edge.weight, Some(7.5));
        assert_eq!(edge.timestamp, Some(1234));
    }

    #[test]
    fn create_edge_allows_different_labels_on_same_endpoints() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
            .unwrap();
        // Different label on same endpoints is now allowed ((src, dst, label) uniqueness).
        g.create_edge(0, 1, Some("LIKES".into()), Vec::new(), 1.0, 1)
            .unwrap();
        assert!(g.edge_matches_label(0, 1, "KNOWS"));
        assert!(g.edge_matches_label(0, 1, "LIKES"));
    }

    #[test]
    fn create_edge_rejects_duplicate_label_on_same_endpoints() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
            .unwrap();
        let err = g
            .create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 1)
            .unwrap_err();
        assert!(matches!(err, GleaphError::UnsupportedFeature(_)));
    }

    #[test]
    fn parallel_edge_guard_detects_identical_payload_duplicates() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.insert(0, 1, 0, 1.0, 1).unwrap();
        g.insert(0, 1, 0, 1.0, 1).unwrap();

        assert!(g.has_parallel_edges_by_endpoints());
    }

    #[test]
    fn graph_view_dedups_overlay_backed_duplicate_endpoint_artifacts() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 7)
            .unwrap();
        // Simulate a legacy/raw PMA duplicate for the same endpoint pair. Because overlay metadata
        // is endpoint-keyed, the parallel-edge guard treats this as a PMA artefact, not a logical
        // parallel edge.
        g.insert(0, 1, 0, 2.0, 9).unwrap();

        assert!(
            !g.has_parallel_edges_by_endpoints(),
            "overlay-backed duplicate should be treated as PMA artefact"
        );

        let out = gleaph_algo::GraphView::neighbors(&g, 0);
        assert_eq!(
            out.iter().filter(|(dst, _, _)| *dst == 1).count(),
            1,
            "GraphView::neighbors should expose one logical edge per endpoint"
        );

        let rev = gleaph_algo::GraphView::reverse_neighbors(&g, 1);
        assert_eq!(
            rev.iter().filter(|(src, _, _)| *src == 0).count(),
            1,
            "GraphView::reverse_neighbors should expose one logical edge per endpoint"
        );
    }

    #[test]
    fn rebuild_reverse_index_dedups_overlay_backed_duplicate_endpoint_artifacts() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_edge(0, 1, Some("KNOWS".into()), Vec::new(), 1.0, 7)
            .unwrap();
        g.insert(0, 1, 0, 2.0, 9).unwrap();

        g.build_reverse_index();

        let entries: Vec<RevEntry> = g
            .rev_index
            .get(&1)
            .into_iter()
            .flat_map(|buckets| buckets.values())
            .flat_map(|entries| entries.iter().copied())
            .collect();
        assert_eq!(
            entries.iter().filter(|e| e.src == 0).count(),
            1,
            "rebuilt reverse index should store one logical reverse neighbor per endpoint"
        );
    }

    #[test]
    fn set_get_vertex_props_round_trip() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 2).unwrap();
        g.set_vertex_props(0, vec![("age".into(), Value::Int64(42))])
            .unwrap();
        assert_eq!(
            g.get_vertex_props(0),
            Some(vec![("age".into(), Value::Int64(42))])
        );
    }

    #[test]
    fn secondary_vertex_property_equality_index_tracks_updates_and_deletes() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.set_vertex_prop(0, "age".into(), Value::Int64(42))
            .unwrap();
        g.set_vertex_prop(1, "age".into(), Value::Int64(42))
            .unwrap();
        g.set_vertex_prop(2, "age".into(), Value::Int64(7)).unwrap();

        assert!(
            g.scan_vertices_by_property_eq("age", &Value::Int64(42))
                .is_empty()
        );
        g.create_index(EntityType::Vertex, "age".into(), IndexType::Equality)
            .unwrap();

        let hits = g.scan_vertices_by_property_eq("age", &Value::Int64(42));
        assert_eq!(hits, VertexIdSet::from_iter([0, 1]));

        g.set_vertex_prop(1, "age".into(), Value::Int64(99))
            .unwrap();
        let hits = g.scan_vertices_by_property_eq("age", &Value::Int64(42));
        assert_eq!(hits, VertexIdSet::from_iter([0]));

        g.delete_vertex_prop(0, "age").unwrap();
        assert!(
            g.scan_vertices_by_property_eq("age", &Value::Int64(42))
                .is_empty()
        );
    }

    #[test]
    fn secondary_index_maintenance_is_selective_to_registered_properties() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
            .unwrap();
        g.set_vertex_prop(0, "uid".into(), Value::Int64(1)).unwrap();
        g.set_vertex_prop(0, "age".into(), Value::Int64(42))
            .unwrap();
        assert_eq!(
            g.scan_vertices_by_property_eq("uid", &Value::Int64(1)),
            VertexIdSet::from_iter([0])
        );
        assert!(
            g.scan_vertices_by_property_eq("age", &Value::Int64(42))
                .is_empty()
        );
    }

    #[test]
    fn property_index_metadata_survives_overlay_snapshot_restore() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
            .unwrap();
        let snap = g.overlay_snapshot();

        let mem2 = VecMemory::default();
        let mut g2 = PmaGraph::new(mem2, 4).unwrap();
        g2.restore_overlay_snapshot(snap).unwrap();

        assert_eq!(
            g2.list_property_indexes(),
            vec![PropertyIndex {
                entity_type: EntityType::Vertex,
                property_name: "uid".into(),
                index_type: IndexType::Equality
            }]
        );
    }

    #[test]
    fn abp_secondary_eq_index_backfill_and_query_match_in_memory_index() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        g.set_vertex_prop(0, "uid".into(), Value::Int64(10))
            .unwrap();
        g.set_vertex_prop(1, "uid".into(), Value::Int64(10))
            .unwrap();
        g.set_vertex_prop(2, "uid".into(), Value::Int64(11))
            .unwrap();
        g.set_vertex_prop(3, "age".into(), Value::Int64(10))
            .unwrap();
        g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
            .unwrap();

        let mut mem_idx = g.mem.clone();
        let region_start = mem_idx.size_bytes() + 4096;
        let idx = g.build_abp_secondary_index(mem_idx, region_start).unwrap();
        mem_idx = idx.into_memory();

        let expected = g.scan_vertices_by_property_eq("uid", &Value::Int64(10));
        let actual = g
            .scan_vertices_by_property_eq_abp(mem_idx, region_start, "uid", &Value::Int64(10))
            .unwrap();
        assert_eq!(actual, expected);

        let miss = g
            .scan_vertices_by_property_eq_abp(g.mem.clone(), region_start, "age", &Value::Int64(10))
            .unwrap();
        assert!(miss.is_empty());
    }

    #[test]
    fn abp_property_store_builder_backfills_visible_vertex_and_edge_props() {
        use gleaph_types::Value;

        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        let a = g
            .create_vertex(
                vec!["User".into()],
                vec![("name".into(), Value::Text("A".into()))],
            )
            .unwrap();
        let b = g
            .create_vertex(vec!["User".into()], vec![("age".into(), Value::Int64(30))])
            .unwrap();
        g.create_edge(
            a,
            b,
            Some("KNOWS".into()),
            vec![("since".into(), Value::Int64(2020))],
            1.0,
            1,
        )
        .unwrap();

        let pma_end =
            layout::total_memory_needed(g.num_vertices, g.elem_capacity, g.segment_count as u64);
        let region_start = pma_end + 8192;
        let store = g
            .build_abp_property_store(g.mem.clone(), region_start)
            .unwrap();
        let mem2 = store.into_memory();
        let store2 = AbpPropertyStore::from_memory(mem2, region_start).unwrap();
        let edge_id = g.edge_id_for_labeled(a, b, Some("KNOWS"));

        assert_eq!(
            store2.get_vertex_prop(a, "name"),
            Some(Value::Text("A".into()))
        );
        assert_eq!(store2.get_vertex_prop(b, "age"), Some(Value::Int64(30)));
        assert_eq!(
            store2.get_edge_prop_by_id(edge_id, "since"),
            Some(Value::Int64(2020))
        );
    }

    #[test]
    fn abp_property_store_delta_helper_updates_snapshot_after_vertex_prop_changes() {
        use gleaph_types::Value;

        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        let v = g
            .create_vertex(vec!["User".into()], vec![("uid".into(), Value::Int64(1))])
            .unwrap();
        let region_start =
            layout::total_memory_needed(g.num_vertices, g.elem_capacity, g.segment_count as u64)
                + 16384;
        let mut store = g
            .build_abp_property_store(g.mem.clone(), region_start)
            .unwrap();
        assert_eq!(store.get_vertex_prop(v, "uid"), Some(Value::Int64(1)));

        g.set_vertex_prop(v, "uid".into(), Value::Int64(2)).unwrap();
        g.apply_vertex_prop_delta_to_abp_property_store(
            &mut store,
            v,
            "uid",
            Some(&Value::Int64(2)),
        )
        .unwrap();
        assert_eq!(store.get_vertex_prop(v, "uid"), Some(Value::Int64(2)));

        g.delete_vertex_prop(v, "uid").unwrap();
        g.apply_vertex_prop_delta_to_abp_property_store(&mut store, v, "uid", None)
            .unwrap();
        assert_eq!(store.get_vertex_prop(v, "uid"), None);
    }

    #[test]
    fn abp_property_store_vertex_props_bulk_helper_adds_and_removes_props() {
        use gleaph_types::Value;

        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        let v = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let region_start =
            layout::total_memory_needed(g.num_vertices, g.elem_capacity, g.segment_count as u64)
                + 24576;
        let mut store = g
            .build_abp_property_store(g.mem.clone(), region_start)
            .unwrap();
        let props = vec![
            ("name".into(), Value::Text("A".into())),
            ("age".into(), Value::Int64(20)),
        ];

        g.apply_vertex_props_to_abp_property_store(&mut store, v, &props, true)
            .unwrap();
        assert_eq!(
            store.get_vertex_prop(v, "name"),
            Some(Value::Text("A".into()))
        );
        assert_eq!(store.get_vertex_prop(v, "age"), Some(Value::Int64(20)));

        g.apply_vertex_props_to_abp_property_store(&mut store, v, &props, false)
            .unwrap();
        assert_eq!(store.get_vertex_prop(v, "name"), None);
        assert_eq!(store.get_vertex_prop(v, "age"), None);
    }

    #[test]
    fn abp_property_store_edge_prop_delta_helper_updates_and_clears() {
        use gleaph_types::Value;

        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        let a = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        let b = g.create_vertex(vec!["User".into()], vec![]).unwrap();
        g.create_edge(a, b, Some("KNOWS".into()), vec![], 1.0, 1)
            .unwrap();
        let region_start =
            layout::total_memory_needed(g.num_vertices, g.elem_capacity, g.segment_count as u64)
                + 32768;
        let mut store = g
            .build_abp_property_store(g.mem.clone(), region_start)
            .unwrap();

        g.set_edge_prop(a, b, Some("KNOWS"), "since".into(), Value::Int64(2024))
            .unwrap();
        let edge_id = g.edge_id_for_labeled(a, b, Some("KNOWS"));
        g.apply_edge_prop_delta_to_abp_property_store(
            &mut store,
            a,
            b,
            Some("KNOWS"),
            "since",
            Some(&Value::Int64(2024)),
        )
        .unwrap();
        assert_eq!(store.get_edge_prop_by_id(edge_id, "since"), Some(Value::Int64(2024)));

        g.delete_edge_prop(a, b, Some("KNOWS"), "since").unwrap();
        g.apply_edge_prop_delta_to_abp_property_store(
            &mut store,
            a,
            b,
            Some("KNOWS"),
            "since",
            None,
        )
        .unwrap();
        assert_eq!(store.get_edge_prop_by_id(edge_id, "since"), None);
    }

    #[test]
    fn abp_property_store_wrapper_mutations_keep_snapshot_in_sync() {
        use gleaph_types::Value;

        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        let region_start =
            layout::total_memory_needed(g.num_vertices, g.elem_capacity, g.segment_count as u64)
                + 65536;
        let mut store = g
            .build_abp_property_store(g.mem.clone(), region_start)
            .unwrap();

        let a = g
            .create_vertex_with_abp_property_store(
                &mut store,
                vec!["User".into()],
                vec![("uid".into(), Value::Int64(1))],
            )
            .unwrap();
        let b = g
            .create_vertex_with_abp_property_store(&mut store, vec!["User".into()], vec![])
            .unwrap();
        assert_eq!(store.get_vertex_prop(a, "uid"), Some(Value::Int64(1)));

        g.set_vertex_prop_with_abp_property_store(&mut store, a, "uid".into(), Value::Int64(2))
            .unwrap();
        assert_eq!(store.get_vertex_prop(a, "uid"), Some(Value::Int64(2)));
        g.set_vertex_props_with_abp_property_store(
            &mut store,
            a,
            vec![
                ("uid".into(), Value::Int64(3)),
                ("name".into(), Value::Text("A".into())),
            ],
        )
        .unwrap();
        assert_eq!(store.get_vertex_prop(a, "uid"), Some(Value::Int64(3)));
        assert_eq!(
            store.get_vertex_prop(a, "name"),
            Some(Value::Text("A".into()))
        );

        g.create_edge_with_abp_property_store(
            &mut store,
            a,
            b,
            Some("KNOWS".into()),
            vec![("weightClass".into(), Value::Text("light".into()))],
            1.0,
            1,
        )
        .unwrap();
        let edge_id = g.edge_id_for_labeled(a, b, Some("KNOWS"));
        assert_eq!(
            store.get_edge_prop_by_id(edge_id, "weightClass"),
            Some(Value::Text("light".into()))
        );
        g.set_edge_prop_with_abp_property_store(
            &mut store,
            a,
            b,
            Some("KNOWS"),
            "since".into(),
            Value::Int64(2024),
        )
        .unwrap();
        assert_eq!(store.get_edge_prop_by_id(edge_id, "since"), Some(Value::Int64(2024)));

        g.delete_edge_prop_with_abp_property_store(&mut store, a, b, Some("KNOWS"), "since")
            .unwrap();
        assert_eq!(store.get_edge_prop_by_id(edge_id, "since"), None);

        g.delete_vertex_prop_with_abp_property_store(&mut store, a, "uid")
            .unwrap();
        assert_eq!(store.get_vertex_prop(a, "uid"), None);
    }

    #[test]
    fn auto_property_eq_scan_uses_reserved_stable_secondary_index_when_present() {
        use gleaph_types::Value;

        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
            .unwrap();
        let v = g
            .create_vertex(vec!["User".into()], vec![("uid".into(), Value::Int64(42))])
            .unwrap();

        let pma_end =
            layout::total_memory_needed(g.num_vertices, g.elem_capacity, g.segment_count as u64);
        let region_start = pma_end + 4096;
        let idx = g
            .build_abp_secondary_index(g.mem.clone(), region_start)
            .unwrap();
        let mem_with_idx = idx.into_memory();
        g.mem = mem_with_idx;

        let regions = ReservedRegionsMeta {
            secondary_index_offset: region_start,
            secondary_index_len: ABP_STORE_HEADER_LEN + u64::from(ABP_PAGE_SIZE),
            non_pma_base: region_start,
            ..Default::default()
        };
        write_reserved_metas(&mut g.mem, None, Some(regions)).unwrap();

        let hits = g
            .scan_vertices_by_property_eq_auto("uid", &Value::Int64(42))
            .unwrap();
        assert_eq!(hits, VertexIdSet::from_iter([v]));
    }

    #[test]
    fn abp_secondary_eq_index_mutation_delta_helpers_cover_create_set_remove_delete() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
            .unwrap();

        let mut idx = AbpSecondaryEqIndex::new(g.mem.clone(), g.mem.size_bytes() + 4096).unwrap();

        // INSERT (vertex props introduced)
        let created_props = vec![
            ("uid".into(), Value::Int64(100)),
            ("age".into(), Value::Int64(42)),
        ];
        g.set_vertex_props(0, created_props.clone()).unwrap();
        g.apply_vertex_props_to_abp_secondary_eq_index(&mut idx, 0, &created_props, true)
            .unwrap();
        assert_eq!(
            idx.scan_vertices_eq("uid", &Value::Int64(100)).unwrap(),
            vec![0]
        );
        assert!(
            idx.scan_vertices_eq("age", &Value::Int64(42))
                .unwrap()
                .is_empty()
        );

        // SET (single prop value change)
        let old_uid = g
            .get_vertex_props(0)
            .and_then(|p| p.into_iter().find(|(k, _)| k == "uid").map(|(_, v)| v));
        g.set_vertex_prop(0, "uid".into(), Value::Int64(101))
            .unwrap();
        g.apply_vertex_prop_delta_to_abp_secondary_eq_index(
            &mut idx,
            0,
            "uid",
            old_uid.as_ref(),
            Some(&Value::Int64(101)),
        )
        .unwrap();
        assert!(
            idx.scan_vertices_eq("uid", &Value::Int64(100))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            idx.scan_vertices_eq("uid", &Value::Int64(101)).unwrap(),
            vec![0]
        );

        // REMOVE (single prop delete)
        let old_uid = g
            .get_vertex_props(0)
            .and_then(|p| p.into_iter().find(|(k, _)| k == "uid").map(|(_, v)| v));
        g.delete_vertex_prop(0, "uid").unwrap();
        g.apply_vertex_prop_delta_to_abp_secondary_eq_index(
            &mut idx,
            0,
            "uid",
            old_uid.as_ref(),
            None,
        )
        .unwrap();
        assert!(
            idx.scan_vertices_eq("uid", &Value::Int64(101))
                .unwrap()
                .is_empty()
        );

        // DELETE (vertex delete removes all indexed props)
        g.set_vertex_prop(0, "uid".into(), Value::Int64(202))
            .unwrap();
        g.apply_vertex_prop_delta_to_abp_secondary_eq_index(
            &mut idx,
            0,
            "uid",
            None,
            Some(&Value::Int64(202)),
        )
        .unwrap();
        let before_delete_props = g.get_vertex_props(0).unwrap_or_default();
        g.delete_vertex(0).unwrap();
        g.apply_vertex_props_to_abp_secondary_eq_index(&mut idx, 0, &before_delete_props, false)
            .unwrap();
        assert!(
            idx.scan_vertices_eq("uid", &Value::Int64(202))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn abp_secondary_eq_index_wrapper_mutations_keep_index_in_sync() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        g.create_index(EntityType::Vertex, "uid".into(), IndexType::Equality)
            .unwrap();

        let mut idx = AbpSecondaryEqIndex::new(g.mem.clone(), g.mem.size_bytes() + 4096).unwrap();

        let v = g
            .create_vertex_with_abp_eq_index(
                &mut idx,
                vec!["User".into()],
                vec![
                    ("uid".into(), Value::Int64(1)),
                    ("age".into(), Value::Int64(42)),
                ],
            )
            .unwrap();
        assert_eq!(
            idx.scan_vertices_eq("uid", &Value::Int64(1)).unwrap(),
            vec![v]
        );

        g.set_vertex_prop_with_abp_eq_index(&mut idx, v, "uid".into(), Value::Int64(2))
            .unwrap();
        assert!(
            idx.scan_vertices_eq("uid", &Value::Int64(1))
                .unwrap()
                .is_empty()
        );
        assert_eq!(
            idx.scan_vertices_eq("uid", &Value::Int64(2)).unwrap(),
            vec![v]
        );

        g.delete_vertex_prop_with_abp_eq_index(&mut idx, v, "uid")
            .unwrap();
        assert!(
            idx.scan_vertices_eq("uid", &Value::Int64(2))
                .unwrap()
                .is_empty()
        );

        g.set_vertex_props_with_abp_eq_index(
            &mut idx,
            v,
            vec![
                ("uid".into(), Value::Int64(9)),
                ("name".into(), Value::Text("A".into())),
            ],
        )
        .unwrap();
        assert_eq!(
            idx.scan_vertices_eq("uid", &Value::Int64(9)).unwrap(),
            vec![v]
        );

        g.delete_vertex_with_abp_eq_index(&mut idx, v).unwrap();
        assert!(
            idx.scan_vertices_eq("uid", &Value::Int64(9))
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn create_vertex_skips_ids_materialized_by_ensure_vertex() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.ensure_vertex(10).unwrap();

        let created = g.create_vertex(vec!["User".into()], Vec::new()).unwrap();
        assert_eq!(created, 11);
    }

    #[test]
    fn create_vertex_after_existing_edges_can_expand_without_oob() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.insert(0, 1, 0, 1.0, 1).unwrap();
        g.insert(1, 2, 0, 1.0, 2).unwrap();

        let created = g.create_vertex(vec!["User".into()], Vec::new()).unwrap();
        assert_eq!(created, 4);
        assert!(g.vertex_count() >= 5);
        assert!(
            g.collect_neighbors(0)
                .unwrap()
                .iter()
                .any(|e| e.target == 1)
        );
    }

    #[test]
    fn reserve_vertices_noop_zero() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        let v_before = g.vertex_count();
        g.reserve_vertices(0).unwrap();
        assert_eq!(g.vertex_count(), v_before);
    }

    #[test]
    fn reserve_vertices_expands_once() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.insert(0, 1, 0, 1.0, 1).unwrap();

        // Pre-expand for 10 new vertices.
        g.reserve_vertices(10).unwrap();
        let cap_after_reserve = g.vertex_count();

        // Creating 10 vertices should not trigger further expand_vertices.
        for _ in 0..10 {
            g.create_vertex(vec!["User".into()], Vec::new()).unwrap();
        }
        assert_eq!(g.vertex_count(), cap_after_reserve);
    }

    #[test]
    fn expand_vertices_ignores_stale_trailing_bytes_in_new_log_index_region() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.insert(0, 1, 0, 1.0, 1).unwrap();
        g.insert(1, 2, 0, 1.0, 2).unwrap();

        // Simulate non-PMA bytes appended after the PMA layout (e.g. graph overlay snapshot).
        let tail_offset = g.mem.size_bytes();
        g.mem.grow(512).unwrap();
        g.mem.write(tail_offset, &[0xFF; 512]);

        // This triggers `expand_vertices`, which must not read stale garbage as log fill counts.
        g.ensure_vertex(32).unwrap();

        assert!(g.vertex_count() >= 33);
        assert!(
            g.collect_neighbors(0)
                .unwrap()
                .iter()
                .any(|e| e.target == 1)
        );
        assert!(
            g.collect_neighbors(1)
                .unwrap()
                .iter()
                .any(|e| e.target == 2)
        );
    }

    #[test]
    fn rebalance_pivot_splits_left_and_right_movers() {
        let oldp = [0, 10, 20, 30];
        let newp = [0, 8, 24, 36];
        // index 2 is the first right-mover (24 > 20)
        assert_eq!(rebalance_pivot_index(&oldp, &newp), 2);

        let all_left = [0, 9, 19];
        assert_eq!(rebalance_pivot_index(&[0, 10, 20], &all_left), 3);
    }

    #[test]
    fn insert_and_neighbors() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        g.insert(0, 1, 0, 1.0, 11).unwrap();
        g.insert(0, 2, 0, 2.0, 12).unwrap();
        let ns = g.collect_neighbors(0).unwrap();
        assert_eq!(ns.len(), 2);
        assert_eq!(g.edge_count(), 2);
    }

    #[test]
    fn insert_fills_segment_triggers_log_insertion() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();

        // Force vertex 1's first insertion to miss on-segment capacity (`edge_index` for v1 is 1).
        g.elem_capacity = 1;
        // Force wrapper to escalate to root and resize so the log-originated edge can be merged safely.
        for seg in 0..g.segment_count {
            layout::write_seg_total(&mut g.mem, g.seg_tree_base, g.segment_count, seg, 1);
            layout::write_seg_actual(&mut g.mem, g.seg_tree_base, seg, 0);
        }
        let seg = g.get_segment_id(1);
        layout::write_seg_actual(&mut g.mem, g.seg_tree_base, seg, 5);

        g.insert(1, 7, 0, 3.0, 99).unwrap();

        let ns = g.collect_neighbors(1).unwrap();
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].target, 7);
        assert_eq!(ns[0].weight, 3.0);
        assert_eq!(ns[0].timestamp, 99);
    }

    #[test]
    fn collect_neighbors_returns_on_seg_and_log_edges() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        g.insert(0, 1, 0, 1.5, 10).unwrap();
        let seg = g.get_segment_id(0);
        g.insert_into_log(seg, 0, 9, 0, 9.0, 90, 1).unwrap();

        let ns = g.collect_neighbors(0).unwrap();
        assert_eq!(ns.len(), 2);
        assert_eq!(ns[0].target, 1);
        assert!(ns.iter().any(|e| e.target == 9));
    }

    #[test]
    fn rebalance_wrapper_clears_log_when_density_within_window() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        let seg = g.get_segment_id(0);
        g.insert_into_log(seg, 0, 3, 0, 3.0, 30, 2).unwrap();
        let before = layout::read_vertex(&g.mem, 0);
        assert!(before.log_offset >= 0);

        g.rebalance_wrapper(0).unwrap();

        let after = layout::read_vertex(&g.mem, 0);
        assert_eq!(after.log_offset, -1);
    }

    #[test]
    fn log_full_triggers_forced_rebalance_before_insert() {
        use crate::segment_log::MAX_LOG_ENTRIES;

        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        g.elem_capacity = 1; // ensure insert initially cannot use on-segment slot for vertex 1

        let seg = g.get_segment_id(1);
        let log = SegmentLog::for_segment(g.seg_log_base, seg, g.seg_log_idx_base);
        for i in 0..MAX_LOG_ENTRIES {
            let wrote = log.append(
                &mut g.mem,
                LogEntry {
                    src: 99,
                    dst: i,
                    label_and_flags: 0,
                    edge_id: i + 1,
                    weight: 0.0,
                    timestamp: 0,
                    prev_offset: -1,
                },
            );
            assert_eq!(wrote, Some(i));
        }
        assert!(log.is_full(&g.mem));

        g.insert(1, 42, 0, 1.0, 123).unwrap();

        // Forced rebalance drains the full log before retrying insert, so the segment log should no longer be full.
        let log_after = SegmentLog::for_segment(g.seg_log_base, seg, g.seg_log_idx_base);
        assert!(!log_after.is_full(&g.mem));
        let ns = g.collect_neighbors(1).unwrap();
        assert!(ns.iter().any(|e| e.target == 42));
    }

    #[test]
    fn rebalance_weighted_merges_log_edges_into_edge_array() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();

        g.insert(0, 1, 0, 2.0, 10).unwrap();
        let seg = g.get_segment_id(0);
        g.insert_into_log(seg, 0, 9, 0, 9.0, 90, 1).unwrap();

        g.rebalance_weighted(0, 2).unwrap();

        let v0 = layout::read_vertex(&g.mem, 0);
        assert_eq!(v0.log_offset, -1);
        let ns = g.collect_neighbors(0).unwrap();
        assert_eq!(ns.len(), 2);
        assert!(ns.iter().any(|e| e.target == 1));
        assert!(ns.iter().any(|e| e.target == 9));
    }

    #[test]
    fn rebalance_weighted_redistributes_vertex_positions() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        g.elem_capacity = 64;

        for _ in 0..4 {
            g.insert(0, 1, 0, 1.0, 1).unwrap();
        }
        for _ in 0..2 {
            g.insert(1, 2, 0, 1.0, 1).unwrap();
        }

        let before0 = layout::read_vertex(&g.mem, 0).edge_index;
        let before1 = layout::read_vertex(&g.mem, 1).edge_index;
        let before2 = layout::read_vertex(&g.mem, 2).edge_index;

        g.rebalance_weighted(0, 8).unwrap();

        let after0 = layout::read_vertex(&g.mem, 0).edge_index;
        let after1 = layout::read_vertex(&g.mem, 1).edge_index;
        let after2 = layout::read_vertex(&g.mem, 2).edge_index;
        assert!(after0 <= after1 && after1 <= after2);
        assert!(after1 > before1 || after2 > before2 || after0 != before0);
    }

    #[test]
    fn resize_doubles_capacity() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 2).unwrap();
        let old = g.elem_capacity;
        g.resize().unwrap();
        assert_eq!(g.elem_capacity, old * 2);
    }

    #[test]
    fn resize_preserves_edges_and_drains_logs() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();
        g.insert(0, 1, 0, 1.0, 10).unwrap();
        let seg = g.get_segment_id(0);
        g.insert_into_log(seg, 0, 9, 0, 9.0, 90, 1).unwrap();
        let before = g.collect_neighbors(0).unwrap();
        assert_eq!(before.len(), 2);

        let old_cap = g.elem_capacity;
        g.resize().unwrap();

        assert_eq!(g.elem_capacity, old_cap * 2);
        let after = g.collect_neighbors(0).unwrap();
        assert_eq!(after.len(), 2);
        assert!(after.iter().any(|e| e.target == 1));
        assert!(after.iter().any(|e| e.target == 9));
        let v0 = layout::read_vertex(&g.mem, 0);
        assert_eq!(v0.log_offset, -1);
    }

    #[test]
    fn resize_relocates_reserved_non_pma_regions() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 8).unwrap();

        let old_pma_end =
            layout::total_memory_needed(g.num_vertices, g.elem_capacity, g.segment_count as u64);
        let new_pma_end = layout::total_memory_needed(
            g.num_vertices,
            g.elem_capacity * 2,
            g.segment_count as u64,
        );
        let overlay_len = 16u32;
        let property_len = 24u64;
        let index_len = 20u64;
        let overlay_offset = old_pma_end + 64;
        let property_offset = overlay_offset + u64::from(overlay_len) + 32;
        let index_offset = property_offset + property_len + 32;
        let non_pma_base = overlay_offset;

        ensure_mem_size(&mut g.mem, index_offset + index_len + 128).unwrap();
        g.mem.write(overlay_offset, &[0xA1; 16]);
        g.mem.write(property_offset, &[0xB2; 24]);
        g.mem.write(index_offset, &[0xC3; 20]);

        let persist = ReservedPersistMeta::new(
            g.num_vertices as u32,
            overlay_offset,
            overlay_len,
            overlay_len,
        );
        let regions = ReservedRegionsMeta {
            property_store_offset: property_offset,
            property_store_len: property_len,
            secondary_index_offset: index_offset,
            secondary_index_len: index_len,
            non_pma_base,
            ..Default::default()
        };
        write_reserved_metas(&mut g.mem, Some(persist), Some(regions)).unwrap();

        g.resize().unwrap();

        let shift = new_pma_end - non_pma_base;
        let (persist2, regions2) = read_reserved_metas(&g.mem);
        let persist2 = persist2.expect("persist meta");
        let regions2 = regions2.expect("regions meta");
        assert_eq!(persist2.overlay_offset, overlay_offset + shift);
        assert_eq!(regions2.property_store_offset, property_offset + shift);
        assert_eq!(regions2.secondary_index_offset, index_offset + shift);
        assert_eq!(regions2.non_pma_base, new_pma_end);

        let mut buf = [0u8; 24];
        g.mem.read(regions2.property_store_offset, &mut buf);
        assert_eq!(buf, [0xB2; 24]);
        let mut overlay_buf = [0u8; 16];
        g.mem.read(persist2.overlay_offset, &mut overlay_buf);
        assert_eq!(overlay_buf, [0xA1; 16]);
        let mut idx_buf = [0u8; 20];
        g.mem.read(regions2.secondary_index_offset, &mut idx_buf);
        assert_eq!(idx_buf, [0xC3; 20]);
    }

    #[test]
    fn expand_vertices_relocates_reserved_non_pma_regions() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();

        let old_pma_end =
            layout::total_memory_needed(g.num_vertices, g.elem_capacity, g.segment_count as u64);
        let target_vertices = 128u64;
        let params = compute_capacity(target_vertices as u32, g.num_edges);
        let new_elem_capacity = g.elem_capacity.max(params.elem_capacity);
        let new_pma_end = layout::total_memory_needed(
            target_vertices,
            new_elem_capacity,
            params.segment_count as u64,
        );
        let property_offset = old_pma_end + 32;
        let property_len = 12u64;

        ensure_mem_size(&mut g.mem, property_offset + property_len + 32).unwrap();
        g.mem.write(property_offset, &[0xD4; 12]);

        let persist = ReservedPersistMeta::new(g.num_vertices as u32, 0, 0, 0);
        let regions = ReservedRegionsMeta {
            property_store_offset: property_offset,
            property_store_len: property_len,
            non_pma_base: 0, // exercise inference path
            ..Default::default()
        };
        write_reserved_metas(&mut g.mem, Some(persist), Some(regions)).unwrap();

        g.ensure_vertex((target_vertices - 1) as u32).unwrap();

        let (persist2, regions2) = read_reserved_metas(&g.mem);
        let _persist2 = persist2.expect("persist meta");
        let regions2 = regions2.expect("regions meta");
        assert_eq!(g.vertex_count(), target_vertices);
        assert_eq!(regions2.non_pma_base, new_pma_end);

        let moved_offset = regions2.property_store_offset;
        let mut buf = [0u8; 12];
        g.mem.read(moved_offset, &mut buf);
        assert_eq!(buf, [0xD4; 12]);
    }

    #[test]
    fn header_roundtrip_reconstruct() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.insert(0, 1, 0, 1.0, 1).unwrap();
        g.write_header().unwrap();
        let g2 = PmaGraph::from_stable_memory(g.mem).unwrap();
        assert_eq!(g2.vertex_count(), 4);
        assert_eq!(g2.edge_count(), 1);
    }

    #[test]
    fn from_stable_memory_rejects_short_memory() {
        let mem = VecMemory::default();
        let err = PmaGraph::from_stable_memory(mem)
            .err()
            .expect("short memory must fail");
        assert!(matches!(err, GleaphError::InvalidHeader));
    }

    #[test]
    fn insert_spills_to_log_when_vertex_local_gap_is_full() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();

        let mut v0 = layout::read_vertex(&g.mem, 0);
        let mut v1 = layout::read_vertex(&g.mem, 1);
        v0.edge_index = 0;
        v0.degree = 1;
        v0.log_offset = -1;
        v1.edge_index = 1;
        v1.degree = 1;
        v1.log_offset = -1;
        layout::write_vertex(&mut g.mem, 0, &v0);
        layout::write_vertex(&mut g.mem, 1, &v1);

        layout::write_edge(
            &mut g.mem,
            g.edge_array_base,
            0,
            &EdgeEntry {
                target: 10,
                weight: 1.0,
                timestamp: 10,
                edge_id: 0,
                label_and_flags: 0,
            },
        );
        layout::write_edge(
            &mut g.mem,
            g.edge_array_base,
            1,
            &EdgeEntry {
                target: 20,
                weight: 2.0,
                timestamp: 20,
                edge_id: 0,
                label_and_flags: 0,
            },
        );
        g.num_edges = 2;
        g.recount_seg_total(0, g.segment_count);

        g.insert(0, 11, 0, 3.0, 30).unwrap();

        let n0 = g.collect_neighbors(0).unwrap();
        let n1 = g.collect_neighbors(1).unwrap();
        assert!(n0.iter().any(|e| e.target == 10));
        assert!(
            n0.iter()
                .any(|e| e.target == 11 && e.weight == 3.0 && e.timestamp == 30)
        );
        assert!(
            n1.iter()
                .any(|e| e.target == 20 && e.weight == 2.0 && e.timestamp == 20)
        );
    }

    #[test]
    fn stress_insert_10k_edges_no_panic() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 256).unwrap();
        for i in 0..10_000u32 {
            let src = i % 256;
            let dst = (i * 7) % 256;
            g.insert(src, dst, 0, 1.0, i as u64).unwrap();
        }
        assert_eq!(g.edge_count(), 10_000);
    }

    #[test]
    fn ensure_vertex_expands_and_preserves_existing_edges() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.insert(0, 1, 0, 1.0, 10).unwrap();
        g.insert(1, 2, 0, 2.0, 20).unwrap();

        g.ensure_vertex(10).unwrap();

        assert_eq!(g.vertex_count(), 11);
        let n0 = g.collect_neighbors(0).unwrap();
        let n1 = g.collect_neighbors(1).unwrap();
        assert_eq!(n0.len(), 1);
        assert_eq!(n0[0].target, 1);
        assert_eq!(n1.len(), 1);
        assert_eq!(n1[0].target, 2);
        assert!(g.collect_neighbors(10).unwrap().is_empty());
    }

    #[test]
    fn new_clears_stale_segment_and_log_metadata() {
        let params = compute_capacity(8, 0);
        let required =
            layout::total_memory_needed(8, params.elem_capacity, params.segment_count as u64);
        let mut mem = VecMemory::with_size(required as usize);
        let junk = vec![0xAB; required as usize];
        mem.write(0, &junk);

        let g = PmaGraph::new(mem, 8).unwrap();

        for seg in 0..g.segment_count {
            assert_eq!(layout::read_seg_actual(&g.mem, g.seg_tree_base, seg), 0);
            let log = SegmentLog::for_segment(g.seg_log_base, seg, g.seg_log_idx_base);
            assert_eq!(log.fill_count(&g.mem), 0);
            assert_eq!(log.read_entry(&g.mem, 0), None);
        }
    }

    #[test]
    fn ensure_vertex_rejects_growth_past_u32_max() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        let before = g.vertex_count();

        let err = g.ensure_vertex(u32::MAX).unwrap_err();
        assert!(matches!(err, GleaphError::Unsupported(_)));
        assert_eq!(g.vertex_count(), before);
    }

    #[test]
    fn degree_weighted_gap_distribution_fairness() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();

        // Force a wider window so extra gaps exist to distribute.
        g.elem_capacity = 64;

        let mut v0 = layout::read_vertex(&g.mem, 0);
        v0.degree = 1;
        layout::write_vertex(&mut g.mem, 0, &v0);
        let mut v1 = layout::read_vertex(&g.mem, 1);
        v1.degree = 7;
        layout::write_vertex(&mut g.mem, 1, &v1);
        let mut v2 = layout::read_vertex(&g.mem, 2);
        v2.degree = 1;
        layout::write_vertex(&mut g.mem, 2, &v2);
        let mut v3 = layout::read_vertex(&g.mem, 3);
        v3.degree = 1;
        layout::write_vertex(&mut g.mem, 3, &v3);

        g.rebuild_vertex_offsets().unwrap();
        // Re-spread across the full expanded capacity.
        let pos = g.calculate_positions(0, 4);
        assert_eq!(pos.len(), 4);

        let span0 = pos[1] - pos[0];
        let span1 = pos[2] - pos[1];
        let span2 = pos[3] - pos[2];

        // Middle vertex has much higher degree and should receive at least as much spacing.
        assert!(span1 >= span0);
        assert!(span1 >= span2);
    }

    // ── EdgeEntry 24-byte layout + edge_id ──────────────────────────────

    #[test]
    fn edge_entry_size_is_24_bytes() {
        assert_eq!(core::mem::size_of::<EdgeEntry>(), 24);
        assert_eq!(layout::EDGE_ENTRY_SIZE, 24);
    }

    #[test]
    fn insert_assigns_monotonically_increasing_edge_ids() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.insert(0, 1, 0, 1.0, 10).unwrap();
        g.insert(0, 2, 0, 2.0, 20).unwrap();
        g.insert(0, 3, 0, 3.0, 30).unwrap();

        let edges = g.collect_neighbors(0).unwrap();
        // All PMA-path edges should have distinct, non-zero edge_ids.
        let ids: Vec<u32> = edges.iter().map(|e| e.edge_id).collect();
        assert!(
            ids.iter().all(|&id| id >= 1),
            "edge_ids must be non-zero: {ids:?}"
        );
        let mut sorted = ids.clone();
        sorted.dedup();
        assert_eq!(sorted.len(), ids.len(), "edge_ids must be unique: {ids:?}");
    }

    #[test]
    fn next_edge_id_persists_across_round_trip() {
        let mem = VecMemory::default();
        let mut g = PmaGraph::new(mem, 4).unwrap();
        g.insert(0, 1, 0, 1.0, 10).unwrap();
        g.insert(1, 2, 0, 2.0, 20).unwrap();
        let id_before = g.next_edge_id;
        g.write_header().unwrap();

        let g2 = PmaGraph::from_stable_memory(g.mem).unwrap();
        assert_eq!(
            g2.next_edge_id, id_before,
            "next_edge_id must survive round-trip"
        );
    }

    // ── Bulk insert tests ────────────────────────────────────────────────────────

    #[test]
    fn bulk_insert_raw_empty() {
        let mut g = PmaGraph::new(VecMemory::default(), 4).unwrap();
        let result = g.bulk_insert_raw(&[]).unwrap();
        assert_eq!(result.inserted, 0);
        assert_eq!(result.skipped, 0);
        assert!(result.edge_ids.is_empty());
    }

    #[test]
    fn bulk_insert_raw_single() {
        let mut g = PmaGraph::new(VecMemory::default(), 4).unwrap();
        let result = g.bulk_insert_raw(&[(0, 1, 0, 1.0, 100)]).unwrap();
        assert_eq!(result.inserted, 1);
        assert_eq!(result.edge_ids.len(), 1);
        assert!(result.edge_ids[0].is_some());

        let ns = g.collect_neighbors(0).unwrap();
        assert_eq!(ns.len(), 1);
        assert_eq!(ns[0].target, 1);
        assert_eq!(ns[0].weight, 1.0);
        assert_eq!(ns[0].timestamp, 100);
    }

    #[test]
    fn bulk_insert_raw_same_src() {
        let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();
        let edges: Vec<(u32, u32, u32, f32, u64)> =
            (0..5).map(|i| (0, i + 1, 0, 1.0, i as u64)).collect();
        let result = g.bulk_insert_raw(&edges).unwrap();
        assert_eq!(result.inserted, 5);
        let ns = g.collect_neighbors(0).unwrap();
        assert_eq!(ns.len(), 5);
        let mut targets: Vec<u32> = ns.iter().map(|e| e.target).collect();
        targets.sort();
        assert_eq!(targets, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn bulk_insert_raw_diverse() {
        let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
        let edges: Vec<(u32, u32, u32, f32, u64)> = vec![
            (0, 1, 0, 1.0, 0),
            (2, 3, 0, 2.0, 1),
            (4, 5, 0, 3.0, 2),
            (6, 7, 0, 4.0, 3),
            (8, 9, 0, 5.0, 4),
        ];
        let result = g.bulk_insert_raw(&edges).unwrap();
        assert_eq!(result.inserted, 5);
        for &(src, dst, _, weight, ts) in &edges {
            let ns = g.collect_neighbors(src).unwrap();
            assert!(
                ns.iter()
                    .any(|e| e.target == dst && e.weight == weight && e.timestamp == ts),
                "edge {src}->{dst} not found"
            );
        }
    }

    #[test]
    fn bulk_insert_raw_triggers_resize() {
        // Start with minimal capacity and insert enough to force resize.
        let mut g = PmaGraph::new(VecMemory::default(), 4).unwrap();
        let initial_cap = g.elem_capacity;
        let edges: Vec<(u32, u32, u32, f32, u64)> = (0..initial_cap + 1)
            .map(|i| (0, (i as u32) + 1, 0, 1.0, i))
            .collect();
        let result = g.bulk_insert_raw(&edges).unwrap();
        assert_eq!(result.inserted, edges.len() as u64);
        assert!(g.elem_capacity > initial_cap, "capacity should have grown");
    }

    #[test]
    fn bulk_insert_raw_expands_vertices() {
        let mut g = PmaGraph::new(VecMemory::default(), 4).unwrap();
        let old_v = g.num_vertices;
        // Insert an edge with dst beyond current vertex count.
        let result = g.bulk_insert_raw(&[(0, 100, 0, 1.0, 0)]).unwrap();
        assert_eq!(result.inserted, 1);
        assert!(g.num_vertices > old_v);
        let ns = g.collect_neighbors(0).unwrap();
        assert!(ns.iter().any(|e| e.target == 100));
    }

    #[test]
    fn bulk_insert_raw_preserves_neighbors() {
        let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();
        // Pre-insert some edges individually.
        g.insert(0, 1, 0, 1.0, 0).unwrap();
        g.insert(0, 2, 0, 2.0, 1).unwrap();

        // Bulk-insert more.
        let result = g
            .bulk_insert_raw(&[(0, 3, 0, 3.0, 2), (0, 4, 0, 4.0, 3)])
            .unwrap();
        assert_eq!(result.inserted, 2);

        let ns = g.collect_neighbors(0).unwrap();
        let mut targets: Vec<u32> = ns.iter().map(|e| e.target).collect();
        targets.sort();
        assert_eq!(targets, vec![1, 2, 3, 4]);
    }

    #[test]
    fn bulk_insert_raw_rev_index() {
        let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();
        g.bulk_insert_raw(&[(0, 5, 0, 1.0, 10), (1, 5, 0, 2.0, 20), (2, 5, 0, 3.0, 30)])
            .unwrap();

        let rev = g.reverse_neighbors(5);
        let mut sources: Vec<u32> = rev.iter().map(|(src, _, _)| *src).collect();
        sources.sort();
        assert_eq!(sources, vec![0, 1, 2]);
    }

    #[test]
    fn bulk_create_edges_dedup_existing() {
        let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();
        g.create_edge(0, 1, None, vec![], 1.0, 0).unwrap();

        let inputs = vec![BulkEdgeInput {
            src: 0,
            dst: 1,
            label: None,
            props: vec![],
            weight: 2.0,
            timestamp: 100,
        }];
        let result = g.bulk_create_edges(&inputs).unwrap();
        assert_eq!(result.skipped, 1);
        assert_eq!(result.inserted, 0);
    }

    #[test]
    fn bulk_create_edges_dedup_batch() {
        let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();
        let inputs = vec![
            BulkEdgeInput {
                src: 0,
                dst: 1,
                label: None,
                props: vec![],
                weight: 1.0,
                timestamp: 0,
            },
            BulkEdgeInput {
                src: 0,
                dst: 1,
                label: None,
                props: vec![],
                weight: 2.0,
                timestamp: 1,
            },
        ];
        let result = g.bulk_create_edges(&inputs).unwrap();
        assert_eq!(result.inserted, 1);
        assert_eq!(result.skipped, 1);
        // Only one edge should exist.
        let ns = g.collect_neighbors(0).unwrap();
        assert_eq!(ns.len(), 1);
    }

    #[test]
    fn bulk_create_edges_with_labels() {
        let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();
        let inputs = vec![
            BulkEdgeInput {
                src: 0,
                dst: 1,
                label: Some("KNOWS".into()),
                props: vec![("since".into(), Value::Int64(2020))],
                weight: 1.0,
                timestamp: 0,
            },
            BulkEdgeInput {
                src: 1,
                dst: 2,
                label: Some("LIKES".into()),
                props: vec![],
                weight: 2.0,
                timestamp: 1,
            },
        ];
        let result = g.bulk_create_edges(&inputs).unwrap();
        assert_eq!(result.inserted, 2);

        assert_eq!(g.edge_label(0, 1), Some("KNOWS".into()));
        assert_eq!(g.edge_label(1, 2), Some("LIKES".into()));
        let rec = g
            .edge_record(0, 1, Some("KNOWS"))
            .expect("edge record should exist");
        assert!(
            rec.props
                .iter()
                .any(|(k, v)| k == "since" && *v == Value::Int64(2020))
        );
    }

    #[test]
    fn bulk_create_edges_revives_tombstoned() {
        let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();
        g.create_edge(0, 1, Some("KNOWS".into()), vec![], 1.0, 0)
            .unwrap();
        g.delete_edge(0, 1, Some("KNOWS")).unwrap();
        assert!(g.is_edge_tombstoned(0, 1, Some("KNOWS")));

        // Re-creating with same label revives the tombstoned edge.
        let inputs = vec![BulkEdgeInput {
            src: 0,
            dst: 1,
            label: Some("KNOWS".into()),
            props: vec![],
            weight: 5.0,
            timestamp: 999,
        }];
        let result = g.bulk_create_edges(&inputs).unwrap();
        assert_eq!(result.inserted, 1);
        assert!(!g.is_edge_tombstoned(0, 1, Some("KNOWS")));
        assert_eq!(g.edge_label(0, 1), Some("KNOWS".into()));
    }

    #[test]
    fn selectivity_snapshot_roundtrip_preserves_dirty_counts() {
        let mut g = PmaGraph::new(VecMemory::default(), 128).unwrap();
        // Create enough vertices so that 1 mutation is below the 10% threshold.
        for i in 0..20u32 {
            g.create_vertex(
                vec!["Person".into()],
                vec![("name".into(), Value::Text(format!("person_{i}")))],
            )
            .unwrap();
        }
        g.compute_property_selectivity();
        assert!(!g.get_property_selectivity().is_empty());
        let baseline = g.selectivity_baseline_for_test("vertex:name");
        assert!(baseline >= 10, "baseline should be >= 10, got {baseline}");

        // Mutate 1 property (1/20 = 5% < 10% threshold).
        g.set_vertex_prop(0, "name".into(), Value::Text("Carol".into()))
            .unwrap();
        assert_eq!(g.selectivity_dirty_count_for_test("vertex:name"), 1);

        // Take snapshot and restore — dirty counts + baselines should round-trip.
        let snap = g.overlay_snapshot();
        let mut g2 = PmaGraph::new(VecMemory::default(), 128).unwrap();
        g2.restore_overlay_snapshot(snap).unwrap();
        assert_eq!(g2.selectivity_dirty_count_for_test("vertex:name"), 1);
        assert!(g2.selectivity_baseline_for_test("vertex:name") >= 10);
        assert!(!g2.get_property_selectivity().is_empty());

        // refresh_selectivity_if_stale should be a no-op (dirty ratio < threshold).
        assert!(!g2.refresh_selectivity_if_stale_with_flag());
    }

    #[test]
    fn edge_prop_mutations_track_per_property_dirty_counts() {
        let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();
        g.create_edge(0, 1, Some("KNOWS".into()), vec![], 1.0, 0)
            .unwrap();
        // Reset so we can track precisely.
        g.compute_property_selectivity();

        // set_edge_prop should track the specific property.
        g.set_edge_prop(0, 1, Some("KNOWS"), "weight".into(), Value::Int64(5))
            .unwrap();
        assert_eq!(g.selectivity_dirty_count_for_test("edge:weight"), 1);

        // delete_edge_prop should also track.
        g.delete_edge_prop(0, 1, Some("KNOWS"), "weight").unwrap();
        assert_eq!(g.selectivity_dirty_count_for_test("edge:weight"), 2);
    }

    #[test]
    fn non_indexed_edge_property_selectivity_computed() {
        let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
        // Create several edges with a non-indexed property "rating".
        for i in 0..10u32 {
            g.create_edge(
                i,
                i + 1,
                Some("RATES".into()),
                vec![("rating".into(), Value::Int64(i as i64 % 3))],
                1.0,
                0,
            )
            .unwrap();
        }
        g.compute_property_selectivity();
        let sel = g.get_property_selectivity();
        // "edge:rating" should be present (bug fix: was missing before).
        assert!(sel.contains_key("edge:rating"), "selectivity map: {sel:?}");
        // There are 3 distinct ratings (0, 1, 2) across 10 edges → sel ≈ 0.3
        let val = sel["edge:rating"];
        assert!(val > 0.0 && val <= 1.0, "unexpected selectivity: {val}");
    }

    #[test]
    fn dirty_ratio_threshold_triggers_selective_refresh() {
        let mut g = PmaGraph::new(VecMemory::default(), 32).unwrap();
        // Create 10 vertices with "age" property.
        for i in 0..10u32 {
            g.create_vertex(
                vec!["Person".into()],
                vec![("age".into(), Value::Int64(20 + i as i64))],
            )
            .unwrap();
        }
        g.compute_property_selectivity();
        let original_sel = g.get_property_selectivity().clone();
        assert!(original_sel.contains_key("vertex:age"));

        // Mutate > 10% of the baseline → should trigger selective refresh.
        // Baseline for "vertex:age" should be ~10. Mutating 2 vertices = 20% > 10%.
        g.set_vertex_prop(0, "age".into(), Value::Int64(99))
            .unwrap();
        g.set_vertex_prop(1, "age".into(), Value::Int64(99))
            .unwrap();
        assert!(g.selectivity_dirty_count_for_test("vertex:age") >= 2);

        let refreshed = g.refresh_selectivity_if_stale_with_flag();
        assert!(refreshed, "expected selective refresh to fire");
        // Dirty count should be cleared for refreshed property.
        assert_eq!(g.selectivity_dirty_count_for_test("vertex:age"), 0);
    }

    #[test]
    fn sub_threshold_dirty_counts_accumulate() {
        let mut g = PmaGraph::new(VecMemory::default(), 256).unwrap();
        // Create 100 vertices so baseline is large.
        for i in 0..100u32 {
            g.create_vertex(
                vec!["Item".into()],
                vec![("score".into(), Value::Int64(i as i64))],
            )
            .unwrap();
        }
        g.compute_property_selectivity();
        assert!(g.selectivity_baseline_for_test("vertex:score") > 0);

        // Mutate 1 vertex (1% < 10% threshold).
        g.set_vertex_prop(0, "score".into(), Value::Int64(999))
            .unwrap();
        assert_eq!(g.selectivity_dirty_count_for_test("vertex:score"), 1);

        // Refresh should not fire.
        assert!(!g.refresh_selectivity_if_stale_with_flag());
        // Dirty count should persist (not cleared).
        assert_eq!(g.selectivity_dirty_count_for_test("vertex:score"), 1);
    }

    #[test]
    fn prng_deterministic_with_fixed_seed() {
        let mut rng1 = Prng::default();
        let mut rng2 = Prng::default();
        // Same seed → same sequence.
        for _ in 0..100 {
            assert_eq!(rng1.next_u64(), rng2.next_u64());
        }
        // Bounded output is in range.
        let mut rng = Prng::default();
        for _ in 0..1000 {
            let v = rng.next_bounded(42);
            assert!(v < 42);
        }
    }

    #[test]
    fn rng_state_persists_across_snapshot() {
        let mut g = PmaGraph::new(VecMemory::default(), 8).unwrap();
        g.create_vertex(vec![], vec![("x".into(), Value::Int64(1))])
            .unwrap();
        g.compute_property_selectivity();
        let state_before = g.rng_state_for_test();

        let snap = g.overlay_snapshot();
        let mut g2 = PmaGraph::new(VecMemory::default(), 8).unwrap();
        g2.restore_overlay_snapshot(snap).unwrap();
        assert_eq!(g2.rng_state_for_test(), state_before);
    }

    // ── Phase 3 tests: interning, hashing, reservoir ──

    #[test]
    fn prop_key_intern_basics() {
        let mut intern = PropKeyIntern::default();
        let a = intern.intern("vertex:name");
        let b = intern.intern("vertex:age");
        let a2 = intern.intern("vertex:name");
        assert_eq!(a, a2, "same key should return same id");
        assert_ne!(a, b, "different keys should return different ids");
        assert_eq!(intern.resolve(a), "vertex:name");
        assert_eq!(intern.resolve(b), "vertex:age");
        assert_eq!(intern.len(), 2);

        // from_pairs round-trip.
        let pairs: Vec<(PropKeyId, String)> =
            intern.iter().map(|(id, s)| (id, s.to_string())).collect();
        let restored = PropKeyIntern::from_pairs(pairs);
        assert_eq!(restored.len(), 2);
        assert_eq!(restored.resolve(a), "vertex:name");
        assert_eq!(restored.resolve(b), "vertex:age");
    }

    #[allow(clippy::approx_constant)]
    #[test]
    fn hash_property_value_deterministic() {
        let v1 = Value::Int64(42);
        let v2 = Value::Int64(42);
        let v3 = Value::Int64(43);
        let v4 = Value::Text("hello".into());
        assert_eq!(
            hash_property_value(&v1),
            hash_property_value(&v2),
            "same value → same hash"
        );
        assert_ne!(
            hash_property_value(&v1),
            hash_property_value(&v3),
            "different int → different hash"
        );
        assert_ne!(
            hash_property_value(&v1),
            hash_property_value(&v4),
            "different type → different hash"
        );

        // Float uses to_bits, so NaN equality doesn't matter, but same bits → same hash.
        let f1 = Value::Float64(3.14);
        let f2 = Value::Float64(3.14);
        assert_eq!(hash_property_value(&f1), hash_property_value(&f2));

        // Null should produce a consistent hash.
        assert_eq!(
            hash_property_value(&Value::Null),
            hash_property_value(&Value::Null)
        );
    }

    #[test]
    fn reservoir_fills_and_replaces() {
        let mut g = PmaGraph::new(VecMemory::default(), 2048).unwrap();
        // 2000 vertex prop mutations → reservoir should fill to RESERVOIR_SIZE.
        for i in 0..2000u32 {
            g.create_vertex(
                vec!["Node".into()],
                vec![("val".into(), Value::Int64(i as i64))],
            )
            .unwrap();
        }
        assert_eq!(g.reservoir_len_for_test(), RESERVOIR_SIZE);
        assert_eq!(g.reservoir_total_seen_for_test(), 2000);
    }

    #[test]
    fn reservoir_selectivity_estimate() {
        let mut g = PmaGraph::new(VecMemory::default(), 512).unwrap();
        // 500 vertices with 5 distinct "color" values.
        let colors = ["red", "green", "blue", "yellow", "purple"];
        for i in 0..500u32 {
            g.create_vertex(
                vec!["Thing".into()],
                vec![("color".into(), Value::Text(colors[(i % 5) as usize].into()))],
            )
            .unwrap();
        }
        g.compute_property_selectivity();
        let sel = g.get_property_selectivity();
        let val = sel["vertex:color"];
        // 5 distinct out of 500 → selectivity ~ 0.01, but reservoir-based estimate uses
        // reservoir sample so some variance. Should be < 0.5 given 5 distinct values.
        assert!(
            val > 0.0 && val < 0.5,
            "expected low selectivity for 5 distinct colors, got {val}"
        );
    }

    #[test]
    fn reservoir_fallback_for_rare_property() {
        let mut g = PmaGraph::new(VecMemory::default(), 16).unwrap();
        // Only 1 vertex with the rare property — reservoir won't have enough samples.
        g.create_vertex(
            vec!["Rare".into()],
            vec![("unique_key".into(), Value::Int64(42))],
        )
        .unwrap();
        let key = "vertex:unique_key";
        let kid = g.prop_key_intern.intern(key);
        // estimate_from_reservoir should return None (< MIN_PROPERTY_SAMPLE).
        assert!(g.estimate_from_reservoir(kid).is_none());
    }

    #[test]
    fn reservoir_snapshot_roundtrip() {
        let mut g = PmaGraph::new(VecMemory::default(), 128).unwrap();
        for i in 0..50u32 {
            g.create_vertex(
                vec!["Item".into()],
                vec![("val".into(), Value::Int64(i as i64))],
            )
            .unwrap();
        }
        let res_len = g.reservoir_len_for_test();
        let res_total = g.reservoir_total_seen_for_test();
        let intern_len = g.prop_key_intern_len_for_test();
        assert!(res_len > 0);
        assert!(res_total > 0);
        assert!(intern_len > 0);

        let snap = g.overlay_snapshot();
        let mut g2 = PmaGraph::new(VecMemory::default(), 128).unwrap();
        g2.restore_overlay_snapshot(snap).unwrap();
        assert_eq!(g2.reservoir_len_for_test(), res_len);
        assert_eq!(g2.reservoir_total_seen_for_test(), res_total);
        assert_eq!(g2.prop_key_intern_len_for_test(), intern_len);
    }

    #[test]
    fn reservoir_seeded_on_first_computation() {
        let mut g = PmaGraph::new(VecMemory::default(), 64).unwrap();
        for i in 0..30u32 {
            g.create_vertex(
                vec!["Node".into()],
                vec![("x".into(), Value::Int64(i as i64))],
            )
            .unwrap();
        }
        // Reservoir has entries from mutations already.
        let before = g.reservoir_len_for_test();
        assert!(before > 0, "reservoir should have entries from mutations");

        // After compute, reservoir is seeded if it was empty (it's not, but seed
        // should still leave it non-empty).
        g.compute_property_selectivity();
        assert!(
            g.reservoir_len_for_test() > 0,
            "reservoir should be non-empty after compute"
        );
    }
}
