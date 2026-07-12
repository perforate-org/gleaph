//! Expression type inference.

use crate::ast::*;
use crate::token::Span;

use super::env::{TypeEnv, WarningKind};
use super::types;
use super::types::*;

/// Infer the type of an expression.
pub(crate) fn infer_expr(env: &TypeEnv<'_>, expr: &Expr) -> Type {
    match &expr.kind {
        ExprKind::Literal(v) => infer_literal(v),
        ExprKind::Variable(name) => env.get(name),
        ExprKind::Parameter(_name) => Type::Unknown,
        ExprKind::Paren(inner) => infer_expr(env, inner),

        ExprKind::PropertyAccess {
            expr: target,
            property,
        } => infer_property_access(env, target, property),

        ExprKind::BinaryOp { op, left, right } => infer_binary_op(env, *op, left, right),
        ExprKind::UnaryOp { op: _, expr: inner } => {
            // Unary +/- preserve the inner type; validation is done in check_unary_op.
            infer_expr(env, inner)
        }

        // Logical operators → Bool
        ExprKind::And(_, _) | ExprKind::Or(_, _) | ExprKind::Not(_) | ExprKind::Xor(_, _) => {
            Type::Scalar(ValueType::Bool {
                keyword: Keyword::new("BOOL"),
            })
        }

        // Comparison/predicate operators → Bool
        ExprKind::Compare { .. }
        | ExprKind::IsNull(_)
        | ExprKind::IsNotNull(_)
        | ExprKind::StringPredicate { .. }
        | ExprKind::IsNormalized { .. }
        | ExprKind::IsTruth { .. }
        | ExprKind::IsLabeled { .. }
        | ExprKind::IsSourceOf { .. }
        | ExprKind::IsDestOf { .. }
        | ExprKind::IsTyped { .. }
        | ExprKind::IsDirected { .. }
        | ExprKind::AllDifferent(_)
        | ExprKind::Same(_)
        | ExprKind::PropertyExists { .. }
        | ExprKind::ExistsSubquery(_)
        | ExprKind::ExistsPattern(_) => Type::Scalar(ValueType::Bool {
            keyword: Keyword::new("BOOL"),
        }),

        #[cfg(feature = "sql-compat")]
        ExprKind::InList { .. } => Type::Scalar(ValueType::Bool {
            keyword: Keyword::new("BOOL"),
        }),

        // CASE
        ExprKind::CaseSimple {
            when_clauses,
            else_clause,
            ..
        }
        | ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            let mut types: Vec<Type> = when_clauses
                .iter()
                .map(|wc| infer_expr(env, &wc.result))
                .collect();
            if let Some(e) = else_clause {
                types.push(infer_expr(env, e));
            }
            make_union(types)
        }

        // COALESCE
        ExprKind::Coalesce(exprs) => {
            let types: Vec<Type> = exprs.iter().map(|e| infer_expr(env, e)).collect();
            make_union(types)
        }
        ExprKind::NullIf(left, _) => infer_expr(env, left),

        // Aggregate
        ExprKind::Aggregate {
            func, expr: arg, ..
        } => infer_aggregate(env, *func, arg.as_deref()),

        // Function call
        ExprKind::FunctionCall { name, args, .. } => {
            let fn_name = name.parts.first().map(String::as_str).unwrap_or("");
            infer_function_call(fn_name, env, args)
        }

        // CAST
        ExprKind::Cast { target, .. } => Type::from_value_type(target),

        // Concat → String
        ExprKind::Concat(_, _) => Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }),

        // List literal
        ExprKind::ListLiteral(elems) | ExprKind::ListConstructor { items: elems, .. } => {
            if elems.is_empty() {
                Type::TypedList(Box::new(Type::Unknown))
            } else {
                let types: Vec<Type> = elems.iter().map(|e| infer_expr(env, e)).collect();
                Type::TypedList(Box::new(make_union(types)))
            }
        }

        #[cfg(feature = "cypher")]
        ExprKind::ListIndex { list, .. } => match infer_expr(env, list) {
            Type::TypedList(inner) => *inner,
            _ => Type::Unknown,
        },
        #[cfg(feature = "cypher")]
        ExprKind::ListSlice { list, .. } => infer_expr(env, list),

        // Record literal
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
            let typed_fields = fields
                .iter()
                .map(|(k, v)| (k.clone(), infer_expr(env, v)))
                .collect();
            Type::Record(typed_fields)
        }

        // Path
        ExprKind::PathConstructor { .. } => Type::Path(PathTypeInfo::default()),
        ExprKind::PathLength(_) => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),

        // Value subquery — extract type from RETURN column without running checks.
        ExprKind::ValueSubquery(cq) => infer_value_subquery_type(env, cq),

        // Let-in: cannot do scoped binding with &TypeEnv; returns Unknown.
        // Full scoped inference is done via `infer_let_in_scoped` with &mut TypeEnv.
        ExprKind::LetIn {
            bindings,
            expr: body,
        } => {
            if bindings.is_empty() {
                infer_expr(env, body)
            } else {
                Type::Unknown
            }
        }

        // Session/datetime functions
        ExprKind::SessionUser => Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }),
        ExprKind::CurrentDate => Type::Scalar(ValueType::Date),
        ExprKind::CurrentTime => Type::Scalar(ValueType::Time),
        ExprKind::CurrentTimestamp => Type::Scalar(ValueType::Timestamp),
        ExprKind::CurrentLocalTime => Type::Scalar(ValueType::LocalTime {
            keyword: Keyword::new("LOCAL_TIME"),
        }),
        ExprKind::CurrentLocalTimestamp => Type::Scalar(ValueType::LocalDateTime {
            keyword: Keyword::new("LOCAL_TIMESTAMP"),
        }),

        // Element ID
        ExprKind::ElementId(_) => Type::Scalar(ValueType::Bytes { max_length: None }),

        // Datetime constructors
        ExprKind::DateLiteral(_) | ExprKind::DateFunction(_) => Type::Scalar(ValueType::Date),
        ExprKind::TimeLiteral(_) => Type::Scalar(ValueType::Time),
        ExprKind::TimeFunction(_) => Type::Scalar(ValueType::LocalTime {
            keyword: Keyword::new("LOCAL_TIME"),
        }),
        ExprKind::DatetimeLiteral(_) => Type::Scalar(ValueType::DateTime),
        ExprKind::TimestampLiteral(_) => Type::Scalar(ValueType::Timestamp),
        ExprKind::ZonedTimeFunction(_) => Type::Scalar(ValueType::ZonedTime {
            keyword: Keyword::new("ZONED_TIME"),
        }),
        ExprKind::ZonedDatetimeFunction(_) => Type::Scalar(ValueType::ZonedDateTime {
            keyword: Keyword::new("ZONED_DATETIME"),
        }),
        ExprKind::LocalTimeFunction(_) => Type::Scalar(ValueType::LocalTime {
            keyword: Keyword::new("LOCAL_TIME"),
        }),
        ExprKind::LocalDatetimeFunction(_) => Type::Scalar(ValueType::LocalDateTime {
            keyword: Keyword::new("LOCAL_DATETIME"),
        }),
        ExprKind::DurationLiteral(_) | ExprKind::DurationFunction(_) => {
            Type::Scalar(ValueType::Duration)
        }
        ExprKind::DurationBetween { .. } => Type::Scalar(ValueType::Duration),

        // String functions
        ExprKind::Normalize { .. }
        | ExprKind::Trim { .. }
        | ExprKind::Upper(_)
        | ExprKind::Lower(_)
        | ExprKind::FoldString { .. } => Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }),
        ExprKind::Left(_, _) | ExprKind::Right(_, _) => Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }),
        ExprKind::TrimList { list, .. } => infer_expr(env, list),

        // Length functions → Int64
        ExprKind::CharLength { .. }
        | ExprKind::ByteLength { .. }
        | ExprKind::Cardinality { .. } => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),

        // Numeric functions
        ExprKind::Abs(inner) => infer_expr(env, inner),
        ExprKind::Mod(_, _) => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),
        ExprKind::Floor(inner) | ExprKind::Ceil(inner) => infer_expr(env, inner),
        ExprKind::Sqrt(_)
        | ExprKind::Exp(_)
        | ExprKind::Ln(_)
        | ExprKind::Log(_, _)
        | ExprKind::Log10(_)
        | ExprKind::Power(_, _)
        | ExprKind::Sin(_)
        | ExprKind::Cos(_)
        | ExprKind::Tan(_)
        | ExprKind::Asin(_)
        | ExprKind::Acos(_)
        | ExprKind::Atan(_)
        | ExprKind::Degrees(_)
        | ExprKind::Radians(_)
        | ExprKind::Cot(_)
        | ExprKind::Sinh(_)
        | ExprKind::Cosh(_)
        | ExprKind::Tanh(_) => Type::Scalar(ValueType::Float64 {
            keyword: Keyword::new("FLOAT64"),
        }),

        #[cfg(feature = "sql-compat")]
        ExprKind::Atan2(_, _) => Type::Scalar(ValueType::Float64 {
            keyword: Keyword::new("FLOAT64"),
        }),
        #[cfg(feature = "sql-compat")]
        ExprKind::Sign(_) => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),
        #[cfg(feature = "sql-compat")]
        ExprKind::Truncate { expr: inner, .. } | ExprKind::Round { expr: inner, .. } => {
            infer_expr(env, inner)
        }

        // Path/graph element functions
        ExprKind::Elements(_) => Type::TypedList(Box::new(Type::Unknown)),
        #[cfg(feature = "cypher")]
        ExprKind::Nodes(_) => {
            Type::TypedList(Box::new(Type::Node(NodeTypeInfo::from_labels(vec![]))))
        }
        #[cfg(feature = "cypher")]
        ExprKind::Edges(_) => Type::TypedList(Box::new(Type::Edge(EdgeTypeInfo::from_label(None)))),
        #[cfg(feature = "cypher")]
        ExprKind::Labels(_) | ExprKind::Label(_) => Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }),
        #[cfg(feature = "cypher")]
        ExprKind::Source(_) | ExprKind::Destination(_) => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),
    }
}

