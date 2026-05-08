//! Numeric operator evaluation for [`Value`].

use crate::Value;
use crate::ast::{BinaryOp, UnaryOp};
use crate::types::{Decimal, Int256, Uint256, narrow_signed, narrow_unsigned};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NumericOpError {
    InvalidOperand,
    DivisionByZero,
    Overflow,
    NonFinite,
    /// Operand is numeric but cannot be represented in the required form (e.g. decimal→float,
    /// wide int→float, or `f256` string roundtrip).
    UnsupportedConversion,
}

pub fn eval_unary_numeric(op: UnaryOp, value: Value) -> Result<Value, NumericOpError> {
    match op {
        UnaryOp::Pos => {
            if value == Value::Null || value.is_numeric() {
                Ok(value)
            } else {
                Err(NumericOpError::InvalidOperand)
            }
        }
        UnaryOp::Neg => eval_neg(value),
    }
}

pub fn eval_binary_numeric(
    left: Value,
    op: BinaryOp,
    right: Value,
) -> Result<Value, NumericOpError> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }

    if (matches!(left, Value::Decimal(_)) || matches!(right, Value::Decimal(_)))
        && (left.is_float() || right.is_float())
    {
        return eval_float_binary(left, op, right);
    }

    if matches!(left, Value::Decimal(_)) || matches!(right, Value::Decimal(_)) {
        return eval_decimal_binary(left, op, right);
    }

    if left.is_any_int() && right.is_any_int() {
        return eval_integer_binary(left, op, right);
    }

    if left.is_float() || right.is_float() {
        return eval_float_binary(left, op, right);
    }

    Err(NumericOpError::InvalidOperand)
}

fn eval_neg(value: Value) -> Result<Value, NumericOpError> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    match value {
        Value::Decimal(value) => Ok(Value::Decimal(Decimal::new(-value.0))),
        Value::Float16(value) => finite_float16(-value),
        Value::Float32(value) => finite_float32(-value),
        Value::Float64(value) => finite_float64(-value),
        #[cfg(feature = "f128")]
        Value::Float128(value) => finite_float128(-value),
        #[cfg(feature = "f256")]
        Value::Float256(value) => finite_float256(-value),
        Value::Int256(value) => value
            .0
            .checked_neg()
            .map(|value| Value::Int256(Int256::new(value)))
            .ok_or(NumericOpError::Overflow),
        value if value.is_signed_int() => {
            let width = value.int_width().unwrap_or(128);
            value
                .as_i128()
                .and_then(i128::checked_neg)
                .and_then(|value| narrow_signed_min_width(value, width))
                .ok_or(NumericOpError::Overflow)
        }
        Value::Uint256(value) => {
            if value.0 == ethnum::U256::ZERO {
                Ok(Value::Uint256(value))
            } else {
                ethnum::I256::try_from(value.0)
                    .ok()
                    .and_then(ethnum::I256::checked_neg)
                    .map(|value| Value::Int256(Int256::new(value)))
                    .ok_or(NumericOpError::Overflow)
            }
        }
        value if value.is_unsigned_int() => {
            let unsigned = value
                .as_u128()
                .and_then(|value| i128::try_from(value).ok())
                .ok_or(NumericOpError::Overflow)?;
            narrow_signed_min_width(-unsigned, 128).ok_or(NumericOpError::Overflow)
        }
        _ => Err(NumericOpError::InvalidOperand),
    }
}

fn eval_decimal_binary(left: Value, op: BinaryOp, right: Value) -> Result<Value, NumericOpError> {
    let left = value_to_decimal(&left).ok_or(NumericOpError::InvalidOperand)?;
    let right = value_to_decimal(&right).ok_or(NumericOpError::InvalidOperand)?;

    let value = match op {
        BinaryOp::Add => left.0.checked_add(right.0),
        BinaryOp::Sub => left.0.checked_sub(right.0),
        BinaryOp::Mul => left.0.checked_mul(right.0),
        BinaryOp::Div => {
            if right.0.is_zero() {
                return Err(NumericOpError::DivisionByZero);
            }
            left.0.checked_div(right.0)
        }
    };
    value
        .map(|value| Value::Decimal(Decimal::new(value).normalize()))
        .ok_or(NumericOpError::Overflow)
}

