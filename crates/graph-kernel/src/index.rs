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
const INDEX_KEY_INTEGER: u8 = 2;
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
        | Value::Uint256(_)) => {
            out.push(INDEX_KEY_INTEGER);
            encode_index_integer(value, &mut out)?;
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
        Value::Decimal(_)
        | Value::List(_)
        | Value::Record(_)
        | Value::Path(_)
        | Value::Extension(_) => return Err(ValueIndexKeyError::UnsupportedValue),
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

fn encode_index_integer(value: &Value, out: &mut Vec<u8>) -> Result<(), ValueIndexKeyError> {
    if value.is_signed_int() {
        let signed = value
            .as_i256()
            .ok_or(ValueIndexKeyError::UnsupportedValue)?;
        if signed.is_negative() {
            out.push(0);
            out.extend_from_slice(&signed.to_be_bytes());
            return Ok(());
        }
    }

    let unsigned = value
        .as_u256()
        .ok_or(ValueIndexKeyError::UnsupportedValue)?;
    if ethnum::I256::try_from(unsigned).is_ok() {
        out.push(1);
    } else {
        out.push(2);
    }
    out.extend_from_slice(&unsigned.to_be_bytes());
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::types::{Decimal, Int256, Uint256};

    #[test]
    fn index_key_orders_mixed_integers_by_numeric_value() {
        let values = [
            Value::Int64(-2),
            Value::Int8(-1),
            Value::Uint8(0),
            Value::Int256(Int256::new(ethnum::I256::from(1))),
            Value::Uint64(2),
            Value::Uint256(Uint256::new(ethnum::I256::MAX.as_u256() + 1u128)),
        ];
        let keys: Vec<_> = values
            .iter()
            .map(|v| value_to_index_key_bytes(v).unwrap().unwrap())
            .collect();
        for pair in keys.windows(2) {
            assert!(pair[0] < pair[1], "keys out of order: {pair:?}");
        }
    }

    #[test]
    fn index_key_unifies_equal_integer_values_across_widths() {
        let keys = [
            Value::Int64(5),
            Value::Uint8(5),
            Value::Int256(Int256::new(ethnum::I256::from(5))),
            Value::Uint256(Uint256::new(ethnum::U256::from(5u8))),
        ]
        .map(|v| value_to_index_key_bytes(&v).unwrap().unwrap());
        assert!(keys.windows(2).all(|pair| pair[0] == pair[1]));
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
            value_to_index_key_bytes(&Value::Decimal(Decimal::from_i64(1))),
            Err(ValueIndexKeyError::UnsupportedValue)
        );
        assert_eq!(
            value_to_index_key_bytes(&Value::Float64(f64::NAN)),
            Err(ValueIndexKeyError::NonFiniteFloat)
        );
    }
}