// ── Property access ──

fn infer_property_access(env: &TypeEnv<'_>, target: &Expr, property: &str) -> Type {
    let target_ty = infer_expr(env, target);
    let target_optional =
        matches!(&target.kind, ExprKind::Variable(v) if env.optional_vars.contains(v.as_str()));
    let narrowed = matches!(&target.kind, ExprKind::Variable(v) if env.narrowed_nonnull.contains(&(v.clone(), property.to_string())));

    let raw_type = match &target_ty {
        Type::Node(info) => lookup_node_property(info, property, env),
        Type::Edge(info) => {
            if !info.properties.is_empty() {
                property_from_list(&info.properties, property)
            } else if let Some(ref label) = info.label {
                property_from_list(&env.schema.edge_property_types(label), property)
            } else {
                Type::Unknown
            }
        }
        Type::Record(fields) => fields
            .iter()
            .find(|(k, _)| k == property)
            .map(|(_, t)| t.clone())
            .unwrap_or(Type::Unknown),
        Type::Union(variants) => {
            let prop_types: Vec<Type> = variants
                .iter()
                .map(|v| match v {
                    Type::Node(info) => lookup_node_property(info, property, env),
                    Type::Edge(info) => {
                        if !info.properties.is_empty() {
                            property_from_list(&info.properties, property)
                        } else if let Some(ref label) = info.label {
                            property_from_list(&env.schema.edge_property_types(label), property)
                        } else {
                            Type::Unknown
                        }
                    }
                    Type::Record(fields) => fields
                        .iter()
                        .find(|(k, _)| k == property)
                        .map(|(_, t)| t.clone())
                        .unwrap_or(Type::Unknown),
                    _ => Type::Unknown,
                })
                .collect();
            make_union(prop_types)
        }
        Type::Never => Type::Never,
        _ => Type::Unknown,
    };

    if target_optional {
        strip_nonnull(raw_type)
    } else if narrowed {
        ensure_nonnull(raw_type)
    } else {
        raw_type
    }
}

