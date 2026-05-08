//! Evaluation of property expression trees for plan mutations (SET / INSERT properties).
//!
//! Kept separate from `plan_mutation_executor` so store operations and expression semantics
//! can evolve independently.

use crate::plan_mutation_error::PlanMutationError;
use gleaph_gql::Value;
use gleaph_gql::ast::{BinaryOp, CmpOp, Expr, ExprKind, TruthValue, UnaryOp};
use gleaph_gql::numeric_ops::{
    eval_binary_numeric, eval_unary_numeric, NumericOpError,
};
use gleaph_gql::value_cmp::compare_values;
use gleaph_gql_planner::plan::PropertyAssignment;
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// Trait abstraction for property expression evaluation (see [`PlanPropertyExprEvaluator`]).
pub trait PlanPropertyExprEvaluation {
    fn eval(&self, property: &str, expr: &Expr) -> Result<Value, PlanMutationError>;

    fn resolve_assignments<'b>(
        &self,
        properties: &'b [PropertyAssignment],
    ) -> Result<Vec<(&'b str, Value)>, PlanMutationError>;
}

/// Evaluates property expressions against planner parameters (mutation-time `$param` bindings).
#[derive(Clone, Copy, Debug)]
pub struct PlanPropertyExprEvaluator<'a> {
    parameters: &'a BTreeMap<String, Value>,
}

impl<'a> PlanPropertyExprEvaluator<'a> {
    pub fn new(parameters: &'a BTreeMap<String, Value>) -> Self {
        Self { parameters }
    }

    pub fn resolve_assignments<'b>(
        &self,
        properties: &'b [PropertyAssignment],
    ) -> Result<Vec<(&'b str, Value)>, PlanMutationError> {
        properties
            .iter()
            .map(|assignment| {
                let value = self.eval(assignment.name.as_ref(), &assignment.value)?;
                Ok((assignment.name.as_ref(), value))
            })
            .collect()
    }

    pub fn eval(&self, property: &str, expr: &Expr) -> Result<Value, PlanMutationError> {
        match &expr.kind {
            ExprKind::Literal(value) => Ok(value.clone()),
            ExprKind::Paren(inner) => self.eval(property, inner),
            ExprKind::Parameter(name) => self
                .parameters
                .get(name)
                .cloned()
                .ok_or_else(|| PlanMutationError::MissingParameter { name: name.clone() }),
            ExprKind::UnaryOp { op, expr } => {
                eval_unary_expr(property, *op, self.eval(property, expr)?)
            }
            ExprKind::BinaryOp { left, op, right } => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_binary_expr(property, left, *op, right)
            }
            ExprKind::Not(expr) => eval_not_expr(property, self.eval(property, expr)?),
            ExprKind::And(left, right) => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_and_expr(property, left, right)
            }
            ExprKind::Or(left, right) => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_or_expr(property, left, right)
            }
            ExprKind::Xor(left, right) => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_xor_expr(property, left, right)
            }
            ExprKind::Compare { left, op, right } => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_compare_expr(property, left, *op, right)
            }
            ExprKind::IsNull(expr) => Ok(Value::Bool(self.eval(property, expr)? == Value::Null)),
            ExprKind::IsNotNull(expr) => Ok(Value::Bool(self.eval(property, expr)? != Value::Null)),
            ExprKind::IsTruth {
                expr,
                value,
                negated,
            } => {
                let evaluated = self.eval(property, expr)?;
                let matched = matches!(
                    (evaluated, *value),
                    (Value::Bool(true), TruthValue::True)
                        | (Value::Bool(false), TruthValue::False)
                        | (Value::Null, TruthValue::Unknown),
                );
                Ok(Value::Bool(if *negated { !matched } else { matched }))
            }
            ExprKind::Concat(left, right) => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_concat_expr(property, left, right)
            }
            ExprKind::Coalesce(exprs) => {
                for expr in exprs {
                    let value = self.eval(property, expr)?;
                    if value != Value::Null {
                        return Ok(value);
                    }
                }
                Ok(Value::Null)
            }
            ExprKind::NullIf(left, right) => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                if left == Value::Null || right == Value::Null {
                    return Ok(left);
                }
                if compare_property_values(&left, &right) == Some(Ordering::Equal) {
                    Ok(Value::Null)
                } else {
                    Ok(left)
                }
            }
            ExprKind::ListLiteral(items) | ExprKind::ListConstructor { items, .. } => items
                .iter()
                .map(|expr| self.eval(property, expr))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::List),
            ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => fields
                .iter()
                .map(|(name, expr)| self.eval(property, expr).map(|value| (name.clone(), value)))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Record),
            _ => Err(PlanMutationError::UnsupportedExpression {
                property: property.to_owned(),
            }),
        }
    }
}

