use super::*;

pub(super) fn materialize_output_rows(
    rows: Vec<OutputRow>,
    columns: &[ProjectColumn],
) -> Vec<BindingRow> {
    rows.into_iter()
        .map(|row| {
            let mut out = BindingRow::new();
            if columns.is_empty() {
                for (key, value) in row {
                    out.insert(Rc::<str>::from(key), BindingValue::Scalar(value));
                }
            } else {
                for (index, column) in columns.iter().enumerate() {
                    let key = materialize_column_name(column, index);
                    let value = row.get(&key).cloned().unwrap_or(Value::Null);
                    out.insert(Rc::<str>::from(key), BindingValue::Scalar(value));
                }
            }
            out
        })
        .collect()
}

pub(super) fn apply_limit(
    rows: &mut Vec<OutputRow>,
    count: Option<&Expr>,
    offset: Option<&Expr>,
) -> ExecutionResultExt<()> {
    let offset = eval_usize_expr(offset)?.unwrap_or(0);
    let count = eval_usize_expr(count)?;
    let truncated: Vec<_> = rows
        .iter()
        .skip(offset)
        .take(count.unwrap_or(usize::MAX))
        .cloned()
        .collect();
    *rows = truncated;
    Ok(())
}

pub(super) fn apply_limit_bindings(
    rows: Vec<BindingRow>,
    count: Option<&Expr>,
    offset: Option<&Expr>,
) -> ExecutionResultExt<Vec<BindingRow>> {
    let offset = eval_usize_expr(offset)?.unwrap_or(0);
    let count = eval_usize_expr(count)?;
    Ok(rows
        .into_iter()
        .skip(offset)
        .take(count.unwrap_or(usize::MAX))
        .collect())
}

pub(super) fn dedup_output_rows(rows: &mut Vec<OutputRow>) {
    let mut deduped = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if !deduped.contains(&row) {
            deduped.push(row);
        }
    }
    *rows = deduped;
}

pub(super) fn dedup_output_rows_owned(rows: Vec<OutputRow>) -> Vec<OutputRow> {
    let mut rows = rows;
    dedup_output_rows(&mut rows);
    rows
}

pub(super) fn dedup_binding_rows(rows: &mut Vec<BindingRow>) {
    let mut deduped = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if !deduped.contains(&row) {
            deduped.push(row);
        }
    }
    *rows = deduped;
}

pub(super) fn normalize_to_output_rows<G: GraphRead>(
    graph: &G,
    rows: Vec<BindingRow>,
    projected: Option<Vec<OutputRow>>,
) -> ExecutionResultExt<Vec<OutputRow>> {
    match projected {
        Some(rows) => Ok(rows),
        None => rows
            .into_iter()
            .map(|row| materialize_row(graph, &row))
            .collect(),
    }
}

pub(super) fn join_key_values(
    row: &BindingRow,
    join_keys: &[Rc<str>],
) -> ExecutionResultExt<Vec<Value>> {
    join_keys
        .iter()
        .map(|key| match row.get(key.as_ref()) {
            Some(BindingValue::Scalar(value)) => Ok(value.clone()),
            Some(BindingValue::Node(node)) => Ok(node_to_value(node)),
            Some(BindingValue::Edge(edge)) => Ok(edge_to_value(edge)),
            None => Err(ExecutionError::MissingBinding(key.to_string())),
        })
        .collect()
}

pub(super) fn merge_rows(left: &BindingRow, right: &BindingRow) -> ExecutionResultExt<BindingRow> {
    let mut merged = left.clone();
    for (key, value) in right {
        if let Some(existing) = merged.get(key.as_ref()) {
            if existing != value {
                return Err(ExecutionError::TypeMismatch("conflicting join bindings"));
            }
            continue;
        }
        merged.insert(key.clone(), value.clone());
    }
    Ok(merged)
}

pub(super) fn subtract_rows(
    left: Vec<OutputRow>,
    right: Vec<OutputRow>,
    distinct: bool,
) -> Vec<OutputRow> {
    let mut out = if distinct {
        dedup_output_rows_owned(left)
    } else {
        left
    };
    let rhs = if distinct {
        dedup_output_rows_owned(right)
    } else {
        right
    };
    for row in rhs {
        if let Some(pos) = out.iter().position(|candidate| *candidate == row) {
            out.remove(pos);
        }
    }
    out
}