/// Look up a property on a node, handling multi-label OR alternatives.
///
/// - Single label set: standard lookup.
/// - Multiple label sets (OR narrowing): intersect property types across all sets.
///   Property must exist in ALL sets with compatible types; otherwise Unknown.
fn lookup_node_property(info: &NodeTypeInfo, property: &str, env: &TypeEnv<'_>) -> Type {
    // If we have cached properties (e.g. from initial binding), use them directly.
    if !info.properties.is_empty() && info.label_sets.len() <= 1 {
        return property_from_list(&info.properties, property);
    }
    match info.label_sets.len() {
        0 => Type::Unknown,
        1 => {
            let props = env.schema.node_property_types(&info.label_sets[0]);
            property_from_list(&props, property)
        }
        _ => {
            // Multi-label OR: intersect property across all label sets.
            let mut result_type: Option<Type> = None;
            for ls in &info.label_sets {
                let props = env.schema.node_property_types(ls);
                match props.iter().find(|(name, _, _)| name == property) {
                    Some((_, vt, required)) => {
                        let ty = if *required {
                            Type::NonNull(Box::new(Type::Scalar(vt.clone())))
                        } else {
                            Type::Scalar(vt.clone())
                        };
                        match &result_type {
                            None => result_type = Some(ty),
                            Some(existing) => {
                                // If types differ, fall back to Unknown.
                                let existing_unwrapped = types::unwrap_nonnull(existing);
                                let ty_unwrapped = types::unwrap_nonnull(&ty);
                                if std::mem::discriminant(existing_unwrapped)
                                    != std::mem::discriminant(ty_unwrapped)
                                {
                                    return Type::Unknown;
                                }
                                // If either is nullable, result is nullable.
                                if !matches!(existing, Type::NonNull(_))
                                    || !matches!(&ty, Type::NonNull(_))
                                {
                                    result_type = Some(ty_unwrapped.clone());
                                }
                            }
                        }
                    }
                    None => {
                        // Property doesn't exist in this label set → Unknown.
                        return Type::Unknown;
                    }
                }
            }
            result_type.unwrap_or(Type::Unknown)
        }
    }
}

