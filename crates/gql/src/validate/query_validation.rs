use crate::ast::*;
use crate::error::GqlError;
use rapidhash::RapidHashSet;

use super::dml::{validate_delete_vars, validate_insert, validate_remove_vars, validate_set_vars};
use super::expr::{validate_expr, validate_let};
use super::{
    VResult, validate_call_procedure, validate_catalog_object_name, validate_inline_scope_vars,
    validate_yield_alias_uniqueness, verr,
};

pub(super) fn validate_composite_query(
    cq: &CompositeQueryExpr,
    outer: &RapidHashSet<String>,
    outer_graph: &RapidHashSet<String>,
) -> VResult {
    validate_linear_query(&cq.left, outer, outer_graph)?;
    let expected_bindings = linear_query_result_layout(&cq.left, outer, outer_graph)?;
    for (_, rhs) in &cq.rest {
        validate_linear_query(rhs, outer, outer_graph)?;
        let rhs_bindings = linear_query_result_layout(rhs, outer, outer_graph)?;
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
                    graph_scope = project_graph_yield_items(yields, &graph_scope);
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
            SimpleQueryStatement::Search(s) => {
                if !scope.contains(&s.binding) {
                    return Err(verr(&format!(
                        "SEARCH binding variable '{}' is not in scope",
                        s.binding
                    )));
                }
                validate_expr(s.provider.query(), &scope, &graph_scope)?;
                validate_expr(s.provider.limit(), &scope, &graph_scope)?;
                if let Some(filter) = s.provider.filter() {
                    validate_expr(filter, &scope, &graph_scope)?;
                }
                scope.insert(s.output.alias.clone());
            }
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
                let outer_graph_scope = graph_scope.clone();
                collect_inline_procedure_result_bindings(
                    ipc,
                    &outer_scope,
                    &outer_graph_scope,
                    &mut scope,
                    &mut graph_scope,
                )?;
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
                                graph_scope = project_graph_yield_items(yields, &graph_scope);
                            } else {
                                scope = match_scope;
                            }
                        }
                        SimpleQueryStatement::CallProcedure(cp) => {
                            validate_call_procedure(cp)?;
                            for arg in &cp.args {
                                validate_expr(arg, &scope, &graph_scope)?;
                            }
                            if let Some(yields) = &cp.yield_items {
                                for yi in yields {
                                    let name = yi.alias.as_ref().unwrap_or(&yi.name);
                                    scope.insert(name.clone());
                                }
                            }
                        }
                        SimpleQueryStatement::Search(s) => {
                            if !scope.contains(&s.binding) {
                                return Err(verr(&format!(
                                    "SEARCH binding variable '{}' is not in scope",
                                    s.binding
                                )));
                            }
                            validate_expr(s.provider.query(), &scope, &graph_scope)?;
                            validate_expr(s.provider.limit(), &scope, &graph_scope)?;
                            if let Some(filter) = s.provider.filter() {
                                validate_expr(filter, &scope, &graph_scope)?;
                            }
                            scope.insert(s.output.alias.clone());
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

fn project_graph_yield_items(
    yields: &[YieldItem],
    visible_graph_scope: &RapidHashSet<String>,
) -> RapidHashSet<String> {
    let mut projected = RapidHashSet::default();
    for item in yields {
        if visible_graph_scope.contains(&item.name) {
            projected.insert(item.alias.clone().unwrap_or_else(|| item.name.clone()));
        }
    }
    projected
}

fn validate_inline_procedure_call(
    ipc: &InlineProcedureCall,
    scope: &RapidHashSet<String>,
    graph_scope: &RapidHashSet<String>,
) -> VResult {
    let body_scope = inline_procedure_body_scope(ipc, scope)?;
    let body_graph_scope = inline_procedure_body_graph_scope(ipc, graph_scope);
    validate_composite_query(&ipc.body, &body_scope, &body_graph_scope)
}

fn inline_procedure_body_scope(
    ipc: &InlineProcedureCall,
    scope: &RapidHashSet<String>,
) -> Result<RapidHashSet<String>, GqlError> {
    match &ipc.scope {
        InlineProcedureScope::ImplicitAll => Ok(scope.clone()),
        InlineProcedureScope::Explicit(vars) => {
            let mut selected = RapidHashSet::default();
            for name in vars {
                if !scope.contains(name) {
                    return Err(verr(&format!(
                        "inline procedure scope variable '{name}' is not in scope"
                    )));
                }
                selected.insert(name.clone());
            }
            Ok(selected)
        }
    }
}

fn inline_procedure_body_graph_scope(
    ipc: &InlineProcedureCall,
    graph_scope: &RapidHashSet<String>,
) -> RapidHashSet<String> {
    match &ipc.scope {
        InlineProcedureScope::ImplicitAll => graph_scope.clone(),
        InlineProcedureScope::Explicit(vars) => vars
            .iter()
            .filter(|name| graph_scope.contains(*name))
            .cloned()
            .collect(),
    }
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
    outer_graph_scope: &RapidHashSet<String>,
    scope: &mut RapidHashSet<String>,
    graph_scope: &mut RapidHashSet<String>,
) -> VResult {
    let body_scope = inline_procedure_body_scope(ipc, outer_scope)?;
    let body_graph_scope = inline_procedure_body_graph_scope(ipc, outer_graph_scope);
    let (bindings, graph_bindings) =
        composite_query_result_scopes(&ipc.body, &body_scope, &body_graph_scope)?;
    scope.extend(bindings);
    graph_scope.extend(graph_bindings);
    Ok(())
}

pub(super) fn composite_query_result_bindings(
    query: &CompositeQueryExpr,
    outer: &RapidHashSet<String>,
) -> Result<RapidHashSet<String>, GqlError> {
    let expected = linear_query_result_bindings(&query.left, outer, &RapidHashSet::default())?;
    for (_, rhs) in &query.rest {
        let rhs_bindings = linear_query_result_bindings(rhs, outer, &RapidHashSet::default())?;
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
    outer_graph: &RapidHashSet<String>,
) -> Result<Vec<Option<String>>, GqlError> {
    let mut query_scope = outer.clone();
    let mut query_graph_scope = outer_graph.clone();
    let mut query_order = sorted_scope_names(outer);
    collect_linear_query_scopes_with_order(
        query,
        &mut query_scope,
        &mut query_graph_scope,
        &mut query_order,
    )?;
    Ok(result_column_layout(query.result.as_ref(), &query_order))
}

fn linear_query_result_bindings(
    query: &LinearQueryStatement,
    outer: &RapidHashSet<String>,
    outer_graph: &RapidHashSet<String>,
) -> Result<RapidHashSet<String>, GqlError> {
    let mut query_scope = outer.clone();
    let mut query_graph_scope = outer_graph.clone();
    collect_linear_query_scopes(query, &mut query_scope, &mut query_graph_scope)?;
    let mut result_scope = RapidHashSet::default();
    collect_result_bindings(query.result.as_ref(), &query_scope, &mut result_scope);
    Ok(result_scope)
}

pub(super) fn composite_query_result_scopes(
    query: &CompositeQueryExpr,
    outer: &RapidHashSet<String>,
    outer_graph: &RapidHashSet<String>,
) -> Result<(RapidHashSet<String>, RapidHashSet<String>), GqlError> {
    let expected = linear_query_result_scopes(&query.left, outer, outer_graph)?;
    for (_, rhs) in &query.rest {
        let rhs_scopes = linear_query_result_scopes(rhs, outer, outer_graph)?;
        if rhs_scopes != expected {
            return Err(verr(
                "composite query branches must expose the same result bindings",
            ));
        }
    }
    Ok(expected)
}

fn linear_query_result_scopes(
    query: &LinearQueryStatement,
    outer: &RapidHashSet<String>,
    outer_graph: &RapidHashSet<String>,
) -> Result<(RapidHashSet<String>, RapidHashSet<String>), GqlError> {
    let mut query_scope = outer.clone();
    let mut query_graph_scope = outer_graph.clone();
    collect_linear_query_scopes(query, &mut query_scope, &mut query_graph_scope)?;
    let mut result_scope = RapidHashSet::default();
    collect_result_bindings(query.result.as_ref(), &query_scope, &mut result_scope);
    let mut result_graph_scope = RapidHashSet::default();
    collect_result_graph_bindings(
        query.result.as_ref(),
        &query_scope,
        &query_graph_scope,
        &mut result_graph_scope,
    );
    Ok((result_scope, result_graph_scope))
}

fn collect_linear_query_scopes(
    query: &LinearQueryStatement,
    scope: &mut RapidHashSet<String>,
    graph_scope: &mut RapidHashSet<String>,
) -> VResult {
    for binding in &query.prefix_bindings {
        scope.insert(binding.variable.clone());
        if matches!(binding.kind, ProcedureBindingKind::Graph) {
            graph_scope.insert(binding.variable.clone());
        }
    }
    for part in &query.parts {
        match part {
            SimpleQueryStatement::Match(m) => {
                let mut match_scope = scope.clone();
                let _ = collect_pattern_bindings(&m.pattern, &mut match_scope);
                if let Some(yields) = &m.yield_items {
                    *scope = project_yield_items(yields);
                    *graph_scope = project_graph_yield_items(yields, graph_scope);
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
            SimpleQueryStatement::Search(s) => {
                scope.insert(s.output.alias.clone());
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
                let outer_graph_scope = graph_scope.clone();
                collect_inline_procedure_result_bindings(
                    ipc,
                    &outer_scope,
                    &outer_graph_scope,
                    scope,
                    graph_scope,
                )?;
            }
            SimpleQueryStatement::Focused { body, .. } => {
                if let Some(inner) = body {
                    match inner.as_ref() {
                        SimpleQueryStatement::Match(m) => {
                            let mut match_scope = scope.clone();
                            let _ = collect_pattern_bindings(&m.pattern, &mut match_scope);
                            if let Some(yields) = &m.yield_items {
                                *scope = project_yield_items(yields);
                                *graph_scope = project_graph_yield_items(yields, graph_scope);
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
                        SimpleQueryStatement::Search(s) => {
                            scope.insert(s.output.alias.clone());
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
    Ok(())
}

fn collect_linear_query_scopes_with_order(
    query: &LinearQueryStatement,
    scope: &mut RapidHashSet<String>,
    graph_scope: &mut RapidHashSet<String>,
    order: &mut Vec<String>,
) -> VResult {
    for binding in &query.prefix_bindings {
        replace_visible_binding(scope, order, &binding.variable);
        if matches!(binding.kind, ProcedureBindingKind::Graph) {
            graph_scope.insert(binding.variable.clone());
        }
    }
    for part in &query.parts {
        match part {
            SimpleQueryStatement::Match(m) => {
                let mut match_scope = scope.clone();
                let mut match_order = order.clone();
                collect_pattern_bindings_with_order(
                    &m.pattern,
                    &mut match_scope,
                    &mut match_order,
                )?;
                if let Some(yields) = &m.yield_items {
                    *scope = project_yield_items(yields);
                    *graph_scope = project_graph_yield_items(yields, graph_scope);
                    *order = projected_yield_order(yields);
                } else {
                    *scope = match_scope;
                    *order = match_order;
                }
            }
            SimpleQueryStatement::Let(l) => {
                for binding in &l.bindings {
                    replace_visible_binding(scope, order, &binding.variable);
                }
            }
            SimpleQueryStatement::For(f) => {
                replace_visible_binding(scope, order, &f.variable);
                if let Some(ord) = &f.ordinality {
                    replace_visible_binding(scope, order, &ord.variable);
                }
            }
            SimpleQueryStatement::Search(s) => {
                replace_visible_binding(scope, order, &s.output.alias);
            }
            SimpleQueryStatement::CallProcedure(cp) => {
                if let Some(yields) = &cp.yield_items {
                    for item in yields {
                        replace_visible_binding(
                            scope,
                            order,
                            item.alias.as_ref().unwrap_or(&item.name),
                        );
                    }
                }
            }
            SimpleQueryStatement::InlineProcedureCall(ipc) => {
                let outer_scope = scope.clone();
                let outer_graph_scope = graph_scope.clone();
                let body_scope = inline_procedure_body_scope(ipc, &outer_scope)?;
                let body_graph_scope = inline_procedure_body_graph_scope(ipc, &outer_graph_scope);
                let layout =
                    composite_query_result_layout(&ipc.body, &body_scope, &body_graph_scope)?;
                collect_inline_procedure_result_bindings(
                    ipc,
                    &outer_scope,
                    &outer_graph_scope,
                    scope,
                    graph_scope,
                )?;
                for name in layout.into_iter().flatten() {
                    replace_visible_binding(scope, order, &name);
                }
            }
            SimpleQueryStatement::Focused { body, .. } => {
                if let Some(inner) = body {
                    match inner.as_ref() {
                        SimpleQueryStatement::Match(m) => {
                            let mut match_scope = scope.clone();
                            let mut match_order = order.clone();
                            collect_pattern_bindings_with_order(
                                &m.pattern,
                                &mut match_scope,
                                &mut match_order,
                            )?;
                            if let Some(yields) = &m.yield_items {
                                *scope = project_yield_items(yields);
                                *graph_scope = project_graph_yield_items(yields, graph_scope);
                                *order = projected_yield_order(yields);
                            } else {
                                *scope = match_scope;
                                *order = match_order;
                            }
                        }
                        SimpleQueryStatement::CallProcedure(cp) => {
                            if let Some(yields) = &cp.yield_items {
                                for item in yields {
                                    replace_visible_binding(
                                        scope,
                                        order,
                                        item.alias.as_ref().unwrap_or(&item.name),
                                    );
                                }
                            }
                        }
                        SimpleQueryStatement::Search(s) => {
                            replace_visible_binding(scope, order, &s.output.alias);
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
    Ok(())
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

fn collect_result_graph_bindings(
    result: Option<&ResultStatement>,
    query_scope: &RapidHashSet<String>,
    query_graph_scope: &RapidHashSet<String>,
    scope: &mut RapidHashSet<String>,
) {
    let Some(result) = result else {
        return;
    };
    match result {
        ResultStatement::Return(ret) => match &ret.body {
            ReturnBody::Star => {
                for name in query_scope {
                    if query_graph_scope.contains(name) {
                        scope.insert(name.clone());
                    }
                }
            }
            #[cfg(feature = "cypher")]
            ReturnBody::NoBindings => {}
            ReturnBody::Items { items, .. } => {
                for item in items {
                    if let ExprKind::Variable(name) = &item.expr.kind
                        && query_graph_scope.contains(name)
                    {
                        scope.insert(item.alias.clone().unwrap_or_else(|| name.clone()));
                    }
                }
            }
        },
        ResultStatement::Select(sel) => match &sel.body {
            SelectBody::Star { .. } => {
                for name in query_scope {
                    if query_graph_scope.contains(name) {
                        scope.insert(name.clone());
                    }
                }
            }
            SelectBody::Items { items, .. } => {
                for item in items {
                    if let ExprKind::Variable(name) = &item.expr.kind
                        && query_graph_scope.contains(name)
                    {
                        scope.insert(item.alias.clone().unwrap_or_else(|| name.clone()));
                    }
                }
            }
        },
        ResultStatement::Finish => {}
    }
}

fn result_column_layout(
    result: Option<&ResultStatement>,
    query_order: &[String],
) -> Vec<Option<String>> {
    match result {
        Some(ResultStatement::Return(ret)) => match &ret.body {
            ReturnBody::Star => query_order.iter().map(|name| Some(name.clone())).collect(),
            #[cfg(feature = "cypher")]
            ReturnBody::NoBindings => vec![],
            ReturnBody::Items { items, .. } => items
                .iter()
                .map(|item| {
                    item.alias.clone().or_else(|| match &item.expr.kind {
                        ExprKind::Variable(name) => Some(name.clone()),
                        _ => None,
                    })
                })
                .collect(),
        },
        Some(ResultStatement::Select(sel)) => match &sel.body {
            SelectBody::Star { .. } => query_order.iter().map(|name| Some(name.clone())).collect(),
            SelectBody::Items { items, .. } => items
                .iter()
                .map(|item| {
                    item.alias.clone().or_else(|| match &item.expr.kind {
                        ExprKind::Variable(name) => Some(name.clone()),
                        _ => None,
                    })
                })
                .collect(),
        },
        Some(ResultStatement::Finish) | None => vec![],
    }
}

fn composite_query_result_layout(
    query: &CompositeQueryExpr,
    outer: &RapidHashSet<String>,
    outer_graph: &RapidHashSet<String>,
) -> Result<Vec<Option<String>>, GqlError> {
    let expected = linear_query_result_layout(&query.left, outer, outer_graph)?;
    for (_, rhs) in &query.rest {
        let rhs_layout = linear_query_result_layout(rhs, outer, outer_graph)?;
        if rhs_layout != expected {
            return Err(verr(
                "composite query branches must expose the same result bindings",
            ));
        }
    }
    Ok(expected)
}

fn sorted_scope_names(scope: &RapidHashSet<String>) -> Vec<String> {
    let mut names: Vec<_> = scope.iter().cloned().collect();
    names.sort();
    names
}

fn replace_visible_binding(scope: &mut RapidHashSet<String>, order: &mut Vec<String>, name: &str) {
    scope.insert(name.to_owned());
    order.retain(|existing| existing != name);
    order.push(name.to_owned());
}

fn push_pattern_binding(scope: &mut RapidHashSet<String>, order: &mut Vec<String>, name: &str) {
    if scope.insert(name.to_owned()) {
        order.push(name.to_owned());
    }
}

fn projected_yield_order(yields: &[YieldItem]) -> Vec<String> {
    yields
        .iter()
        .map(|item| item.alias.clone().unwrap_or_else(|| item.name.clone()))
        .collect()
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
    if super::session_graph::ingress_seed_active()
        && let Some(seed) = super::session_graph::active_seed()
    {
        match ident.as_str() {
            "CURRENT_GRAPH" if seed.current_graph.is_none() => {
                return Err(verr("CURRENT_GRAPH is unset in this program"));
            }
            "HOME_GRAPH" if seed.home_graph.is_none() => {
                return Err(verr("HOME_GRAPH is unset in this program"));
            }
            _ => {}
        }
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

fn collect_pattern_bindings_with_order(
    pattern: &GraphPattern,
    scope: &mut RapidHashSet<String>,
    order: &mut Vec<String>,
) -> VResult {
    for path in &pattern.paths {
        if let Some(ref var) = path.variable {
            push_pattern_binding(scope, order, var);
        }
        collect_path_expr_bindings_with_order(&path.expr, scope, order)?;
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

fn collect_path_expr_bindings_with_order(
    expr: &PathPatternExpr,
    scope: &mut RapidHashSet<String>,
    order: &mut Vec<String>,
) -> VResult {
    match expr {
        PathPatternExpr::Term(term) => collect_path_term_bindings_with_order(term, scope, order),
        PathPatternExpr::MultisetAlternation(terms) | PathPatternExpr::PatternUnion(terms) => {
            for term in terms {
                collect_path_term_bindings_with_order(term, scope, order)?;
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

fn collect_path_term_bindings_with_order(
    term: &PathTerm,
    scope: &mut RapidHashSet<String>,
    order: &mut Vec<String>,
) -> VResult {
    for factor in &term.factors {
        collect_path_primary_bindings_with_order(&factor.primary, scope, order)?;
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

fn collect_path_primary_bindings_with_order(
    primary: &PathPrimary,
    scope: &mut RapidHashSet<String>,
    order: &mut Vec<String>,
) -> VResult {
    match primary {
        PathPrimary::Node(node) => {
            if let Some(ref var) = node.variable {
                push_pattern_binding(scope, order, var);
            }
        }
        PathPrimary::Edge(edge) => {
            if let Some(ref var) = edge.variable {
                push_pattern_binding(scope, order, var);
            }
        }
        PathPrimary::Parenthesized { expr, .. } => {
            collect_path_expr_bindings_with_order(expr, scope, order)?
        }
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
