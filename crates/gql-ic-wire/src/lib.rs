//! Lightweight Candid wire types for GQL query results.
//!
//! This crate intentionally contains no graph-kernel or planner dependency. It is suitable for
//! SDK and canister runtimes that need to decode Router response blobs.

#![cfg_attr(feature = "nightly-f128", feature(f128))]

use std::any::Any;
use std::borrow::Cow;
use std::fmt;

use candid::{CandidType, Principal};
use ethnum::{I256, U256};
use gleaph_gql::{ExtensionValue, Value};
use serde::{Deserialize, Serialize};

#[cfg(feature = "chrono")]
mod temporal_chrono;
#[cfg(all(feature = "jiff", not(feature = "chrono")))]
mod temporal_jiff;

/// A path element in a GQL result value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum GqlWirePathElement {
    /// Vertex identifier.
    Vertex(Vec<u8>),
    /// Edge identifier.
    Edge(Vec<u8>),
}

/// A scalar or composite value in the Candid GQL result wire format.
///
/// Exotic scalar values use opaque byte representations (big integers and decimals as
/// little-endian packed bytes, floats as bit patterns or little-endian bytes) so the wire
/// format stays independent of upstream numeric crates. Construct values through the
/// [`From`] impls for the upstream types where one exists.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub enum GqlWireValue {
    Null,
    Bool(bool),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Int128(i128),
    Int256(Vec<u8>),
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uint64(u64),
    Uint128(u128),
    Uint256(Vec<u8>),
    Float16(u16),
    Float32(f32),
    Float64(f64),
    Float128(Vec<u8>),
    Float256(Vec<u8>),
    Decimal(Vec<u8>),
    Text(String),
    Bytes(Vec<u8>),
    Date(i32),
    Time(u64),
    LocalTime(u64),
    DateTime {
        seconds: i64,
        nanos: u32,
    },
    LocalDateTime {
        seconds: i64,
        nanos: u32,
    },
    ZonedDateTime {
        seconds: i64,
        nanos: u32,
        offset_seconds: i32,
    },
    ZonedTime {
        nanos: u64,
        offset_seconds: i32,
    },
    Duration {
        months: i32,
        nanos: i64,
    },
    Principal(Vec<u8>),
    ExtensionLeaf {
        type_name: String,
        payload: Vec<u8>,
    },
    ValueBinary(Vec<u8>),
    List(Vec<GqlWireValue>),
    Path(Vec<GqlWirePathElement>),
    Record(Vec<(String, GqlWireValue)>),
}

/// Principal extension used when projecting IC wire values into logical GQL values.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlPrincipal(pub Principal);

impl GqlPrincipal {
    /// Construct from an Internet Computer principal.
    pub const fn from_inner(principal: Principal) -> Self {
        Self(principal)
    }

    /// Return the wrapped Internet Computer principal.
    pub const fn into_inner(self) -> Principal {
        self.0
    }
}

impl Serialize for GqlPrincipal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_str(&self.0.to_text())
    }
}

impl<'de> Deserialize<'de> for GqlPrincipal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        Principal::from_text(String::deserialize(deserializer)?)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

impl fmt::Display for GqlPrincipal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

impl ExtensionValue for GqlPrincipal {
    fn type_name(&self) -> &str {
        "PRINCIPAL"
    }

    fn clone_box(&self) -> Box<dyn ExtensionValue> {
        Box::new(*self)
    }

    fn eq_ext(&self, other: &dyn ExtensionValue) -> bool {
        other
            .as_any()
            .downcast_ref::<Self>()
            .is_some_and(|other| self == other)
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn short_blob(&self) -> Option<Cow<'_, [u8]>> {
        Some(Cow::Borrowed(self.0.as_slice()))
    }
}

/// One result row as ordered column/value pairs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, CandidType)]
pub struct GqlWireRow {
    /// Columns in wire order.
    pub columns: Vec<(String, GqlWireValue)>,
}