fn property_from_list(props: &[(String, ValueType, bool)], property: &str) -> Type {
    props
        .iter()
        .find(|(name, _, _)| name == property)
        .map(|(_, vt, required)| {
            let base = Type::Scalar(vt.clone());
            if *required {
                Type::NonNull(Box::new(base))
            } else {
                base
            }
        })
        .unwrap_or(Type::Unknown)
}

// ── Binary operations ──

fn infer_binary_op(env: &TypeEnv<'_>, op: BinaryOp, left: &Expr, right: &Expr) -> Type {
    let lt = infer_expr(env, left);
    let rt = infer_expr(env, right);

    if is_never(&lt) || is_never(&rt) {
        return Type::Never;
    }
    if is_unknown(&lt) || is_unknown(&rt) {
        return Type::Unknown;
    }

    match op {
        BinaryOp::Add => infer_add(&lt, &rt),
        BinaryOp::Sub => infer_sub(&lt, &rt),
        BinaryOp::Mul | BinaryOp::Div => infer_mul_div(&lt, &rt),
    }
}

fn infer_add(lt: &Type, rt: &Type) -> Type {
    match (lt, rt) {
        (Type::Scalar(a), Type::Scalar(b)) if is_string_vt(a) && is_string_vt(b) => lt.clone(),
        (Type::TypedList(_), Type::TypedList(_)) => lt.clone(),
        (Type::Scalar(a), Type::Scalar(b)) if is_integer_vt(a) && is_integer_vt(b) => {
            Type::Scalar(wider_int_vt(a, b))
        }
        (Type::Scalar(a), Type::Scalar(b)) if is_numeric_vt(a) && is_numeric_vt(b) => {
            // Float promotion.
            if is_float_vt(a) || is_float_vt(b) {
                Type::Scalar(ValueType::Float64 {
                    keyword: Keyword::new("FLOAT64"),
                })
            } else {
                Type::Unknown
            }
        }
        // Temporal + Duration
        (Type::Scalar(a), Type::Scalar(b)) if is_temporal_vt(a) && is_duration_vt(b) => lt.clone(),
        (Type::Scalar(a), Type::Scalar(b)) if is_duration_vt(a) && is_temporal_vt(b) => rt.clone(),
        (Type::Scalar(a), Type::Scalar(b)) if is_duration_vt(a) && is_duration_vt(b) => {
            Type::Scalar(ValueType::Duration)
        }
        _ => Type::Unknown,
    }
}

fn infer_sub(lt: &Type, rt: &Type) -> Type {
    match (lt, rt) {
        (Type::Scalar(a), Type::Scalar(b)) if is_integer_vt(a) && is_integer_vt(b) => {
            Type::Scalar(wider_int_vt(a, b))
        }
        (Type::Scalar(a), Type::Scalar(b)) if is_numeric_vt(a) && is_numeric_vt(b) => {
            if is_float_vt(a) || is_float_vt(b) {
                Type::Scalar(ValueType::Float64 {
                    keyword: Keyword::new("FLOAT64"),
                })
            } else {
                Type::Unknown
            }
        }
        // Temporal - Temporal → Duration
        (Type::Scalar(a), Type::Scalar(b)) if is_temporal_vt(a) && is_temporal_vt(b) => {
            Type::Scalar(ValueType::Duration)
        }
        // Temporal - Duration → same temporal type
        (Type::Scalar(a), Type::Scalar(b)) if is_temporal_vt(a) && is_duration_vt(b) => lt.clone(),
        (Type::Scalar(a), Type::Scalar(b)) if is_duration_vt(a) && is_duration_vt(b) => {
            Type::Scalar(ValueType::Duration)
        }
        _ => Type::Unknown,
    }
}

