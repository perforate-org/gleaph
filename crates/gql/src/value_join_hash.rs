//! Stable hashing for join-key bucketing (HashJoin, DISTINCT seeds, etc.).
//!
//! Feed into any [`std::hash::Hasher`]. Not a Rust `Hash` impl on [`Value`]:
//! equality is [`PartialEq`] only; this mixes discriminants and payloads so that **when two
//! values compare equal under [`PartialEq`], they should hit the same bucket key** for typical
//! scalars. IEEE floats use **canonical positive zero** bits when `v == 0.0` so `+0` and `-0`
//! hash identically.

use crate::Value;
use crate::types::PathElement;
use half::f16;
use std::hash::Hasher;

#[inline]
fn canonical_f16_bits(v: f16) -> u16 {
    if v == f16::ZERO {
        f16::ZERO.to_bits()
    } else {
        v.to_bits()
    }
}

#[inline]
fn canonical_f32_bits(v: f32) -> u32 {
    if v == 0.0 {
        0.0f32.to_bits()
    } else {
        v.to_bits()
    }
}

#[inline]
fn canonical_f64_bits(v: f64) -> u64 {
    if v == 0.0 {
        0.0f64.to_bits()
    } else {
        v.to_bits()
    }
}

#[cfg(feature = "f128")]
#[inline]
fn canonical_f128_bits(v: f128) -> u128 {
    if v == 0.0 {
        0.0f128.to_bits()
    } else {
        v.to_bits()
    }
}

#[cfg(feature = "f256")]
#[inline]
fn canonical_f256_le_bytes(v: f256::f256) -> [u8; 32] {
    let zero = f256::f256::from(0.0f64);
    if v == zero {
        zero.to_le_bytes()
    } else {
        v.to_le_bytes()
    }
}

/// Mix a [`PathElement`] into `hasher` (must stay aligned with [`hash_value_for_join`] for paths).
pub fn hash_path_element_for_join<H: Hasher>(pe: &PathElement, hasher: &mut H) {
    match pe {
        PathElement::Vertex(id) => {
            hasher.write_u8(10);
            hasher.write_u64(id.len() as u64);
            hasher.write(id.as_ref());
        }
        PathElement::Edge(id) => {
            hasher.write_u8(11);
            hasher.write_u64(id.len() as u64);
            hasher.write(id.as_ref());
        }
    }
}