pub(super) fn intersect_rows(
    left: Vec<OutputRow>,
    right: Vec<OutputRow>,
    distinct: bool,
) -> Vec<OutputRow> {
    let mut lhs = if distinct {
        dedup_output_rows_owned(left)
    } else {
        left
    };
    let mut rhs = if distinct {
        dedup_output_rows_owned(right)
    } else {
        right
    };
    let mut out = Vec::new();

    while let Some(row) = lhs.pop() {
        if let Some(pos) = rhs.iter().position(|candidate| *candidate == row) {
            rhs.remove(pos);
            out.push(row);
        }
    }

    if distinct {
        dedup_output_rows_owned(out)
    } else {
        out
    }
}

pub(super) fn materialize_column_name(column: &ProjectColumn, index: usize) -> String {
    if let Some(alias) = &column.alias {
        return alias.as_ref().to_owned();
    }
    match &column.expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::PropertyAccess { property, .. } => property.clone(),
        ExprKind::Aggregate { func, expr, .. } => {
            aggregate_expr_binding_name(func, expr.as_deref(), index)
        }
        _ => format!("col_{index}"),
    }
}

pub(super) fn collect_produced_vars(ops: &[PlanOp]) -> Vec<Rc<str>> {
    let mut vars: Vec<Rc<str>> = Vec::new();
    for op in ops {
        match op {
            PlanOp::NodeScan { variable, .. }
            | PlanOp::IndexScan { variable, .. }
            | PlanOp::IndexIntersection { variable, .. }
            | PlanOp::EdgeIndexScan { variable, .. } => push_var(&mut vars, variable),
            PlanOp::ConditionalIndexScan {
                fallback_variable, ..
            } => push_var(&mut vars, fallback_variable),
            PlanOp::Expand {
                src,
                edge,
                dst,
                hop_aux_binding,
                ..
            }
            | PlanOp::ExpandFilter {
                src,
                edge,
                dst,
                hop_aux_binding,
                ..
            } => {
                push_var(&mut vars, src);
                push_var(&mut vars, edge);
                push_var(&mut vars, dst);
                if let Some(pv) = hop_aux_binding {
                    push_var(&mut vars, pv);
                }
            }
            PlanOp::ShortestPath { src, edge, dst, .. } => {
                push_var(&mut vars, src);
                push_var(&mut vars, edge);
                push_var(&mut vars, dst);
            }
            PlanOp::EdgeBindEndpoints {
                edge,
                near,
                far,
                hop_aux_binding,
                ..
            } => {
                push_var(&mut vars, edge);
                push_var(&mut vars, near);
                push_var(&mut vars, far);
                if let Some(pv) = hop_aux_binding {
                    push_var(&mut vars, pv);
                }
            }
            PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
                for var in collect_produced_vars(left) {
                    push_var(&mut vars, &var);
                }
                for var in collect_produced_vars(right) {
                    push_var(&mut vars, &var);
                }
            }
            PlanOp::OptionalMatch { sub_plan } => {
                for var in collect_produced_vars(sub_plan) {
                    push_var(&mut vars, &var);
                }
            }
            PlanOp::Materialize { columns, .. } => {
                for (index, column) in columns.iter().enumerate() {
                    let name = Rc::<str>::from(materialize_column_name(column, index));
                    push_var(&mut vars, &name);
                }
            }
            PlanOp::WorstCaseOptimalJoin { variables, edges } => {
                for v in variables {
                    push_var(&mut vars, v);
                }
                for e in edges {
                    push_var(&mut vars, &e.variable);
                    if let Some(pv) = &e.hop_aux_binding {
                        push_var(&mut vars, pv);
                    }
                }
            }
            _ => {}
        }
    }
    vars
}

fn push_var(vars: &mut Vec<Rc<str>>, var: &Rc<str>) {
    if !vars.iter().any(|existing| existing == var) {
        vars.push(var.clone());
    }
}

