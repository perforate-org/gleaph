//! Router-facing wire types, the query response envelope, read freshness contracts, and the
//! typed-binding helpers used by `gleaph-codegen` output.

use candid::{CandidType, Deserialize};
use gleaph_gql_value::types::{Decimal, Int256, Uint256};

/// Principal extension used when projecting IC wire values into logical GQL values.
pub use gleaph_gql_ic_wire::GqlPrincipal;
/// Decode error for the Router rows blob.
pub use gleaph_gql_ic_wire::GqlWireDecodeError;
/// One row in the wire rows blob.
pub use gleaph_gql_ic_wire::GqlWireRow;
/// Materialized rows returned in a Router response blob.
pub use gleaph_gql_ic_wire::GqlWireRows;
/// One wire value in a materialized row.
pub use gleaph_gql_ic_wire::GqlWireValue;
use gleaph_gql_ic_wire::decode_rows_blob;
use gleaph_gql_params::GqlParams;
/// Logical GQL value shared by dynamic GQL, prepared operations, and procedures.
pub use gleaph_gql_params::GqlValue;

/// Path element in a logical GQL value.
pub use gleaph_gql_value::types::PathElement as GqlPathElement;

/// Ordered GQL record representation.
///
/// GQL record order is retained because the compact wire representation preserves field order.
pub type GqlRecord = GqlParams;

/// One GQL result row.
pub type GqlRow = GqlParams;

/// Federated mutation lifecycle phase for idempotent mutations (ADR 0029).
///
/// Mirrors `gleaph_graph_kernel::plan_exec::MutationLifecyclePhase`; variant order is part of
/// the Candid wire contract and must stay in lockstep.
#[derive(CandidType, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum MutationLifecyclePhase {
    /// Router is resolving and durably recording the immutable dispatch envelope.
    Routing,
    /// At least one required canonical shard outcome is not yet known.
    CanonicalPending,
    /// All required canonical shard writes are durable; no projection has advanced yet.
    CanonicalCommitted,
    /// Canonical writes are durable; one or more required derived projections still lag.
    ProjectionPending,
    /// Canonical writes and every projection required by the mutation contract converged.
    Completed,
    /// Validation or execution failed before any canonical write committed.
    Failed,
}

/// Router query/mutation response envelope.
///
/// Mirrors `gleaph_graph_kernel::plan_exec::GqlQueryResult` (Candid variant order and field
/// names are the wire contract). `phase`/`token` are only populated for idempotent mutations;
/// reads always yield `None`.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GqlQueryResult {
    /// Number of rows returned by the Router.
    pub row_count: u64,
    /// Optional compact Candid rows blob.
    pub rows_blob: Option<Vec<u8>>,
    /// Federated mutation lifecycle phase for idempotent mutations (ADR 0029).
    pub phase: Option<MutationLifecyclePhase>,
    /// Read-your-writes token for idempotent mutations (ADR 0029 §5, Phase 2). `None`
    /// for reads and untracked escape-hatch writes.
    pub token: Option<MutationToken>,
}

impl GqlQueryResult {
    /// Return the materialized rows blob, if the query produced one.
    pub fn rows_blob(&self) -> Option<&[u8]> {
        self.rows_blob.as_deref()
    }

    /// Decode the materialized rows using the lightweight public IC wire codec.
    pub fn decode_rows(&self) -> Result<Option<GqlWireRows>, GqlWireDecodeError> {
        self.rows_blob.as_deref().map(decode_rows_blob).transpose()
    }

    /// Decode materialized rows directly into generated Rust row types.
    pub fn decode_typed_rows<Row: FromGqlRow>(
        &self,
    ) -> Result<Option<Vec<Row>>, GqlWireDecodeError> {
        self.decode_rows()?
            .map(|rows| {
                rows.rows
                    .into_iter()
                    .map(|row| Row::from_gql_row(row.try_into_gql_row()?))
                    .collect()
            })
            .transpose()
    }