fn infer_mul_div(lt: &Type, rt: &Type) -> Type {
    match (lt, rt) {
        (Type::Scalar(a), Type::Scalar(b)) if is_integer_vt(a) && is_integer_vt(b) => {
            Type::Scalar(wider_int_vt(a, b))
        }
        _ if is_numeric(lt) && is_numeric(rt) => Type::Scalar(ValueType::Float64 {
            keyword: Keyword::new("FLOAT64"),
        }),
        // Duration * Numeric → Duration, Numeric * Duration → Duration
        (Type::Scalar(a), Type::Scalar(b)) if is_duration_vt(a) && is_numeric_vt(b) => {
            Type::Scalar(ValueType::Duration)
        }
        (Type::Scalar(a), Type::Scalar(b)) if is_numeric_vt(a) && is_duration_vt(b) => {
            Type::Scalar(ValueType::Duration)
        }
        // Duration / Numeric → Duration
        _ => Type::Unknown,
    }
}

// ── Aggregate inference ──

fn infer_aggregate(env: &TypeEnv<'_>, func: AggregateFunc, arg: Option<&Expr>) -> Type {
    match func {
        AggregateFunc::Count | AggregateFunc::CountStar => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),
        AggregateFunc::Sum => {
            let inner = arg.map(|e| infer_expr(env, e)).unwrap_or(Type::Unknown);
            match &inner {
                Type::Scalar(vt) if types::is_unsigned_vt(vt) => Type::Scalar(ValueType::Uint64 {
                    keyword: Keyword::new("UINT64"),
                }),
                Type::Scalar(vt) if types::is_integer_vt(vt) => Type::Scalar(ValueType::Int64 {
                    keyword: Keyword::new("INT64"),
                }),
                Type::Scalar(vt) if types::is_float_vt(vt) => Type::Scalar(ValueType::Float64 {
                    keyword: Keyword::new("FLOAT64"),
                }),
                other => other.clone(),
            }
        }
        AggregateFunc::Min | AggregateFunc::Max => {
            arg.map(|e| infer_expr(env, e)).unwrap_or(Type::Unknown)
        }
        AggregateFunc::Avg | AggregateFunc::PercentileCont | AggregateFunc::PercentileDisc => {
            Type::Scalar(ValueType::Float64 {
                keyword: Keyword::new("FLOAT64"),
            })
        }
        AggregateFunc::Collect => {
            let elem = arg.map(|e| infer_expr(env, e)).unwrap_or(Type::Unknown);
            Type::TypedList(Box::new(elem))
        }
        AggregateFunc::StddevSamp | AggregateFunc::StddevPop => Type::Scalar(ValueType::Float64 {
            keyword: Keyword::new("FLOAT64"),
        }),
    }
}

// ── Function call inference ──

fn infer_function_call(name: &str, _env: &TypeEnv<'_>, _args: &[Expr]) -> Type {
    let lower = name.to_ascii_lowercase();
    match lower.as_str() {
        // ── GQL standard functions that reach FunctionCall ──
        // Most GQL functions are parsed as dedicated AST nodes and never arrive here.
        // SUBSTRING is the only GQL standard function without a dedicated ExprKind.
        "substring" => Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }),
        _ => {
            #[cfg(feature = "cypher")]
            {
                return infer_cypher_function(&lower, _env, _args);
            }
            #[allow(unreachable_code)]
            Type::Unknown
        }
    }
}

/// Cypher-only function return types.
///
/// These functions exist in Cypher (Neo4j) but are not part of the GQL standard.
/// Users can still call arbitrary functions — unknown names fall through to `Type::Unknown`.
#[cfg(feature = "cypher")]
fn infer_cypher_function(name: &str, _env: &TypeEnv<'_>, _args: &[Expr]) -> Type {
    match name {
        "id" => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),
        "type" => Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }),
        "labels" | "keys" => Type::TypedList(Box::new(Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }))),
        "properties" => Type::Unknown,
        "length" => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),
        "replace" | "reverse" => Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }),
        "round" => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),
        "atan2" | "pi" => Type::Scalar(ValueType::Float64 {
            keyword: Keyword::new("FLOAT64"),
        }),
        "tostring" => Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }),
        "tointeger" => Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }),
        "tofloat" => Type::Scalar(ValueType::Float64 {
            keyword: Keyword::new("FLOAT64"),
        }),
        "toboolean" => Type::Scalar(ValueType::Bool {
            keyword: Keyword::new("BOOL"),
        }),
        "head" | "last" => Type::Unknown,
        "tail" => Type::TypedList(Box::new(Type::Unknown)),
        "range" => Type::TypedList(Box::new(Type::Scalar(ValueType::Int64 {
            keyword: Keyword::new("INT64"),
        }))),
        "split" => Type::TypedList(Box::new(Type::Scalar(ValueType::String {
            min_length: None,
            max_length: None,
        }))),
        "date" => Type::Scalar(ValueType::Date),
        "time" => Type::Scalar(ValueType::Time),
        "datetime" | "localdatetime" => Type::Scalar(ValueType::DateTime),
        "duration" => Type::Scalar(ValueType::Duration),
        _ => Type::Unknown,
    }
}

