mod label_expr;
pub use label_expr::{LabelExpr, matches_edge_label};

pub type VertexIdSet = roaring::RoaringBitmap;

use candid::{CandidType, Principal};
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Fixed-point decimal: wraps `rust_decimal::Decimal` with IC Candid support.
/// Serializes as Text over Candid for precision preservation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Decimal(pub rust_decimal::Decimal);

impl CandidType for Decimal {
    fn _ty() -> candid::types::Type {
        <String as CandidType>::_ty() // Text on the wire
    }
    fn idl_serialize<S: candid::types::Serializer>(&self, s: S) -> Result<(), S::Error> {
        self.0.to_string().idl_serialize(s)
    }
}

impl Serialize for Decimal {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Decimal {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        rust_decimal::Decimal::from_str_exact(&s)
            .map(Decimal)
            .map_err(serde::de::Error::custom)
    }
}

impl Decimal {
    pub fn new(d: rust_decimal::Decimal) -> Self {
        Self(d)
    }
    pub fn from_str(s: &str) -> Option<Self> {
        rust_decimal::Decimal::from_str_exact(s).ok().map(Decimal)
    }
    pub fn to_f64(&self) -> Option<f64> {
        use rust_decimal::prelude::ToPrimitive;
        self.0.to_f64()
    }
    pub fn from_i64(v: i64) -> Self {
        Self(rust_decimal::Decimal::from(v))
    }
    pub fn from_u64(v: u64) -> Self {
        Self(rust_decimal::Decimal::from(v))
    }
    pub fn from_i128(v: i128) -> Self {
        Self(rust_decimal::Decimal::from(v))
    }
    pub fn from_u128(v: u128) -> Option<Self> {
        use std::convert::TryFrom;
        rust_decimal::Decimal::try_from(v).ok().map(Self)
    }
    pub fn normalize(&self) -> Self {
        Self(self.0.normalize())
    }
}

impl std::fmt::Display for Decimal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

impl From<Decimal> for Value {
    fn from(d: Decimal) -> Self {
        Value::Decimal(d)
    }
}

/// 256-bit signed integer: wraps `ethnum::I256` with IC Candid support.
/// Serializes as Text over Candid (same pattern as Decimal).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Int256(pub ethnum::I256);

impl CandidType for Int256 {
    fn _ty() -> candid::types::Type {
        <String as CandidType>::_ty()
    }
    fn idl_serialize<S: candid::types::Serializer>(&self, s: S) -> Result<(), S::Error> {
        self.0.to_string().idl_serialize(s)
    }
}

impl Serialize for Int256 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Int256 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse::<ethnum::I256>()
            .map(Int256)
            .map_err(serde::de::Error::custom)
    }
}

impl Int256 {
    pub fn new(v: ethnum::I256) -> Self {
        Self(v)
    }
    pub fn from_str(s: &str) -> Option<Self> {
        s.parse::<ethnum::I256>().ok().map(Int256)
    }
}

impl std::fmt::Display for Int256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

/// 256-bit unsigned integer: wraps `ethnum::U256` with IC Candid support.
/// Serializes as Text over Candid (same pattern as Decimal).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Uint256(pub ethnum::U256);

impl CandidType for Uint256 {
    fn _ty() -> candid::types::Type {
        <String as CandidType>::_ty()
    }
    fn idl_serialize<S: candid::types::Serializer>(&self, s: S) -> Result<(), S::Error> {
        self.0.to_string().idl_serialize(s)
    }
}

impl Serialize for Uint256 {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&self.0.to_string())
    }
}

impl<'de> Deserialize<'de> for Uint256 {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let s = String::deserialize(deserializer)?;
        s.parse::<ethnum::U256>()
            .map(Uint256)
            .map_err(serde::de::Error::custom)
    }
}

impl Uint256 {
    pub fn new(v: ethnum::U256) -> Self {
        Self(v)
    }
    pub fn from_str(s: &str) -> Option<Self> {
        s.parse::<ethnum::U256>().ok().map(Uint256)
    }
}

impl std::fmt::Display for Uint256 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

pub const STABLE_MAGIC: u32 = 0x474C_5048; // GLPH
/// Current stable-memory layout version.
pub const STABLE_VERSION: u16 = 1;

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
/// Stable-memory vertex index entry stored in the vertex array.
pub struct VertexEntry {
    pub edge_index: u64,
    pub degree: u32,
    pub log_offset: i32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
/// Stable-memory edge entry stored in the PMA edge region.
///
/// Layout (24 bytes, little-endian):
/// - `target`:    u32  at offset  0
/// - `weight`:    f32  at offset  4
/// - `timestamp`: u64  at offset  8
/// - `label+flags`: u32 at offset 16 (low 24 bits = label_id, high 8 bits = flags)
/// - `edge_id`:   u32  at offset 20
pub struct EdgeEntry {
    pub target: u32,
    pub weight: f32,
    pub timestamp: u64,
    /// Low 24 bits: label identifier from `LabelIndex` (0 = "no label").
    /// High 8 bits: edge flags.
    pub label_and_flags: u32,
    /// Monotonically-assigned edge identifier.
    pub edge_id: u32,
}

pub const EDGE_LABEL_BITS: u32 = 24;
pub const EDGE_LABEL_MASK: u32 = (1u32 << EDGE_LABEL_BITS) - 1;

pub const EDGE_FLAG_TOMBSTONED: u8 = 1 << 0;
pub const EDGE_FLAG_HAS_PROPS: u8 = 1 << 1;

#[inline]
pub const fn pack_label_and_flags(label_id: u32, flags: u8) -> u32 {
    (label_id & EDGE_LABEL_MASK) | ((flags as u32) << EDGE_LABEL_BITS)
}

#[inline]
pub const fn unpack_edge_label_id(label_and_flags: u32) -> u32 {
    label_and_flags & EDGE_LABEL_MASK
}

#[inline]
pub const fn unpack_edge_flags(label_and_flags: u32) -> u8 {
    (label_and_flags >> EDGE_LABEL_BITS) as u8
}

impl EdgeEntry {
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
        self.flags() & EDGE_FLAG_TOMBSTONED != 0
    }