    /// Decode materialized rows into any serde-deserializable row type.
    ///
    /// This is the path used by `gleaph-codegen`'s `PreparedCanisterExecutor`, whose `Row`
    /// bound is `DeserializeOwned`: each wire row is projected to JSON (see
    /// [`gql_value_to_json`]) and deserialized with the row type's serde derives and renames.
    pub fn decode_serde_rows<Row>(&self) -> Result<Option<Vec<Row>>, GqlWireDecodeError>
    where
        Row: serde::de::DeserializeOwned,
    {
        self.decode_rows()?
            .map(|rows| {
                rows.rows
                    .into_iter()
                    .map(|row| {
                        let row = row.try_into_gql_row()?;
                        let mut object = serde_json::Map::new();
                        for (name, value) in row {
                            object.insert(name, gql_value_to_json(value)?);
                        }
                        serde_json::from_value(serde_json::Value::Object(object))
                            .map_err(|error| GqlWireDecodeError::Json(error.to_string()))
                    })
                    .collect()
            })
            .transpose()
    }
}

/// Read freshness contract accepted by Router query entry points.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq, Default)]
pub enum ReadMode {
    /// Non-blocking; may observe documented projection lag.
    #[default]
    Eventual,
    /// Block (retryable) until every shard reaches the token's watermarks.
    AtLeast(MutationToken),
}

/// Read-your-writes token returned by an idempotent Router mutation.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MutationToken {
    /// Router-issued mutation id; `0` is reserved and ids are never reused.
    pub mutation_id: u64,
    /// Per-shard watermarks the read must reach.
    pub shards: Vec<MutationTokenShard>,
}

/// Per-shard watermarks carried by a Router mutation token.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct MutationTokenShard {
    /// Shard id.
    pub shard_id: u32,
    /// Highest label-stats delta seq the label-stats projection must reach on this shard.
    pub label_stats_seq: Option<u64>,
}

/// Convert one logical GQL result row into a generated Rust row type.
pub trait FromGqlRow: Sized {
    /// Convert an ordered logical GQL record into this typed row.
    fn from_gql_row(row: GqlRow) -> Result<Self, GqlWireDecodeError>;
}

/// Remove one named field from an ordered logical GQL row.
pub fn take_gql_row_field(row: &mut GqlRow, name: &str) -> Result<GqlValue, GqlWireDecodeError> {
    let mut found = None;
    for (index, (field_name, _)) in row.iter().enumerate() {
        if field_name == name {
            if found.is_some() {
                return Err(GqlWireDecodeError::DuplicateField(name.to_string()));
            }
            found = Some(index);
        }
    }
    found
        .map(|index| row.remove(index).1)
        .ok_or_else(|| GqlWireDecodeError::MissingField(name.to_string()))
}

/// Rust signed binary256 integer used by generated canister bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GqlInt256(Int256);

impl GqlInt256 {
    /// Construct from the shared GQL integer wrapper.
    pub const fn from_inner(value: Int256) -> Self {
        Self(value)
    }

    /// Return the shared GQL integer wrapper.
    pub const fn into_inner(self) -> Int256 {
        self.0
    }
}

impl core::fmt::Display for GqlInt256 {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl core::str::FromStr for GqlInt256 {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<Int256>()
            .map(Self::from_inner)
            .map_err(|error| error.to_string())
    }
}

impl serde::Serialize for GqlInt256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for GqlInt256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <Self as core::str::FromStr>::from_str(&String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

/// Rust unsigned binary256 integer used by generated canister bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GqlUint256(Uint256);

impl GqlUint256 {
    /// Construct from the shared GQL integer wrapper.
    pub const fn from_inner(value: Uint256) -> Self {
        Self(value)
    }

    /// Return the shared GQL integer wrapper.
    pub const fn into_inner(self) -> Uint256 {
        self.0
    }
}

impl core::fmt::Display for GqlUint256 {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl core::str::FromStr for GqlUint256 {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        value
            .parse::<Uint256>()
            .map(Self::from_inner)
            .map_err(|error| error.to_string())
    }
}

impl serde::Serialize for GqlUint256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for GqlUint256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <Self as core::str::FromStr>::from_str(&String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

/// Rust decimal value used by generated canister bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GqlDecimal(Decimal);

impl GqlDecimal {
    /// Construct from the shared GQL decimal wrapper.
    pub const fn from_inner(value: Decimal) -> Self {
        Self(value)
    }

    /// Return the shared GQL decimal wrapper.
    pub const fn into_inner(self) -> Decimal {
        self.0
    }
}

impl core::fmt::Display for GqlDecimal {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        self.0.fmt(formatter)
    }
}

impl core::str::FromStr for GqlDecimal {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        Decimal::parse(value)
            .map(Self::from_inner)
            .ok_or_else(|| format!("invalid GQL decimal {value:?}"))
    }
}