// ── Expression checking (emits warnings) ──

/// Check an expression in boolean context, emitting NonBooleanCondition if not boolean.
pub(crate) fn check_boolean_context(env: &mut TypeEnv<'_>, expr: &Expr) {
    let ty = infer_expr(env, expr);
    let unwrapped = unwrap_nonnull(&ty);
    if is_never(unwrapped) || is_unknown(unwrapped) || is_null(unwrapped) {
        return;
    }
    match unwrapped {
        Type::Scalar(ValueType::Bool { .. }) => {}
        Type::Scalar(vt) => {
            env.warn_at(
                WarningKind::NonBooleanCondition,
                format!("WHERE/FILTER condition has type {vt:?}, expected Bool"),
                expr.span,
            );
        }
        _ => {}
    }
}

/// Check a comparison expression for type compatibility.
pub(crate) fn check_comparison(env: &mut TypeEnv<'_>, left: &Expr, right: &Expr) {
    let lt = infer_expr(env, left);
    let rt = infer_expr(env, right);
    if is_unknown(&lt)
        || is_unknown(&rt)
        || is_null(&lt)
        || is_null(&rt)
        || is_never(&lt)
        || is_never(&rt)
    {
        return;
    }
    let lefts = flatten_union(&lt);
    let rights = flatten_union(&rt);
    if !lefts
        .iter()
        .any(|l| rights.iter().any(|r| types_comparable(l, r)))
    {
        env.warn_at(
            WarningKind::ComparisonMismatch,
            format!("comparison between incompatible types: {lt:?} and {rt:?}"),
            left.span,
        );
    }
}

/// Check a binary arithmetic operation for type compatibility.
pub(crate) fn check_arithmetic(env: &mut TypeEnv<'_>, op: BinaryOp, left: &Expr, right: &Expr) {
    let lt = infer_expr(env, left);
    let rt = infer_expr(env, right);
    if is_unknown(&lt)
        || is_unknown(&rt)
        || is_null(&lt)
        || is_null(&rt)
        || is_never(&lt)
        || is_never(&rt)
    {
        return;
    }
    let lefts = flatten_union(&lt);
    let rights = flatten_union(&rt);
    let ok = match op {
        BinaryOp::Add => lefts
            .iter()
            .any(|l| rights.iter().any(|r| types_addable(l, r))),
        BinaryOp::Sub | BinaryOp::Mul | BinaryOp::Div => lefts.iter().any(|l| {
            rights
                .iter()
                .any(|r| types_arithmetic(l, r, op == BinaryOp::Sub))
        }),
    };
    if !ok {
        env.warn_at(
            WarningKind::BinaryOpMismatch,
            format!("operator {op} applied to incompatible types: {lt:?} and {rt:?}"),
            left.span,
        );
    }
}

/// Check IS NULL / IS NOT NULL on NonNull properties.
pub(crate) fn check_null_test(env: &mut TypeEnv<'_>, expr: &Expr, negated: bool) {
    let ty = infer_expr(env, expr);
    if matches!(&ty, Type::NonNull(_)) {
        env.warn_at(
            WarningKind::NullCheckOnNonNull,
            if negated {
                "IS NOT NULL on a NOT NULL property (always true)".into()
            } else {
                "IS NULL on a NOT NULL property (always false)".into()
            },
            expr.span,
        );
    }
}

