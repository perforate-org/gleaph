//! Shared property-index API types and sortable value-key encoding.

use gleaph_gql::Value;
use std::fmt;

#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub struct PostingHit {
    pub shard_id: u64,
    pub vertex_id: u32,
}

/// Compare encoded property values using the same lexicographic order as index posting keys.
#[derive(Clone, Debug, PartialEq, Eq, candid::CandidType, serde::Deserialize, serde::Serialize)]
pub enum PostingRangeRequest {
    Ge(Vec<u8>),
    Gt(Vec<u8>),
    Le(Vec<u8>),
    Lt(Vec<u8>),
}

/// Error returned when a [`Value`] cannot be encoded as an order-preserving
/// property-index key.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ValueIndexKeyError {
    UnsupportedValue,
    NonFiniteFloat,
}

impl fmt::Display for ValueIndexKeyError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnsupportedValue => write!(f, "value is not supported by property index keys"),
            Self::NonFiniteFloat => {
                write!(
                    f,
                    "non-finite float is not supported by property index keys"
                )
            }
        }
    }
}

impl std::error::Error for ValueIndexKeyError {}

const INDEX_KEY_BOOL: u8 = 1;
const INDEX_KEY_EXACT_NUMERIC: u8 = 2;
const INDEX_KEY_FLOAT16: u8 = 3;
const INDEX_KEY_FLOAT32: u8 = 4;
const INDEX_KEY_FLOAT64: u8 = 5;
const INDEX_KEY_TEXT: u8 = 6;
const INDEX_KEY_BYTES: u8 = 7;
const INDEX_KEY_TEMPORAL: u8 = 8;

pub fn value_to_index_key_bytes(value: &Value) -> Result<Option<Vec<u8>>, ValueIndexKeyError> {
    let mut out = Vec::new();
    match value {
        Value::Null => return Ok(None),
        Value::Bool(value) => {
            out.push(INDEX_KEY_BOOL);
            out.push(u8::from(*value));
        }
        value @ (Value::Int8(_)
        | Value::Int16(_)
        | Value::Int32(_)
        | Value::Int64(_)
        | Value::Int128(_)
        | Value::Int256(_)
        | Value::Uint8(_)
        | Value::Uint16(_)
        | Value::Uint32(_)
        | Value::Uint64(_)
        | Value::Uint128(_)
        | Value::Uint256(_)
        | Value::Decimal(_)) => {
            out.push(INDEX_KEY_EXACT_NUMERIC);
            encode_exact_numeric_key(value, &mut out)?;
        }
        Value::Float16(value) => {
            if !value.is_finite() {
                return Err(ValueIndexKeyError::NonFiniteFloat);
            }
            out.push(INDEX_KEY_FLOAT16);
            out.extend_from_slice(&encode_float_order_u16(value.to_bits()));
        }
        Value::Float32(value) => {
            if !value.is_finite() {
                return Err(ValueIndexKeyError::NonFiniteFloat);
            }
            out.push(INDEX_KEY_FLOAT32);
            out.extend_from_slice(&encode_float_order_u32(value.to_bits()));
        }
        Value::Float64(value) => {
            if !value.is_finite() {
                return Err(ValueIndexKeyError::NonFiniteFloat);
            }
            out.push(INDEX_KEY_FLOAT64);
            out.extend_from_slice(&encode_float_order_u64(value.to_bits()));
        }
        Value::Text(value) => {
            out.push(INDEX_KEY_TEXT);
            push_escaped_index_bytes(&mut out, value.as_bytes());
        }
        Value::Bytes(value) => {
            out.push(INDEX_KEY_BYTES);
            push_escaped_index_bytes(&mut out, value);
        }
        Value::Date(value) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 1]);
            encode_signed_order_i64(&mut out, i64::from(*value));
        }
        Value::Time(value) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 2]);
            out.extend_from_slice(&value.to_be_bytes());
        }
        Value::LocalTime(value) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 3]);
            out.extend_from_slice(&value.to_be_bytes());
        }
        Value::DateTime(seconds, nanos) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 4]);
            encode_signed_order_i64(&mut out, *seconds);
            out.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::LocalDateTime(seconds, nanos) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 5]);
            encode_signed_order_i64(&mut out, *seconds);
            out.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::ZonedDateTime(seconds, nanos, _) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 6]);
            encode_signed_order_i64(&mut out, *seconds);
            out.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::ZonedTime(nanos, tz_seconds) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 7]);
            let utc = (*nanos as i64 - (*tz_seconds as i64) * 1_000_000_000)
                .rem_euclid(86_400_000_000_000);
            out.extend_from_slice(&(utc as u64).to_be_bytes());
        }
        Value::Duration(months, nanos) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 8]);
            encode_signed_order_i64(&mut out, i64::from(*months));
            encode_signed_order_i64(&mut out, *nanos);
        }
        Value::List(_) | Value::Record(_) | Value::Path(_) | Value::Extension(_) => {
            return Err(ValueIndexKeyError::UnsupportedValue);
        }
        #[cfg(feature = "f128")]
        Value::Float128(_) => return Err(ValueIndexKeyError::UnsupportedValue),
        #[cfg(feature = "f256")]
        Value::Float256(_) => return Err(ValueIndexKeyError::UnsupportedValue),
    }
    Ok(Some(out))
}