/// Mix a [`Value`] into `hasher` for join-style equality buckets.
pub fn hash_value_for_join<H: Hasher>(value: &Value, hasher: &mut H) {
    match value {
        Value::Null => hasher.write_u8(20),
        Value::Bool(b) => {
            hasher.write_u8(21);
            hasher.write_u8(u8::from(*b));
        }
        Value::Int8(v) => {
            hasher.write_u8(22);
            hasher.write_i8(*v);
        }
        Value::Int16(v) => {
            hasher.write_u8(23);
            hasher.write_i16(*v);
        }
        Value::Int32(v) => {
            hasher.write_u8(24);
            hasher.write_i32(*v);
        }
        Value::Int64(v) => {
            hasher.write_u8(25);
            hasher.write_i64(*v);
        }
        Value::Int128(v) => {
            hasher.write_u8(26);
            hasher.write_i128(*v);
        }
        Value::Int256(v) => {
            hasher.write_u8(27);
            hasher.write(v.0.to_le_bytes().as_slice());
        }
        Value::Uint8(v) => {
            hasher.write_u8(28);
            hasher.write_u8(*v);
        }
        Value::Uint16(v) => {
            hasher.write_u8(29);
            hasher.write_u16(*v);
        }
        Value::Uint32(v) => {
            hasher.write_u8(30);
            hasher.write_u32(*v);
        }
        Value::Uint64(v) => {
            hasher.write_u8(31);
            hasher.write_u64(*v);
        }
        Value::Uint128(v) => {
            hasher.write_u8(32);
            hasher.write_u128(*v);
        }
        Value::Uint256(v) => {
            hasher.write_u8(33);
            hasher.write(v.0.to_le_bytes().as_slice());
        }
        Value::Float16(v) => {
            hasher.write_u8(34);
            hasher.write_u16(canonical_f16_bits(*v));
        }
        Value::Float32(v) => {
            hasher.write_u8(35);
            hasher.write_u32(canonical_f32_bits(*v));
        }
        Value::Float64(v) => {
            hasher.write_u8(36);
            hasher.write_u64(canonical_f64_bits(*v));
        }
        #[cfg(feature = "f128")]
        Value::Float128(v) => {
            hasher.write_u8(37);
            hasher.write_u128(canonical_f128_bits(*v));
        }
        #[cfg(feature = "f256")]
        Value::Float256(v) => {
            hasher.write_u8(38);
            hasher.write(canonical_f256_le_bytes(*v).as_slice());
        }
        Value::Decimal(d) => {
            hasher.write_u8(39);
            hasher.write(&d.normalize().0.serialize());
        }
        Value::Text(s) => {
            hasher.write_u8(40);
            hasher.write_u64(s.len() as u64);
            hasher.write(s.as_bytes());
        }
        Value::Bytes(b) => {
            hasher.write_u8(41);
            hasher.write_u64(b.len() as u64);
            hasher.write(b.as_slice());
        }
        Value::Date(v) => {
            hasher.write_u8(42);
            hasher.write_i32(*v);
        }
        Value::Time(v) => {
            hasher.write_u8(43);
            hasher.write_u64(*v);
        }
        Value::LocalTime(v) => {
            hasher.write_u8(44);
            hasher.write_u64(*v);
        }
        Value::DateTime(s, n) => {
            hasher.write_u8(45);
            hasher.write_i64(*s);
            hasher.write_u32(*n);
        }
        Value::LocalDateTime(s, n) => {
            hasher.write_u8(46);
            hasher.write_i64(*s);
            hasher.write_u32(*n);
        }
        Value::ZonedDateTime(s, n, tz) => {
            hasher.write_u8(47);
            hasher.write_i64(*s);
            hasher.write_u32(*n);
            hasher.write_i32(*tz);
        }
        Value::ZonedTime(n, tz) => {
            hasher.write_u8(48);
            hasher.write_u64(*n);
            hasher.write_i32(*tz);
        }
        Value::Duration(m, n) => {
            hasher.write_u8(49);
            hasher.write_i32(*m);
            hasher.write_i64(*n);
        }
        Value::List(items) => {
            hasher.write_u8(50);
            hasher.write_u64(items.len() as u64);
            for item in items {
                hash_value_for_join(item, hasher);
            }
        }
        Value::Path(elements) => {
            hasher.write_u8(51);
            hasher.write_u64(elements.len() as u64);
            for pe in elements {
                hash_path_element_for_join(pe, hasher);
            }
        }
        Value::Record(fields) => {
            hasher.write_u8(52);
            hasher.write_u64(fields.len() as u64);
            for (name, field_val) in fields {
                hasher.write_u64(name.len() as u64);
                hasher.write(name.as_bytes());
                hash_value_for_join(field_val, hasher);
            }
        }
        Value::Extension(ext) => {
            hasher.write_u8(53);
            crate::ExtensionValue::hash_join_key(&**ext, hasher);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Decimal;
    use rapidhash::fast::RapidHasher;
    use std::hash::Hasher;

    fn hash64(v: &Value) -> u64 {
        let mut h = RapidHasher::default();
        hash_value_for_join(v, &mut h);
        h.finish()
    }

    #[test]
    fn float_plus_minus_zero_share_join_hash_f32() {
        let p = Value::Float32(0.0);
        let n = Value::Float32(-0.0);
        assert_eq!(p, n);
        assert_eq!(hash64(&p), hash64(&n));
    }

    #[test]
    fn float_plus_minus_zero_share_join_hash_f64() {
        let p = Value::Float64(0.0);
        let n = Value::Float64(-0.0);
        assert_eq!(p, n);
        assert_eq!(hash64(&p), hash64(&n));
    }

    #[test]
    fn float_plus_minus_zero_share_join_hash_f16() {
        let p = Value::Float16(f16::ZERO);
        let n = Value::Float16(-f16::ZERO);
        assert_eq!(p, n);
        assert_eq!(hash64(&p), hash64(&n));
    }

    #[cfg(feature = "f128")]
    #[test]
    fn float_plus_minus_zero_share_join_hash_f128() {
        let p = Value::Float128(0.0);
        let n = Value::Float128(-0.0);
        assert_eq!(p, n);
        assert_eq!(hash64(&p), hash64(&n));
    }

    #[cfg(feature = "f256")]
    #[test]
    fn float_plus_minus_zero_share_join_hash_f256() {
        let zero = f256::f256::from(0.0f64);
        let p = Value::Float256(zero);
        let n = Value::Float256(-zero);
        assert_eq!(p, n);
        assert_eq!(hash64(&p), hash64(&n));
    }

    #[test]
    fn decimal_equivalent_scales_share_join_hash() {
        let a = Value::Decimal(Decimal::parse("1.0").expect("parse"));
        let b = Value::Decimal(Decimal::parse("1.00").expect("parse"));
        assert_eq!(a, b);
        assert_eq!(hash64(&a), hash64(&b));
    }

    #[test]
    fn path_join_hash_distinguishes_vertex_and_edge_with_same_id() {
        let vertex = Value::Path(vec![crate::types::PathElement::Vertex(vec![1, 2].into())]);
        let edge = Value::Path(vec![crate::types::PathElement::Edge(vec![1, 2].into())]);
        assert_ne!(vertex, edge);
        assert_ne!(hash64(&vertex), hash64(&edge));
    }
}
