//! Graph-owned scalar codec for fixed-width inline edge payloads (ADR 0034 Slices 20-22).
//!
//! Centralizes `Value <-> fixed-width bytes` for the raw scalar encodings that can be named
//! inline by Router schema. Weight encodings, raw opaque bytes, and vectors are intentionally
//! outside this codec: they are owned by traversal/predicate or admin paths with distinct
//! contracts.

use gleaph_gql::Value;
use gleaph_graph_kernel::entry::{
    DecodedEdgePayload, EdgePayloadEncoding, EdgePayloadProfile, EdgePayloadProfileError,
    decode_edge_payload,
};
use half::f16;
use std::fmt;

/// Error encoding or decoding a scalar inline edge payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EdgePayloadScalarCodecError {
    /// The profile uses an encoding this scalar codec does not own (weight, vector, raw bytes).
    UnsupportedEncoding,
    /// The value kind is not compatible with the target scalar encoding.
    InvalidValueKind { expected: &'static str },
    /// An integer value does not fit in the target signed/unsigned width.
    IntegerOverflow,
    /// A signed integer is negative and cannot be stored in an unsigned target.
    NegativeToUnsigned,
    /// A float value is not finite or overflows the target IEEE width.
    NonFiniteFloat,
    /// `FIXED32` / `FIXED64` requires an exact-length byte string.
    FixedByteLengthMismatch { expected: usize, actual: usize },
    /// The decoded payload width does not match the profile.
    WidthMismatch { expected: usize, actual: usize },
    /// Decode produced a payload shape the scalar codec cannot map back to `Value`.
    UnsupportedDecodedShape,
}

impl fmt::Display for EdgePayloadScalarCodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedEncoding => write!(f, "unsupported scalar payload encoding"),
            Self::InvalidValueKind { expected } => {
                write!(f, "value kind is not valid for {expected} payload")
            }
            Self::IntegerOverflow => write!(f, "integer value overflows target payload width"),
            Self::NegativeToUnsigned => {
                write!(f, "negative integer cannot be stored in unsigned payload")
            }
            Self::NonFiniteFloat => write!(f, "non-finite or overflowing float value"),
            Self::FixedByteLengthMismatch { expected, actual } => write!(
                f,
                "fixed-width payload expects {expected} bytes, got {actual}"
            ),
            Self::WidthMismatch { expected, actual } => write!(
                f,
                "payload byte width mismatch: expected {expected}, got {actual}"
            ),
            Self::UnsupportedDecodedShape => {
                write!(f, "decoded payload shape is not a supported scalar")
            }
        }
    }
}

impl std::error::Error for EdgePayloadScalarCodecError {}

impl From<EdgePayloadProfileError> for EdgePayloadScalarCodecError {
    fn from(err: EdgePayloadProfileError) -> Self {
        match err {
            EdgePayloadProfileError::WidthEncodingMismatch => Self::WidthMismatch {
                expected: 0,
                actual: 0,
            },
            _ => Self::UnsupportedEncoding,
        }
    }
}