pub(super) fn sort_binding_rows<G: GraphRead>(
    graph: &G,
    rows: &mut [BindingRow],
    order_by: &OrderByClause,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<()> {
    rows.sort_by(|left, right| compare_binding_rows(graph, left, right, order_by, ctx));
    Ok(())
}

pub(super) fn sort_output_rows(
    rows: &mut [OutputRow],
    order_by: &OrderByClause,
) -> ExecutionResultExt<()> {
    rows.sort_by(|left, right| compare_output_rows(left, right, order_by));
    Ok(())
}

fn eval_usize_expr(expr: Option<&Expr>) -> ExecutionResultExt<Option<usize>> {
    match expr {
        None => Ok(None),
        Some(Expr {
            kind: ExprKind::Literal(value),
            ..
        }) => match value {
            Value::Int8(v) if *v >= 0 => Ok(Some(*v as usize)),
            Value::Int16(v) if *v >= 0 => Ok(Some(*v as usize)),
            Value::Int32(v) if *v >= 0 => Ok(Some(*v as usize)),
            Value::Int64(v) if *v >= 0 => Ok(Some(*v as usize)),
            Value::Uint8(v) => Ok(Some(*v as usize)),
            Value::Uint16(v) => Ok(Some(*v as usize)),
            Value::Uint32(v) => Ok(Some(*v as usize)),
            Value::Uint64(v) => usize::try_from(*v)
                .map(Some)
                .map_err(|_| ExecutionError::InvalidLimit),
            _ => Err(ExecutionError::InvalidLimit),
        },
        Some(_) => Err(ExecutionError::InvalidLimit),
    }
}

pub(super) fn eval_expr<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    expr: &Expr,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Value> {
    match &expr.kind {
        ExprKind::Literal(value) => Ok(value.clone()),
        ExprKind::Parameter(name) => Ok(lookup_param(ctx, name)),
        ExprKind::Variable(name) => binding_to_value(graph, row, name),
        ExprKind::PropertyAccess { expr, property } => {
            eval_property_access(graph, row, expr, property, ctx)
        }
        ExprKind::Compare { left, op, right } => {
            let left = eval_expr(graph, row, left, ctx)?;
            let right = eval_expr(graph, row, right, ctx)?;
            let ord = compare_values(&left, &right);
            Ok(Value::Bool(apply_cmp(*op, ord)))
        }
        ExprKind::And(left, right) => {
            let left = eval_expr(graph, row, left, ctx)?;
            let right = eval_expr(graph, row, right, ctx)?;
            Ok(Value::Bool(expect_bool(left)? && expect_bool(right)?))
        }
        ExprKind::Or(left, right) => {
            let left = eval_expr(graph, row, left, ctx)?;
            let right = eval_expr(graph, row, right, ctx)?;
            Ok(Value::Bool(expect_bool(left)? || expect_bool(right)?))
        }
        ExprKind::Not(expr) => {
            let value = eval_expr(graph, row, expr, ctx)?;
            Ok(Value::Bool(!expect_bool(value)?))
        }
        ExprKind::IsNull(expr) => Ok(Value::Bool(matches!(
            eval_expr(graph, row, expr, ctx)?,
            Value::Null
        ))),
        ExprKind::IsNotNull(expr) => Ok(Value::Bool(!matches!(
            eval_expr(graph, row, expr, ctx)?,
            Value::Null
        ))),
        ExprKind::Paren(expr) => eval_expr(graph, row, expr, ctx),
        ExprKind::ListLiteral(items) => {
            let mut out = Vec::with_capacity(items.len());
            for item in items {
                out.push(eval_expr(graph, row, item, ctx)?);
            }
            Ok(Value::List(out))
        }
        ExprKind::FunctionCall { name, args, .. } => {
            let is_caller = name
                .parts
                .last()
                .is_some_and(|part| part.eq_ignore_ascii_case("caller"));
            if is_caller {
                if args.is_empty() {
                    return Ok(ctx.caller.clone().unwrap_or(Value::Null));
                }
                return Ok(Value::Null);
            }
            Err(ExecutionError::UnsupportedExpr("function call"))
        }
        _ => Err(ExecutionError::UnsupportedExpr("expression kind")),
    }
}

pub(super) fn eval_project_expr<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    column: &ProjectColumn,
    index: usize,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Value> {
    if let ExprKind::Aggregate { func, expr, .. } = &column.expr.kind {
        if let Some(alias) = &column.alias
            && let Some(BindingValue::Scalar(value)) = row.get(alias.as_ref())
        {
            return Ok(value.clone());
        }
        let key = aggregate_expr_binding_name(func, expr.as_deref(), index);
        if let Some(BindingValue::Scalar(value)) = row.get(key.as_str()) {
            return Ok(value.clone());
        }
    }
    eval_expr(graph, row, &column.expr, ctx)
}

pub(super) fn eval_property_assignments<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    properties: &[gleaph_gql_planner::plan::PropertyAssignment],
    ctx: &ExecutionContext,
) -> ExecutionResultExt<PropertyMap> {
    let mut map = PropertyMap::new();
    for assignment in properties {
        let value = eval_expr(graph, row, &assignment.value, ctx)?;
        map.insert(assignment.name.to_string(), value);
    }
    Ok(map)
}

