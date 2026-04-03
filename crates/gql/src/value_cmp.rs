//! Value comparison logic supporting cross-width and cross-signedness integer
//! comparisons, mixed integer/float, and decimal promotions.

use std::cmp::Ordering;

use crate::Value;
use crate::types::Decimal;

/// Compare two integer [`Value`]s of any width/signedness and return their
/// ordering. Returns `None` if either operand is not an integer variant.
fn compare_int_values(left: &Value, right: &Value) -> Option<Ordering> {
    // Fast path: same discriminant.
    if std::mem::discriminant(left) == std::mem::discriminant(right) {
        return match (left, right) {
            (Value::Int8(a), Value::Int8(b)) => Some(a.cmp(b)),
            (Value::Int16(a), Value::Int16(b)) => Some(a.cmp(b)),
            (Value::Int32(a), Value::Int32(b)) => Some(a.cmp(b)),
            (Value::Int64(a), Value::Int64(b)) => Some(a.cmp(b)),
            (Value::Int128(a), Value::Int128(b)) => Some(a.cmp(b)),
            (Value::Int256(a), Value::Int256(b)) => Some(a.0.cmp(&b.0)),
            (Value::Uint8(a), Value::Uint8(b)) => Some(a.cmp(b)),
            (Value::Uint16(a), Value::Uint16(b)) => Some(a.cmp(b)),
            (Value::Uint32(a), Value::Uint32(b)) => Some(a.cmp(b)),
            (Value::Uint64(a), Value::Uint64(b)) => Some(a.cmp(b)),
            (Value::Uint128(a), Value::Uint128(b)) => Some(a.cmp(b)),
            (Value::Uint256(a), Value::Uint256(b)) => Some(a.0.cmp(&b.0)),
            _ => None,
        };
    }

    // Both signed (different widths) — promote to i128 or I256.
    if left.is_signed_int() && right.is_signed_int() {
        if let Value::Int256(a) = left {
            let b256 = int_to_i256(right)?;
            return Some(a.0.cmp(&b256));
        }
        if let Value::Int256(b) = right {
            let a256 = int_to_i256(left)?;
            return Some(a256.cmp(&b.0));
        }
        return Some(left.as_i128()?.cmp(&right.as_i128()?));
    }

    // Both unsigned (different widths) — promote to u128 or U256.
    if left.is_unsigned_int() && right.is_unsigned_int() {
        if let Value::Uint256(a) = left {
            let b256 = uint_to_u256(right)?;
            return Some(a.0.cmp(&b256));
        }
        if let Value::Uint256(b) = right {
            let a256 = uint_to_u256(left)?;
            return Some(a256.cmp(&b.0));
        }
        return Some(left.as_u128()?.cmp(&right.as_u128()?));
    }

    // Mixed signed x unsigned.
    compare_signed_unsigned(left, right)
}

fn compare_signed_unsigned(left: &Value, right: &Value) -> Option<Ordering> {
    if left.is_signed_int() && right.is_unsigned_int() {
        return Some(cmp_signed_vs_unsigned(left, right));
    }
    if left.is_unsigned_int() && right.is_signed_int() {
        return Some(cmp_signed_vs_unsigned(right, left).reverse());
    }
    None
}

/// Order a signed `s` vs unsigned `u`. Caller guarantees types.
fn cmp_signed_vs_unsigned(s: &Value, u: &Value) -> Ordering {
    // 256-bit cases.
    if let Value::Int256(sv) = s {
        if sv.0.is_negative() {
            return Ordering::Less;
        }
        let u256 = uint_to_u256(u).unwrap_or(ethnum::U256::ZERO);
        return sv.0.as_u256().cmp(&u256);
    }
    if let Value::Uint256(uv) = u {
        let sv128 = s.as_i128().unwrap_or(0);
        if sv128 < 0 {
            return Ordering::Less;
        }
        let sv_u256 = ethnum::U256::from(sv128 as u128);
        return sv_u256.cmp(&uv.0);
    }

    // Both fit in 128-bit.
    let sv = s.as_i128().unwrap_or(0);
    let uv = u.as_u128().unwrap_or(0);
    if sv < 0 {
        Ordering::Less
    } else {
        (sv as u128).cmp(&uv)
    }
}

fn int_to_i256(v: &Value) -> Option<ethnum::I256> {
    match v {
        Value::Int256(i) => Some(i.0),
        _ => v.as_i128().map(ethnum::I256::from),
    }
}

fn uint_to_u256(v: &Value) -> Option<ethnum::U256> {
    match v {
        Value::Uint256(u) => Some(u.0),
        _ => v.as_u128().map(ethnum::U256::from),
    }
}

