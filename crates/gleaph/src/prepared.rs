use crate::{parse_block, plan_block, ApiTypeDiagnostic, GleaphError};
use gleaph_gql::ast::{Expr, ExprKind};
use gleaph_gql::type_check::{type_check_statement_block, type_diagnostic_from_warning, TypeDiagnostic};
use gleaph_gql::ast::StatementBlock;
use gleaph_gql_planner::{GraphStats, PlanBuildOutput, PlanOp};
use gleaph_gql::Value;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedParameterInfo {
    pub name: String,
    pub required: bool,
    pub nullable: bool,
    pub inferred: bool,
    pub type_hints: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum PreparedStatementKind {
    Query,
    Mutation,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PreparedStatementInfo {
    pub name: String,
    pub kind: PreparedStatementKind,
    pub source: String,
    pub columns: Vec<PreparedColumnInfo>,
    pub parameters: Vec<PreparedParameterInfo>,
    pub type_warnings: Vec<ApiTypeDiagnostic>,
    pub explain: String,
    pub summary: crate::ApiPlanSummary,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PreparedColumnInfo {
    pub name: String,
    pub expr: String,
    pub aliased: bool,
}

#[derive(Clone, Debug)]
pub struct PreparedStatementEntry {
    pub info: PreparedStatementInfo,
    pub block: StatementBlock,
    pub plan: PlanBuildOutput,
}

#[derive(Clone, Debug, Default)]
pub struct PreparedRegistry {
    entries: BTreeMap<String, PreparedStatementEntry>,
}

impl PreparedRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn prepare(
        &mut self,
        name: impl Into<String>,
        source: impl Into<String>,
        stats: Option<&dyn GraphStats>,
    ) -> Result<PreparedStatementInfo, GleaphError> {
        let name = name.into();
        let source = source.into();
        let block = parse_block(&source)?;
        let plan = plan_block(&block, stats)?;
        let kind = if plan.summary.has_dml {
            PreparedStatementKind::Mutation
        } else {
            PreparedStatementKind::Query
        };
        let parameters = collect_prepared_parameters(&source, &plan);
        let columns = collect_prepared_columns(&plan);
        let type_warnings: Vec<ApiTypeDiagnostic> = type_check_statement_block(&block)
            .iter()
            .map(type_diagnostic_from_warning)
            .map(|warning: TypeDiagnostic| ApiTypeDiagnostic::from(&warning))
            .collect();
        let info = PreparedStatementInfo {
            name: name.clone(),
            kind,
            source: source.clone(),
            columns,
            parameters,
            type_warnings,
            explain: plan.explain.clone(),
            summary: crate::ApiPlanSummary::from(&plan.summary),
        };
        self.entries.insert(
            name,
            PreparedStatementEntry {
                info: info.clone(),
                block,
                plan,
            },
        );
        Ok(info)
    }

    pub fn get(&self, name: &str) -> Option<&PreparedStatementEntry> {
        self.entries.get(name)
    }

    pub fn drop(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    pub fn list(&self) -> Vec<PreparedStatementInfo> {
        self.entries.values().map(|entry| entry.info.clone()).collect()
    }
}

#[derive(Clone, Debug, Default)]
struct ParameterMetadata {
    required: bool,
    nullable: bool,
    type_hints: BTreeSet<String>,
}

fn collect_prepared_parameters(source: &str, plan: &PlanBuildOutput) -> Vec<PreparedParameterInfo> {
    let mut metadata: BTreeMap<String, ParameterMetadata> = BTreeMap::new();

    let chars: Vec<char> = source.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] != '$' {
            i += 1;
            continue;
        }
        if i + 1 >= chars.len() {
            break;
        }
        if chars[i + 1] == '$' {
            i += 2;
            while i < chars.len() && is_parameter_continue(chars[i]) {
                i += 1;
            }
            continue;
        }
        if !is_parameter_start(chars[i + 1]) {
            i += 1;
            continue;
        }
        let start = i + 1;
        let mut end = start + 1;
        while end < chars.len() && is_parameter_continue(chars[end]) {
            end += 1;
        }
        metadata
            .entry(chars[start..end].iter().collect::<String>())
            .or_insert_with(|| ParameterMetadata {
                required: true,
                nullable: false,
                type_hints: BTreeSet::new(),
            });
        i = end;
    }

    collect_parameter_hints_from_ops(&plan.plan.ops, &mut metadata);

    metadata
        .into_iter()
        .map(|(name, meta)| PreparedParameterInfo {
            name,
            required: meta.required,
            nullable: meta.nullable,
            inferred: true,
            type_hints: meta.type_hints.into_iter().collect(),
        })
        .collect()
}

fn is_parameter_start(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphabetic()
}

fn is_parameter_continue(ch: char) -> bool {
    ch == '_' || ch.is_ascii_alphanumeric()
}

fn collect_parameter_hints_from_ops(
    ops: &[PlanOp],
    metadata: &mut BTreeMap<String, ParameterMetadata>,
) {
    for op in ops {
        match op {
            PlanOp::PropertyFilter { predicates, .. } => {
                for expr in predicates {
                    collect_parameter_hints_from_expr(expr, metadata);
                }
            }
            PlanOp::ConditionalIndexScan { candidates, .. } => {
                for candidate in candidates {
                    let entry = metadata
                        .entry(candidate.param_name.trim_start_matches('$').to_owned())
                        .or_insert_with(|| ParameterMetadata {
                            required: true,
                            nullable: false,
                            type_hints: BTreeSet::new(),
                        });
                    entry.required = false;
                    entry.nullable = true;
                }
            }
            PlanOp::Aggregate { group_by, aggregates } => {
                for expr in group_by {
                    collect_parameter_hints_from_expr(expr, metadata);
                }
                for agg in aggregates {
                    if let Some(expr) = &agg.expr {
                        collect_parameter_hints_from_expr(expr, metadata);
                    }
                }
            }
            PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
                for column in columns {
                    collect_parameter_hints_from_expr(&column.expr, metadata);
                }
            }
            PlanOp::Sort { order_by } => {
                for item in &order_by.items {
                    collect_parameter_hints_from_expr(&item.expr, metadata);
                }
            }
            PlanOp::TopK {
                order_by,
                k,
                offset,
            } => {
                for item in &order_by.items {
                    collect_parameter_hints_from_expr(&item.expr, metadata);
                }
                collect_numeric_parameter_hint(k, metadata);
                if let Some(offset) = offset {
                    collect_numeric_parameter_hint(offset, metadata);
                }
            }
            PlanOp::Limit { count, offset } => {
                if let Some(count) = count {
                    collect_numeric_parameter_hint(count, metadata);
                }
                if let Some(offset) = offset {
                    collect_numeric_parameter_hint(offset, metadata);
                }
            }
            PlanOp::OptionalMatch { sub_plan } => {
                collect_parameter_hints_from_ops(sub_plan, metadata);
            }
            PlanOp::SetOperation { right, .. } => {
                collect_parameter_hints_from_ops(&right.ops, metadata);
            }
            PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
                collect_parameter_hints_from_ops(left, metadata);
                collect_parameter_hints_from_ops(right, metadata);
            }
            PlanOp::CallProcedure { args, .. } => {
                for arg in args {
                    collect_parameter_hints_from_expr(arg, metadata);
                }
            }
            PlanOp::InlineProcedureCall { sub_plan, .. } => {
                collect_parameter_hints_from_ops(&sub_plan.ops, metadata);
            }
            PlanOp::SetProperties { items } => {
                for item in items {
                    match item {
                        gleaph_gql_planner::plan::SetPlanItem::Property { value, .. } => {
                            collect_parameter_hints_from_expr(value, metadata);
                        }
                        gleaph_gql_planner::plan::SetPlanItem::AllProperties { value, .. } => {
                            collect_parameter_hints_from_expr(value, metadata);
                        }
                        gleaph_gql_planner::plan::SetPlanItem::Label { .. } => {}
                    }
                }
            }
            PlanOp::InsertVertex { properties, .. } | PlanOp::InsertEdge { properties, .. } => {
                for property in properties {
                    collect_parameter_hints_from_expr(&property.value, metadata);
                }
            }
            _ => {}
        }
    }
}

