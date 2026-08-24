//! Sortable property-index key encoding for [`Value`](crate::Value).
//!
//! [`Value::Path`] keys use [`INDEX_KEY_PATH`] and order consistently with
//! [`crate::value::cmp::compare_values`] (same rules as [`PathElement`](crate::types::PathElement) comparison).

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
    /// The value encodes into a posting-key domain whose comparison semantics do not realize
    /// `property OP value` as one contiguous ordered interval (Bool, List, Record, Path,
    /// Extension, and Duration). Range consumers must not push such literals down.
    UnsupportedRangeDomain,
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
            Self::UnsupportedRangeDomain => write!(
                f,
                "value's comparison domain does not support ordered index ranges"
            ),
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
/// Path values; element order matches [`crate::value::cmp::compare_path_slices`].
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
        | Value::Uint8(_)
        | Value::Uint16(_)
        | Value::Uint32(_)
        | Value::Uint64(_)
        | Value::Uint128(_)
        | Value::Float32(_)
        | Value::Float64(_)) => {
            out.push(INDEX_KEY_NUMERIC);
            encode_numeric_key(value, out)?;
        }
        #[cfg(feature = "i256")]
        value @ Value::Int256(_) => {
            out.push(INDEX_KEY_NUMERIC);
            encode_numeric_key(value, out)?;
        }
        #[cfg(feature = "u256")]
        value @ Value::Uint256(_) => {
            out.push(INDEX_KEY_NUMERIC);
            encode_numeric_key(value, out)?;
        }
        #[cfg(feature = "decimal")]
        value @ Value::Decimal(_) => {
            out.push(INDEX_KEY_NUMERIC);
            encode_numeric_key(value, out)?;
        }
        #[cfg(feature = "f16")]
        value @ Value::Float16(_) => {
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

/// Escape one posting-key payload without the terminating `[0, 0]`.
fn push_escaped_index_payload(out: &mut Vec<u8>, bytes: &[u8]) {
    for byte in bytes {
        if *byte == 0 {
            out.extend_from_slice(&[0, 255]);
        } else {
            out.push(*byte);
        }
    }
}

fn push_escaped_index_bytes(out: &mut Vec<u8>, bytes: &[u8]) {
    push_escaped_index_payload(out, bytes);
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

/// Derive the finite half-open encoded-key interval `[low, high)` of the comparison domain that
/// realizes `property OP value` for any supported ordered literal.
///
/// The interval is clamped to the literal's posting-key domain: NUMERIC (`[2]..[3]`), TEXT
/// (`[6]..[7]`), BYTES (`[7]..[8]`), and each chronologically-ordered TEMPORAL subtype
/// (`[8, k]..succ([8, k])`, subtypes Date..ZonedTime). A scan over these bounds returns exactly
/// the postings whose stored value satisfies the GQL comparison; postings from other domains are
/// never included, so mixed-type properties cannot leak wrong-domain rows. Bool, List, Record,
/// Path, Extension, Null, and Duration (tuple order is not chronological) reject with
/// [`ValueIndexKeyError::UnsupportedRangeDomain`] or [`ValueIndexKeyError::UnsupportedValue`];
/// consumers must answer those through their non-index filter path instead of pushing down.
///
/// The returned bounds may form an empty interval when no domain value can satisfy the
/// comparison (for example `> Date::MAX`); consumers must treat `low >= high` as zero hits.
///
/// This is the canonical place where GQL value-type tags meet comparison ordering. Router and
/// Property Index must not duplicate tag or ordering knowledge.
pub fn range_bounds(
    value: &Value,
    op: crate::ast::CmpOp,
) -> Result<(Vec<u8>, Vec<u8>), ValueIndexKeyError> {
    let bound_key = encoded_range_bound_key(value)?;
    one_sided_domain_interval(&bound_key, op)
}

/// Same contract as [`range_bounds`] for an already-encoded index key (for example a cached
/// seed-probe bound whose original [`Value`] is not retained).
pub fn range_bounds_for_encoded_key(
    bound_key: &[u8],
    op: crate::ast::CmpOp,
) -> Result<(Vec<u8>, Vec<u8>), ValueIndexKeyError> {
    if bound_key.is_empty() {
        return Err(ValueIndexKeyError::UnsupportedRangeDomain);
    }
    one_sided_domain_interval(bound_key, op)
}

/// Exclusive upper-bound sentinel for TEXT prefix intervals.
///
/// UTF-8 payloads contain neither `0x00` nor `0xFF`, and the escape rule only emits `0xFF`
/// as the second byte of a `[0, 255]` pair, so no stored TEXT posting key can continue a
/// prefix with a byte equal to this sentinel. Appending it to the stripped pattern therefore
/// keeps every key that extends the pattern below `high` while keys diverging before the end
/// of the pattern fall outside on an earlier byte. A plain byte-successor of the last pattern
/// byte is rejected instead because multi-byte characters can terminate a prefix, making
/// "last byte + 1" exclude valid continuations.
const TEXT_PREFIX_EXCLUSIVE_END: u8 = 255;

/// Derive the finite half-open encoded-key interval `[low, high)` realizing
/// `property STARTS WITH pattern` over the TEXT posting-key domain.
///
/// Stored TEXT keys are `[6] + escaped(text) + [0, 0]`; escaping maps `0x00` to `[0, 255]`.
/// Because escaping is a per-byte map it preserves prefix relations, so every key whose
/// decoded text starts with `pattern` lies in `[low, high)`, and keys whose text diverges or
/// ends before the pattern are excluded. The empty pattern yields the whole TEXT domain
/// (`[low, high)` = `[6], [7]`), matching `x STARTS WITH ''` being true for every non-Null
/// text. BYTES-domain keys (`tag = 7`) never enter the interval: their leading tag byte sorts
/// above any `[6, …]` bound.
///
/// `Null` rejects with [`ValueIndexKeyError::UnsupportedValue`] and every non-TEXT value with
/// [`ValueIndexKeyError::UnsupportedRangeDomain`]; consumers must answer those through their
/// non-index filter path instead of pushing down. This is, together with [`range_bounds`],
/// the canonical place where GQL value-type tags meet comparison ordering — Router and
/// Property Index must not duplicate tag or ordering knowledge.
pub fn text_prefix_range_bounds(pattern: &Value) -> Result<(Vec<u8>, Vec<u8>), ValueIndexKeyError> {
    let Some(bound_key) = value_to_index_key_bytes(pattern)? else {
        return Err(ValueIndexKeyError::UnsupportedValue);
    };
    text_prefix_range_bounds_for_encoded_key(&bound_key)
}

/// Same contract as [`text_prefix_range_bounds`] for an already-encoded index key (for
/// example a cached seed-probe pattern whose original [`Value`] is not retained).
pub fn text_prefix_range_bounds_for_encoded_key(
    pattern_key: &[u8],
) -> Result<(Vec<u8>, Vec<u8>), ValueIndexKeyError> {
    let (&tag, payload) = pattern_key
        .split_first()
        .ok_or(ValueIndexKeyError::UnsupportedRangeDomain)?;
    // A stored TEXT key always carries at least the [0, 0] terminator after the tag;
    // anything shorter is not a well-formed encoded TEXT value.
    if tag != INDEX_KEY_TEXT || payload.len() < 2 {
        return Err(ValueIndexKeyError::UnsupportedRangeDomain);
    }
    // The empty prefix spans the whole TEXT domain: [low, high) = [6], [7].
    if payload.len() == 2 && payload == [END_MARKER, END_MARKER] {
        let floor = vec![INDEX_KEY_TEXT];
        let ceiling = lex_succ_bytes(&floor);
        return Ok((floor, ceiling));
    }
    let mut low = Vec::with_capacity(pattern_key.len() - 1);
    low.extend_from_slice(&pattern_key[..pattern_key.len() - 2]);
    let mut high = low.clone();
    high.push(TEXT_PREFIX_EXCLUSIVE_END);
    Ok((low, high))
}

fn encoded_range_bound_key(value: &Value) -> Result<Vec<u8>, ValueIndexKeyError> {
    value_to_index_key_bytes(value)?.ok_or(ValueIndexKeyError::UnsupportedValue)
}

/// Merge the one-sided cut of `op` with the bound key's comparison-domain interval.
fn one_sided_domain_interval(
    bound_key: &[u8],
    op: crate::ast::CmpOp,
) -> Result<(Vec<u8>, Vec<u8>), ValueIndexKeyError> {
    use crate::ast::CmpOp;

    if !matches!(op, CmpOp::Lt | CmpOp::Le | CmpOp::Gt | CmpOp::Ge) {
        return Err(ValueIndexKeyError::UnsupportedValue);
    }

    let Some((floor, ceiling)) = comparison_domain_interval(bound_key) else {
        return Err(ValueIndexKeyError::UnsupportedRangeDomain);
    };

    // `lex_succ_bytes(bound)` spills past the ceiling when the bound tail is all 0xFF
    // (for example Date(i64::MAX)); every upper bound is clamped to the domain ceiling.
    let clamped_succ = |bound: &[u8]| {
        let succ = lex_succ_bytes(bound);
        if succ.as_slice() > ceiling.as_slice() {
            ceiling.clone()
        } else {
            succ
        }
    };
    match op {
        CmpOp::Ge => Ok((bound_key.to_vec(), ceiling)),
        CmpOp::Gt => Ok((clamped_succ(bound_key), ceiling)),
        CmpOp::Le => Ok((floor, clamped_succ(bound_key))),
        CmpOp::Lt => Ok((floor, bound_key.to_vec())),
        _ => unreachable!(),
    }
}

/// Half-open `[floor, ceiling)` memcmp interval holding exactly the posting keys whose leading
/// bytes share `key`'s comparison domain. `None` for domains without a contiguous ordered
/// interval (Bool, List, Record, Path, Extension) and for Duration, whose `(months, nanos)`
/// tuple order is not chronological.
fn comparison_domain_interval(key: &[u8]) -> Option<(Vec<u8>, Vec<u8>)> {
    let (&tag, rest) = key.split_first()?;
    let floor = match tag {
        INDEX_KEY_NUMERIC => vec![INDEX_KEY_NUMERIC],
        INDEX_KEY_TEXT | INDEX_KEY_BYTES => vec![tag],
        INDEX_KEY_TEMPORAL => {
            let (&subtype, _) = rest.split_first()?;
            // Subtypes 1..=7 (Date .. ZonedTime) keep chronological order in key space;
            // subtype 8 is Duration, rejected until it has a spec-backed total order.
            if !(1..=7).contains(&subtype) {
                return None;
            }
            vec![INDEX_KEY_TEMPORAL, subtype]
        }
        _ => return None,
    };
    let ceiling = lex_succ_bytes(&floor);
    Some((floor, ceiling))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(feature = "decimal")]
    use crate::types::Decimal;
    #[cfg(feature = "i256")]
    use crate::types::Int256;
    #[cfg(feature = "u256")]
    use crate::types::Uint256;
    use crate::value::{ExtensionSortableKey, ExtensionValue};
    use std::any::Any;
    use std::borrow::Cow;
    use std::cmp::Ordering;
    use std::fmt;

    fn key(value: Value) -> Vec<u8> {
        value_to_index_key_bytes(&value).unwrap().unwrap()
    }

    #[cfg(feature = "decimal")]
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

    #[cfg(all(feature = "i256", feature = "u256", feature = "decimal"))]
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

    #[cfg(all(feature = "i256", feature = "u256", feature = "decimal"))]
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

    #[cfg(feature = "decimal")]
    #[test]
    fn index_key_unifies_equal_float_decimal_values() {
        let keys = [Value::Float64(1.5), Value::Float32(1.5), decimal("1.5")].map(key);
        assert!(keys.windows(2).all(|pair| pair[0] == pair[1]));
    }

    #[cfg(feature = "decimal")]
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

    #[cfg(feature = "decimal")]
    #[test]
    fn index_key_distinguishes_exact_binary_and_decimal_tenths() {
        let float_key = key(Value::Float64(0.1));
        let decimal_key = key(decimal("0.1"));
        assert_ne!(float_key, decimal_key);
        assert!(decimal_key < float_key);
    }

    #[cfg(all(feature = "decimal", feature = "cmp"))]
    #[test]
    fn index_key_order_matches_mixed_numeric_comparison() {
        #[cfg(feature = "cmp")]
        use crate::value::cmp::compare_values;

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

    #[cfg(feature = "decimal")]
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

    #[cfg(feature = "decimal")]
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

    #[cfg(feature = "cmp")]
    #[test]
    fn index_key_order_matches_path_comparison() {
        use crate::types::PathElement;
        #[cfg(feature = "cmp")]
        use crate::value::cmp::compare_values;

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

    #[cfg(feature = "cmp")]
    #[test]
    fn index_key_order_matches_extension_cmp_within_domain() {
        let left = extension("domain/v1", b"a");
        let right = extension("domain/v1", b"b");
        assert_eq!(
            crate::value::cmp::compare_values(&left, &right),
            Some(Ordering::Less)
        );
        assert!(key(left) < key(right));

        let cross_left = extension("a/v1", b"z");
        let cross_right = extension("b/v1", b"a");
        assert_eq!(
            crate::value::cmp::compare_values(&cross_left, &cross_right),
            None
        );
        assert!(key(cross_left) < key(cross_right));
    }

    #[test]
    fn range_bounds_rejects_non_range_operators() {
        use crate::ast::CmpOp;
        assert!(range_bounds(&Value::Int64(5), CmpOp::Eq).is_err());
        assert!(range_bounds(&Value::Int64(5), CmpOp::Ne).is_err());
        assert!(range_bounds(&Value::Text("a".into()), CmpOp::Eq).is_err());
        // The bytes-facing entry shares the operator gate.
        assert!(range_bounds_for_encoded_key(&key(Value::Int64(5)), CmpOp::Ne).is_err());
    }

    #[test]
    fn range_bounds_rejects_null_and_unencodable_values_as_unsupported() {
        use crate::ast::CmpOp;
        assert_eq!(
            range_bounds(&Value::Null, CmpOp::Ge),
            Err(ValueIndexKeyError::UnsupportedValue)
        );
        assert_eq!(
            range_bounds(&Value::Float64(f64::NAN), CmpOp::Ge),
            Err(ValueIndexKeyError::NonFiniteFloat)
        );
        assert_eq!(
            range_bounds(&Value::Float64(f64::INFINITY), CmpOp::Lt),
            Err(ValueIndexKeyError::NonFiniteFloat)
        );
    }

    #[test]
    fn range_bounds_rejects_domains_without_ordered_interval() {
        use crate::ast::CmpOp;
        let rejected = [
            Value::Bool(true),
            Value::List(vec![Value::Int64(1)]),
            Value::Record(vec![("a".into(), Value::Int64(1))]),
            Value::Path(vec![PathElement::Vertex(vec![1].into())]),
            extension("domain/v1", b"a"),
            Value::Duration(1, 0),
        ];
        for value in rejected {
            assert_eq!(
                range_bounds(&value, CmpOp::Ge),
                Err(ValueIndexKeyError::UnsupportedRangeDomain),
                "{value:?} must not push down"
            );
            assert_eq!(
                range_bounds(&value, CmpOp::Lt),
                Err(ValueIndexKeyError::UnsupportedRangeDomain),
                "{value:?} must not push down"
            );
        }
    }

    #[test]
    fn range_bounds_domain_table_pins_floor_and_ceiling_per_domain() {
        use crate::ast::CmpOp;

        let cases: Vec<(Value, Vec<u8>, Vec<u8>)> = vec![
            (
                Value::Int64(5),
                vec![INDEX_KEY_NUMERIC],
                vec![INDEX_KEY_NUMERIC + 1],
            ),
            (
                Value::Text("m".into()),
                vec![INDEX_KEY_TEXT],
                vec![INDEX_KEY_TEXT + 1],
            ),
            (
                Value::Bytes(vec![7]),
                vec![INDEX_KEY_BYTES],
                vec![INDEX_KEY_BYTES + 1],
            ),
            (
                Value::Date(19_000),
                vec![INDEX_KEY_TEMPORAL, 1],
                vec![INDEX_KEY_TEMPORAL, 2],
            ),
            (
                Value::Time(500),
                vec![INDEX_KEY_TEMPORAL, 2],
                vec![INDEX_KEY_TEMPORAL, 3],
            ),
            (
                Value::LocalTime(500),
                vec![INDEX_KEY_TEMPORAL, 3],
                vec![INDEX_KEY_TEMPORAL, 4],
            ),
            (
                Value::DateTime(1_700_000_000, 1),
                vec![INDEX_KEY_TEMPORAL, 4],
                vec![INDEX_KEY_TEMPORAL, 5],
            ),
            (
                Value::LocalDateTime(1_700_000_000, 1),
                vec![INDEX_KEY_TEMPORAL, 5],
                vec![INDEX_KEY_TEMPORAL, 6],
            ),
            (
                Value::ZonedDateTime(1_700_000_000, 1, 3_600),
                vec![INDEX_KEY_TEMPORAL, 6],
                vec![INDEX_KEY_TEMPORAL, 7],
            ),
            (
                Value::ZonedTime(500, 3_600),
                vec![INDEX_KEY_TEMPORAL, 7],
                vec![INDEX_KEY_TEMPORAL, 8],
            ),
        ];
        for (value, floor, ceiling) in cases {
            let bound_key = key(value.clone());
            let ge = range_bounds(&value, CmpOp::Ge).expect("supported domain");
            assert_eq!(
                ge,
                (bound_key.clone(), ceiling.clone()),
                "Ge keeps the bound"
            );
            let gt = range_bounds(&value, CmpOp::Gt).expect("supported domain");
            assert!(
                gt.0 > bound_key && gt.0 <= gt.1,
                "Gt starts strictly above the bound and stays in-domain"
            );
            assert_eq!(gt.1, ceiling, "Gt high is the domain ceiling");
            let (low, high) = range_bounds(&value, CmpOp::Le).expect("supported domain");
            assert_eq!(low, floor, "Le: low is the domain floor");
            assert!(
                bound_key < high && high <= ceiling,
                "Le: high holds the bound and stays under the ceiling"
            );
            let (low, high) = range_bounds(&value, CmpOp::Lt).expect("supported domain");
            assert_eq!(low, floor, "Lt: low is the domain floor");
            assert_eq!(high, bound_key, "Lt: high is the raw bound");
        }
    }

    #[cfg(feature = "cmp")]
    #[test]
    fn range_bounds_are_half_open_and_agree_with_comparison_across_domains() {
        use crate::ast::CmpOp;
        use crate::value::cmp::compare_values;

        // (literal, probe) pairs straddling the bound within the literal's own domain,
        // plus probes from foreign domains that must always fall outside the interval.
        let text_cases = [
            ("b", "a", CmpOp::Ge, false),
            ("b", "b", CmpOp::Ge, true),
            ("b", "c", CmpOp::Ge, true),
            ("b", "a", CmpOp::Gt, false),
            ("b", "b", CmpOp::Gt, false),
            ("b", "bb", CmpOp::Gt, true),
            ("b", "a", CmpOp::Le, true),
            ("b", "b", CmpOp::Le, true),
            ("b", "c", CmpOp::Le, false),
            ("b\0", "b", CmpOp::Lt, true),
            ("b\0", "b\0", CmpOp::Lt, false),
        ];
        for (bound, probe, op, expected) in text_cases {
            let (low, high) =
                range_bounds(&Value::Text(bound.into()), op).expect("text range bounds");
            let probe_key = key(Value::Text(probe.into()));
            assert_eq!(
                low <= probe_key && probe_key < high,
                expected,
                "Text({probe:?}) {op:?} Text({bound:?})"
            );
        }

        let byte_cases = [
            (vec![2u8], vec![1u8], CmpOp::Le, true),
            (vec![2u8], vec![2u8], CmpOp::Le, true),
            (vec![2u8], vec![2u8, 0], CmpOp::Le, false),
            (vec![2u8], vec![3u8], CmpOp::Le, false),
            (vec![2u8], vec![1, 255], CmpOp::Gt, false),
            (vec![2u8], vec![2u8], CmpOp::Gt, false),
            (vec![2u8], vec![2, 0], CmpOp::Gt, true),
        ];
        for (bound, probe, op, expected) in byte_cases {
            let (low, high) = range_bounds(&Value::Bytes(bound.clone()), op).expect("bytes range");
            let probe_key = key(Value::Bytes(probe));
            assert_eq!(low <= probe_key && probe_key < high, expected, "{op:?}");
        }

        let datetime_cases: [(i64, u32, i64, u32, CmpOp, bool); 6] = [
            (100, 0, 99, 0, CmpOp::Ge, false),
            (100, 0, 100, 0, CmpOp::Ge, true),
            (100, 0, 101, 5, CmpOp::Ge, true),
            (100, 0, 100, 0, CmpOp::Gt, false),
            (100, 0, 100, 1, CmpOp::Gt, true),
            (99, u32::MAX, 100, 0, CmpOp::Ge, true),
        ];
        for (bs, bn, ps, pn, op, expected) in datetime_cases {
            let (low, high) =
                range_bounds(&Value::DateTime(bs, bn), op).expect("datetime range bounds");
            let probe_key = key(Value::DateTime(ps, pn));
            let in_range = low <= probe_key && probe_key < high;
            assert_eq!(
                in_range, expected,
                "DateTime({ps},{pn}) {op:?} DateTime({bs},{bn})"
            );
            let probe_value = Value::DateTime(ps, pn);
            let bound_value = Value::DateTime(bs, bn);
            let ordering = compare_values(&probe_value, &bound_value);
            let satisfies = match op {
                CmpOp::Ge => ordering >= Some(Ordering::Equal),
                CmpOp::Gt => ordering == Some(Ordering::Greater),
                _ => unreachable!("datetime cases only use lower bounds"),
            };
            assert_eq!(
                in_range, satisfies,
                "encoded interval disagrees with compare_values for {op:?}"
            );
        }
    }

    #[cfg(feature = "cmp")]
    #[test]
    fn numeric_range_bounds_are_half_open_and_ordered() {
        use crate::ast::CmpOp;
        use crate::value::cmp::compare_values;

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
            let (low, high) = range_bounds(&bound_value, op).expect("numeric range");
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
    fn range_bounds_excludes_every_foreign_domain() {
        use crate::ast::CmpOp;

        // For each supported domain, probes encoded in every OTHER domain must fall outside
        // the literal's interval: mixed-type properties cannot leak wrong-domain rows.
        let literals = [
            Value::Int64(5),
            Value::Text("m".into()),
            Value::Bytes(vec![9]),
            Value::Date(19_000),
            Value::DateTime(1_700_000_000, 1),
            Value::ZonedTime(500, 0),
        ];
        let foreign_probes = [
            Value::Bool(true),
            Value::Int64(5),
            Value::Text("m".into()),
            Value::Bytes(vec![9]),
            Value::Date(19_000),
            Value::Time(500),
            Value::LocalTime(500),
            Value::DateTime(1_700_000_000, 1),
            Value::LocalDateTime(1_700_000_000, 1),
            Value::ZonedDateTime(1_700_000_000, 1, 0),
            Value::ZonedTime(500, 0),
            Value::Duration(1, 0),
        ];
        for literal in &literals {
            for op in [CmpOp::Ge, CmpOp::Le] {
                let (low, high) = range_bounds(literal, op).expect("supported domain");
                for probe in &foreign_probes {
                    if compare_tags(probe, literal) {
                        continue;
                    }
                    let probe_key = key(probe.clone());
                    assert!(
                        !(low <= probe_key && probe_key < high),
                        "{probe:?} key must stay outside {literal:?} {op:?} interval"
                    );
                }
            }
        }
    }

    /// Whether `a` and `b` share the same posting-key comparison-domain tag (and TEMPORAL subtype).
    fn compare_tags(a: &Value, b: &Value) -> bool {
        let tag = |v: &Value| key(v.clone())[0];
        if tag(a) != tag(b) {
            return false;
        }
        if tag(a) != INDEX_KEY_TEMPORAL {
            return true;
        }
        key(a.clone())[1] == key(b.clone())[1]
    }

    #[test]
    fn range_bounds_all_ff_tail_bound_does_not_spill_past_ceiling() {
        use crate::ast::CmpOp;

        // DateTime(i64::MAX, u32::MAX) encodes to [8, 4, FF*12]: an all-0xFF payload tail.
        // The successor of that bound would walk into subtype 5 ([8, 5]); the upper bound must
        // clamp to the ceiling instead of crossing it.
        let max_datetime = Value::DateTime(i64::MAX, u32::MAX);
        let bound_key = key(max_datetime.clone());
        assert!(
            bound_key[2..].iter().all(|&b| b == 255),
            "precondition: all-FF tail"
        );

        let le = range_bounds(&max_datetime, CmpOp::Le).expect("supported domain");
        assert_eq!(
            le.1,
            vec![INDEX_KEY_TEMPORAL, 5],
            "Le clamps at the ceiling"
        );
        assert_eq!(
            le.0,
            vec![INDEX_KEY_TEMPORAL, 4],
            "Le starts at the subtype floor"
        );

        // Nothing is strictly greater than the maximum value: the clamped interval is empty,
        // which consumers must treat as zero hits rather than an open-ended scan.
        let gt = range_bounds(&max_datetime, CmpOp::Gt).expect("supported domain");
        assert!(gt.0 >= gt.1, "Gt past the domain maximum is empty");

        // Adversarial cached keys through the bytes-facing entry behave the same way.
        let adversarial: &[&[u8]] = &[
            &[INDEX_KEY_TEXT, 255, 255],
            &[INDEX_KEY_BYTES, 255, 255],
            &[INDEX_KEY_TEMPORAL, 7, 255, 255],
        ];
        let ceilings: Vec<Vec<u8>> = vec![
            vec![INDEX_KEY_TEXT + 1],
            vec![INDEX_KEY_BYTES + 1],
            vec![INDEX_KEY_TEMPORAL, 8],
        ];
        for (bound, ceiling) in adversarial.iter().zip(ceilings) {
            for op in [CmpOp::Le, CmpOp::Lt, CmpOp::Ge, CmpOp::Gt] {
                let (low, high) =
                    range_bounds_for_encoded_key(bound, op).expect("tagged key has a domain");
                assert!(
                    high <= ceiling,
                    "{op:?}: high must not spill past the ceiling"
                );
                assert!(low <= high, "{op:?}: interval stays ordered");
            }
        }
    }

    #[test]
    fn range_bounds_temporal_subtypes_are_pinned() {
        use crate::ast::CmpOp;

        // A DateTime literal must never admit Date or LocalDateTime postings even when their
        // payloads would order between the interval endpoints.
        let bound = Value::DateTime(1_000, 0);
        let (low, high) = range_bounds(&bound, CmpOp::Ge).expect("datetime bounds");
        let date_probe = key(Value::Date(40_000));
        let local_dt_probe = key(Value::LocalDateTime(2_000, 0));
        assert!(
            !(low <= date_probe && date_probe < high),
            "Date postings are outside the DateTime subtype interval"
        );
        assert!(
            !(low <= local_dt_probe && local_dt_probe < high),
            "LocalDateTime postings are outside the DateTime subtype interval"
        );
        let bound_key = key(bound);
        assert!(low <= bound_key && bound_key < high);
    }

    #[test]
    fn range_bounds_for_encoded_key_matches_value_entry() {
        use crate::ast::CmpOp;

        let values = [
            Value::Int64(-7),
            Value::Text("mixed\0bytes".into()),
            Value::Bytes(vec![0, 255, 1]),
            Value::Date(19_500),
            Value::DateTime(1_700_000_000, 999),
        ];
        for value in values {
            for op in [CmpOp::Lt, CmpOp::Le, CmpOp::Gt, CmpOp::Ge] {
                assert_eq!(
                    range_bounds(&value, op),
                    range_bounds_for_encoded_key(&key(value.clone()), op),
                    "{value:?} {op:?}"
                );
            }
        }
        assert!(range_bounds_for_encoded_key(&[], CmpOp::Ge).is_err());
    }

    #[cfg(feature = "decimal")]
    #[test]
    fn range_bounds_unifies_across_numeric_widths() {
        use crate::ast::CmpOp;

        // Int64(5), Uint8(5), Decimal("5.0") and Float64(5.0) should all produce the same
        // encoded bound and therefore the same range.
        let bound1 = range_bounds(&Value::Int64(5), CmpOp::Ge).unwrap();
        let bound2 = range_bounds(&Value::Uint8(5), CmpOp::Ge).unwrap();
        let bound3 = range_bounds(&Value::Float64(5.0), CmpOp::Ge).unwrap();
        let bound4 = range_bounds(&decimal("5.0"), CmpOp::Ge).unwrap();
        assert_eq!(bound1, bound2);
        assert_eq!(bound2, bound3);
        assert_eq!(bound3, bound4);
    }

    fn text_key(text: &str) -> Vec<u8> {
        key(Value::Text(text.into()))
    }

    #[test]
    fn text_prefix_range_bounds_pins_exact_interval_bytes() {
        let (low, high) = text_prefix_range_bounds(&Value::Text("ab".into())).unwrap();
        assert_eq!(low, vec![INDEX_KEY_TEXT, b'a', b'b']);
        assert_eq!(high, vec![INDEX_KEY_TEXT, b'a', b'b', 255]);

        let (low, high) = text_prefix_range_bounds(&Value::Text("".into())).unwrap();
        assert_eq!(low, vec![INDEX_KEY_TEXT]);
        assert_eq!(high, vec![INDEX_KEY_TEXT + 1], "empty pattern spans TEXT");

        let (low, high) = text_prefix_range_bounds(&Value::Text("a\0b".into())).unwrap();
        assert_eq!(low, vec![INDEX_KEY_TEXT, b'a', 0, 255, b'b']);
        assert_eq!(high.last(), Some(&255));
    }

    #[test]
    fn text_prefix_range_bounds_rejects_null_and_non_text_domains() {
        assert_eq!(
            text_prefix_range_bounds(&Value::Null),
            Err(ValueIndexKeyError::UnsupportedValue)
        );
        let non_text = [
            Value::Int64(7),
            Value::Bool(true),
            Value::Bytes(vec![b'a']),
            Value::List(vec![Value::Int64(1)]),
            Value::Date(19_000),
        ];
        for value in non_text {
            assert_eq!(
                text_prefix_range_bounds(&value),
                Err(ValueIndexKeyError::UnsupportedRangeDomain),
                "{value:?} must not push down as a prefix"
            );
        }
        assert_eq!(
            text_prefix_range_bounds_for_encoded_key(&[]),
            Err(ValueIndexKeyError::UnsupportedRangeDomain)
        );
        assert_eq!(
            text_prefix_range_bounds_for_encoded_key(&key(Value::Int64(7))),
            Err(ValueIndexKeyError::UnsupportedRangeDomain)
        );
    }

    #[test]
    fn text_prefix_range_bounds_matches_stored_keys_exactly() {
        let cases: [(&str, &[&str], &[&str]); 6] = [
            // (pattern, keys inside the interval, keys outside it)
            ("ab", &["ab", "abc", "ab\0x", "abé"], &["a", "b", "ac"]),
            ("", &["", "a", "\u{FFFD}", "é"], &[]),
            ("日本", &["日本", "日本語"], &["日", "米", "東"]),
            ("a\0", &["a\0z", "a\0"], &["az", "a"]),
            ("é", &["é", "école"], &["e", "d"]),
            ("z", &["z", "zz", "za\0"], &["y", "x"]),
        ];
        for (pattern, included, excluded) in cases {
            let (low, high) =
                text_prefix_range_bounds(&Value::Text(pattern.into())).expect("text pattern");
            for text in included {
                let probe = text_key(text);
                assert!(
                    low <= probe && probe < high,
                    "{text:?} must satisfy STARTS WITH {pattern:?}"
                );
            }
            for text in excluded {
                let probe = text_key(text);
                assert!(
                    !(low <= probe && probe < high),
                    "{text:?} must not satisfy STARTS WITH {pattern:?}"
                );
            }
        }
    }

    #[test]
    fn text_prefix_range_bounds_agrees_with_value_entry_and_never_spills_domain() {
        let patterns = ["", "a", "ab\0", "é", "\u{10FFFF}"];
        for pattern in patterns {
            let value_entry =
                text_prefix_range_bounds(&Value::Text(pattern.into())).expect("supported");
            let encoded_entry =
                text_prefix_range_bounds_for_encoded_key(&text_key(pattern)).expect("supported");
            assert_eq!(value_entry, encoded_entry, "{pattern:?}");
            let (low, high) = value_entry;
            assert!(low < high, "prefix interval is never empty");
            assert!(
                low.starts_with(&[INDEX_KEY_TEXT]),
                "interval stays inside the TEXT domain"
            );
            assert!(
                high <= vec![INDEX_KEY_TEXT + 1],
                "never spills past ceiling"
            );
        }
        // A multi-byte character terminating the pattern is why the sentinel is 0xFF and
        // not a byte successor of the last pattern byte: "日" ends with 0xAC whose
        // successor 0xAD would wrongly exclude "日本" (0xE6 0x97 0xA5 0xE6 ...).
        let (low, high) = text_prefix_range_bounds(&Value::Text("日".into())).unwrap();
        let nippon = text_key("日本");
        assert!(low <= nippon && nippon < high);
    }

    #[test]
    fn text_prefix_range_bounds_separates_bytes_domain_by_tag() {
        let (low, high) = text_prefix_range_bounds(&Value::Text("ab".into())).unwrap();
        let bytes_probe = key(Value::Bytes(vec![b'a', b'b']));
        assert!(
            !(low <= bytes_probe && bytes_probe < high),
            "BYTES-domain postings never satisfy a TEXT prefix interval"
        );
    }
}