fn value_to_decimal(value: &Value) -> Option<Decimal> {
    match value {
        Value::Decimal(value) => Some(*value),
        value if value.is_signed_int() => value.as_i128().map(Decimal::from_i128),
        value if value.is_unsigned_int() => value.as_u128().map(Decimal::from_u128),
        _ => None,
    }
}

fn eval_integer_binary(left: Value, op: BinaryOp, right: Value) -> Result<Value, NumericOpError> {
    if matches!(left, Value::Int256(_)) || matches!(right, Value::Int256(_)) {
        return eval_i256_binary(left, op, right);
    }
    if matches!(left, Value::Uint256(_)) || matches!(right, Value::Uint256(_)) {
        if left.is_unsigned_int() && right.is_unsigned_int() {
            return eval_u256_binary(left, op, right);
        }
        return eval_i256_binary(left, op, right);
    }
    if left.is_unsigned_int() && right.is_unsigned_int() {
        return eval_u128_binary(left, op, right);
    }
    eval_i128_binary(left, op, right)
}

fn eval_i128_binary(left: Value, op: BinaryOp, right: Value) -> Result<Value, NumericOpError> {
    let width = max_int_width(&left, &right);
    let left_value = left;
    let right_value = right;
    let left = signed_i128(&left_value).ok_or(NumericOpError::InvalidOperand)?;
    let right = signed_i128(&right_value).ok_or(NumericOpError::InvalidOperand)?;
    let value = match op {
        BinaryOp::Add => left.checked_add(right),
        BinaryOp::Sub => left.checked_sub(right),
        BinaryOp::Mul => left.checked_mul(right),
        BinaryOp::Div => {
            if right == 0 {
                return Err(NumericOpError::DivisionByZero);
            }
            let Some(remainder) = left.checked_rem(right) else {
                return eval_i256_binary(left_value, op, right_value);
            };
            if remainder == 0 {
                return match left.checked_div(right) {
                    Some(value) => {
                        narrow_signed_min_width(value, width).ok_or(NumericOpError::Overflow)
                    }
                    None => eval_i256_binary(left_value, op, right_value),
                };
            }
            let value = Decimal::from_i128(left)
                .0
                .checked_div(Decimal::from_i128(right).0)
                .ok_or(NumericOpError::Overflow)?;
            return Ok(Value::Decimal(Decimal::new(value).normalize()));
        }
    };
    match value.and_then(|value| narrow_signed_min_width(value, width)) {
        Some(value) => Ok(value),
        None => eval_i256_binary(left_value, op, right_value),
    }
}

fn eval_u128_binary(left: Value, op: BinaryOp, right: Value) -> Result<Value, NumericOpError> {
    let width = max_int_width(&left, &right);
    let left_value = left;
    let right_value = right;
    let left = left_value.as_u128().ok_or(NumericOpError::InvalidOperand)?;
    let right = right_value
        .as_u128()
        .ok_or(NumericOpError::InvalidOperand)?;
    match op {
        BinaryOp::Add => left
            .checked_add(right)
            .and_then(|value| narrow_unsigned_min_width(value, width))
            .ok_or(NumericOpError::Overflow)
            .or_else(|_| eval_u256_binary(left_value, op, right_value)),
        BinaryOp::Sub => {
            if let Some(value) = left.checked_sub(right) {
                return narrow_unsigned_min_width(value, width).ok_or(NumericOpError::Overflow);
            }
            let value = right
                .checked_sub(left)
                .and_then(|value| i128::try_from(value).ok())
                .and_then(i128::checked_neg)
                .and_then(|value| narrow_signed_min_width(value, 128));
            value
                .ok_or(NumericOpError::Overflow)
                .or_else(|_| eval_u256_binary(left_value, op, right_value))
        }
        BinaryOp::Mul => left
            .checked_mul(right)
            .and_then(|value| narrow_unsigned_min_width(value, width))
            .ok_or(NumericOpError::Overflow)
            .or_else(|_| eval_u256_binary(left_value, op, right_value)),
        BinaryOp::Div => {
            if right == 0 {
                return Err(NumericOpError::DivisionByZero);
            }
            if left % right == 0 {
                return left
                    .checked_div(right)
                    .and_then(|value| narrow_unsigned_min_width(value, width))
                    .ok_or(NumericOpError::Overflow);
            }
            let value = Decimal::from_u128(left)
                .0
                .checked_div(Decimal::from_u128(right).0)
                .ok_or(NumericOpError::Overflow)?;
            Ok(Value::Decimal(Decimal::new(value).normalize()))
        }
    }
}