fn collect_numeric_parameter_hint(
    expr: &Expr,
    metadata: &mut BTreeMap<String, ParameterMetadata>,
) {
    if let ExprKind::Parameter(name) = &expr.kind {
        ensure_parameter(metadata, name).type_hints.insert("Int64".to_owned());
    } else {
        collect_parameter_hints_from_expr(expr, metadata);
    }
}

fn collect_parameter_hints_from_expr(
    expr: &Expr,
    metadata: &mut BTreeMap<String, ParameterMetadata>,
) {
    match &expr.kind {
        ExprKind::Paren(inner)
        | ExprKind::Not(inner)
        | ExprKind::IsNull(inner)
        | ExprKind::IsNotNull(inner)
        | ExprKind::ElementId(inner)
        | ExprKind::PathLength(inner) => collect_parameter_hints_from_expr(inner, metadata),
        ExprKind::Parameter(name) => {
            ensure_parameter(metadata, name);
        }
        ExprKind::PropertyAccess { expr, .. } => collect_parameter_hints_from_expr(expr, metadata),
        ExprKind::BinaryOp { left, right, .. }
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Xor(left, right)
        | ExprKind::Concat(left, right)
        | ExprKind::NullIf(left, right) => {
            collect_parameter_hints_from_expr(left, metadata);
            collect_parameter_hints_from_expr(right, metadata);
        }
        ExprKind::Compare { left, right, .. } => {
            collect_parameter_hints_from_expr(left, metadata);
            collect_parameter_hints_from_expr(right, metadata);
            match (&left.kind, &right.kind) {
                (ExprKind::Parameter(name), ExprKind::Literal(value))
                | (ExprKind::Literal(value), ExprKind::Parameter(name)) => {
                    ensure_parameter(metadata, name)
                        .type_hints
                        .insert(value_type_hint(value));
                }
                _ => {}
            }
        }
        ExprKind::UnaryOp { expr, .. } => collect_parameter_hints_from_expr(expr, metadata),
        ExprKind::IsTyped { expr, target, .. } | ExprKind::Cast { expr, target } => {
            if let ExprKind::Parameter(name) = &expr.kind {
                ensure_parameter(metadata, name)
                    .type_hints
                    .insert(format!("{target:?}"));
            }
            collect_parameter_hints_from_expr(expr, metadata);
        }
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                collect_parameter_hints_from_expr(arg, metadata);
            }
        }
        ExprKind::Aggregate {
            expr,
            expr2,
            filter,
            order_by,
            ..
        } => {
            if let Some(expr) = expr {
                collect_parameter_hints_from_expr(expr, metadata);
            }
            if let Some(expr2) = expr2 {
                collect_parameter_hints_from_expr(expr2, metadata);
            }
            if let Some(filter) = filter {
                collect_parameter_hints_from_expr(filter, metadata);
            }
            if let Some(order_by) = order_by {
                for item in &order_by.items {
                    collect_parameter_hints_from_expr(&item.expr, metadata);
                }
            }
        }
        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            collect_parameter_hints_from_expr(operand, metadata);
            for clause in when_clauses {
                collect_parameter_hints_from_expr(&clause.condition, metadata);
                collect_parameter_hints_from_expr(&clause.result, metadata);
            }
            if let Some(else_clause) = else_clause {
                collect_parameter_hints_from_expr(else_clause, metadata);
            }
        }
        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            for clause in when_clauses {
                collect_parameter_hints_from_expr(&clause.condition, metadata);
                collect_parameter_hints_from_expr(&clause.result, metadata);
            }
            if let Some(else_clause) = else_clause {
                collect_parameter_hints_from_expr(else_clause, metadata);
            }
        }
        ExprKind::Coalesce(items)
        | ExprKind::ListLiteral(items)
        | ExprKind::ListConstructor { items, .. }
        | ExprKind::AllDifferent(items)
        | ExprKind::Same(items) => {
            for item in items {
                collect_parameter_hints_from_expr(item, metadata);
            }
        }
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
            for (_, value) in fields {
                collect_parameter_hints_from_expr(value, metadata);
            }
        }
        ExprKind::PathConstructor { elements } => {
            for element in elements {
                collect_parameter_hints_from_expr(element, metadata);
            }
        }
        ExprKind::Variable(_)
        | ExprKind::Literal(_)
        | ExprKind::ValueSubquery(_)
        | ExprKind::ExistsSubquery(_)
        | ExprKind::ExistsPattern(_)
        | ExprKind::SessionUser
        | ExprKind::CurrentDate
        | ExprKind::CurrentTime
        | ExprKind::CurrentTimestamp
        | ExprKind::CurrentLocalTime
        | ExprKind::CurrentLocalTimestamp => {}
        _ => {}
    }
}