impl serde::Serialize for GqlDecimal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> serde::Deserialize<'de> for GqlDecimal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <Self as core::str::FromStr>::from_str(&String::deserialize(deserializer)?)
            .map_err(serde::de::Error::custom)
    }
}

/// Rust binary16 value used by generated canister bindings.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GqlFloat16(half::f16);

impl GqlFloat16 {
    /// Construct from the IEEE 754 binary16 bit pattern.
    pub const fn from_bits(bits: u16) -> Self {
        Self(half::f16::from_bits(bits))
    }

    /// Return the IEEE 754 binary16 bit pattern.
    pub const fn to_bits(self) -> u16 {
        self.0.to_bits()
    }

    /// Return the upstream half-precision value.
    pub const fn into_inner(self) -> half::f16 {
        self.0
    }
}

impl serde::Serialize for GqlFloat16 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_u16(self.to_bits())
    }
}

impl<'de> serde::Deserialize<'de> for GqlFloat16 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Ok(Self::from_bits(u16::deserialize(deserializer)?))
    }
}

/// Rust binary128 value used by generated canister bindings.
#[cfg(feature = "nightly-f128")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GqlFloat128(f128);

#[cfg(feature = "nightly-f128")]
impl GqlFloat128 {
    /// Construct a logical GQL float128 from the primitive value.
    pub const fn from_inner(value: f128) -> Self {
        Self(value)
    }

    /// Return the primitive value.
    pub const fn into_inner(self) -> f128 {
        self.0
    }

    /// Construct from the canonical little-endian wire representation.
    pub const fn from_le_bytes(bytes: [u8; 16]) -> Self {
        Self(f128::from_bits(u128::from_le_bytes(bytes)))
    }

    /// Return the canonical little-endian wire representation.
    pub const fn to_le_bytes(self) -> [u8; 16] {
        self.0.to_bits().to_le_bytes()
    }
}

#[cfg(feature = "nightly-f128")]
impl serde::Serialize for GqlFloat128 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.to_le_bytes())
    }
}

#[cfg(feature = "nightly-f128")]
impl<'de> serde::Deserialize<'de> for GqlFloat128 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        let bytes: [u8; 16] = bytes
            .try_into()
            .map_err(|bytes: Vec<u8>| serde::de::Error::invalid_length(bytes.len(), &"16 bytes"))?;
        Ok(Self::from_le_bytes(bytes))
    }
}

/// Rust binary256 value used by generated canister bindings.
///
/// The Serde representation is exactly 32 little-endian bytes. The wrapper is necessary because
/// the upstream `f256` type intentionally does not provide Serde implementations.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GqlFloat256(f256::f256);

impl GqlFloat256 {
    /// Construct a logical GQL float256 from the upstream numeric value.
    pub const fn from_inner(value: f256::f256) -> Self {
        Self(value)
    }

    /// Return the upstream numeric value.
    pub const fn into_inner(self) -> f256::f256 {
        self.0
    }

    /// Construct from the canonical little-endian wire representation.
    pub const fn from_le_bytes(bytes: [u8; 32]) -> Self {
        Self(f256::f256::from_le_bytes(bytes))
    }

    /// Return the canonical little-endian wire representation.
    pub const fn to_le_bytes(self) -> [u8; 32] {
        self.0.to_le_bytes()
    }
}

impl serde::Serialize for GqlFloat256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.to_le_bytes())
    }
}

impl<'de> serde::Deserialize<'de> for GqlFloat256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let bytes = Vec::<u8>::deserialize(deserializer)?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|bytes: Vec<u8>| serde::de::Error::invalid_length(bytes.len(), &"32 bytes"))?;
        Ok(Self::from_le_bytes(bytes))
    }
}