    #[inline]
    pub fn set_label_id(&mut self, label_id: u32) {
        self.label_and_flags = pack_label_and_flags(label_id, self.flags());
    }

    #[inline]
    pub fn set_flags(&mut self, flags: u8) {
        self.label_and_flags = pack_label_and_flags(self.label_id(), flags);
    }
}

#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq)]
/// Per-segment overflow log entry used before rebalancing.
pub struct LogEntry {
    pub src: u32,
    pub dst: u32,
    pub prev_offset: i32,
    pub weight: f32,
    pub timestamp: u64,
    /// Low 24 bits: label identifier from `LabelIndex` (0 = "no label").
    /// High 8 bits: edge flags.
    pub label_and_flags: u32,
    /// Monotonically-assigned edge identifier.
    pub edge_id: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
/// Fixed-size stable-memory header persisted at the beginning of graph storage.
pub struct StableHeader {
    pub magic: u32,
    pub version: u16,
    pub _pad: u16,
    pub num_vertices: u64,
    pub num_edges: u64,
    pub elem_capacity: u64,
    pub segment_size: u32,
    pub segment_count: u32,
    pub tree_height: u32,
    /// Monotonic counter for assigning unique `edge_id` values to new edges.
    pub next_edge_id: u32,
    pub vertex_array_base: u64,
    pub edge_array_base: u64,
    pub seg_tree_base: u64,
    pub seg_log_base: u64,
    pub seg_log_idx_base: u64,
    pub _reserved: [u8; 4008],
}

impl Default for StableHeader {
    fn default() -> Self {
        Self {
            magic: STABLE_MAGIC,
            version: STABLE_VERSION,
            _pad: 0,
            num_vertices: 0,
            num_edges: 0,
            elem_capacity: 0,
            segment_size: 0,
            segment_count: 0,
            tree_height: 0,
            next_edge_id: 0,
            vertex_array_base: 0,
            edge_array_base: 0,
            seg_tree_base: 0,
            seg_log_base: 0,
            seg_log_idx_base: 0,
            _reserved: [0; 4008],
        }
    }
}

const _: [(); 16] = [(); core::mem::size_of::<VertexEntry>()];
const _: [(); 24] = [(); core::mem::size_of::<EdgeEntry>()];
// LogEntry is serialized to stable memory without padding.
const _: [(); 32] = [(); core::mem::size_of::<LogEntry>()];
const _: [(); 4096] = [(); core::mem::size_of::<StableHeader>()];

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// API payload for creating or ensuring a vertex exists.
pub struct VertexData {
    pub id: u32,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// API payload for inserting a directed weighted edge.
pub struct EdgeData {
    pub src: u32,
    pub dst: u32,
    pub weight: f32,
    pub timestamp: u64,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// API response item describing a neighbor edge.
pub struct EdgeInfo {
    pub target: u32,
    pub weight: f32,
    pub timestamp: u64,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// GQL/runtime scalar value shared across parser, executor, and canister APIs.
pub enum Value {
    Null,
    Bool(bool),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Int128(i128),
    Int256(Int256),
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uint64(u64),
    Uint128(u128),
    Uint256(Uint256),
    Float32(f32),
    Float64(f64),
    Text(String),
    Timestamp(u64),
    List(Vec<Value>),
    Path(Vec<PathElement>),
    Bytes(Vec<u8>),
    Date(i32),
    Time(u64),
    DateTime(i64, u32),
    Duration(i32, i64),
    Principal(Principal),
    Decimal(Decimal),
}

// ── From impls for Value ──

impl From<i8> for Value {
    fn from(v: i8) -> Self {
        Self::Int8(v)
    }
}
impl From<i16> for Value {
    fn from(v: i16) -> Self {
        Self::Int16(v)
    }
}
impl From<i32> for Value {
    fn from(v: i32) -> Self {
        Self::Int32(v)
    }
}
impl From<i64> for Value {
    fn from(v: i64) -> Self {
        Self::Int64(v)
    }
}
impl From<i128> for Value {
    fn from(v: i128) -> Self {
        Self::Int128(v)
    }
}
impl From<u8> for Value {
    fn from(v: u8) -> Self {
        Self::Uint8(v)
    }
}
impl From<u16> for Value {
    fn from(v: u16) -> Self {
        Self::Uint16(v)
    }
}
impl From<u32> for Value {
    fn from(v: u32) -> Self {
        Self::Uint32(v)
    }
}
impl From<u64> for Value {
    fn from(v: u64) -> Self {
        Self::Uint64(v)
    }
}
impl From<u128> for Value {
    fn from(v: u128) -> Self {
        Self::Uint128(v)
    }
}
impl From<f32> for Value {
    fn from(v: f32) -> Self {
        Self::Float32(v)
    }
}
impl From<f64> for Value {
    fn from(v: f64) -> Self {
        Self::Float64(v)
    }
}
impl From<String> for Value {
    fn from(v: String) -> Self {
        Self::Text(v)
    }
}
impl From<&str> for Value {
    fn from(v: &str) -> Self {
        Self::Text(v.to_string())
    }
}
impl From<bool> for Value {
    fn from(v: bool) -> Self {
        Self::Bool(v)
    }
}
impl From<Vec<u8>> for Value {
    fn from(v: Vec<u8>) -> Self {
        Self::Bytes(v)
    }
}
impl From<Principal> for Value {
    fn from(v: Principal) -> Self {
        Self::Principal(v)
    }
}
impl<T: Into<Value>> From<Option<T>> for Value {
    fn from(v: Option<T>) -> Self {
        match v {
            Some(inner) => inner.into(),
            None => Self::Null,
        }
    }
}

// ── Value helper methods ──

impl Value {
    /// Extract value as i64 (works for Int8 through Int64; Int128/Int256 if in range).
    pub fn as_i64(&self) -> Option<i64> {
        match self {
            Self::Int8(v) => Some(*v as i64),
            Self::Int16(v) => Some(*v as i64),
            Self::Int32(v) => Some(*v as i64),
            Self::Int64(v) => Some(*v),
            Self::Int128(v) => i64::try_from(*v).ok(),
            Self::Int256(v) => v.0.try_into().ok(),
            _ => None,
        }
    }