fn ensure_parameter<'a>(
    metadata: &'a mut BTreeMap<String, ParameterMetadata>,
    name: &str,
) -> &'a mut ParameterMetadata {
    metadata
        .entry(name.trim_start_matches('$').to_owned())
        .or_insert_with(|| ParameterMetadata {
            required: true,
            nullable: false,
            type_hints: BTreeSet::new(),
        })
}

fn value_type_hint(value: &Value) -> String {
    match value {
        Value::Null => "Null".to_owned(),
        Value::Bool(_) => "Bool".to_owned(),
        Value::Int8(_) => "Int8".to_owned(),
        Value::Int16(_) => "Int16".to_owned(),
        Value::Int32(_) => "Int32".to_owned(),
        Value::Int64(_) => "Int64".to_owned(),
        Value::Int128(_) => "Int128".to_owned(),
        Value::Int256(_) => "Int256".to_owned(),
        Value::Uint8(_) => "Uint8".to_owned(),
        Value::Uint16(_) => "Uint16".to_owned(),
        Value::Uint32(_) => "Uint32".to_owned(),
        Value::Uint64(_) => "Uint64".to_owned(),
        Value::Uint128(_) => "Uint128".to_owned(),
        Value::Uint256(_) => "Uint256".to_owned(),
        Value::Float16(_) => "Float16".to_owned(),
        Value::Float32(_) => "Float32".to_owned(),
        Value::Float64(_) => "Float64".to_owned(),
        Value::Decimal(_) => "Decimal".to_owned(),
        Value::Text(_) => "Text".to_owned(),
        Value::Bytes(_) => "Bytes".to_owned(),
        Value::Date(_) => "Date".to_owned(),
        Value::Time(_) => "Time".to_owned(),
        Value::LocalTime(_) => "LocalTime".to_owned(),
        Value::DateTime(_, _) => "DateTime".to_owned(),
        Value::LocalDateTime(_, _) => "LocalDateTime".to_owned(),
        Value::ZonedDateTime(_, _, _) => "ZonedDateTime".to_owned(),
        Value::ZonedTime(_, _) => "ZonedTime".to_owned(),
        Value::Duration(_, _) => "Duration".to_owned(),
        Value::List(_) => "List".to_owned(),
        Value::Path(_) => "Path".to_owned(),
        Value::Record(_) => "Record".to_owned(),
        Value::Extension(value) => value.type_name().to_owned(),
    }
}