/// Encode a single GQL scalar value into the exact fixed-width bytes for an inline profile.
///
/// Integral coercion is lossless: any GQL integer width is accepted when the value is exactly
/// representable in the target, with no wrapping or truncation. Unsigned targets reject negative
/// values. Float targets accept any finite numeric input and round to the target IEEE width.
/// `FIXED32` / `FIXED64` accept only a `Value::Bytes` of the exact required length.
pub fn encode_edge_payload_scalar(
    profile: &EdgePayloadProfile,
    value: &Value,
) -> Result<Vec<u8>, EdgePayloadScalarCodecError> {
    if matches!(value, Value::Null) {
        return Err(EdgePayloadScalarCodecError::InvalidValueKind {
            expected: "non-null scalar",
        });
    }

    profile
        .validate()
        .map_err(EdgePayloadScalarCodecError::from)?;

    let bytes = match &profile.encoding {
        EdgePayloadEncoding::RawU8 => vec![unsigned_to_exact::<u8>(value, "u8")?],
        EdgePayloadEncoding::RawU16 => unsigned_to_exact::<u16>(value, "u16")?
            .to_le_bytes()
            .to_vec(),
        EdgePayloadEncoding::RawU32 => unsigned_to_exact::<u32>(value, "u32")?
            .to_le_bytes()
            .to_vec(),
        EdgePayloadEncoding::RawU64 => unsigned_to_exact::<u64>(value, "u64")?
            .to_le_bytes()
            .to_vec(),
        EdgePayloadEncoding::RawU128 => unsigned_to_exact::<u128>(value, "u128")?
            .to_le_bytes()
            .to_vec(),
        EdgePayloadEncoding::RawI8 => {
            vec![signed_to_exact::<i8>(value, "i8")? as u8]
        }
        EdgePayloadEncoding::RawI16 => signed_to_exact::<i16>(value, "i16")?.to_le_bytes().to_vec(),
        EdgePayloadEncoding::RawI32 => signed_to_exact::<i32>(value, "i32")?.to_le_bytes().to_vec(),
        EdgePayloadEncoding::RawI64 => signed_to_exact::<i64>(value, "i64")?.to_le_bytes().to_vec(),
        EdgePayloadEncoding::RawI128 => signed_to_exact::<i128>(value, "i128")?
            .to_le_bytes()
            .to_vec(),
        EdgePayloadEncoding::F16 => finite_f16(value)?.to_le_bytes().to_vec(),
        EdgePayloadEncoding::F32 => finite_f32(value)?.to_le_bytes().to_vec(),
        EdgePayloadEncoding::F64 => finite_f64(value)?.to_le_bytes().to_vec(),
        EdgePayloadEncoding::RawFixed32 => fixed_bytes(value, 32)?,
        EdgePayloadEncoding::RawFixed64 => fixed_bytes(value, 64)?,
        _ => return Err(EdgePayloadScalarCodecError::UnsupportedEncoding),
    };

    let expected = usize::from(profile.required_byte_width());
    if bytes.len() != expected {
        return Err(EdgePayloadScalarCodecError::WidthMismatch {
            expected,
            actual: bytes.len(),
        });
    }
    Ok(bytes)
}

/// Decode fixed-width payload bytes into the exact GQL scalar `Value` for an inline profile.
pub fn decode_edge_payload_scalar(
    profile: &EdgePayloadProfile,
    bytes: &[u8],
) -> Result<Value, EdgePayloadScalarCodecError> {
    profile
        .validate()
        .map_err(EdgePayloadScalarCodecError::from)?;

    let expected = usize::from(profile.required_byte_width());
    let actual = bytes.len();
    if actual != expected {
        return Err(EdgePayloadScalarCodecError::WidthMismatch { expected, actual });
    }

    let decoder = profile
        .prepare()
        .map_err(|_| EdgePayloadScalarCodecError::UnsupportedEncoding)?;
    let decoded = decode_edge_payload(&decoder, bytes)
        .map_err(|_| EdgePayloadScalarCodecError::UnsupportedEncoding)?;

    match decoded {
        DecodedEdgePayload::U8(v) => Ok(Value::Uint8(v)),
        DecodedEdgePayload::U16(v) => Ok(Value::Uint16(v)),
        DecodedEdgePayload::U32(v) => Ok(Value::Uint32(v)),
        DecodedEdgePayload::U64(v) => Ok(Value::Uint64(v)),
        DecodedEdgePayload::U128(v) => Ok(Value::Uint128(v)),
        DecodedEdgePayload::I8(v) => Ok(Value::Int8(v)),
        DecodedEdgePayload::I16(v) => Ok(Value::Int16(v)),
        DecodedEdgePayload::I32(v) => Ok(Value::Int32(v)),
        DecodedEdgePayload::I64(v) => Ok(Value::Int64(v)),
        DecodedEdgePayload::I128(v) => Ok(Value::Int128(v)),
        DecodedEdgePayload::F16(v) => Ok(Value::Float16(v)),
        DecodedEdgePayload::F32(v) => Ok(Value::Float32(v)),
        DecodedEdgePayload::F64(v) => Ok(Value::Float64(v)),
        DecodedEdgePayload::Fixed32(v) => Ok(Value::Bytes(v.to_vec())),
        DecodedEdgePayload::Fixed64(v) => Ok(Value::Bytes(v.to_vec())),
        _ => Err(EdgePayloadScalarCodecError::UnsupportedDecodedShape),
    }
}

fn unsigned_to_exact<T>(
    value: &Value,
    _name: &'static str,
) -> Result<T, EdgePayloadScalarCodecError>
where
    T: TryFrom<u128> + TryFrom<i128>,
{
    let intermediate = unsigned_intermediate(value)?;
    T::try_from(intermediate).map_err(|_| EdgePayloadScalarCodecError::IntegerOverflow)
}

