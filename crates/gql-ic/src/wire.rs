//! Candid-oriented wire values: lossless bridge to [`gleaph_gql::Value`].
//!
//! Unlike ad-hoc conversion that maps unknown [`Value::Extension`] to text, this module
//! preserves extension payloads using the same compact binary leaf encoding the graph
//! runtime uses (tags **33** / **34**), and falls back to a full compact value blob when
//! no native Candid primitive exists (e.g. `Float128`).
//!
//! ## Canister GQL parameters (preferred)
//!
//! Pass a single [`Vec<u8>`] at the IC boundary: [`encode_gql_params_blob`] /
//! [`decode_gql_params_blob`] — one compact-binary [`Value::Record`] (same codec as
//! [`Value::to_binary_bytes`]), not a Candid-deep [`IcWireValue`] tree.

use std::collections::BTreeMap;

use candid::{CandidType, Principal};
use f256::f256;
use gleaph_gql::value::ValueBinaryError;
use gleaph_gql::{ExtensionValue, Value};
use serde::{Deserialize, Serialize};

use crate::{IcExtensionBinaryDecode, PrincipalValue};

/// Structured path element on the API wire.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub enum IcWirePathElement {
    Vertex(Vec<u8>),
    Edge(Vec<u8>),
}

/// Internet-Computer–friendly wire representation of a [`Value`].
///
/// # Design
///
/// - Scalar variants preserve the exact GQL width. Non-native floats use their canonical bit
///   representation (`Float16` as `u16`, `Float128`/`Float256` as little-endian bytes).
/// - [`Principal`] uses a dedicated variant backed by [`PrincipalValue`] on the GQL side
///   ([`ExtensionValue::type_name`](gleaph_gql::value::ExtensionValue::type_name) is `IC.PRINCIPAL`).
/// - Other extensions use [`Self::ExtensionLeaf`], wrapping the **compact binary leaf** for
///   `Value::Extension` (starting with byte `33` or `34`).
/// - Values with no dedicated Candid tuple use [`Self::ValueBinary`], a full [`Value::encode_binary_into`]
///   payload (still decoded with [`IcExtensionBinaryDecode`] where extensions appear).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub enum IcWireValue {
    Null,
    Bool(bool),
    Int8(i8),
    Int16(i16),
    Int32(i32),
    Int64(i64),
    Uint8(u8),
    Uint16(u16),
    Uint32(u32),
    Uint64(u64),
    Int128(i128),
    Uint128(u128),
    Int256(Vec<u8>),
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
    /// Compact binary for a **single** [`Value::Extension`] (tag 33 / 34 + payload).
    ///
    /// `type_name` duplicates [`ExtensionValue::type_name`] for Candid readability; decoders
    /// authoritative content is always `payload`.
    ExtensionLeaf {
        type_name: String,
        payload: Vec<u8>,
    },
    /// Lossless encoding of an arbitrary [`Value`] (full compact binary tree).
    ValueBinary(Vec<u8>),
    List(Vec<IcWireValue>),
    Path(Vec<IcWirePathElement>),
    /// String-keyed records; order matches [`Value::Record`] field order (significant for Gleaph).
    Record(Vec<(String, IcWireValue)>),
}

