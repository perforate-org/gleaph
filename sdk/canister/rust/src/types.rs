//! Router-facing wire types, the query response envelope, read freshness contracts, and the
//! typed-binding helpers used by `gleaph-codegen` output.

use candid::{CandidType, Deserialize};

/// Decode error for the Router rows blob.
pub use gleaph_gql_ic_wire::GqlWireDecodeError;
use gleaph_gql_ic_wire::GqlWirePathElement;
/// One row in the wire rows blob.
pub use gleaph_gql_ic_wire::GqlWireRow;
/// Materialized rows returned in a Router response blob.
pub use gleaph_gql_ic_wire::GqlWireRows;
/// One wire value in a materialized row.
pub use gleaph_gql_ic_wire::GqlWireValue;
/// Principal extension used when projecting IC wire values into logical GQL values.
pub use gleaph_gql_ic_wire::PrincipalValue;
use gleaph_gql_ic_wire::decode_rows_blob;
use gleaph_gql_params::GqlParams;
use gleaph_gql_params::GqlPathElement;
/// Logical GQL value shared by dynamic GQL, prepared operations, and procedures.
pub use gleaph_gql_params::GqlValue;

/// Typed path element binding used by generated canister bindings.
///
/// Path element ids are fixed-length opaque bytes on the Router wire (vertex 8 bytes,
/// edge 12 bytes; ADR 0005), enforced by [`VertexPathElementId`] / [`EdgePathElementId`].
pub use gleaph_gql_params::{EdgePathElementId, PathElement, VertexPathElementId};

/// Upstream binary256 value used by generated canister bindings; serde is unavailable upstream,
/// so params use the raw primitive and rows decode through [`Float256`].
#[cfg(feature = "f256")]
pub use f256::f256 as GqlFloat256;
/// Binary128 row binding carrying the canonical little-endian wire form.
#[cfg(feature = "nightly-f128")]
pub use gleaph_gql_ic_wire::Float128;
/// Binary256 row binding carrying the canonical little-endian wire form.
#[cfg(feature = "f256")]
pub use gleaph_gql_ic_wire::Float256;
/// Decimal row binding used by generated canister bindings.
///
/// Serde and Candid use the textual form; the Router wire form is the packed 16-byte form
/// ([`GqlWireValue::Decimal`]).
#[cfg(feature = "decimal")]
pub use gleaph_gql_ic_wire::GqlDecimal;
/// Half-precision float row binding used by generated canister bindings.
///
/// Serde and Candid use the raw `u16` bit pattern ([`GqlWireValue::Float16`]).
#[cfg(feature = "f16")]
pub use gleaph_gql_ic_wire::GqlFloat16;
/// Signed 256-bit integer row binding used by generated canister bindings.
///
/// Serde and Candid use the decimal textual form; the Router wire form is 32 little-endian
/// bytes ([`GqlWireValue::Int256`]).
#[cfg(feature = "i256")]
pub use gleaph_gql_ic_wire::GqlInt256;
/// Unsigned 256-bit integer row binding used by generated canister bindings.
#[cfg(feature = "u256")]
pub use gleaph_gql_ic_wire::GqlUint256;

