use crate::catalog::GraphCatalog;
use crate::{
    ApiTypeDiagnostic, GleaphError, parse_block, plan_block, plan_block_with_catalog,
};
use candid::CandidType;
use gleaph_gql::Value;
use gleaph_gql::ast::{
    CompositeQueryExpr, Expr, ExprKind, NullOrder, OrderByClause, ResultStatement, ReturnBody,
    SelectBody, SessionCommand, SessionSetCommand, SimpleQueryStatement, SortDirection, SortItem,
    Statement, StatementBlock,
};
use gleaph_gql::token::Span;
use gleaph_gql::type_check::{
    TypeDiagnostic, type_check_statement_block, type_diagnostic_from_warning,
};
use gleaph_gql_planner::{GraphStats, PlanBuildOutput, PlanOp};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct PreparedParameterInfo {
    pub name: String,
    pub required: bool,
    pub nullable: bool,
    pub inferred: bool,
    pub type_hints: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub enum PreparedQueryKind {
    Query,
    Update,
}

/// User-declared dynamic sort key (`key` → sort expression text).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct PreparedSortKey {
    pub key: String,
    pub expr: String,
}

/// Requested ordering for one allowed sort key at execute time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct PreparedSortSpec {
    pub key: String,
    pub descending: bool,
    pub nulls_first: Option<bool>,
}

/// Optional prepare-time configuration (dynamic sort).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct PreparedOptions {
    pub description: Option<String>,
    pub allowed_sorts: Vec<PreparedSortKey>,
    pub default_sort: Option<Vec<PreparedSortSpec>>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CandidType)]
pub struct PreparedQueryInfo {
    pub name: String,
    pub kind: PreparedQueryKind,
    pub requires_caller: bool,
    pub extension_types: Vec<String>,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub columns: Vec<PreparedColumnInfo>,
    pub parameters: Vec<PreparedParameterInfo>,
    #[serde(default)]
    pub allowed_sorts: Vec<PreparedSortKey>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_sort: Option<Vec<PreparedSortSpec>>,
    pub type_warnings: Vec<ApiTypeDiagnostic>,
    pub explain: String,
    pub summary: crate::ApiPlanSummary,
    #[serde(default)]
    pub use_graph_pushdown: Vec<crate::ApiUseGraphPushdownInfo>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, CandidType)]
pub struct PreparedColumnInfo {
    pub name: String,
    pub expr: String,
    pub aliased: bool,
}

#[derive(Clone, Debug)]
struct PreparedSortDef {
    key: String,
    expr: Expr,
}

/// Registered prepared entry (internal).
#[derive(Clone, Debug)]
pub struct PreparedQueryEntry {
    pub info: PreparedQueryInfo,
    pub block: StatementBlock,
    pub plan: PlanBuildOutput,
    prepared_sort_defs: Vec<PreparedSortDef>,
}

#[derive(Clone, Debug, Default)]
pub struct PreparedQueryRegistry {
    entries: BTreeMap<String, PreparedQueryEntry>,
}

impl PreparedQueryRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn prepare(
        &mut self,
        name: impl Into<String>,
        source: impl Into<String>,
        options: Option<&PreparedOptions>,
        stats: Option<&dyn GraphStats>,
        catalog_base: Option<&GraphCatalog>,
    ) -> Result<PreparedQueryInfo, GleaphError> {
        let name = name.into();
        let source = source.into();
        let block = parse_block(&source)?;
        let opts = options.cloned().unwrap_or_default();
        let mut overlay = catalog_base.cloned().unwrap_or_default();
        overlay
            .apply_statement_block(&block)
            .map_err(|e| GleaphError::Catalog(e.to_string()))?;
        let plan = plan_block_with_catalog(&block, stats, &overlay, None)?;
        let kind = if plan.summary.has_dml {
            PreparedQueryKind::Update
        } else {
            PreparedQueryKind::Query
        };

        if kind == PreparedQueryKind::Update
            && (!opts.allowed_sorts.is_empty() || opts.default_sort.is_some())
        {
            return Err(GleaphError::PreparedValidation(
                "dynamic sort options are only supported for query prepared statements".into(),
            ));
        }

        let prepared_sort_defs = build_prepared_sort_defs(&block, &opts, stats)?;

        let parameters = collect_prepared_parameters(&source, &plan);
        let columns = collect_prepared_columns(&plan);
        let type_warnings: Vec<ApiTypeDiagnostic> = type_check_statement_block(&block)
            .iter()
            .map(type_diagnostic_from_warning)
            .map(|warning: TypeDiagnostic| ApiTypeDiagnostic::from(&warning))
            .collect();
        let requires_caller = plan_uses_caller(&plan.plan.ops);
        let extension_types = plan_extension_types(&plan.plan.ops);
        let info = PreparedQueryInfo {
            name: name.clone(),
            kind,
            requires_caller,
            extension_types,
            source: source.clone(),
            description: opts.description.clone(),
            columns,
            parameters,
            allowed_sorts: opts.allowed_sorts.clone(),
            default_sort: opts.default_sort.clone(),
            type_warnings,
            explain: plan.explain.clone(),
            summary: crate::ApiPlanSummary::from(&plan.summary),
            use_graph_pushdown: plan
                .plan
                .annotations
                .optimizer
                .use_graph_pushdown
                .iter()
                .map(crate::ApiUseGraphPushdownInfo::from)
                .collect(),
        };
        self.entries.insert(
            name,
            PreparedQueryEntry {
                info: info.clone(),
                block,
                plan,
                prepared_sort_defs,
            },
        );
        Ok(info)
    }

    pub fn get(&self, name: &str) -> Option<&PreparedQueryEntry> {
        self.entries.get(name)
    }

    pub fn drop(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    pub fn list(&self) -> Vec<PreparedQueryInfo> {
        self.entries
            .values()
            .map(|entry| entry.info.clone())
            .collect()
    }
}