/// Failed to project between [`Value`] and [`IcWireValue`].
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("gleaph_gql binary codec: {0:?}")]
    Binary(#[from] ValueBinaryError),
    #[error("IcWireValue::ValueBinary payload could not be decoded: {0:?}")]
    OpaqueDecode(ValueBinaryError),
    #[error("IcWireValue::ExtensionLeaf payload could not be decoded ({type_name}): {source:?}")]
    ExtensionLeafDecode {
        type_name: String,
        source: ValueBinaryError,
    },
    #[error("invalid numeric string for wire conversion: {kind}")]
    InvalidNumericString { kind: &'static str },
    #[error("invalid canonical float representation for wire conversion: {kind}")]
    InvalidFloatRepresentation { kind: &'static str },
    #[error("GQL params blob must decode to a Record at top level")]
    ParamsTopLevelNotRecord,
    #[error("candid wire error: {0}")]
    Candid(String),
}

/// Encode GQL named parameters for the graph canister: one compact-binary [`Value::Record`].
#[inline]
pub fn encode_gql_params_blob(fields: Vec<(String, Value)>) -> Result<Vec<u8>, WireError> {
    Value::Record(fields).to_binary_bytes().map_err(Into::into)
}

/// Decode [`encode_gql_params_blob`] output into a parameter map. Empty input yields an empty map.
pub fn decode_gql_params_blob(bytes: &[u8]) -> Result<BTreeMap<String, Value>, WireError> {
    if bytes.is_empty() {
        return Ok(BTreeMap::new());
    }
    let v = Value::from_binary_bytes_with_extensions(bytes, ic_extension_decode())?;
    match v {
        Value::Record(fields) => Ok(fields.into_iter().collect()),
        _ => Err(WireError::ParamsTopLevelNotRecord),
    }
}

/// Decode extension / value compact blobs using the default IC decoder (Principal, …).
#[inline]
pub fn ic_extension_decode() -> &'static IcExtensionBinaryDecode {
    &IcExtensionBinaryDecode::INSTANCE
}

impl From<&gleaph_gql::types::PathElement> for IcWirePathElement {
    fn from(value: &gleaph_gql::types::PathElement) -> Self {
        match value {
            gleaph_gql::types::PathElement::Vertex(id) => Self::Vertex(id.as_ref().to_vec()),
            gleaph_gql::types::PathElement::Edge(id) => Self::Edge(id.as_ref().to_vec()),
        }
    }
}

impl From<&IcWirePathElement> for gleaph_gql::types::PathElement {
    fn from(value: &IcWirePathElement) -> Self {
        match value {
            IcWirePathElement::Vertex(id) => {
                gleaph_gql::types::PathElement::Vertex(id.clone().into())
            }
            IcWirePathElement::Edge(id) => gleaph_gql::types::PathElement::Edge(id.clone().into()),
        }
    }
}

impl IcWireValue {
    /// Project a GQL runtime value into an IC-stable wire value (lossless).
    pub fn try_from_value(value: &Value) -> Result<Self, WireError> {
        Ok(match value {
            Value::Null => Self::Null,
            Value::Bool(v) => Self::Bool(*v),
            Value::Int8(v) => Self::Int8(*v),
            Value::Int16(v) => Self::Int16(*v),
            Value::Int32(v) => Self::Int32(*v),
            Value::Int64(v) => Self::Int64(*v),
            Value::Int128(v) => Self::Int128(*v),
            Value::Int256(v) => Self::Int256(v.0.to_le_bytes().to_vec()),
            Value::Uint8(v) => Self::Uint8(*v),
            Value::Uint16(v) => Self::Uint16(*v),
            Value::Uint32(v) => Self::Uint32(*v),
            Value::Uint64(v) => Self::Uint64(*v),
            Value::Uint128(v) => Self::Uint128(*v),
            Value::Uint256(v) => Self::Uint256(v.0.to_le_bytes().to_vec()),
            Value::Float16(v) => Self::Float16(v.to_bits()),
            Value::Float32(v) => Self::Float32(*v),
            Value::Float64(v) => Self::Float64(*v),
            Value::Float128(_) => Self::Float128(canonical_float128_bytes(value)?),
            Value::Float256(v) => Self::Float256(v.to_le_bytes().to_vec()),
            Value::Decimal(v) => Self::Decimal(v.0.serialize().to_vec()),
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
                Self::Path(elements.iter().map(IcWirePathElement::from).collect())
            }
            Value::Record(fields) => Self::Record(
                fields
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), Self::try_from_value(v)?)))
                    .collect::<Result<Vec<_>, WireError>>()?,
            ),
        })
    }

    fn extension_leaf(ext: &dyn ExtensionValue) -> Result<Self, WireError> {
        let wrapped = Value::Extension(ext.clone_box());
        let payload = wrapped.to_binary_bytes()?;
        Ok(Self::ExtensionLeaf {
            type_name: ext.type_name().to_owned(),
            payload,
        })
    }

    /// Convert wire data into a GQL [`Value`] using [`ic_extension_decode`].
    pub fn try_into_value(&self) -> Result<Value, WireError> {
        Ok(match self {
            Self::Null => Value::Null,
            Self::Bool(v) => Value::Bool(*v),
            Self::Int8(v) => Value::Int8(*v),
            Self::Int16(v) => Value::Int16(*v),
            Self::Int32(v) => Value::Int32(*v),
            Self::Int64(v) => Value::Int64(*v),
            Self::Uint8(v) => Value::Uint8(*v),
            Self::Uint16(v) => Value::Uint16(*v),
            Self::Uint32(v) => Value::Uint32(*v),
            Self::Uint64(v) => Value::Uint64(*v),
            Self::Int128(v) => Value::Int128(*v),
            Self::Uint128(v) => Value::Uint128(*v),
            Self::Int256(bytes) => {
                Value::Int256(gleaph_gql::types::Int256(ethnum::I256::from_le_bytes(
                    <[u8; 32]>::try_from(bytes.as_slice())
                        .map_err(|_| WireError::InvalidNumericString { kind: "Int256" })?,
                )))
            }
            Self::Uint256(bytes) => {
                Value::Uint256(gleaph_gql::types::Uint256(ethnum::U256::from_le_bytes(
                    <[u8; 32]>::try_from(bytes.as_slice())
                        .map_err(|_| WireError::InvalidNumericString { kind: "Uint256" })?,
                )))
            }
            Self::Float16(bits) => Value::Float16(half::f16::from_bits(*bits)),
            Self::Float32(v) => Value::Float32(*v),
            Self::Float64(v) => Value::Float64(*v),
            Self::Float128(bytes) => {
                let bits = <[u8; 16]>::try_from(bytes.as_slice())
                    .map_err(|_| WireError::InvalidFloatRepresentation { kind: "Float128" })?;
                let mut encoded = Vec::with_capacity(17);
                encoded.push(31);
                encoded.extend_from_slice(&bits);
                Value::from_binary_bytes_with_extensions(&encoded, ic_extension_decode())?
            }
            Self::Float256(bytes) => {
                let bytes = <[u8; 32]>::try_from(bytes.as_slice())
                    .map_err(|_| WireError::InvalidFloatRepresentation { kind: "Float256" })?;
                Value::Float256(f256::from_le_bytes(bytes))
            }
            Self::Decimal(bytes) => Value::Decimal(gleaph_gql::types::Decimal(
                rust_decimal::Decimal::deserialize(
                    <[u8; 16]>::try_from(bytes.as_slice())
                        .map_err(|_| WireError::InvalidNumericString { kind: "Decimal" })?,
                ),
            )),
            Self::Text(v) => Value::Text(v.clone()),
            Self::Bytes(v) => Value::Bytes(v.clone()),
            Self::Date(v) => Value::Date(*v),
            Self::Time(v) => Value::Time(*v),
            Self::LocalTime(v) => Value::LocalTime(*v),
            Self::DateTime { seconds, nanos } => Value::DateTime(*seconds, *nanos),
            Self::LocalDateTime { seconds, nanos } => Value::LocalDateTime(*seconds, *nanos),
            Self::ZonedDateTime {
                seconds,
                nanos,
                offset_seconds,
            } => Value::ZonedDateTime(*seconds, *nanos, *offset_seconds),
            Self::ZonedTime {
                nanos,
                offset_seconds,
            } => Value::ZonedTime(*nanos, *offset_seconds),
            Self::Duration { months, nanos } => Value::Duration(*months, *nanos),
            Self::Principal(p) => Value::Extension(Box::new(PrincipalValue(*p))),
            Self::ExtensionLeaf { type_name, payload } => {
                Value::from_binary_bytes_with_extensions(payload, ic_extension_decode()).map_err(
                    |e| WireError::ExtensionLeafDecode {
                        type_name: type_name.clone(),
                        source: e,
                    },
                )?
            }
            Self::ValueBinary(bytes) => {
                Value::from_binary_bytes_with_extensions(bytes, ic_extension_decode())
                    .map_err(WireError::OpaqueDecode)?
            }
            Self::List(items) => Value::List(
                items
                    .iter()
                    .map(|i| i.try_into_value())
                    .collect::<Result<_, _>>()?,
            ),
            Self::Path(elements) => Value::Path(
                elements
                    .iter()
                    .map(gleaph_gql::types::PathElement::from)
                    .collect(),
            ),
            Self::Record(fields) => Value::Record(
                fields
                    .iter()
                    .map(|(k, v)| Ok((k.clone(), v.try_into_value()?)))
                    .collect::<Result<Vec<_>, WireError>>()?,
            ),
        })
    }
}