/// GQL `Date` row binding used by generated canister bindings.
///
/// Backed by `jiff::civil::Date` (default `temporal-jiff` feature) or `chrono::NaiveDate`
/// (`temporal-chrono`). Serde and Candid use the days-since-epoch wire form
/// ([`GqlWireValue::Date`]).
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlDate;
/// GQL `DateTime` row binding used by generated canister bindings.
///
/// Backed by `jiff::Timestamp` or `chrono::DateTime<Utc>`; serde and Candid use the
/// `{seconds, nanos}` wire form ([`GqlWireValue::DateTime`]).
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlDateTime;
/// GQL `Duration` row binding used by generated canister bindings.
///
/// Backed by `jiff::Span` (faithful, includes months) or `chrono::TimeDelta` (months are not
/// representable and are serialized as zero); serde and Candid use the `{months, nanos}` wire
/// form ([`GqlWireValue::Duration`]).
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlDuration;
/// GQL `LocalDateTime` row binding used by generated canister bindings.
///
/// Backed by `jiff::civil::DateTime` or `chrono::NaiveDateTime`, interpreted as a civil
/// date-time in UTC; serde and Candid use the `{seconds, nanos}` wire form.
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlLocalDateTime;
/// GQL `LocalTime` row binding used by generated canister bindings.
///
/// Same representation as [`GqlTime`] but projects to `Value::LocalTime`.
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlLocalTime;
/// GQL `Time` row binding used by generated canister bindings.
///
/// Backed by `jiff::civil::Time` or `chrono::NaiveTime`; serde and Candid use nanoseconds since
/// midnight ([`GqlWireValue::Time`]).
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlTime;
/// GQL `ZonedDateTime` row binding used by generated canister bindings.
///
/// Backed by `jiff::Zoned` or `chrono::DateTime<FixedOffset>`; serde and Candid use the
/// `{seconds, nanos, offset_seconds}` wire form ([`GqlWireValue::ZonedDateTime`]).
#[cfg(any(feature = "temporal-jiff", feature = "temporal-chrono"))]
pub use gleaph_gql_ic_wire::GqlZonedDateTime;
/// GQL `ZonedTime` row binding used by generated canister bindings.
///
/// Time of day with a fixed UTC offset; neither `jiff` nor `chrono` models this, so the binding
/// keeps the `{nanos, offset_seconds}` wire record form.
pub use gleaph_gql_ic_wire::GqlZonedTime;

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
    /// This is the path used by `gleaph-codegen`'s `PreparedExt` operations: each wire value is
    /// projected to JSON (see [`gql_wire_value_to_json`]) and deserialized with the row type's
    /// serde derives and renames.
    pub fn decode_serde_rows<Row>(&self) -> Result<Option<Vec<Row>>, GqlWireDecodeError>
    where
        Row: serde::de::DeserializeOwned,
    {
        self.decode_rows()?
            .map(|rows| {
                rows.rows
                    .into_iter()
                    .map(|row| {
                        let mut object = serde_json::Map::new();
                        for (name, value) in row.columns {
                            object.insert(name, gql_wire_value_to_json(value)?);
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
        GqlValue::Int128(value) => serde_json::json!(value),
        GqlValue::Int256(value) => serde_json::Value::String(value.to_string()),
        GqlValue::Uint8(value) => serde_json::json!(value),
        GqlValue::Uint16(value) => serde_json::json!(value),
        GqlValue::Uint32(value) => serde_json::json!(value),
        GqlValue::Uint64(value) => serde_json::json!(value),
        GqlValue::Uint128(value) => serde_json::json!(value),
        GqlValue::Uint256(value) => serde_json::Value::String(value.to_string()),
        GqlValue::Float16(value) => serde_json::json!(value.to_bits()),
        GqlValue::Float32(value) => serde_json::to_value(value)
            .map_err(|error| GqlWireDecodeError::Json(error.to_string()))?,
        GqlValue::Float64(value) => serde_json::to_value(value)
            .map_err(|error| GqlWireDecodeError::Json(error.to_string()))?,
        #[cfg(feature = "nightly-f128")]
        GqlValue::Float128(value) => serde_json::json!(value.to_bits().to_le_bytes().to_vec()),
        #[cfg(feature = "f256")]
        GqlValue::Float256(value) => serde_json::json!(value.to_le_bytes().to_vec()),
        #[cfg(feature = "decimal")]
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
            .downcast_ref::<PrincipalValue>()
            .map(|principal| serde_json::Value::String(principal.to_string()))
            .ok_or(GqlWireDecodeError::UnsupportedValue("extension"))?,
        #[cfg(any(
            not(feature = "nightly-f128"),
            not(feature = "f256"),
            not(feature = "decimal")
        ))]
        _ => {
            return Err(GqlWireDecodeError::UnsupportedValue(
                "Float128 or Float256 or Decimal",
            ));
        }
    };
    Ok(json)
}

/// Project one logical GQL record into the generated open-record map shape.
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

/// Project one wire value into the JSON representation used by generated row types.
///
/// The projection mirrors the wire representations exactly (big integers and decimals as decimal
/// strings, floats as bit patterns or little-endian bytes, principals as text), so generated row
/// fields deserialize without lossy conversion. It is the row-decode counterpart of
/// [`gql_value_to_json`], which projects the logical value model instead.
pub fn gql_wire_value_to_json(
    value: GqlWireValue,
) -> Result<serde_json::Value, GqlWireDecodeError> {
    let json = match value {
        GqlWireValue::Null => serde_json::Value::Null,
        GqlWireValue::Bool(value) => serde_json::Value::Bool(value),
        GqlWireValue::Int8(value) => serde_json::json!(value),
        GqlWireValue::Int16(value) => serde_json::json!(value),
        GqlWireValue::Int32(value) => serde_json::json!(value),
        GqlWireValue::Int64(value) => serde_json::json!(value),
        GqlWireValue::Int128(value) => serde_json::json!(value),
        #[cfg(feature = "i256")]
        GqlWireValue::Int256(value) => {
            let bytes = <[u8; 32]>::try_from(value.as_slice())
                .map_err(|_| GqlWireDecodeError::InvalidNumeric("Int256"))?;
            serde_json::Value::String(ethnum::I256::from_le_bytes(bytes).to_string())
        }
        #[cfg(not(feature = "i256"))]
        GqlWireValue::Int256(_) => {
            return Err(GqlWireDecodeError::UnsupportedValue("Int256"));
        }
        GqlWireValue::Uint8(value) => serde_json::json!(value),
        GqlWireValue::Uint16(value) => serde_json::json!(value),
        GqlWireValue::Uint32(value) => serde_json::json!(value),
        GqlWireValue::Uint64(value) => serde_json::json!(value),
        GqlWireValue::Uint128(value) => serde_json::json!(value),
        #[cfg(feature = "u256")]
        GqlWireValue::Uint256(value) => {
            let bytes = <[u8; 32]>::try_from(value.as_slice())
                .map_err(|_| GqlWireDecodeError::InvalidNumeric("Uint256"))?;
            serde_json::Value::String(ethnum::U256::from_le_bytes(bytes).to_string())
        }
        #[cfg(not(feature = "u256"))]
        GqlWireValue::Uint256(_) => {
            return Err(GqlWireDecodeError::UnsupportedValue("Uint256"));
        }
        GqlWireValue::Float16(value) => serde_json::json!(value),
        GqlWireValue::Float32(value) => serde_json::to_value(value)
            .map_err(|error| GqlWireDecodeError::Json(error.to_string()))?,
        GqlWireValue::Float64(value) => serde_json::to_value(value)
            .map_err(|error| GqlWireDecodeError::Json(error.to_string()))?,
        GqlWireValue::Float128(value) => serde_json::json!(value),
        GqlWireValue::Float256(value) => serde_json::json!(value),
        #[cfg(feature = "decimal")]
        GqlWireValue::Decimal(value) => {
            let bytes = <[u8; 16]>::try_from(value.as_slice())
                .map_err(|_| GqlWireDecodeError::InvalidNumeric("Decimal"))?;
            serde_json::Value::String(rust_decimal::Decimal::deserialize(bytes).to_string())
        }
        #[cfg(not(feature = "decimal"))]
        GqlWireValue::Decimal(_) => {
            return Err(GqlWireDecodeError::UnsupportedValue("Decimal"));
        }
        GqlWireValue::Text(value) => serde_json::Value::String(value),
        GqlWireValue::Bytes(value) => serde_json::Value::Array(
            value
                .into_iter()
                .map(|value| serde_json::json!(value))
                .collect(),
        ),
        GqlWireValue::Date(value) => serde_json::json!(value),
        GqlWireValue::Time(value) => serde_json::json!(value),
        GqlWireValue::LocalTime(value) => serde_json::json!(value),
        GqlWireValue::DateTime { seconds, nanos } => {
            serde_json::json!({ "seconds": seconds, "nanos": nanos })
        }
        GqlWireValue::LocalDateTime { seconds, nanos } => {
            serde_json::json!({ "seconds": seconds, "nanos": nanos })
        }
        GqlWireValue::ZonedDateTime {
            seconds,
            nanos,
            offset_seconds,
        } => serde_json::json!({
            "seconds": seconds,
            "nanos": nanos,
            "offset_seconds": offset_seconds
        }),
        GqlWireValue::ZonedTime {
            nanos,
            offset_seconds,
        } => serde_json::json!({ "nanos": nanos, "offset_seconds": offset_seconds }),
        GqlWireValue::Duration { months, nanos } => {
            serde_json::json!({ "months": months, "nanos": nanos })
        }
        GqlWireValue::Principal(principal) => serde_json::Value::String(principal.to_text()),
        GqlWireValue::ExtensionLeaf { .. } => {
            return Err(GqlWireDecodeError::UnsupportedValue("ExtensionLeaf"));
        }
        GqlWireValue::ValueBinary(bytes) => {
            let value = GqlValue::from_binary_bytes(&bytes)
                .map_err(|_| GqlWireDecodeError::UnsupportedValue("ValueBinary"))?;
            return gql_value_to_json(value);
        }
        GqlWireValue::List(values) => serde_json::Value::Array(
            values
                .into_iter()
                .map(gql_wire_value_to_json)
                .collect::<Result<_, _>>()?,
        ),
        GqlWireValue::Path(elements) => serde_json::Value::Array(
            elements
                .into_iter()
                .map(|element| match element {
                    GqlWirePathElement::Vertex(id) => serde_json::json!({ "Vertex": id }),
                    GqlWirePathElement::Edge(id) => serde_json::json!({ "Edge": id }),
                })
                .collect(),
        ),
        GqlWireValue::Record(fields) => {
            let mut object = serde_json::Map::new();
            for (name, value) in fields {
                if object.contains_key(&name) {
                    return Err(GqlWireDecodeError::DuplicateField(name));
                }
                object.insert(name, gql_wire_value_to_json(value)?);
            }
            serde_json::Value::Object(object)
        }
    };
    Ok(json)
}

/// Extract an IC Principal extension from a logical GQL value.
pub fn gql_principal_from_value(value: GqlValue) -> Result<PrincipalValue, GqlWireDecodeError> {
    match value {
        GqlValue::Extension(value) => value
            .as_any()
            .downcast_ref::<PrincipalValue>()
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
