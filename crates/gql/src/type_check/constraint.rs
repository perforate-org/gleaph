//! Constraint-based type checking (Phase B).
//!
//! Instead of emitting warnings inline during expression traversal, this module
//! collects **typed constraints** from the AST, assigns type variables to
//! sub-expressions, and solves them in a separate pass. This decouples inference
//! from validation and enables future cross-expression type propagation.

use crate::ast::*;
use crate::token::Span;

use super::env::{TypeEnv, WarningKind};
use super::infer::infer_expr;
use super::types::*;

// ── Type variable / constraint definitions ──

/// Opaque identifier for a type variable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypeVarId(pub usize);

/// A typed constraint extracted from the AST.
#[derive(Clone, Debug)]
pub enum TypedConstraint {
    /// Operands of an arithmetic operator must be compatible.
    Arithmetic {
        op: BinaryOp,
        left: TypeVarId,
        right: TypeVarId,
        span: Span,
    },
    /// Operands of a comparison must be comparable.
    Comparison {
        left: TypeVarId,
        right: TypeVarId,
        span: Span,
    },
    /// Expression must be boolean (WHERE / FILTER / HAVING context).
    MustBeBoolean { expr: TypeVarId, span: Span },
    /// IS NULL / IS NOT NULL on an expression.
    NullTest {
        expr: TypeVarId,
        negated: bool,
        span: Span,
    },
    /// Function call argument type constraint.
    FunctionCall {
        name: String,
        args: Vec<TypeVarId>,
        span: Span,
    },
}

/// The solved type table maps type variables to their inferred types.
#[derive(Debug)]
pub struct ConstraintSet {
    /// Type assigned to each type variable, indexed by `TypeVarId.0`.
    types: Vec<Type>,
    /// Accumulated constraints.
    constraints: Vec<TypedConstraint>,
}

impl Default for ConstraintSet {
    fn default() -> Self {
        Self::new()
    }
}

impl ConstraintSet {
    pub fn new() -> Self {
        Self {
            types: Vec::new(),
            constraints: Vec::new(),
        }
    }

    /// Allocate a new type variable with the given inferred type.
    pub fn alloc(&mut self, ty: Type) -> TypeVarId {
        let id = TypeVarId(self.types.len());
        self.types.push(ty);
        id
    }

    /// Get the resolved type for a type variable.
    pub fn get(&self, id: TypeVarId) -> &Type {
        &self.types[id.0]
    }

    /// Add a constraint.
    pub fn add(&mut self, constraint: TypedConstraint) {
        self.constraints.push(constraint);
    }

    /// Collect constraints from a composite query expression.
    pub(crate) fn collect_from_composite_query(
        &mut self,
        env: &TypeEnv<'_>,
        cq: &CompositeQueryExpr,
    ) {
        self.collect_from_linear_query(env, &cq.left);
        for (_, lq) in &cq.rest {
            self.collect_from_linear_query(env, lq);
        }
    }

    fn collect_from_linear_query(&mut self, env: &TypeEnv<'_>, lq: &LinearQueryStatement) {
        for part in &lq.parts {
            self.collect_from_simple_query(env, part);
        }
        if let Some(ref result) = lq.result {
            self.collect_from_result(env, result);
        }
    }

    fn collect_from_simple_query(&mut self, env: &TypeEnv<'_>, sq: &SimpleQueryStatement) {
        match sq {
            SimpleQueryStatement::Match(m) => {
                if let Some(ref where_expr) = m.pattern.where_clause {
                    self.collect_from_expr_constraints(env, where_expr);
                    let id = self.alloc(infer_expr(env, where_expr));
                    self.add(TypedConstraint::MustBeBoolean {
                        expr: id,
                        span: where_expr.span,
                    });
                }
            }
            SimpleQueryStatement::Filter(f) => {
                self.collect_from_expr_constraints(env, &f.condition);
                let id = self.alloc(infer_expr(env, &f.condition));
                self.add(TypedConstraint::MustBeBoolean {
                    expr: id,
                    span: f.condition.span,
                });
            }
            SimpleQueryStatement::Let(l) => {
                for binding in &l.bindings {
                    self.collect_from_expr_constraints(env, &binding.value);
                }
            }
            SimpleQueryStatement::For(f) => {
                self.collect_from_expr_constraints(env, &f.list);
            }
            SimpleQueryStatement::InlineProcedureCall(ipc) => {
                self.collect_from_composite_query(env, &ipc.body);
            }
            _ => {}
        }
    }

