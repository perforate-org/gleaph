//! Sortable property-index key encoding for [`Value`](crate::Value).
//!
//! [`Value::Path`] keys use [`INDEX_KEY_PATH`] and order consistently with
//! [`crate::value_cmp::compare_values`] (same rules as [`PathElement`](crate::types::PathElement) comparison).

use crate::Value;
use crate::numeric_order::{NumericOrderError, normalized_numeric_parts};
use crate::types::PathElement;
use std::fmt;

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
const INDEX_KEY_EXTENSION: u8 = 11;
/// Path values; element order matches [`crate::value_cmp::compare_path_slices`].
const INDEX_KEY_PATH: u8 = 12;
/// Path element discriminant: [`PathElement::Vertex`] sorts before [`PathElement::Edge`].
const PATH_KEY_VERTEX: u8 = 1;
const PATH_KEY_EDGE: u8 = 2;
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

/// Decode a property-index key produced by [`value_to_index_key_bytes`] (supported leaf types only).
pub fn index_key_bytes_to_value(bytes: &[u8]) -> Option<Value> {
    let (&tag, rest) = bytes.split_first()?;
    match tag {
        INDEX_KEY_BOOL => {
            let &byte = rest.first()?;
            Some(Value::Bool(byte != 0))
        }
        INDEX_KEY_TEXT => read_escaped_index_bytes(rest)
            .and_then(|text| String::from_utf8(text).ok().map(Value::Text)),
        INDEX_KEY_BYTES => read_escaped_index_bytes(rest).map(Value::Bytes),
        _ => None,
    }
}

fn read_escaped_index_bytes(rest: &[u8]) -> Option<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == 0 {
            if i + 1 < rest.len() && rest[i + 1] == 0 {
                return Some(out);
            }
            if i + 1 < rest.len() && rest[i + 1] == 255 {
                out.push(0);
                i += 2;
                continue;
            }
            return None;
        }
        out.push(rest[i]);
        i += 1;
    }
    None
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
        Value::Path(elements) => encode_path_key(elements, out)?,
        Value::Extension(ext) => {
            let Some(key) = ext.sortable_index_key() else {
                return Err(ValueIndexKeyError::UnsupportedValue);
            };
            out.push(INDEX_KEY_EXTENSION);
            push_escaped_index_bytes(out, key.domain.as_bytes());
            push_escaped_index_bytes(out, key.bytes.as_ref());
        }
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