/// Materialized rows returned in a Router response blob.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, CandidType)]
pub struct GqlWireRows {
    /// Rows in result order.
    pub rows: Vec<GqlWireRow>,
}

/// Rust binary128 value used by generated canister bindings.
///
/// The serde representation is the canonical little-endian 16-byte wire form; the upstream
/// `f128` primitive has no serde support, so this wrapper carries it for row decoding.
#[cfg(feature = "nightly-f128")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Float128(f128);

#[cfg(feature = "nightly-f128")]
impl Float128 {
    /// Construct from the canonical little-endian wire representation.
    pub const fn from_le_bytes(bytes: [u8; 16]) -> Self {
        Self(f128::from_bits(u128::from_le_bytes(bytes)))
    }

    /// Return the canonical little-endian wire representation.
    pub const fn to_le_bytes(self) -> [u8; 16] {
        self.0.to_bits().to_le_bytes()
    }

    /// Return the upstream primitive value.
    pub const fn into_inner(self) -> f128 {
        self.0
    }
}

#[cfg(feature = "nightly-f128")]
impl Serialize for Float128 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.to_le_bytes())
    }
}

#[cfg(feature = "nightly-f128")]
impl<'de> Deserialize<'de> for Float128 {
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
/// The serde representation is the canonical little-endian 32-byte wire form; the upstream
/// `f256` type intentionally provides no serde support, so this wrapper carries it for row
/// decoding.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Float256(f256::f256);

impl Float256 {
    /// Construct from the canonical little-endian wire representation.
    pub const fn from_le_bytes(bytes: [u8; 32]) -> Self {
        Self(f256::f256::from_le_bytes(bytes))
    }

    /// Return the canonical little-endian wire representation.
    pub const fn to_le_bytes(self) -> [u8; 32] {
        self.0.to_le_bytes()
    }

    /// Return the upstream numeric value.
    pub const fn into_inner(self) -> f256::f256 {
        self.0
    }
}

impl Serialize for Float256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.to_le_bytes())
    }
}

impl<'de> Deserialize<'de> for Float256 {
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

#[cfg(feature = "nightly-f128")]
impl CandidType for Float128 {
    fn _ty() -> candid::types::Type {
        candid::types::Type::from(candid::types::TypeInner::Vec(
            candid::types::TypeInner::Nat8.into(),
        ))
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        serializer.serialize_blob(&self.to_le_bytes())
    }
}

impl CandidType for Float256 {
    fn _ty() -> candid::types::Type {
        candid::types::Type::from(candid::types::TypeInner::Vec(
            candid::types::TypeInner::Nat8.into(),
        ))
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        serializer.serialize_blob(&self.to_le_bytes())
    }
}

/// Signed 256-bit integer row binding used by generated canister bindings.
///
/// Serde and Candid both use the decimal textual form (matching the JSON row projection); the
/// Router wire form is 32 little-endian bytes ([`GqlWireValue::Int256`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlInt256(I256);

impl GqlInt256 {
    /// Wrap an upstream 256-bit signed integer.
    pub const fn from_inner(value: I256) -> Self {
        Self(value)
    }

    /// Return the wrapped upstream 256-bit signed integer.
    pub const fn into_inner(self) -> I256 {
        self.0
    }
}

impl From<I256> for GqlInt256 {
    fn from(value: I256) -> Self {
        Self(value)
    }
}

impl From<GqlInt256> for I256 {
    fn from(value: GqlInt256) -> Self {
        value.0
    }
}

impl Serialize for GqlInt256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ethnum::serde::decimal::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for GqlInt256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        ethnum::serde::decimal::deserialize(deserializer).map(Self)
    }
}

impl CandidType for GqlInt256 {
    fn _ty() -> candid::types::Type {
        candid::types::TypeInner::Text.into()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        serializer.serialize_text(&self.0.to_string())
    }
}

/// Unsigned 256-bit integer row binding used by generated canister bindings.
///
/// Serde and Candid both use the decimal textual form (matching the JSON row projection); the
/// Router wire form is 32 little-endian bytes ([`GqlWireValue::Uint256`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlUint256(U256);

