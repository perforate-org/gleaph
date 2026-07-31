//! Gleaph canister SDK.
//!
//! Small, opinionated helpers for application canisters that delegate fixed read scenarios to the
//! Gleaph Router via its `prepared_query` interface. The API is intentionally generic: it
//! knows the Candid shape of a prepared query call `(String name, Vec<u8> params)` and how to make
//! a bounded-wait inter-canister call, but it does not know application-specific scenario names or
//! semantics.

#![cfg_attr(feature = "nightly-f128", feature(f128))]

use candid::{CandidType, Deserialize, Principal};

/// Serde facade used by generated canister bindings.
pub use serde;

/// JSON facade used by generated canister bindings for open record values.
pub use serde_json;

/// Logical GQL value shared by dynamic GQL, prepared operations, and procedures.
pub use gleaph_gql::Value as GqlValue;
pub use gleaph_gql_ic_wire::GqlPrincipal;

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

/// Rust signed binary256 integer used by generated canister bindings.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct GqlInt256(gleaph_gql::types::Int256);

impl GqlInt256 {
    /// Construct from the shared GQL integer wrapper.
    pub const fn from_inner(value: gleaph_gql::types::Int256) -> Self {
        Self(value)
    }

    /// Return the shared GQL integer wrapper.
    pub const fn into_inner(self) -> gleaph_gql::types::Int256 {
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
            .parse::<gleaph_gql::types::Int256>()
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
pub struct GqlUint256(gleaph_gql::types::Uint256);

impl GqlUint256 {
    /// Construct from the shared GQL integer wrapper.
    pub const fn from_inner(value: gleaph_gql::types::Uint256) -> Self {
        Self(value)
    }

    /// Return the shared GQL integer wrapper.
    pub const fn into_inner(self) -> gleaph_gql::types::Uint256 {
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
            .parse::<gleaph_gql::types::Uint256>()
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
pub struct GqlDecimal(gleaph_gql::types::Decimal);

impl GqlDecimal {
    /// Construct from the shared GQL decimal wrapper.
    pub const fn from_inner(value: gleaph_gql::types::Decimal) -> Self {
        Self(value)
    }

    /// Return the shared GQL decimal wrapper.
    pub const fn into_inner(self) -> gleaph_gql::types::Decimal {
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
        gleaph_gql::types::Decimal::parse(value)
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

/// Ordered GQL record representation.
///
/// GQL record order is retained because the compact wire representation preserves field order.
pub type GqlRecord = Vec<(String, GqlValue)>;

/// Named GQL parameters, usable by dynamic GQL and prepared operations.
pub type GqlParams = GqlRecord;

/// One GQL result row.
pub type GqlRow = GqlRecord;

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
// The workspace can unify `gleaph-gql/f128` through another dependent crate even when this SDK's
// `nightly-f128` feature is disabled, while a standalone SDK build has no Float128 variant.
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

/// Path element in a logical GQL value.
pub use gleaph_gql::types::PathElement as GqlPathElement;
pub use gleaph_gql_ic_wire::{
    GqlWireDecodeError, GqlWirePathElement, GqlWireRow, GqlWireRows, GqlWireValue, decode_rows_blob,
};

/// Error returned when logical GQL parameters cannot be compact-binary encoded.
pub type GqlEncodingError = gleaph_gql::ValueBinaryError;

/// Encode ordered named GQL parameters for a dynamic GQL or prepared-query call.
pub fn encode_gql_params(params: GqlParams) -> Result<Vec<u8>, GqlEncodingError> {
    GqlValue::Record(params).to_binary_bytes()
}

/// Router query response envelope shared by dynamic GQL and prepared operations.
///
/// The materialized rows remain an opaque compact Candid blob until the public IC wire codec is
/// selected. This keeps the CDK API independent from graph-kernel implementation types.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct GqlResponse {
    /// Number of rows returned by the Router.
    pub row_count: u64,
    /// Optional compact Candid rows blob.
    pub rows_blob: Option<Vec<u8>>,
}

impl GqlResponse {
    /// Return the materialized rows blob, if the query produced one.
    pub fn rows_blob(&self) -> Option<&[u8]> {
        self.rows_blob.as_deref()
    }

    /// Decode materialized rows using the lightweight public IC wire codec.
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
}

/// Error returned when a prepared-query inter-canister call fails before yielding a typed result.
#[derive(CandidType, Deserialize, Clone, Debug, PartialEq, Eq)]
pub enum PreparedCallError {
    /// The IC rejected the call (timeout, canister error, destination invalid, etc.).
    Reject {
        /// Reject code as a human-readable string. This is the textual form of
        /// [`ic_cdk::call::RejectCode`] when available, or the raw numeric code otherwise.
        code: String,
        /// Reject or transport-level error message.
        message: String,
    },
    /// The call succeeded at the transport layer but the response could not be Candid-decoded as `R`.
    Decode {
        /// Candid decode error message.
        message: String,
    },
}

impl core::fmt::Display for PreparedCallError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            PreparedCallError::Reject { code, message } => {
                write!(f, "prepared query rejected ({code}): {message}")
            }
            PreparedCallError::Decode { message } => {
                write!(f, "failed to decode prepared query response: {message}")
            }
        }
    }
}

/// Candid-encode the `(String, Vec<u8>)` argument tuple used by Router prepared queries.
///
/// # Panics
///
/// Panics only if the inputs somehow fail to encode, which cannot happen for the `(String, Vec<u8>)`
/// tuple under normal Candid operation.
pub fn encode_prepared_query_args(name: impl Into<String>, params: Vec<u8>) -> Vec<u8> {
    candid::utils::encode_args((name.into(), params)).expect("Candid encode (String, Vec<u8>)")
}

/// Make a bounded-wait call to `prepared_query` on `canister_id`.
///
/// `name` is the registered prepared-query name; `params` is the compact-binary GQL parameter blob
/// produced by the caller (often via `gleaph-gql-ic`). On success the Router's return value is
/// Candid-decoded into `R`, which is typically `Result<T, RouterError>`.
pub async fn call_prepared_query<R>(
    canister_id: Principal,
    name: impl Into<String>,
    params: Vec<u8>,
) -> Result<R, PreparedCallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    let args = encode_prepared_query_args(name, params);
    call_prepared_method(canister_id, "prepared_query", args).await
}

/// Make a bounded-wait call to the Router's dynamic `gql_query` endpoint.
pub async fn call_gql_query<R>(
    canister_id: Principal,
    query: impl Into<String>,
    params: GqlParams,
) -> Result<R, PreparedCallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    let params = encode_gql_params(params).map_err(|error| PreparedCallError::Decode {
        message: format!("failed to encode GQL params: {error:?}"),
    })?;
    let args = candid::utils::encode_args((query.into(), params))
        .expect("Candid encode GQL query arguments");
    call_prepared_method(canister_id, "gql_query", args).await
}