fn composite_has_static_order_by(comp: &CompositeQueryExpr) -> bool {
    if linear_has_static_order_by(&comp.left) {
        return true;
    }
    comp.rest.iter().any(|(_, q)| linear_has_static_order_by(q))
}

fn linear_has_static_order_by(q: &gleaph_gql::ast::LinearQueryStatement) -> bool {
    for p in &q.parts {
        if matches!(p, SimpleQueryStatement::OrderBy(_)) {
            return true;
        }
    }
    let Some(res) = &q.result else {
        return false;
    };
    match res {
        ResultStatement::Return(r) => match &r.body {
            ReturnBody::Items { order_by, .. } => order_by.is_some(),
            _ => false,
        },
        ResultStatement::Select(s) => match &s.body {
            SelectBody::Star { order_by, .. } | SelectBody::Items { order_by, .. } => {
                order_by.is_some()
            }
        },
        ResultStatement::Finish => false,
    }
}

fn parse_sort_expr(expr_source: &str) -> Result<Expr, GleaphError> {
    let wrapped = format!("START TRANSACTION READ ONLY\nRETURN {expr_source}");
    let block = parse_block(&wrapped)?;
    extract_first_return_expr_from_probe(&block)
}

fn extract_first_return_expr_from_probe(block: &StatementBlock) -> Result<Expr, GleaphError> {
    let Statement::Query(comp) = &block.first else {
        return Err(GleaphError::PreparedValidation(
            "failed to parse dynamic sort expression".into(),
        ));
    };
    if !comp.rest.is_empty() {
        return Err(GleaphError::PreparedValidation(
            "sort probe must be a single linear query".into(),
        ));
    }
    let Some(ResultStatement::Return(ret)) = &comp.left.result else {
        return Err(GleaphError::PreparedValidation(
            "failed to extract dynamic sort expression".into(),
        ));
    };
    match &ret.body {
        ReturnBody::Items { items, .. } => items.first().map(|i| i.expr.clone()).ok_or_else(|| {
            GleaphError::PreparedValidation("failed to extract dynamic sort expression".into())
        }),
        _ => Err(GleaphError::PreparedValidation(
            "failed to extract dynamic sort expression (need RETURN <expr>)".into(),
        )),
    }
}

fn validate_sort_against_prepared(
    prepared_block: &StatementBlock,
    expr: &Expr,
    stats: Option<&dyn GraphStats>,
) -> Result<(), GleaphError> {
    let clause = OrderByClause {
        span: Span::DUMMY,
        items: vec![SortItem {
            span: Span::DUMMY,
            expr: expr.clone(),
            direction: None,
            null_order: None,
        }],
    };
    let probe = inject_order_by_shallow_clone(prepared_block, clause)?;
    plan_block(&probe, stats).map(|_| ())
}

fn inject_order_by_shallow_clone(
    block: &StatementBlock,
    clause: OrderByClause,
) -> Result<StatementBlock, GleaphError> {
    let mut block = block.clone();
    let Statement::Query(comp) = &mut block.first else {
        return Err(GleaphError::ExpectedQuery);
    };
    if !comp.rest.is_empty() {
        return Err(GleaphError::PreparedValidation(
            "dynamic sort requires a single linear query (no set operators)".into(),
        ));
    }
    comp.left.parts.push(SimpleQueryStatement::OrderBy(clause));
    Ok(block)
}