/// Project a logical GQL value into the JSON representation used by generated open records.
// The workspace can unify `gleaph-gql-value/f128` through another dependent crate (e.g.
// `gleaph-gql` default features) even when this SDK's `nightly-f128` feature is disabled, while a
// standalone SDK build has no Float128 variant.
#[allow(
    unreachable_patterns,
    reason = "Float128 depends on the SDK nightly-f128 feature"
)]
pub fn gql_value_to_json(value: GqlValue) -> Result<serde_json::Value, GqlWireDecodeError> {
    let json = match value {
        GqlValue::Null => serde_json::Value::Null,
        GqlValue::Bool(value) => serde_json::Value::Bool(value),
        GqlValue::Int8(value) => serde_json::json!(value),
        GqlValue::Int16(value) => serde_json::json!(value),
        GqlValue::Int32(value) => serde_json::json!(value),
        GqlValue::Int64(value) => serde_json::json!(value),
        GqlValue::Int128(value) => serde_json::Value::String(value.to_string()),
        GqlValue::Int256(value) => serde_json::Value::String(value.to_string()),
        GqlValue::Uint8(value) => serde_json::json!(value),
        GqlValue::Uint16(value) => serde_json::json!(value),
        GqlValue::Uint32(value) => serde_json::json!(value),
        GqlValue::Uint64(value) => serde_json::json!(value),
        GqlValue::Uint128(value) => serde_json::Value::String(value.to_string()),
        GqlValue::Uint256(value) => serde_json::Value::String(value.to_string()),
        GqlValue::Float16(value) => serde_json::json!(value.to_f32()),
        GqlValue::Float32(value) => serde_json::to_value(value)
            .map_err(|error| GqlWireDecodeError::Json(error.to_string()))?,
        GqlValue::Float64(value) => serde_json::to_value(value)
            .map_err(|error| GqlWireDecodeError::Json(error.to_string()))?,
        #[cfg(feature = "nightly-f128")]
        GqlValue::Float128(value) => serde_json::to_value(GqlFloat128::from_inner(value))
            .map_err(|error| GqlWireDecodeError::Json(error.to_string()))?,
        GqlValue::Float256(value) => serde_json::Value::String(value.to_string()),
        GqlValue::Decimal(value) => serde_json::Value::String(value.to_string()),
        GqlValue::Text(value) => serde_json::Value::String(value),
        GqlValue::Bytes(value) => serde_json::Value::Array(
            value
                .into_iter()
                .map(|value| serde_json::json!(value))
                .collect(),
        ),
        GqlValue::Date(value) => serde_json::json!(value),
        GqlValue::Time(value) => serde_json::json!(value),
        GqlValue::LocalTime(value) => serde_json::json!(value),
        GqlValue::DateTime(seconds, nanos) | GqlValue::LocalDateTime(seconds, nanos) => {
            serde_json::json!({ "seconds": seconds, "nanos": nanos })
        }
        GqlValue::ZonedDateTime(seconds, nanos, offset_seconds) => {
            serde_json::json!({
                "seconds": seconds,
                "nanos": nanos,
                "offset_seconds": offset_seconds
            })
        }
        GqlValue::ZonedTime(nanos, offset_seconds) => {
            serde_json::json!({ "nanos": nanos, "offset_seconds": offset_seconds })
        }
        GqlValue::Duration(months, nanos) => {
            serde_json::json!({ "months": months, "nanos": nanos })
        }
        GqlValue::List(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(gql_value_to_json)
                .collect::<Result<_, _>>()?,
        ),
        GqlValue::Path(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(|value| match value {
                    GqlPathElement::Vertex(value) => {
                        serde_json::json!({ "Vertex": value.as_ref().to_vec() })
                    }
                    GqlPathElement::Edge(value) => {
                        serde_json::json!({ "Edge": value.as_ref().to_vec() })
                    }
                })
                .collect(),
        ),
        GqlValue::Record(values) => {
            let mut object = serde_json::Map::new();
            for (name, value) in values {
                if object.contains_key(&name) {
                    return Err(GqlWireDecodeError::DuplicateField(name));
                }
                object.insert(name, gql_value_to_json(value)?);
            }
            serde_json::Value::Object(object)
        }
        GqlValue::Extension(value) => value
            .as_any()
            .downcast_ref::<GqlPrincipal>()
            .map(|principal| serde_json::Value::String(principal.to_string()))
            .ok_or(GqlWireDecodeError::UnsupportedValue("extension"))?,
        #[cfg(not(feature = "nightly-f128"))]
        _ => return Err(GqlWireDecodeError::UnsupportedValue("Float128")),
    };
    Ok(json)
}

/// Project a logical GQL record into the generated open-record map shape.
pub fn gql_record_to_json_map(
    value: GqlValue,
) -> Result<std::collections::BTreeMap<String, serde_json::Value>, GqlWireDecodeError> {
    match value {
        GqlValue::Record(values) => {
            let mut record = std::collections::BTreeMap::new();
            for (name, value) in values {
                if record.contains_key(&name) {
                    return Err(GqlWireDecodeError::DuplicateField(name));
                }
                record.insert(name, gql_value_to_json(value)?);
            }
            Ok(record)
        }
        _ => Err(GqlWireDecodeError::TypeMismatch("Record")),
    }
}

