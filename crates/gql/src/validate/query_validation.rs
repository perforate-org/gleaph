use crate::ast::*;
use crate::error::GqlError;
use rapidhash::RapidHashSet;

use super::{
    VResult, validate_call_procedure, validate_catalog_object_name, validate_delete_vars,
    validate_expr, validate_inline_scope_vars, validate_insert, validate_let, validate_remove_vars,
    validate_set_vars, validate_yield_alias_uniqueness, verr,
};

pub(super) fn validate_composite_query(
    cq: &CompositeQueryExpr,
    outer: &RapidHashSet<String>,
    outer_graph: &RapidHashSet<String>,
) -> VResult {
    validate_linear_query(&cq.left, outer, outer_graph)?;
    let expected_bindings = linear_query_result_layout(&cq.left, outer)?;
    for (_, rhs) in &cq.rest {
        validate_linear_query(rhs, outer, outer_graph)?;
        let rhs_bindings = linear_query_result_layout(rhs, outer)?;
        if rhs_bindings != expected_bindings {
            return Err(verr(
                "composite query branches must expose the same result bindings",
            ));
        }
    }
    Ok(())
}

fn validate_linear_query(
    lq: &LinearQueryStatement,
    outer: &RapidHashSet<String>,
    outer_graph: &RapidHashSet<String>,
) -> VResult {
    let mut scope = outer.clone();
    let mut graph_scope = outer_graph.clone();
    validate_procedure_bindings(&lq.prefix_bindings, &mut scope, &mut graph_scope)?;

    for part in &lq.parts {
        match part {
            SimpleQueryStatement::Match(m) => {
                if let Some(graph_name) = &m.graph_name {
                    validate_graph_reference(graph_name, &scope, &graph_scope)?;
                }
                let mut match_scope = scope.clone();
                collect_pattern_bindings(&m.pattern, &mut match_scope)?;
                if let Some(ref w) = m.pattern.where_clause {
                    validate_expr(w, &match_scope, &graph_scope)?;
                }
                if let Some(yields) = &m.yield_items {
                    validate_yield_alias_uniqueness(yields, "MATCH YIELD")?;
                    for yi in yields {
                        if !match_scope.contains(&yi.name) {
                            return Err(verr(&format!(
                                "MATCH YIELD variable '{}' is not in scope",
                                yi.name
                            )));
                        }
                    }
                    scope = project_yield_items(yields);
                } else {
                    scope = match_scope;
                }
            }
            SimpleQueryStatement::Filter(f) => validate_expr(&f.condition, &scope, &graph_scope)?,
            SimpleQueryStatement::Let(l) => validate_let(&l.bindings, &mut scope, &graph_scope)?,
            SimpleQueryStatement::For(f) => {
                validate_expr(&f.list, &scope, &graph_scope)?;
                scope.insert(f.variable.clone());
                if let Some(ref ord) = f.ordinality {
                    scope.insert(ord.variable.clone());
                }
            }
            SimpleQueryStatement::OrderBy(ob) => {
                for item in &ob.items {
                    validate_expr(&item.expr, &scope, &graph_scope)?;
                }
            }
            SimpleQueryStatement::Limit(lim) => validate_expr(&lim.count, &scope, &graph_scope)?,
            SimpleQueryStatement::Offset(off) => validate_expr(&off.count, &scope, &graph_scope)?,
            SimpleQueryStatement::CallProcedure(cp) => {
                validate_call_procedure(cp)?;
                for arg in &cp.args {
                    validate_expr(arg, &scope, &graph_scope)?;
                }
                if let Some(ref yields) = cp.yield_items {
                    for yi in yields {
                        let name = yi.alias.as_ref().unwrap_or(&yi.name);
                        scope.insert(name.clone());
                    }
                }
            }
            SimpleQueryStatement::InlineProcedureCall(ipc) => {
                validate_inline_scope_vars(ipc)?;
                validate_inline_procedure_call(ipc, &scope, &graph_scope)?;
                let outer_scope = scope.clone();
                collect_inline_procedure_result_bindings(ipc, &outer_scope, &mut scope)?;
            }
            SimpleQueryStatement::Focused { graph, body } => {
                validate_graph_reference(graph, &scope, &graph_scope)?;
                if let Some(inner) = body {
                    match inner.as_ref() {
                        SimpleQueryStatement::Match(m) => {
                            if let Some(gn) = &m.graph_name {
                                validate_graph_reference(gn, &scope, &graph_scope)?;
                            }
                            let mut match_scope = scope.clone();
                            collect_pattern_bindings(&m.pattern, &mut match_scope)?;
                            if let Some(ref w) = m.pattern.where_clause {
                                validate_expr(w, &match_scope, &graph_scope)?;
                            }
                            if let Some(yields) = &m.yield_items {
                                for yi in yields {
                                    if !match_scope.contains(&yi.name) {
                                        return Err(verr(&format!(
                                            "MATCH YIELD variable '{}' is not in scope",
                                            yi.name
                                        )));
                                    }
                                }
                                scope = project_yield_items(yields);
                            } else {
                                scope = match_scope;
                            }
                        }
                        SimpleQueryStatement::CallProcedure(cp) => {
                            validate_call_procedure(cp)?;
                            if let Some(yields) = &cp.yield_items {
                                for yi in yields {
                                    let name = yi.alias.as_ref().unwrap_or(&yi.name);
                                    scope.insert(name.clone());
                                }
                            }
                        }
                        SimpleQueryStatement::Insert(ins) => validate_insert(ins)?,
                        SimpleQueryStatement::Set(set) => {
                            validate_set_vars(&set.items, &scope, &graph_scope)?;
                        }
                        SimpleQueryStatement::Remove(rem) => {
                            validate_remove_vars(&rem.items, &scope)?;
                        }
                        _ => {}
                    }
                }
            }
            SimpleQueryStatement::Insert(ins) => validate_insert(ins)?,
            SimpleQueryStatement::Set(set) => validate_set_vars(&set.items, &scope, &graph_scope)?,
            SimpleQueryStatement::Remove(rem) => validate_remove_vars(&rem.items, &scope)?,
            SimpleQueryStatement::Delete(del) => validate_delete_vars(del, &scope, &graph_scope)?,
        }
    }

    if let Some(ref result) = lq.result {
        match result {
            ResultStatement::Return(ret) => validate_return(ret, &scope, &graph_scope)?,
            ResultStatement::Select(sel) => validate_select(sel, &scope, &graph_scope)?,
            ResultStatement::Finish => {}
        }
    }

    Ok(())
}

