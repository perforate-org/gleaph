//! Lightweight Candid wire types for GQL query results.
//!
//! This crate intentionally contains no graph-kernel or planner dependency. It is suitable for
//! SDK and canister runtimes that need to decode Router response blobs.

#![cfg_attr(feature = "nightly-f128", feature(f128))]

use std::borrow::Cow;
use std::fmt;
use std::ops::Deref;

use candid::{CandidType, Principal};
#[cfg(any(feature = "i256", feature = "u256"))]
use ethnum::{I256, U256};
use gleaph_gql::extensions::gql_extension;
use gleaph_gql::value::{ExtensionValue, Value, ValueBinaryError};
use serde::{Deserialize, Serialize};

/// Sortable-index domain for [`PrincipalValue`] (not the GQL extension type string).
pub const PRINCIPAL_EXTENSION_SORTABLE_DOMAIN: &str = "Pr";

/// Internet Computer [`Principal`] as a [`gleaph_gql::Value::Extension`].
///
/// Shared by `gleaph-gql-ic` and `gleaph-gql-ic-wire` so both Candid wire enums resolve to one
/// Principal Extension type. Compact-binary encoding is tag 34 (see the `gql_extension!` block).
#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, PartialEq, Ord, PartialOrd, Hash)]
pub struct PrincipalValue(pub Principal);

impl Deref for PrincipalValue {
    type Target = Principal;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl fmt::Display for PrincipalValue {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&self.0, f)
    }
}

fn decode_principal_payload(payload: &[u8]) -> Result<Box<dyn ExtensionValue>, ValueBinaryError> {
    let p = Principal::try_from_slice(payload)
        .map_err(|_| ValueBinaryError::InvalidExtensionPayload)?;
    Ok(Box::new(PrincipalValue(p)))
}

gql_extension! {
    prefix: "IC",
    types: [
        {
            rust_type: PrincipalValue,
            type_name: "PRINCIPAL",
            decoder: IcExtensionBinaryDecode,
            eq: |this, other| this.0 == other.0,
            cmp: |this, other| this.0.cmp(&other.0),
            sortable_index_key: {
                domain: PRINCIPAL_EXTENSION_SORTABLE_DOMAIN,
                bytes: |this| Cow::Borrowed(this.0.as_slice()),
            },
            binary_payload: |this| Cow::Borrowed(this.0.as_slice()),
            short_blob: |this| Cow::Borrowed(this.0.as_slice()),
            short_blob_decode: decode_principal_payload,
        },
    ],
}

impl From<Principal> for PrincipalValue {
    fn from(value: Principal) -> Self {
        Self(value)
    }
}

impl From<PrincipalValue> for Value {
    fn from(value: PrincipalValue) -> Self {
        Value::Extension(Box::new(value))
    }
}

/// Extract a [`Principal`] if this value is a [`PrincipalValue`] extension.
pub fn value_as_principal(value: &Value) -> Option<Principal> {
    match value {
        Value::Extension(ext) => ext.as_any().downcast_ref::<PrincipalValue>().map(|p| p.0),
        _ => None,
    }
}

/// Wrap a [`Principal`] as [`Value::Extension`].
#[inline]
pub fn principal_to_value(p: Principal) -> Value {
    Value::Extension(Box::new(PrincipalValue(p)))
}

/// Registers [`IcExtensionBinaryDecode`] for deserializing extension values embedded in rkyv
/// archives (e.g. AST or property [`Value`] blobs).
///
/// Idempotent for the process: only the first successful
/// [`gleaph_gql::try_install_global_rkyv_extension_binary_decode`] wins. Call during canister or
/// service startup before loading rkyv data that may contain [`Principal`]. Requires the `rkyv`
/// feature.
#[cfg(feature = "rkyv")]
pub fn install_ic_extension_binary_decode_for_rkyv() {
    let _ = gleaph_gql::try_install_global_rkyv_extension_binary_decode(
        &IcExtensionBinaryDecode::INSTANCE,
    );
}

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

