//! Planner-independent expression evaluation primitives for plan execution.
//!
//! This module owns value-level runtime semantics shared by query and mutation
//! execution. It intentionally knows nothing about physical plans, graph
//! storage, bindings, or planner optimization metadata.

use gleaph_gql::Value;
use gleaph_gql::ast::{BinaryOp, CmpOp, UnaryOp};
use gleaph_gql::numeric_ops::{NumericOpError, eval_binary_numeric, eval_unary_numeric};
use gleaph_gql::value_cmp::compare_values;
use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ExprEvaluationError {
    InvalidValue,
    DivisionByZero,
    NumericOverflow,
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
}
