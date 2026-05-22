//! Planner-independent expression evaluation primitives for plan execution.
//!
//! This module owns value-level runtime semantics shared by query and mutation
//! execution. It intentionally knows nothing about physical plans, graph
//! storage, bindings, or planner optimization metadata.

use gleaph_gql::Value;
use gleaph_gql::ast::{BinaryOp, CmpOp, UnaryOp, ValueType};
use gleaph_gql::numeric_ops::{
    NumericOpError, eval_abs_numeric, eval_binary_numeric, eval_unary_numeric,
};
use gleaph_gql::types::{narrow_signed, narrow_unsigned};
use gleaph_gql::value_cmp::compare_values;
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExprEvaluationError {
    InvalidValue,
    DivisionByZero,
    NumericOverflow,
    NumericPrecisionOverflow,
    NonFiniteNumeric,
    IncomparableValues,
    UnsupportedNumericConversion,
}

pub(crate) fn eval_not_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    match value {
        Value::Bool(value) => Ok(Value::Bool(!value)),
        Value::Null => Ok(Value::Null),
        _ => invalid_expr_value(),
    }
}

pub(crate) fn eval_and_expr(left: Value, right: Value) -> Result<Value, ExprEvaluationError> {
    match (truthy(left)?, truthy(right)?) {
        (Some(false), _) | (_, Some(false)) => Ok(Value::Bool(false)),
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(true), Some(true)) => Ok(Value::Bool(true)),
    }
}

pub(crate) fn eval_or_expr(left: Value, right: Value) -> Result<Value, ExprEvaluationError> {
    match (truthy(left)?, truthy(right)?) {
        (Some(true), _) | (_, Some(true)) => Ok(Value::Bool(true)),
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(false), Some(false)) => Ok(Value::Bool(false)),
    }
}

pub(crate) fn eval_xor_expr(left: Value, right: Value) -> Result<Value, ExprEvaluationError> {
    match (truthy(left)?, truthy(right)?) {
        (Some(left), Some(right)) => Ok(Value::Bool(left ^ right)),
        _ => Ok(Value::Null),
    }
}

pub(crate) fn truthy(value: Value) -> Result<Option<bool>, ExprEvaluationError> {
    match value {
        Value::Bool(value) => Ok(Some(value)),
        Value::Null => Ok(None),
        _ => invalid_expr_value().map(|_| None),
    }
}

pub(crate) fn eval_compare_expr(
    left: Value,
    op: CmpOp,
    right: Value,
) -> Result<Value, ExprEvaluationError> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }

    let Some(ordering) = compare_property_values(&left, &right) else {
        return Err(ExprEvaluationError::IncomparableValues);
    };
    let matched = match op {
        CmpOp::Eq => ordering == Ordering::Equal,
        CmpOp::Ne => ordering != Ordering::Equal,
        CmpOp::Lt => ordering == Ordering::Less,
        CmpOp::Le => ordering != Ordering::Greater,
        CmpOp::Gt => ordering == Ordering::Greater,
        CmpOp::Ge => ordering != Ordering::Less,
    };
    Ok(Value::Bool(matched))
}

pub(crate) fn compare_property_values(left: &Value, right: &Value) -> Option<Ordering> {
    compare_values(left, right)
}

pub(crate) fn eval_concat_expr(left: Value, right: Value) -> Result<Value, ExprEvaluationError> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Text(left), Value::Text(right)) => Ok(Value::Text(format!("{left}{right}"))),
        (Value::Bytes(mut left), Value::Bytes(right)) => {
            left.extend_from_slice(&right);
            Ok(Value::Bytes(left))
        }
        _ => invalid_expr_value(),
    }
}

pub(crate) fn eval_unary_expr(op: UnaryOp, value: Value) -> Result<Value, ExprEvaluationError> {
    eval_unary_numeric(op, value).map_err(map_numeric_op_err)
}

pub(crate) fn eval_abs_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    eval_abs_numeric(value).map_err(map_numeric_op_err)
}

pub(crate) fn try_finite_f32_from_f64(out: f64) -> Result<f32, ExprEvaluationError> {
    if !out.is_finite() {
        return Err(ExprEvaluationError::NonFiniteNumeric);
    }
    let narrowed = out as f32;
    if !narrowed.is_finite() {
        return Err(ExprEvaluationError::NonFiniteNumeric);
    }
    Ok(narrowed)
}

fn finite_f32_from_f64(out: f64) -> Result<Value, ExprEvaluationError> {
    try_finite_f32_from_f64(out).map(Value::Float32)
}

fn try_finite_f16_from_f64(out: f64) -> Result<half::f16, ExprEvaluationError> {
    if !out.is_finite() {
        return Err(ExprEvaluationError::NonFiniteNumeric);
    }
    let narrowed = half::f16::from_f64(out);
    if !narrowed.is_finite() {
        return Err(ExprEvaluationError::NonFiniteNumeric);
    }
    Ok(narrowed)
}

fn finite_f16_from_f64(out: f64) -> Result<Value, ExprEvaluationError> {
    try_finite_f16_from_f64(out).map(Value::Float16)
}

fn numeric_unary_f16(
    value: Value,
    f: impl FnOnce(f64) -> f64,
) -> Result<Value, ExprEvaluationError> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(v) = value.as_f64() else {
        return invalid_expr_value();
    };
    finite_f16_from_f64(f(v))
}

fn numeric_unary_f32(
    value: Value,
    f: impl FnOnce(f64) -> f64,
) -> Result<Value, ExprEvaluationError> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    let Some(v) = value.as_f64() else {
        return invalid_expr_value();
    };
    finite_f32_from_f64(f(v))
}

pub(crate) fn eval_floor_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::floor)
}

pub(crate) fn eval_ceil_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::ceil)
}

pub(crate) fn eval_sqrt_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::sqrt)
}

pub(crate) fn eval_exp_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::exp)
}

pub(crate) fn eval_ln_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::ln)
}

pub(crate) fn eval_log10_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::log10)
}

pub(crate) fn eval_sin_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::sin)
}

pub(crate) fn eval_cos_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::cos)
}

pub(crate) fn eval_tan_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::tan)
}

pub(crate) fn eval_asin_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::asin)
}

pub(crate) fn eval_acos_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::acos)
}

pub(crate) fn eval_atan_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::atan)
}