impl From<&gleaph_gql::types::PathElement> for GqlWirePathElement {
    fn from(value: &gleaph_gql::types::PathElement) -> Self {
        match value {
            gleaph_gql::types::PathElement::Vertex(id) => Self::Vertex(id.as_ref().to_vec()),
            gleaph_gql::types::PathElement::Edge(id) => Self::Edge(id.as_ref().to_vec()),
        }
    }
}

impl From<&GqlWirePathElement> for gleaph_gql::types::PathElement {
    fn from(value: &GqlWirePathElement) -> Self {
        match value {
            GqlWirePathElement::Vertex(id) => {
                gleaph_gql::types::PathElement::Vertex(id.clone().into())
            }
            GqlWirePathElement::Edge(id) => gleaph_gql::types::PathElement::Edge(id.clone().into()),
        }
    }
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
    Principal(Principal),
    ExtensionLeaf {
        type_name: String,
        payload: Vec<u8>,
    },
    ValueBinary(Vec<u8>),
    List(Vec<GqlWireValue>),
    Path(Vec<GqlWirePathElement>),
    Record(Vec<(String, GqlWireValue)>),
}

/// One result row as ordered column/value pairs.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, CandidType)]
pub struct GqlWireRow {
    /// Columns in wire order.
    pub columns: Vec<(String, GqlWireValue)>,
}

impl GqlWireRow {
    /// Project one ordered logical GQL record into a wire row.
    pub fn try_from_value_row(
        row: &std::collections::BTreeMap<String, Value>,
    ) -> Result<Self, GqlWireDecodeError> {
        let mut columns = Vec::with_capacity(row.len());
        for (k, v) in row.iter() {
            columns.push((k.clone(), GqlWireValue::try_from_value(v)?));
        }
        Ok(Self { columns })
    }

    /// Project one wire row into an ordered logical GQL record.
    pub fn try_into_value_row(
        self,
    ) -> Result<std::collections::BTreeMap<String, Value>, GqlWireDecodeError> {
        let mut out = std::collections::BTreeMap::new();
        for (k, wv) in self.columns {
            out.insert(k, wv.try_into_gql_value()?);
        }
        Ok(out)
    }
}

/// Materialized rows returned in a Router response blob.
#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize, CandidType)]
pub struct GqlWireRows {
    /// Rows in result order.
    pub rows: Vec<GqlWireRow>,
}

impl GqlWireRows {
    /// Project a slice of ordered logical GQL records into wire rows.
    pub fn try_from_value_rows(
        rows: &[std::collections::BTreeMap<String, Value>],
    ) -> Result<Self, GqlWireDecodeError> {
        rows.iter()
            .map(GqlWireRow::try_from_value_row)
            .collect::<Result<Vec<_>, _>>()
            .map(|rows| Self { rows })
    }

    /// Project all wire rows into ordered logical GQL records.
    pub fn try_into_value_rows(
        self,
    ) -> Result<Vec<std::collections::BTreeMap<String, Value>>, GqlWireDecodeError> {
        self.rows
            .into_iter()
            .map(GqlWireRow::try_into_value_row)
            .collect()
    }

    /// Encode rows into the Router response blob format.
    pub fn encode_blob(&self) -> Result<Vec<u8>, GqlWireDecodeError> {
        candid::encode_one(self).map_err(|error| GqlWireDecodeError::Candid(error.to_string()))
    }

    /// Decode the opaque `rows_blob` carried by a Router response.
    pub fn decode_blob(blob: &[u8]) -> Result<Self, GqlWireDecodeError> {
        candid::decode_one(blob).map_err(|error| GqlWireDecodeError::Candid(error.to_string()))
    }

    /// Concatenate independent shard-local row batches (union merge).
    pub fn merge_batch_blobs(left: &[u8], right: &[u8]) -> Result<Vec<u8>, GqlWireDecodeError> {
        let mut merged = Self::decode_blob(left)?;
        merged.rows.extend(Self::decode_blob(right)?.rows);
        merged.encode_blob()
    }