pub(super) fn eval_materialize_expr<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    column: &ProjectColumn,
    index: usize,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<BindingValue> {
    match &column.expr.kind {
        ExprKind::Variable(name) => row
            .get(name.as_str())
            .cloned()
            .ok_or_else(|| ExecutionError::MissingBinding(name.clone())),
        ExprKind::Aggregate { func, expr, .. } => {
            if let Some(alias) = &column.alias
                && let Some(value) = row.get(alias.as_ref()).cloned()
            {
                return Ok(value);
            }
            let key = aggregate_expr_binding_name(func, expr.as_deref(), index);
            row.get(key.as_str())
                .cloned()
                .ok_or_else(|| ExecutionError::MissingBinding(key))
        }
        _ => Ok(BindingValue::Scalar(eval_project_expr(
            graph, row, column, index, ctx,
        )?)),
    }
}

fn eval_output_expr(row: &OutputRow, expr: &Expr) -> ExecutionResultExt<Value> {
    match &expr.kind {
        ExprKind::Literal(value) => Ok(value.clone()),
        ExprKind::Variable(name) => Ok(row.get(name).cloned().unwrap_or(Value::Null)),
        ExprKind::Aggregate { func, expr, .. } => {
            let key = aggregate_expr_binding_name(func, expr.as_deref(), 0);
            Ok(row.get(&key).cloned().unwrap_or(Value::Null))
        }
        ExprKind::Paren(expr) => eval_output_expr(row, expr),
        _ => Err(ExecutionError::UnsupportedExpr(
            "output sort expression kind",
        )),
    }
}