pub(crate) fn eval_degrees_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, |v| v.to_degrees())
}

pub(crate) fn eval_radians_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, |v| v.to_radians())
}

pub(crate) fn eval_cot_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, |v| 1.0 / v.tan())
}

pub(crate) fn eval_sinh_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::sinh)
}

pub(crate) fn eval_cosh_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::cosh)
}

pub(crate) fn eval_tanh_expr(value: Value) -> Result<Value, ExprEvaluationError> {
    numeric_unary_f32(value, f64::tanh)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SearchedCaseWhenOutcome {
    Match,
    Continue,
}

pub(crate) fn searched_case_when_outcome(
    condition: Value,
) -> Result<SearchedCaseWhenOutcome, ExprEvaluationError> {
    match truthy(condition)? {
        Some(true) => Ok(SearchedCaseWhenOutcome::Match),
        None | Some(false) => Ok(SearchedCaseWhenOutcome::Continue),
    }
}

fn decimal_precision_to_width(precision: u64) -> u16 {
    match precision {
        0..=2 => 8,
        3..=4 => 16,
        5..=9 => 32,
        10..=18 => 64,
        19..=38 => 128,
        _ => 256,
    }
}

fn fits_signed_decimal_digits(v: i128, precision: u64) -> Result<(), ExprEvaluationError> {
    if precision == 0 {
        return if v == 0 {
            Ok(())
        } else {
            Err(ExprEvaluationError::NumericPrecisionOverflow)
        };
    }
    let digits = if v == 0 {
        1
    } else {
        v.unsigned_abs().ilog10() + 1
    };
    if u64::from(digits) > precision {
        return Err(ExprEvaluationError::NumericPrecisionOverflow);
    }
    Ok(())
}

fn fits_unsigned_decimal_digits(v: u128, precision: u64) -> Result<(), ExprEvaluationError> {
    if precision == 0 {
        return if v == 0 {
            Ok(())
        } else {
            Err(ExprEvaluationError::NumericPrecisionOverflow)
        };
    }
    let digits = if v == 0 { 1 } else { v.ilog10() + 1 };
    if u64::from(digits) > precision {
        return Err(ExprEvaluationError::NumericPrecisionOverflow);
    }
    Ok(())
}

fn fits_i256_decimal_digits(
    value: &gleaph_gql::types::Int256,
    precision: u64,
) -> Result<(), ExprEvaluationError> {
    if precision == 0 {
        return if value.is_zero() {
            Ok(())
        } else {
            Err(ExprEvaluationError::NumericPrecisionOverflow)
        };
    }
    let digits = value.unsigned_decimal_digit_count();
    if digits > precision {
        return Err(ExprEvaluationError::NumericPrecisionOverflow);
    }
    Ok(())
}

fn fits_u256_decimal_digits(
    value: &gleaph_gql::types::Uint256,
    precision: u64,
) -> Result<(), ExprEvaluationError> {
    if precision == 0 {
        return if value.is_zero() {
            Ok(())
        } else {
            Err(ExprEvaluationError::NumericPrecisionOverflow)
        };
    }
    let digits = value.unsigned_decimal_digit_count();
    if digits > precision {
        return Err(ExprEvaluationError::NumericPrecisionOverflow);
    }
    Ok(())
}

fn cast_signed_with_decimal_precision(
    value: Value,
    precision: u64,
) -> Result<Value, ExprEvaluationError> {
    let width = decimal_precision_to_width(precision);
    if let Value::Int256(v) = &value {
        fits_i256_decimal_digits(v, precision)?;
        if width == 256 {
            return Ok(value);
        }
        let narrowed = i128::try_from(v.0).map_err(|_| ExprEvaluationError::NumericOverflow)?;
        return narrow_signed(narrowed, width).ok_or(ExprEvaluationError::NumericOverflow);
    }
    let v = numeric_to_i128(&value)?;
    fits_signed_decimal_digits(v, precision)?;
    narrow_signed(v, width).ok_or(ExprEvaluationError::NumericOverflow)
}

fn cast_unsigned_with_decimal_precision(
    value: Value,
    precision: u64,
) -> Result<Value, ExprEvaluationError> {
    let width = decimal_precision_to_width(precision);
    if let Value::Uint256(v) = &value {
        fits_u256_decimal_digits(v, precision)?;
        if width == 256 {
            return Ok(value);
        }
        let narrowed = u128::try_from(v.0).map_err(|_| ExprEvaluationError::NumericOverflow)?;
        return narrow_unsigned(narrowed, width).ok_or(ExprEvaluationError::NumericOverflow);
    }
    let v = numeric_to_u128(&value)?;
    fits_unsigned_decimal_digits(v, precision)?;
    narrow_unsigned(v, width).ok_or(ExprEvaluationError::NumericOverflow)
}

fn cast_to_float64(value: Value) -> Result<Value, ExprEvaluationError> {
    let Some(v) = value.as_f64() else {
        return Err(ExprEvaluationError::UnsupportedNumericConversion);
    };
    if !v.is_finite() {
        return Err(ExprEvaluationError::NonFiniteNumeric);
    }
    Ok(Value::Float64(v))
}

fn cast_to_float128(value: Value) -> Result<Value, ExprEvaluationError> {
    if value.is_float() {
        if let Value::Float128(v) = &value {
            if !gleaph_gql::value::f128_is_finite(*v) {
                return Err(ExprEvaluationError::NonFiniteNumeric);
            }
        } else {
            let Some(v) = value.as_f64() else {
                return Err(ExprEvaluationError::UnsupportedNumericConversion);
            };
            if !v.is_finite() {
                return Err(ExprEvaluationError::NonFiniteNumeric);
            }
        }
    }
    let narrowed = value
        .as_f128()
        .ok_or(ExprEvaluationError::UnsupportedNumericConversion)?;
    if !gleaph_gql::value::f128_is_finite(narrowed) {
        return Err(ExprEvaluationError::NonFiniteNumeric);
    }
    Ok(Value::Float128(narrowed))
}

fn cast_to_float256(value: Value) -> Result<Value, ExprEvaluationError> {
    if value.is_float() {
        if let Value::Float256(v) = &value {
            if !v.is_finite() {
                return Err(ExprEvaluationError::NonFiniteNumeric);
            }
        } else {
            let Some(v) = value.as_f64() else {
                return Err(ExprEvaluationError::UnsupportedNumericConversion);
            };
            if !v.is_finite() {
                return Err(ExprEvaluationError::NonFiniteNumeric);
            }
        }
    }
    let narrowed = value
        .as_f256()
        .ok_or(ExprEvaluationError::UnsupportedNumericConversion)?;
    if !narrowed.is_finite() {
        return Err(ExprEvaluationError::NonFiniteNumeric);
    }
    Ok(Value::Float256(narrowed))
}

fn cast_to_float_precision(
    value: Value,
    precision: u64,
    scale: Option<u64>,
) -> Result<Value, ExprEvaluationError> {
    let Some(v) = value.as_f64() else {
        return Err(ExprEvaluationError::UnsupportedNumericConversion);
    };
    if !v.is_finite() {
        return Err(ExprEvaluationError::NonFiniteNumeric);
    }
    if let Some(scale) = scale {
        let decimal =
            gleaph_gql::types::Decimal::from_f64(v).ok_or(ExprEvaluationError::NumericOverflow)?;
        let rounded = decimal.round_to_scale(scale as u32);
        if !rounded.fits_sql_precision_scale(precision, scale) {
            return Err(ExprEvaluationError::NumericPrecisionOverflow);
        }
        let rounded_f64 = rounded
            .to_f64()
            .ok_or(ExprEvaluationError::NumericOverflow)?;
        return Ok(Value::Float64(rounded_f64));
    }
    if precision == 0 {
        return if v == 0.0 {
            Ok(Value::Float64(0.0))
        } else {
            Err(ExprEvaluationError::NumericOverflow)
        };
    }
    if precision <= 16 {
        try_finite_f16_from_f64(v)?;
    } else if precision <= 24 {
        try_finite_f32_from_f64(v)?;
    }
    Ok(Value::Float64(v))
}

fn decimal_to_i128(d: gleaph_gql::types::Decimal) -> Result<i128, ExprEvaluationError> {
    d.trunc_to_i128()
        .ok_or(ExprEvaluationError::NumericOverflow)
}

fn decimal_to_u128(d: gleaph_gql::types::Decimal) -> Result<u128, ExprEvaluationError> {
    d.trunc_to_u128()
        .ok_or(ExprEvaluationError::NumericOverflow)
}

fn numeric_to_i128(value: &Value) -> Result<i128, ExprEvaluationError> {
    if let Some(v) = value.as_i128() {
        return Ok(v);
    }
    if let Some(v) = value.as_i256() {
        return i128::try_from(v).map_err(|_| ExprEvaluationError::NumericOverflow);
    }
    if let Value::Decimal(d) = value {
        return decimal_to_i128(*d);
    }
    if let Some(f) = value.as_f64() {
        if !f.is_finite() {
            return Err(ExprEvaluationError::NonFiniteNumeric);
        }
        if f < i128::MIN as f64 || f > i128::MAX as f64 {
            return Err(ExprEvaluationError::NumericOverflow);
        }
        return Ok(f as i128);
    }
    Err(ExprEvaluationError::UnsupportedNumericConversion)
}

fn numeric_to_u128(value: &Value) -> Result<u128, ExprEvaluationError> {
    if let Some(v) = value.as_u128() {
        return Ok(v);
    }
    if let Some(v) = value.as_u256() {
        return u128::try_from(v).map_err(|_| ExprEvaluationError::NumericOverflow);
    }
    if let Value::Decimal(d) = value {
        return decimal_to_u128(*d);
    }
    if let Some(f) = value.as_f64() {
        if !f.is_finite() {
            return Err(ExprEvaluationError::NonFiniteNumeric);
        }
        if f < 0.0 || f > u128::MAX as f64 {
            return Err(ExprEvaluationError::NumericOverflow);
        }
        return Ok(f as u128);
    }
    Err(ExprEvaluationError::UnsupportedNumericConversion)
}

fn cast_signed(value: Value, width: u16) -> Result<Value, ExprEvaluationError> {
    if width == 256
        && let Some(v) = value.as_i256()
    {
        return Ok(Value::Int256(gleaph_gql::types::Int256::new(v)));
    }
    let v = numeric_to_i128(&value)?;
    narrow_signed(v, width).ok_or(ExprEvaluationError::NumericOverflow)
}

fn cast_unsigned(value: Value, width: u16) -> Result<Value, ExprEvaluationError> {
    if width == 256
        && let Some(v) = value.as_u256()
    {
        return Ok(Value::Uint256(gleaph_gql::types::Uint256::new(v)));
    }
    let v = numeric_to_u128(&value)?;
    narrow_unsigned(v, width).ok_or(ExprEvaluationError::NumericOverflow)
}

fn cast_to_decimal(value: Value) -> Result<Value, ExprEvaluationError> {
    if let Value::Decimal(decimal) = value {
        return Ok(Value::Decimal(decimal));
    }
    if let Some(v) = value.as_i128() {
        return Ok(Value::Decimal(gleaph_gql::types::Decimal::from_i128(v)));
    }
    if let Some(v) = value.as_u128() {
        return Ok(Value::Decimal(gleaph_gql::types::Decimal::from_u128(v)));
    }
    if let Some(v) = value.as_i256() {
        return gleaph_gql::types::Decimal::parse(&v.to_string())
            .map(Value::Decimal)
            .ok_or(ExprEvaluationError::NumericOverflow);
    }
    if let Some(v) = value.as_u256() {
        return gleaph_gql::types::Decimal::parse(&v.to_string())
            .map(Value::Decimal)
            .ok_or(ExprEvaluationError::NumericOverflow);
    }
    if let Some(f) = value.as_f64() {
        if !f.is_finite() {
            return Err(ExprEvaluationError::NonFiniteNumeric);
        }
        return gleaph_gql::types::Decimal::from_f64(f)
            .map(Value::Decimal)
            .ok_or(ExprEvaluationError::NumericOverflow);
    }
    Err(ExprEvaluationError::UnsupportedNumericConversion)
}

fn cast_decimal(
    value: Value,
    precision: Option<u64>,
    scale: Option<u64>,
) -> Result<Value, ExprEvaluationError> {
    let result = cast_to_decimal(value)?;
    let Some(precision) = precision else {
        return Ok(result);
    };
    let scale = scale.unwrap_or(0);
    let Value::Decimal(decimal) = result else {
        return Err(ExprEvaluationError::UnsupportedNumericConversion);
    };
    let rounded = decimal.round_to_scale(scale as u32);
    if !rounded.fits_sql_precision_scale(precision, scale) {
        return Err(ExprEvaluationError::NumericPrecisionOverflow);
    }
    Ok(Value::Decimal(rounded))
}

pub(crate) fn eval_cast_expr(
    value: Value,
    target: &ValueType,
) -> Result<Value, ExprEvaluationError> {
    if value == Value::Null {
        return Ok(Value::Null);
    }
    if !value.is_numeric() {
        return invalid_expr_value();
    }
    match target {
        ValueType::Float32 { .. } => numeric_unary_f32(value, |v| v),
        ValueType::Float64 { .. } => cast_to_float64(value),
        ValueType::Float16 { .. } => numeric_unary_f16(value, |v| v),
        ValueType::Decimal {
            precision, scale, ..
        } => cast_decimal(value, *precision, *scale),
        ValueType::FloatPrecision { precision, scale } => {
            cast_to_float_precision(value, *precision, *scale)
        }
        ValueType::Int8 { .. } => cast_signed(value, 8),
        ValueType::Int16 { .. } => cast_signed(value, 16),
        ValueType::Int32 { .. } => cast_signed(value, 32),
        ValueType::Int64 { .. } => cast_signed(value, 64),
        ValueType::Int128 { .. } => cast_signed(value, 128),
        ValueType::Int256 { .. } => cast_signed(value, 256),
        ValueType::Uint8 { .. } => cast_unsigned(value, 8),
        ValueType::Uint16 { .. } => cast_unsigned(value, 16),
        ValueType::Uint32 { .. } => cast_unsigned(value, 32),
        ValueType::Uint64 { .. } => cast_unsigned(value, 64),
        ValueType::Uint128 { .. } => cast_unsigned(value, 128),
        ValueType::Uint256 { .. } => cast_unsigned(value, 256),
        ValueType::IntPrecision { precision, .. } => {
            cast_signed_with_decimal_precision(value, *precision)
        }
        ValueType::UintPrecision { precision, .. } => {
            cast_unsigned_with_decimal_precision(value, *precision)
        }
        ValueType::Float128 => cast_to_float128(value),
        ValueType::Float256 => cast_to_float256(value),
        _ => Err(ExprEvaluationError::UnsupportedNumericConversion),
    }
}

pub(crate) fn eval_case_simple_expr(
    operand: Value,
    when_clauses: &[(Value, Value)],
    else_clause: Option<Value>,
) -> Value {
    for (condition, result) in when_clauses {
        if operand == Value::Null || *condition == Value::Null {
            continue;
        }
        if eval_compare_expr(operand.clone(), CmpOp::Eq, condition.clone()).ok()
            == Some(Value::Bool(true))
        {
            return result.clone();
        }
    }
    else_clause.unwrap_or(Value::Null)
}

pub(crate) fn eval_case_searched_expr(
    when_clauses: &[(Value, Value)],
    else_clause: Option<Value>,
) -> Result<Value, ExprEvaluationError> {
    for (condition, result) in when_clauses {
        if searched_case_when_outcome(condition.clone())? == SearchedCaseWhenOutcome::Match {
            return Ok(result.clone());
        }
    }
    Ok(else_clause.unwrap_or(Value::Null))
}

pub(crate) fn eval_log_expr(left: Value, right: Value) -> Result<Value, ExprEvaluationError> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let (Some(base), Some(x)) = (left.as_f64(), right.as_f64()) else {
        return invalid_expr_value();
    };
    if base <= 0.0 || x <= 0.0 || !base.is_finite() || !x.is_finite() {
        return Err(ExprEvaluationError::InvalidValue);
    }
    let out = x.log(base);
    finite_f32_from_f64(out)
}

