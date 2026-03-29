use crate::ast::{
    CreateStmt, DeleteStmt, Expr, MatchClause, MergeStmt, NodePattern, PathLength, PatternElement,
    QueryStmt, RemoveItem, RemoveStmt, ReturnItem, SetItem, SetStmt, Statement, WhereClause,
    WithClause,
};
use gleaph_types::GleaphError;
use std::collections::BTreeSet;

/// Validates a parsed [`Statement`] against semantic rules.
///
/// Checks performed (in order):
/// 1. Feature gates — rejects constructs not supported in the current phase.
/// 2. Clause ordering — e.g. RETURN must be non-empty.
/// 3. Statement-specific rules — variable scoping, property hint literals, and
///    the requirement that DELETE always has a WHERE clause.
pub fn validate_statement(stmt: &Statement) -> Result<(), GleaphError> {
    validate_feature_gates(stmt)?;
    validate_clause_ordering(stmt)?;
    match stmt {
        Statement::Query(q) => validate_query(q),
        Statement::Compound { op, left, right } => {
            validate_statement(left)?;
            // §9.2: NEXT pipeline — right side can use variables from left's output.
            // Skip right-side variable scoping validation; it will be validated at runtime.
            if matches!(op, crate::ast::SetOp::Next(_)) {
                return Ok(());
            }
            validate_statement(right)?;
            let (lcols, rcols) = (statement_column_count(left), statement_column_count(right));
            let (Some(l), Some(r)) = (lcols, rcols) else {
                return Err(GleaphError::ValidationError(
                    "compound query branches must be read-only query statements".into(),
                ));
            };
            if l != r {
                return Err(GleaphError::ValidationError(format!(
                    "compound query column count mismatch: left={l}, right={r}"
                )));
            }
            Ok(())
        }
        Statement::Create(cs) => {
            for c in cs {
                validate_create(c)?;
            }
            Ok(())
        }
        Statement::Delete(d) => validate_delete(d),
        Statement::Set(s) => validate_set(s),
        Statement::Remove(r) => validate_remove(r),
        Statement::Finish => Ok(()),
        Statement::Filter(f) => validate_filter(f),
        Statement::Let(l) => validate_let(l),
        Statement::For(_) => Ok(()),
        Statement::Call(_) => Ok(()),
        Statement::Merge(m) => validate_merge(m),
        Statement::UseGraph(_) | Statement::CreateGraph { .. } | Statement::DropGraph { .. } => {
            Ok(())
        }
        Statement::CreateGraphType { .. }
        | Statement::DropGraphType { .. }
        | Statement::CreateSchema { .. }
        | Statement::DropSchema { .. }
        | Statement::DescribeGraphType(_)
        | Statement::CreateIndex { .. }
        | Statement::DropIndex { .. }
        | Statement::Show(_)
        | Statement::Grant { .. }
        | Statement::Revoke { .. }
        | Statement::Analyze
        | Statement::CallProcedure(_)
        | Statement::SetTypeCheck(_)
        | Statement::CreateConstraint(_)
        | Statement::DropConstraint(_) => Ok(()),
    }
}