/// Make a bounded-wait call to `prepared_update` on `canister_id`.
///
/// The returned type is normally `u64` for the current Router endpoint. The generic form keeps
/// the helper usable with a future result-wire contract without duplicating call error handling.
pub async fn call_prepared_update<R>(
    canister_id: Principal,
    name: impl Into<String>,
    params: Vec<u8>,
) -> Result<R, PreparedCallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    let args = encode_prepared_query_args(name, params);
    call_prepared_method(canister_id, "prepared_update", args).await
}

/// Make a bounded-wait call to `prepared_update_idempotent` on `canister_id`.
pub async fn call_prepared_update_idempotent<R>(
    canister_id: Principal,
    name: impl Into<String>,
    params: Vec<u8>,
    client_mutation_key: impl Into<String>,
) -> Result<R, PreparedCallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    let args = candid::utils::encode_args((name.into(), params, client_mutation_key.into()))
        .expect("Candid encode prepared idempotent update arguments");
    call_prepared_method(canister_id, "prepared_update_idempotent", args).await
}

async fn call_prepared_method<R>(
    canister_id: Principal,
    method: &str,
    args: Vec<u8>,
) -> Result<R, PreparedCallError>
where
    R: CandidType + for<'de> Deserialize<'de>,
{
    use ic_cdk::call::{CallFailed, Response};

    let call_result: Result<Response, CallFailed> =
        ic_cdk::call::Call::bounded_wait(canister_id, method)
            .with_raw_args(&args)
            .await;

    match call_result {
        Ok(response) => response.candid().map_err(|e| PreparedCallError::Decode {
            message: e.to_string(),
        }),
        Err(CallFailed::CallRejected(rejected)) => Err(PreparedCallError::Reject {
            code: rejected
                .reject_code()
                .map(|code| format!("{code:?}"))
                .unwrap_or_else(|_| rejected.raw_reject_code().to_string()),
            message: rejected.reject_message().to_string(),
        }),
        Err(other) => Err(PreparedCallError::Reject {
            code: "CallFailed".to_string(),
            message: other.to_string(),
        }),
    }
}