impl<'a> PlanPropertyExprEvaluation for PlanPropertyExprEvaluator<'a> {
    fn eval(&self, property: &str, expr: &Expr) -> Result<Value, PlanMutationError> {
        PlanPropertyExprEvaluator::eval(self, property, expr)
    }

    fn resolve_assignments<'b>(
        &self,
        properties: &'b [PropertyAssignment],
    ) -> Result<Vec<(&'b str, Value)>, PlanMutationError> {
        PlanPropertyExprEvaluator::resolve_assignments(self, properties)
    }
}

fn eval_not_expr(property: &str, value: Value) -> Result<Value, PlanMutationError> {
    match value {
        Value::Bool(value) => Ok(Value::Bool(!value)),
        Value::Null => Ok(Value::Null),
        _ => invalid_expr_value(property),
    }
}

fn eval_and_expr(property: &str, left: Value, right: Value) -> Result<Value, PlanMutationError> {
    match (truthy(property, left)?, truthy(property, right)?) {
        (Some(false), _) | (_, Some(false)) => Ok(Value::Bool(false)),
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(true), Some(true)) => Ok(Value::Bool(true)),
    }
}

fn eval_or_expr(property: &str, left: Value, right: Value) -> Result<Value, PlanMutationError> {
    match (truthy(property, left)?, truthy(property, right)?) {
        (Some(true), _) | (_, Some(true)) => Ok(Value::Bool(true)),
        (None, _) | (_, None) => Ok(Value::Null),
        (Some(false), Some(false)) => Ok(Value::Bool(false)),
    }
}

fn eval_xor_expr(property: &str, left: Value, right: Value) -> Result<Value, PlanMutationError> {
    match (truthy(property, left)?, truthy(property, right)?) {
        (Some(left), Some(right)) => Ok(Value::Bool(left ^ right)),
        _ => Ok(Value::Null),
    }
}

fn truthy(property: &str, value: Value) -> Result<Option<bool>, PlanMutationError> {
    match value {
        Value::Bool(value) => Ok(Some(value)),
        Value::Null => Ok(None),
        _ => invalid_expr_value(property).map(|_| None),
    }
}