fn eval_i256_binary(left: Value, op: BinaryOp, right: Value) -> Result<Value, NumericOpError> {
    let left = left.as_i256().ok_or(NumericOpError::InvalidOperand)?;
    let right = right.as_i256().ok_or(NumericOpError::InvalidOperand)?;
    let value = match op {
        BinaryOp::Add => left.checked_add(right),
        BinaryOp::Sub => left.checked_sub(right),
        BinaryOp::Mul => left.checked_mul(right),
        BinaryOp::Div => {
            if right == ethnum::I256::ZERO {
                return Err(NumericOpError::DivisionByZero);
            }
            if left.checked_rem(right) == Some(ethnum::I256::ZERO) {
                return left
                    .checked_div(right)
                    .map(|value| Value::Int256(Int256::new(value)))
                    .ok_or(NumericOpError::Overflow);
            }
            #[cfg(feature = "f256")]
            {
                let value = i256_to_f256(left)? / i256_to_f256(right)?;
                return finite_float256(value);
            }
            #[cfg(not(feature = "f256"))]
            {
                return Err(NumericOpError::InvalidOperand);
            }
        }
    };
    value
        .map(|value| Value::Int256(Int256::new(value)))
        .ok_or(NumericOpError::Overflow)
}

fn eval_u256_binary(left: Value, op: BinaryOp, right: Value) -> Result<Value, NumericOpError> {
    let left = left.as_u256().ok_or(NumericOpError::InvalidOperand)?;
    let right = right.as_u256().ok_or(NumericOpError::InvalidOperand)?;
    let value = match op {
        BinaryOp::Add => left.checked_add(right),
        BinaryOp::Sub => {
            return if let Some(value) = left.checked_sub(right) {
                Ok(Value::Uint256(Uint256::new(value)))
            } else {
                let value = ethnum::I256::try_from(right - left)
                    .ok()
                    .and_then(ethnum::I256::checked_neg)
                    .map(|value| Value::Int256(Int256::new(value)));
                value.ok_or(NumericOpError::Overflow)
            };
        }
        BinaryOp::Mul => left.checked_mul(right),
        BinaryOp::Div => {
            if right == ethnum::U256::ZERO {
                return Err(NumericOpError::DivisionByZero);
            }
            if left.checked_rem(right) == Some(ethnum::U256::ZERO) {
                return left
                    .checked_div(right)
                    .map(|value| Value::Uint256(Uint256::new(value)))
                    .ok_or(NumericOpError::Overflow);
            }
            #[cfg(feature = "f256")]
            {
                let value = u256_to_f256(left)? / u256_to_f256(right)?;
                return finite_float256(value);
            }
            #[cfg(not(feature = "f256"))]
            {
                return Err(NumericOpError::InvalidOperand);
            }
        }
    };
    value
        .map(|value| Value::Uint256(Uint256::new(value)))
        .ok_or(NumericOpError::Overflow)
}