/// Canister-id-bound client for dynamic GQL and prepared operations.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GleaphClient {
    canister_id: Principal,
}

impl GleaphClient {
    /// Bind the client to a Router canister.
    pub const fn new(canister_id: Principal) -> Self {
        Self { canister_id }
    }

    /// Execute a named prepared query through the configured Router canister.
    pub async fn execute<R>(
        &self,
        name: impl Into<String>,
        params: Vec<u8>,
    ) -> Result<R, PreparedCallError>
    where
        R: CandidType + for<'de> Deserialize<'de>,
    {
        call_prepared_query(self.canister_id, name, params).await
    }

    /// Execute a named prepared query from logical GQL parameters.
    pub async fn execute_gql_params<R>(
        &self,
        name: impl Into<String>,
        params: GqlParams,
    ) -> Result<R, PreparedCallError>
    where
        R: CandidType + for<'de> Deserialize<'de>,
    {
        let params = encode_gql_params(params).map_err(|error| PreparedCallError::Decode {
            message: format!("failed to encode GQL params: {error:?}"),
        })?;
        call_prepared_query(self.canister_id, name, params).await
    }

    /// Execute dynamic GQL through the configured Router canister.
    pub async fn gql_query<R>(
        &self,
        query: impl Into<String>,
        params: GqlParams,
    ) -> Result<R, PreparedCallError>
    where
        R: CandidType + for<'de> Deserialize<'de>,
    {
        call_gql_query(self.canister_id, query, params).await
    }

    /// Execute a named prepared update through the configured Router canister.
    pub async fn execute_update<R>(
        &self,
        name: impl Into<String>,
        params: Vec<u8>,
    ) -> Result<R, PreparedCallError>
    where
        R: CandidType + for<'de> Deserialize<'de>,
    {
        call_prepared_update(self.canister_id, name, params).await
    }