    /// Merge two optional row blobs (union merge).
    pub fn merge_optional_batch_blobs(
        acc: Option<Vec<u8>>,
        next: Option<Vec<u8>>,
    ) -> Result<Option<Vec<u8>>, GqlWireDecodeError> {
        match (acc, next) {
            (None, None) => Ok(None),
            (Some(blob), None) | (None, Some(blob)) => Ok(Some(blob)),
            (Some(left), Some(right)) => Ok(Some(Self::merge_batch_blobs(&left, &right)?)),
        }
    }
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
#[cfg(feature = "f256")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Float256(f256::f256);

#[cfg(feature = "f256")]
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

#[cfg(feature = "f256")]
impl Serialize for Float256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_bytes(&self.to_le_bytes())
    }
}

#[cfg(feature = "f256")]
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

#[cfg(feature = "f256")]
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
#[cfg(feature = "i256")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlInt256(I256);

#[cfg(feature = "i256")]
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

#[cfg(feature = "i256")]
impl From<I256> for GqlInt256 {
    fn from(value: I256) -> Self {
        Self(value)
    }
}

#[cfg(feature = "i256")]
impl From<GqlInt256> for I256 {
    fn from(value: GqlInt256) -> Self {
        value.0
    }
}

#[cfg(feature = "i256")]
impl Serialize for GqlInt256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ethnum::serde::decimal::serialize(&self.0, serializer)
    }
}

#[cfg(feature = "i256")]
impl<'de> Deserialize<'de> for GqlInt256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        ethnum::serde::decimal::deserialize(deserializer).map(Self)
    }
}

#[cfg(feature = "i256")]
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
#[cfg(feature = "u256")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlUint256(U256);

#[cfg(feature = "u256")]
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

#[cfg(feature = "u256")]
impl From<U256> for GqlUint256 {
    fn from(value: U256) -> Self {
        Self(value)
    }
}

#[cfg(feature = "u256")]
impl From<GqlUint256> for U256 {
    fn from(value: GqlUint256) -> Self {
        value.0
    }
}

#[cfg(feature = "u256")]
impl Serialize for GqlUint256 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        ethnum::serde::decimal::serialize(&self.0, serializer)
    }
}

#[cfg(feature = "u256")]
impl<'de> Deserialize<'de> for GqlUint256 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        ethnum::serde::decimal::deserialize(deserializer).map(Self)
    }
}

#[cfg(feature = "u256")]
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
#[cfg(feature = "decimal")]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GqlDecimal(rust_decimal::Decimal);

#[cfg(feature = "decimal")]
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

#[cfg(feature = "decimal")]
impl From<rust_decimal::Decimal> for GqlDecimal {
    fn from(value: rust_decimal::Decimal) -> Self {
        Self(value)
    }
}

#[cfg(feature = "decimal")]
impl From<GqlDecimal> for rust_decimal::Decimal {
    fn from(value: GqlDecimal) -> Self {
        value.0
    }
}

#[cfg(feature = "decimal")]
impl Serialize for GqlDecimal {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serde::Serialize::serialize(&self.0, serializer)
    }
}

#[cfg(feature = "decimal")]
impl<'de> Deserialize<'de> for GqlDecimal {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        <rust_decimal::Decimal as serde::Deserialize>::deserialize(deserializer).map(Self)
    }
}

#[cfg(feature = "decimal")]
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
#[cfg(feature = "f16")]
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GqlFloat16(half::f16);

#[cfg(feature = "f16")]
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

#[cfg(feature = "f16")]
impl From<half::f16> for GqlFloat16 {
    fn from(value: half::f16) -> Self {
        Self(value)
    }
}

#[cfg(feature = "f16")]
impl From<GqlFloat16> for half::f16 {
    fn from(value: GqlFloat16) -> Self {
        value.0
    }
}