/// Wrap an IC Principal extension as a logical GQL value.
pub fn gql_principal_value(principal: GqlPrincipal) -> GqlValue {
    GqlValue::Extension(Box::new(principal))
}

/// Extract an IC Principal extension from a logical GQL value.
pub fn gql_principal_from_value(value: GqlValue) -> Result<GqlPrincipal, GqlWireDecodeError> {
    match value {
        GqlValue::Extension(value) => value
            .as_any()
            .downcast_ref::<GqlPrincipal>()
            .copied()
            .ok_or(GqlWireDecodeError::TypeMismatch("Principal")),
        _ => Err(GqlWireDecodeError::TypeMismatch("Principal")),
    }
}

/// Prepared-operation wire types shared with the Router.
pub use gleaph_prepared_api::{PreparedManifest, PreparedOperation, PreparedSortSpec};

/// Why production vector-index dispatch/backfill is fail-closed (ADR 0031 Slice 3/4).
///
/// Mirrors `gleaph_graph_kernel::federation::RouterError::VectorActivationBlockReason`;
/// variant order is part of the Candid wire contract.
#[derive(CandidType, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
pub enum VectorActivationBlockReason {
    /// Missing delete-spanning embedding incarnation fence (retained for wire stability).
    MissingEmbeddingIncarnationFence,
    /// The global vector-dispatch activation flag is off.
    DispatchNotActivated,
    /// Not every live shard of the graph is vector-attached yet.
    ShardsNotVectorAttached,
}

/// Error returned by a Router data-plane entrypoint.
///
/// Mirrors `gleaph_graph_kernel::federation::RouterError`; variant order and field names are
/// the Candid wire contract and must stay in lockstep. The CDK mirrors instead of depending on
/// `gleaph-graph-kernel` to keep the canister SDK dependency-light.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum RouterError {
    /// The caller is not authorized.
    NotAuthorized,
    /// The caller is forbidden.
    Forbidden,
    /// The named resource was not found.
    NotFound(String),
    /// The request conflicts with committed state.
    Conflict(String),
    /// Retryable command contention; the named durable operation must settle first.
    Busy { operation: String },
    /// The request is invalid.
    InvalidArgument(String),
    /// GQL program kind does not match the canister entrypoint (query vs update).
    ExecutionPathMismatch {
        entrypoint: String,
        program_kind: String,
        call_kind: String,
        remedy: String,
    },
    /// The graph is unavailable.
    GraphUnavailable,
    /// The API graph context does not match the GQL-resolved graph.
    GraphContextMismatch {
        api_graph: String,
        resolved_graph: String,
    },
    /// The shard is not registered.
    ShardNotRegistered,
    /// A read-your-writes read could not be served because a shard projection lags; retryable.
    ProjectionLag {
        shard_id: u32,
        watermark: String,
        required: u64,
        current: u64,
    },
    /// A federated update bundle contained more than one top-level DML statement.
    UnsupportedMultiDmlBundle {
        dml_statements: u32,
        shard_count: u32,
    },
    /// The shard is already registered.
    ShardAlreadyRegistered,
    /// An id is exhausted.
    IdExhausted(String),
    /// A uniqueness constraint would be violated (not retryable).
    UniquenessViolation(String),
    /// A uniqueness value is claimed by an in-flight mutation (retryable).
    UniquenessReservationInFlight(String),
    /// A recognized operation whose implementation is intentionally inactive.
    NotImplemented(String),
    /// Vector dispatch/backfill is blocked by the activation fence.
    VectorDispatchActivationBlocked(VectorActivationBlockReason),
    /// A Router -> Provision cross-canister call failed at the transport layer.
    ProvisionCallFailed(String),
    /// Candid encoding/decoding failed for a Router -> Provision call.
    ProvisionEncodingFailed(String),
    /// The Provision canister rejected the envelope because of a conflict.
    ProvisionConflict(String),
    /// The Provision canister rejected the envelope with a structured ingress error.
    ProvisionRejected(String),
    /// The requested deployment is not bound in the Provision canister.
    UnknownDeployment(String),
    /// The Provision canister sent an ack with a mismatched registry version.
    AckConflict { stored: u64 },
    /// The target record is not in the expected lifecycle phase.
    InvalidState(String),
    /// Internal error.
    Internal(String),
}

