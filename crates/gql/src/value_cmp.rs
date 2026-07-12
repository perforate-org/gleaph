//! Value comparison logic supporting cross-width values and exact mixed numeric
//! comparisons.

use std::cmp::Ordering;

use crate::Value;
use crate::numeric_order::compare_normalized_numeric;
use crate::temporal::zoned_time_to_utc_nanos;

fn compare_value_slices(left: &[Value], right: &[Value]) -> Option<Ordering> {
    for (l, r) in left.iter().zip(right) {
        let ord = compare_values(l, r)?;
        if ord != Ordering::Equal {
            return Some(ord);
        }
    }
    Some(left.len().cmp(&right.len()))
}

fn compare_record_fields(left: &[(String, Value)], right: &[(String, Value)]) -> Option<Ordering> {
    let mut left_fields: Vec<_> = left.iter().collect();
    let mut right_fields: Vec<_> = right.iter().collect();
    left_fields.sort_by(|a, b| a.0.cmp(&b.0));
    right_fields.sort_by(|a, b| a.0.cmp(&b.0));

    for ((left_name, left_value), (right_name, right_value)) in
        left_fields.iter().zip(right_fields.iter())
    {
        match left_name.cmp(right_name) {
            Ordering::Equal => {}
            ordering => return Some(ordering),
        }
        let value_ordering = compare_values(left_value, right_value)?;
        if value_ordering != Ordering::Equal {
            return Some(value_ordering);
        }
    }
    Some(left_fields.len().cmp(&right_fields.len()))
}

fn compare_path_elements(
    left: &crate::types::PathElement,
    right: &crate::types::PathElement,
) -> Ordering {
    use crate::types::PathElement;

    match (left, right) {
        (PathElement::Vertex(a), PathElement::Vertex(b)) => a.cmp(b),
        (PathElement::Edge(a), PathElement::Edge(b)) => a.cmp(b),
        (PathElement::Vertex(_), PathElement::Edge(_)) => Ordering::Less,
        (PathElement::Edge(_), PathElement::Vertex(_)) => Ordering::Greater,
    }
}