fn build_prepared_sort_defs(
    block: &StatementBlock,
    options: &PreparedOptions,
    stats: Option<&dyn GraphStats>,
) -> Result<Vec<PreparedSortDef>, GleaphError> {
    if options.allowed_sorts.is_empty() {
        return Ok(vec![]);
    }
    let Statement::Query(comp) = &block.first else {
        return Err(GleaphError::PreparedValidation(
            "dynamic sort is only supported when the first statement is a query".into(),
        ));
    };
    if !comp.rest.is_empty() {
        return Err(GleaphError::PreparedValidation(
            "dynamic sort requires a single linear query (no set operators)".into(),
        ));
    }
    if composite_has_static_order_by(comp) {
        return Err(GleaphError::PreparedValidation(
            "cannot use dynamic sort options together with ORDER BY in the GQL source".into(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut out = Vec::with_capacity(options.allowed_sorts.len());
    for sort in &options.allowed_sorts {
        if !seen.insert(sort.key.to_ascii_lowercase()) {
            return Err(GleaphError::PreparedValidation(format!(
                "duplicate prepared sort key '{}'",
                sort.key
            )));
        }
        let expr = parse_sort_expr(&sort.expr)?;
        validate_sort_against_prepared(block, &expr, stats)?;
        out.push(PreparedSortDef {
            key: sort.key.clone(),
            expr,
        });
    }
    if let Some(default) = &options.default_sort {
        for spec in default {
            if !out.iter().any(|d| d.key == spec.key) {
                return Err(GleaphError::PreparedValidation(format!(
                    "default_sort key '{}' is not present in allowed_sorts",
                    spec.key
                )));
            }
        }
    }
    Ok(out)
}

fn build_order_by_from_specs(
    defs: &[PreparedSortDef],
    specs: &[PreparedSortSpec],
) -> Result<OrderByClause, GleaphError> {
    let mut items = Vec::with_capacity(specs.len());
    for spec in specs {
        let def = defs.iter().find(|d| d.key == spec.key).ok_or_else(|| {
            GleaphError::PreparedValidation(format!("unknown prepared sort key '{}'", spec.key))
        })?;
        let direction = if spec.descending {
            Some(SortDirection::Desc)
        } else {
            Some(SortDirection::Asc)
        };
        let null_order = spec.nulls_first.map(|nf| {
            if nf {
                NullOrder::First
            } else {
                NullOrder::Last
            }
        });
        items.push(SortItem {
            span: Span::DUMMY,
            expr: def.expr.clone(),
            direction,
            null_order,
        });
    }
    Ok(OrderByClause {
        span: Span::DUMMY,
        items,
    })
}

/// Build the plan used for execute (`sort` overrides [`PreparedQueryInfo::default_sort`]).
pub fn plan_for_prepared_execute(
    entry: &PreparedQueryEntry,
    sort: Option<&Vec<PreparedSortSpec>>,
    stats: Option<&dyn GraphStats>,
) -> Result<PlanBuildOutput, GleaphError> {
    if entry.prepared_sort_defs.is_empty() {
        if sort.is_some() {
            return Err(GleaphError::PreparedValidation(
                "prepared statement does not support dynamic sort".into(),
            ));
        }
        return Ok(entry.plan.clone());
    }
    let specs = sort
        .or(entry.info.default_sort.as_ref())
        .filter(|s| !s.is_empty());
    let block = if let Some(specs) = specs {
        let ob = build_order_by_from_specs(&entry.prepared_sort_defs, specs)?;
        inject_order_by_shallow_clone(&entry.block, ob)?
    } else {
        entry.block.clone()
    };
    plan_block(&block, stats)
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
            PlanOp::Aggregate {
                group_by,
                aggregates,
            } => {
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

fn collect_numeric_parameter_hint(expr: &Expr, metadata: &mut BTreeMap<String, ParameterMetadata>) {
    if let ExprKind::Parameter(name) = &expr.kind {
        ensure_parameter(metadata, name)
            .type_hints
            .insert("Int64".to_owned());
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

fn plan_uses_caller(ops: &[PlanOp]) -> bool {
    ops.iter().any(op_uses_caller)
}

pub(crate) fn collect_extension_types_from_statement_block_ast(
    block: &StatementBlock,
) -> Vec<String> {
    let mut set = BTreeSet::new();
    for statement in block.iter_statements() {
        collect_extension_types_from_statement(statement, &mut set);
    }
    set.into_iter().collect()
}

fn collect_extension_types_from_statement(statement: &Statement, out: &mut BTreeSet<String>) {
    match statement {
        Statement::CreateGraphType(create) => {
            for element in &create.definition.elements {
                match element {
                    gleaph_gql::ast::GraphTypeElement::Node(node) => {
                        for property in &node.properties {
                            collect_extension_types_from_value_type(&property.value_type, out);
                        }
                    }
                    gleaph_gql::ast::GraphTypeElement::Edge(edge) => {
                        for property in &edge.properties {
                            collect_extension_types_from_value_type(&property.value_type, out);
                        }
                    }
                }
            }
        }
        Statement::Query(query) => collect_extension_types_from_composite_query_expr(query, out),
        Statement::Insert(insert) => collect_extension_types_from_insert_statement(insert, out),
        Statement::Set(set) => collect_extension_types_from_set_statement(set, out),
        Statement::Delete(delete) => collect_extension_types_from_delete_statement(delete, out),
        Statement::Session(SessionCommand::Set(set)) => match set {
            SessionSetCommand::Parameter {
                type_annotation, ..
            }
            | SessionSetCommand::GraphParameter {
                type_annotation, ..
            }
            | SessionSetCommand::BindingTableParameter {
                type_annotation, ..
            } => {
                if let Some(gleaph_gql::ast::BindingTypeAnnotation::Value(value_type)) =
                    type_annotation
                {
                    collect_extension_types_from_value_type(value_type, out);
                }
            }
            SessionSetCommand::Schema(_)
            | SessionSetCommand::Graph { .. }
            | SessionSetCommand::TimeZone(_) => {}
        },
        _ => {}
    }
}

fn collect_extension_types_from_composite_query_expr(
    query: &gleaph_gql::ast::CompositeQueryExpr,
    out: &mut BTreeSet<String>,
) {
    collect_extension_types_from_linear_query_statement(&query.left, out);
    for (_, right) in &query.rest {
        collect_extension_types_from_linear_query_statement(right, out);
    }
}

fn collect_extension_types_from_linear_query_statement(
    query: &gleaph_gql::ast::LinearQueryStatement,
    out: &mut BTreeSet<String>,
) {
    for binding in &query.prefix_bindings {
        if let Some(gleaph_gql::ast::BindingTypeAnnotation::Value(value_type)) =
            &binding.type_annotation
        {
            collect_extension_types_from_value_type(value_type, out);
        }
        match &binding.initializer {
            gleaph_gql::ast::ProcedureBindingInitializer::Expr(expr) => {
                collect_extension_types_from_expr(expr, out);
            }
            gleaph_gql::ast::ProcedureBindingInitializer::Query(query) => {
                collect_extension_types_from_composite_query_expr(query, out);
            }
            gleaph_gql::ast::ProcedureBindingInitializer::Object(_) => {}
        }
    }
    for part in &query.parts {
        collect_extension_types_from_simple_query_statement(part, out);
    }
    if let Some(result) = &query.result {
        collect_extension_types_from_result_statement(result, out);
    }
}

fn collect_extension_types_from_simple_query_statement(
    statement: &gleaph_gql::ast::SimpleQueryStatement,
    out: &mut BTreeSet<String>,
) {
    match statement {
        gleaph_gql::ast::SimpleQueryStatement::Match(stmt) => {
            collect_extension_types_from_graph_pattern(&stmt.pattern, out);
        }
        gleaph_gql::ast::SimpleQueryStatement::Filter(stmt) => {
            collect_extension_types_from_expr(&stmt.condition, out);
        }
        gleaph_gql::ast::SimpleQueryStatement::Let(stmt) => {
            for binding in &stmt.bindings {
                collect_extension_types_from_expr(&binding.value, out);
            }
        }
        gleaph_gql::ast::SimpleQueryStatement::For(stmt) => {
            collect_extension_types_from_expr(&stmt.list, out);
        }
        gleaph_gql::ast::SimpleQueryStatement::OrderBy(clause) => {
            collect_extension_types_from_order_by_clause(clause, out);
        }
        gleaph_gql::ast::SimpleQueryStatement::Limit(clause) => {
            collect_extension_types_from_expr(&clause.count, out);
        }
        gleaph_gql::ast::SimpleQueryStatement::Offset(clause) => {
            collect_extension_types_from_expr(&clause.count, out);
        }
        gleaph_gql::ast::SimpleQueryStatement::CallProcedure(call) => {
            for arg in &call.args {
                collect_extension_types_from_expr(arg, out);
            }
        }
        gleaph_gql::ast::SimpleQueryStatement::InlineProcedureCall(call) => {
            collect_extension_types_from_composite_query_expr(&call.body, out);
        }
        gleaph_gql::ast::SimpleQueryStatement::Focused { body, .. } => {
            if let Some(body) = body {
                collect_extension_types_from_simple_query_statement(body, out);
            }
        }
        gleaph_gql::ast::SimpleQueryStatement::Insert(stmt) => {
            collect_extension_types_from_insert_statement(stmt, out);
        }
        gleaph_gql::ast::SimpleQueryStatement::Set(stmt) => {
            collect_extension_types_from_set_statement(stmt, out);
        }
        gleaph_gql::ast::SimpleQueryStatement::Remove(_) => {}
        gleaph_gql::ast::SimpleQueryStatement::Delete(stmt) => {
            collect_extension_types_from_delete_statement(stmt, out);
        }
    }
}

fn collect_extension_types_from_result_statement(
    result: &gleaph_gql::ast::ResultStatement,
    out: &mut BTreeSet<String>,
) {
    match result {
        gleaph_gql::ast::ResultStatement::Return(stmt) => {
            collect_extension_types_from_return_body(&stmt.body, out);
        }
        gleaph_gql::ast::ResultStatement::Select(stmt) => {
            collect_extension_types_from_select_statement(stmt, out);
        }
        gleaph_gql::ast::ResultStatement::Finish => {}
    }
}

fn collect_extension_types_from_return_body(
    body: &gleaph_gql::ast::ReturnBody,
    out: &mut BTreeSet<String>,
) {
    match body {
        gleaph_gql::ast::ReturnBody::Star => {}
        #[cfg(feature = "cypher")]
        gleaph_gql::ast::ReturnBody::NoBindings => {}
        gleaph_gql::ast::ReturnBody::Items {
            items,
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => {
            for item in items {
                collect_extension_types_from_expr(&item.expr, out);
            }
            if let Some(group_by) = group_by {
                collect_extension_types_from_group_by_clause(group_by, out);
            }
            if let Some(having) = having {
                collect_extension_types_from_expr(having, out);
            }
            if let Some(order_by) = order_by {
                collect_extension_types_from_order_by_clause(order_by, out);
            }
            if let Some(limit) = limit {
                collect_extension_types_from_expr(&limit.count, out);
            }
            if let Some(offset) = offset {
                collect_extension_types_from_expr(&offset.count, out);
            }
        }
    }
}

fn collect_extension_types_from_select_statement(
    statement: &gleaph_gql::ast::SelectStatement,
    out: &mut BTreeSet<String>,
) {
    if let Some(source) = &statement.source {
        match source {
            gleaph_gql::ast::SelectSource::GraphMatchList(matches) => {
                for graph_match in matches {
                    collect_extension_types_from_graph_pattern(
                        &graph_match.match_statement.pattern,
                        out,
                    );
                }
            }
            gleaph_gql::ast::SelectSource::QuerySpecification(spec) => match spec {
                gleaph_gql::ast::SelectQuerySpecification::Nested(query)
                | gleaph_gql::ast::SelectQuerySpecification::GraphNested { query, .. } => {
                    collect_extension_types_from_composite_query_expr(query, out);
                }
            },
        }
    }
    match &statement.body {
        gleaph_gql::ast::SelectBody::Star {
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => {
            if let Some(group_by) = group_by {
                collect_extension_types_from_group_by_clause(group_by, out);
            }
            if let Some(having) = having {
                collect_extension_types_from_expr(having, out);
            }
            if let Some(order_by) = order_by {
                collect_extension_types_from_order_by_clause(order_by, out);
            }
            if let Some(limit) = limit {
                collect_extension_types_from_expr(&limit.count, out);
            }
            if let Some(offset) = offset {
                collect_extension_types_from_expr(&offset.count, out);
            }
        }
        gleaph_gql::ast::SelectBody::Items {
            items,
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => {
            for item in items {
                collect_extension_types_from_expr(&item.expr, out);
            }
            if let Some(group_by) = group_by {
                collect_extension_types_from_group_by_clause(group_by, out);
            }
            if let Some(having) = having {
                collect_extension_types_from_expr(having, out);
            }
            if let Some(order_by) = order_by {
                collect_extension_types_from_order_by_clause(order_by, out);
            }
            if let Some(limit) = limit {
                collect_extension_types_from_expr(&limit.count, out);
            }
            if let Some(offset) = offset {
                collect_extension_types_from_expr(&offset.count, out);
            }
        }
    }
}

fn collect_extension_types_from_order_by_clause(
    clause: &gleaph_gql::ast::OrderByClause,
    out: &mut BTreeSet<String>,
) {
    for item in &clause.items {
        collect_extension_types_from_expr(&item.expr, out);
    }
}

fn collect_extension_types_from_group_by_clause(
    clause: &gleaph_gql::ast::GroupByClause,
    out: &mut BTreeSet<String>,
) {
    for item in &clause.items {
        collect_extension_types_from_expr(item, out);
    }
}

fn collect_extension_types_from_graph_pattern(
    pattern: &gleaph_gql::ast::GraphPattern,
    out: &mut BTreeSet<String>,
) {
    if let Some(where_clause) = &pattern.where_clause {
        collect_extension_types_from_expr(where_clause, out);
    }
    for path in &pattern.paths {
        collect_extension_types_from_path_pattern_expr(&path.expr, out);
    }
}

fn collect_extension_types_from_path_pattern_expr(
    expr: &gleaph_gql::ast::PathPatternExpr,
    out: &mut BTreeSet<String>,
) {
    match expr {
        gleaph_gql::ast::PathPatternExpr::Term(term) => {
            collect_extension_types_from_path_term(term, out);
        }
        gleaph_gql::ast::PathPatternExpr::MultisetAlternation(terms)
        | gleaph_gql::ast::PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                collect_extension_types_from_path_term(term, out);
            }
        }
    }
}

fn collect_extension_types_from_path_term(
    term: &gleaph_gql::ast::PathTerm,
    out: &mut BTreeSet<String>,
) {
    for factor in &term.factors {
        collect_extension_types_from_path_primary(&factor.primary, out);
    }
}

fn collect_extension_types_from_path_primary(
    primary: &gleaph_gql::ast::PathPrimary,
    out: &mut BTreeSet<String>,
) {
    match primary {
        gleaph_gql::ast::PathPrimary::Node(node) => {
            for property in &node.properties {
                collect_extension_types_from_expr(&property.value, out);
            }
            if let Some(where_clause) = &node.where_clause {
                collect_extension_types_from_expr(where_clause, out);
            }
        }
        gleaph_gql::ast::PathPrimary::Edge(edge) => {
            for property in &edge.properties {
                collect_extension_types_from_expr(&property.value, out);
            }
            if let Some(where_clause) = &edge.where_clause {
                collect_extension_types_from_expr(where_clause, out);
            }
        }
        gleaph_gql::ast::PathPrimary::Parenthesized {
            expr, where_clause, ..
        } => {
            collect_extension_types_from_path_pattern_expr(expr, out);
            if let Some(where_clause) = where_clause {
                collect_extension_types_from_expr(where_clause, out);
            }
        }
        gleaph_gql::ast::PathPrimary::Simplified(_) => {}
    }
}

fn collect_extension_types_from_insert_statement(
    statement: &gleaph_gql::ast::InsertStatement,
    out: &mut BTreeSet<String>,
) {
    for pattern in &statement.patterns {
        for element in &pattern.elements {
            match element {
                gleaph_gql::ast::InsertElement::Node(node) => {
                    for property in &node.properties {
                        collect_extension_types_from_expr(&property.value, out);
                    }
                }
                gleaph_gql::ast::InsertElement::Edge(edge) => {
                    for property in &edge.properties {
                        collect_extension_types_from_expr(&property.value, out);
                    }
                }
            }
        }
    }
}

fn collect_extension_types_from_set_statement(
    statement: &gleaph_gql::ast::SetStatement,
    out: &mut BTreeSet<String>,
) {
    for item in &statement.items {
        match item {
            gleaph_gql::ast::SetItem::Property { value, .. }
            | gleaph_gql::ast::SetItem::AllProperties { value, .. } => {
                collect_extension_types_from_expr(value, out);
            }
            gleaph_gql::ast::SetItem::Label { .. } => {}
        }
    }
}

fn collect_extension_types_from_delete_statement(
    statement: &gleaph_gql::ast::DeleteStatement,
    out: &mut BTreeSet<String>,
) {
    for item in &statement.items {
        collect_extension_types_from_expr(item, out);
    }
}

fn plan_extension_types(ops: &[PlanOp]) -> Vec<String> {
    let mut set = BTreeSet::new();
    for op in ops {
        collect_extension_types_from_op(op, &mut set);
    }
    set.into_iter().collect()
}

fn collect_extension_types_from_op(op: &PlanOp, out: &mut BTreeSet<String>) {
    match op {
        PlanOp::PropertyFilter { predicates, .. } => {
            for expr in predicates {
                collect_extension_types_from_expr(expr, out);
            }
        }
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            for expr in group_by {
                collect_extension_types_from_expr(expr, out);
            }
            for agg in aggregates {
                if let Some(expr) = &agg.expr {
                    collect_extension_types_from_expr(expr, out);
                }
            }
        }
        PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
            for column in columns {
                collect_extension_types_from_expr(&column.expr, out);
            }
        }
        PlanOp::Sort { order_by } | PlanOp::TopK { order_by, .. } => {
            for item in &order_by.items {
                collect_extension_types_from_expr(&item.expr, out);
            }
        }
        PlanOp::Limit { count, offset } => {
            if let Some(count) = count {
                collect_extension_types_from_expr(count, out);
            }
            if let Some(offset) = offset {
                collect_extension_types_from_expr(offset, out);
            }
        }
        PlanOp::OptionalMatch { sub_plan } => {
            for nested in sub_plan {
                collect_extension_types_from_op(nested, out);
            }
        }
        PlanOp::SetOperation { right, .. }
        | PlanOp::InlineProcedureCall {
            sub_plan: right, ..
        } => {
            for nested in &right.ops {
                collect_extension_types_from_op(nested, out);
            }
        }
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            for nested in left {
                collect_extension_types_from_op(nested, out);
            }
            for nested in right {
                collect_extension_types_from_op(nested, out);
            }
        }
        PlanOp::CallProcedure { args, .. } => {
            for arg in args {
                collect_extension_types_from_expr(arg, out);
            }
        }
        PlanOp::SetProperties { items } => {
            for item in items {
                match item {
                    gleaph_gql_planner::plan::SetPlanItem::Property { value, .. }
                    | gleaph_gql_planner::plan::SetPlanItem::AllProperties { value, .. } => {
                        collect_extension_types_from_expr(value, out);
                    }
                    gleaph_gql_planner::plan::SetPlanItem::Label { .. } => {}
                }
            }
        }
        PlanOp::InsertVertex { properties, .. } | PlanOp::InsertEdge { properties, .. } => {
            for property in properties {
                collect_extension_types_from_expr(&property.value, out);
            }
        }
        _ => {}
    }
}

fn collect_extension_types_from_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match &expr.kind {
        ExprKind::IsTyped { expr, target, .. } | ExprKind::Cast { expr, target } => {
            collect_extension_types_from_value_type(target, out);
            collect_extension_types_from_expr(expr, out);
        }
        ExprKind::StringPredicate { expr, pattern, .. } => {
            collect_extension_types_from_expr(expr, out);
            collect_extension_types_from_expr(pattern, out);
        }
        ExprKind::IsNormalized { expr, .. }
        | ExprKind::IsTruth { expr, .. }
        | ExprKind::IsLabeled { expr, .. }
        | ExprKind::IsDirected { expr, .. } => collect_extension_types_from_expr(expr, out),
        ExprKind::IsSourceOf { node, edge, .. } | ExprKind::IsDestOf { node, edge, .. } => {
            collect_extension_types_from_expr(node, out);
            collect_extension_types_from_expr(edge, out);
        }
        ExprKind::PropertyExists { expr, .. } => collect_extension_types_from_expr(expr, out),
        ExprKind::Paren(inner)
        | ExprKind::Not(inner)
        | ExprKind::IsNull(inner)
        | ExprKind::IsNotNull(inner)
        | ExprKind::ElementId(inner)
        | ExprKind::PathLength(inner) => collect_extension_types_from_expr(inner, out),
        ExprKind::PropertyAccess { expr, .. } => collect_extension_types_from_expr(expr, out),
        ExprKind::BinaryOp { left, right, .. }
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Xor(left, right)
        | ExprKind::Concat(left, right)
        | ExprKind::NullIf(left, right)
        | ExprKind::Compare { left, right, .. } => {
            collect_extension_types_from_expr(left, out);
            collect_extension_types_from_expr(right, out);
        }
        ExprKind::UnaryOp { expr, .. } => collect_extension_types_from_expr(expr, out),
        ExprKind::FunctionCall { args, .. } => {
            for arg in args {
                collect_extension_types_from_expr(arg, out);
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
                collect_extension_types_from_expr(expr, out);
            }
            if let Some(expr2) = expr2 {
                collect_extension_types_from_expr(expr2, out);
            }
            if let Some(filter) = filter {
                collect_extension_types_from_expr(filter, out);
            }
            if let Some(order_by) = order_by {
                for item in &order_by.items {
                    collect_extension_types_from_expr(&item.expr, out);
                }
            }
        }
        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            collect_extension_types_from_expr(operand, out);
            for clause in when_clauses {
                collect_extension_types_from_expr(&clause.condition, out);
                collect_extension_types_from_expr(&clause.result, out);
            }
            if let Some(else_clause) = else_clause {
                collect_extension_types_from_expr(else_clause, out);
            }
        }
        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            for clause in when_clauses {
                collect_extension_types_from_expr(&clause.condition, out);
                collect_extension_types_from_expr(&clause.result, out);
            }
            if let Some(else_clause) = else_clause {
                collect_extension_types_from_expr(else_clause, out);
            }
        }
        ExprKind::Coalesce(items)
        | ExprKind::ListLiteral(items)
        | ExprKind::ListConstructor { items, .. }
        | ExprKind::AllDifferent(items)
        | ExprKind::Same(items) => {
            for item in items {
                collect_extension_types_from_expr(item, out);
            }
        }
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
            for (_, value) in fields {
                collect_extension_types_from_expr(value, out);
            }
        }
        ExprKind::PathConstructor { elements } => {
            for element in elements {
                collect_extension_types_from_expr(element, out);
            }
        }
        ExprKind::ExistsSubquery(query) | ExprKind::ValueSubquery(query) => {
            collect_extension_types_from_composite_query_expr(query, out);
        }
        ExprKind::ExistsPattern(pattern) => {
            collect_extension_types_from_graph_pattern(pattern, out);
        }
        ExprKind::LetIn { bindings, expr } => {
            for binding in bindings {
                collect_extension_types_from_expr(&binding.value, out);
            }
            collect_extension_types_from_expr(expr, out);
        }
        _ => {}
    }
}

fn collect_extension_types_from_value_type(
    ty: &gleaph_gql::ast::ValueType,
    out: &mut BTreeSet<String>,
) {
    match ty {
        gleaph_gql::ast::ValueType::ExtensionType { name } => {
            out.insert(name.parts.join(".").to_ascii_uppercase());
        }
        gleaph_gql::ast::ValueType::NotNull(inner) => {
            collect_extension_types_from_value_type(inner, out)
        }
        gleaph_gql::ast::ValueType::List { element_type, .. } => {
            collect_extension_types_from_value_type(element_type, out)
        }
        gleaph_gql::ast::ValueType::Record { fields, .. } => {
            for field in fields {
                collect_extension_types_from_value_type(&field.value_type, out);
            }
        }
        gleaph_gql::ast::ValueType::ClosedDynamicUnion(items) => {
            for item in items {
                collect_extension_types_from_value_type(item, out);
            }
        }
        _ => {}
    }
}

fn op_uses_caller(op: &PlanOp) -> bool {
    match op {
        PlanOp::PropertyFilter { predicates, .. } => predicates.iter().any(expr_uses_caller),
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            group_by.iter().any(expr_uses_caller)
                || aggregates
                    .iter()
                    .filter_map(|agg| agg.expr.as_ref())
                    .any(expr_uses_caller)
        }
        PlanOp::Project { columns, .. } | PlanOp::Materialize { columns, .. } => {
            columns.iter().any(|col| expr_uses_caller(&col.expr))
        }
        PlanOp::Sort { order_by } | PlanOp::TopK { order_by, .. } => order_by
            .items
            .iter()
            .any(|item| expr_uses_caller(&item.expr)),
        PlanOp::Limit { count, offset } => {
            count.as_ref().is_some_and(expr_uses_caller)
                || offset.as_ref().is_some_and(expr_uses_caller)
        }
        PlanOp::OptionalMatch { sub_plan } => plan_uses_caller(sub_plan),
        PlanOp::InlineProcedureCall { sub_plan, .. } => plan_uses_caller(&sub_plan.ops),
        PlanOp::SetOperation { right, .. } => plan_uses_caller(&right.ops),
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            plan_uses_caller(left) || plan_uses_caller(right)
        }
        PlanOp::CallProcedure { args, .. } => args.iter().any(expr_uses_caller),
        PlanOp::SetProperties { items } => items.iter().any(|item| match item {
            gleaph_gql_planner::plan::SetPlanItem::Property { value, .. }
            | gleaph_gql_planner::plan::SetPlanItem::AllProperties { value, .. } => {
                expr_uses_caller(value)
            }
            gleaph_gql_planner::plan::SetPlanItem::Label { .. } => false,
        }),
        PlanOp::InsertVertex { properties, .. } | PlanOp::InsertEdge { properties, .. } => {
            properties
                .iter()
                .any(|property| expr_uses_caller(&property.value))
        }
        _ => false,
    }
}