/// Infer the result type of a VALUE { ... } subquery.
/// Creates a temporary env with pattern bindings to infer the RETURN column type.
fn infer_value_subquery_type(env: &TypeEnv<'_>, cq: &CompositeQueryExpr) -> Type {
    use super::pattern::build_env_from_graph_pattern;

    // Fork env, add bindings from the subquery's MATCH patterns, then infer RETURN.
    let mut forked = env.fork();
    // Walk parts of the leftmost linear query to add bindings.
    for part in &cq.left.parts {
        match part {
            SimpleQueryStatement::Match(m) => {
                build_env_from_graph_pattern(&mut forked, &m.pattern, m.optional);
            }
            SimpleQueryStatement::Let(l) => {
                for b in &l.bindings {
                    let ty = infer_expr(&forked, &b.value);
                    forked.bind(b.variable.clone(), ty);
                }
            }
            SimpleQueryStatement::For(f) => {
                let list_ty = infer_expr(&forked, &f.list);
                let elem_ty = match &list_ty {
                    Type::TypedList(inner) => (**inner).clone(),
                    _ => Type::Unknown,
                };
                forked.bind(f.variable.clone(), elem_ty);
            }
            SimpleQueryStatement::Search(s) => {
                // The output alias is a scalar score/distance, represented as FLOAT64
                // in the plan until metric-specific typing lands.
                forked.bind(
                    s.output.alias.clone(),
                    Type::Scalar(ValueType::Float64 {
                        keyword: Keyword::new("FLOAT64"),
                    }),
                );
            }
            _ => {}
        }
    }
    // Extract the first RETURN item's type.
    match &cq.left.result {
        Some(ResultStatement::Return(ret)) => match &ret.body {
            ReturnBody::Items { items, .. } => items
                .first()
                .map(|item| infer_expr(&forked, &item.expr))
                .unwrap_or(Type::Unknown),
            _ => Type::Unknown,
        },
        Some(ResultStatement::Select(sel)) => match &sel.body {
            SelectBody::Items { items, .. } => items
                .first()
                .map(|item| infer_expr(&forked, &item.expr))
                .unwrap_or(Type::Unknown),
            _ => Type::Unknown,
        },
        _ => Type::Unknown,
    }
}

/// Infer type of a LET-IN expression with scoped bindings.
/// Uses a snapshot/restore to avoid leaking bindings.
#[allow(dead_code)]
pub(crate) fn infer_let_in_scoped(
    env: &mut TypeEnv<'_>,
    bindings: &[crate::ast::LetBinding],
    body: &Expr,
) -> Type {
    let snapshot = env.snapshot_bindings();
    for b in bindings {
        let ty = infer_expr(env, &b.value);
        env.bind(b.variable.clone(), ty);
    }
    let result = infer_expr(env, body);
    env.restore_bindings(snapshot);
    result
}

/// Check unary operator applied to compatible type.
pub(crate) fn check_unary_op(env: &mut TypeEnv<'_>, op: UnaryOp, inner: &Expr) {
    let ty = infer_expr(env, inner);
    if is_unknown(&ty) || is_null(&ty) || is_never(&ty) {
        return;
    }
    let unwrapped = unwrap_nonnull(&ty);
    match op {
        UnaryOp::Neg | UnaryOp::Pos => {
            if !is_numeric(unwrapped) {
                env.warn_at(
                    WarningKind::BinaryOpMismatch,
                    format!("unary {op} applied to non-numeric type: {ty:?}"),
                    inner.span,
                );
            }
        }
    }
}

/// Check string predicate operands are string-compatible.
pub(crate) fn check_string_predicate(env: &mut TypeEnv<'_>, expr: &Expr, pattern: &Expr) {
    let lt = infer_expr(env, expr);
    let rt = infer_expr(env, pattern);
    if is_unknown(&lt)
        || is_unknown(&rt)
        || is_null(&lt)
        || is_null(&rt)
        || is_never(&lt)
        || is_never(&rt)
    {
        return;
    }
    let check_string = |ty: &Type| -> bool {
        let unwrapped = unwrap_nonnull(ty);
        matches!(unwrapped, Type::Scalar(vt) if is_string_vt(vt))
            || matches!(unwrapped, Type::Unknown)
    };
    if !check_string(&lt) || !check_string(&rt) {
        env.warn_at(
            WarningKind::ComparisonMismatch,
            format!("string predicate applied to non-string types: {lt:?} and {rt:?}"),
            expr.span,
        );
    }
}

/// Check function argument types.
pub(crate) fn check_function_args(env: &mut TypeEnv<'_>, name: &str, args: &[Expr], span: Span) {
    let lower = name.to_ascii_lowercase();
    check_gql_function_args(env, &lower, args, span);
    #[cfg(feature = "cypher")]
    check_cypher_function_args(env, &lower, args, span);
}