    fn collect_from_result(&mut self, env: &TypeEnv<'_>, result: &ResultStatement) {
        match result {
            ResultStatement::Return(ret) => {
                if let ReturnBody::Items { items, having, .. } = &ret.body {
                    for item in items {
                        self.collect_from_expr_constraints(env, &item.expr);
                    }
                    if let Some(h) = having {
                        let id = self.alloc(infer_expr(env, h));
                        self.add(TypedConstraint::MustBeBoolean {
                            expr: id,
                            span: h.span,
                        });
                    }
                }
            }
            ResultStatement::Select(sel) => {
                if let SelectBody::Items { items, having, .. } = &sel.body {
                    for item in items {
                        self.collect_from_expr_constraints(env, &item.expr);
                    }
                    if let Some(h) = having {
                        let id = self.alloc(infer_expr(env, h));
                        self.add(TypedConstraint::MustBeBoolean {
                            expr: id,
                            span: h.span,
                        });
                    }
                }
            }
            ResultStatement::Finish => {}
        }
    }

    /// Recursively collect constraints from an expression tree.
    fn collect_from_expr_constraints(&mut self, env: &TypeEnv<'_>, expr: &Expr) {
        match &expr.kind {
            ExprKind::BinaryOp { op, left, right } => {
                self.collect_from_expr_constraints(env, left);
                self.collect_from_expr_constraints(env, right);
                let lt = self.alloc(infer_expr(env, left));
                let rt = self.alloc(infer_expr(env, right));
                self.add(TypedConstraint::Arithmetic {
                    op: *op,
                    left: lt,
                    right: rt,
                    span: left.span,
                });
            }
            ExprKind::Compare { left, right, .. } => {
                self.collect_from_expr_constraints(env, left);
                self.collect_from_expr_constraints(env, right);
                let lt = self.alloc(infer_expr(env, left));
                let rt = self.alloc(infer_expr(env, right));
                self.add(TypedConstraint::Comparison {
                    left: lt,
                    right: rt,
                    span: left.span,
                });
            }
            ExprKind::IsNull(inner) => {
                self.collect_from_expr_constraints(env, inner);
                let id = self.alloc(infer_expr(env, inner));
                self.add(TypedConstraint::NullTest {
                    expr: id,
                    negated: false,
                    span: inner.span,
                });
            }
            ExprKind::IsNotNull(inner) => {
                self.collect_from_expr_constraints(env, inner);
                let id = self.alloc(infer_expr(env, inner));
                self.add(TypedConstraint::NullTest {
                    expr: id,
                    negated: true,
                    span: inner.span,
                });
            }
            ExprKind::FunctionCall { name, args, .. } => {
                for arg in args {
                    self.collect_from_expr_constraints(env, arg);
                }
                let arg_ids: Vec<TypeVarId> = args
                    .iter()
                    .map(|a| self.alloc(infer_expr(env, a)))
                    .collect();
                let fn_name = name.parts.first().cloned().unwrap_or_default();
                self.add(TypedConstraint::FunctionCall {
                    name: fn_name,
                    args: arg_ids,
                    span: expr.span,
                });
            }
            ExprKind::Aggregate {
                expr: arg,
                expr2,
                filter,
                ..
            } => {
                if let Some(e) = arg {
                    self.collect_from_expr_constraints(env, e);
                }
                if let Some(e) = expr2 {
                    self.collect_from_expr_constraints(env, e);
                }
                if let Some(f) = filter {
                    let id = self.alloc(infer_expr(env, f));
                    self.add(TypedConstraint::MustBeBoolean {
                        expr: id,
                        span: f.span,
                    });
                }
            }
            ExprKind::And(l, r) | ExprKind::Or(l, r) | ExprKind::Xor(l, r) => {
                self.collect_from_expr_constraints(env, l);
                self.collect_from_expr_constraints(env, r);
            }
            ExprKind::Not(inner) => self.collect_from_expr_constraints(env, inner),
            ExprKind::CaseSimple {
                operand,
                when_clauses,
                else_clause,
            } => {
                self.collect_from_expr_constraints(env, operand);
                for wc in when_clauses {
                    self.collect_from_expr_constraints(env, &wc.condition);
                    self.collect_from_expr_constraints(env, &wc.result);
                }
                if let Some(e) = else_clause {
                    self.collect_from_expr_constraints(env, e);
                }
            }
            ExprKind::CaseSearched {
                when_clauses,
                else_clause,
            } => {
                for wc in when_clauses {
                    let id = self.alloc(infer_expr(env, &wc.condition));
                    self.add(TypedConstraint::MustBeBoolean {
                        expr: id,
                        span: wc.condition.span,
                    });
                    self.collect_from_expr_constraints(env, &wc.result);
                }
                if let Some(e) = else_clause {
                    self.collect_from_expr_constraints(env, e);
                }
            }
            ExprKind::Coalesce(exprs) => {
                for e in exprs {
                    self.collect_from_expr_constraints(env, e);
                }
            }
            ExprKind::UnaryOp { expr: inner, .. } => {
                self.collect_from_expr_constraints(env, inner);
                // UnaryOp type checking handled in Phase A.
            }
            ExprKind::StringPredicate {
                expr: target,
                pattern,
                ..
            } => {
                self.collect_from_expr_constraints(env, target);
                self.collect_from_expr_constraints(env, pattern);
            }
            ExprKind::Concat(l, r) | ExprKind::NullIf(l, r) => {
                self.collect_from_expr_constraints(env, l);
                self.collect_from_expr_constraints(env, r);
            }
            ExprKind::Cast { expr: inner, .. }
            | ExprKind::PropertyAccess { expr: inner, .. }
            | ExprKind::Paren(inner)
            | ExprKind::IsLabeled { expr: inner, .. }
            | ExprKind::IsTyped { expr: inner, .. }
            | ExprKind::IsDirected { expr: inner, .. }
            | ExprKind::IsNormalized { expr: inner, .. }
            | ExprKind::PropertyExists { expr: inner, .. }
            | ExprKind::IsTruth { expr: inner, .. } => {
                self.collect_from_expr_constraints(env, inner);
            }
            ExprKind::IsSourceOf { node, edge, .. } | ExprKind::IsDestOf { node, edge, .. } => {
                self.collect_from_expr_constraints(env, node);
                self.collect_from_expr_constraints(env, edge);
            }
            ExprKind::ListLiteral(elems) | ExprKind::ListConstructor { items: elems, .. } => {
                for e in elems {
                    self.collect_from_expr_constraints(env, e);
                }
            }
            ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
                for (_, v) in fields {
                    self.collect_from_expr_constraints(env, v);
                }
            }
            ExprKind::LetIn {
                bindings,
                expr: body,
            } => {
                for b in bindings {
                    self.collect_from_expr_constraints(env, &b.value);
                }
                self.collect_from_expr_constraints(env, body);
            }
            ExprKind::AllDifferent(exprs) | ExprKind::Same(exprs) => {
                for e in exprs {
                    self.collect_from_expr_constraints(env, e);
                }
            }
            ExprKind::ExistsSubquery(cq) | ExprKind::ValueSubquery(cq) => {
                self.collect_from_composite_query(env, cq);
            }
            _ => {
                // Leaf nodes: Literal, Variable, Parameter, etc.
            }
        }
    }

    /// Solve all collected constraints and emit warnings into `env`.
    pub(crate) fn solve(&self, env: &mut TypeEnv<'_>) {
        for (idx, constraint) in self.constraints.iter().enumerate() {
            match constraint {
                TypedConstraint::Arithmetic {
                    op,
                    left,
                    right,
                    span,
                } => {
                    let lt = self.get(*left);
                    let rt = self.get(*right);
                    if is_unknown(lt)
                        || is_unknown(rt)
                        || is_null(lt)
                        || is_null(rt)
                        || is_never(lt)
                        || is_never(rt)
                    {
                        continue;
                    }
                    let lefts = flatten_union(lt);
                    let rights = flatten_union(rt);
                    let ok = match op {
                        BinaryOp::Add => lefts
                            .iter()
                            .any(|l| rights.iter().any(|r| types_addable(l, r))),
                        BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => lefts.iter().any(|l| {
                            rights
                                .iter()
                                .any(|r| types_arithmetic(l, r, *op == BinaryOp::Sub))
                        }),
                    };
                    if !ok {
                        env.warn_at_with_provenance(
                            WarningKind::BinaryOpMismatch,
                            format!(
                                "[phase-b] operator {op} applied to incompatible types: {lt:?} and {rt:?}"
                            ),
                            *span,
                            super::env::WarningProvenance::Constraint(idx),
                        );
                    }
                }
                TypedConstraint::Comparison { left, right, span } => {
                    let lt = self.get(*left);
                    let rt = self.get(*right);
                    if is_unknown(lt)
                        || is_unknown(rt)
                        || is_null(lt)
                        || is_null(rt)
                        || is_never(lt)
                        || is_never(rt)
                    {
                        continue;
                    }
                    let lefts = flatten_union(lt);
                    let rights = flatten_union(rt);
                    if !lefts
                        .iter()
                        .any(|l| rights.iter().any(|r| types_comparable(l, r)))
                    {
                        env.warn_at_with_provenance(
                            WarningKind::ComparisonMismatch,
                            format!(
                                "[phase-b] comparison between incompatible types: {lt:?} and {rt:?}"
                            ),
                            *span,
                            super::env::WarningProvenance::Constraint(idx),
                        );
                    }
                }
                TypedConstraint::MustBeBoolean { expr, span } => {
                    let ty = self.get(*expr);
                    let unwrapped = unwrap_nonnull(ty);
                    if is_never(unwrapped) || is_unknown(unwrapped) || is_null(unwrapped) {
                        continue;
                    }
                    match unwrapped {
                        Type::Scalar(ValueType::Bool { .. }) => {}
                        Type::Scalar(vt) => {
                            env.warn_at_with_provenance(
                                WarningKind::NonBooleanCondition,
                                format!("[phase-b] condition has type {vt:?}, expected Bool"),
                                *span,
                                super::env::WarningProvenance::Constraint(idx),
                            );
                        }
                        _ => {}
                    }
                }
                TypedConstraint::NullTest {
                    expr,
                    negated,
                    span,
                } => {
                    let ty = self.get(*expr);
                    if matches!(ty, Type::NonNull(_)) {
                        env.warn_at_with_provenance(
                            WarningKind::NullCheckOnNonNull,
                            if *negated {
                                "[phase-b] IS NOT NULL on a NOT NULL property (always true)".into()
                            } else {
                                "[phase-b] IS NULL on a NOT NULL property (always false)".into()
                            },
                            *span,
                            super::env::WarningProvenance::Constraint(idx),
                        );
                    }
                }
                TypedConstraint::FunctionCall {
                    name: _name,
                    args: _args,
                    span: _span,
                } => {
                    // GQL standard: no argument-type constraints for generic FunctionCall.
                    // Cypher-only checks below.
                    #[cfg(feature = "cypher")]
                    {
                        let lower = _name.to_ascii_lowercase();
                        match lower.as_str() {
                            "id" | "labels" if _args.len() == 1 => {
                                let ty = self.get(_args[0]);
                                if !is_unknown(ty) && !matches!(ty, Type::Node(_)) {
                                    env.warn_at_with_provenance(
                                        WarningKind::FunctionArgMismatch,
                                        format!(
                                            "[phase-b] {lower}() expects a node argument, got {ty:?}"
                                        ),
                                        *_span,
                                        super::env::WarningProvenance::Constraint(idx),
                                    );
                                }
                            }
                            "type" if _args.len() == 1 => {
                                let ty = self.get(_args[0]);
                                if !is_unknown(ty) && !matches!(ty, Type::Edge(_)) {
                                    env.warn_at_with_provenance(
                                        WarningKind::FunctionArgMismatch,
                                        format!(
                                            "[phase-b] type() expects an edge argument, got {ty:?}"
                                        ),
                                        *_span,
                                        super::env::WarningProvenance::Constraint(idx),
                                    );
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
}
