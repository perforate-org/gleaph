//! Mutation-facing evaluation of property expression trees (SET / INSERT properties).
//!
//! Value-level expression semantics live in `plan::expr_evaluator`; this wrapper adds
//! mutation assignment resolution, parameter lookup, and mutation-specific error context.

use super::error::PlanMutationError;
use crate::gql_execution_context::try_eval_runtime_function_call;
use crate::plan::expr_evaluator::{
    ExprEvaluationError, compare_property_values, eval_and_expr, eval_binary_expr,
    eval_compare_expr, eval_concat_expr, eval_not_expr, eval_or_expr, eval_unary_expr,
    eval_xor_expr,
};
use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind, TruthValue};
use gleaph_gql_planner::plan::PropertyAssignment;
use std::cmp::Ordering;
use std::collections::BTreeMap;

/// Trait abstraction for mutation property expression evaluation.
pub trait MutationPropertyExprEvaluation {
    fn eval(&self, property: &str, expr: &Expr) -> Result<Value, PlanMutationError>;

    fn resolve_assignments<'b>(
        &self,
        properties: &'b [PropertyAssignment],
    ) -> Result<Vec<(&'b str, Value)>, PlanMutationError>;
}

/// Evaluates property expressions against planner parameters (mutation-time `$param` bindings).
#[derive(Clone, Copy, Debug)]
pub struct MutationPropertyExprEvaluator<'a> {
    parameters: &'a BTreeMap<String, Value>,
    caller: Option<Principal>,
}

impl<'a> MutationPropertyExprEvaluator<'a> {
    pub fn new(parameters: &'a BTreeMap<String, Value>, caller: Option<Principal>) -> Self {
        Self { parameters, caller }
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
            ExprKind::UnaryOp { op, expr } => eval_unary_expr(*op, self.eval(property, expr)?)
                .map_err(|err| map_expr_eval_err(property, err)),
            ExprKind::BinaryOp { left, op, right } => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_binary_expr(left, *op, right).map_err(|err| map_expr_eval_err(property, err))
            }
            ExprKind::Not(expr) => eval_not_expr(self.eval(property, expr)?)
                .map_err(|err| map_expr_eval_err(property, err)),
            ExprKind::And(left, right) => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_and_expr(left, right).map_err(|err| map_expr_eval_err(property, err))
            }
            ExprKind::Or(left, right) => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_or_expr(left, right).map_err(|err| map_expr_eval_err(property, err))
            }
            ExprKind::Xor(left, right) => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_xor_expr(left, right).map_err(|err| map_expr_eval_err(property, err))
            }
            ExprKind::Compare { left, op, right } => {
                let left = self.eval(property, left)?;
                let right = self.eval(property, right)?;
                eval_compare_expr(left, *op, right).map_err(|err| map_expr_eval_err(property, err))
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
                eval_concat_expr(left, right).map_err(|err| map_expr_eval_err(property, err))
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
            ExprKind::FunctionCall {
                name,
                args,
                distinct,
            } => match try_eval_runtime_function_call(self.caller, name, args, *distinct) {
                Ok(Some(value)) => Ok(value),
                Ok(None) => Err(PlanMutationError::UnsupportedExpression {
                    property: property.to_owned(),
                }),
                Err(e) => Err(e.into()),
            },
            _ => Err(PlanMutationError::UnsupportedExpression {
                property: property.to_owned(),
            }),
        }
    }
}

impl<'a> MutationPropertyExprEvaluation for MutationPropertyExprEvaluator<'a> {
    fn eval(&self, property: &str, expr: &Expr) -> Result<Value, PlanMutationError> {
        MutationPropertyExprEvaluator::eval(self, property, expr)
    }

    fn resolve_assignments<'b>(
        &self,
        properties: &'b [PropertyAssignment],
    ) -> Result<Vec<(&'b str, Value)>, PlanMutationError> {
        MutationPropertyExprEvaluator::resolve_assignments(self, properties)
    }
}

fn map_expr_eval_err(property: &str, err: ExprEvaluationError) -> PlanMutationError {
    let property = property.to_owned();
    match err {
        ExprEvaluationError::InvalidValue => PlanMutationError::InvalidExpressionValue { property },
        ExprEvaluationError::DivisionByZero => {
            PlanMutationError::ExpressionDivisionByZero { property }
        }
        ExprEvaluationError::NumericOverflow => {
            PlanMutationError::ExpressionNumericOverflow { property }
        }
        ExprEvaluationError::NonFiniteNumeric => {
            PlanMutationError::ExpressionNonFiniteNumeric { property }
        }
        ExprEvaluationError::IncomparableValues => {
            PlanMutationError::ExpressionIncomparableValues { property }
        }
        ExprEvaluationError::UnsupportedNumericConversion => {
            PlanMutationError::ExpressionUnsupportedNumericConversion { property }
        }
    }
}