/// Compares two [`Value`]s and returns their ordering, or `None` if the types
/// are incomparable (e.g. `Text` vs `Int64`).
///
/// Supports cross-width integer comparison, mixed integer/float promotion to
/// `f64`, and integer/decimal promotion.
pub fn compare_values(left: &Value, right: &Value) -> Option<Ordering> {
    // Integer x Integer (all width/signedness combos).
    if left.is_any_int() && right.is_any_int() {
        return compare_int_values(left, right);
    }

    match (left, right) {
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Float16(a), Value::Float16(b)) => a.partial_cmp(b),
        (Value::Float32(a), Value::Float32(b)) => a.partial_cmp(b),
        (Value::Float64(a), Value::Float64(b)) => a.partial_cmp(b),
        // Float16 x Float32/Float64: promote to f64
        (Value::Float16(a), Value::Float32(b)) => a.to_f64().partial_cmp(&(*b as f64)),
        (Value::Float32(a), Value::Float16(b)) => (*a as f64).partial_cmp(&b.to_f64()),
        (Value::Float16(a), Value::Float64(b)) => a.to_f64().partial_cmp(b),
        (Value::Float64(a), Value::Float16(b)) => a.partial_cmp(&b.to_f64()),
        // Float32 x Float64: promote to f64
        (Value::Float32(a), Value::Float64(b)) => (*a as f64).partial_cmp(b),
        (Value::Float64(a), Value::Float32(b)) => a.partial_cmp(&(*b as f64)),
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
            let utc1 = *n1 as i64 - (*t1 as i64) * 1_000_000_000;
            let utc2 = *n2 as i64 - (*t2 as i64) * 1_000_000_000;
            Some(utc1.cmp(&utc2))
        }
        (Value::Duration(m1, n1), Value::Duration(m2, n2)) => Some((m1, n1).cmp(&(m2, n2))),
        (Value::List(a), Value::List(b)) => Some(a.len().cmp(&b.len())),
        (Value::Path(a), Value::Path(b)) => Some(a.len().cmp(&b.len())),
        (Value::Decimal(a), Value::Decimal(b)) => Some(a.cmp(b)),
        (Value::Extension(a), Value::Extension(b)) => a.cmp_ext(b.as_ref()),

        // Integer x Float: promote both to f64
        (l, Value::Float16(b)) if l.is_any_int() => l.as_f64()?.partial_cmp(&b.to_f64()),
        (Value::Float16(a), r) if r.is_any_int() => a.to_f64().partial_cmp(&r.as_f64()?),
        (l, Value::Float32(b)) if l.is_any_int() => l.as_f64()?.partial_cmp(&(*b as f64)),
        (Value::Float32(a), r) if r.is_any_int() => (*a as f64).partial_cmp(&r.as_f64()?),
        (l, Value::Float64(b)) if l.is_any_int() => l.as_f64()?.partial_cmp(b),
        (Value::Float64(a), r) if r.is_any_int() => a.partial_cmp(&r.as_f64()?),

        // Signed int x Decimal
        (l, Value::Decimal(b)) if l.is_signed_int() => l
            .as_i128()
            .map(|a| Decimal::new(rust_decimal::Decimal::from(a)).cmp(b)),
        (Value::Decimal(a), r) if r.is_signed_int() => r
            .as_i128()
            .map(|b| a.cmp(&Decimal::new(rust_decimal::Decimal::from(b)))),

        // Unsigned int x Decimal
        (l, Value::Decimal(b)) if l.is_unsigned_int() => l
            .as_u128()
            .map(|a| Decimal::new(rust_decimal::Decimal::from(a)).cmp(b)),
        (Value::Decimal(a), r) if r.is_unsigned_int() => r
            .as_u128()
            .map(|b| a.cmp(&Decimal::new(rust_decimal::Decimal::from(b)))),

        // Float x Decimal: promote both to f64
        (Value::Float16(a), Value::Decimal(b)) => {
            b.to_f64().and_then(|bf| a.to_f64().partial_cmp(&bf))
        }
        (Value::Decimal(a), Value::Float16(b)) => {
            a.to_f64().and_then(|af| af.partial_cmp(&b.to_f64()))
        }
        (Value::Float32(a), Value::Decimal(b)) => {
            b.to_f64().and_then(|bf| (*a as f64).partial_cmp(&bf))
        }
        (Value::Decimal(a), Value::Float32(b)) => {
            a.to_f64().and_then(|af| af.partial_cmp(&(*b as f64)))
        }
        (Value::Float64(a), Value::Decimal(b)) => b.to_f64().and_then(|bf| a.partial_cmp(&bf)),
        (Value::Decimal(a), Value::Float64(b)) => a.to_f64().and_then(|af| af.partial_cmp(b)),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{Int256, Uint256};

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

    // ── List comparison (by length) ────────────────────────────────────
    #[test]
    fn list_comparisons() {
        let short = Value::List(vec![Value::Int32(1)]);
        let long = Value::List(vec![Value::Int32(1), Value::Int32(2), Value::Int32(3)]);
        let empty = Value::List(vec![]);
        let also_short = Value::List(vec![Value::Int32(99)]);
        assert_eq!(compare_values(&short, &long), Some(Ordering::Less));
        assert_eq!(compare_values(&long, &short), Some(Ordering::Greater));
        assert_eq!(compare_values(&empty, &short), Some(Ordering::Less));
        // Same length => Equal (content ignored)
        assert_eq!(compare_values(&short, &also_short), Some(Ordering::Equal));
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
}