#[cfg(feature = "i256")]
impl From<GqlInt256> for Value {
    fn from(value: GqlInt256) -> Self {
        Value::Int256(gleaph_gql::types::Int256(value.0))
    }
}

#[cfg(feature = "u256")]
impl From<GqlUint256> for Value {
    fn from(value: GqlUint256) -> Self {
        Value::Uint256(gleaph_gql::types::Uint256(value.0))
    }
}

#[cfg(feature = "decimal")]
impl From<GqlDecimal> for Value {
    fn from(value: GqlDecimal) -> Self {
        Value::Decimal(gleaph_gql::types::Decimal(value.0))
    }
}

#[cfg(feature = "f16")]
impl From<GqlFloat16> for Value {
    fn from(value: GqlFloat16) -> Self {
        Value::Float16(value.0)
    }
}

#[cfg(feature = "f16")]
impl Serialize for GqlFloat16 {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        self.0.serialize(serializer)
    }
}

#[cfg(feature = "f16")]
impl<'de> Deserialize<'de> for GqlFloat16 {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        half::f16::deserialize(deserializer).map(Self)
    }
}

#[cfg(feature = "f16")]
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
#[cfg(feature = "i256")]
fn i256_from_le_bytes(bytes: &[u8]) -> Option<I256> {
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    Some(I256::from_le_bytes(bytes))
}

/// Decode the canonical little-endian 32-byte form of an unsigned 256-bit integer.
#[cfg(feature = "u256")]
fn u256_from_le_bytes(bytes: &[u8]) -> Option<U256> {
    let bytes: [u8; 32] = bytes.try_into().ok()?;
    Some(U256::from_le_bytes(bytes))
}