pub(crate) fn eval_mod_expr(left: Value, right: Value) -> Result<Value, ExprEvaluationError> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let (Some(l), Some(r)) = (left.as_f64(), right.as_f64()) else {
        return invalid_expr_value();
    };
    if r == 0.0 {
        return Err(ExprEvaluationError::DivisionByZero);
    }
    let out = l % r;
    finite_f32_from_f64(out)
}

pub(crate) fn eval_power_expr(left: Value, right: Value) -> Result<Value, ExprEvaluationError> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }
    let (Some(l), Some(r)) = (left.as_f64(), right.as_f64()) else {
        return invalid_expr_value();
    };
    let out = l.powf(r);
    finite_f32_from_f64(out)
}

pub(crate) fn eval_binary_expr(
    left: Value,
    op: BinaryOp,
    right: Value,
) -> Result<Value, ExprEvaluationError> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }

    if let BinaryOp::Add = op
        && let (Value::Text(left), Value::Text(right)) = (&left, &right)
    {
        return Ok(Value::Text(format!("{left}{right}")));
    }

    eval_binary_numeric(left, op, right).map_err(map_numeric_op_err)
}

fn map_numeric_op_err(err: NumericOpError) -> ExprEvaluationError {
    match err {
        NumericOpError::DivisionByZero => ExprEvaluationError::DivisionByZero,
        NumericOpError::Overflow => ExprEvaluationError::NumericOverflow,
        NumericOpError::NonFinite => ExprEvaluationError::NonFiniteNumeric,
        NumericOpError::UnsupportedConversion => ExprEvaluationError::UnsupportedNumericConversion,
        NumericOpError::InvalidOperand => ExprEvaluationError::InvalidValue,
    }
}