fn compare_path_slices(
    left: &[crate::types::PathElement],
    right: &[crate::types::PathElement],
) -> Ordering {
    for (l, r) in left.iter().zip(right) {
        let ord = compare_path_elements(l, r);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    left.len().cmp(&right.len())
}

fn normalize_zoned_time_utc(nanos: u64, tz_seconds: i32) -> i64 {
    zoned_time_to_utc_nanos(nanos, tz_seconds).expect("zoned time value is valid")
}

/// Compares two [`Value`]s and returns their ordering, or `None` if the types
/// are incomparable (e.g. `Text` vs `Int64`).
pub fn compare_values(left: &Value, right: &Value) -> Option<Ordering> {
    if left.is_numeric() && right.is_numeric() {
        return compare_normalized_numeric(left, right);
    }

    match (left, right) {
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
        (Value::Bytes(a), Value::Bytes(b)) => Some(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
        (Value::Time(a), Value::Time(b)) => Some(a.cmp(b)),
        (Value::LocalTime(a), Value::LocalTime(b)) => Some(a.cmp(b)),
        (Value::DateTime(s1, n1), Value::DateTime(s2, n2)) => Some((s1, n1).cmp(&(s2, n2))),
        (Value::LocalDateTime(s1, n1), Value::LocalDateTime(s2, n2)) => {
            Some((s1, n1).cmp(&(s2, n2)))
        }
        (Value::ZonedDateTime(s1, n1, _), Value::ZonedDateTime(s2, n2, _)) => {
            // Compare by UTC instant (ignore timezone for ordering).
            Some((s1, n1).cmp(&(s2, n2)))
        }
        (Value::ZonedTime(n1, t1), Value::ZonedTime(n2, t2)) => {
            // Normalize to UTC for comparison.
            let utc1 = normalize_zoned_time_utc(*n1, *t1);
            let utc2 = normalize_zoned_time_utc(*n2, *t2);
            Some(utc1.cmp(&utc2))
        }
        (Value::Duration(m1, n1), Value::Duration(m2, n2)) => Some((m1, n1).cmp(&(m2, n2))),
        (Value::List(a), Value::List(b)) => compare_value_slices(a, b),
        (Value::Record(a), Value::Record(b)) => compare_record_fields(a, b),
        (Value::Path(a), Value::Path(b)) => Some(compare_path_slices(a, b)),
        (Value::Extension(a), Value::Extension(b)) => a.cmp_ext(b.as_ref()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Decimal, Int256, Uint256};

    #[test]
    fn compares_mixed_numeric_values() {
        assert_eq!(
            compare_values(&Value::Int64(1), &Value::Float64(1.0)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_values(&Value::Float64(2.0), &Value::Int64(1)),
            Some(Ordering::Greater)
        );
    }

    #[cfg(feature = "f128")]
    #[test]
    fn compares_float128_without_f64_rounding() {
        assert_eq!(
            compare_values(
                &Value::Float128(1.0f128 + f128::EPSILON),
                &Value::Float128(1.0f128),
            ),
            Some(Ordering::Greater)
        );
    }

    #[cfg(feature = "f256")]
    #[test]
    fn compares_float256_without_f64_rounding() {
        assert_eq!(
            compare_values(
                &Value::Float256("1.0000000000000000000000000000000000001".parse().unwrap()),
                &Value::Float256("1.0000000000000000000000000000000000000".parse().unwrap()),
            ),
            Some(Ordering::Greater)
        );
    }

    #[cfg(feature = "f256")]
    #[test]
    fn compares_decimal_and_float256_using_float256() {
        assert_eq!(
            compare_values(
                &Value::Decimal(Decimal::parse("1.0000000000000000000000000001").unwrap()),
                &Value::Float256("1.0000000000000000000000000000".parse().unwrap()),
            ),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn rejects_cross_type_comparison() {
        assert_eq!(
            compare_values(&Value::Text("a".into()), &Value::Int64(1)),
            None
        );
    }

    #[test]
    fn compares_same_width_integers() {
        assert_eq!(
            compare_values(&Value::Int8(1), &Value::Int8(2)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(&Value::Uint32(100), &Value::Uint32(100)),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn compares_cross_width_signed() {
        assert_eq!(
            compare_values(&Value::Int8(127), &Value::Int64(128)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(&Value::Int128(1000), &Value::Int16(999)),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn compares_signed_vs_unsigned() {
        assert_eq!(
            compare_values(&Value::Int64(-1), &Value::Uint64(0)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(&Value::Uint64(100), &Value::Int64(100)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_values(&Value::Uint64(u64::MAX), &Value::Int64(1)),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn compares_256_bit_integers() {
        let a = Value::Int256(Int256::new(ethnum::I256::from(42)));
        let b = Value::Int256(Int256::new(ethnum::I256::from(43)));
        assert_eq!(compare_values(&a, &b), Some(Ordering::Less));

        let c = Value::Int64(42);
        assert_eq!(compare_values(&c, &a), Some(Ordering::Equal));

        let d = Value::Uint256(Uint256::new(ethnum::U256::from(100u128)));
        let e = Value::Uint128(100);
        assert_eq!(compare_values(&d, &e), Some(Ordering::Equal));
    }

    #[test]
    fn compares_float16() {
        let a = Value::Float16(half::f16::from_f32(1.0));
        let b = Value::Float16(half::f16::from_f32(2.0));
        assert_eq!(compare_values(&a, &b), Some(Ordering::Less));

        // Float16 x Float64
        let c = Value::Float64(1.0);
        assert_eq!(compare_values(&a, &c), Some(Ordering::Equal));
    }

    #[test]
    fn compares_decimals() {
        let a = Value::Decimal(Decimal::from_i64(10));
        let b = Value::Decimal(Decimal::from_i64(20));
        assert_eq!(compare_values(&a, &b), Some(Ordering::Less));

        // Int x Decimal
        let c = Value::Int64(10);
        assert_eq!(compare_values(&c, &a), Some(Ordering::Equal));
    }

    #[test]
    fn compares_temporal() {
        assert_eq!(
            compare_values(&Value::Date(100), &Value::Date(200)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(&Value::DateTime(1000, 0), &Value::DateTime(1000, 0)),
            Some(Ordering::Equal)
        );
    }

    // ── Float16 x Float16 ──────────────────────────────────────────────
    #[test]
    fn float16_x_float16() {
        let a = Value::Float16(half::f16::from_f32(1.5));
        let b = Value::Float16(half::f16::from_f32(2.5));
        let c = Value::Float16(half::f16::from_f32(1.5));
        assert_eq!(compare_values(&a, &b), Some(Ordering::Less));
        assert_eq!(compare_values(&b, &a), Some(Ordering::Greater));
        assert_eq!(compare_values(&a, &c), Some(Ordering::Equal));

        // NaN is incomparable
        let nan = Value::Float16(half::f16::NAN);
        assert_eq!(compare_values(&a, &nan), None);
        assert_eq!(compare_values(&nan, &nan), None);
    }

    // ── Float32 x Float32 ──────────────────────────────────────────────
    #[test]
    fn float32_x_float32() {
        let a = Value::Float32(1.5);
        let b = Value::Float32(2.5);
        let c = Value::Float32(1.5);
        assert_eq!(compare_values(&a, &b), Some(Ordering::Less));
        assert_eq!(compare_values(&b, &a), Some(Ordering::Greater));
        assert_eq!(compare_values(&a, &c), Some(Ordering::Equal));

        let nan = Value::Float32(f32::NAN);
        assert_eq!(compare_values(&a, &nan), None);
    }

    // ── Text comparisons ───────────────────────────────────────────────
    #[test]
    fn text_comparisons() {
        let a = Value::Text("apple".into());
        let b = Value::Text("banana".into());
        let c = Value::Text("apple".into());
        assert_eq!(compare_values(&a, &b), Some(Ordering::Less));
        assert_eq!(compare_values(&b, &a), Some(Ordering::Greater));
        assert_eq!(compare_values(&a, &c), Some(Ordering::Equal));

        // Empty vs non-empty
        let empty = Value::Text("".into());
        assert_eq!(compare_values(&empty, &a), Some(Ordering::Less));
    }

    // ── Bytes comparisons ──────────────────────────────────────────────
    #[test]
    fn bytes_comparisons() {
        let a = Value::Bytes(vec![1, 2, 3]);
        let b = Value::Bytes(vec![1, 2, 4]);
        let c = Value::Bytes(vec![1, 2, 3]);
        assert_eq!(compare_values(&a, &b), Some(Ordering::Less));
        assert_eq!(compare_values(&b, &a), Some(Ordering::Greater));
        assert_eq!(compare_values(&a, &c), Some(Ordering::Equal));

        let shorter = Value::Bytes(vec![1, 2]);
        assert_eq!(compare_values(&shorter, &a), Some(Ordering::Less));
    }

    // ── Date, Time, LocalTime ──────────────────────────────────────────
    #[test]
    fn date_comparisons() {
        assert_eq!(
            compare_values(&Value::Date(0), &Value::Date(0)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_values(&Value::Date(200), &Value::Date(100)),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn time_comparisons() {
        assert_eq!(
            compare_values(&Value::Time(1000), &Value::Time(2000)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(&Value::Time(5000), &Value::Time(5000)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_values(&Value::Time(9000), &Value::Time(1000)),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn local_time_comparisons() {
        assert_eq!(
            compare_values(&Value::LocalTime(100), &Value::LocalTime(200)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(&Value::LocalTime(500), &Value::LocalTime(500)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_values(&Value::LocalTime(999), &Value::LocalTime(1)),
            Some(Ordering::Greater)
        );
    }

    // ── LocalDateTime comparisons ──────────────────────────────────────
    #[test]
    fn local_datetime_comparisons() {
        // Same seconds, different nanos
        assert_eq!(
            compare_values(&Value::LocalDateTime(100, 0), &Value::LocalDateTime(100, 1)),
            Some(Ordering::Less)
        );
        // Different seconds
        assert_eq!(
            compare_values(
                &Value::LocalDateTime(200, 0),
                &Value::LocalDateTime(100, 999)
            ),
            Some(Ordering::Greater)
        );
        // Equal
        assert_eq!(
            compare_values(&Value::LocalDateTime(50, 50), &Value::LocalDateTime(50, 50)),
            Some(Ordering::Equal)
        );
    }

    // ── ZonedDateTime (timezone-ignored) ───────────────────────────────
    #[test]
    fn zoned_datetime_ignores_timezone() {
        // Same UTC instant but different timezone offsets — should be equal
        assert_eq!(
            compare_values(
                &Value::ZonedDateTime(1000, 0, 3600),
                &Value::ZonedDateTime(1000, 0, -3600)
            ),
            Some(Ordering::Equal)
        );
        // Different instants
        assert_eq!(
            compare_values(
                &Value::ZonedDateTime(999, 0, 0),
                &Value::ZonedDateTime(1000, 0, 0)
            ),
            Some(Ordering::Less)
        );
        // Same seconds, different nanos
        assert_eq!(
            compare_values(
                &Value::ZonedDateTime(1000, 500, 0),
                &Value::ZonedDateTime(1000, 100, 0)
            ),
            Some(Ordering::Greater)
        );
    }

    // ── ZonedTime (UTC normalization) ──────────────────────────────────
    #[test]
    fn zoned_time_normalizes_to_utc() {
        // Two times that differ in local representation but same UTC
        // utc = nanos - offset_seconds * 1_000_000_000
        // (10_000_000_000, +1) => utc = 10B - 1B = 9B
        // (9_000_000_000,  0)  => utc = 9B - 0   = 9B
        assert_eq!(
            compare_values(
                &Value::ZonedTime(10_000_000_000, 1),
                &Value::ZonedTime(9_000_000_000, 0)
            ),
            Some(Ordering::Equal)
        );
        // Different UTC values
        assert_eq!(
            compare_values(
                &Value::ZonedTime(5_000_000_000, 0),
                &Value::ZonedTime(10_000_000_000, 0)
            ),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(
                &Value::ZonedTime(20_000_000_000, 0),
                &Value::ZonedTime(10_000_000_000, 0)
            ),
            Some(Ordering::Greater)
        );
    }

    // ── Duration comparison ────────────────────────────────────────────
    #[test]
    fn duration_comparisons() {
        // months first, then nanos
        assert_eq!(
            compare_values(&Value::Duration(1, 0), &Value::Duration(2, 0)),
            Some(Ordering::Less)
        );
        // Same months, different nanos
        assert_eq!(
            compare_values(&Value::Duration(5, 100), &Value::Duration(5, 200)),
            Some(Ordering::Less)
        );
        // Equal
        assert_eq!(
            compare_values(&Value::Duration(3, 50), &Value::Duration(3, 50)),
            Some(Ordering::Equal)
        );
        // Greater
        assert_eq!(
            compare_values(&Value::Duration(10, 0), &Value::Duration(3, 999)),
            Some(Ordering::Greater)
        );
    }

    // ── List comparison (lexicographic) ────────────────────────────────
    #[test]
    fn list_comparisons() {
        let short = Value::List(vec![Value::Int32(1)]);
        let long = Value::List(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]);
        let empty = Value::List(vec![]);
        let also_short = Value::List(vec![Value::Int32(99)]);
        assert_eq!(compare_values(&short, &long), Some(Ordering::Less));
        assert_eq!(compare_values(&long, &short), Some(Ordering::Greater));
        assert_eq!(compare_values(&empty, &short), Some(Ordering::Less));
        assert_eq!(compare_values(&short, &also_short), Some(Ordering::Less));
    }

    #[test]
    fn path_comparisons_are_lexicographic() {
        let a = Value::Path(vec![crate::types::PathElement::Vertex(vec![1].into())]);
        let b = Value::Path(vec![crate::types::PathElement::Vertex(vec![2].into())]);
        let c = Value::Path(vec![
            crate::types::PathElement::Vertex(vec![1].into()),
            crate::types::PathElement::Edge(vec![1, 2].into()),
        ]);
        let d = Value::Path(vec![crate::types::PathElement::Edge(vec![1].into())]);
        assert_eq!(compare_values(&a, &b), Some(Ordering::Less));
        assert_eq!(compare_values(&b, &a), Some(Ordering::Greater));
        assert_eq!(compare_values(&a, &c), Some(Ordering::Less));
        assert_eq!(compare_values(&a, &d), Some(Ordering::Less));
    }

    #[test]
    fn zoned_time_comparison_wraps_utc_day_boundary() {
        let plus_one = Value::ZonedTime(30 * 60 * 1_000_000_000u64, 3600);
        let utc_prev_day = Value::ZonedTime((23 * 3600 + 30 * 60) * 1_000_000_000u64, 0);
        assert_eq!(
            compare_values(&plus_one, &utc_prev_day),
            Some(Ordering::Equal)
        );
    }

    // ── Integer x Decimal (signed) ─────────────────────────────────────
    #[test]
    fn signed_int_x_decimal() {
        let d = Value::Decimal(Decimal::from_i64(42));
        let i = Value::Int64(42);
        assert_eq!(compare_values(&i, &d), Some(Ordering::Equal));
        assert_eq!(compare_values(&d, &i), Some(Ordering::Equal));

        let smaller = Value::Int32(10);
        assert_eq!(compare_values(&smaller, &d), Some(Ordering::Less));
        assert_eq!(compare_values(&d, &smaller), Some(Ordering::Greater));

        let bigger = Value::Int64(100);
        assert_eq!(compare_values(&bigger, &d), Some(Ordering::Greater));
        assert_eq!(compare_values(&d, &bigger), Some(Ordering::Less));

        // Negative
        let neg = Value::Int64(-5);
        assert_eq!(compare_values(&neg, &d), Some(Ordering::Less));
    }

    // ── Integer x Decimal (unsigned) ───────────────────────────────────
    #[test]
    fn unsigned_int_x_decimal() {
        let d = Value::Decimal(Decimal::from_i64(50));
        let u = Value::Uint64(50);
        assert_eq!(compare_values(&u, &d), Some(Ordering::Equal));
        assert_eq!(compare_values(&d, &u), Some(Ordering::Equal));

        let smaller = Value::Uint32(10);
        assert_eq!(compare_values(&smaller, &d), Some(Ordering::Less));
        assert_eq!(compare_values(&d, &smaller), Some(Ordering::Greater));
    }

    // ── Float x Decimal ────────────────────────────────────────────────
    #[test]
    fn float_x_decimal() {
        let d = Value::Decimal(Decimal::from_i64(10));

        // Float16 x Decimal
        let f16 = Value::Float16(half::f16::from_f32(10.0));
        assert_eq!(compare_values(&f16, &d), Some(Ordering::Equal));
        assert_eq!(compare_values(&d, &f16), Some(Ordering::Equal));

        let f16_small = Value::Float16(half::f16::from_f32(5.0));
        assert_eq!(compare_values(&f16_small, &d), Some(Ordering::Less));
        assert_eq!(compare_values(&d, &f16_small), Some(Ordering::Greater));

        // Float32 x Decimal
        let f32v = Value::Float32(10.0);
        assert_eq!(compare_values(&f32v, &d), Some(Ordering::Equal));
        assert_eq!(compare_values(&d, &f32v), Some(Ordering::Equal));

        let f32_big = Value::Float32(20.0);
        assert_eq!(compare_values(&f32_big, &d), Some(Ordering::Greater));
        assert_eq!(compare_values(&d, &f32_big), Some(Ordering::Less));

        // Float64 x Decimal
        let f64v = Value::Float64(10.0);
        assert_eq!(compare_values(&f64v, &d), Some(Ordering::Equal));
        assert_eq!(compare_values(&d, &f64v), Some(Ordering::Equal));

        #[allow(clippy::approx_constant)]
        let f64_small = Value::Float64(3.14);
        assert_eq!(compare_values(&f64_small, &d), Some(Ordering::Less));
        assert_eq!(compare_values(&d, &f64_small), Some(Ordering::Greater));
    }

    // ── Bool comparisons ───────────────────────────────────────────────
    #[test]
    fn bool_comparisons() {
        assert_eq!(
            compare_values(&Value::Bool(false), &Value::Bool(true)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(&Value::Bool(true), &Value::Bool(false)),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_values(&Value::Bool(true), &Value::Bool(true)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_values(&Value::Bool(false), &Value::Bool(false)),
            Some(Ordering::Equal)
        );
    }

    // ── Null comparison ────────────────────────────────────────────────
    #[test]
    fn null_comparisons() {
        assert_eq!(
            compare_values(&Value::Null, &Value::Null),
            Some(Ordering::Equal)
        );
        // Null vs other type => None
        assert_eq!(compare_values(&Value::Null, &Value::Int64(0)), None);
        assert_eq!(compare_values(&Value::Int64(0), &Value::Null), None);
    }

    // ── Mixed-width integer comparisons ────────────────────────────────
    #[test]
    fn mixed_width_signed_integers() {
        // Int8 vs Int32
        assert_eq!(
            compare_values(&Value::Int8(10), &Value::Int32(10)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_values(&Value::Int8(-1), &Value::Int32(1)),
            Some(Ordering::Less)
        );
        // Int16 vs Int64
        assert_eq!(
            compare_values(&Value::Int16(1000), &Value::Int64(999)),
            Some(Ordering::Greater)
        );
        // Int8 vs Int128
        assert_eq!(
            compare_values(&Value::Int8(50), &Value::Int128(50)),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn mixed_width_unsigned_integers() {
        // Uint8 vs Uint32
        assert_eq!(
            compare_values(&Value::Uint8(10), &Value::Uint32(20)),
            Some(Ordering::Less)
        );
        // Uint16 vs Uint64
        assert_eq!(
            compare_values(&Value::Uint16(5000), &Value::Uint64(5000)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_values(&Value::Uint16(65535), &Value::Uint64(100)),
            Some(Ordering::Greater)
        );
    }

    // ── Signed vs Unsigned integer comparisons ─────────────────────────
    #[test]
    fn signed_vs_unsigned_cross_type() {
        // Int32 vs Uint32
        assert_eq!(
            compare_values(&Value::Int32(-1), &Value::Uint32(0)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(&Value::Int32(50), &Value::Uint32(50)),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_values(&Value::Uint32(100), &Value::Int32(99)),
            Some(Ordering::Greater)
        );
        // Uint vs signed (reversed argument order)
        assert_eq!(
            compare_values(&Value::Uint64(0), &Value::Int64(-1)),
            Some(Ordering::Greater)
        );
    }

    // ── Int256 x signed comparisons ────────────────────────────────────
    #[test]
    fn int256_x_signed() {
        let big = Value::Int256(Int256::new(ethnum::I256::from(1000)));
        let small = Value::Int64(500);
        assert_eq!(compare_values(&big, &small), Some(Ordering::Greater));
        assert_eq!(compare_values(&small, &big), Some(Ordering::Less));

        let equal = Value::Int128(1000);
        assert_eq!(compare_values(&big, &equal), Some(Ordering::Equal));
        assert_eq!(compare_values(&equal, &big), Some(Ordering::Equal));

        // Negative Int256
        let neg = Value::Int256(Int256::new(ethnum::I256::from(-100)));
        let pos = Value::Int64(1);
        assert_eq!(compare_values(&neg, &pos), Some(Ordering::Less));
        assert_eq!(compare_values(&pos, &neg), Some(Ordering::Greater));
    }

    // ── Uint256 x unsigned comparisons ─────────────────────────────────
    #[test]
    fn uint256_x_unsigned() {
        let big = Value::Uint256(Uint256::new(ethnum::U256::from(500u128)));
        let small = Value::Uint64(100);
        assert_eq!(compare_values(&big, &small), Some(Ordering::Greater));
        assert_eq!(compare_values(&small, &big), Some(Ordering::Less));

        let equal = Value::Uint128(500);
        assert_eq!(compare_values(&big, &equal), Some(Ordering::Equal));
        assert_eq!(compare_values(&equal, &big), Some(Ordering::Equal));
    }

    // ── Int256 vs Uint256 (signed vs unsigned 256-bit) ─────────────────
    #[test]
    fn int256_vs_uint256() {
        // Negative Int256 vs any Uint256 => Less
        let neg = Value::Int256(Int256::new(ethnum::I256::from(-1)));
        let uval = Value::Uint256(Uint256::new(ethnum::U256::from(0u128)));
        assert_eq!(compare_values(&neg, &uval), Some(Ordering::Less));
        assert_eq!(compare_values(&uval, &neg), Some(Ordering::Greater));

        // Positive Int256 vs Uint256 with same magnitude
        let pos = Value::Int256(Int256::new(ethnum::I256::from(42)));
        let u42 = Value::Uint256(Uint256::new(ethnum::U256::from(42u128)));
        assert_eq!(compare_values(&pos, &u42), Some(Ordering::Equal));
        assert_eq!(compare_values(&u42, &pos), Some(Ordering::Equal));

        // Positive Int256 < larger Uint256
        let u100 = Value::Uint256(Uint256::new(ethnum::U256::from(100u128)));
        assert_eq!(compare_values(&pos, &u100), Some(Ordering::Less));
        assert_eq!(compare_values(&u100, &pos), Some(Ordering::Greater));

        // Int256 vs small unsigned (non-256)
        let neg256 = Value::Int256(Int256::new(ethnum::I256::from(-999)));
        let u8val = Value::Uint8(5);
        assert_eq!(compare_values(&neg256, &u8val), Some(Ordering::Less));
        assert_eq!(compare_values(&u8val, &neg256), Some(Ordering::Greater));

        // Uint256 vs small signed
        let u256 = Value::Uint256(Uint256::new(ethnum::U256::from(50u128)));
        let i32val = Value::Int32(-10);
        assert_eq!(compare_values(&u256, &i32val), Some(Ordering::Greater));
        assert_eq!(compare_values(&i32val, &u256), Some(Ordering::Less));

        // Uint256 vs positive signed
        let i32pos = Value::Int32(50);
        assert_eq!(compare_values(&u256, &i32pos), Some(Ordering::Equal));
    }

    // ── Float16 x Float32 cross comparisons ────────────────────────────
    #[test]
    fn float16_x_float32() {
        let f16v = Value::Float16(half::f16::from_f32(3.0));
        let f32v = Value::Float32(3.0);
        assert_eq!(compare_values(&f16v, &f32v), Some(Ordering::Equal));
        assert_eq!(compare_values(&f32v, &f16v), Some(Ordering::Equal));

        let f32_big = Value::Float32(5.0);
        assert_eq!(compare_values(&f16v, &f32_big), Some(Ordering::Less));
        assert_eq!(compare_values(&f32_big, &f16v), Some(Ordering::Greater));
    }

    // ── Integer x Float (promotion to f64) ─────────────────────────────
    #[test]
    fn integer_x_float16() {
        let i = Value::Int32(10);
        let f = Value::Float16(half::f16::from_f32(10.0));
        assert_eq!(compare_values(&i, &f), Some(Ordering::Equal));
        assert_eq!(compare_values(&f, &i), Some(Ordering::Equal));
    }

    #[test]
    fn integer_x_float32() {
        let i = Value::Int64(5);
        let f = Value::Float32(10.0);
        assert_eq!(compare_values(&i, &f), Some(Ordering::Less));
        assert_eq!(compare_values(&f, &i), Some(Ordering::Greater));
    }

    #[test]
    fn compares_records_by_field_name_and_value() {
        let left = Value::Record(vec![
            ("b".into(), Value::Int64(2)),
            ("a".into(), Value::Int64(1)),
        ]);
        let right = Value::Record(vec![
            ("a".into(), Value::Int64(1)),
            ("b".into(), Value::Int64(2)),
        ]);
        assert_eq!(compare_values(&left, &right), Some(Ordering::Equal));

        assert_eq!(
            compare_values(
                &Value::Record(vec![("a".into(), Value::Int64(1))]),
                &Value::Record(vec![("a".into(), Value::Int64(2))]),
            ),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(
                &Value::Record(vec![("a".into(), Value::Int64(1))]),
                &Value::Record(vec![("b".into(), Value::Int64(1))]),
            ),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_values(
                &Value::Record(vec![("a".into(), Value::Int64(1))]),
                &Value::Record(vec![
                    ("a".into(), Value::Int64(1)),
                    ("b".into(), Value::Int64(2)),
                ]),
            ),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn compares_nested_records_and_lists() {
        assert_eq!(
            compare_values(
                &Value::Record(vec![(
                    "a".into(),
                    Value::List(vec![Value::Int64(1), Value::Int64(2)]),
                )]),
                &Value::Record(vec![(
                    "a".into(),
                    Value::List(vec![Value::Int64(1), Value::Int64(3)]),
                )]),
            ),
            Some(Ordering::Less)
        );
    }

    #[test]
    fn record_comparison_returns_none_for_incomparable_inner_values() {
        assert_eq!(
            compare_values(
                &Value::Record(vec![("a".into(), Value::Int64(1))]),
                &Value::Record(vec![("a".into(), Value::Text("x".into()))]),
            ),
            None
        );
        assert_eq!(
            compare_values(
                &Value::Record(vec![("a".into(), Value::Float64(f64::NAN))]),
                &Value::Record(vec![("a".into(), Value::Float64(f64::NAN))]),
            ),
            None
        );
    }
}