impl GqlUint256 {
    /// Wrap an upstream 256-bit unsigned integer.
    pub const fn from_inner(value: U256) -> Self {
        Self(value)
    }

    /// Return the wrapped upstream 256-bit unsigned integer.
    pub const fn into_inner(self) -> U256 {
        self.0
    }
}

impl From<U256> for GqlUint256 {
    fn from(value: U256) -> Self {
        Self(value)
    }
}

impl From<GqlUint256> for U256 {
    fn from(value: GqlUint256) -> Self {
        value.0
    }
}

impl Serialize for GqlUint256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ethnum::serde::decimal::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for GqlUint256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        ethnum::serde::decimal::deserialize(deserializer).map(Self)
    }
}

impl CandidType for GqlUint256 {
    fn _ty() -> candid::types::Type {
        candid::types::TypeInner::Text.into()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        serializer.serialize_text(&self.0.to_string())
    }
}

/// Decimal row binding used by generated canister bindings.
///
/// Serde and Candid both use the textual form (matching the JSON row projection); the Router
/// wire form is the packed 16-byte form ([`GqlWireValue::Decimal`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlDecimal(rust_decimal::Decimal);

impl GqlDecimal {
    /// Wrap an upstream decimal value.
    pub const fn from_inner(value: rust_decimal::Decimal) -> Self {
        Self(value)
    }

    /// Return the wrapped upstream decimal value.
    pub const fn into_inner(self) -> rust_decimal::Decimal {
        self.0
    }
}

impl From<rust_decimal::Decimal> for GqlDecimal {
    fn from(value: rust_decimal::Decimal) -> Self {
        Self(value)
    }
}

impl From<GqlDecimal> for rust_decimal::Decimal {
    fn from(value: GqlDecimal) -> Self {
        value.0
    }
}

impl Serialize for GqlDecimal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(&self.0, serializer)
    }
}

impl<'de> Deserialize<'de> for GqlDecimal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <rust_decimal::Decimal as serde::Deserialize>::deserialize(deserializer).map(Self)
    }
}

impl CandidType for GqlDecimal {
    fn _ty() -> candid::types::Type {
        candid::types::TypeInner::Text.into()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        serializer.serialize_text(&self.0.to_string())
    }
}

/// Half-precision float row binding used by generated canister bindings.
///
/// Serde and Candid both use the raw `u16` bit pattern (matching the JSON row projection and
/// [`GqlWireValue::Float16`]).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GqlFloat16(half::f16);

impl GqlFloat16 {
    /// Wrap an upstream half-precision float.
    pub const fn from_inner(value: half::f16) -> Self {
        Self(value)
    }

    /// Return the wrapped upstream half-precision float.
    pub const fn into_inner(self) -> half::f16 {
        self.0
    }
}

impl From<half::f16> for GqlFloat16 {
    fn from(value: half::f16) -> Self {
        Self(value)
    }
}

impl From<GqlFloat16> for half::f16 {
    fn from(value: GqlFloat16) -> Self {
        value.0
    }
}

impl From<GqlInt256> for Value {
    fn from(value: GqlInt256) -> Self {
        Value::Int256(gleaph_gql::types::Int256(value.0))
    }
}

impl From<GqlUint256> for Value {
    fn from(value: GqlUint256) -> Self {
        Value::Uint256(gleaph_gql::types::Uint256(value.0))
    }
}

impl From<GqlDecimal> for Value {
    fn from(value: GqlDecimal) -> Self {
        Value::Decimal(gleaph_gql::types::Decimal(value.0))
    }
}

impl From<GqlFloat16> for Value {
    fn from(value: GqlFloat16) -> Self {
        Value::Float16(value.0)
    }
}

impl Serialize for GqlFloat16 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for GqlFloat16 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        half::f16::deserialize(deserializer).map(Self)
    }
}

impl CandidType for GqlFloat16 {
    fn _ty() -> candid::types::Type {
        candid::types::TypeInner::Nat16.into()
    }

