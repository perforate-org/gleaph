//! Shared property-index API types and sortable value-key encoding.

use gleaph_gql::Value;
use gleaph_gql::numeric_order::{NumericOrderError, normalized_numeric_parts};
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
const INDEX_KEY_NUMERIC: u8 = 2;
const INDEX_KEY_TEXT: u8 = 6;
const INDEX_KEY_BYTES: u8 = 7;
const INDEX_KEY_TEMPORAL: u8 = 8;
const INDEX_KEY_LIST: u8 = 9;
const INDEX_KEY_RECORD: u8 = 10;
const ITEM_MARKER: u8 = 1;
const END_MARKER: u8 = 0;

pub fn value_to_index_key_bytes(value: &Value) -> Result<Option<Vec<u8>>, ValueIndexKeyError> {
    if matches!(value, Value::Null) {
        return Ok(None);
    }
    let mut out = Vec::new();
    encode_value_key(value, &mut out)?;
    Ok(Some(out))
}

fn encode_value_key(value: &Value, out: &mut Vec<u8>) -> Result<(), ValueIndexKeyError> {
    match value {
        Value::Null => out.push(END_MARKER),
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
        | Value::Decimal(_)
        | Value::Float16(_)
        | Value::Float32(_)
        | Value::Float64(_)) => {
            out.push(INDEX_KEY_NUMERIC);
            encode_numeric_key(value, out)?;
        }
        Value::Text(value) => {
            out.push(INDEX_KEY_TEXT);
            push_escaped_index_bytes(out, value.as_bytes());
        }
        Value::Bytes(value) => {
            out.push(INDEX_KEY_BYTES);
            push_escaped_index_bytes(out, value);
        }
        Value::Date(value) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 1]);
            encode_signed_order_i64(out, i64::from(*value));
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
            encode_signed_order_i64(out, *seconds);
            out.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::LocalDateTime(seconds, nanos) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 5]);
            encode_signed_order_i64(out, *seconds);
            out.extend_from_slice(&nanos.to_be_bytes());
        }
        Value::ZonedDateTime(seconds, nanos, _) => {
            out.extend_from_slice(&[INDEX_KEY_TEMPORAL, 6]);
            encode_signed_order_i64(out, *seconds);
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
            encode_signed_order_i64(out, i64::from(*months));
            encode_signed_order_i64(out, *nanos);
        }
        Value::List(values) => encode_list_key(values, out)?,
        Value::Record(fields) => encode_record_key(fields, out)?,
        Value::Path(_) | Value::Extension(_) => return Err(ValueIndexKeyError::UnsupportedValue),
        #[cfg(feature = "f128")]
        Value::Float128(_) => {
            out.push(INDEX_KEY_NUMERIC);
            encode_numeric_key(value, out)?;
        }
        #[cfg(feature = "f256")]
        Value::Float256(_) => {
            out.push(INDEX_KEY_NUMERIC);
            encode_numeric_key(value, out)?;
        }
    }
    Ok(())
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