fn eval_float_binary(left: Value, op: BinaryOp, right: Value) -> Result<Value, NumericOpError> {
    match max_float_rank(&left, &right) {
        Some(FloatRank::F16) => {
            let left = float16_value(&left).ok_or(NumericOpError::UnsupportedConversion)?;
            let right = float16_value(&right).ok_or(NumericOpError::UnsupportedConversion)?;
            finite_float16(apply_float_op(left, op, right))
        }
        Some(FloatRank::F32) => {
            let left = float32_value(&left).ok_or(NumericOpError::UnsupportedConversion)?;
            let right = float32_value(&right).ok_or(NumericOpError::UnsupportedConversion)?;
            finite_float32(apply_float_op(left, op, right))
        }
        Some(FloatRank::F64) => {
            let left = left.as_f64().ok_or(NumericOpError::UnsupportedConversion)?;
            let right = right.as_f64().ok_or(NumericOpError::UnsupportedConversion)?;
            finite_float64(apply_float_op(left, op, right))
        }
        #[cfg(feature = "f128")]
        Some(FloatRank::F128) => {
            let left = left.as_f128().ok_or(NumericOpError::UnsupportedConversion)?;
            let right = right.as_f128().ok_or(NumericOpError::UnsupportedConversion)?;
            finite_float128(apply_float_op(left, op, right))
        }
        #[cfg(feature = "f256")]
        Some(FloatRank::F256) => {
            let left = left.as_f256().ok_or(NumericOpError::UnsupportedConversion)?;
            let right = right.as_f256().ok_or(NumericOpError::UnsupportedConversion)?;
            finite_float256(apply_float_op(left, op, right))
        }
        None => Err(NumericOpError::InvalidOperand),
    }
}

fn apply_float_op<T>(left: T, op: BinaryOp, right: T) -> T
where
    T: Copy
        + std::ops::Add<Output = T>
        + std::ops::Sub<Output = T>
        + std::ops::Mul<Output = T>
        + std::ops::Div<Output = T>,
{
    match op {
        BinaryOp::Add => left + right,
        BinaryOp::Sub => left - right,
        BinaryOp::Mul => left * right,
        BinaryOp::Div => left / right,
    }
}

fn signed_i128(value: &Value) -> Option<i128> {
    value
        .as_i128()
        .or_else(|| value.as_u128().and_then(|value| i128::try_from(value).ok()))
}

fn max_int_width(left: &Value, right: &Value) -> u16 {
    left.int_width()
        .unwrap_or(128)
        .max(right.int_width().unwrap_or(128))
}

fn narrow_signed_min_width(value: i128, min_width: u16) -> Option<Value> {
    [8, 16, 32, 64, 128]
        .into_iter()
        .find(|width| *width >= min_width && narrow_signed(value, *width).is_some())
        .and_then(|width| narrow_signed(value, width))
}