fn signed_to_exact<T>(value: &Value, _name: &'static str) -> Result<T, EdgePayloadScalarCodecError>
where
    T: TryFrom<i128> + TryFrom<u128>,
{
    let intermediate = signed_intermediate(value)?;
    T::try_from(intermediate).map_err(|_| EdgePayloadScalarCodecError::IntegerOverflow)
}

fn unsigned_intermediate(value: &Value) -> Result<u128, EdgePayloadScalarCodecError> {
    match value {
        Value::Uint8(v) => Ok(u128::from(*v)),
        Value::Uint16(v) => Ok(u128::from(*v)),
        Value::Uint32(v) => Ok(u128::from(*v)),
        Value::Uint64(v) => Ok(u128::from(*v)),
        Value::Uint128(v) => Ok(*v),
        Value::Int8(v) => {
            u128::try_from(*v).map_err(|_| EdgePayloadScalarCodecError::NegativeToUnsigned)
        }
        Value::Int16(v) => {
            u128::try_from(*v).map_err(|_| EdgePayloadScalarCodecError::NegativeToUnsigned)
        }
        Value::Int32(v) => {
            u128::try_from(*v).map_err(|_| EdgePayloadScalarCodecError::NegativeToUnsigned)
        }
        Value::Int64(v) => {
            u128::try_from(*v).map_err(|_| EdgePayloadScalarCodecError::NegativeToUnsigned)
        }
        Value::Int128(v) => {
            u128::try_from(*v).map_err(|_| EdgePayloadScalarCodecError::NegativeToUnsigned)
        }
        _ => Err(EdgePayloadScalarCodecError::InvalidValueKind {
            expected: "integer",
        }),
    }
}

fn signed_intermediate(value: &Value) -> Result<i128, EdgePayloadScalarCodecError> {
    match value {
        Value::Int8(v) => Ok(i128::from(*v)),
        Value::Int16(v) => Ok(i128::from(*v)),
        Value::Int32(v) => Ok(i128::from(*v)),
        Value::Int64(v) => Ok(i128::from(*v)),
        Value::Int128(v) => Ok(*v),
        Value::Uint8(v) => Ok(i128::from(*v)),
        Value::Uint16(v) => Ok(i128::from(*v)),
        Value::Uint32(v) => Ok(i128::from(*v)),
        Value::Uint64(v) => Ok(i128::from(*v)),
        Value::Uint128(v) => {
            i128::try_from(*v).map_err(|_| EdgePayloadScalarCodecError::IntegerOverflow)
        }
        _ => Err(EdgePayloadScalarCodecError::InvalidValueKind {
            expected: "integer",
        }),
    }
}

fn finite_f16(value: &Value) -> Result<f16, EdgePayloadScalarCodecError> {
    let f = finite_f32(value)?;
    let rounded = f16::from_f32(f);
    if !rounded.is_finite() {
        return Err(EdgePayloadScalarCodecError::NonFiniteFloat);
    }
    Ok(rounded)
}

fn finite_f32(value: &Value) -> Result<f32, EdgePayloadScalarCodecError> {
    let f = match value {
        Value::Float16(v) => v.to_f32(),
        Value::Float32(v) => *v,
        Value::Float64(v) => *v as f32,
        other if is_integer_value(other) => finite_f32_from_integer(other)?,
        _ => {
            return Err(EdgePayloadScalarCodecError::InvalidValueKind {
                expected: "finite numeric",
            });
        }
    };
    if !f.is_finite() {
        return Err(EdgePayloadScalarCodecError::NonFiniteFloat);
    }
    Ok(f)
}

fn finite_f64(value: &Value) -> Result<f64, EdgePayloadScalarCodecError> {
    let f = match value {
        Value::Float16(v) => f64::from(v.to_f32()),
        Value::Float32(v) => f64::from(*v),
        Value::Float64(v) => *v,
        other if is_integer_value(other) => finite_f64_from_integer(other)?,
        _ => {
            return Err(EdgePayloadScalarCodecError::InvalidValueKind {
                expected: "finite numeric",
            });
        }
    };
    if !f.is_finite() {
        return Err(EdgePayloadScalarCodecError::NonFiniteFloat);
    }
    Ok(f)
}

fn is_integer_value(value: &Value) -> bool {
    matches!(
        value,
        Value::Int8(_)
            | Value::Int16(_)
            | Value::Int32(_)
            | Value::Int64(_)
            | Value::Int128(_)
            | Value::Uint8(_)
            | Value::Uint16(_)
            | Value::Uint32(_)
            | Value::Uint64(_)
            | Value::Uint128(_)
    )
}

fn finite_f32_from_integer(value: &Value) -> Result<f32, EdgePayloadScalarCodecError> {
    let f = if is_unsigned_integer_value(value) {
        unsigned_intermediate(value)? as f32
    } else {
        signed_intermediate(value)? as f32
    };
    if !f.is_finite() {
        return Err(EdgePayloadScalarCodecError::NonFiniteFloat);
    }
    Ok(f)
}

fn finite_f64_from_integer(value: &Value) -> Result<f64, EdgePayloadScalarCodecError> {
    let f = if is_unsigned_integer_value(value) {
        unsigned_intermediate(value)? as f64
    } else {
        signed_intermediate(value)? as f64
    };
    if !f.is_finite() {
        return Err(EdgePayloadScalarCodecError::NonFiniteFloat);
    }
    Ok(f)
}

fn is_unsigned_integer_value(value: &Value) -> bool {
    matches!(
        value,
        Value::Uint8(_)
            | Value::Uint16(_)
            | Value::Uint32(_)
            | Value::Uint64(_)
            | Value::Uint128(_)
    )
}

fn fixed_bytes(value: &Value, expected: usize) -> Result<Vec<u8>, EdgePayloadScalarCodecError> {
    match value {
        Value::Bytes(bytes) if bytes.len() == expected => Ok(bytes.clone()),
        Value::Bytes(bytes) => Err(EdgePayloadScalarCodecError::FixedByteLengthMismatch {
            expected,
            actual: bytes.len(),
        }),
        _ => Err(EdgePayloadScalarCodecError::InvalidValueKind { expected: "bytes" }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile(encoding: EdgePayloadEncoding, byte_width: u16) -> EdgePayloadProfile {
        EdgePayloadProfile {
            encoding,
            byte_width,
        }
    }

    #[test]
    fn round_trip_unsigned_widths() {
        let cases = [
            (Value::Uint8(7), EdgePayloadEncoding::RawU8, 1u16),
            (Value::Uint16(7), EdgePayloadEncoding::RawU16, 2u16),
            (Value::Uint32(7), EdgePayloadEncoding::RawU32, 4u16),
            (Value::Uint64(7), EdgePayloadEncoding::RawU64, 8u16),
            (Value::Uint128(7), EdgePayloadEncoding::RawU128, 16u16),
        ];
        for (value, encoding, width) in cases {
            let p = profile(encoding.clone(), width);
            let bytes = encode_edge_payload_scalar(&p, &value).expect("encode");
            assert_eq!(bytes.len(), width as usize);
            let decoded = decode_edge_payload_scalar(&p, &bytes).expect("decode");
            assert_eq!(decoded, value, "round trip for {encoding:?}");
        }
    }

    #[test]
    fn round_trip_signed_widths() {
        let cases = [
            (Value::Int8(-7), EdgePayloadEncoding::RawI8, 1u16),
            (Value::Int16(-7), EdgePayloadEncoding::RawI16, 2u16),
            (Value::Int32(-7), EdgePayloadEncoding::RawI32, 4u16),
            (Value::Int64(-7), EdgePayloadEncoding::RawI64, 8u16),
            (Value::Int128(-7), EdgePayloadEncoding::RawI128, 16u16),
        ];
        for (value, encoding, width) in cases {
            let p = profile(encoding.clone(), width);
            let bytes = encode_edge_payload_scalar(&p, &value).expect("encode");
            let decoded = decode_edge_payload_scalar(&p, &bytes).expect("decode");
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn round_trip_float_widths() {
        let cases = [
            (
                Value::Float16(f16::from_f32(1.5)),
                EdgePayloadEncoding::F16,
                2u16,
            ),
            (Value::Float32(1.5), EdgePayloadEncoding::F32, 4u16),
            (Value::Float64(1.5), EdgePayloadEncoding::F64, 8u16),
        ];
        for (value, encoding, width) in cases {
            let p = profile(encoding.clone(), width);
            let bytes = encode_edge_payload_scalar(&p, &value).expect("encode");
            let decoded = decode_edge_payload_scalar(&p, &bytes).expect("decode");
            assert_eq!(decoded, value);
        }
    }

    #[test]
    fn integer_coercion_to_wider_unsigned_is_lossless() {
        let p = profile(EdgePayloadEncoding::RawU64, 8);
        let bytes = encode_edge_payload_scalar(&p, &Value::Uint8(7)).expect("encode");
        assert_eq!(bytes, 7u64.to_le_bytes().to_vec());
        assert_eq!(
            decode_edge_payload_scalar(&p, &bytes).expect("decode"),
            Value::Uint64(7)
        );
    }

    #[test]
    fn signed_coercion_to_wider_signed_is_lossless() {
        let p = profile(EdgePayloadEncoding::RawI64, 8);
        let bytes = encode_edge_payload_scalar(&p, &Value::Int8(-7)).expect("encode");
        assert_eq!(bytes, (-7i64).to_le_bytes().to_vec());
    }

    #[test]
    fn negative_value_rejected_for_unsigned_target() {
        let p = profile(EdgePayloadEncoding::RawU8, 1);
        assert_eq!(
            encode_edge_payload_scalar(&p, &Value::Int8(-1)),
            Err(EdgePayloadScalarCodecError::NegativeToUnsigned)
        );
    }

    #[test]
    fn overflow_rejected_without_wrapping() {
        let p = profile(EdgePayloadEncoding::RawU8, 1);
        assert_eq!(
            encode_edge_payload_scalar(&p, &Value::Uint16(256)),
            Err(EdgePayloadScalarCodecError::IntegerOverflow)
        );
    }

    #[test]
    fn unsigned_overflow_rejected_for_signed_target() {
        let p = profile(EdgePayloadEncoding::RawI8, 1);
        assert_eq!(
            encode_edge_payload_scalar(&p, &Value::Uint128(u128::MAX)),
            Err(EdgePayloadScalarCodecError::IntegerOverflow)
        );
    }

    #[test]
    fn non_finite_float_rejected() {
        let p = profile(EdgePayloadEncoding::F32, 4);
        assert_eq!(
            encode_edge_payload_scalar(&p, &Value::Float32(f32::NAN)),
            Err(EdgePayloadScalarCodecError::NonFiniteFloat)
        );
        assert_eq!(
            encode_edge_payload_scalar(&p, &Value::Float32(f32::INFINITY)),
            Err(EdgePayloadScalarCodecError::NonFiniteFloat)
        );
    }

    #[test]
    fn integer_rounds_into_float_target() {
        let p = profile(EdgePayloadEncoding::F32, 4);
        let bytes = encode_edge_payload_scalar(&p, &Value::Int64(7)).expect("encode");
        assert_eq!(
            decode_edge_payload_scalar(&p, &bytes).expect("decode"),
            Value::Float32(7.0)
        );
    }

    #[test]
    fn unsigned128_rounds_into_float_target_without_overflow() {
        // F32 cannot represent u128::MAX as a finite value; the codec rejects it as NonFiniteFloat.
        let p32 = profile(EdgePayloadEncoding::F32, 4);
        assert_eq!(
            encode_edge_payload_scalar(&p32, &Value::Uint128(u128::MAX)),
            Err(EdgePayloadScalarCodecError::NonFiniteFloat)
        );
        // A value well inside the f32 finite range still rounds through the unsigned path.
        let bytes =
            encode_edge_payload_scalar(&p32, &Value::Uint128(u64::MAX.into())).expect("encode");
        let decoded = decode_edge_payload_scalar(&p32, &bytes).expect("decode");
        assert!(matches!(decoded, Value::Float32(f) if f.is_finite()));

        // F64 can represent u128::MAX as a finite rounded value.
        let p64 = profile(EdgePayloadEncoding::F64, 8);
        let bytes = encode_edge_payload_scalar(&p64, &Value::Uint128(u128::MAX)).expect("encode");
        let decoded = decode_edge_payload_scalar(&p64, &bytes).expect("decode");
        assert!(matches!(decoded, Value::Float64(f) if f.is_finite()));
    }

    #[test]
    fn fixed_requires_exact_byte_length() {
        let p32 = profile(EdgePayloadEncoding::RawFixed32, 32);
        let bytes = vec![0u8; 32];
        assert_eq!(
            encode_edge_payload_scalar(&p32, &Value::Bytes(bytes.clone())).expect("encode"),
            bytes
        );

        let p64 = profile(EdgePayloadEncoding::RawFixed64, 64);
        assert_eq!(
            encode_edge_payload_scalar(&p64, &Value::Bytes(vec![0u8; 31])),
            Err(EdgePayloadScalarCodecError::FixedByteLengthMismatch {
                expected: 64,
                actual: 31,
            })
        );
    }

    #[test]
    fn unsupported_encoding_rejected_for_scalar_codec() {
        let p = profile(EdgePayloadEncoding::WeightRawU16, 2);
        assert_eq!(
            encode_edge_payload_scalar(&p, &Value::Uint16(7)),
            Err(EdgePayloadScalarCodecError::UnsupportedEncoding)
        );
    }

    #[test]
    fn null_value_rejected() {
        let p = profile(EdgePayloadEncoding::RawU32, 4);
        assert!(matches!(
            encode_edge_payload_scalar(&p, &Value::Null),
            Err(EdgePayloadScalarCodecError::InvalidValueKind { .. })
        ));
    }
}