fn encode_path_key(elements: &[PathElement], out: &mut Vec<u8>) -> Result<(), ValueIndexKeyError> {
    out.push(INDEX_KEY_PATH);
    for element in elements {
        match element {
            PathElement::Vertex(id) => {
                out.push(PATH_KEY_VERTEX);
                push_escaped_index_bytes(out, id.as_ref());
            }
            PathElement::Edge(id) => {
                out.push(PATH_KEY_EDGE);
                push_escaped_index_bytes(out, id.as_ref());
            }
        }
    }
    out.push(END_MARKER);
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

/// Lexicographic successor of a byte sequence in `memcmp` order.
fn lex_succ_bytes(b: &[u8]) -> Vec<u8> {
    let mut out = b.to_vec();
    for i in (0..out.len()).rev() {
        if out[i] < 255 {
            out[i] += 1;
            out.truncate(i + 1);
            return out;
        }
    }
    // All bytes are 255: append a trailing 0 so the successor is longer and greater.
    out.push(0);
    out
}

/// Derive a finite half-open encoded-key range `[low, high)` that corresponds to the numeric
/// comparison-domain of `property OP value`.
///
/// The returned bounds are encoded bytes that, when used as a `[low, high)` scan over the
/// ordered property-index posting keys for the same `property_id`, return exactly the postings
/// whose stored value satisfies the GQL comparison. Non-numeric values and non-range operators
/// are rejected because their comparison semantics do not map to a single contiguous encoded
/// interval.
///
/// This is the canonical place where GQL value-type tags meet numeric ordering. Router and
/// Property Index must not duplicate tag or numeric-ordering knowledge.
pub fn numeric_range_bounds(
    value: &Value,
    op: crate::ast::CmpOp,
) -> Result<(Vec<u8>, Vec<u8>), ValueIndexKeyError> {
    use crate::ast::CmpOp;

    if !matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge) {
        return Err(ValueIndexKeyError::UnsupportedValue);
    }

    let bound_key = value_to_index_key_bytes(value)?.ok_or(ValueIndexKeyError::UnsupportedValue)?;
    if bound_key.is_empty() || bound_key[0] != INDEX_KEY_NUMERIC {
        return Err(ValueIndexKeyError::UnsupportedValue);
    }

    let numeric_prefix = vec![INDEX_KEY_NUMERIC];
    let after_numeric = lex_succ_bytes(&numeric_prefix);
    match op {
        CmpOp::Ge => Ok((bound_key.clone(), after_numeric)),
        CmpOp::Gt => Ok((lex_succ_bytes(&bound_key), after_numeric)),
        CmpOp::Le => Ok((numeric_prefix, lex_succ_bytes(&bound_key))),
        CmpOp::Lt => Ok((numeric_prefix, bound_key)),
        _ => unreachable!(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Decimal, Int256, Uint256};
    use crate::value::{ExtensionSortableKey, ExtensionValue};
    use std::any::Any;
    use std::borrow::Cow;
    use std::cmp::Ordering;
    use std::fmt;

    fn key(value: Value) -> Vec<u8> {
        value_to_index_key_bytes(&value).unwrap().unwrap()
    }

    fn decimal(value: &str) -> Value {
        Value::Decimal(Decimal::parse(value).expect("decimal"))
    }

    #[derive(Clone, Debug)]
    struct NonOrderableExt;

    impl fmt::Display for NonOrderableExt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "NonOrderableExt")
        }
    }

    impl ExtensionValue for NonOrderableExt {
        fn type_name(&self) -> &str {
            "NonOrderableExt"
        }

        fn clone_box(&self) -> Box<dyn ExtensionValue> {
            Box::new(self.clone())
        }

        fn eq_ext(&self, other: &dyn ExtensionValue) -> bool {
            other.as_any().downcast_ref::<Self>().is_some()
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    #[derive(Clone, Debug)]
    struct OrderableExt {
        domain: &'static str,
        bytes: Vec<u8>,
    }

    impl fmt::Display for OrderableExt {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            write!(f, "OrderableExt({})", self.domain)
        }
    }

    impl ExtensionValue for OrderableExt {
        fn type_name(&self) -> &str {
            "OrderableExt"
        }

        fn clone_box(&self) -> Box<dyn ExtensionValue> {
            Box::new(self.clone())
        }

        fn eq_ext(&self, other: &dyn ExtensionValue) -> bool {
            other
                .as_any()
                .downcast_ref::<Self>()
                .is_some_and(|o| self.domain == o.domain && self.bytes == o.bytes)
        }

        fn cmp_ext(&self, other: &dyn ExtensionValue) -> Option<Ordering> {
            other
                .as_any()
                .downcast_ref::<Self>()
                .and_then(|o| (self.domain == o.domain).then(|| self.bytes.cmp(&o.bytes)))
        }

        fn sortable_index_key(&self) -> Option<ExtensionSortableKey<'_>> {
            Some(ExtensionSortableKey {
                domain: Cow::Borrowed(self.domain),
                bytes: Cow::Borrowed(self.bytes.as_slice()),
            })
        }

        fn as_any(&self) -> &dyn Any {
            self
        }
    }

    fn extension(domain: &'static str, bytes: &[u8]) -> Value {
        Value::Extension(Box::new(OrderableExt {
            domain,
            bytes: bytes.to_vec(),
        }))
    }

    #[test]
    fn public_api_encodes_simple_value() {
        assert!(
            crate::value_to_index_key_bytes(&Value::Text("hello".into()))
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn index_key_text_roundtrips_through_decode() {
        let value = Value::Text("hello\0world".into());
        let key = value_to_index_key_bytes(&value).unwrap().unwrap();
        assert_eq!(index_key_bytes_to_value(&key), Some(value));
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
        use crate::value_cmp::compare_values;

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
    }

    #[test]
    fn index_key_encodes_path_values() {
        use crate::types::PathElement;

        assert!(
            value_to_index_key_bytes(&Value::List(vec![Value::Path(vec![])]))
                .unwrap()
                .is_some()
        );
        assert!(
            value_to_index_key_bytes(&Value::Record(vec![("a".into(), Value::Path(vec![]))]))
                .unwrap()
                .is_some()
        );
        let p = Value::Path(vec![
            PathElement::Vertex(vec![1].into()),
            PathElement::Edge(vec![2, 3, b'e'].into()),
        ]);
        assert_eq!(
            value_to_index_key_bytes(&p).unwrap().unwrap(),
            value_to_index_key_bytes(&p).unwrap().unwrap()
        );
    }

    #[test]
    fn index_key_order_matches_path_comparison() {
        use crate::types::PathElement;
        use crate::value_cmp::compare_values;

        let ordered = [
            Value::Path(vec![]),
            Value::Path(vec![PathElement::Vertex(Vec::<u8>::new().into())]),
            Value::Path(vec![PathElement::Vertex(vec![0].into())]),
            Value::Path(vec![PathElement::Vertex(vec![0, 0].into())]),
            Value::Path(vec![
                PathElement::Vertex(vec![0, 0].into()),
                PathElement::Vertex(vec![0].into()),
            ]),
            Value::Path(vec![
                PathElement::Vertex(vec![0, 0].into()),
                PathElement::Edge(Vec::<u8>::new().into()),
            ]),
            Value::Path(vec![
                PathElement::Vertex(vec![0, 0].into()),
                PathElement::Edge(vec![0].into()),
            ]),
            Value::Path(vec![
                PathElement::Vertex(vec![0, 0].into()),
                PathElement::Edge(vec![0, 0].into()),
            ]),
        ];
        for pair in ordered.windows(2) {
            assert_eq!(
                compare_values(&pair[0], &pair[1]),
                Some(Ordering::Less),
                "compare_values: {:?} <? {:?}",
                pair[0],
                pair[1]
            );
            assert!(
                key(pair[0].clone()) < key(pair[1].clone()),
                "path keys: {:?} <? {:?}",
                pair[0],
                pair[1]
            );
        }
    }

    #[test]
    fn index_key_rejects_non_orderable_extensions() {
        assert_eq!(
            value_to_index_key_bytes(&Value::Extension(Box::new(NonOrderableExt))),
            Err(ValueIndexKeyError::UnsupportedValue)
        );
        assert_eq!(
            value_to_index_key_bytes(&Value::List(vec![Value::Extension(Box::new(
                NonOrderableExt
            ))])),
            Err(ValueIndexKeyError::UnsupportedValue)
        );
        assert_eq!(
            value_to_index_key_bytes(&Value::Record(vec![(
                "a".into(),
                Value::Extension(Box::new(NonOrderableExt))
            )])),
            Err(ValueIndexKeyError::UnsupportedValue)
        );
    }

    #[test]
    fn index_key_orders_orderable_extensions_by_domain_and_bytes() {
        let ordered = [
            extension("domain-a/v1", b"z"),
            extension("domain-b/v1", b"a"),
            extension("domain-b/v1", b"a\0"),
            extension("domain-b/v1", b"b"),
        ]
        .map(key);
        for pair in ordered.windows(2) {
            assert!(pair[0] < pair[1], "extension keys out of order: {pair:?}");
        }
    }

    #[test]
    fn index_key_order_matches_extension_cmp_within_domain() {
        let left = extension("domain/v1", b"a");
        let right = extension("domain/v1", b"b");
        assert_eq!(
            crate::value_cmp::compare_values(&left, &right),
            Some(Ordering::Less)
        );
        assert!(key(left) < key(right));

        let cross_left = extension("a/v1", b"z");
        let cross_right = extension("b/v1", b"a");
        assert_eq!(
            crate::value_cmp::compare_values(&cross_left, &cross_right),
            None
        );
        assert!(key(cross_left) < key(cross_right));
    }

    #[test]
    fn numeric_range_bounds_rejects_non_range_operators() {
        use crate::ast::CmpOp;
        assert!(numeric_range_bounds(&Value::Int64(5), CmpOp::Eq).is_err());
        assert!(numeric_range_bounds(&Value::Int64(5), CmpOp::Ne).is_err());
    }

    #[test]
    fn numeric_range_bounds_rejects_non_numeric_and_null() {
        use crate::ast::CmpOp;
        assert!(numeric_range_bounds(&Value::Text("a".into()), CmpOp::Ge).is_err());
        assert!(numeric_range_bounds(&Value::Null, CmpOp::Ge).is_err());
        assert!(numeric_range_bounds(&Value::Bytes(vec![1]), CmpOp::Lt).is_err());
    }

    #[test]
    fn numeric_range_bounds_rejects_non_finite_floats() {
        use crate::ast::CmpOp;
        assert!(numeric_range_bounds(&Value::Float64(f64::NAN), CmpOp::Ge).is_err());
        assert!(numeric_range_bounds(&Value::Float64(f64::INFINITY), CmpOp::Lt).is_err());
    }

    #[test]
    fn numeric_range_bounds_are_half_open_and_ordered() {
        use crate::ast::CmpOp;
        use crate::value_cmp::compare_values;

        // Pick values that straddle the bound.
        let cases = [
            (Value::Int64(5), CmpOp::Ge, Value::Int64(4), false), // 4 not in [5, ...)
            (Value::Int64(5), CmpOp::Ge, Value::Int64(5), true),  // 5 in [5, ...)
            (Value::Int64(5), CmpOp::Ge, Value::Int64(6), true),
            (Value::Int64(5), CmpOp::Gt, Value::Int64(5), false), // 5 not in (5, ...)
            (Value::Int64(5), CmpOp::Gt, Value::Int64(6), true),
            (Value::Int64(5), CmpOp::Le, Value::Int64(4), true), // 4 in (... 5]
            (Value::Int64(5), CmpOp::Le, Value::Int64(5), true),
            (Value::Int64(5), CmpOp::Le, Value::Int64(6), false),
            (Value::Int64(5), CmpOp::Lt, Value::Int64(4), true), // 4 in (... 5)
            (Value::Int64(5), CmpOp::Lt, Value::Int64(5), false),
        ];

        for (bound_value, op, probe_value, expected_in_range) in cases {
            let (low, high) = numeric_range_bounds(&bound_value, op).expect("numeric range");
            let probe_key = value_to_index_key_bytes(&probe_value).unwrap().unwrap();
            let in_range = low <= probe_key && probe_key < high;
            assert_eq!(
                in_range,
                expected_in_range,
                "{probe_value:?} should {} satisfy {bound_value:?} {op:?}",
                if expected_in_range { "" } else { "not" }
            );

            // The relational answer from GQL must agree.
            let relation_satisfies = match op {
                CmpOp::Ge => {
                    compare_values(&probe_value, &bound_value) == Some(std::cmp::Ordering::Greater)
                        || compare_values(&probe_value, &bound_value)
                            == Some(std::cmp::Ordering::Equal)
                }
                CmpOp::Gt => {
                    compare_values(&probe_value, &bound_value) == Some(std::cmp::Ordering::Greater)
                }
                CmpOp::Le => {
                    compare_values(&probe_value, &bound_value) == Some(std::cmp::Ordering::Less)
                        || compare_values(&probe_value, &bound_value)
                            == Some(std::cmp::Ordering::Equal)
                }
                CmpOp::Lt => {
                    compare_values(&probe_value, &bound_value) == Some(std::cmp::Ordering::Less)
                }
                _ => unreachable!(),
            };
            assert_eq!(
                in_range, relation_satisfies,
                "encoded range disagrees with compare_values for {probe_value:?} {op:?} {bound_value:?}"
            );
        }
    }

    #[test]
    fn numeric_range_bounds_excludes_adjacent_non_numeric_domains() {
        use crate::ast::CmpOp;

        // All numeric keys start with INDEX_KEY_NUMERIC. A `Ge` scan must stop before the next
        // type domain, so a text key is never included.
        let (low, high) = numeric_range_bounds(&Value::Int64(0), CmpOp::Ge).unwrap();
        assert_eq!(low[0], INDEX_KEY_NUMERIC);
        assert_eq!(high[0], INDEX_KEY_NUMERIC + 1);
        let text_key = value_to_index_key_bytes(&Value::Text("a".into()))
            .unwrap()
            .unwrap();
        assert!(
            text_key >= high || text_key < low,
            "text key must be outside numeric range"
        );
    }

    #[test]
    fn numeric_range_bounds_unifies_across_numeric_widths() {
        use crate::ast::CmpOp;

        // Int64(5), Uint8(5), Decimal("5.0") and Float64(5.0) should all produce the same
        // encoded bound and therefore the same range.
        let bound1 = numeric_range_bounds(&Value::Int64(5), CmpOp::Ge).unwrap();
        let bound2 = numeric_range_bounds(&Value::Uint8(5), CmpOp::Ge).unwrap();
        let bound3 = numeric_range_bounds(&Value::Float64(5.0), CmpOp::Ge).unwrap();
        let bound4 = numeric_range_bounds(&decimal("5.0"), CmpOp::Ge).unwrap();
        assert_eq!(bound1, bound2);
        assert_eq!(bound2, bound3);
        assert_eq!(bound3, bound4);
    }
}