fn validate_merge(m: &MergeStmt) -> Result<(), GleaphError> {
    // Validate ON CREATE/MATCH SET items reference sensible variable names.
    for item in m.on_create_set.iter().chain(m.on_match_set.iter()) {
        match item {
            SetItem::Property { var, .. } | SetItem::Label { var, .. } if var.is_empty() => {
                return Err(GleaphError::ValidationError(
                    "MERGE SET item requires a non-empty variable name".into(),
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Validates that all variables referenced in WHERE, RETURN, and ORDER BY are
/// bound by the MATCH clause.
fn validate_query(q: &QueryStmt) -> Result<(), GleaphError> {
    let mut bindings = collect_query_match_bindings(q)?;
    for m in query_match_clauses(q) {
        validate_path_lengths(m)?;
    }
    validate_where(&bindings, q.where_clause.as_ref())?;
    for w in &q.with_clauses {
        validate_with_clause(&bindings, w)?;
        if !w.star {
            bindings = projected_binding_names(&w.items)?;
        }
        // WITH * leaves all bindings in scope unchanged.
        if let Some(where_clause) = &w.where_clause {
            validate_where_expr(&bindings, where_clause)?;
        }
        if let Some(order_by) = &w.order_by {
            let aliases = w
                .items
                .iter()
                .filter_map(|i| i.alias.as_deref())
                .collect::<BTreeSet<_>>();
            for item in &order_by.items {
                match &item.expr {
                    Expr::Variable(v) if aliases.iter().any(|a| v.eq_ignore_ascii_case(a)) => {}
                    _ => validate_where_expr(&bindings, &item.expr)?,
                }
            }
        }
        // Validate follow-on MATCH clauses in the WITH continuation.
        for m in &w.match_clauses {
            validate_path_lengths(&m.pattern)?;
            // Collect new variable bindings from this match clause, allowing
            // variables already projected by the WITH to be used as anchors
            // (re-binding the same variable is legal in continuation context).
            let new_bindings = collect_match_bindings_allowing_reuse(&m.pattern, &bindings)?;
            for v in new_bindings {
                bindings.insert(v);
            }
            if let Some(path_var) = &m.path_variable {
                bindings.insert(path_var.clone());
            }
        }
        if let Some(post_where) = &w.post_match_where {
            validate_where_expr(&bindings, post_where)?;
        }
    }
    for item in &q.return_clause.items {
        validate_where_expr(&bindings, &item.expr)?;
    }
    if let Some(group_by) = &q.group_by {
        for expr in group_by {
            validate_where_expr(&bindings, expr)?;
        }
    }
    if let Some(having) = &q.having {
        validate_where_expr(&bindings, having)?;
    }
    if let Some(order_by) = &q.order_by {
        // ORDER BY may reference any RETURN alias as well as MATCH-bound variables.
        // Build an extended scope so that expressions like `cnt + 1` (where cnt is a
        // RETURN alias) validate correctly.
        let mut order_scope = bindings.clone();
        for item in &q.return_clause.items {
            let col = column_name_for_validate(item);
            order_scope.insert(col);
        }
        for item in &order_by.items {
            validate_where_expr(&order_scope, &item.expr)?;
        }
    }
    Ok(())
}

/// Derive the projected column name for a return item — mirrors `column_name()` in executor.
fn column_name_for_validate(item: &ReturnItem) -> String {
    if let Some(alias) = &item.alias {
        return alias.clone();
    }
    match &item.expr {
        Expr::Variable(v) => v.clone(),
        Expr::PropertyAccess { target, property } => {
            if let Expr::Variable(v) = target.as_ref() {
                format!("{v}.{property}")
            } else {
                property.clone()
            }
        }
        _ => "expr".into(),
    }
}

fn validate_with_clause(bindings: &BTreeSet<String>, w: &WithClause) -> Result<(), GleaphError> {
    for item in &w.items {
        validate_where_expr(bindings, &item.expr)?;
    }
    Ok(())
}

fn projected_binding_names(items: &[ReturnItem]) -> Result<BTreeSet<String>, GleaphError> {
    let mut out = BTreeSet::new();
    for item in items {
        let name = if let Some(alias) = &item.alias {
            alias.clone()
        } else {
            match &item.expr {
                Expr::Variable(v) => v.clone(),
                Expr::PropertyAccess { target, property } => {
                    if let Expr::Variable(v) = target.as_ref() {
                        format!("{v}.{property}")
                    } else {
                        property.clone()
                    }
                }
                _ => {
                    return Err(GleaphError::ValidationError(
                        "WITH expressions must use AS alias unless they are simple variables or property accesses".into(),
                    ))
                }
            }
        };
        out.insert(name);
    }
    Ok(out)
}

fn query_match_clauses(q: &QueryStmt) -> Vec<&MatchClause> {
    q.match_clauses.iter().map(|e| &e.pattern).collect()
}

fn collect_query_match_bindings(q: &QueryStmt) -> Result<BTreeSet<String>, GleaphError> {
    let mut all = BTreeSet::new();
    for entry in &q.match_clauses {
        let local = collect_match_bindings(&entry.pattern)?;
        for v in local {
            all.insert(v);
        }
        if let Some(path_var) = &entry.path_variable {
            all.insert(path_var.clone());
        }
    }
    Ok(all)
}

/// Validates that all property hints in a CREATE statement use literal values.
fn validate_create(c: &CreateStmt) -> Result<(), GleaphError> {
    match c {
        CreateStmt::Node(n) => validate_props_hint(&n.node),
        CreateStmt::Edge(e) => {
            validate_props_hint(&e.left)?;
            validate_props_hint(&e.right)?;
            Ok(())
        }
    }
}

/// Validates a DELETE statement:
/// - WHERE clause is mandatory (unbounded deletes are rejected).
/// - The delete target variable must be bound by the MATCH clause.
fn validate_delete(d: &DeleteStmt) -> Result<(), GleaphError> {
    let bindings = collect_match_bindings(&d.match_clause)?;
    validate_path_lengths(&d.match_clause)?;
    validate_where(&bindings, d.where_clause.as_ref())?;
    for var in &d.target_vars {
        if !bindings.contains(var) {
            return Err(GleaphError::ValidationError(format!(
                "DELETE target '{var}' is not bound by MATCH"
            )));
        }
    }
    Ok(())
}

fn validate_set(s: &SetStmt) -> Result<(), GleaphError> {
    let bindings = collect_match_bindings(&s.match_clause)?;
    validate_path_lengths(&s.match_clause)?;
    validate_where(&bindings, s.where_clause.as_ref())?;
    for item in &s.set_clause.items {
        match item {
            SetItem::Property { var, value, .. } => {
                if !bindings.contains(var) {
                    return Err(GleaphError::ValidationError(format!(
                        "SET target '{}' is not bound by MATCH",
                        var
                    )));
                }
                validate_where_expr(&bindings, value)?;
            }
            SetItem::AllProperties { var, properties } => {
                if !bindings.contains(var) {
                    return Err(GleaphError::ValidationError(format!(
                        "SET target '{}' is not bound by MATCH",
                        var
                    )));
                }
                for (_, value) in properties {
                    validate_where_expr(&bindings, value)?;
                }
            }
            SetItem::Label { var, .. } => {
                if !bindings.contains(var) {
                    return Err(GleaphError::ValidationError(format!(
                        "SET target '{}' is not bound by MATCH",
                        var
                    )));
                }
            }
        }
    }
    Ok(())
}

fn validate_remove(r: &RemoveStmt) -> Result<(), GleaphError> {
    let bindings = collect_match_bindings(&r.match_clause)?;
    validate_path_lengths(&r.match_clause)?;
    validate_where(&bindings, r.where_clause.as_ref())?;
    for item in &r.remove_clause.items {
        let var = match item {
            RemoveItem::Property { var, .. } | RemoveItem::Label { var, .. } => var,
        };
        if !bindings.contains(var) {
            return Err(GleaphError::ValidationError(format!(
                "REMOVE target '{}' is not bound by MATCH",
                var
            )));
        }
    }
    Ok(())
}

/// Collects all variable names introduced by a MATCH clause (nodes and edges),
/// returning an error on duplicate bindings.
fn collect_match_bindings(m: &MatchClause) -> Result<BTreeSet<String>, GleaphError> {
    let mut bindings = BTreeSet::new();
    bind_node(&mut bindings, &m.start)?;
    collect_element_bindings(&mut bindings, &m.elements)?;
    Ok(bindings)
}

fn collect_element_bindings(
    bindings: &mut BTreeSet<String>,
    elements: &[PatternElement],
) -> Result<(), GleaphError> {
    for elem in elements {
        match elem {
            PatternElement::Hop(chain) => {
                if !chain.edge.properties.is_empty() && chain.edge.where_clause.is_some() {
                    return Err(GleaphError::ValidationError(
                        "edge pattern cannot combine property map {…} and inline WHERE clause"
                            .into(),
                    ));
                }
                if let Some(v) = &chain.edge.var {
                    bind_var(bindings, v)?;
                }
                if let Some(w) = chain.edge.where_clause.as_deref() {
                    validate_where_expr(bindings, w)?;
                }
                bind_node(bindings, &chain.node)?;
            }
            PatternElement::SubPath {
                inner_start,
                inner_elements,
                trailing_node,
                ..
            } => {
                bind_node(bindings, inner_start)?;
                collect_element_bindings(bindings, inner_elements)?;
                if let Some(tn) = trailing_node {
                    bind_node(bindings, tn)?;
                }
            }
        }
    }
    Ok(())
}

/// Like [`collect_match_bindings`] but allows variables in `existing` to be
/// re-used as anchors (they are already bound by a preceding WITH clause).
/// Only truly new variable names are returned.
fn collect_match_bindings_allowing_reuse(
    m: &MatchClause,
    existing: &BTreeSet<String>,
) -> Result<BTreeSet<String>, GleaphError> {
    let mut new_bindings = BTreeSet::new();
    let try_bind = |new_bindings: &mut BTreeSet<String>, var: &str| -> Result<(), GleaphError> {
        if existing.contains(var) {
            // Re-use of an existing variable as an anchor — allowed.
            return Ok(());
        }
        if !new_bindings.insert(var.to_string()) {
            return Err(GleaphError::ValidationError(format!(
                "duplicate variable binding '{}'",
                var
            )));
        }
        Ok(())
    };
    if let Some(v) = &m.start.var {
        try_bind(&mut new_bindings, v)?;
    }
    // GQL §16.7: property map and inline WHERE are mutually exclusive.
    if !m.start.props_hint.is_empty() && m.start.where_clause.is_some() {
        return Err(GleaphError::ValidationError(
            "node pattern cannot combine property map {…} and inline WHERE clause".into(),
        ));
    }
    validate_props_hint(&m.start)?;
    collect_reuse_element_bindings(&mut new_bindings, existing, &m.elements)?;
    Ok(new_bindings)
}

fn collect_reuse_element_bindings(
    new_bindings: &mut BTreeSet<String>,
    existing: &BTreeSet<String>,
    elements: &[PatternElement],
) -> Result<(), GleaphError> {
    let try_bind = |new_bindings: &mut BTreeSet<String>, var: &str| -> Result<(), GleaphError> {
        if existing.contains(var) {
            return Ok(());
        }
        if !new_bindings.insert(var.to_string()) {
            return Err(GleaphError::ValidationError(format!(
                "duplicate variable binding '{var}'"
            )));
        }
        Ok(())
    };
    for elem in elements {
        match elem {
            PatternElement::Hop(chain) => {
                if !chain.edge.properties.is_empty() && chain.edge.where_clause.is_some() {
                    return Err(GleaphError::ValidationError(
                        "edge pattern cannot combine property map {…} and inline WHERE clause"
                            .into(),
                    ));
                }
                if let Some(v) = &chain.edge.var {
                    try_bind(new_bindings, v)?;
                }
                if let Some(v) = &chain.node.var {
                    try_bind(new_bindings, v)?;
                }
                if !chain.node.props_hint.is_empty() && chain.node.where_clause.is_some() {
                    return Err(GleaphError::ValidationError(
                        "node pattern cannot combine property map {…} and inline WHERE clause"
                            .into(),
                    ));
                }
                validate_props_hint(&chain.node)?;
            }
            PatternElement::SubPath {
                inner_start,
                inner_elements,
                trailing_node,
                ..
            } => {
                if let Some(v) = &inner_start.var {
                    try_bind(new_bindings, v)?;
                }
                collect_reuse_element_bindings(new_bindings, existing, inner_elements)?;
                if let Some(tn) = trailing_node
                    && let Some(v) = &tn.var
                {
                    try_bind(new_bindings, v)?;
                }
            }
        }
    }
    Ok(())
}

fn bind_node(bindings: &mut BTreeSet<String>, node: &NodePattern) -> Result<(), GleaphError> {
    if let Some(v) = &node.var {
        bind_var(bindings, v)?;
    }
    // Type annotation is mutually exclusive with labels/label_expr.
    if node.type_annotation.is_some() && (!node.labels.is_empty() || node.label_expr.is_some()) {
        return Err(GleaphError::ValidationError(
            "cannot combine labels and type annotation on the same node pattern".into(),
        ));
    }
    // GQL §16.7: property map and inline WHERE are mutually exclusive.
    if !node.props_hint.is_empty() && node.where_clause.is_some() {
        return Err(GleaphError::ValidationError(
            "node pattern cannot combine property map {…} and inline WHERE clause".into(),
        ));
    }
    validate_props_hint(node)?;
    // Validate inline WHERE predicate (variables are in scope after the node var is bound).
    if let Some(w) = node.where_clause.as_deref() {
        validate_where_expr(bindings, w)?;
    }
    Ok(())
}

fn validate_props_hint(node: &NodePattern) -> Result<(), GleaphError> {
    for (_, expr) in &node.props_hint {
        if !matches!(expr, Expr::Literal(_)) {
            return Err(GleaphError::ValidationError(
                "property hints must use literal values".into(),
            ));
        }
    }
    Ok(())
}

fn bind_var(bindings: &mut BTreeSet<String>, var: &str) -> Result<(), GleaphError> {
    if !bindings.insert(var.to_string()) {
        return Err(GleaphError::ValidationError(format!(
            "duplicate variable binding '{}'",
            var
        )));
    }
    Ok(())
}

fn validate_where(
    bindings: &BTreeSet<String>,
    where_clause: Option<&WhereClause>,
) -> Result<(), GleaphError> {
    if let Some(w) = where_clause {
        validate_where_expr(bindings, w)?;
    }
    Ok(())
}

fn validate_where_expr(bindings: &BTreeSet<String>, expr: &Expr) -> Result<(), GleaphError> {
    match expr {
        Expr::Literal(_) => Ok(()),
        Expr::Variable(v) | Expr::PathVar(v) | Expr::Parameter { name: v, .. } => {
            if bindings.contains(v) || matches!(expr, Expr::Parameter { .. }) {
                Ok(())
            } else {
                Err(GleaphError::ValidationError(format!(
                    "undefined variable '{}'",
                    v
                )))
            }
        }
        Expr::PropertyAccess { target, .. } => validate_where_expr(bindings, target),
        Expr::BinaryOp { left, right, .. }
        | Expr::Compare { left, right, .. }
        | Expr::NullIf { left, right }
        | Expr::Concat(left, right)
        | Expr::And(left, right)
        | Expr::Or(left, right)
        | Expr::Xor(left, right) => {
            validate_where_expr(bindings, left)?;
            validate_where_expr(bindings, right)
        }
        Expr::UnaryOp { expr, .. }
        | Expr::Not(expr)
        | Expr::IsNull(expr)
        | Expr::IsNotNull(expr)
        | Expr::PathLength(expr) => validate_where_expr(bindings, expr),
        Expr::InList { expr, list, .. } => {
            validate_where_expr(bindings, expr)?;
            for item in list {
                validate_where_expr(bindings, item)?;
            }
            Ok(())
        }
        Expr::StringPredicate { expr, pattern, .. } => {
            validate_where_expr(bindings, expr)?;
            validate_where_expr(bindings, pattern)
        }
        Expr::Case(case_expr) => {
            if let Some(operand) = &case_expr.operand {
                validate_where_expr(bindings, operand)?;
            }
            for wt in &case_expr.when_then {
                validate_where_expr(bindings, &wt.when)?;
                validate_where_expr(bindings, &wt.then)?;
            }
            if let Some(e) = &case_expr.else_expr {
                validate_where_expr(bindings, e)?;
            }
            Ok(())
        }
        Expr::Coalesce(items) | Expr::ListLiteral(items) => {
            for item in items {
                validate_where_expr(bindings, item)?;
            }
            Ok(())
        }
        Expr::Aggregate(agg) => {
            if let Some(e) = &agg.expr {
                validate_where_expr(bindings, e)?;
            }
            Ok(())
        }
        Expr::FunctionCall { args, .. } => {
            for arg in args {
                validate_where_expr(bindings, arg)?;
            }
            Ok(())
        }
        // §19.4: Correlated EXISTS — validate the subquery with outer bindings in scope
        // so that references to outer variables (e.g. `n` from the enclosing MATCH) are allowed.
        Expr::Exists(stmt) => match stmt.as_ref() {
            Statement::Query(q) => {
                let mut inner_bindings = collect_query_match_bindings(q)?;
                // Merge outer bindings so correlated references like n.name inside EXISTS are valid.
                for v in bindings {
                    inner_bindings.insert(v.clone());
                }
                validate_where(&inner_bindings, q.where_clause.as_ref())
            }
            _ => validate_statement(stmt),
        },
        Expr::ListIndex { list, index } => {
            validate_where_expr(bindings, list)?;
            validate_where_expr(bindings, index)
        }
        Expr::Cast { expr, .. } => validate_where_expr(bindings, expr),
        Expr::IsTruth { expr, .. } => validate_where_expr(bindings, expr),
        Expr::IsLabeled { expr, .. } => validate_where_expr(bindings, expr),
        Expr::IsSourceOf { node, edge, .. } => {
            validate_where_expr(bindings, node)?;
            validate_where_expr(bindings, edge)
        }
        Expr::IsDestOf { node, edge, .. } => {
            validate_where_expr(bindings, node)?;
            validate_where_expr(bindings, edge)
        }
        Expr::AllDifferent(exprs) | Expr::Same(exprs) => {
            for e in exprs {
                validate_where_expr(bindings, e)?;
            }
            Ok(())
        }
        Expr::PropertyExists { target, .. } => validate_where_expr(bindings, target),
        Expr::RecordLiteral(pairs) => {
            for (_, e) in pairs {
                validate_where_expr(bindings, e)?;
            }
            Ok(())
        }
        Expr::IsType { expr, .. } => validate_where_expr(bindings, expr),
        Expr::IsDirected { expr, .. } => validate_where_expr(bindings, expr),
        Expr::ValueSubquery(stmt) => validate_statement(stmt),
        Expr::LetIn {
            bindings: let_bindings,
            body,
        } => {
            let mut local = bindings.clone();
            for (name, e) in let_bindings {
                validate_where_expr(&local, e)?;
                local.insert(name.clone());
            }
            validate_where_expr(&local, body)
        }
        Expr::PathConstructor(elems) => {
            for e in elems {
                validate_where_expr(bindings, e)?;
            }
            Ok(())
        }
    }
}

/// Checks structural invariants that are independent of variable scoping, such
/// as RETURN containing at least one item and DELETE having a non-empty target.
fn validate_clause_ordering(stmt: &Statement) -> Result<(), GleaphError> {
    match stmt {
        Statement::Query(q) => {
            if q.return_clause.items.is_empty()
                && !q.return_clause.star
                && !q.return_clause.no_bindings
                && !q.return_clause.finish
            {
                return Err(GleaphError::ValidationError(
                    "RETURN clause must contain at least one item".into(),
                ));
            }
            Ok(())
        }
        Statement::Compound { op, left, right } => {
            validate_clause_ordering(left)?;
            if !matches!(op, crate::ast::SetOp::Next(_)) {
                validate_clause_ordering(right)?;
            }
            Ok(())
        }
        Statement::Delete(d) => {
            if d.target_vars.is_empty() {
                return Err(GleaphError::ValidationError(
                    "DELETE target variable is required".into(),
                ));
            }
            Ok(())
        }
        Statement::Set(s) => {
            if s.set_clause.items.is_empty() {
                return Err(GleaphError::ValidationError(
                    "SET clause must contain at least one item".into(),
                ));
            }
            Ok(())
        }
        Statement::Remove(r) => {
            if r.remove_clause.items.is_empty() {
                return Err(GleaphError::ValidationError(
                    "REMOVE clause must contain at least one item".into(),
                ));
            }
            Ok(())
        }
        Statement::Create(_) => Ok(()),
        Statement::Merge(_) => Ok(()),
        Statement::Finish => Ok(()),
        Statement::Filter(_) => Ok(()),
        Statement::Let(l) => {
            if l.bindings.is_empty() {
                return Err(GleaphError::ValidationError(
                    "LET clause must contain at least one binding".into(),
                ));
            }
            Ok(())
        }
        Statement::For(_) => Ok(()),
        Statement::Call(_) => Ok(()),
        Statement::UseGraph(_) | Statement::CreateGraph { .. } | Statement::DropGraph { .. } => {
            Ok(())
        }
        Statement::CreateGraphType { .. }
        | Statement::DropGraphType { .. }
        | Statement::CreateSchema { .. }
        | Statement::DropSchema { .. }
        | Statement::DescribeGraphType(_)
        | Statement::CreateIndex { .. }
        | Statement::DropIndex { .. }
        | Statement::Show(_)
        | Statement::Grant { .. }
        | Statement::Revoke { .. }
        | Statement::Analyze
        | Statement::CallProcedure(_)
        | Statement::SetTypeCheck(_)
        | Statement::CreateConstraint(_)
        | Statement::DropConstraint(_) => Ok(()),
    }
}

fn validate_path_lengths(m: &MatchClause) -> Result<(), GleaphError> {
    for chain in m.hops() {
        match chain.edge.length {
            PathLength::Fixed(n) if (1..=10).contains(&n) => {}
            PathLength::Range { min, max } if min >= 1 && min <= max && max <= 10 => {}
            _ => {
                return Err(GleaphError::ValidationError(
                    "variable-length path bounds must satisfy 1 <= min <= max <= 10".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Rejects statements that use features gated to a later phase, such as MATCH
/// patterns with more than 3 hops.
fn validate_feature_gates(stmt: &Statement) -> Result<(), GleaphError> {
    match stmt {
        Statement::Query(_q) => {
            // Query MATCH patterns allow any number of hops (including 0 for bare node scans).
            Ok(())
        }
        Statement::Compound { left, right, .. } => {
            validate_feature_gates(left)?;
            validate_feature_gates(right)
        }
        Statement::Delete(d) => {
            if d.match_clause.elements.is_empty() {
                return Err(GleaphError::UnsupportedFeature(
                    "DELETE MATCH requires at least 1 hop".into(),
                ));
            }
            Ok(())
        }
        Statement::Create(_) => Ok(()),
        Statement::Merge(_) => Ok(()),
        Statement::Set(s) => {
            if s.match_clause.elements.is_empty() {
                return Err(GleaphError::UnsupportedFeature(
                    "SET MATCH requires at least 1 hop".into(),
                ));
            }
            Ok(())
        }
        Statement::Remove(r) => {
            if r.match_clause.elements.is_empty() {
                return Err(GleaphError::UnsupportedFeature(
                    "REMOVE MATCH requires at least 1 hop".into(),
                ));
            }
            Ok(())
        }
        Statement::Finish => Ok(()),
        Statement::Filter(_f) => Ok(()),
        Statement::Let(_l) => Ok(()),
        Statement::For(_) => Ok(()),
        Statement::Call(_) => Ok(()),
        Statement::UseGraph(_) | Statement::CreateGraph { .. } | Statement::DropGraph { .. } => {
            Ok(())
        }
        Statement::CreateGraphType { .. }
        | Statement::DropGraphType { .. }
        | Statement::CreateSchema { .. }
        | Statement::DropSchema { .. }
        | Statement::DescribeGraphType(_)
        | Statement::CreateIndex { .. }
        | Statement::DropIndex { .. }
        | Statement::Show(_)
        | Statement::Grant { .. }
        | Statement::Revoke { .. }
        | Statement::Analyze
        | Statement::CallProcedure(_)
        | Statement::SetTypeCheck(_)
        | Statement::CreateConstraint(_)
        | Statement::DropConstraint(_) => Ok(()),
    }
}

fn statement_column_count(stmt: &Statement) -> Option<usize> {
    match stmt {
        Statement::Query(q) => {
            if q.return_clause.star {
                None
            } else {
                Some(q.return_clause.items.len())
            }
        }
        Statement::Compound { left, .. } => statement_column_count(left),
        Statement::Finish => Some(0),
        Statement::Filter(_f) => None,
        Statement::Let(l) => Some(l.return_clause.items.len()),
        _ => None,
    }
}

fn validate_filter(f: &crate::ast::FilterStmt) -> Result<(), GleaphError> {
    let bindings = collect_match_bindings(&f.match_clause)?;
    validate_path_lengths(&f.match_clause)?;
    validate_where(&bindings, f.where_clause.as_ref())?;
    validate_where_expr(&bindings, &f.filter_expr)?;
    Ok(())
}

fn validate_let(l: &crate::ast::LetStmt) -> Result<(), GleaphError> {
    let mut bindings = collect_match_bindings(&l.match_clause)?;
    validate_path_lengths(&l.match_clause)?;
    validate_where(&bindings, l.where_clause.as_ref())?;
    for (var, expr) in &l.bindings {
        validate_where_expr(&bindings, expr)?;
        bindings.insert(var.clone());
    }
    for item in &l.return_clause.items {
        validate_where_expr(&bindings, &item.expr)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ast::{
        CmpOp, DeleteStmt, Direction, EdgePattern, MatchChain, MatchClause, NodePattern,
        PathLength, PatternElement, Statement,
    };
    use crate::parser::parse_statement;

    #[test]
    fn validates_bound_variables() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) WHERE a.id = 1 RETURN b").unwrap();
        validate_statement(&stmt).unwrap();
    }

    #[test]
    fn rejects_undefined_variable_in_return() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) RETURN c").unwrap();
        let err = validate_statement(&stmt).unwrap_err();
        assert!(matches!(err, GleaphError::ValidationError(_)));
    }

    #[test]
    fn rejects_compound_column_mismatch() {
        let stmt = parse_statement(
            "MATCH (a)-[:X]->(b) RETURN a \
             UNION \
             MATCH (a)-[:X]->(b) RETURN a, b",
        )
        .unwrap();
        let err = validate_statement(&stmt).unwrap_err();
        assert!(err.to_string().contains("column count mismatch"));
    }

    #[test]
    fn with_requires_alias_for_non_variable_expr() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) WITH a.id + 1 RETURN a").unwrap();
        let err = validate_statement(&stmt).unwrap_err();
        assert!(
            err.to_string()
                .contains("WITH expressions must use AS alias")
        );
    }

    #[test]
    fn rejects_delete_target_not_in_match() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) DELETE c").unwrap();
        let err = validate_statement(&stmt).unwrap_err();
        assert!(matches!(err, GleaphError::ValidationError(_)));
    }

    #[test]
    fn allows_delete_without_where() {
        // DELETE without WHERE is now allowed
        let stmt = parse_statement("MATCH (a)-[:X]->(b) DELETE b").unwrap();
        validate_statement(&stmt).unwrap();
    }

    #[test]
    fn rejects_set_target_not_in_match() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) SET c.age = 1").unwrap();
        let err = validate_statement(&stmt).unwrap_err();
        assert!(matches!(err, GleaphError::ValidationError(_)));
        assert!(err.to_string().contains("SET target"));
    }

    #[test]
    fn rejects_remove_target_not_in_match() {
        let stmt = parse_statement("MATCH (a)-[:X]->(b) REMOVE c.age").unwrap();
        let err = validate_statement(&stmt).unwrap_err();
        assert!(matches!(err, GleaphError::ValidationError(_)));
        assert!(err.to_string().contains("REMOVE target"));
    }

    #[test]
    fn feature_gate_allows_more_than_three_hops() {
        // Now any number of hops is supported for DELETE
        let stmt = Statement::Delete(DeleteStmt {
            match_clause: MatchClause {
                start: NodePattern {
                    var: Some("a".into()),
                    labels: vec![],
                    props_hint: vec![],
                    label_expr: None,
                    where_clause: None,
                    type_annotation: None,
                },
                elements: vec![
                    PatternElement::Hop(mk_chain("b")),
                    PatternElement::Hop(mk_chain("c")),
                    PatternElement::Hop(mk_chain("d")),
                    PatternElement::Hop(mk_chain("e")),
                ],
            },
            where_clause: Some(Expr::Compare {
                left: Box::new(Expr::PropertyAccess {
                    target: Box::new(Expr::Variable("a".into())),
                    property: "id".into(),
                }),
                op: CmpOp::Eq,
                right: Box::new(Expr::Literal(gleaph_types::Value::Int64(1))),
            }),
            detach: false,
            nodetach: false,
            target_vars: vec!["b".into()],
        });
        // 4 hops is now allowed
        validate_statement(&stmt).unwrap();
    }

    fn mk_chain(var: &str) -> MatchChain {
        MatchChain {
            edge: EdgePattern {
                var: None,
                label: Some("X".into()),
                label_expr: None,
                direction: Direction::Outgoing,
                length: PathLength::Fixed(1),
                properties: vec![],
                where_clause: None,
                type_annotation: None,
            },
            node: NodePattern {
                var: Some(var.into()),
                labels: vec![],
                props_hint: vec![],
                label_expr: None,
                where_clause: None,
                type_annotation: None,
            },
        }
    }
}