fn push_escaped_index_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    for byte in bytes {
        if *byte == 0 {
            out.extend_from_slice(&[0, 255]);
        } else {
            out.push(*byte);
        }
    }
    out.extend_from_slice(&[0, 0]);
}

fn encode_signed_order_i64(out: &mut Vec<u8>, value: i64) {
    out.extend_from_slice(&(value ^ i64::MIN).to_be_bytes());
}

fn encode_float_order_u16(bits: u16) -> [u8; 2] {
    let key = if bits & 0x8000 == 0 {
        bits ^ 0x8000
    } else {
        !bits
    };
    key.to_be_bytes()
}

fn encode_float_order_u32(bits: u32) -> [u8; 4] {
    let key = if bits & 0x8000_0000 == 0 {
        bits ^ 0x8000_0000
    } else {
        !bits
    };
    key.to_be_bytes()
}

fn encode_float_order_u64(bits: u64) -> [u8; 8] {
    let key = if bits & 0x8000_0000_0000_0000 == 0 {
        bits ^ 0x8000_0000_0000_0000
    } else {
        !bits
    };
    key.to_be_bytes()
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct ExactNumericParts {
    negative: bool,
    digits: String,
    point_pos: i32,
}

fn encode_exact_numeric_key(value: &Value, out: &mut Vec<u8>) -> Result<(), ValueIndexKeyError> {
    let Some(parts) = exact_numeric_parts(value)? else {
        out.push(1);
        return Ok(());
    };

    let mut magnitude = Vec::with_capacity(4 + parts.digits.len() + 1);
    magnitude.extend_from_slice(&(parts.point_pos ^ i32::MIN).to_be_bytes());
    magnitude.extend_from_slice(parts.digits.as_bytes());
    magnitude.push(0);

    if parts.negative {
        out.push(0);
        out.extend(magnitude.into_iter().map(|byte| !byte));
    } else {
        out.push(2);
        out.extend(magnitude);
    }
    Ok(())
}

fn exact_numeric_parts(value: &Value) -> Result<Option<ExactNumericParts>, ValueIndexKeyError> {
    let (negative, digits, scale) = match value {
        Value::Decimal(value) => {
            let decimal = value.normalize().0;
            let mantissa = decimal.mantissa();
            if mantissa == 0 {
                return Ok(None);
            }
            (
                mantissa < 0,
                mantissa.unsigned_abs().to_string(),
                decimal.scale(),
            )
        }
        value if value.is_signed_int() => {
            let signed = value
                .as_i256()
                .ok_or(ValueIndexKeyError::UnsupportedValue)?;
            if signed == ethnum::I256::ZERO {
                return Ok(None);
            }
            (signed.is_negative(), signed.unsigned_abs().to_string(), 0)
        }
        value if value.is_unsigned_int() => {
            let unsigned = value
                .as_u256()
                .ok_or(ValueIndexKeyError::UnsupportedValue)?;
            if unsigned == ethnum::U256::ZERO {
                return Ok(None);
            }
            (false, unsigned.to_string(), 0)
        }
        _ => return Err(ValueIndexKeyError::UnsupportedValue),
    };

    let point_pos = i32::try_from(digits.len())
        .ok()
        .and_then(|len| len.checked_sub(i32::try_from(scale).ok()?))
        .ok_or(ValueIndexKeyError::UnsupportedValue)?;
    Ok(Some(ExactNumericParts {
        negative,
        digits,
        point_pos,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::types::{Decimal, Int256, Uint256};

    fn key(value: Value) -> Vec<u8> {
        value_to_index_key_bytes(&value).unwrap().unwrap()
    }

    fn decimal(value: &str) -> Value {
        Value::Decimal(Decimal::parse(value).expect("decimal"))
    }

    #[test]
    fn index_key_orders_exact_numeric_values() {
        let values = [
            Value::Int64(-100),
            decimal("-1.5"),
            Value::Int8(-1),
            decimal("-0.01"),
            Value::Uint8(0),
            decimal("0.001"),
            Value::Int256(Int256::new(ethnum::I256::from(1))),
            decimal("1.01"),
            Value::Uint64(10),
            Value::Uint256(Uint256::new(ethnum::I256::MAX.as_u256() + 1u128)),
        ];
        let keys: Vec<_> = values.into_iter().map(key).collect();
        for pair in keys.windows(2) {
            assert!(pair[0] < pair[1], "keys out of order: {pair:?}");
        }
    }

    #[test]
    fn index_key_unifies_equal_exact_numeric_values_across_widths() {
        let keys = [
            decimal("5.0"),
            decimal("5.00"),
            Value::Int64(5),
            Value::Uint8(5),
            Value::Int256(Int256::new(ethnum::I256::from(5))),
            Value::Uint256(Uint256::new(ethnum::U256::from(5u8))),
        ]
        .map(key);
        assert!(keys.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn index_key_orders_decimal_scale_cases() {
        let ordered = [
            decimal("0.09"),
            decimal("0.9"),
            decimal("1.001"),
            decimal("1.01"),
            decimal("1.1"),
            decimal("2"),
            decimal("10"),
            decimal("100"),
        ]
        .map(key);
        for pair in ordered.windows(2) {
            assert!(pair[0] < pair[1], "decimal keys out of order: {pair:?}");
        }
    }

    #[test]
    fn index_key_orders_negative_decimal_magnitudes_in_reverse() {
        let ordered = [
            decimal("-100"),
            decimal("-10"),
            decimal("-1.1"),
            decimal("-1"),
        ]
        .map(key);
        for pair in ordered.windows(2) {
            assert!(
                pair[0] < pair[1],
                "negative decimal keys out of order: {pair:?}"
            );
        }
    }

    #[test]
    fn index_key_orders_text_and_bytes_prefixes() {
        let text_keys = ["", "a", "a\0", "aa"].map(|s| {
            value_to_index_key_bytes(&Value::Text(s.into()))
                .unwrap()
                .unwrap()
        });
        assert!(text_keys[0] < text_keys[1]);
        assert!(text_keys[1] < text_keys[2]);
        assert!(text_keys[2] < text_keys[3]);

        let byte_keys = [vec![], vec![b'a'], vec![b'a', 0], vec![b'a', b'a']]
            .map(|b| value_to_index_key_bytes(&Value::Bytes(b)).unwrap().unwrap());
        assert!(byte_keys[0] < byte_keys[1]);
        assert!(byte_keys[1] < byte_keys[2]);
        assert!(byte_keys[2] < byte_keys[3]);
    }

    #[test]
    fn index_key_reports_null_and_unsupported_values() {
        assert_eq!(value_to_index_key_bytes(&Value::Null).unwrap(), None);
        assert_eq!(
            value_to_index_key_bytes(&Value::Float64(f64::NAN)),
            Err(ValueIndexKeyError::NonFiniteFloat)
        );
        assert_eq!(
            value_to_index_key_bytes(&Value::List(vec![])),
            Err(ValueIndexKeyError::UnsupportedValue)
        );
    }
}