fn invalid_expr_value() -> Result<Value, ExprEvaluationError> {
    Err(ExprEvaluationError::InvalidValue)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::types::{Decimal, Int256, Uint256};

    #[test]
    fn preserves_numeric_precision_when_evaluating_arithmetic() {
        assert_eq!(
            eval_binary_expr(Value::Int8(120), BinaryOp::Add, Value::Int8(10))
                .expect("signed widening"),
            Value::Int16(130)
        );
        assert_eq!(
            eval_binary_expr(Value::Uint8(250), BinaryOp::Add, Value::Uint8(10))
                .expect("unsigned widening"),
            Value::Uint16(260)
        );
        assert_eq!(
            eval_binary_expr(Value::Int64(1), BinaryOp::Div, Value::Int64(4))
                .expect("integer division decimal"),
            Value::Decimal(Decimal::parse("0.25").expect("decimal"))
        );
        assert_eq!(
            eval_binary_expr(Value::Uint8(2), BinaryOp::Sub, Value::Uint8(5))
                .expect("unsigned subtraction below zero"),
            Value::Int128(-3)
        );

        let large = Value::Int256(Int256::new(ethnum::I256::from(i128::MAX)));
        assert_eq!(
            eval_binary_expr(large, BinaryOp::Add, Value::Int64(1)).expect("i256 add"),
            Value::Int256(Int256::new(ethnum::I256::from(i128::MAX) + 1))
        );
        assert_eq!(
            eval_binary_expr(Value::Int128(i128::MAX), BinaryOp::Add, Value::Int64(1))
                .expect("i128 overflow widens"),
            Value::Int256(Int256::new(ethnum::I256::from(i128::MAX) + 1))
        );
        assert_eq!(
            eval_binary_expr(Value::Uint128(u128::MAX), BinaryOp::Add, Value::Uint8(1))
                .expect("u128 overflow widens"),
            Value::Uint256(Uint256::new(ethnum::U256::from(u128::MAX) + 1))
        );
        assert_eq!(
            eval_binary_expr(
                Value::Uint128(u128::MAX),
                BinaryOp::Sub,
                Value::Uint256(Uint256::new(ethnum::U256::from(u128::MAX) + 1)),
            )
            .expect("large unsigned subtraction below zero"),
            Value::Int256(Int256::new(ethnum::I256::from(-1)))
        );
        assert_eq!(
            eval_binary_expr(
                Value::Int256(Int256::new(ethnum::I256::from(1))),
                BinaryOp::Div,
                Value::Int256(Int256::new(ethnum::I256::from(4))),
            )
            .expect("i256 fractional division"),
            Value::Float256("0.25".parse::<f256::f256>().expect("f256"))
        );
        assert_eq!(
            eval_binary_expr(
                Value::Uint256(Uint256::new(ethnum::U256::from(1u8))),
                BinaryOp::Div,
                Value::Uint256(Uint256::new(ethnum::U256::from(4u8))),
            )
            .expect("u256 fractional division"),
            Value::Float256("0.25".parse::<f256::f256>().expect("f256"))
        );
    }

    #[test]
    fn preserves_float_width_when_evaluating_arithmetic() {
        assert_eq!(
            eval_binary_expr(
                Value::Float16(half::f16::from_f32(1.5)),
                BinaryOp::Add,
                Value::Float16(half::f16::from_f32(2.0)),
            )
            .expect("f16 add"),
            Value::Float16(half::f16::from_f32(3.5))
        );
        assert_eq!(
            eval_binary_expr(Value::Float32(1.5), BinaryOp::Add, Value::Int64(2))
                .expect("f32 plus int"),
            Value::Float32(3.5)
        );
        assert_eq!(
            eval_binary_expr(
                Value::Float128(1.5f128),
                BinaryOp::Add,
                Value::Float128(2.25f128),
            )
            .expect("f128 add"),
            Value::Float128(3.75f128)
        );
        assert_eq!(
            eval_binary_expr(
                Value::Float256("1.5".parse::<f256::f256>().expect("f256")),
                BinaryOp::Add,
                Value::Int64(2),
            )
            .expect("f256 plus int"),
            Value::Float256("3.5".parse::<f256::f256>().expect("f256"))
        );
        assert_eq!(
            eval_binary_expr(
                Value::Decimal(Decimal::parse("1.25").expect("decimal")),
                BinaryOp::Add,
                Value::Float32(2.25),
            )
            .expect("decimal plus f32"),
            Value::Float64(3.5)
        );
        assert_eq!(
            eval_binary_expr(
                Value::Decimal(Decimal::parse("1.25").expect("decimal")),
                BinaryOp::Add,
                Value::Float256("2.25".parse::<f256::f256>().expect("f256")),
            )
            .expect("decimal plus f256"),
            Value::Float256("3.5".parse::<f256::f256>().expect("f256"))
        );
    }

    #[test]
    fn division_by_zero_maps_to_distinct_error() {
        assert_eq!(
            eval_binary_expr(Value::Int64(1), BinaryOp::Div, Value::Int64(0)),
            Err(ExprEvaluationError::DivisionByZero)
        );
    }

    #[test]
    fn incomparable_comparison_maps_to_distinct_error() {
        assert_eq!(
            eval_compare_expr(Value::Text("a".into()), CmpOp::Lt, Value::Int64(1)),
            Err(ExprEvaluationError::IncomparableValues)
        );
    }

    #[test]
    fn non_finite_float_division_maps_to_distinct_error() {
        assert_eq!(
            eval_binary_expr(Value::Float32(1.0), BinaryOp::Div, Value::Float32(0.0)),
            Err(ExprEvaluationError::NonFiniteNumeric)
        );
    }

    #[test]
    fn preserves_numeric_precision_when_comparing_values() {
        assert_eq!(
            compare_property_values(
                &Value::Float128(1.0f128 + f128::EPSILON),
                &Value::Float128(1.0f128),
            ),
            Some(Ordering::Greater)
        );
        assert_eq!(
            compare_property_values(
                &Value::Float256("1.0000000000000000000000000000000000001".parse().unwrap()),
                &Value::Float256("1.0000000000000000000000000000000000000".parse().unwrap()),
            ),
            Some(Ordering::Greater)
        );
    }

    #[test]
    fn abs_null_is_null() {
        assert_eq!(eval_abs_expr(Value::Null).expect("abs null"), Value::Null);
    }

    #[test]
    fn abs_widens_int32_min() {
        assert_eq!(
            eval_abs_expr(Value::Int32(i32::MIN)).expect("abs i32 min"),
            Value::Int64(2_147_483_648)
        );
    }

    #[test]
    fn abs_widens_int64_min() {
        assert_eq!(
            eval_abs_expr(Value::Int64(i64::MIN)).expect("abs i64 min"),
            Value::Int128(i128::from(i64::MAX) + 1)
        );
    }

    #[test]
    fn eval_case_searched_unknown_condition_falls_through() {
        let result = eval_case_searched_expr(
            &[
                (Value::Null, Value::Int32(1)),
                (Value::Bool(true), Value::Int32(2)),
            ],
            Some(Value::Int32(3)),
        )
        .expect("case searched");
        assert_eq!(result, Value::Int32(2));
    }

    #[test]
    fn eval_case_searched_all_unknown_uses_else() {
        let result =
            eval_case_searched_expr(&[(Value::Null, Value::Int32(1))], Some(Value::Int32(3)))
                .expect("case searched");
        assert_eq!(result, Value::Int32(3));
    }

    #[test]
    fn cast_large_float32_errors() {
        let err = eval_cast_expr(
            Value::Float64(1e40),
            &ValueType::Float32 {
                keyword: gleaph_gql::ast::Keyword::new("FLOAT32"),
            },
        )
        .expect_err("large float32 cast");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn floor_large_float32_errors() {
        let err = eval_floor_expr(Value::Float64(1e40)).expect_err("large floor");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn cast_int8_returns_int8() {
        let result = eval_cast_expr(
            Value::Float64(42.0),
            &ValueType::Int8 {
                keyword: gleaph_gql::ast::Keyword::new("INT8"),
            },
        )
        .expect("int8 cast");
        assert_eq!(result, Value::Int8(42));
    }

    #[test]
    fn cast_int8_overflow_errors() {
        let err = eval_cast_expr(
            Value::Float64(1e40),
            &ValueType::Int8 {
                keyword: gleaph_gql::ast::Keyword::new("INT8"),
            },
        )
        .expect_err("int8 overflow cast");
        assert_eq!(err, ExprEvaluationError::NumericOverflow);
    }

    #[test]
    fn cast_float64_non_finite_errors() {
        let err = eval_cast_expr(
            Value::Float64(f64::NAN),
            &ValueType::Float64 {
                keyword: gleaph_gql::ast::Keyword::new("FLOAT64"),
            },
        )
        .expect_err("nan float64 cast");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn cast_float16_returns_float16() {
        let result = eval_cast_expr(
            Value::Float64(1.5),
            &ValueType::Float16 {
                keyword: gleaph_gql::ast::Keyword::new("FLOAT16"),
            },
        )
        .expect("float16 cast");
        assert_eq!(result, Value::Float16(half::f16::from_f64(1.5)));
    }

    #[test]
    fn cast_float16_large_errors() {
        let err = eval_cast_expr(
            Value::Float64(1e40),
            &ValueType::Float16 {
                keyword: gleaph_gql::ast::Keyword::new("FLOAT16"),
            },
        )
        .expect_err("large float16 cast");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn case_searched_all_unknown_no_else_is_null() {
        let result = eval_case_searched_expr(&[(Value::Null, Value::Int32(1))], None)
            .expect("case searched");
        assert_eq!(result, Value::Null);
    }

    #[test]
    fn power_large_float32_errors() {
        let err =
            eval_power_expr(Value::Float64(1e20), Value::Float64(2.0)).expect_err("large power");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn eval_mod_large_float32_errors() {
        let err = eval_mod_expr(Value::Float64(1e40), Value::Float64(1e41)).expect_err("large mod");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn floor_negative_large_float32_errors() {
        let err = eval_floor_expr(Value::Float64(-1e40)).expect_err("negative large floor");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn cast_int_precision_returns_int32() {
        let result = eval_cast_expr(
            Value::Int32(42),
            &ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 9,
            },
        )
        .expect("int precision cast");
        assert_eq!(result, Value::Int32(42));
    }

    #[test]
    fn cast_int_precision_10_returns_int64() {
        let result = eval_cast_expr(
            Value::Int32(42),
            &ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 10,
            },
        )
        .expect("int(10) precision cast");
        assert_eq!(result, Value::Int64(42));
    }

    #[test]
    fn cast_int_precision_digit_limit_errors() {
        let err = eval_cast_expr(
            Value::Int32(999),
            &ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 2,
            },
        )
        .expect_err("too many digits for int(2)");
        assert_eq!(err, ExprEvaluationError::NumericPrecisionOverflow);
    }

    #[test]
    fn cast_int_precision_digit_overflow_differs_from_width_overflow() {
        let digit_err = eval_cast_expr(
            Value::Int32(999),
            &ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 2,
            },
        )
        .expect_err("too many digits for int(2)");
        assert_eq!(digit_err, ExprEvaluationError::NumericPrecisionOverflow);

        let width_err = eval_cast_expr(
            Value::Int32(100_000),
            &ValueType::Int16 {
                keyword: gleaph_gql::ast::Keyword::new("INT16"),
            },
        )
        .expect_err("int16 width overflow");
        assert_eq!(width_err, ExprEvaluationError::NumericOverflow);
    }

    #[test]
    fn cast_uint_precision_digit_overflow_differs_from_width_overflow() {
        let digit_err = eval_cast_expr(
            Value::Uint32(999),
            &ValueType::UintPrecision {
                keyword: gleaph_gql::ast::Keyword::new("UINT"),
                precision: 2,
            },
        )
        .expect_err("too many digits for uint(2)");
        assert_eq!(digit_err, ExprEvaluationError::NumericPrecisionOverflow);

        let width_err = eval_cast_expr(
            Value::Uint32(100_000),
            &ValueType::Uint16 {
                keyword: gleaph_gql::ast::Keyword::new("UINT16"),
            },
        )
        .expect_err("uint16 width overflow");
        assert_eq!(width_err, ExprEvaluationError::NumericOverflow);
    }

    #[test]
    fn cast_to_decimal_from_f64_overflow_errors() {
        let err = eval_cast_expr(
            Value::Float64(1e29),
            &ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: None,
                scale: None,
            },
        )
        .expect_err("unrepresentable float to decimal");
        assert_eq!(err, ExprEvaluationError::NumericOverflow);
    }

    #[test]
    fn cast_uint_precision_returns_uint32() {
        let result = eval_cast_expr(
            Value::Uint32(42),
            &ValueType::UintPrecision {
                keyword: gleaph_gql::ast::Keyword::new("UINT"),
                precision: 9,
            },
        )
        .expect("uint precision cast");
        assert_eq!(result, Value::Uint32(42));
    }

    #[test]
    fn cast_uint_precision_digit_limit_errors() {
        let err = eval_cast_expr(
            Value::Uint32(999),
            &ValueType::UintPrecision {
                keyword: gleaph_gql::ast::Keyword::new("UINT"),
                precision: 2,
            },
        )
        .expect_err("too many digits for uint(2)");
        assert_eq!(err, ExprEvaluationError::NumericPrecisionOverflow);
    }

    #[test]
    fn cast_int256_precision_narrows_to_int64() {
        let int256 = gleaph_gql::types::Int256::parse("1000000000000000").expect("int256");
        let result = eval_cast_expr(
            Value::Int256(int256),
            &ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 18,
            },
        )
        .expect("int256 precision cast");
        assert_eq!(result, Value::Int64(1_000_000_000_000_000));
    }

    #[test]
    fn cast_int256_to_decimal() {
        let int256 = gleaph_gql::types::Int256::parse("42").expect("int256");
        let result = eval_cast_expr(
            Value::Int256(int256),
            &ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: None,
                scale: None,
            },
        )
        .expect("int256 to decimal");
        let Value::Decimal(decimal) = result else {
            panic!("expected decimal");
        };
        assert_eq!(decimal.to_string(), "42");
    }

    #[test]
    fn cast_float_precision_with_scale_rounds_fraction() {
        let decimal = gleaph_gql::types::Decimal::parse("12.345").expect("decimal");
        let rounded = decimal.round_to_scale(2).to_f64().expect("rounded f64");
        let result = eval_cast_expr(
            Value::Float64(rounded),
            &ValueType::FloatPrecision {
                precision: 4,
                scale: Some(2),
            },
        )
        .expect("rounds into float(4,2)");
        assert_eq!(result, Value::Float64(12.34));
    }

    #[test]
    fn cast_float_precision_with_scale_rejects_after_round() {
        let err = eval_cast_expr(
            Value::Float64(999.945),
            &ValueType::FloatPrecision {
                precision: 4,
                scale: Some(2),
            },
        )
        .expect_err("integer part too wide after round");
        assert_eq!(err, ExprEvaluationError::NumericPrecisionOverflow);
    }

    #[test]
    fn cast_float_precision_with_scale_rejects_large_integer_part() {
        let err = eval_cast_expr(
            Value::Float64(999.9),
            &ValueType::FloatPrecision {
                precision: 4,
                scale: Some(2),
            },
        )
        .expect_err("integer part too wide");
        assert_eq!(err, ExprEvaluationError::NumericPrecisionOverflow);
    }

    #[test]
    fn cast_float_precision_with_scale_accepts_in_range() {
        let result = eval_cast_expr(
            Value::Float64(99.99),
            &ValueType::FloatPrecision {
                precision: 4,
                scale: Some(2),
            },
        )
        .expect("float(4,2) in range");
        assert_eq!(result, Value::Float64(99.99));
    }

    #[test]
    fn cast_decimal_with_precision_scale_rejects_overflow() {
        let err = eval_cast_expr(
            Value::Int32(12_345),
            &ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: Some(4),
                scale: Some(2),
            },
        )
        .expect_err("decimal(4,2) integer overflow");
        assert_eq!(err, ExprEvaluationError::NumericPrecisionOverflow);
    }

    #[test]
    fn cast_decimal_with_precision_only_defaults_scale_zero() {
        let err = eval_cast_expr(
            Value::Int32(12_345),
            &ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: Some(4),
                scale: None,
            },
        )
        .expect_err("decimal(4) overflow");
        assert_eq!(err, ExprEvaluationError::NumericPrecisionOverflow);
    }

    #[test]
    fn cast_decimal_with_precision_scale_rounds_fraction() {
        let decimal = gleaph_gql::types::Decimal::parse("12.345").expect("decimal");
        let rounded = decimal.round_to_scale(2).to_f64().expect("rounded f64");
        let result = eval_cast_expr(
            Value::Float64(rounded),
            &ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: Some(4),
                scale: Some(2),
            },
        )
        .expect("decimal(4,2) rounds");
        let Value::Decimal(decimal) = result else {
            panic!("expected decimal");
        };
        assert_eq!(decimal.to_string(), "12.34");
    }

    #[test]
    fn cast_decimal_with_precision_scale_rejects_float_overflow() {
        let err = eval_cast_expr(
            Value::Float64(999.9),
            &ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: Some(4),
                scale: Some(2),
            },
        )
        .expect_err("decimal(4,2) overflow");
        assert_eq!(err, ExprEvaluationError::NumericPrecisionOverflow);
    }

    #[test]
    fn cast_decimal_with_precision_scale_accepts_in_range() {
        let result = eval_cast_expr(
            Value::Float64(99.99),
            &ValueType::Decimal {
                keyword: gleaph_gql::ast::Keyword::new("DECIMAL"),
                precision: Some(4),
                scale: Some(2),
            },
        )
        .expect("decimal(4,2) in range");
        let Value::Decimal(decimal) = result else {
            panic!("expected decimal");
        };
        assert_eq!(decimal.to_string(), "99.99");
    }

    #[test]
    fn cast_int256_precision_digit_limit_errors() {
        let int256 = gleaph_gql::types::Int256::parse("1000").expect("int256");
        let err = eval_cast_expr(
            Value::Int256(int256),
            &ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 2,
            },
        )
        .expect_err("too many digits for int(2)");
        assert_eq!(err, ExprEvaluationError::NumericPrecisionOverflow);
    }

    #[test]
    fn cast_negative_int256_precision_narrows_to_int64() {
        let int256 = gleaph_gql::types::Int256::parse("-1000000000000000").expect("int256");
        let result = eval_cast_expr(
            Value::Int256(int256),
            &ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 18,
            },
        )
        .expect("negative int256 precision cast");
        assert_eq!(result, Value::Int64(-1_000_000_000_000_000));
    }

    #[test]
    fn cast_float_precision_16_rejects_f16_overflow() {
        let err = eval_cast_expr(
            Value::Float64(70_000.0),
            &ValueType::FloatPrecision {
                precision: 16,
                scale: None,
            },
        )
        .expect_err("float(16) overflow");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn cast_uint256_precision_narrows_to_uint64() {
        let uint256 = gleaph_gql::types::Uint256::parse("1000000000000000").expect("uint256");
        let result = eval_cast_expr(
            Value::Uint256(uint256),
            &ValueType::UintPrecision {
                keyword: gleaph_gql::ast::Keyword::new("UINT"),
                precision: 18,
            },
        )
        .expect("uint256 precision cast");
        assert_eq!(result, Value::Uint64(1_000_000_000_000_000));
    }

    #[test]
    fn cast_float_precision_binary_24_rejects_large_value() {
        let err = eval_cast_expr(
            Value::Float64(1e40),
            &ValueType::FloatPrecision {
                precision: 24,
                scale: None,
            },
        )
        .expect_err("float32-range overflow");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn cast_float_precision_returns_float64() {
        let result = eval_cast_expr(
            Value::Float32(2.5),
            &ValueType::FloatPrecision {
                precision: 10,
                scale: None,
            },
        )
        .expect("float precision cast");
        assert_eq!(result, Value::Float64(2.5));
    }

    #[test]
    fn cast_decimal_to_int32() {
        let decimal = gleaph_gql::types::Decimal::parse("123.45").expect("decimal");
        let result = eval_cast_expr(
            Value::Decimal(decimal),
            &ValueType::Int32 {
                keyword: gleaph_gql::ast::Keyword::new("INT32"),
            },
        )
        .expect("decimal to int32");
        assert_eq!(result, Value::Int32(123));
    }

    #[test]
    fn cast_decimal_to_int32_overflow() {
        let decimal =
            gleaph_gql::types::Decimal::parse("100000000000000000000").expect("large decimal");
        let err = eval_cast_expr(
            Value::Decimal(decimal),
            &ValueType::Int32 {
                keyword: gleaph_gql::ast::Keyword::new("INT32"),
            },
        )
        .expect_err("decimal overflow");
        assert_eq!(err, ExprEvaluationError::NumericOverflow);
    }

    #[test]
    fn cast_int256_min_int_precision_does_not_panic() {
        let int256 = gleaph_gql::types::Int256::new(ethnum::I256::MIN);
        let err = eval_cast_expr(
            Value::Int256(int256),
            &ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 38,
            },
        )
        .expect_err("int256 min exceeds int(38) digit limit");
        assert_eq!(err, ExprEvaluationError::NumericPrecisionOverflow);
    }

    #[test]
    fn cast_int256_min_int_precision_76_errors() {
        let int256 = gleaph_gql::types::Int256::new(ethnum::I256::MIN);
        let err = eval_cast_expr(
            Value::Int256(int256),
            &ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 76,
            },
        )
        .expect_err("int256 min exceeds int(76) digit limit");
        assert_eq!(err, ExprEvaluationError::NumericPrecisionOverflow);
    }

    #[test]
    fn cast_int256_min_int_precision_77_ok() {
        let int256 = gleaph_gql::types::Int256::new(ethnum::I256::MIN);
        let result = eval_cast_expr(
            Value::Int256(int256),
            &ValueType::IntPrecision {
                keyword: gleaph_gql::ast::Keyword::new("INT"),
                precision: 77,
            },
        )
        .expect("int256 min fits int(77)");
        assert_eq!(result, Value::Int256(int256));
    }

    #[test]
    fn cast_to_float128_from_float32() {
        let result = eval_cast_expr(Value::Float32(2.5), &gleaph_gql::ast::ValueType::Float128)
            .expect("float128 cast");
        assert_eq!(result, Value::Float128(2.5f128));
    }

    #[test]
    fn cast_to_float256_from_float32() {
        let result = eval_cast_expr(Value::Float32(2.5), &gleaph_gql::ast::ValueType::Float256)
            .expect("float256 cast");
        let Value::Float256(decimal) = result else {
            panic!("expected float256");
        };
        assert_eq!(decimal, f256::f256::from(2.5f64));
    }

    #[test]
    fn cast_to_float128_non_finite_errors() {
        let err = eval_cast_expr(
            Value::Float64(f64::NAN),
            &gleaph_gql::ast::ValueType::Float128,
        )
        .expect_err("float128 non-finite");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn cast_to_float128_nan_errors() {
        let nan = f128::from_bits((0x7FFFu128 << 112) | 1);
        let err = eval_cast_expr(Value::Float128(nan), &gleaph_gql::ast::ValueType::Float128)
            .expect_err("float128 nan");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn cast_to_float256_non_finite_errors() {
        let err = eval_cast_expr(
            Value::Float64(f64::NAN),
            &gleaph_gql::ast::ValueType::Float256,
        )
        .expect_err("float256 non-finite");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }

    #[test]
    fn cast_float128_identity_preserves_value() {
        let fine = 1.0f128 + f128::from(2f64.powi(-55));
        let original = Value::Float128(fine);
        let result = eval_cast_expr(original.clone(), &gleaph_gql::ast::ValueType::Float128)
            .expect("float128 identity cast");
        assert_eq!(result, original);
    }

    #[test]
    fn cast_float128_large_finite_identity_preserves_value() {
        let large = f128::from(f64::MAX) * 2.0f128;
        let original = Value::Float128(large);
        let result = eval_cast_expr(original.clone(), &gleaph_gql::ast::ValueType::Float128)
            .expect("large finite float128 identity cast");
        assert_eq!(result, original);
    }

    #[test]
    fn cast_float256_identity_preserves_value() {
        let original = Value::Float256(
            "1.234567890123456789012345678901234567890123456789"
                .parse::<f256::f256>()
                .expect("high-precision f256"),
        );
        let result = eval_cast_expr(original.clone(), &gleaph_gql::ast::ValueType::Float256)
            .expect("float256 identity cast");
        assert_eq!(result, original);
    }

    #[test]
    fn cast_float256_large_finite_identity_preserves_value() {
        let large = "1e400".parse::<f256::f256>().expect("large finite f256");
        let original = Value::Float256(large);
        let result = eval_cast_expr(original.clone(), &gleaph_gql::ast::ValueType::Float256)
            .expect("large finite float256 identity cast");
        assert_eq!(result, original);
    }

    #[test]
    fn cast_float256_to_float128_unsupported() {
        let finite = f256::f256::from(2.5f64);
        let err = eval_cast_expr(
            Value::Float256(finite),
            &gleaph_gql::ast::ValueType::Float128,
        )
        .expect_err("float256 to float128");
        assert_eq!(err, ExprEvaluationError::UnsupportedNumericConversion);
    }

    #[test]
    fn eval_log_non_finite_result_errors() {
        let err =
            eval_log_expr(Value::Float64(1.0), Value::Float64(2.0)).expect_err("log non-finite");
        assert_eq!(err, ExprEvaluationError::NonFiniteNumeric);
    }
}