fn project_yield_items(yields: &[YieldItem]) -> RapidHashSet<String> {
    let mut projected = RapidHashSet::default();
    for item in yields {
        projected.insert(item.alias.clone().unwrap_or_else(|| item.name.clone()));
    }
    projected
}

fn validate_inline_procedure_call(
    ipc: &InlineProcedureCall,
    scope: &RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    let body_scope = inline_procedure_body_scope(ipc, scope)?;
    validate_composite_query(&ipc.body, &body_scope, graph_scope)
}

fn inline_procedure_body_scope(
    ipc: &InlineProcedureCall,
    scope: &RapidHashSet<String>,
) -> Result<RapidHashSet<String>, GqlError> {
    if ipc.scope_vars.is_empty() {
        return Ok(scope.clone());
    }
    let mut selected = RapidHashSet::default();
    for name in &ipc.scope_vars {
        if !scope.contains(name) {
            return Err(verr(&format!(
                "inline procedure scope variable '{name}' is not in scope"
            )));
        }
        selected.insert(name.clone());
    }
    Ok(selected)
}

fn validate_procedure_bindings(
    bindings: &[ProcedureBindingDefinition],
    scope: &mut RapidHashSet<String>,
    graph_scope: &mut RapidHashSet<String>,
) -> VResult {
    for binding in bindings {
        match &binding.initializer {
            ProcedureBindingInitializer::Expr(expr) => validate_expr(expr, scope, graph_scope)?,
            ProcedureBindingInitializer::Object(_) => {}
            ProcedureBindingInitializer::Query(query) => {
                validate_composite_query(query, scope, graph_scope)?
            }
        }
        if matches!(binding.kind, ProcedureBindingKind::Graph) {
            graph_scope.insert(binding.variable.clone());
        }
        scope.insert(binding.variable.clone());
    }
    Ok(())
}