/// Maximum number of receipt rows returned by one bulk-load status page.
pub const MAX_BULK_LOAD_RECEIPTS_PER_PAGE: u32 = 64;

/// Router-owned projection of the Graph-specific durable receipts.
///
/// Mirrors `gleaph_bulk_load_api::AtomicInsertReceiptV1`. The CDK mirrors instead of depending
/// on `gleaph-bulk-load-api`, which pulls in `gleaph-graph-kernel`.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AtomicInsertReceiptV1 {
    pub logical_operation_count: u64,
    pub logical_vertex_count: u64,
    pub logical_edge_count: u64,
    /// Opaque graph-scoped encoded IDs in vertex-operation ordinal order.
    pub allocated_vertex_ids: Vec<Vec<u8>>,
}

/// One vertex operation in a bulk-load vertex chunk.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AtomicInsertVertexV1 {
    pub vertex_labels: Vec<String>,
    pub initial_properties: Vec<AtomicInsertPropertyV1>,
}

/// One named property in a vertex/edge initial property set.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct AtomicInsertPropertyV1 {
    pub property_name: String,
    /// Compact-binary GQL value bytes.
    pub value: Vec<u8>,
}

/// Public durable bulk-load command family (ADR 0057).
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BulkLoadCommand {
    Start {
        graph_name: Option<String>,
        client_bulk_key: String,
    },
    Append {
        graph_name: Option<String>,
        client_bulk_key: String,
        chunk_index: u32,
        chunk: BulkLoadChunkV1,
    },
    Finalize {
        graph_name: Option<String>,
        client_bulk_key: String,
    },
    Abort {
        graph_name: Option<String>,
        client_bulk_key: String,
    },
}

/// Self-contained vertex-only or existing-ID edge-only chunk.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BulkLoadChunkV1 {
    Vertices(Vec<AtomicInsertVertexV1>),
    Edges(Vec<BulkLoadEdgeV1>),
}

/// One edge in a durable bulk-load chunk.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkLoadEdgeV1 {
    pub source: BulkLoadEndpointV1,
    pub target: BulkLoadEndpointV1,
    pub directed: bool,
    pub edge_label_name: Option<String>,
    pub inline_property: Option<Vec<u8>>,
    pub initial_edge_properties: Vec<AtomicInsertPropertyV1>,
}

/// One edge endpoint in a durable bulk-load chunk.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BulkLoadEndpointV1 {
    /// Graph-scoped encoded existing vertex ID.
    Existing(Vec<u8>),
    /// Vertex referenced by label + property equality, resolved through the property index.
    ByProperty(BulkLoadPropertyEndpointV1),
}

/// Property-based vertex reference for a bulk-load edge endpoint.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkLoadPropertyEndpointV1 {
    pub vertex_label: String,
    pub property_name: String,
    /// Sortable index key bytes for the GQL value.
    pub value: Vec<u8>,
}

/// Response to one public bulk-load command.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BulkLoadResponse {
    Started {
        next_chunk_index: u32,
    },
    Appended {
        chunk_index: u32,
        next_offset: u32,
        receipt: AtomicInsertReceiptV1,
    },
    FinalizeAccepted {
        state: BulkLoadPublicStateV1,
    },
    AbortAccepted {
        state: BulkLoadPublicStateV1,
    },
}

/// Public projection of the durable bulk-load lifecycle.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum BulkLoadPublicStateV1 {
    Open,
    AppendPending,
    FinalizePending,
    AbortPending,
    Completed,
    Aborted,
    Failed { reason: String },
}

/// One committed chunk receipt in a status page.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkLoadChunkReceiptV1 {
    pub chunk_index: u32,
    pub receipt: AtomicInsertReceiptV1,
}

/// Bounded status response for one graph-scoped durable bulk-load job.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BulkLoadStatusPage {
    pub state: BulkLoadPublicStateV1,
    pub next_chunk_index: u32,
    pub committed_chunk_count: u32,
    pub completed_chunk_count: u32,
    pub terminal_at_ns: Option<u64>,
    pub expires_at_ns: Option<u64>,
    pub receipts: Vec<BulkLoadChunkReceiptV1>,
    pub next_receipt_cursor: Option<u32>,
}