fn eval_compare_expr(
    property: &str,
    left: Value,
    op: CmpOp,
    right: Value,
) -> Result<Value, PlanMutationError> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }

    let Some(ordering) = compare_property_values(&left, &right) else {
        return Err(PlanMutationError::ExpressionIncomparableValues {
            property: property.to_owned(),
        });
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

fn compare_property_values(left: &Value, right: &Value) -> Option<Ordering> {
    compare_values(left, right)
}

fn eval_concat_expr(property: &str, left: Value, right: Value) -> Result<Value, PlanMutationError> {
    match (left, right) {
        (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
        (Value::Text(left), Value::Text(right)) => Ok(Value::Text(format!("{left}{right}"))),
        (Value::Bytes(mut left), Value::Bytes(right)) => {
            left.extend_from_slice(&right);
            Ok(Value::Bytes(left))
        }
        _ => invalid_expr_value(property),
    }
}

fn eval_unary_expr(property: &str, op: UnaryOp, value: Value) -> Result<Value, PlanMutationError> {
    eval_unary_numeric(op, value).map_err(|err| map_numeric_op_err(property, err))
}

fn eval_binary_expr(
    property: &str,
    left: Value,
    op: BinaryOp,
    right: Value,
) -> Result<Value, PlanMutationError> {
    if left == Value::Null || right == Value::Null {
        return Ok(Value::Null);
    }

    if let BinaryOp::Add = op
        && let (Value::Text(left), Value::Text(right)) = (&left, &right)
    {
        return Ok(Value::Text(format!("{left}{right}")));
    }

    eval_binary_numeric(left, op, right).map_err(|err| map_numeric_op_err(property, err))
}

fn map_numeric_op_err(property: &str, err: NumericOpError) -> PlanMutationError {
    let property = property.to_owned();
    match err {
        NumericOpError::DivisionByZero => PlanMutationError::ExpressionDivisionByZero { property },
        NumericOpError::Overflow => PlanMutationError::ExpressionNumericOverflow { property },
        NumericOpError::NonFinite => PlanMutationError::ExpressionNonFiniteNumeric { property },
        NumericOpError::UnsupportedConversion => {
            PlanMutationError::ExpressionUnsupportedNumericConversion { property }
        }
        NumericOpError::InvalidOperand => PlanMutationError::InvalidExpressionValue { property },
    }
}

fn invalid_expr_value(property: &str) -> Result<Value, PlanMutationError> {
    Err(invalid_expr_value_err(property))
}

fn invalid_expr_value_err(property: &str) -> PlanMutationError {
    PlanMutationError::InvalidExpressionValue {
        property: property.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::types::{Decimal, Int256, Uint256};

    #[test]
    fn preserves_numeric_precision_when_evaluating_arithmetic() {
        assert_eq!(
            eval_binary_expr("p", Value::Int8(120), BinaryOp::Add, Value::Int8(10))
                .expect("signed widening"),
            Value::Int16(130)
        );
        assert_eq!(
            eval_binary_expr("p", Value::Uint8(250), BinaryOp::Add, Value::Uint8(10))
                .expect("unsigned widening"),
            Value::Uint16(260)
        );
        assert_eq!(
            eval_binary_expr("p", Value::Int64(1), BinaryOp::Div, Value::Int64(4))
                .expect("integer division decimal"),
            Value::Decimal(Decimal::parse("0.25").expect("decimal"))
        );
        assert_eq!(
            eval_binary_expr("p", Value::Uint8(2), BinaryOp::Sub, Value::Uint8(5))
                .expect("unsigned subtraction below zero"),
            Value::Int128(-3)
        );

        let large = Value::Int256(Int256::new(ethnum::I256::from(i128::MAX)));
        assert_eq!(
            eval_binary_expr("p", large, BinaryOp::Add, Value::Int64(1)).expect("i256 add"),
            Value::Int256(Int256::new(ethnum::I256::from(i128::MAX) + 1))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Int128(i128::MAX),
                BinaryOp::Add,
                Value::Int64(1),
            )
            .expect("i128 overflow widens"),
            Value::Int256(Int256::new(ethnum::I256::from(i128::MAX) + 1))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Uint128(u128::MAX),
                BinaryOp::Add,
                Value::Uint8(1),
            )
            .expect("u128 overflow widens"),
            Value::Uint256(Uint256::new(ethnum::U256::from(u128::MAX) + 1))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Uint128(u128::MAX),
                BinaryOp::Sub,
                Value::Uint256(Uint256::new(ethnum::U256::from(u128::MAX) + 1)),
            )
            .expect("large unsigned subtraction below zero"),
            Value::Int256(Int256::new(ethnum::I256::from(-1)))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Int256(Int256::new(ethnum::I256::from(1))),
                BinaryOp::Div,
                Value::Int256(Int256::new(ethnum::I256::from(4))),
            )
            .expect("i256 fractional division"),
            Value::Float256("0.25".parse::<f256::f256>().expect("f256"))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
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
                "p",
                Value::Float16(half::f16::from_f32(1.5)),
                BinaryOp::Add,
                Value::Float16(half::f16::from_f32(2.0)),
            )
            .expect("f16 add"),
            Value::Float16(half::f16::from_f32(3.5))
        );
        assert_eq!(
            eval_binary_expr("p", Value::Float32(1.5), BinaryOp::Add, Value::Int64(2))
                .expect("f32 plus int"),
            Value::Float32(3.5)
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Float128(1.5f128),
                BinaryOp::Add,
                Value::Float128(2.25f128),
            )
            .expect("f128 add"),
            Value::Float128(3.75f128)
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Float256("1.5".parse::<f256::f256>().expect("f256")),
                BinaryOp::Add,
                Value::Int64(2),
            )
            .expect("f256 plus int"),
            Value::Float256("3.5".parse::<f256::f256>().expect("f256"))
        );
        assert_eq!(
            eval_binary_expr(
                "p",
                Value::Decimal(Decimal::parse("1.25").expect("decimal")),
                BinaryOp::Add,
                Value::Float32(2.25),
            )
            .expect("decimal plus f32"),
            Value::Float64(3.5)
        );
        assert_eq!(
            eval_binary_expr(
                "p",
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
        assert!(matches!(
            eval_binary_expr("x", Value::Int64(1), BinaryOp::Div, Value::Int64(0)),
            Err(PlanMutationError::ExpressionDivisionByZero { property }) if property == "x"
        ));
    }

    #[test]
    fn incomparable_comparison_maps_to_distinct_error() {
        let err = eval_compare_expr(
            "c",
            Value::Text("a".into()),
            CmpOp::Lt,
            Value::Int64(1),
        )
        .expect_err("expected incomparable");
        assert!(matches!(
            err,
            PlanMutationError::ExpressionIncomparableValues { property } if property == "c"
        ));
    }

    #[test]
    fn non_finite_float_division_maps_to_distinct_error() {
        let err = eval_binary_expr(
            "f",
            Value::Float32(1.0),
            BinaryOp::Div,
            Value::Float32(0.0),
        )
        .expect_err("expected non-finite");
        assert!(matches!(
            err,
            PlanMutationError::ExpressionNonFiniteNumeric { property } if property == "f"
        ));
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