/// Decode the canonical little-endian 16-byte packed form of a decimal.
#[cfg(feature = "decimal")]
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
    /// The GQL binary codec failed while projecting a value to/from the wire form.
    #[error("gleaph_gql binary codec: {0:?}")]
    Binary(#[from] ValueBinaryError),
    /// An `ExtensionLeaf` payload could not be decoded.
    #[error("GQL wire ExtensionLeaf payload could not be decoded ({type_name}): {source:?}")]
    ExtensionLeafDecode {
        type_name: String,
        source: ValueBinaryError,
    },
    /// A wire value contains an invalid canonical float representation.
    #[error("invalid canonical float representation for wire conversion: {kind}")]
    InvalidFloatRepresentation { kind: &'static str },
    /// A GQL params blob must decode to a Record at top level.
    #[error("GQL params blob must decode to a Record at top level")]
    ParamsTopLevelNotRecord,
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
            #[cfg(feature = "i256")]
            Self::Int256(value) => Value::Int256(gleaph_gql::types::Int256(
                i256_from_le_bytes(&value).ok_or(GqlWireDecodeError::InvalidNumeric("Int256"))?,
            )),
            #[cfg(not(feature = "i256"))]
            Self::Int256(_) => {
                return Err(GqlWireDecodeError::UnsupportedValue("Int256"));
            }
            Self::Uint8(value) => Value::Uint8(value),
            Self::Uint16(value) => Value::Uint16(value),
            Self::Uint32(value) => Value::Uint32(value),
            Self::Uint64(value) => Value::Uint64(value),
            Self::Uint128(value) => Value::Uint128(value),
            #[cfg(feature = "u256")]
            Self::Uint256(value) => Value::Uint256(gleaph_gql::types::Uint256(
                u256_from_le_bytes(&value).ok_or(GqlWireDecodeError::InvalidNumeric("Uint256"))?,
            )),
            #[cfg(not(feature = "u256"))]
            Self::Uint256(_) => {
                return Err(GqlWireDecodeError::UnsupportedValue("Uint256"));
            }
            #[cfg(feature = "f16")]
            Self::Float16(bits) => Value::Float16(half::f16::from_bits(bits)),
            #[cfg(not(feature = "f16"))]
            Self::Float16(_) => {
                return Err(GqlWireDecodeError::UnsupportedValue("Float16"));
            }
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
            #[cfg(feature = "f256")]
            Self::Float256(bytes) => {
                let bytes = <[u8; 32]>::try_from(bytes.as_slice())
                    .map_err(|_| GqlWireDecodeError::InvalidNumeric("Float256"))?;
                Value::Float256(f256::f256::from_le_bytes(bytes))
            }
            #[cfg(not(feature = "f256"))]
            Self::Float256(_) => {
                return Err(GqlWireDecodeError::UnsupportedValue("Float256"));
            }
            #[cfg(feature = "decimal")]
            Self::Decimal(value) => Value::Decimal(gleaph_gql::types::Decimal(
                decimal_from_le_bytes(&value)
                    .ok_or(GqlWireDecodeError::InvalidNumeric("Decimal"))?,
            )),
            #[cfg(not(feature = "decimal"))]
            Self::Decimal(_) => {
                return Err(GqlWireDecodeError::UnsupportedValue("Decimal"));
            }
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
            Self::Principal(principal) => Value::Extension(Box::new(PrincipalValue(principal))),
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

    /// Project one IC wire value into the logical GQL value model, borrowing the value.
    ///
    /// Convenience for callers that hold a `&GqlWireValue` (e.g. row-column lookups) and do not
    /// want to clone explicitly. Clones internally.
    pub fn try_into_gql_value_ref(&self) -> Result<Value, GqlWireDecodeError> {
        self.clone().try_into_gql_value()
    }

    /// Alias for [`Self::try_into_gql_value`], kept for callers that used the former
    /// `IcWireValue::try_into_value` name.
    pub fn try_into_value(&self) -> Result<Value, GqlWireDecodeError> {
        self.try_into_gql_value_ref()
    }

    /// Project a GQL runtime value into the Candid wire value (lossless).
    ///
    /// Mirrors the encode direction of the shared wire enum so both the execution side and the SDK
    /// can produce the same wire shape. `Value::Extension` is projected to [`Self::Principal`] when
    /// it is a [`PrincipalValue`], otherwise to an opaque [`Self::ExtensionLeaf`].
    pub fn try_from_value(value: &Value) -> Result<Self, GqlWireDecodeError> {
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(v) => Self::Bool(*v),
            Value::Int8(v) => Self::Int8(*v),
            Value::Int16(v) => Self::Int16(*v),
            Value::Int32(v) => Self::Int32(*v),
            Value::Int64(v) => Self::Int64(*v),
            Value::Int128(v) => Self::Int128(*v),
            #[cfg(feature = "i256")]
            Value::Int256(v) => Self::Int256(v.0.to_le_bytes().to_vec()),
            #[cfg(not(feature = "i256"))]
            Value::Int256(_) => {
                return Err(GqlWireDecodeError::UnsupportedValue("Int256"));
            }
            Value::Uint8(v) => Self::Uint8(*v),
            Value::Uint16(v) => Self::Uint16(*v),
            Value::Uint32(v) => Self::Uint32(*v),
            Value::Uint64(v) => Self::Uint64(*v),
            Value::Uint128(v) => Self::Uint128(*v),
            #[cfg(feature = "u256")]
            Value::Uint256(v) => Self::Uint256(v.0.to_le_bytes().to_vec()),
            #[cfg(not(feature = "u256"))]
            Value::Uint256(_) => {
                return Err(GqlWireDecodeError::UnsupportedValue("Uint256"));
            }
            #[cfg(feature = "f16")]
            Value::Float16(v) => Self::Float16(v.to_bits()),
            #[cfg(not(feature = "f16"))]
            Value::Float16(_) => {
                return Err(GqlWireDecodeError::UnsupportedValue("Float16"));
            }
            Value::Float32(v) => Self::Float32(*v),
            Value::Float64(v) => Self::Float64(*v),
            #[cfg(feature = "nightly-f128")]
            Value::Float128(_) => Self::Float128(canonical_float128_bytes(value)?),
            #[cfg(feature = "f256")]
            Value::Float256(v) => Self::Float256(v.to_le_bytes().to_vec()),
            #[cfg(feature = "decimal")]
            Value::Decimal(v) => Self::Decimal(v.0.serialize().to_vec()),
            #[cfg(not(feature = "decimal"))]
            Value::Decimal(_) => {
                return Err(GqlWireDecodeError::UnsupportedValue("Decimal"));
            }
            Value::Text(v) => Self::Text(v.clone()),
            Value::Bytes(v) => Self::Bytes(v.clone()),
            Value::Date(v) => Self::Date(*v),
            Value::Time(v) => Self::Time(*v),
            Value::LocalTime(v) => Self::LocalTime(*v),
            Value::DateTime(seconds, nanos) => Self::DateTime {
                seconds: *seconds,
                nanos: *nanos,
            },
            Value::LocalDateTime(seconds, nanos) => Self::LocalDateTime {
                seconds: *seconds,
                nanos: *nanos,
            },
            Value::ZonedDateTime(seconds, nanos, offset_seconds) => Self::ZonedDateTime {
                seconds: *seconds,
                nanos: *nanos,
                offset_seconds: *offset_seconds,
            },
            Value::ZonedTime(nanos, offset_seconds) => Self::ZonedTime {
                nanos: *nanos,
                offset_seconds: *offset_seconds,
            },
            Value::Duration(months, nanos) => Self::Duration {
                months: *months,
                nanos: *nanos,
            },
            Value::Extension(ext) => {
                if let Some(p) = ext.as_any().downcast_ref::<PrincipalValue>() {
                    Self::Principal(p.0)
                } else {
                    Self::extension_leaf(ext.as_ref())?
                }
            }
            Value::List(values) => Self::List(
                values
                    .iter()
                    .map(Self::try_from_value)
                    .collect::<Result<_, _>>()?,
            ),
            Value::Path(elements) => {
                Self::Path(elements.iter().map(GqlWirePathElement::from).collect())
            }
            Value::Record(fields) => Self::Record(
                fields
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), Self::try_from_value(v)?)))
                    .collect::<Result<Vec<_>, GqlWireDecodeError>>()?,
            ),
        })
    }

    fn extension_leaf(ext: &dyn ExtensionValue) -> Result<Self, GqlWireDecodeError> {
        let wrapped = Value::Extension(ext.clone_box());
        let payload = wrapped.to_binary_bytes()?;
        Ok(Self::ExtensionLeaf {
            type_name: ext.type_name().to_owned(),
            payload,
        })
    }
}