fn narrow_unsigned_min_width(value: u128, min_width: u16) -> Option<Value> {
    [8, 16, 32, 64, 128]
        .into_iter()
        .find(|width| *width >= min_width && narrow_unsigned(value, *width).is_some())
        .and_then(|width| narrow_unsigned(value, width))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
enum FloatRank {
    F16,
    F32,
    F64,
    #[cfg(feature = "f128")]
    F128,
    #[cfg(feature = "f256")]
    F256,
}

fn max_float_rank(left: &Value, right: &Value) -> Option<FloatRank> {
    Some(float_rank(left)?.max(float_rank(right)?))
}

fn float_rank(value: &Value) -> Option<FloatRank> {
    match value {
        Value::Float16(_) => Some(FloatRank::F16),
        Value::Float32(_) => Some(FloatRank::F32),
        Value::Float64(_) | Value::Decimal(_) => Some(FloatRank::F64),
        #[cfg(feature = "f128")]
        Value::Float128(_) => Some(FloatRank::F128),
        #[cfg(feature = "f256")]
        Value::Float256(_) => Some(FloatRank::F256),
        value if value.is_any_int() => Some(FloatRank::F16),
        _ => None,
    }
}

fn float16_value(value: &Value) -> Option<half::f16> {
    match value {
        Value::Float16(value) => Some(*value),
        value => value.as_f64().map(half::f16::from_f64),
    }
}

fn float32_value(value: &Value) -> Option<f32> {
    match value {
        Value::Float16(value) => Some(value.to_f32()),
        Value::Float32(value) => Some(*value),
        value => value.as_f64().map(|value| value as f32),
    }
}

#[cfg(feature = "f256")]
fn i256_to_f256(value: ethnum::I256) -> Result<f256::f256, NumericOpError> {
    value
        .to_string()
        .parse::<f256::f256>()
        .map_err(|_| NumericOpError::UnsupportedConversion)
}

#[cfg(feature = "f256")]
fn u256_to_f256(value: ethnum::U256) -> Result<f256::f256, NumericOpError> {
    value
        .to_string()
        .parse::<f256::f256>()
        .map_err(|_| NumericOpError::UnsupportedConversion)
}

fn finite_float16(value: half::f16) -> Result<Value, NumericOpError> {
    if value.is_finite() {
        Ok(Value::Float16(value))
    } else {
        Err(NumericOpError::NonFinite)
    }
}

fn finite_float32(value: f32) -> Result<Value, NumericOpError> {
    if value.is_finite() {
        Ok(Value::Float32(value))
    } else {
        Err(NumericOpError::NonFinite)
    }
}

fn finite_float64(value: f64) -> Result<Value, NumericOpError> {
    if value.is_finite() {
        Ok(Value::Float64(value))
    } else {
        Err(NumericOpError::NonFinite)
    }
}

#[cfg(feature = "f128")]
fn finite_float128(value: f128) -> Result<Value, NumericOpError> {
    if value.is_finite() {
        Ok(Value::Float128(value))
    } else {
        Err(NumericOpError::NonFinite)
    }
}

#[cfg(feature = "f256")]
fn finite_float256(value: f256::f256) -> Result<Value, NumericOpError> {
    if value.is_finite() {
        Ok(Value::Float256(value))
    } else {
        Err(NumericOpError::NonFinite)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widens_integer_results_without_losing_precision() {
        assert_eq!(
            eval_binary_numeric(Value::Int8(120), BinaryOp::Add, Value::Int8(10)).unwrap(),
            Value::Int16(130)
        );
        assert_eq!(
            eval_binary_numeric(Value::Uint8(250), BinaryOp::Add, Value::Uint8(10)).unwrap(),
            Value::Uint16(260)
        );
        assert_eq!(
            eval_binary_numeric(Value::Int128(i128::MAX), BinaryOp::Add, Value::Int64(1)).unwrap(),
            Value::Int256(Int256::new(ethnum::I256::from(i128::MAX) + 1))
        );
    }

    #[test]
    fn returns_decimal_for_fractional_128_bit_integer_division() {
        assert_eq!(
            eval_binary_numeric(Value::Int64(1), BinaryOp::Div, Value::Int64(4)).unwrap(),
            Value::Decimal(Decimal::parse("0.25").unwrap())
        );
    }

    #[cfg(feature = "f256")]
    #[test]
    fn returns_float256_for_fractional_256_bit_integer_division() {
        assert_eq!(
            eval_binary_numeric(
                Value::Int256(Int256::new(ethnum::I256::from(1))),
                BinaryOp::Div,
                Value::Int256(Int256::new(ethnum::I256::from(4))),
            )
            .unwrap(),
            Value::Float256("0.25".parse::<f256::f256>().unwrap())
        );
    }

    #[cfg(feature = "f128")]
    #[test]
    fn preserves_float128_width() {
        assert_eq!(
            eval_binary_numeric(
                Value::Float128(1.5f128),
                BinaryOp::Add,
                Value::Float128(2.25f128),
            )
            .unwrap(),
            Value::Float128(3.75f128)
        );
    }

    #[cfg(feature = "f256")]
    #[test]
    fn promotes_decimal_and_float256_to_float256() {
        assert_eq!(
            eval_binary_numeric(
                Value::Decimal(Decimal::parse("1.25").unwrap()),
                BinaryOp::Add,
                Value::Float256("2.25".parse::<f256::f256>().unwrap()),
            )
            .unwrap(),
            Value::Float256("3.5".parse::<f256::f256>().unwrap())
        );
    }
}
