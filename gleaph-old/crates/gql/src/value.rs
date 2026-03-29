use gleaph_types::Value;
use std::cmp::Ordering;

/// Compare two integer [`Value`]s of any width/signedness and return their
/// ordering. Returns `None` if either operand is not an integer variant.
fn compare_int_values(left: &Value, right: &Value) -> Option<Ordering> {
    // Fast path: same discriminant (same type, same width).
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
        // If either is 256-bit, widen the other.
        if let Value::Int256(a) = left {
            let b256 = int_to_i256(right)?;
            return Some(a.0.cmp(&b256));
        }
        if let Value::Int256(b) = right {
            let a256 = int_to_i256(left)?;
            return Some(a256.cmp(&b.0));
        }
        // Both fit in i128.
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

    // Mixed signed × unsigned.
    compare_signed_unsigned(left, right)
}

/// Compare a signed integer value against an unsigned integer value (or vice
/// versa). Returns `None` if neither pattern matches.
fn compare_signed_unsigned(left: &Value, right: &Value) -> Option<Ordering> {
    // Determine which is signed and which is unsigned.
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
        // sv is non-negative, cast to U256 for comparison.
        let sv_u = sv.0.as_u256();
        return sv_u.cmp(&u256);
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
        // sv >= 0, safe to cast to u128.
        (sv as u128).cmp(&uv)
    }
}

/// Convert any signed integer value to `ethnum::I256`.
fn int_to_i256(v: &Value) -> Option<ethnum::I256> {
    match v {
        Value::Int256(i) => Some(i.0),
        _ => v.as_i128().map(ethnum::I256::from),
    }
}

/// Convert any unsigned integer value to `ethnum::U256`.
fn uint_to_u256(v: &Value) -> Option<ethnum::U256> {
    match v {
        Value::Uint256(u) => Some(u.0),
        _ => v.as_u128().map(ethnum::U256::from),
    }
}

/// Compares two [`Value`]s and returns their ordering, or `None` if the types
/// are incomparable (e.g. `Text` vs `Int64`).
///
/// Mixed integer/float comparisons are supported via promotion to `f64`.
pub fn compare_values(left: &Value, right: &Value) -> Option<Ordering> {
    // Integer × Integer (all width/signedness combos).
    if left.is_any_int() && right.is_any_int() {
        return compare_int_values(left, right);
    }

    match (left, right) {
        (Value::Null, Value::Null) => Some(Ordering::Equal),
        (Value::Bool(a), Value::Bool(b)) => Some(a.cmp(b)),
        (Value::Float32(a), Value::Float32(b)) => a.partial_cmp(b),
        (Value::Float64(a), Value::Float64(b)) => a.partial_cmp(b),
        // Float32 × Float64: promote to f64
        (Value::Float32(a), Value::Float64(b)) => (*a as f64).partial_cmp(b),
        (Value::Float64(a), Value::Float32(b)) => a.partial_cmp(&(*b as f64)),
        (Value::Text(a), Value::Text(b)) => Some(a.cmp(b)),
        (Value::Timestamp(a), Value::Timestamp(b)) => Some(a.cmp(b)),
        (Value::List(a), Value::List(b)) => Some(a.len().cmp(&b.len())),
        (Value::Path(a), Value::Path(b)) => Some(a.len().cmp(&b.len())),
        (Value::Bytes(a), Value::Bytes(b)) => Some(a.cmp(b)),
        (Value::Date(a), Value::Date(b)) => Some(a.cmp(b)),
        (Value::Time(a), Value::Time(b)) => Some(a.cmp(b)),
        (Value::DateTime(s1, n1), Value::DateTime(s2, n2)) => Some((s1, n1).cmp(&(s2, n2))),
        (Value::Duration(m1, n1), Value::Duration(m2, n2)) => Some((m1, n1).cmp(&(m2, n2))),
        (Value::Principal(a), Value::Principal(b)) => Some(a.cmp(b)),
        (Value::Decimal(a), Value::Decimal(b)) => Some(a.cmp(b)),

        // Integer × Float32: promote both to f64
        (l, Value::Float32(b)) if l.is_any_int() => l.as_f64()?.partial_cmp(&(*b as f64)),
        (Value::Float32(a), r) if r.is_any_int() => (*a as f64).partial_cmp(&r.as_f64()?),

        // Integer × Float64
        (l, Value::Float64(b)) if l.is_signed_int() => l.as_f64()?.partial_cmp(b),
        (Value::Float64(a), r) if r.is_signed_int() => a.partial_cmp(&r.as_f64()?),
        (l, Value::Float64(b)) if l.is_unsigned_int() => l.as_f64()?.partial_cmp(b),
        (Value::Float64(a), r) if r.is_unsigned_int() => a.partial_cmp(&r.as_f64()?),

        // Signed int × Timestamp
        (Value::Timestamp(a), r) if r.is_signed_int() => r
            .as_i128()
            .and_then(|b| u64::try_from(b).ok())
            .map(|b| a.cmp(&b)),
        (l, Value::Timestamp(b)) if l.is_signed_int() => l
            .as_i128()
            .and_then(|a| u64::try_from(a).ok())
            .map(|a| a.cmp(b)),

        // Signed int × Decimal
        (l, Value::Decimal(b)) if l.is_signed_int() => l
            .as_i128()
            .map(|a| gleaph_types::Decimal::new(rust_decimal::Decimal::from(a)).cmp(b)),
        (Value::Decimal(a), r) if r.is_signed_int() => r
            .as_i128()
            .map(|b| a.cmp(&gleaph_types::Decimal::new(rust_decimal::Decimal::from(b)))),

        // Unsigned int × Decimal
        (l, Value::Decimal(b)) if l.is_unsigned_int() => l
            .as_u128()
            .map(|a| gleaph_types::Decimal::new(rust_decimal::Decimal::from(a)).cmp(b)),
        (Value::Decimal(a), r) if r.is_unsigned_int() => r
            .as_u128()
            .map(|b| a.cmp(&gleaph_types::Decimal::new(rust_decimal::Decimal::from(b)))),

        // Float32 × Decimal
        (Value::Float32(a), Value::Decimal(b)) => {
            b.to_f64().and_then(|bf| (*a as f64).partial_cmp(&bf))
        }
        (Value::Decimal(a), Value::Float32(b)) => {
            a.to_f64().and_then(|af| af.partial_cmp(&(*b as f64)))
        }

        // Float64 × Decimal
        (Value::Float64(a), Value::Decimal(b)) => b.to_f64().and_then(|bf| a.partial_cmp(&bf)),
        (Value::Decimal(a), Value::Float64(b)) => a.to_f64().and_then(|af| af.partial_cmp(b)),

        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_types::{Int256, Uint256};

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
}