fn eval_property_access<G: GraphRead>(
    graph: &G,
    row: &BindingRow,
    expr: &Expr,
    property: &str,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Value> {
    match &expr.kind {
        ExprKind::Variable(name) => match row.get(name.as_str()) {
            Some(BindingValue::Node(node)) => {
                if let Some(v) = node.properties.get(property) {
                    return Ok(v.clone());
                }
                Ok(graph
                    .get_node_property_value(node.id, property)?
                    .unwrap_or(Value::Null))
            }
            Some(BindingValue::Edge(edge)) => {
                if let Some(v) = edge.properties.get(property) {
                    return Ok(v.clone());
                }
                Ok(graph
                    .get_edge_property_value(edge.id, property)?
                    .unwrap_or(Value::Null))
            }
            Some(BindingValue::Scalar(Value::Null)) => Ok(Value::Null),
            Some(BindingValue::Scalar(Value::Record(fields))) => Ok(fields
                .iter()
                .find(|(name, _)| name == property)
                .map(|(_, value)| value.clone())
                .unwrap_or(Value::Null)),
            Some(BindingValue::Scalar(_)) => Err(ExecutionError::TypeMismatch(
                "property access requires node, edge, or record",
            )),
            None => Err(ExecutionError::MissingBinding(name.clone())),
        },
        _ => {
            let base = eval_expr(graph, row, expr, ctx)?;
            match base {
                Value::Record(fields) => Ok(fields
                    .into_iter()
                    .find(|(name, _)| name == property)
                    .map(|(_, value)| value)
                    .unwrap_or(Value::Null)),
                _ => Err(ExecutionError::TypeMismatch(
                    "property access requires node, edge, or record",
                )),
            }
        }
    }
}

pub(super) fn resolve_scan_value(
    value: &ScanValue,
    ctx: &ExecutionContext,
) -> ExecutionResultExt<Value> {
    match value {
        ScanValue::Literal(value) => Ok(value.clone()),
        ScanValue::Parameter(name) => Ok(lookup_param(ctx, name.as_ref())),
    }
}

fn compare_binding_rows<G: GraphRead>(
    graph: &G,
    left: &BindingRow,
    right: &BindingRow,
    order_by: &OrderByClause,
    ctx: &ExecutionContext,
) -> Ordering {
    for item in &order_by.items {
        let left_value = match eval_expr(graph, left, &item.expr, ctx) {
            Ok(value) => value,
            Err(_) => return Ordering::Equal,
        };
        let right_value = match eval_expr(graph, right, &item.expr, ctx) {
            Ok(value) => value,
            Err(_) => return Ordering::Equal,
        };
        let ord = compare_sort_values(&left_value, &right_value, item.direction, item.null_order);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn compare_output_rows(left: &OutputRow, right: &OutputRow, order_by: &OrderByClause) -> Ordering {
    for item in &order_by.items {
        let left_value = match eval_output_expr(left, &item.expr) {
            Ok(value) => value,
            Err(_) => return Ordering::Equal,
        };
        let right_value = match eval_output_expr(right, &item.expr) {
            Ok(value) => value,
            Err(_) => return Ordering::Equal,
        };
        let ord = compare_sort_values(&left_value, &right_value, item.direction, item.null_order);
        if ord != Ordering::Equal {
            return ord;
        }
    }
    Ordering::Equal
}

fn compare_sort_values(
    left: &Value,
    right: &Value,
    direction: Option<SortDirection>,
    null_order: Option<NullOrder>,
) -> Ordering {
    match (left, right) {
        (Value::Null, Value::Null) => return Ordering::Equal,
        (Value::Null, _) => {
            return match null_order.unwrap_or(NullOrder::Last) {
                NullOrder::First => Ordering::Less,
                NullOrder::Last => Ordering::Greater,
            };
        }
        (_, Value::Null) => {
            return match null_order.unwrap_or(NullOrder::Last) {
                NullOrder::First => Ordering::Greater,
                NullOrder::Last => Ordering::Less,
            };
        }
        _ => {}
    }

    let ord = compare_values(left, right).unwrap_or(Ordering::Equal);
    match direction.unwrap_or(SortDirection::Asc) {
        SortDirection::Asc | SortDirection::Ascending => ord,
        SortDirection::Desc | SortDirection::Descending => ord.reverse(),
    }
}

fn binding_to_value<G: GraphRead>(
    _graph: &G,
    row: &BindingRow,
    name: &str,
) -> ExecutionResultExt<Value> {
    match row.get(name) {
        Some(BindingValue::Scalar(value)) => Ok(value.clone()),
        Some(BindingValue::Node(node)) => Ok(node_to_value(node)),
        Some(BindingValue::Edge(edge)) => Ok(edge_to_value(edge)),
        None => Err(ExecutionError::MissingBinding(name.to_owned())),
    }
}

pub(super) fn materialize_row<G: GraphRead>(
    _graph: &G,
    row: &BindingRow,
) -> ExecutionResultExt<OutputRow> {
    row.iter()
        .map(|(key, value)| {
            let value = match value {
                BindingValue::Scalar(value) => value.clone(),
                BindingValue::Node(node) => node_to_value(node),
                BindingValue::Edge(edge) => edge_to_value(edge),
            };
            Ok((key.to_string(), value))
        })
        .collect()
}

fn node_to_value(node: &NodeRecord) -> Value {
    let mut fields = Vec::with_capacity(node.properties.len() + 3);
    fields.push(("id".to_owned(), Value::Uint64(node.id.into())));
    fields.push((
        "labels".to_owned(),
        Value::List(node.labels.iter().cloned().map(Value::Text).collect()),
    ));
    for (key, value) in &node.properties {
        fields.push((key.clone(), value.clone()));
    }
    Value::Record(fields)
}

fn edge_to_value(edge: &EdgeRecord) -> Value {
    let mut fields = Vec::with_capacity(edge.properties.len() + 4);
    fields.push(("id".to_owned(), Value::Uint64(edge.id)));
    fields.push(("src".to_owned(), Value::Uint64(edge.src.into())));
    fields.push(("dst".to_owned(), Value::Uint64(edge.dst.into())));
    if let Some(label) = &edge.label {
        fields.push(("label".to_owned(), Value::Text(label.clone())));
    }
    for (key, value) in &edge.properties {
        fields.push((key.clone(), value.clone()));
    }
    Value::Record(fields)
}

fn apply_cmp(op: CmpOp, ordering: Option<Ordering>) -> bool {
    match op {
        CmpOp::Eq => ordering == Some(Ordering::Equal),
        CmpOp::Ne => ordering != Some(Ordering::Equal),
        CmpOp::Lt => ordering == Some(Ordering::Less),
        CmpOp::Le => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
        CmpOp::Gt => ordering == Some(Ordering::Greater),
        CmpOp::Ge => matches!(ordering, Some(Ordering::Greater | Ordering::Equal)),
    }
}

fn expect_bool(value: Value) -> ExecutionResultExt<bool> {
    match value {
        Value::Bool(value) => Ok(value),
        _ => Err(ExecutionError::TypeMismatch("expected boolean")),
    }
}

pub(super) fn column_name(column: &ProjectColumn) -> String {
    if let Some(alias) = &column.alias {
        return alias.as_ref().to_owned();
    }
    match &column.expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::PropertyAccess { property, .. } => property.clone(),
        _ => "expr".to_owned(),
    }
}