fn collect_prepared_columns(plan: &PlanBuildOutput) -> Vec<PreparedColumnInfo> {
    let Some(PlanOp::Project { columns, .. }) = plan
        .plan
        .ops
        .iter()
        .rev()
        .find(|op| matches!(op, PlanOp::Project { .. }))
    else {
        return Vec::new();
    };

    columns
        .iter()
        .enumerate()
        .map(|(index, column)| {
            let expr = display_expr(&column.expr);
            let alias = column.alias.as_ref().map(|alias| alias.to_string());
            PreparedColumnInfo {
                name: alias
                    .clone()
                    .unwrap_or_else(|| default_column_name(&column.expr, index)),
                expr,
                aliased: alias.is_some(),
            }
        })
        .collect()
}

fn default_column_name(expr: &Expr, index: usize) -> String {
    match &expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::Parameter(name) => name.trim_start_matches('$').to_owned(),
        ExprKind::PropertyAccess { property, .. } => property.clone(),
        _ => format!("expr{}", index + 1),
    }
}

fn display_expr(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Paren(inner) => format!("({})", display_expr(inner)),
        ExprKind::Literal(value) => display_value(value),
        ExprKind::Variable(name) => name.clone(),
        ExprKind::Parameter(name) => name.clone(),
        ExprKind::PropertyAccess { expr, property } => format!("{}.{}", display_expr(expr), property),
        ExprKind::BinaryOp { left, op, right } => {
            format!("{} {} {}", display_expr(left), display_binary_op(*op), display_expr(right))
        }
        ExprKind::UnaryOp { op, expr } => format!("{}{}", display_unary_op(*op), display_expr(expr)),
        ExprKind::And(left, right) => format!("{} AND {}", display_expr(left), display_expr(right)),
        ExprKind::Or(left, right) => format!("{} OR {}", display_expr(left), display_expr(right)),
        ExprKind::Not(inner) => format!("NOT {}", display_expr(inner)),
        ExprKind::Xor(left, right) => format!("{} XOR {}", display_expr(left), display_expr(right)),
        ExprKind::Compare { left, op, right } => {
            format!("{} {} {}", display_expr(left), display_cmp_op(*op), display_expr(right))
        }
        _ => format!("{:?}", expr.kind),
    }
}