    /// Extract value as i128 (works for Int8 through Int128).
    pub fn as_i128(&self) -> Option<i128> {
        match self {
            Self::Int8(v) => Some(*v as i128),
            Self::Int16(v) => Some(*v as i128),
            Self::Int32(v) => Some(*v as i128),
            Self::Int64(v) => Some(*v as i128),
            Self::Int128(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract value as u128 (works for Uint8 through Uint128).
    pub fn as_u128(&self) -> Option<u128> {
        match self {
            Self::Uint8(v) => Some(*v as u128),
            Self::Uint16(v) => Some(*v as u128),
            Self::Uint32(v) => Some(*v as u128),
            Self::Uint64(v) => Some(*v as u128),
            Self::Uint128(v) => Some(*v),
            _ => None,
        }
    }

    /// Convert any integer variant to f64.
    pub fn as_f64(&self) -> Option<f64> {
        match self {
            Self::Int8(v) => Some(*v as f64),
            Self::Int16(v) => Some(*v as f64),
            Self::Int32(v) => Some(*v as f64),
            Self::Int64(v) => Some(*v as f64),
            Self::Int128(v) => Some(*v as f64),
            Self::Int256(v) => Some(v.0.as_f64()),
            Self::Uint8(v) => Some(*v as f64),
            Self::Uint16(v) => Some(*v as f64),
            Self::Uint32(v) => Some(*v as f64),
            Self::Uint64(v) => Some(*v as f64),
            Self::Uint128(v) => Some(*v as f64),
            Self::Uint256(v) => Some(v.0.as_f64()),
            Self::Float32(v) => Some(*v as f64),
            Self::Float64(v) => Some(*v),
            _ => None,
        }
    }

    /// Extract value as f32. Float32 → direct, Float64 → cast.
    pub fn as_f32(&self) -> Option<f32> {
        match self {
            Self::Float32(v) => Some(*v),
            Self::Float64(v) => Some(*v as f32),
            _ => None,
        }
    }

    /// Returns true for Int8..Int256.
    pub fn is_signed_int(&self) -> bool {
        matches!(
            self,
            Self::Int8(_)
                | Self::Int16(_)
                | Self::Int32(_)
                | Self::Int64(_)
                | Self::Int128(_)
                | Self::Int256(_)
        )
    }

    /// Returns true for Uint8..Uint256.
    pub fn is_unsigned_int(&self) -> bool {
        matches!(
            self,
            Self::Uint8(_)
                | Self::Uint16(_)
                | Self::Uint32(_)
                | Self::Uint64(_)
                | Self::Uint128(_)
                | Self::Uint256(_)
        )
    }

    /// Returns true for any integer variant.
    pub fn is_any_int(&self) -> bool {
        self.is_signed_int() || self.is_unsigned_int()
    }

    /// Returns the bit width for integer variants (8, 16, 32, 64, 128, 256).
    pub fn int_width(&self) -> Option<u16> {
        match self {
            Self::Int8(_) | Self::Uint8(_) => Some(8),
            Self::Int16(_) | Self::Uint16(_) => Some(16),
            Self::Int32(_) | Self::Uint32(_) => Some(32),
            Self::Int64(_) | Self::Uint64(_) => Some(64),
            Self::Int128(_) | Self::Uint128(_) => Some(128),
            Self::Int256(_) | Self::Uint256(_) => Some(256),
            _ => None,
        }
    }
}

/// Narrow an i128 to the smallest signed integer of the given width.
/// Returns Null on overflow.
pub fn narrow_signed(v: i128, width: u16) -> Value {
    match width {
        8 => i8::try_from(v).map(Value::Int8).unwrap_or(Value::Null),
        16 => i16::try_from(v).map(Value::Int16).unwrap_or(Value::Null),
        32 => i32::try_from(v).map(Value::Int32).unwrap_or(Value::Null),
        64 => i64::try_from(v).map(Value::Int64).unwrap_or(Value::Null),
        128 => Value::Int128(v),
        _ => Value::Null,
    }
}

/// Narrow a u128 to the smallest unsigned integer of the given width.
/// Returns Null on overflow.
pub fn narrow_unsigned(v: u128, width: u16) -> Value {
    match width {
        8 => u8::try_from(v).map(Value::Uint8).unwrap_or(Value::Null),
        16 => u16::try_from(v).map(Value::Uint16).unwrap_or(Value::Null),
        32 => u32::try_from(v).map(Value::Uint32).unwrap_or(Value::Null),
        64 => u64::try_from(v).map(Value::Uint64).unwrap_or(Value::Null),
        128 => Value::Uint128(v),
        _ => Value::Null,
    }
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub enum PathElement {
    Node(u32),
    Edge {
        src: u32,
        dst: u32,
        label: Option<String>,
    },
}

#[derive(
    Clone, Copy, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub enum EntityType {
    Vertex,
    Edge,
}

#[derive(
    Clone, Copy, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub enum IndexType {
    Equality,
    Range,
}

#[derive(
    Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq, PartialOrd, Ord, Hash,
)]
pub struct PropertyIndex {
    pub entity_type: EntityType,
    pub property_name: String,
    pub index_type: IndexType,
}

/// Candid-friendly property map representation preserving insertion order.
pub type PropertyMap = Vec<(String, Value)>;

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Vertex record returned by GQL query projections and debugging APIs.
pub struct VertexRecord {
    pub id: u32,
    pub labels: Vec<String>,
    pub props: PropertyMap,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Edge record returned by GQL query projections and debugging APIs.
pub struct EdgeRecord {
    pub src: u32,
    pub dst: u32,
    pub label: Option<String>,
    pub weight: Option<f32>,
    pub timestamp: Option<u64>,
    pub props: PropertyMap,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Fine-grained execution-path counters for query debugging.
///
/// These fields are intentionally coarse and low-cardinality so they can be
/// inspected quickly when a benchmark unexpectedly behaves like a full scan.
pub struct QueryExecutionBreakdown {
    /// `true` if the physical plan included `IndexScan`.
    pub index_fast_path_attempted: bool,
    /// `true` if execution actually used the index fast-path.
    pub index_fast_path_used: bool,
    /// `true` if the physical plan included `Aggregate`.
    pub aggregate_fast_path_attempted: bool,
    /// `true` if execution actually used the aggregate fast-path.
    pub aggregate_fast_path_used: bool,
    /// `true` if aggregate projection used the compiled streaming fast path.
    pub aggregate_compiled_fast_path_used: bool,
    /// `true` if the physical plan included `ShortestPath`.
    pub shortest_fast_path_attempted: bool,
    /// `true` if execution actually used the shortest-path fast-path.
    pub shortest_fast_path_used: bool,
    /// `true` if execution used the recent 2-hop top-k projection fast path.
    pub recent_two_hop_projection_fast_path_used: bool,
    /// `true` if execution used the variable-length terminal projection fast path.
    pub var_len_terminal_projection_fast_path_used: bool,
    /// `true` if execution used the 2-hop terminal-key top-k count fast path.
    pub two_hop_top_k_count_fast_path_used: bool,
    /// Number of binding rows right after MATCH expansion.
    pub rows_after_match: u64,
    /// Number of binding rows after WITH pipelines are applied.
    pub rows_after_with: u64,
    /// Number of binding rows immediately before final projection.
    pub rows_before_projection: u64,
    /// Number of groups formed by aggregate projection.
    pub groups_formed: u64,
    /// Count of top-k selection calls (`ORDER BY ... LIMIT` fast path).
    pub top_k_calls: u64,
    /// Count of full sort calls.
    pub full_sort_calls: u64,
    /// Count of explicit row truncation operations due to `LIMIT`.
    pub limit_truncate_calls: u64,
    /// `true` if `refresh_selectivity_if_stale()` recomputed selectivity.
    pub selectivity_refresh_ran: bool,
    /// Count of `PmaGraph::edge_label()` calls during this query.
    pub edge_label_calls: u64,
    /// Count of `PmaGraph::edge_record()` calls during this query.
    pub edge_record_calls: u64,
    /// Count of `PmaGraph::is_edge_tombstoned()` calls during this query.
    pub is_edge_tombstoned_calls: u64,
    /// Number of reverse-neighbor callbacks processed by the executor.
    pub reverse_neighbor_callbacks: u64,
    /// Number of variable-length DFS frames entered by the executor.
    pub var_len_dfs_calls: u64,
    /// Number of compiled aggregate match records accepted before grouping/finalization.
    pub compiled_match_records: u64,
    /// Number of `Bindings` clones performed in variable-length traversal kernels.
    pub var_len_binding_clones: u64,
    /// Number of path-membership checks (`path.contains`) in variable-length traversal.
    pub var_len_path_contains_checks: u64,
    /// Number of terminal/node predicate checks in variable-length traversal.
    pub var_len_node_match_checks: u64,
    /// Number of row clones performed in reverse anchor traversal.
    pub reverse_row_clones: u64,
    /// Number of node predicate checks in reverse anchor traversal.
    pub reverse_node_match_checks: u64,
    /// Number of group-key expression evaluations in compiled aggregate execution.
    pub compiled_group_key_evals: u64,
    /// Number of existing-group bucket probes in compiled aggregate execution.
    pub compiled_group_bucket_probes: u64,
    /// Number of accumulator updates in compiled aggregate execution.
    pub compiled_agg_updates: u64,
    /// Number of times the row-based compiled aggregate projection was entered.
    pub compiled_projection_fast_calls: u64,
    /// Number of input binding rows seen by the row-based compiled aggregate projection.
    pub compiled_projection_input_rows: u64,
    /// Number of empty-row early returns in the row-based compiled aggregate projection.
    pub compiled_projection_empty_returns: u64,
    /// Number of WITH continuation MATCH invocations.
    pub with_continuation_match_calls: u64,
    /// Total input rows fed into WITH continuation MATCH clauses.
    pub with_continuation_match_input_rows: u64,
    /// Total output rows produced by WITH continuation MATCH clauses.
    pub with_continuation_match_output_rows: u64,
    /// Total number of start candidates enumerated inside joined MATCH execution.
    pub joined_match_start_candidates: u64,
    /// Total local rows produced before inline-WHERE safety-net filtering.
    pub joined_match_local_rows_before_inline_where: u64,
    /// Total local rows remaining after inline-WHERE safety-net filtering.
    pub joined_match_local_rows_after_inline_where: u64,
    /// Delta start-candidate count attributable to WITH continuation MATCH only.
    pub with_continuation_joined_match_start_candidates: u64,
    /// Delta local rows before inline-WHERE attributable to WITH continuation MATCH only.
    pub with_continuation_joined_local_rows_before_inline_where: u64,
    /// Delta local rows after inline-WHERE attributable to WITH continuation MATCH only.
    pub with_continuation_joined_local_rows_after_inline_where: u64,
    /// Delta scanned-edge count attributable to WITH continuation MATCH only.
    pub with_continuation_scanned_edges: u64,
    /// Delta execution-step count attributable to WITH continuation MATCH only.
    pub with_continuation_execution_steps: u64,
    /// Count of outgoing hop candidates examined by generic matcher kernels.
    pub outgoing_hop_candidates: u64,
    /// Count of incoming/reverse hop candidates examined by generic matcher kernels.
    pub incoming_hop_candidates: u64,
    /// Count of hop candidates rejected by label mismatch.
    pub hop_label_rejects: u64,
    /// Count of outgoing hop candidates rejected by label mismatch.
    pub outgoing_hop_label_rejects: u64,
    /// Count of incoming/reverse hop candidates rejected by label mismatch.
    pub incoming_hop_label_rejects: u64,
    /// Count of hop candidates rejected by node predicate mismatch.
    pub hop_node_rejects: u64,
    /// Count of hop candidates rejected by edge property mismatch.
    pub hop_edge_property_rejects: u64,
    /// Count of hop candidates rejected by WHERE pushdown.
    pub hop_where_pushdown_rejects: u64,
    /// Count of variable-length candidates rejected due to revisiting a path vertex.
    pub var_len_cycle_rejects: u64,
    /// Delta label-mismatch rejects attributable to WITH continuation MATCH only.
    pub with_continuation_hop_label_rejects: u64,
    /// Delta node-mismatch rejects attributable to WITH continuation MATCH only.
    pub with_continuation_hop_node_rejects: u64,
    /// Delta edge-property rejects attributable to WITH continuation MATCH only.
    pub with_continuation_hop_edge_property_rejects: u64,
    /// Delta WHERE-pushdown rejects attributable to WITH continuation MATCH only.
    pub with_continuation_hop_where_pushdown_rejects: u64,
    /// Delta var-len cycle rejects attributable to WITH continuation MATCH only.
    pub with_continuation_var_len_cycle_rejects: u64,
    /// Delta outgoing hop candidates attributable to WITH continuation MATCH only.
    pub with_continuation_outgoing_hop_candidates: u64,
    /// Delta incoming/reverse hop candidates attributable to WITH continuation MATCH only.
    pub with_continuation_incoming_hop_candidates: u64,
    /// Delta outgoing label-mismatch rejects attributable to WITH continuation MATCH only.
    pub with_continuation_outgoing_hop_label_rejects: u64,
    /// Delta incoming/reverse label-mismatch rejects attributable to WITH continuation MATCH only.
    pub with_continuation_incoming_hop_label_rejects: u64,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Executor statistics exposed for observability and guardrail debugging.
pub struct QueryStats {
    pub scanned_vertices: u64,
    pub scanned_edges: u64,
    pub rows_emitted: u64,
    pub execution_steps: u64,
    pub breakdown: QueryExecutionBreakdown,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Tabular response envelope for read-only GQL statements.
pub struct QueryResult {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
    pub stats: QueryStats,
    pub warnings: Vec<TypeDiagnostic>,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Mutation response envelope for CREATE/DELETE statements.
pub struct MutationResult {
    pub affected_vertices: u64,
    pub affected_edges: u64,
    pub warnings: Vec<TypeDiagnostic>,
}

/// Unified response from the `execute_gql` endpoint.
///
/// Wraps either a query result, a mutation result, or a graph catalog result
/// so that a single async endpoint can handle all GQL statement kinds.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub enum ExecuteGqlResult {
    Query(QueryResultWithContinuation),
    Mutation(MutationResult),
    GraphCreated(GraphInfo),
    GraphDropped(bool),
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// High-level graph statistics returned by the graph canister.
pub struct GraphStats {
    pub num_vertices: u64,
    pub num_edges: u64,
    pub elem_capacity: u64,
    pub segment_size: u32,
    pub segment_count: u32,
    pub avg_degree: f64,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Planner-oriented statistics exposed by the graph canister.
///
/// `label_cardinality` and `property_selectivity` are serialized as sorted `(key, value)` pairs
/// because Candid records do not support arbitrary-keyed maps.
pub struct PlannerStats {
    /// Active vertex count per label (sorted by label name).
    pub label_cardinality: Vec<(String, u64)>,
    /// Average out-degree (edges / vertices).
    pub avg_degree: f64,
    /// Selectivity estimates keyed by `entity:prop` (e.g. `vertex:uid`).
    /// A value of 1.0 means all sampled vertices had a distinct value; 0.0 means a single value.
    pub property_selectivity: Vec<(String, f64)>,
    /// Vertex property names that have a registered equality index.
    pub indexed_vertex_properties: Vec<String>,
    /// Vertex property names that have a registered range index.
    pub range_indexed_vertex_properties: Vec<String>,
    /// Total active vertices.
    pub vertex_count: u64,
    /// Total active edges.
    pub edge_count: u64,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Inclusive timestamp range used by temporal edge filtering.
pub struct TimestampRange {
    pub start: Option<u64>,
    pub end: Option<u64>,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Collaborative filtering recommendation result.
pub struct Recommendation {
    pub vertex_id: u32,
    pub score: f64,
    pub path: Vec<u32>,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Breadth-first search result envelope.
pub struct BfsResult {
    pub visited: Vec<u32>,
    pub distances: Vec<(u32, u32)>,
    pub path: Option<Vec<u32>>,
}

#[derive(
    Clone,
    Debug,
    Default,
    CandidType,
    Deserialize,
    Serialize,
    PartialEq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
/// PageRank computation result envelope.
pub struct PageRankResult {
    pub scores: Vec<(u32, f64)>,
    pub iterations: u32,
    pub converged: bool,
}

#[derive(
    Clone,
    Debug,
    Default,
    CandidType,
    Deserialize,
    Serialize,
    PartialEq,
    rkyv::Archive,
    rkyv::Serialize,
    rkyv::Deserialize,
)]
/// Single-source shortest path result envelope.
pub struct SsspResult {
    pub distances: Vec<(u32, f64)>,
    pub predecessors: Vec<(u32, Option<u32>)>,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Operational metrics for monitoring query, mutation, and algorithm usage.
pub struct OperationalMetrics {
    /// Total successful GQL query executions.
    pub query_count: u64,
    /// Total successful GQL mutation executions (including individual batch items).
    pub mutation_count: u64,
    /// Total GQL requests rejected (parse error, validation error, quota exceeded, etc.).
    pub rejected_count: u64,
    /// Total algorithm calls (BFS, PageRank, SSSP, Recommend).
    pub algorithm_calls: u64,
    /// Stable memory bytes in use (always 0 on non-wasm targets).
    pub stable_memory_bytes: u64,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Per-tenant resource quotas enforced at the gql_bridge layer.
pub struct UsageQuota {
    /// Maximum vertex count allowed. 0 means unlimited.
    pub max_vertices: u64,
    /// Maximum edge count allowed. 0 means unlimited.
    pub max_edges: u64,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Certified query response payload with certificate and witness bytes.
pub struct CertifiedResponse<T> {
    pub data: T,
    pub certificate: Vec<u8>,
    pub witness: Vec<u8>,
}

#[derive(Clone, Debug, Error, CandidType, Deserialize, Serialize, PartialEq)]
/// Shared domain errors used across graph and registry crates.
pub enum GleaphError {
    #[error("vertex not found: {0}")]
    VertexNotFound(u32),
    #[error("out of capacity")]
    OutOfCapacity,
    #[error("invalid header")]
    InvalidHeader,
    #[error("memory error: {0}")]
    Memory(String),
    #[error("unsupported: {0}")]
    Unsupported(String),
    #[error("gql parse error: {0}")]
    ParseError(String),
    #[error("gql validation error: {0}")]
    ValidationError(String),
    #[error("gql unsupported feature: {0}")]
    UnsupportedFeature(String),
    #[error("gql execution error: {0}")]
    ExecutionError(String),
    #[error("instruction budget exhausted")]
    BudgetExhausted,
    #[error("algorithm error: {0}")]
    AlgorithmError(String),
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Registry-side configuration used to provision a graph canister.
pub struct GraphConfig {
    pub name: String,
    pub initial_vertex_capacity: u32,
    pub initial_edge_capacity: u64,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Access control levels for registry-managed graphs.
pub enum AccessLevel {
    Execute,
    Read,
    Write,
    Admin,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Whether a prepared statement is a read-only query or a mutation.
pub enum PreparedKind {
    Query,
    Mutation,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq, Eq)]
/// Dynamic sort configuration declared on a prepared statement.
pub struct PreparedOptions {
    /// Optional human-written description used in generated API docs.
    pub description: Option<String>,
    /// Allowed dynamic sort keys for this prepared statement.
    pub allowed_sorts: Vec<PreparedSortKey>,
    /// Optional default sort used when execute_prepared() omits the sort argument.
    pub default_sort: Option<Vec<PreparedSortSpec>>,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
/// Publicly exposed dynamic sort key for a prepared statement.
pub struct PreparedSortKey {
    /// Stable external identifier used by SDK/UI callers, e.g. "age".
    pub key: String,
    /// GQL expression bound to this key, e.g. "u.age".
    pub expr: String,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
/// Concrete sort request supplied when executing a prepared query.
pub struct PreparedSortSpec {
    /// One of PreparedOptions.allowed_sorts.key.
    pub key: String,
    /// true = DESC, false = ASC.
    pub descending: bool,
    /// None = engine default, Some(true) = NULLS FIRST, Some(false) = NULLS LAST.
    pub nulls_first: Option<bool>,
}

/// Scalar element type for typed lists in prepared statement metadata.
///
/// Used as the inner type of `PreparedValueType::TypedList`.
#[derive(Clone, Copy, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub enum PreparedScalarType {
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    Int256,
    Uint8,
    Uint16,
    Uint32,
    Uint64,
    Uint128,
    Uint256,
    Float32,
    Float64,
    Text,
    Bool,
    Timestamp,
    Bytes,
    Date,
    Time,
    DateTime,
    Duration,
    Principal,
    Decimal,
}

/// Scalar value type exposed in prepared statement metadata.
///
/// Mirrors `ast::ValueType` but lives in the public API crate so that Candid
/// serialisation and SDK codegen can consume it without depending on the GQL
/// engine internals.
#[derive(Clone, Copy, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub enum PreparedValueType {
    Int8,
    Int16,
    Int32,
    Int64,
    Int128,
    Int256,
    Uint8,
    Uint16,
    Uint32,
    Uint64,
    Uint128,
    Uint256,
    Float32,
    Float64,
    Text,
    Bool,
    Timestamp,
    List,
    /// Typed list with known element type: `LIST<INT>`, `LIST<TEXT>`, etc.
    TypedList(PreparedScalarType),
    Null,
    Bytes,
    Date,
    Time,
    DateTime,
    Duration,
    Principal,
    Decimal,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
/// Metadata for a prepared statement parameter.
pub struct PreparedParameterInfo {
    /// Wire parameter name without the `$` prefix.
    pub name: String,
    /// Whether execute_prepared requires this parameter to be present.
    ///
    /// Parameters annotated with `:: ... | NULL` are exposed as optional.
    pub required: bool,
    /// Inferred or annotated value types for this parameter.
    ///
    /// Empty means the type could not be determined (treated as `Value`).
    /// One entry means a concrete scalar type.
    /// Multiple entries mean a union (e.g. `INT | NULL`).
    pub types: Vec<PreparedValueType>,
    /// `true` when `types` was derived from reverse inference rather than
    /// an explicit `:: TYPE` annotation.  Inferred types may be conservative.
    pub inferred: bool,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Metadata about a registered prepared statement, returned by `prepare` and `list_prepared`.
pub struct PreparedStatementInfo {
    /// Registered name.
    pub name: String,
    /// Query or Mutation.
    pub kind: PreparedKind,
    /// User-supplied parameter metadata.
    pub parameters: Vec<PreparedParameterInfo>,
    /// RETURN clause column names (empty for mutations).
    pub columns: Vec<String>,
    /// Whether this statement uses `caller()` (resolved via `ic_cdk::api::msg_caller()`).
    pub requires_caller: bool,
    /// Original GQL source.
    pub source: String,
    /// Optional human-written description used in generated API docs.
    pub description: Option<String>,
    /// Allowed dynamic sort keys for query prepared statements.
    pub allowed_sorts: Vec<PreparedSortKey>,
    /// Default sort applied when execute_prepared() does not specify one.
    pub default_sort: Option<Vec<PreparedSortSpec>>,
    /// Schema-aware static type-check warnings captured at prepare time.
    pub type_warnings: Vec<TypeDiagnostic>,
}

#[derive(Clone, Copy, Debug, CandidType, Deserialize, Serialize, PartialEq, Eq)]
pub enum TypeDiagnosticKind {
    Info,
    BinaryOpMismatch,
    NonBooleanCondition,
    FunctionArgMismatch,
    ComparisonMismatch,
    NullCheckOnNonNull,
    ImpossiblePattern,
    GroupingViolation,
    ParameterInferenceConflict,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub struct TypeDiagnostic {
    pub kind: TypeDiagnosticKind,
    pub message: String,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// ACL entry associating a principal with an access level for a graph canister.
pub struct AclEntry {
    pub principal: Principal,
    pub level: AccessLevel,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Maps a human-readable graph name to a canister principal for cross-canister routing.
pub struct GraphAlias {
    pub name: String,
    pub canister_id: Principal,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Registry record exposed to clients for a provisioned graph.
pub struct GraphInfo {
    pub id: u64,
    pub name: String,
    pub canister_id: Option<Principal>,
    pub owner: Principal,
    pub max_vertices: u32,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// Diagnostic information about a graph canister's current state.
///
/// Returned by the `get_canister_info()` query endpoint for monitoring and canary deployments.
pub struct CanisterInfo {
    /// Current stable-memory layout version.
    pub layout_version: u16,
    /// Wasm module hash as a hex string (empty in native tests).
    pub wasm_hash: String,
    /// Nanoseconds elapsed since the canister was created (0 in native tests).
    pub uptime_ns: u64,
    /// Timestamp of the last `post_upgrade` call in nanoseconds (0 in native tests).
    pub last_upgrade_ns: u64,
}

// ── Algorithm continuation tokens ──────────────────────────────────────

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Identifies which algorithm a continuation token belongs to.
pub enum AlgorithmKind {
    Bfs,
    Sssp,
    PageRank,
    GqlQuery,
    Mutation,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Lightweight graph identity used to detect mutations between continuation calls.
pub struct GraphFingerprint {
    pub num_vertices: u64,
    pub num_edges: u64,
    pub next_edge_id: u32,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Opaque continuation token returned when an algorithm is suspended due to budget exhaustion.
///
/// The `data` field contains CBOR-serialized algorithm checkpoint state.
/// The `graph_fingerprint` is validated on resume to detect graph mutations.
pub struct ContinuationToken {
    pub kind: AlgorithmKind,
    pub data: Vec<u8>,
    pub graph_fingerprint: GraphFingerprint,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// BFS result with optional continuation for resumable execution.
pub struct BfsResultWithContinuation {
    pub result: BfsResult,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// SSSP result with optional continuation for resumable execution.
pub struct SsspResultWithContinuation {
    pub result: SsspResult,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Clone, Debug, Default, CandidType, Deserialize, Serialize, PartialEq)]
/// PageRank result with optional continuation for resumable execution.
pub struct PageRankResultWithContinuation {
    pub result: PageRankResult,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Query result with optional continuation for cursor-based pagination.
pub struct QueryResultWithContinuation {
    pub result: QueryResult,
    pub continuation: Option<ContinuationToken>,
}

/// Serializable checkpoint for a suspended DELETE mutation.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub struct DeleteCheckpoint {
    pub remaining_vertices: Vec<u32>,
    pub remaining_edges: Vec<(u32, u32, Option<String>)>,
    pub affected_vertices: u64,
    pub affected_edges: u64,
    pub affected_vertex_ids: Vec<u32>,
}

/// Serializable checkpoint for a suspended SET or REMOVE mutation.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub struct SetRemoveCheckpoint {
    pub remaining_ops: Vec<MutationOp>,
    pub affected_vertices: u64,
    pub affected_edges: u64,
    pub affected_vertex_ids: Vec<u32>,
}

/// A single, flat, serializable mutation operation (used for SET/REMOVE continuation).
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub enum MutationOp {
    SetVertexProp {
        id: u32,
        property: String,
        value: Value,
    },
    SetEdgeProp {
        src: u32,
        dst: u32,
        label: Option<String>,
        property: String,
        value: Value,
    },
    AddVertexLabel {
        id: u32,
        label: String,
    },
    RemoveVertexProp {
        id: u32,
        property: String,
    },
    RemoveEdgeProp {
        src: u32,
        dst: u32,
        label: Option<String>,
        property: String,
    },
    RemoveVertexLabel {
        id: u32,
        label: String,
    },
}

/// Discriminated checkpoint for all resumable mutation types.
#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
pub enum MutationCheckpoint {
    Delete(DeleteCheckpoint),
    Set(SetRemoveCheckpoint),
    Remove(SetRemoveCheckpoint),
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Mutation result with optional continuation for resumable mutations.
pub struct MutationResultWithContinuation {
    pub result: MutationResult,
    pub continuation: Option<ContinuationToken>,
}

#[derive(Clone, Debug, CandidType, Deserialize, Serialize, PartialEq)]
/// Discriminated result from `query_continue` / `mutate_continue`, wrapping the type-specific result.
pub enum ContinuationResult {
    Bfs(BfsResultWithContinuation),
    Sssp(SsspResultWithContinuation),
    PageRank(PageRankResultWithContinuation),
    Query(QueryResultWithContinuation),
    Mutation(MutationResultWithContinuation),
}