pub(super) fn validate_return(
    ret: &ReturnStatement,
    scope: &RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    match &ret.body {
        ReturnBody::Star => Ok(()),
        #[cfg(feature = "cypher")]
        ReturnBody::NoBindings => Ok(()),
        ReturnBody::Items {
            items,
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => {
            if items.is_empty() {
                return Err(verr(
                    "RETURN must have at least one item, *, or NO BINDINGS",
                ));
            }
            validate_result_item_output_name_uniqueness(items, "RETURN")?;
            let mut extended = scope.clone();
            for item in items {
                validate_expr(&item.expr, scope, graph_scope)?;
                if let Some(gb) = group_by
                    && !expr_is_group_compatible(&item.expr, &gb.items)
                {
                    return Err(verr(
                        "RETURN item must be grouped or aggregated when GROUP BY is present",
                    ));
                }
                if let Some(ref alias) = item.alias {
                    extended.insert(alias.clone());
                }
            }
            if let Some(gb) = group_by {
                for expr in &gb.items {
                    validate_expr(expr, scope, graph_scope)?;
                }
            }
            if let Some(expr) = having {
                validate_expr(expr, &extended, graph_scope)?;
            }
            if let Some(ob) = order_by {
                for item in &ob.items {
                    if let Some(gb) = group_by
                        && !expr_is_group_compatible(&item.expr, &gb.items)
                    {
                        return Err(verr(
                            "ORDER BY expression must be grouped or aggregated when GROUP BY is present",
                        ));
                    }
                    validate_expr(&item.expr, &extended, graph_scope)?;
                }
            }
            if let Some(lim) = limit {
                validate_expr(&lim.count, scope, graph_scope)?;
            }
            if let Some(off) = offset {
                validate_expr(&off.count, scope, graph_scope)?;
            }
            Ok(())
        }
    }
}

pub(super) fn validate_select(
    sel: &SelectStatement,
    scope: &RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    let mut scope = scope.clone();
    let graph_scope = graph_scope.clone();
    if let Some(source) = &sel.source {
        validate_select_source(source, &mut scope, &graph_scope)?;
    }
    match &sel.body {
        SelectBody::Star {
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => {
            if let Some(gb) = group_by {
                for expr in &gb.items {
                    validate_expr(expr, &scope, &graph_scope)?;
                }
            }
            if let Some(expr) = having {
                if let Some(gb) = group_by
                    && !expr_is_group_compatible(expr, &gb.items)
                {
                    return Err(verr(
                        "HAVING expression must be grouped or aggregated when GROUP BY is present",
                    ));
                }
                validate_expr(expr, &scope, &graph_scope)?;
            }
            if let Some(ob) = order_by {
                for item in &ob.items {
                    if let Some(gb) = group_by
                        && !expr_is_group_compatible(&item.expr, &gb.items)
                    {
                        return Err(verr(
                            "ORDER BY expression must be grouped or aggregated when GROUP BY is present",
                        ));
                    }
                    validate_expr(&item.expr, &scope, &graph_scope)?;
                }
            }
            if let Some(lim) = limit {
                validate_expr(&lim.count, &scope, &graph_scope)?;
            }
            if let Some(off) = offset {
                validate_expr(&off.count, &scope, &graph_scope)?;
            }
            Ok(())
        }
        SelectBody::Items {
            items,
            group_by,
            having,
            order_by,
            limit,
            offset,
        } => {
            if items.is_empty() {
                return Err(verr("SELECT must have at least one item or *"));
            }
            validate_result_item_output_name_uniqueness(items, "SELECT")?;
            let mut extended = scope.clone();
            for item in items {
                validate_expr(&item.expr, &scope, &graph_scope)?;
                if let Some(gb) = group_by
                    && !expr_is_group_compatible(&item.expr, &gb.items)
                {
                    return Err(verr(
                        "SELECT item must be grouped or aggregated when GROUP BY is present",
                    ));
                }
                if let Some(ref alias) = item.alias {
                    extended.insert(alias.clone());
                }
            }
            if let Some(gb) = group_by {
                for expr in &gb.items {
                    validate_expr(expr, &scope, &graph_scope)?;
                }
            }
            if let Some(expr) = having {
                if let Some(gb) = group_by
                    && !expr_is_group_compatible(expr, &gb.items)
                {
                    return Err(verr(
                        "HAVING expression must be grouped or aggregated when GROUP BY is present",
                    ));
                }
                validate_expr(expr, &extended, &graph_scope)?;
            }
            if let Some(ob) = order_by {
                for item in &ob.items {
                    if let Some(gb) = group_by
                        && !expr_is_group_compatible(&item.expr, &gb.items)
                    {
                        return Err(verr(
                            "ORDER BY expression must be grouped or aggregated when GROUP BY is present",
                        ));
                    }
                    validate_expr(&item.expr, &extended, &graph_scope)?;
                }
            }
            if let Some(lim) = limit {
                validate_expr(&lim.count, &scope, &graph_scope)?;
            }
            if let Some(off) = offset {
                validate_expr(&off.count, &scope, &graph_scope)?;
            }
            Ok(())
        }
    }
}

fn validate_result_item_output_name_uniqueness(items: &[ReturnItem], context: &str) -> VResult {
    let mut seen = RapidHashSet::default();
    for item in items {
        let output_name = item.alias.as_ref().or(match &item.expr.kind {
            ExprKind::Variable(name) => Some(name),
            _ => None,
        });
        if let Some(output_name) = output_name
            && !seen.insert(output_name.clone())
        {
            return Err(verr(&format!(
                "{context}: duplicate output name '{output_name}'"
            )));
        }
    }
    Ok(())
}

fn validate_select_source(
    source: &SelectSource,
    scope: &mut RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    match source {
        SelectSource::GraphMatchList(items) => {
            for item in items {
                validate_graph_reference(&item.graph, scope, graph_scope)?;
                collect_pattern_bindings(&item.match_statement.pattern, scope)?;
                if let Some(ref w) = item.match_statement.pattern.where_clause {
                    validate_expr(w, scope, graph_scope)?;
                }
            }
            Ok(())
        }
        SelectSource::QuerySpecification(SelectQuerySpecification::Nested(query)) => {
            validate_composite_query(query, scope, graph_scope)?;
            collect_query_result_bindings(query, scope);
            Ok(())
        }
        SelectSource::QuerySpecification(SelectQuerySpecification::GraphNested {
            graph,
            query,
        }) => {
            validate_graph_reference(graph, scope, graph_scope)?;
            validate_composite_query(query, scope, graph_scope)?;
            collect_query_result_bindings(query, scope);
            Ok(())
        }
    }
}

fn collect_query_result_bindings(query: &CompositeQueryExpr, scope: &mut RapidHashSet<String>) {
    if let Ok(bindings) = composite_query_result_bindings(query, scope) {
        scope.extend(bindings);
    }
}

fn collect_inline_procedure_result_bindings(
    ipc: &InlineProcedureCall,
    outer_scope: &RapidHashSet<String>,
    scope: &mut RapidHashSet<String>,
) -> VResult {
    let body_scope = inline_procedure_body_scope(ipc, outer_scope)?;
    let bindings = composite_query_result_bindings(&ipc.body, &body_scope)?;
    scope.extend(bindings);
    Ok(())
}

pub(super) fn composite_query_result_bindings(
    query: &CompositeQueryExpr,
    outer: &RapidHashSet<String>,
) -> Result<RapidHashSet<String>, GqlError> {
    let expected = linear_query_result_bindings(&query.left, outer)?;
    for (_, rhs) in &query.rest {
        let rhs_bindings = linear_query_result_bindings(rhs, outer)?;
        if rhs_bindings != expected {
            return Err(verr(
                "composite query branches must expose the same result bindings",
            ));
        }
    }
    Ok(expected)
}

fn linear_query_result_layout(
    query: &LinearQueryStatement,
    outer: &RapidHashSet<String>,
) -> Result<(RapidHashSet<String>, usize), GqlError> {
    let mut query_scope = outer.clone();
    collect_linear_query_bindings(query, &mut query_scope);
    let result_scope = linear_query_result_bindings(query, outer)?;
    Ok((
        result_scope,
        result_column_count(query.result.as_ref(), &query_scope),
    ))
}

fn linear_query_result_bindings(
    query: &LinearQueryStatement,
    outer: &RapidHashSet<String>,
) -> Result<RapidHashSet<String>, GqlError> {
    let mut query_scope = outer.clone();
    collect_linear_query_bindings(query, &mut query_scope);
    let mut result_scope = RapidHashSet::default();
    collect_result_bindings(query.result.as_ref(), &query_scope, &mut result_scope);
    Ok(result_scope)
}

fn collect_linear_query_bindings(query: &LinearQueryStatement, scope: &mut RapidHashSet<String>) {
    for binding in &query.prefix_bindings {
        scope.insert(binding.variable.clone());
    }
    for part in &query.parts {
        match part {
            SimpleQueryStatement::Match(m) => {
                let mut match_scope = scope.clone();
                let _ = collect_pattern_bindings(&m.pattern, &mut match_scope);
                if let Some(yields) = &m.yield_items {
                    *scope = project_yield_items(yields);
                } else {
                    *scope = match_scope;
                }
            }
            SimpleQueryStatement::Let(l) => {
                for binding in &l.bindings {
                    scope.insert(binding.variable.clone());
                }
            }
            SimpleQueryStatement::For(f) => {
                scope.insert(f.variable.clone());
                if let Some(ord) = &f.ordinality {
                    scope.insert(ord.variable.clone());
                }
            }
            SimpleQueryStatement::CallProcedure(cp) => {
                if let Some(yields) = &cp.yield_items {
                    for item in yields {
                        scope.insert(item.alias.clone().unwrap_or_else(|| item.name.clone()));
                    }
                }
            }
            SimpleQueryStatement::InlineProcedureCall(ipc) => {
                let outer_scope = scope.clone();
                let _ = collect_inline_procedure_result_bindings(ipc, &outer_scope, scope);
            }
            SimpleQueryStatement::Focused { body, .. } => {
                if let Some(inner) = body {
                    match inner.as_ref() {
                        SimpleQueryStatement::Match(m) => {
                            let mut match_scope = scope.clone();
                            let _ = collect_pattern_bindings(&m.pattern, &mut match_scope);
                            if let Some(yields) = &m.yield_items {
                                *scope = project_yield_items(yields);
                            } else {
                                *scope = match_scope;
                            }
                        }
                        SimpleQueryStatement::CallProcedure(cp) => {
                            if let Some(yields) = &cp.yield_items {
                                for item in yields {
                                    scope.insert(
                                        item.alias.clone().unwrap_or_else(|| item.name.clone()),
                                    );
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
            SimpleQueryStatement::Filter(_)
            | SimpleQueryStatement::OrderBy(_)
            | SimpleQueryStatement::Limit(_)
            | SimpleQueryStatement::Offset(_)
            | SimpleQueryStatement::Insert(_)
            | SimpleQueryStatement::Set(_)
            | SimpleQueryStatement::Remove(_)
            | SimpleQueryStatement::Delete(_) => {}
        }
    }
}

fn collect_result_bindings(
    result: Option<&ResultStatement>,
    query_scope: &RapidHashSet<String>,
    scope: &mut RapidHashSet<String>,
) {
    let Some(result) = result else {
        return;
    };
    match result {
        ResultStatement::Return(ret) => match &ret.body {
            ReturnBody::Star => scope.extend(query_scope.iter().cloned()),
            #[cfg(feature = "cypher")]
            ReturnBody::NoBindings => {}
            ReturnBody::Items { items, .. } => {
                for item in items {
                    if let Some(alias) = &item.alias {
                        scope.insert(alias.clone());
                    } else if let ExprKind::Variable(name) = &item.expr.kind {
                        scope.insert(name.clone());
                    }
                }
            }
        },
        ResultStatement::Select(sel) => match &sel.body {
            SelectBody::Star { .. } => scope.extend(query_scope.iter().cloned()),
            SelectBody::Items { items, .. } => {
                for item in items {
                    if let Some(alias) = &item.alias {
                        scope.insert(alias.clone());
                    } else if let ExprKind::Variable(name) = &item.expr.kind {
                        scope.insert(name.clone());
                    }
                }
            }
        },
        ResultStatement::Finish => {}
    }
}

fn result_column_count(
    result: Option<&ResultStatement>,
    query_scope: &RapidHashSet<String>,
) -> usize {
    match result {
        Some(ResultStatement::Return(ret)) => match &ret.body {
            ReturnBody::Star => query_scope.len(),
            #[cfg(feature = "cypher")]
            ReturnBody::NoBindings => 0,
            ReturnBody::Items { items, .. } => items.len(),
        },
        Some(ResultStatement::Select(sel)) => match &sel.body {
            SelectBody::Star { .. } => query_scope.len(),
            SelectBody::Items { items, .. } => items.len(),
        },
        Some(ResultStatement::Finish) | None => 0,
    }
}

pub(super) fn validate_graph_reference(
    name: &ObjectName,
    value_scope: &RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    validate_catalog_object_name(name)?;
    if name.parts.len() != 1 {
        return Ok(());
    }
    let ident = &name.parts[0];
    if ident.starts_with("$$") || graph_scope.contains(ident) {
        return Ok(());
    }
    if value_scope.contains(ident) {
        return Err(verr(&format!(
            "'{ident}' is bound in value/table scope and cannot be used as a graph reference"
        )));
    }
    Ok(())
}

pub(super) fn expr_is_group_compatible(expr: &Expr, group_items: &[Expr]) -> bool {
    if group_items.iter().any(|group_expr| group_expr == expr) {
        return true;
    }
    match &expr.kind {
        ExprKind::Aggregate { .. } => true,
        ExprKind::Literal(_)
        | ExprKind::Parameter(_)
        | ExprKind::SessionUser
        | ExprKind::CurrentDate
        | ExprKind::CurrentTime
        | ExprKind::CurrentTimestamp
        | ExprKind::CurrentLocalTime
        | ExprKind::CurrentLocalTimestamp => true,
        ExprKind::Variable(_) => false,
        ExprKind::PropertyAccess { expr, .. } => expr_is_group_compatible(expr, group_items),
        ExprKind::BinaryOp { left, right, .. }
        | ExprKind::And(left, right)
        | ExprKind::Or(left, right)
        | ExprKind::Xor(left, right)
        | ExprKind::Compare { left, right, .. }
        | ExprKind::Concat(left, right)
        | ExprKind::NullIf(left, right)
        | ExprKind::Mod(left, right)
        | ExprKind::Log(left, right)
        | ExprKind::Power(left, right)
        | ExprKind::DurationBetween { left, right, .. }
        | ExprKind::Left(left, right)
        | ExprKind::Right(left, right)
        | ExprKind::TrimList {
            list: left,
            count: right,
        }
        | ExprKind::IsSourceOf {
            node: left,
            edge: right,
            ..
        }
        | ExprKind::IsDestOf {
            node: left,
            edge: right,
            ..
        } => {
            expr_is_group_compatible(left, group_items)
                && expr_is_group_compatible(right, group_items)
        }
        ExprKind::Paren(expr)
        | ExprKind::Not(expr)
        | ExprKind::UnaryOp { expr, .. }
        | ExprKind::IsNull(expr)
        | ExprKind::IsNotNull(expr)
        | ExprKind::IsNormalized { expr, .. }
        | ExprKind::IsTruth { expr, .. }
        | ExprKind::IsLabeled { expr, .. }
        | ExprKind::IsTyped { expr, .. }
        | ExprKind::IsDirected { expr, .. }
        | ExprKind::Cast { expr, .. }
        | ExprKind::Normalize { expr, .. }
        | ExprKind::Upper(expr)
        | ExprKind::Lower(expr)
        | ExprKind::CharLength { expr, .. }
        | ExprKind::ByteLength { expr, .. }
        | ExprKind::Cardinality { expr, .. }
        | ExprKind::Abs(expr)
        | ExprKind::Floor(expr)
        | ExprKind::Ceil(expr)
        | ExprKind::Sqrt(expr)
        | ExprKind::Exp(expr)
        | ExprKind::Ln(expr)
        | ExprKind::Log10(expr)
        | ExprKind::Sin(expr)
        | ExprKind::Cos(expr)
        | ExprKind::Tan(expr)
        | ExprKind::Asin(expr)
        | ExprKind::Acos(expr)
        | ExprKind::Atan(expr)
        | ExprKind::ElementId(expr)
        | ExprKind::PathLength(expr)
        | ExprKind::Elements(expr)
        | ExprKind::Degrees(expr)
        | ExprKind::Radians(expr)
        | ExprKind::Cot(expr)
        | ExprKind::Sinh(expr)
        | ExprKind::Cosh(expr)
        | ExprKind::Tanh(expr) => expr_is_group_compatible(expr, group_items),
        #[cfg(feature = "sql-compat")]
        ExprKind::Sign(expr) => expr_is_group_compatible(expr, group_items),
        #[cfg(feature = "cypher")]
        ExprKind::Nodes(expr)
        | ExprKind::Edges(expr)
        | ExprKind::Labels(expr)
        | ExprKind::Label(expr)
        | ExprKind::Source(expr)
        | ExprKind::Destination(expr) => expr_is_group_compatible(expr, group_items),
        ExprKind::Trim {
            trim_char, expr, ..
        } => {
            trim_char
                .as_ref()
                .is_none_or(|inner| expr_is_group_compatible(inner, group_items))
                && expr_is_group_compatible(expr, group_items)
        }
        #[cfg(feature = "sql-compat")]
        ExprKind::Atan2(left, right) => {
            expr_is_group_compatible(left, group_items)
                && expr_is_group_compatible(right, group_items)
        }
        #[cfg(feature = "sql-compat")]
        ExprKind::Truncate { expr, places } | ExprKind::Round { expr, places } => {
            expr_is_group_compatible(expr, group_items)
                && places
                    .as_ref()
                    .is_none_or(|inner| expr_is_group_compatible(inner, group_items))
        }
        #[cfg(feature = "sql-compat")]
        ExprKind::InList { expr, list, .. } => {
            expr_is_group_compatible(expr, group_items)
                && list
                    .iter()
                    .all(|item| expr_is_group_compatible(item, group_items))
        }
        ExprKind::StringPredicate { expr, pattern, .. } => {
            expr_is_group_compatible(expr, group_items)
                && expr_is_group_compatible(pattern, group_items)
        }
        ExprKind::FoldString { expr, chars, .. } => {
            expr_is_group_compatible(expr, group_items)
                && chars
                    .as_ref()
                    .is_none_or(|inner| expr_is_group_compatible(inner, group_items))
        }
        ExprKind::ListLiteral(items)
        | ExprKind::ListConstructor { items, .. }
        | ExprKind::Coalesce(items)
        | ExprKind::AllDifferent(items)
        | ExprKind::Same(items)
        | ExprKind::PathConstructor { elements: items }
        | ExprKind::DateLiteral(items)
        | ExprKind::DateFunction(items)
        | ExprKind::TimeLiteral(items)
        | ExprKind::DatetimeLiteral(items)
        | ExprKind::TimestampLiteral(items)
        | ExprKind::DurationLiteral(items)
        | ExprKind::ZonedTimeFunction(items)
        | ExprKind::ZonedDatetimeFunction(items)
        | ExprKind::LocalTimeFunction(items)
        | ExprKind::LocalDatetimeFunction(items)
        | ExprKind::DurationFunction(items) => items
            .iter()
            .all(|item| expr_is_group_compatible(item, group_items)),
        ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => fields
            .iter()
            .all(|(_, expr)| expr_is_group_compatible(expr, group_items)),
        #[cfg(feature = "cypher")]
        ExprKind::ListIndex { list, index } => {
            expr_is_group_compatible(list, group_items)
                && expr_is_group_compatible(index, group_items)
        }
        #[cfg(feature = "cypher")]
        ExprKind::ListSlice { list, from, to } => {
            expr_is_group_compatible(list, group_items)
                && from
                    .as_ref()
                    .is_none_or(|inner| expr_is_group_compatible(inner, group_items))
                && to
                    .as_ref()
                    .is_none_or(|inner| expr_is_group_compatible(inner, group_items))
        }
        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            expr_is_group_compatible(operand, group_items)
                && when_clauses.iter().all(|wc| {
                    expr_is_group_compatible(&wc.condition, group_items)
                        && expr_is_group_compatible(&wc.result, group_items)
                })
                && else_clause
                    .as_ref()
                    .is_none_or(|inner| expr_is_group_compatible(inner, group_items))
        }
        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            when_clauses.iter().all(|wc| {
                expr_is_group_compatible(&wc.condition, group_items)
                    && expr_is_group_compatible(&wc.result, group_items)
            }) && else_clause
                .as_ref()
                .is_none_or(|inner| expr_is_group_compatible(inner, group_items))
        }
        ExprKind::FunctionCall { args, .. } => args
            .iter()
            .all(|arg| expr_is_group_compatible(arg, group_items)),
        ExprKind::ExistsPattern(pattern) => pattern
            .where_clause
            .as_ref()
            .is_none_or(|expr| expr_is_group_compatible(expr, group_items)),
        ExprKind::ExistsSubquery(_) | ExprKind::ValueSubquery(_) => true,
        ExprKind::LetIn { bindings, expr } => {
            bindings
                .iter()
                .all(|binding| expr_is_group_compatible(&binding.value, group_items))
                && expr_is_group_compatible(expr, group_items)
        }
        ExprKind::PropertyExists { expr, .. } => expr_is_group_compatible(expr, group_items),
    }
}

pub(super) fn collect_pattern_bindings(
    pattern: &GraphPattern,
    scope: &mut RapidHashSet<String>,
) -> VResult {
    for path in &pattern.paths {
        if let Some(ref var) = path.variable {
            scope.insert(var.clone());
        }
        collect_path_expr_bindings(&path.expr, scope)?;
    }
    Ok(())
}

fn collect_path_expr_bindings(expr: &PathPatternExpr, scope: &mut RapidHashSet<String>) -> VResult {
    match expr {
        PathPatternExpr::Term(term) => collect_path_term_bindings(term, scope),
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                collect_path_term_bindings(term, scope)?;
            }
            Ok(())
        }
    }
}

fn collect_path_term_bindings(term: &PathTerm, scope: &mut RapidHashSet<String>) -> VResult {
    for factor in &term.factors {
        collect_path_primary_bindings(&factor.primary, scope)?;
        validate_path_quantifier(&factor.quantifier)?;
    }
    Ok(())
}

fn collect_path_primary_bindings(
    primary: &PathPrimary,
    scope: &mut RapidHashSet<String>,
) -> VResult {
    match primary {
        PathPrimary::Node(node) => {
            if let Some(ref var) = node.variable {
                scope.insert(var.clone());
            }
        }
        PathPrimary::Edge(edge) => {
            if let Some(ref var) = edge.variable {
                scope.insert(var.clone());
            }
        }
        PathPrimary::Parenthesized { expr, .. } => collect_path_expr_bindings(expr, scope)?,
        PathPrimary::Simplified(_) => {}
    }
    Ok(())
}

fn validate_path_quantifier(q: &Option<PathQuantifier>) -> VResult {
    if let Some(PathQuantifier::Range {
        lower,
        upper: Some(upper),
    }) = q
        && lower > upper
    {
        return Err(verr(&format!(
            "path quantifier lower bound ({lower}) exceeds upper bound ({upper})"
        )));
    }
    Ok(())
}