impl TryFrom<&Value> for GqlWireValue {
    type Error = GqlWireDecodeError;

    fn try_from(value: &Value) -> Result<Self, Self::Error> {
        Self::try_from_value(value)
    }
}

impl TryFrom<Value> for GqlWireValue {
    type Error = GqlWireDecodeError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Self::try_from_value(&value)
    }
}

#[cfg(feature = "nightly-f128")]
fn canonical_float128_bytes(value: &Value) -> Result<Vec<u8>, GqlWireDecodeError> {
    let encoded = value.to_binary_bytes()?;
    if encoded.len() != 17 || encoded[0] != 31 {
        return Err(GqlWireDecodeError::InvalidFloatRepresentation { kind: "Float128" });
    }
    Ok(encoded[1..].to_vec())
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

    #[cfg(all(feature = "i256", feature = "decimal"))]
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
        let value = GqlWireValue::Principal(principal)
            .try_into_gql_value()
            .unwrap();
        assert!(matches!(value, Value::Extension(_)));
    }

    #[cfg(feature = "f256")]
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
    fn rejects_unimplemented_wide_float_projection() {
        #[cfg(not(feature = "nightly-f128"))]
        assert_eq!(
            GqlWireValue::Float128(vec![0; 16]).try_into_gql_value(),
            Err(GqlWireDecodeError::UnsupportedValue("Float128"))
        );
    }

    #[cfg(feature = "f256")]
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
