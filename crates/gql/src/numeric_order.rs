//! Exact numeric normalization for comparisons and sortable index keys.

use crate::Value;
use num_bigint::BigUint;
use num_traits::{One, Zero};
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericOrderError {
    NonFiniteFloat,
    UnsupportedValue,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NormalizedNumeric {
    pub negative: bool,
    pub digits: String,
    pub point_pos: i32,
}

impl NormalizedNumeric {
    pub fn cmp_numeric(&self, other: &Self) -> Ordering {
        match (self.negative, other.negative) {
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => cmp_magnitude(self, other),
            (true, true) => cmp_magnitude(other, self),
        }
    }
}

pub fn normalized_numeric_parts(
    value: &Value,
) -> Result<Option<NormalizedNumeric>, NumericOrderError> {
    match value {
        value if value.is_signed_int() => signed_integer_parts(value),
        value if value.is_unsigned_int() => unsigned_integer_parts(value),
        Value::Decimal(value) => {
            let decimal = value.normalize().0;
            let mantissa = decimal.mantissa();
            if mantissa == 0 {
                return Ok(None);
            }
            Ok(Some(parts_from_digits_and_scale(
                mantissa < 0,
                mantissa.unsigned_abs().to_string(),
                decimal.scale(),
            )?))
        }
        Value::Float16(value) => {
            if !value.is_finite() {
                return Err(NumericOrderError::NonFiniteFloat);
            }
            binary_float_parts(
                value.is_sign_negative(),
                u64::from(value.to_bits() & 0x03ff),
                u32::from((value.to_bits() >> 10) & 0x001f),
                10,
                15,
            )
        }
        Value::Float32(value) => {
            if !value.is_finite() {
                return Err(NumericOrderError::NonFiniteFloat);
            }
            let bits = value.to_bits();
            binary_float_parts(
                value.is_sign_negative(),
                u64::from(bits & 0x007f_ffff),
                (bits >> 23) & 0x00ff,
                23,
                127,
            )
        }
        Value::Float64(value) => {
            if !value.is_finite() {
                return Err(NumericOrderError::NonFiniteFloat);
            }
            let bits = value.to_bits();
            binary_float_parts(
                value.is_sign_negative(),
                bits & 0x000f_ffff_ffff_ffff,
                ((bits >> 52) & 0x07ff) as u32,
                52,
                1023,
            )
        }
        #[cfg(feature = "f128")]
        Value::Float128(value) => {
            if !value.is_finite() {
                return Err(NumericOrderError::NonFiniteFloat);
            }
            let bits = value.to_bits();
            let fraction = BigUint::from(bits & ((1u128 << 112) - 1));
            binary_float_big_parts(
                value.is_sign_negative(),
                fraction,
                ((bits >> 112) & 0x7fff) as u32,
                112,
                16383,
            )
        }
        #[cfg(feature = "f256")]
        Value::Float256(value) => {
            if !value.is_finite() {
                return Err(NumericOrderError::NonFiniteFloat);
            }
            let (hi, lo) = value.to_bits();
            let hi_fraction_bits = 108usize;
            let fraction_mask = (1u128 << hi_fraction_bits) - 1;
            let fraction = (BigUint::from(hi & fraction_mask) << 128usize) | BigUint::from(lo);
            binary_float_big_parts(
                (hi >> 127) != 0,
                fraction,
                ((hi >> hi_fraction_bits) & 0x7ffff) as u32,
                236,
                262143,
            )
        }
        _ => Err(NumericOrderError::UnsupportedValue),
    }
}

pub fn compare_normalized_numeric(left: &Value, right: &Value) -> Option<Ordering> {
    let left = normalized_numeric_parts(left).ok()?;
    let right = normalized_numeric_parts(right).ok()?;
    match (left, right) {
        (None, None) => Some(Ordering::Equal),
        (None, Some(right)) => Some(if right.negative {
            Ordering::Greater
        } else {
            Ordering::Less
        }),
        (Some(left), None) => Some(if left.negative {
            Ordering::Less
        } else {
            Ordering::Greater
        }),
        (Some(left), Some(right)) => Some(left.cmp_numeric(&right)),
    }
}

fn signed_integer_parts(value: &Value) -> Result<Option<NormalizedNumeric>, NumericOrderError> {
    let signed = value.as_i256().ok_or(NumericOrderError::UnsupportedValue)?;
    if signed == ethnum::I256::ZERO {
        return Ok(None);
    }
    Ok(Some(parts_from_digits_and_scale(
        signed.is_negative(),
        signed.unsigned_abs().to_string(),
        0,
    )?))
}

fn unsigned_integer_parts(value: &Value) -> Result<Option<NormalizedNumeric>, NumericOrderError> {
    let unsigned = value.as_u256().ok_or(NumericOrderError::UnsupportedValue)?;
    if unsigned == ethnum::U256::ZERO {
        return Ok(None);
    }
    Ok(Some(parts_from_digits_and_scale(
        false,
        unsigned.to_string(),
        0,
    )?))
}

fn binary_float_parts(
    negative: bool,
    fraction: u64,
    biased_exponent: u32,
    fraction_bits: u32,
    exponent_bias: i32,
) -> Result<Option<NormalizedNumeric>, NumericOrderError> {
    binary_float_big_parts(
        negative,
        BigUint::from(fraction),
        biased_exponent,
        fraction_bits,
        exponent_bias,
    )
}

fn binary_float_big_parts(
    negative: bool,
    fraction: BigUint,
    biased_exponent: u32,
    fraction_bits: u32,
    exponent_bias: i32,
) -> Result<Option<NormalizedNumeric>, NumericOrderError> {
    if biased_exponent == 0 && fraction.is_zero() {
        return Ok(None);
    }

    let (significand, exponent2) = if biased_exponent == 0 {
        (
            fraction,
            1 - exponent_bias - i32::try_from(fraction_bits).unwrap(),
        )
    } else {
        (
            fraction | (BigUint::one() << usize::try_from(fraction_bits).unwrap()),
            i32::try_from(biased_exponent).unwrap()
                - exponent_bias
                - i32::try_from(fraction_bits).unwrap(),
        )
    };

    if significand.is_zero() {
        return Ok(None);
    }

    if exponent2 >= 0 {
        let magnitude = significand << usize::try_from(exponent2).unwrap();
        return Ok(Some(parts_from_digits_and_scale(
            negative,
            magnitude.to_string(),
            0,
        )?));
    }

    let scale = u32::try_from(-exponent2).map_err(|_| NumericOrderError::UnsupportedValue)?;
    let magnitude = significand * BigUint::from(5u8).pow(scale);
    Ok(Some(parts_from_digits_and_scale(
        negative,
        magnitude.to_string(),
        scale,
    )?))
}

fn parts_from_digits_and_scale(
    negative: bool,
    mut digits: String,
    mut scale: u32,
) -> Result<NormalizedNumeric, NumericOrderError> {
    let first_non_zero = digits
        .bytes()
        .position(|byte| byte != b'0')
        .ok_or(NumericOrderError::UnsupportedValue)?;
    if first_non_zero > 0 {
        digits.drain(..first_non_zero);
    }
    while scale > 0 && digits.ends_with('0') {
        digits.pop();
        scale -= 1;
    }
    let point_pos = i32::try_from(digits.len())
        .ok()
        .and_then(|len| len.checked_sub(i32::try_from(scale).ok()?))
        .ok_or(NumericOrderError::UnsupportedValue)?;
    Ok(NormalizedNumeric {
        negative,
        digits,
        point_pos,
    })
}

fn cmp_magnitude(left: &NormalizedNumeric, right: &NormalizedNumeric) -> Ordering {
    left.point_pos.cmp(&right.point_pos).then_with(|| {
        let max_len = left.digits.len().max(right.digits.len());
        for idx in 0..max_len {
            let a = left.digits.as_bytes().get(idx).copied().unwrap_or(b'0');
            let b = right.digits.as_bytes().get(idx).copied().unwrap_or(b'0');
            match a.cmp(&b) {
                Ordering::Equal => {}
                ordering => return ordering,
            }
        }
        Ordering::Equal
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Decimal;
    use std::cmp::Ordering;

    fn decimal(value: &str) -> Value {
        Value::Decimal(Decimal::parse(value).expect("decimal"))
    }

    #[test]
    fn exact_numeric_comparison_preserves_large_integer_float_boundary() {
        assert_eq!(
            compare_normalized_numeric(
                &Value::Int64(9_007_199_254_740_993),
                &Value::Float64(9_007_199_254_740_992.0),
            ),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn exact_numeric_comparison_distinguishes_binary_and_decimal_literals() {
        assert_eq!(
            compare_normalized_numeric(&decimal("0.1"), &Value::Float64(0.1)),
            Some(Ordering::Less)
        );
        assert_eq!(
            compare_normalized_numeric(&decimal("0.5"), &Value::Float64(0.5)),
            Some(Ordering::Equal)
        );
    }

    #[test]
    fn exact_numeric_comparison_orders_mixed_float_int_decimal_values() {
        let values = [
            Value::Float64(-1.5),
            Value::Int64(-1),
            Value::Float64(-0.0),
            Value::Int64(0),
            Value::Float32(0.0),
            decimal("0.5"),
            Value::Int64(1),
            Value::Float16(half::f16::from_f32(1.5)),
        ];
        for pair in values.windows(2) {
            let ordering = compare_normalized_numeric(&pair[0], &pair[1]);
            if matches!(
                (&pair[0], &pair[1]),
                (Value::Float64(_), Value::Int64(0)) | (Value::Int64(0), Value::Float32(_))
            ) {
                assert_eq!(ordering, Some(Ordering::Equal));
            } else {
                assert_eq!(ordering, Some(Ordering::Less));
            }
        }
    }

    #[test]
    fn exact_numeric_comparison_handles_subnormal_float() {
        assert_eq!(
            compare_normalized_numeric(&Value::Float64(f64::MIN_POSITIVE / 2.0), &Value::Int64(0)),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn exact_numeric_comparison_rejects_non_finite_float() {
        assert_eq!(
            compare_normalized_numeric(&Value::Float64(f64::NAN), &Value::Int64(0)),
            None
        );
        assert_eq!(
            compare_normalized_numeric(&Value::Float64(f64::INFINITY), &Value::Int64(0)),
            None
        );
    }

    #[cfg(feature = "f128")]
    #[test]
    fn exact_numeric_comparison_supports_float128() {
        assert_eq!(
            compare_normalized_numeric(&Value::Float128(1.5f128), &decimal("1.5")),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_normalized_numeric(&Value::Float128(1.5f128), &decimal("2.0")),
            Some(Ordering::Less)
        );
    }

    #[cfg(feature = "f256")]
    #[test]
    fn exact_numeric_comparison_supports_float256() {
        assert_eq!(
            compare_normalized_numeric(
                &Value::Float256("1.5".parse::<f256::f256>().expect("f256")),
                &decimal("1.5"),
            ),
            Some(Ordering::Equal)
        );
        assert_eq!(
            compare_normalized_numeric(
                &Value::Float256("1.5".parse::<f256::f256>().expect("f256")),
                &decimal("2.0"),
            ),
            Some(Ordering::Less)
        );
    }
}
