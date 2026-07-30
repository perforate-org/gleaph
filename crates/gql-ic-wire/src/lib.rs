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

/// A path element in a GQL result value.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub enum GqlWirePathElement {
    /// Vertex identifier.
    Vertex(Vec<u8>),
    /// Edge identifier.
    Edge(Vec<u8>),
}

/// A scalar or composite value in the Candid GQL result wire format.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub enum GqlWireValue {
    Null,
    Bool(bool),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Int128(i128),
    Int256(String),
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uint64(u64),
    Uint128(u128),
    Uint256(String),
    Float16(u16),
    Float32(f32),
    Float64(f64),
    Float128(Vec<u8>),
    Float256(Vec<u8>),
    Decimal(String),
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
                value
                    .parse::<I256>()
                    .map_err(|_| GqlWireDecodeError::InvalidNumeric("Int256"))?,
            )),
            Self::Uint8(value) => Value::Uint8(value),
            Self::Uint16(value) => Value::Uint16(value),
            Self::Uint32(value) => Value::Uint32(value),
            Self::Uint64(value) => Value::Uint64(value),
            Self::Uint128(value) => Value::Uint128(value),
            Self::Uint256(value) => Value::Uint256(gleaph_gql::types::Uint256(
                value
                    .parse::<U256>()
                    .map_err(|_| GqlWireDecodeError::InvalidNumeric("Uint256"))?,
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
                value
                    .parse::<rust_decimal::Decimal>()
                    .map_err(|_| GqlWireDecodeError::InvalidNumeric("Decimal"))?,
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
            GqlWireValue::Int256("-7".into())
                .try_into_gql_value()
                .unwrap()
                .to_string(),
            "-7"
        );
        assert_eq!(
            GqlWireValue::Decimal("1.25".into())
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