fn canonical_float128_bytes(value: &Value) -> Result<Vec<u8>, WireError> {
    let encoded = value.to_binary_bytes()?;
    if encoded.len() != 17 || encoded[0] != 31 {
        return Err(WireError::InvalidFloatRepresentation { kind: "Float128" });
    }
    Ok(encoded[1..].to_vec())
}

impl TryFrom<&Value> for IcWireValue {
    type Error = WireError;

    fn try_from(value: &Value) -> Result<Self, Self::Error> {
        Self::try_from_value(value)
    }
}

impl TryFrom<Value> for IcWireValue {
    type Error = WireError;

    fn try_from(value: Value) -> Result<Self, Self::Error> {
        Self::try_from_value(&value)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn principal_round_trips() {
        let p = Principal::from_text("aaaaa-aa").expect("mgmt");
        let v = principal_to_value(p);
        let w = IcWireValue::try_from_value(&v).expect("to wire");
        assert_eq!(w, IcWireValue::Principal(p));
        let back = w.try_into_value().expect("from wire");
        assert_eq!(back, v);
    }

    #[test]
    fn extension_leaf_round_trips_unknown_shape_via_payload() {
        let p = Principal::from_text("2vxsx-fae").expect("user");
        let v = principal_to_value(p);
        let w = IcWireValue::try_from_value(&v).expect("wire");
        match &w {
            IcWireValue::Principal(_) => {}
            other => panic!("expected Principal variant, got {other:?}"),
        }
    }

    #[test]
    fn nested_record_with_principal() {
        let p = Principal::from_text("aaaaa-aa").expect("mgmt");
        let v = Value::Record(vec![
            ("who".to_owned(), principal_to_value(p)),
            ("n".to_owned(), Value::Int64(7)),
        ]);
        let w = IcWireValue::try_from_value(&v).expect("wire");
        let back = w.try_into_value().expect("back");
        assert_eq!(back, v);
    }

    #[test]
    fn path_elements_round_trip_as_opaque_bytes() {
        let v = Value::Path(vec![
            gleaph_gql::types::PathElement::Vertex(vec![1, 2, 3].into()),
            gleaph_gql::types::PathElement::Edge(vec![4, 5, 6, 7].into()),
        ]);
        let w = IcWireValue::try_from_value(&v).expect("wire");
        assert_eq!(
            w,
            IcWireValue::Path(vec![
                IcWirePathElement::Vertex(vec![1, 2, 3]),
                IcWirePathElement::Edge(vec![4, 5, 6, 7]),
            ])
        );
        let back = w.try_into_value().expect("back");
        assert_eq!(back, v);
    }

    #[test]
    fn non_native_float_variants_round_trip_exactly() {
        let f16 = Value::Float16(half::f16::from_bits(0x8001));
        let w16 = IcWireValue::try_from_value(&f16).expect("to wire");
        assert_eq!(w16, IcWireValue::Float16(0x8001));
        assert_eq!(w16.try_into_value().expect("from wire"), f16);

        let v = Value::Float128(1.25f128);
        let w = IcWireValue::try_from_value(&v).expect("to wire");
        let IcWireValue::Float128(blob) = &w else {
            panic!("expected Float128");
        };
        assert_eq!(blob.len(), 16);
        let back = w.try_into_value().expect("from wire");
        assert_eq!(back, v);
    }

    #[test]
    fn big_integer_and_decimal_round_trip_as_packed_bytes() {
        let original = Value::Record(vec![
            (
                "big".to_owned(),
                Value::Int256(gleaph_gql::types::Int256(ethnum::I256::from(-7))),
            ),
            (
                "unsigned".to_owned(),
                Value::Uint256(gleaph_gql::types::Uint256(ethnum::U256::from(42u64))),
            ),
            (
                "price".to_owned(),
                Value::Decimal(gleaph_gql::types::Decimal(
                    "1.25".parse::<rust_decimal::Decimal>().unwrap(),
                )),
            ),
        ]);
        let wire = IcWireValue::try_from_value(&original).expect("to wire");
        match &wire {
            IcWireValue::Record(fields) => {
                assert!(matches!(&fields[0].1, IcWireValue::Int256(bytes) if bytes.len() == 32));
                assert!(matches!(&fields[1].1, IcWireValue::Uint256(bytes) if bytes.len() == 32));
                assert!(matches!(&fields[2].1, IcWireValue::Decimal(bytes) if bytes.len() == 16));
            }
            other => panic!("expected Record, got {other:?}"),
        }
        assert_eq!(wire.try_into_value().expect("from wire"), original);
    }

    #[test]
    fn gql_params_blob_empty_round_trip() {
        assert!(decode_gql_params_blob(&[]).expect("decode").is_empty());
        let enc = encode_gql_params_blob(vec![]).expect("encode");
        assert!(!enc.is_empty());
        assert!(decode_gql_params_blob(&enc).expect("decode").is_empty());
    }

    #[test]
    fn gql_params_blob_round_trip_with_principal() {
        let p = Principal::from_text("aaaaa-aa").expect("mgmt");
        let original = vec![
            ("n".into(), Value::Int64(7)),
            ("who".into(), principal_to_value(p)),
        ];
        let bytes = encode_gql_params_blob(original.clone()).expect("encode");
        let map = decode_gql_params_blob(&bytes).expect("decode");
        assert_eq!(map.len(), 2);
        assert_eq!(map.get("n"), Some(&Value::Int64(7)));
        assert_eq!(map.get("who"), Some(&principal_to_value(p)));
    }

    #[test]
    fn gql_params_blob_rejects_non_record_top_level() {
        let bytes = Value::Int64(1).to_binary_bytes().expect("encode scalar");
        let err = decode_gql_params_blob(&bytes).expect_err("expected error");
        assert_eq!(err, WireError::ParamsTopLevelNotRecord);
    }
}