    fn idl_serialize<S>(&self, serializer: S) -> Result<(), S::Error>
    where
        S: candid::types::Serializer,
    {
        serializer.serialize_nat16(self.0.to_bits())
    }
}

/// Decode the canonical little-endian 32-byte form of a signed 256-bit integer.
fn i256_from_le_bytes(bytes: &[u8]) -> Option<I256> {
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    Some(I256::from_le_bytes(bytes))
}

/// Decode the canonical little-endian 32-byte form of an unsigned 256-bit integer.
fn u256_from_le_bytes(bytes: &[u8]) -> Option<U256> {
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    Some(U256::from_le_bytes(bytes))
}

/// Decode the canonical little-endian 16-byte packed form of a decimal.
fn decimal_from_le_bytes(bytes: &[u8]) -> Option<rust_decimal::Decimal> {
    let bytes: [u8; 16] = bytes.try_into().ok()?;
    Some(rust_decimal::Decimal::deserialize(bytes))
}

/// Error while decoding the Router rows blob.
#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GqlWireDecodeError {
    /// Candid payload was not a valid GQL rows envelope.
    #[error("invalid GQL rows Candid blob: {0}")]
    Candid(String),
    /// A wire value uses a representation not yet projected by this codec.
    #[error("unsupported GQL wire value: {0}")]
    UnsupportedValue(&'static str),
    /// A wire value contains an invalid textual numeric representation.
    #[error("invalid GQL wire numeric value: {0}")]
    InvalidNumeric(&'static str),
    /// A typed row did not contain a requested column.
    #[error("missing GQL row field: {0}")]
    MissingField(String),
    /// A typed row contained the same column more than once.
    #[error("duplicate GQL row field: {0}")]
    DuplicateField(String),
    /// A typed row field contained a different logical GQL value variant.
    #[error("GQL row field has the wrong type; expected {0}")]
    TypeMismatch(&'static str),
    /// A logical GQL value could not be represented as JSON.
    #[error("GQL value cannot be represented as JSON: {0}")]
    Json(String),
}

/// Decode the opaque `rows_blob` carried by a Router response.
pub fn decode_rows_blob(bytes: &[u8]) -> Result<GqlWireRows, GqlWireDecodeError> {
    candid::decode_one(bytes).map_err(|error| GqlWireDecodeError::Candid(error.to_string()))
}

/// Encode rows into the Router response blob format.
pub fn encode_rows_blob(rows: &GqlWireRows) -> Result<Vec<u8>, GqlWireDecodeError> {
    candid::encode_one(rows).map_err(|error| GqlWireDecodeError::Candid(error.to_string()))
}

impl GqlWireValue {
    /// Project one IC wire value into the logical GQL value model.
    pub fn try_into_gql_value(self) -> Result<Value, GqlWireDecodeError> {
        Ok(match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Int8(value) => Value::Int8(value),
            Self::Int16(value) => Value::Int16(value),
            Self::Int32(value) => Value::Int32(value),
            Self::Int64(value) => Value::Int64(value),
            Self::Int128(value) => Value::Int128(value),
            Self::Int256(value) => Value::Int256(gleaph_gql::types::Int256(
                i256_from_le_bytes(&value).ok_or(GqlWireDecodeError::InvalidNumeric("Int256"))?,
            )),
            Self::Uint8(value) => Value::Uint8(value),
            Self::Uint16(value) => Value::Uint16(value),
            Self::Uint32(value) => Value::Uint32(value),
            Self::Uint64(value) => Value::Uint64(value),
            Self::Uint128(value) => Value::Uint128(value),
            Self::Uint256(value) => Value::Uint256(gleaph_gql::types::Uint256(
                u256_from_le_bytes(&value).ok_or(GqlWireDecodeError::InvalidNumeric("Uint256"))?,
            )),
            Self::Float16(bits) => Value::Float16(half::f16::from_bits(bits)),
            Self::Float32(value) => Value::Float32(value),
            Self::Float64(value) => Value::Float64(value),
            Self::Float128(bytes) => {
                #[cfg(feature = "nightly-f128")]
                {
                    let bytes = <[u8; 16]>::try_from(bytes.as_slice())
                        .map_err(|_| GqlWireDecodeError::InvalidNumeric("Float128"))?;
                    Value::Float128(f128::from_bits(u128::from_le_bytes(bytes)))
                }
                #[cfg(not(feature = "nightly-f128"))]
                {
                    let _ = bytes;
                    return Err(GqlWireDecodeError::UnsupportedValue("Float128"));
                }
            }
            Self::Float256(bytes) => {
                let bytes = <[u8; 32]>::try_from(bytes.as_slice())
                    .map_err(|_| GqlWireDecodeError::InvalidNumeric("Float256"))?;
                Value::Float256(f256::f256::from_le_bytes(bytes))
            }
            Self::Decimal(value) => Value::Decimal(gleaph_gql::types::Decimal(
                decimal_from_le_bytes(&value)
                    .ok_or(GqlWireDecodeError::InvalidNumeric("Decimal"))?,
            )),
            Self::Text(value) => Value::Text(value),
            Self::Bytes(value) => Value::Bytes(value),
            Self::Date(value) => Value::Date(value),
            Self::Time(value) => Value::Time(value),
            Self::LocalTime(value) => Value::LocalTime(value),
            Self::DateTime { seconds, nanos } => Value::DateTime(seconds, nanos),
            Self::LocalDateTime { seconds, nanos } => Value::LocalDateTime(seconds, nanos),
            Self::ZonedDateTime {
                seconds,
                nanos,
                offset_seconds,
            } => Value::ZonedDateTime(seconds, nanos, offset_seconds),
            Self::ZonedTime {
                nanos,
                offset_seconds,
            } => Value::ZonedTime(nanos, offset_seconds),
            Self::Duration { months, nanos } => Value::Duration(months, nanos),
            Self::Principal(bytes) => Value::Extension(Box::new(GqlPrincipal(
                Principal::try_from_slice(&bytes)
                    .map_err(|_| GqlWireDecodeError::InvalidNumeric("Principal"))?,
            ))),
            Self::ExtensionLeaf { .. } => {
                return Err(GqlWireDecodeError::UnsupportedValue("ExtensionLeaf"));
            }
            Self::ValueBinary(bytes) => Value::from_binary_bytes(&bytes)
                .map_err(|_| GqlWireDecodeError::UnsupportedValue("ValueBinary"))?,
            Self::List(values) => Value::List(
                values
                    .into_iter()
                    .map(Self::try_into_gql_value)
                    .collect::<Result<_, _>>()?,
            ),
            Self::Path(elements) => Value::Path(
                elements
                    .into_iter()
                    .map(|element| match element {
                        GqlWirePathElement::Vertex(id) => {
                            gleaph_gql::types::PathElement::Vertex(id.into())
                        }
                        GqlWirePathElement::Edge(id) => {
                            gleaph_gql::types::PathElement::Edge(id.into())
                        }
                    })
                    .collect(),
            ),
            Self::Record(fields) => Value::Record(
                fields
                    .into_iter()
                    .map(|(name, value)| Ok((name, value.try_into_gql_value()?)))
                    .collect::<Result<_, GqlWireDecodeError>>()?,
            ),
        })
    }
}

impl GqlWireRow {
    /// Project one wire row into an ordered logical GQL record.
    pub fn try_into_gql_row(self) -> Result<Vec<(String, Value)>, GqlWireDecodeError> {
        self.columns
            .into_iter()
            .map(|(name, value)| Ok((name, value.try_into_gql_value()?)))
            .collect()
    }
}

impl GqlWireRows {
    /// Project all rows into ordered logical GQL records.
    pub fn try_into_gql_rows(self) -> Result<Vec<Vec<(String, Value)>>, GqlWireDecodeError> {
        self.rows
            .into_iter()
            .map(GqlWireRow::try_into_gql_row)
            .collect()
    }
}

/// GQL `ZonedTime` row binding (time of day with a fixed UTC offset).
///
/// Neither `jiff` nor `chrono` models a time-of-day with an offset, so this binding keeps the
/// wire record form directly.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct GqlZonedTime {
    /// Nanoseconds since midnight (local time of day).
    pub nanos: u64,
    /// UTC offset in seconds.
    pub offset_seconds: i32,
}