    /// Execute an idempotent named prepared update through the configured Router canister.
    pub async fn execute_update_idempotent<R>(
        &self,
        name: impl Into<String>,
        params: Vec<u8>,
        client_mutation_key: impl Into<String>,
    ) -> Result<R, PreparedCallError>
    where
        R: CandidType + for<'de> Deserialize<'de>,
    {
        call_prepared_update_idempotent(self.canister_id, name, params, client_mutation_key).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_prepared_query_args_round_trips() {
        let name = "alice_home_feed";
        let params: Vec<u8> = vec![0, 1, 2, 255];
        let encoded = encode_prepared_query_args(name, params.clone());

        let (decoded_name, decoded_params): (String, Vec<u8>) =
            candid::utils::decode_args(&encoded).expect("decode args");

        assert_eq!(decoded_name, name);
        assert_eq!(decoded_params, params);
    }

    #[test]
    fn prepared_call_error_display_contains_context() {
        let err = PreparedCallError::Reject {
            code: "CanisterReject".to_string(),
            message: "no query".to_string(),
        };
        let text = format!("{err}");
        assert!(text.contains("no query"), "{text}");
        assert!(text.contains("CanisterReject"), "{text}");
    }

    #[test]
    fn encode_prepared_idempotent_update_args_round_trips() {
        let encoded = candid::utils::encode_args((
            "increment".to_string(),
            vec![1_u8, 2, 3],
            "request-1".to_string(),
        ))
        .expect("encode args");
        let decoded: (String, Vec<u8>, String) =
            candid::utils::decode_args(&encoded).expect("decode args");
        assert_eq!(
            decoded,
            ("increment".into(), vec![1, 2, 3], "request-1".into())
        );
    }

    #[test]
    fn logical_gql_params_round_trip_preserves_order() {
        let params = vec![
            ("first".to_string(), GqlValue::Int8(1)),
            ("second".to_string(), GqlValue::Text("two".to_string())),
        ];
        let encoded = encode_gql_params(params.clone()).expect("encode GQL params");
        let decoded = GqlValue::from_binary_bytes(&encoded).expect("decode GQL params");
        assert_eq!(decoded, GqlValue::Record(params));
    }

    #[test]
    fn gql_response_envelope_round_trips() {
        let response = GqlResponse {
            row_count: 2,
            rows_blob: Some(vec![1, 2, 3]),
        };
        let bytes = candid::encode_one(&response).expect("encode response");
        let decoded: GqlResponse = candid::decode_one(&bytes).expect("decode response");
        assert_eq!(decoded, response);
        assert_eq!(decoded.rows_blob(), Some(&[1, 2, 3][..]));
    }

    #[test]
    fn gql_response_decodes_wire_rows() {
        let rows = GqlWireRows {
            rows: vec![GqlWireRow {
                columns: vec![("name".into(), GqlWireValue::Text("Ada".into()))],
            }],
        };
        let response = GqlResponse {
            row_count: 1,
            rows_blob: Some(candid::encode_one(&rows).expect("encode rows")),
        };
        assert_eq!(response.decode_rows().unwrap(), Some(rows));
    }

    #[derive(Debug, PartialEq)]
    struct TypedRow {
        name: String,
    }

    impl FromGqlRow for TypedRow {
        fn from_gql_row(mut row: GqlRow) -> Result<Self, GqlWireDecodeError> {
            let value = take_gql_row_field(&mut row, "name")?;
            match value {
                GqlValue::Text(name) => Ok(Self { name }),
                _ => Err(GqlWireDecodeError::TypeMismatch("Text")),
            }
        }
    }

    #[test]
    fn gql_response_decodes_typed_rows() {
        let rows = GqlWireRows {
            rows: vec![GqlWireRow {
                columns: vec![("name".into(), GqlWireValue::Text("Ada".into()))],
            }],
        };
        let response = GqlResponse {
            row_count: 1,
            rows_blob: Some(candid::encode_one(&rows).expect("encode rows")),
        };
        assert_eq!(
            response.decode_typed_rows::<TypedRow>().unwrap(),
            Some(vec![TypedRow { name: "Ada".into() }])
        );
    }

    #[test]
    fn duplicate_typed_row_fields_are_rejected() {
        let mut row = vec![
            ("name".into(), GqlValue::Text("Ada".into())),
            ("name".into(), GqlValue::Text("Grace".into())),
        ];
        assert_eq!(
            take_gql_row_field(&mut row, "name"),
            Err(GqlWireDecodeError::DuplicateField("name".into()))
        );
    }

    #[test]
    fn logical_gql_records_project_to_open_json_maps() {
        let value = GqlValue::Record(vec![
            ("name".into(), GqlValue::Text("Ada".into())),
            ("active".into(), GqlValue::Bool(true)),
            (
                "nested".into(),
                GqlValue::List(vec![GqlValue::Int8(1), GqlValue::Int8(2)]),
            ),
            ("bytes".into(), GqlValue::Bytes(vec![1, 2, 3])),
        ]);

        let projected = gql_record_to_json_map(value).expect("project record");
        assert_eq!(projected["name"], serde_json::json!("Ada"));
        assert_eq!(projected["active"], serde_json::json!(true));
        assert_eq!(projected["nested"], serde_json::json!([1, 2]));
        assert_eq!(projected["bytes"], serde_json::json!([1, 2, 3]));
    }

    #[test]
    fn logical_gql_records_reject_duplicate_fields() {
        let value = GqlValue::Record(vec![
            ("name".into(), GqlValue::Text("Ada".into())),
            ("name".into(), GqlValue::Text("Grace".into())),
        ]);

        assert_eq!(
            gql_record_to_json_map(value),
            Err(GqlWireDecodeError::DuplicateField("name".into()))
        );
    }

    #[derive(serde::Deserialize, serde::Serialize)]
    struct StableFloatSerdeFixture {
        float256: GqlFloat256,
        #[cfg(feature = "nightly-f128")]
        float128: GqlFloat128,
    }

    #[test]
    fn shared_wide_float_types_support_serde_derives() {
        fn assert_serde<T: serde::Serialize + for<'de> serde::Deserialize<'de>>() {}

        assert_serde::<StableFloatSerdeFixture>();
    }

    #[test]
    fn float256_serde_uses_exact_little_endian_bytes() {
        let bytes = [0xabu8; 32];
        let value = GqlFloat256::from_le_bytes(bytes);
        let encoded = serde_json::to_vec(&value).expect("serialize float256");
        let decoded: GqlFloat256 = serde_json::from_slice(&encoded).expect("deserialize float256");
        assert_eq!(decoded.to_le_bytes(), bytes);
    }

    #[test]
    fn float16_serde_uses_exact_bit_pattern() {
        let value = GqlFloat16::from_bits(0x3c00);
        let encoded = serde_json::to_vec(&value).expect("serialize float16");
        let decoded: GqlFloat16 = serde_json::from_slice(&encoded).expect("deserialize float16");
        assert_eq!(decoded.to_bits(), 0x3c00);
    }

    #[test]
    fn exact_integer_and_decimal_wrappers_round_trip_through_serde() {
        let int256: GqlInt256 = "-123456789012345678901234567890".parse().unwrap();
        let uint256: GqlUint256 = "123456789012345678901234567890".parse().unwrap();
        let decimal: GqlDecimal = "123.4500".parse().unwrap();

        let int256_json = serde_json::to_string(&int256).unwrap();
        let uint256_json = serde_json::to_string(&uint256).unwrap();
        let decimal_json = serde_json::to_string(&decimal).unwrap();

        assert_eq!(
            serde_json::from_str::<GqlInt256>(&int256_json).unwrap(),
            int256
        );
        assert_eq!(
            serde_json::from_str::<GqlUint256>(&uint256_json).unwrap(),
            uint256
        );
        assert_eq!(
            serde_json::from_str::<GqlDecimal>(&decimal_json).unwrap(),
            decimal
        );
        assert_eq!(int256.to_string(), "-123456789012345678901234567890");
        assert_eq!(uint256.to_string(), "123456789012345678901234567890");
        assert_eq!(decimal.to_string(), "123.4500");
    }

    #[test]
    fn principal_value_uses_the_shared_extension() {
        let principal = GqlPrincipal::from_inner(Principal::anonymous());
        assert!(matches!(
            gql_principal_value(principal),
            GqlValue::Extension(_)
        ));
        let encoded = serde_json::to_string(&principal).unwrap();
        assert_eq!(
            serde_json::from_str::<GqlPrincipal>(&encoded).unwrap(),
            principal
        );
    }

    #[cfg(feature = "nightly-f128")]
    #[test]
    fn float128_serde_uses_exact_little_endian_bytes() {
        let bytes = [0xabu8; 16];
        let value = GqlFloat128::from_le_bytes(bytes);
        let encoded = serde_json::to_vec(&value).expect("serialize float128");
        let decoded: GqlFloat128 = serde_json::from_slice(&encoded).expect("deserialize float128");
        assert_eq!(decoded.to_le_bytes(), bytes);
    }
}