/// GQL standard function argument type checks.
fn check_gql_function_args(env: &mut TypeEnv<'_>, name: &str, args: &[Expr], span: Span) {
    // Signature: (expected_arg_count_range, arg_type_checker)
    match name {
        // String functions expecting String argument
        "char_length" | "character_length" | "byte_length" | "octet_length" | "upper" | "lower"
        | "trim" | "btrim" | "ltrim" | "rtrim" | "normalize" | "reverse" => {
            if let Some(first) = args.first() {
                check_arg_is_string(env, first, name, span);
            }
        }
        "substring" | "substr" => {
            if let Some(first) = args.first() {
                check_arg_is_string(env, first, name, span);
            }
            // 2nd and 3rd args should be numeric (start, length)
            for arg in args.iter().skip(1) {
                check_arg_is_numeric(env, arg, name, span);
            }
        }
        "left" | "right" => {
            if let Some(first) = args.first() {
                check_arg_is_string(env, first, name, span);
            }
            if let Some(second) = args.get(1) {
                check_arg_is_numeric(env, second, name, span);
            }
        }
        // Numeric functions
        "abs" | "ceil" | "ceiling" | "floor" | "sign" | "sqrt" | "ln" | "log" | "log10" | "exp"
        | "sin" | "cos" | "tan" | "asin" | "acos" | "atan" | "degrees" | "radians" => {
            if let Some(first) = args.first() {
                check_arg_is_numeric(env, first, name, span);
            }
        }
        "round" | "truncate" => {
            if let Some(first) = args.first() {
                check_arg_is_numeric(env, first, name, span);
            }
            if let Some(second) = args.get(1) {
                check_arg_is_numeric(env, second, name, span);
            }
        }
        "power" | "mod" => {
            for arg in args.iter().take(2) {
                check_arg_is_numeric(env, arg, name, span);
            }
        }
        // element_id expects Node or Edge
        "element_id" => {
            if let Some(first) = args.first() {
                let ty = infer_expr(env, first);
                if !is_unknown(&ty) && !matches!(&ty, Type::Node(_) | Type::Edge(_)) {
                    env.warn_at(
                        WarningKind::FunctionArgMismatch,
                        format!("{name}() expects a node or edge argument, got {ty:?}"),
                        span,
                    );
                }
            }
        }
        // size/cardinality expects List
        "size" | "cardinality" => {
            if let Some(first) = args.first() {
                let ty = infer_expr(env, first);
                if !is_unknown(&ty) && !matches!(unwrap_nonnull(&ty), Type::TypedList(_)) {
                    env.warn_at(
                        WarningKind::FunctionArgMismatch,
                        format!("{name}() expects a list argument, got {ty:?}"),
                        span,
                    );
                }
            }
        }
        _ => {}
    }
}

fn check_arg_is_string(env: &mut TypeEnv<'_>, arg: &Expr, fn_name: &str, span: Span) {
    let ty = infer_expr(env, arg);
    if is_unknown(&ty) || is_null(&ty) || is_never(&ty) {
        return;
    }
    let unwrapped = unwrap_nonnull(&ty);
    if !matches!(unwrapped, Type::Scalar(vt) if is_string_vt(vt)) {
        env.warn_at(
            WarningKind::FunctionArgMismatch,
            format!("{fn_name}() expects a string argument, got {ty:?}"),
            span,
        );
    }
}

fn check_arg_is_numeric(env: &mut TypeEnv<'_>, arg: &Expr, fn_name: &str, span: Span) {
    let ty = infer_expr(env, arg);
    if is_unknown(&ty) || is_null(&ty) || is_never(&ty) {
        return;
    }
    if !is_numeric(unwrap_nonnull(&ty)) {
        env.warn_at(
            WarningKind::FunctionArgMismatch,
            format!("{fn_name}() expects a numeric argument, got {ty:?}"),
            span,
        );
    }
}

/// Cypher-only function argument type checks.
#[cfg(feature = "cypher")]
fn check_cypher_function_args(env: &mut TypeEnv<'_>, name: &str, args: &[Expr], span: Span) {
    match name {
        "id" | "labels" if args.len() == 1 => {
            let ty = infer_expr(env, &args[0]);
            if !is_unknown(&ty) && !matches!(&ty, Type::Node(_)) {
                env.warn_at(
                    WarningKind::FunctionArgMismatch,
                    format!("{name}() expects a node argument, got {ty:?}"),
                    span,
                );
            }
        }
        "type" if args.len() == 1 => {
            let ty = infer_expr(env, &args[0]);
            if !is_unknown(&ty) && !matches!(&ty, Type::Edge(_)) {
                env.warn_at(
                    WarningKind::FunctionArgMismatch,
                    format!("type() expects an edge argument, got {ty:?}"),
                    span,
                );
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::ValueType;
    use crate::type_check::schema::NoSchema;

    #[test]
    fn element_id_infers_bytes() {
        let env = TypeEnv::new(&NoSchema);
        let expr = Expr::new(ExprKind::ElementId(Box::new(Expr::new(
            ExprKind::Variable("n".to_owned()),
        ))));

        assert_eq!(
            infer_expr(&env, &expr),
            Type::Scalar(ValueType::Bytes { max_length: None })
        );
    }
}