fn encode_numeric_key(value: &Value, out: &mut Vec<u8>) -> Result<(), ValueIndexKeyError> {
    let Some(parts) = normalized_numeric_parts(value).map_err(index_key_error_from_numeric)? else {
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

fn encode_list_key(values: &[Value], out: &mut Vec<u8>) -> Result<(), ValueIndexKeyError> {
    out.push(INDEX_KEY_LIST);
    for value in values {
        out.push(ITEM_MARKER);
        encode_value_key(value, out)?;
    }
    out.push(END_MARKER);
    Ok(())
}

fn encode_record_key(
    fields: &[(String, Value)],
    out: &mut Vec<u8>,
) -> Result<(), ValueIndexKeyError> {
    out.push(INDEX_KEY_RECORD);
    let mut sorted_fields: Vec<_> = fields.iter().collect();
    sorted_fields.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, value) in sorted_fields {
        out.push(ITEM_MARKER);
        push_escaped_index_bytes(out, name.as_bytes());
        encode_value_key(value, out)?;
    }
    out.push(END_MARKER);
    Ok(())
}

fn index_key_error_from_numeric(error: NumericOrderError) -> ValueIndexKeyError {
    match error {
        NumericOrderError::NonFiniteFloat => ValueIndexKeyError::NonFiniteFloat,
        NumericOrderError::UnsupportedValue => ValueIndexKeyError::UnsupportedValue,
    }
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
    fn index_key_unifies_equal_float_decimal_values() {
        let keys = [Value::Float64(1.5), Value::Float32(1.5), decimal("1.5")].map(key);
        assert!(keys.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn index_key_unifies_numeric_zero_values() {
        let keys = [
            Value::Float64(-0.0),
            Value::Float64(0.0),
            Value::Int64(0),
            decimal("0.00"),
        ]
        .map(key);
        assert!(keys.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[test]
    fn index_key_distinguishes_exact_binary_and_decimal_tenths() {
        let float_key = key(Value::Float64(0.1));
        let decimal_key = key(decimal("0.1"));
        assert_ne!(float_key, decimal_key);
        assert!(decimal_key < float_key);
    }

    #[test]
    fn index_key_order_matches_mixed_numeric_comparison() {
        use gleaph_gql::value_cmp::compare_values;
        use std::cmp::Ordering;

        let ordered = [
            Value::Float64(-1.5),
            Value::Int64(-1),
            Value::Float64(-0.0),
            decimal("0.5"),
            Value::Int64(1),
            Value::Float32(1.5),
        ];
        for pair in ordered.windows(2) {
            assert_eq!(compare_values(&pair[0], &pair[1]), Some(Ordering::Less));
            assert!(key(pair[0].clone()) < key(pair[1].clone()));
        }
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
    fn index_key_unifies_equal_list_values() {
        let left = key(Value::List(vec![Value::Int64(1), Value::Text("a".into())]));
        let right = key(Value::List(vec![Value::Uint8(1), Value::Text("a".into())]));
        assert_eq!(left, right);
    }

    #[test]
    fn index_key_orders_lists_lexicographically() {
        let ordered = [
            Value::List(vec![]),
            Value::List(vec![Value::Int64(1)]),
            Value::List(vec![Value::Int64(1), Value::Int64(2)]),
            Value::List(vec![Value::Int64(2)]),
        ]
        .map(key);
        for pair in ordered.windows(2) {
            assert!(pair[0] < pair[1], "list keys out of order: {pair:?}");
        }
    }

    #[test]
    fn index_key_orders_nested_list_record_values() {
        let left = key(Value::List(vec![Value::Record(vec![(
            "a".into(),
            Value::Int64(1),
        )])]));
        let right = key(Value::List(vec![Value::Record(vec![(
            "a".into(),
            Value::Int64(2),
        )])]));
        assert!(left < right);
    }

    #[test]
    fn index_key_unifies_records_independent_of_field_order() {
        let left = key(Value::Record(vec![
            ("a".into(), Value::Int64(1)),
            ("b".into(), Value::Int64(2)),
        ]));
        let right = key(Value::Record(vec![
            ("b".into(), Value::Int64(2)),
            ("a".into(), Value::Int64(1)),
        ]));
        assert_eq!(left, right);
    }

    #[test]
    fn index_key_orders_records_by_field_name_and_value() {
        let ordered = [
            Value::Record(vec![("a".into(), Value::Int64(1))]),
            Value::Record(vec![("a".into(), Value::Int64(2))]),
            Value::Record(vec![("b".into(), Value::Int64(1))]),
            Value::Record(vec![
                ("b".into(), Value::Int64(1)),
                ("c".into(), Value::Int64(2)),
            ]),
        ]
        .map(key);
        for pair in ordered.windows(2) {
            assert!(pair[0] < pair[1], "record keys out of order: {pair:?}");
        }

        assert!(
            key(Value::Record(vec![("a".into(), Value::Int64(1))]))
                < key(Value::Record(vec![
                    ("a".into(), Value::Int64(1)),
                    ("b".into(), Value::Int64(2)),
                ]))
        );
    }

    #[test]
    fn index_key_encodes_nested_null_values() {
        assert!(
            value_to_index_key_bytes(&Value::List(vec![Value::Null]))
                .unwrap()
                .is_some()
        );
        assert!(
            value_to_index_key_bytes(&Value::Record(vec![("a".into(), Value::Null)]))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn index_key_reports_null_and_unsupported_values() {
        assert_eq!(value_to_index_key_bytes(&Value::Null).unwrap(), None);
        assert_eq!(
            value_to_index_key_bytes(&Value::Float64(f64::NAN)),
            Err(ValueIndexKeyError::NonFiniteFloat)
        );
        assert_eq!(
            value_to_index_key_bytes(&Value::Float64(f64::INFINITY)),
            Err(ValueIndexKeyError::NonFiniteFloat)
        );
        assert_eq!(
            value_to_index_key_bytes(&Value::List(vec![Value::Float64(f64::NAN)])),
            Err(ValueIndexKeyError::NonFiniteFloat)
        );
        assert_eq!(
            value_to_index_key_bytes(&Value::List(vec![Value::Path(vec![])])),
            Err(ValueIndexKeyError::UnsupportedValue)
        );
        assert_eq!(
            value_to_index_key_bytes(&Value::Record(vec![("a".into(), Value::Path(vec![]))])),
            Err(ValueIndexKeyError::UnsupportedValue)
        );
    }
}