fn expr_uses_caller(expr: &Expr) -> bool {
    match &expr.kind {
        ExprKind::FunctionCall { name, args, .. } => {
            name.parts
                .last()
                .is_some_and(|part| part.eq_ignore_ascii_case("caller"))
                || args.iter().any(expr_uses_caller)
        }
        ExprKind::Paren(inner)
        | ExprKind::Not(inner)
        | ExprKind::IsNull(inner)
        | ExprKind::IsNotNull(inner)
        | ExprKind::ElementId(inner)
        | ExprKind::PathLength(inner) => expr_uses_caller(inner),
        ExprKind::PropertyAccess { expr, .. } => expr_uses_caller(expr),
        ExprKind::BinaryOp { left, right, .. }
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Xor(left, right)
        | ExprKind::Concat(left, right)
        | ExprKind::NullIf(left, right) => expr_uses_caller(left) || expr_uses_caller(right),
        ExprKind::Compare { left, right, .. } => expr_uses_caller(left) || expr_uses_caller(right),
        ExprKind::UnaryOp { expr, .. } => expr_uses_caller(expr),
        ExprKind::IsTyped { expr, .. } | ExprKind::Cast { expr, .. } => expr_uses_caller(expr),
        ExprKind::Aggregate {
            expr,
            expr2,
            filter,
            order_by,
            ..
        } => {
            expr.as_ref().is_some_and(|e| expr_uses_caller(e))
                || expr2.as_ref().is_some_and(|e| expr_uses_caller(e))
                || filter.as_ref().is_some_and(|e| expr_uses_caller(e))
                || order_by
                    .as_ref()
                    .is_some_and(|o| o.items.iter().any(|i| expr_uses_caller(&i.expr)))
        }
        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            expr_uses_caller(operand)
                || when_clauses
                    .iter()
                    .any(|wc| expr_uses_caller(&wc.condition) || expr_uses_caller(&wc.result))
                || else_clause.as_ref().is_some_and(|e| expr_uses_caller(e))
        }
        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            when_clauses
                .iter()
                .any(|wc| expr_uses_caller(&wc.condition) || expr_uses_caller(&wc.result))
                || else_clause.as_ref().is_some_and(|e| expr_uses_caller(e))
        }
        ExprKind::Coalesce(exprs)
        | ExprKind::ListLiteral(exprs)
        | ExprKind::ListConstructor { items: exprs, .. }
        | ExprKind::AllDifferent(exprs)
        | ExprKind::Same(exprs) => exprs.iter().any(expr_uses_caller),
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => {
            fields.iter().any(|(_, e)| expr_uses_caller(e))
        }
        ExprKind::PathConstructor { elements } => elements.iter().any(expr_uses_caller),
        _ => false,
    }
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
        Value::Float128(_) => "Float128".to_owned(),
        Value::Float256(_) => "Float256".to_owned(),
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
        ExprKind::PropertyAccess { expr, property } => {
            format!("{}.{}", display_expr(expr), property)
        }
        ExprKind::BinaryOp { left, op, right } => {
            format!(
                "{} {} {}",
                display_expr(left),
                display_binary_op(*op),
                display_expr(right)
            )
        }
        ExprKind::UnaryOp { op, expr } => {
            format!("{}{}", display_unary_op(*op), display_expr(expr))
        }
        ExprKind::And(left, right) => format!("{} AND {}", display_expr(left), display_expr(right)),
        ExprKind::Or(left, right) => format!("{} OR {}", display_expr(left), display_expr(right)),
        ExprKind::Not(inner) => format!("NOT {}", display_expr(inner)),
        ExprKind::Xor(left, right) => format!("{} XOR {}", display_expr(left), display_expr(right)),
        ExprKind::Compare { left, op, right } => {
            format!(
                "{} {} {}",
                display_expr(left),
                display_cmp_op(*op),
                display_expr(right)
            )
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
        Value::Float128(v) => format!("{v:?}"),
        Value::Float256(v) => format!("{v}"),
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
        let plan =
            plan_block_str("MATCH (n:User) RETURN n.name AS name, n.uid", None).expect("plan");
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

    #[test]
    fn prepared_query_info_candid_roundtrip() {
        use super::{
            PreparedColumnInfo, PreparedParameterInfo, PreparedQueryInfo, PreparedQueryKind,
        };
        use crate::{ApiDiagnosticSeverity, ApiPlanSummary, ApiTypeDiagnostic};

        let info = PreparedQueryInfo {
            name: "q".into(),
            kind: PreparedQueryKind::Query,
            requires_caller: false,
            extension_types: vec![],
            source: "MATCH (n) RETURN n".into(),
            description: None,
            columns: vec![PreparedColumnInfo {
                name: "n".into(),
                expr: "n".into(),
                aliased: false,
            }],
            parameters: vec![PreparedParameterInfo {
                name: "p".into(),
                required: true,
                nullable: false,
                inferred: false,
                type_hints: vec!["Int64".into()],
            }],
            allowed_sorts: vec![],
            default_sort: None,
            type_warnings: vec![ApiTypeDiagnostic {
                code: None,
                message: "w".into(),
                span_start: 0,
                span_end: 1,
                severity: ApiDiagnosticSeverity::Warning,
            }],
            explain: "e".into(),
            summary: ApiPlanSummary {
                estimated_rows: Some(1.0),
                estimated_cost: None,
                has_dml: false,
                dml_error_count: 0,
                dml_warning_count: 0,
                type_warning_count: 1,
            },
            use_graph_pushdown: vec![],
        };
        let bytes = candid::encode_args((&info,)).expect("encode");
        let (back,): (PreparedQueryInfo,) = candid::decode_args(&bytes).expect("decode");
        assert_eq!(info, back);
    }
}