fn display_value(value: &Value) -> String {
    match value {
        Value::Null => "NULL".to_owned(),
        Value::Bool(v) => v.to_string(),
        Value::Text(v) => format!("'{}'", v),
        Value::Int8(v) => v.to_string(),
        Value::Int16(v) => v.to_string(),
        Value::Int32(v) => v.to_string(),
        Value::Int64(v) => v.to_string(),
        Value::Int128(v) => v.to_string(),
        Value::Int256(v) => v.to_string(),
        Value::Uint8(v) => v.to_string(),
        Value::Uint16(v) => v.to_string(),
        Value::Uint32(v) => v.to_string(),
        Value::Uint64(v) => v.to_string(),
        Value::Uint128(v) => v.to_string(),
        Value::Uint256(v) => v.to_string(),
        Value::Float16(v) => f32::from(*v).to_string(),
        Value::Float32(v) => v.to_string(),
        Value::Float64(v) => v.to_string(),
        Value::Decimal(v) => v.to_string(),
        Value::Bytes(_) => "<bytes>".to_owned(),
        Value::Date(v) => v.to_string(),
        Value::Time(v) => v.to_string(),
        Value::LocalTime(v) => v.to_string(),
        Value::DateTime(seconds, nanos) => format!("{seconds}:{nanos}"),
        Value::LocalDateTime(seconds, nanos) => format!("{seconds}:{nanos}"),
        Value::ZonedDateTime(seconds, nanos, offset) => format!("{seconds}:{nanos}:{offset}"),
        Value::ZonedTime(nanos, offset) => format!("{nanos}:{offset}"),
        Value::Duration(months, nanos) => format!("{months}:{nanos}"),
        Value::List(_) => "<list>".to_owned(),
        Value::Path(_) => "<path>".to_owned(),
        Value::Record(_) => "<record>".to_owned(),
        Value::Extension(value) => value.to_string(),
    }
}

fn display_binary_op(op: gleaph_gql::ast::BinaryOp) -> &'static str {
    match op {
        gleaph_gql::ast::BinaryOp::Add => "+",
        gleaph_gql::ast::BinaryOp::Sub => "-",
        gleaph_gql::ast::BinaryOp::Mul => "*",
        gleaph_gql::ast::BinaryOp::Div => "/",
    }
}

fn display_unary_op(op: gleaph_gql::ast::UnaryOp) -> &'static str {
    match op {
        gleaph_gql::ast::UnaryOp::Pos => "+",
        gleaph_gql::ast::UnaryOp::Neg => "-",
    }
}

fn display_cmp_op(op: gleaph_gql::ast::CmpOp) -> &'static str {
    match op {
        gleaph_gql::ast::CmpOp::Eq => "=",
        gleaph_gql::ast::CmpOp::Ne => "<>",
        gleaph_gql::ast::CmpOp::Lt => "<",
        gleaph_gql::ast::CmpOp::Le => "<=",
        gleaph_gql::ast::CmpOp::Gt => ">",
        gleaph_gql::ast::CmpOp::Ge => ">=",
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_prepared_columns, collect_prepared_parameters};
    use crate::plan_block_str;

    #[test]
    fn collects_unique_general_parameters_without_prefix() {
        let plan = plan_block_str(
            "MATCH (n) WHERE n.uid = $uid AND n.name = $name RETURN $uid",
            None,
        )
        .expect("plan");
        let params = collect_prepared_parameters(
            "MATCH (n) WHERE n.uid = $uid AND n.name = $name RETURN $uid",
            &plan,
        );
        assert_eq!(params.len(), 2);
        assert_eq!(params[0].name, "name");
        assert!(params[0].required);
        assert!(!params[0].nullable);
        assert_eq!(params[1].name, "uid");
    }

    #[test]
    fn collects_output_columns_from_project() {
        let plan = plan_block_str("MATCH (n:User) RETURN n.name AS name, n.uid", None)
            .expect("plan");
        let columns = collect_prepared_columns(&plan);
        assert_eq!(columns.len(), 2);
        assert_eq!(columns[0].name, "name");
        assert!(columns[0].aliased);
        assert_eq!(columns[0].expr, "n.name");
        assert_eq!(columns[1].name, "uid");
        assert_eq!(columns[1].expr, "n.uid");
        assert!(!columns[1].aliased);
    }

    #[test]
    fn infers_numeric_hint_for_limit_parameter() {
        let source = "MATCH (n:User) RETURN n.uid LIMIT $limit";
        let plan = plan_block_str(source, None).expect("plan");
        let params = collect_prepared_parameters(source, &plan);
        assert_eq!(params.len(), 1);
        assert_eq!(params[0].name, "limit");
        assert!(params[0].type_hints.iter().any(|hint| hint == "Int64"));
    }
}