impl From<GqlZonedTime> for Value {
    fn from(value: GqlZonedTime) -> Value {
        Value::ZonedTime(value.nanos, value.offset_seconds)
    }
}

#[cfg(feature = "chrono")]
pub use temporal_chrono::{
    GqlDate, GqlDateTime, GqlDuration, GqlLocalDateTime, GqlLocalTime, GqlTime, GqlZonedDateTime,
};
/// GQL temporal row bindings backed by `jiff` (default) or `chrono` (opt-in).
#[cfg(all(feature = "jiff", not(feature = "chrono")))]
pub use temporal_jiff::{
    GqlDate, GqlDateTime, GqlDuration, GqlLocalDateTime, GqlLocalTime, GqlTime, GqlZonedDateTime,
};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_blob_round_trips() {
        let rows = GqlWireRows {
            rows: vec![GqlWireRow {
                columns: vec![("name".into(), GqlWireValue::Text("Ada".into()))],
            }],
        };
        let decoded = decode_rows_blob(&encode_rows_blob(&rows).unwrap()).unwrap();
        assert_eq!(decoded, rows);
    }

    #[test]
    fn projects_exact_scalars_and_principal() {
        assert_eq!(
            GqlWireValue::Int256(I256::from(-7).to_le_bytes().to_vec())
                .try_into_gql_value()
                .unwrap()
                .to_string(),
            "-7"
        );
        assert_eq!(
            GqlWireValue::Decimal(
                "1.25"
                    .parse::<rust_decimal::Decimal>()
                    .unwrap()
                    .serialize()
                    .to_vec()
            )
            .try_into_gql_value()
            .unwrap()
            .to_string(),
            "1.25"
        );
        let principal = Principal::anonymous();
        let value = GqlWireValue::Principal(principal.as_slice().to_vec())
            .try_into_gql_value()
            .unwrap();
        assert!(matches!(value, Value::Extension(_)));
    }

    #[test]
    fn wide_float_wrappers_round_trip_serde_bytes() {
        let bytes = [0xabu8; 32];
        let value = Float256::from_le_bytes(bytes);
        let encoded = serde_json::to_vec(&value).unwrap();
        let decoded: Float256 = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded.to_le_bytes(), bytes);
        assert_eq!(decoded.into_inner(), value.into_inner());
    }

    #[test]
    fn principal_serde_round_trips_text_form() {
        let principal = GqlPrincipal::from_inner(Principal::anonymous());
        let encoded = serde_json::to_string(&principal).unwrap();
        let decoded: GqlPrincipal = serde_json::from_str(&encoded).unwrap();
        assert_eq!(decoded, principal);
    }

    #[test]
    fn rejects_unimplemented_wide_float_projection() {
        #[cfg(not(feature = "nightly-f128"))]
        assert_eq!(
            GqlWireValue::Float128(vec![0; 16]).try_into_gql_value(),
            Err(GqlWireDecodeError::UnsupportedValue("Float128"))
        );
    }

    #[test]
    fn projects_float256_from_canonical_bytes() {
        let value = GqlWireValue::Float256(f256::f256::from(1.25_f64).to_le_bytes().to_vec())
            .try_into_gql_value()
            .unwrap();
        assert!(matches!(value, Value::Float256(_)));
    }

    #[cfg(feature = "nightly-f128")]
    #[test]
    fn projects_float128_when_nightly_feature_is_enabled() {
        let bits = 1.25_f128.to_bits().to_le_bytes().to_vec();
        let value = GqlWireValue::Float128(bits).try_into_gql_value().unwrap();
        assert!(matches!(value, Value::Float128(_)));
    }
}
