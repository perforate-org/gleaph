use std::collections::BTreeMap;
use std::collections::BTreeSet;
use std::sync::{Arc, RwLock};

use crate::ApiValue;
use candid::{CandidType, Deserialize, Principal};
use futures::executor::block_on;
use gleaph_gql::Value;
use gleaph_gql::ast::{Expr, ExprKind, NullOrder, SortDirection};
use gleaph_gql_executor::{
    BindingRow, BindingValue, ExecutionContext, ExecutionError, ExecutionResultExt,
    GraphRegistryResolver, GraphResolution, OutputRow, UseGraphRouter,
};
use gleaph_gql_ic::PrincipalValue;
use gleaph_gql_ic::graph_registry::{
    GraphRegistryEntry, GraphRegistryError, GraphRegistryStore, InMemoryGraphRegistry,
};
use gleaph_gql_planner::PlanOp;
use gleaph_gql_planner::plan::{ScanValue, VarLenSpec};
use ic_cdk::call::Call;

#[derive(Clone, Default)]
pub struct InMemoryGraphRegistryResolver {
    inner: Arc<RwLock<InMemoryGraphRegistry>>,
}

impl InMemoryGraphRegistryResolver {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register_graph(&self, entry: GraphRegistryEntry) -> Result<(), GraphRegistryError> {
        let mut guard = self
            .inner
            .write()
            .map_err(|_| GraphRegistryError::Unavailable)?;
        guard.register_graph(entry)
    }
}

impl GraphRegistryResolver for InMemoryGraphRegistryResolver {
    fn resolve(
        &self,
        requested_graph: &str,
        caller: Option<&Value>,
    ) -> ExecutionResultExt<GraphResolution> {
        let caller = extract_principal_caller(caller)?;

        let guard = self
            .inner
            .read()
            .map_err(|_| ExecutionError::InvalidPlan("graph registry unavailable".to_owned()))?;
        let resolved = guard
            .resolve_graph(requested_graph, caller)
            .map_err(map_registry_error)?;
        Ok(GraphResolution {
            graph_name: resolved.graph_name,
            canister_id: Some(resolved.canister_id.to_text()),
        })
    }
}

/// Registry client that caches `resolve_graph` results from a registry canister.
///
/// [`GraphRegistryResolver::resolve`] blocks on a canister call when the graph name is not yet
/// cached, then stores the result. [`Self::refresh_graph`] is still useful to pre-warm the cache
/// before queries (fewer nested waits) or to force-refresh after registry updates.
#[derive(Clone)]
struct CachedGraphResolution {
    resolution: GraphResolution,
    /// Wall clock from [`ic_cdk::api::time`] (nanoseconds).
    fetched_at_ns: u64,
}

#[derive(Clone)]
pub struct IcGraphRegistryResolver {
    registry_canister_id: Principal,
    cache: Arc<RwLock<BTreeMap<String, CachedGraphResolution>>>,
    /// When set, entries older than this age are refreshed on [`GraphRegistryResolver::resolve`].
    ttl_ns: Option<u64>,
}

impl IcGraphRegistryResolver {
    pub fn new(registry_canister_id: Principal) -> Self {
        Self {
            registry_canister_id,
            cache: Arc::new(RwLock::new(BTreeMap::new())),
            ttl_ns: None,
        }
    }

    /// Cache entries are treated as stale after this duration and refreshed on the next resolve.
    pub fn with_cache_ttl(mut self, ttl: std::time::Duration) -> Self {
        let n = ttl.as_nanos().min(u128::from(u64::MAX)) as u64;
        self.ttl_ns = Some(n);
        self
    }

    pub fn cache_ttl(&self) -> Option<std::time::Duration> {
        self.ttl_ns.map(std::time::Duration::from_nanos)
    }

    pub fn registry_canister_id(&self) -> Principal {
        self.registry_canister_id
    }

    fn cache_now_ns() -> u64 {
        ic_cdk::api::time()
    }

    pub async fn refresh_graph(&self, graph_name: &str) -> ExecutionResultExt<GraphResolution> {
        let resolved = resolve_graph_via_canister(self.registry_canister_id, graph_name).await?;
        let mut guard = self.cache.write().map_err(|_| {
            ExecutionError::InvalidPlan("graph registry cache unavailable".to_owned())
        })?;
        guard.insert(
            graph_name.to_owned(),
            CachedGraphResolution {
                resolution: resolved.clone(),
                fetched_at_ns: Self::cache_now_ns(),
            },
        );
        Ok(resolved)
    }

    /// Drops one cached graph mapping (e.g. after the registry canister updates routing).
    pub fn invalidate_cached_graph(&self, graph_name: &str) {
        if let Ok(mut guard) = self.cache.write() {
            guard.remove(graph_name);
        }
    }

    pub fn clear_cache(&self) {
        if let Ok(mut guard) = self.cache.write() {
            guard.clear();
        }
    }
}

#[derive(Clone, Default)]
pub struct IcUseGraphRouter;

fn scalar_binding_row_to_remote_params(
    row: &BindingRow,
) -> ExecutionResultExt<Option<BTreeMap<String, ApiValue>>> {
    if row.is_empty() {
        return Ok(None);
    }
    let mut params = BTreeMap::new();
    for (k, v) in row.iter() {
        match v {
            BindingValue::Scalar(val) => {
                params.insert(k.to_string(), ApiValue::from(val));
            }
            BindingValue::Node(_) | BindingValue::Edge(_) => {
                return Err(ExecutionError::InvalidPlan(
                    "remote USE graph input row may only contain scalar bindings".to_owned(),
                ));
            }
        }
    }
    Ok(Some(params))
}

fn remote_param_sets_from_rows(
    rows: &[BindingRow],
) -> ExecutionResultExt<Vec<Option<BTreeMap<String, ApiValue>>>> {
    rows.iter()
        .map(scalar_binding_row_to_remote_params)
        .collect::<ExecutionResultExt<Vec<_>>>()
}

fn query_subject_for_remote(ctx: &ExecutionContext) -> Option<Principal> {
    let v = ctx.caller.as_ref()?;
    match v {
        Value::Extension(e) => e
            .as_any()
            .downcast_ref::<PrincipalValue>()
            .map(|p| p.0),
        _ => None,
    }
}

#[async_trait::async_trait(?Send)]
impl UseGraphRouter for IcUseGraphRouter {
    async fn execute_remote_subplan(
        &self,
        target: &GraphResolution,
        sub_plan: &[PlanOp],
        ctx: &ExecutionContext,
        input_rows: Vec<BindingRow>,
    ) -> ExecutionResultExt<(Vec<BindingRow>, Option<Vec<OutputRow>>)> {
        if input_rows.is_empty() {
            return Ok((Vec::new(), Some(Vec::new())));
        }
        let query = subplan_to_routed_query(sub_plan)?;
        let remote_param_sets = remote_param_sets_from_rows(&input_rows)?;
        let canister_id = target.canister_id.as_ref().ok_or_else(|| {
            ExecutionError::InvalidPlan("remote USE graph target missing canister_id".to_owned())
        })?;
        let canister_id = Principal::from_text(canister_id)
            .map_err(|e| ExecutionError::InvalidPlan(format!("invalid remote canister id: {e}")))?;
        let subject = query_subject_for_remote(ctx);
        let param_rows: Vec<BTreeMap<String, ApiValue>> = remote_param_sets
            .into_iter()
            .map(|o| o.unwrap_or_default())
            .collect();
        if param_rows.len() > crate::MAX_FEDERATION_ROUTED_PARAM_ROWS {
            return Err(ExecutionError::InvalidPlan(format!(
                "remote USE GRAPH exceeds federation param row limit {}",
                crate::MAX_FEDERATION_ROUTED_PARAM_ROWS
            )));
        }
        let remote: Result<crate::ApiExecutionResult, String> =
            Call::bounded_wait(canister_id, "execute_routed_query_batch")
                .with_arg((query, param_rows, subject))
                .await
                .map_err(|e| ExecutionError::InvalidPlan(format!("remote call failed: {e}")))?
                .candid()
                .map_err(|e| ExecutionError::InvalidPlan(format!("remote decode failed: {e}")))?;
        let response = remote.map_err(|e| {
            ExecutionError::InvalidPlan(format!("remote query execution failed: {e}"))
        })?;
        let rows = response
            .rows
            .into_iter()
            .map(|row| {
                row.into_iter()
                    .map(|(k, v)| (k, Value::from(&v)))
                    .collect::<OutputRow>()
            })
            .collect();
        Ok((Vec::new(), Some(rows)))
    }
}

pub(crate) fn subplan_to_routed_query(sub_plan: &[PlanOp]) -> ExecutionResultExt<String> {
    if sub_plan.is_empty() {
        return Err(ExecutionError::InvalidPlan(
            "remote USE graph subplan is empty".to_owned(),
        ));
    }
    let root = translate_remote_subplan_root(sub_plan)?;
    let (
        route_prefix,
        route_where_exprs,
        project_start_index,
        aggregate_aliases_seed,
        group_by_exprs_seed,
    ) = match root {
        RemoteSubplanRoot::Call {
            name,
            args,
            yield_columns,
            optional,
        } => {
            let optional_kw = if optional { "OPTIONAL " } else { "" };
            let rendered_args = args
                .iter()
                .map(render_expr)
                .collect::<ExecutionResultExt<Vec<_>>>()?
                .join(", ");
            let proc_name = name
                .iter()
                .map(|p| p.as_ref())
                .collect::<Vec<_>>()
                .join(".");
            let yield_clause = if let Some(columns) = yield_columns {
                let names = columns
                    .iter()
                    .map(|c| match c.alias.as_ref() {
                        Some(alias) => format!("{} AS {}", c.name, alias),
                        None => c.name.to_string(),
                    })
                    .collect::<Vec<_>>()
                    .join(", ");
                format!(" YIELD {names}")
            } else {
                String::new()
            };
            (
                format!("{optional_kw}CALL {proc_name}({rendered_args}){yield_clause}"),
                Vec::new(),
                1,
                BTreeMap::new(),
                BTreeSet::new(),
            )
        }
        RemoteSubplanRoot::Match {
            pattern,
            where_exprs,
            consumed_ops,
        } => (
            pattern,
            where_exprs,
            consumed_ops,
            BTreeMap::new(),
            BTreeSet::new(),
        ),
    };
    let mut where_exprs: Vec<String> = route_where_exprs;
    let mut having_exprs: Vec<String> = Vec::new();
    let mut project_columns = None;
    let mut project_distinct = false;
    let mut limit_count = None;
    let mut limit_offset = None;
    let mut order_by_items: Vec<String> = Vec::new();
    let mut aggregate_aliases: BTreeMap<String, String> = aggregate_aliases_seed;
    let mut group_by_exprs: BTreeSet<String> = group_by_exprs_seed;
    let mut seen_aggregate = false;
    for op in &sub_plan[project_start_index..] {
        match op {
            PlanOp::Filter { condition } => {
                if seen_aggregate {
                    having_exprs.push(render_expr_with_aliases(condition, &aggregate_aliases)?);
                } else {
                    where_exprs.push(render_expr(condition)?);
                }
            }
            PlanOp::PropertyFilter { predicates, .. } => {
                for p in predicates {
                    if seen_aggregate {
                        having_exprs.push(render_expr_with_aliases(p, &aggregate_aliases)?);
                    } else {
                        where_exprs.push(render_expr(p)?);
                    }
                }
            }
            PlanOp::Project { columns, distinct } => {
                project_columns = Some(columns);
                project_distinct = *distinct;
            }
            PlanOp::Limit { count, offset } => {
                limit_count = count.as_ref().map(render_expr).transpose()?;
                limit_offset = offset.as_ref().map(render_expr).transpose()?;
            }
            PlanOp::Sort { order_by } => {
                order_by_items = order_by
                    .items
                    .iter()
                    .map(|item| {
                        let expr = if seen_aggregate {
                            render_expr_with_aliases(&item.expr, &aggregate_aliases)?
                        } else {
                            render_expr(&item.expr)?
                        };
                        let dir = match item.direction {
                            Some(SortDirection::Desc | SortDirection::Descending) => " DESC",
                            Some(SortDirection::Asc | SortDirection::Ascending) | None => "",
                        };
                        let nulls = match item.null_order {
                            Some(NullOrder::First) => " NULLS FIRST",
                            Some(NullOrder::Last) => " NULLS LAST",
                            None => "",
                        };
                        Ok(format!("{expr}{dir}{nulls}"))
                    })
                    .collect::<ExecutionResultExt<Vec<_>>>()?;
            }
            PlanOp::TopK {
                order_by,
                k,
                offset,
            } => {
                order_by_items = order_by
                    .items
                    .iter()
                    .map(|item| {
                        let expr = if seen_aggregate {
                            render_expr_with_aliases(&item.expr, &aggregate_aliases)?
                        } else {
                            render_expr(&item.expr)?
                        };
                        let dir = match item.direction {
                            Some(SortDirection::Desc | SortDirection::Descending) => " DESC",
                            Some(SortDirection::Asc | SortDirection::Ascending) | None => "",
                        };
                        let nulls = match item.null_order {
                            Some(NullOrder::First) => " NULLS FIRST",
                            Some(NullOrder::Last) => " NULLS LAST",
                            None => "",
                        };
                        Ok(format!("{expr}{dir}{nulls}"))
                    })
                    .collect::<ExecutionResultExt<Vec<_>>>()?;
                limit_count = Some(render_expr(k)?);
                limit_offset = offset.as_ref().map(render_expr).transpose()?;
            }
            PlanOp::Aggregate {
                aggregates,
                group_by,
            } => {
                seen_aggregate = true;
                aggregate_aliases.clear();
                group_by_exprs.clear();
                for expr in group_by {
                    group_by_exprs.insert(render_expr(expr)?);
                }
                for agg in aggregates {
                    let alias = agg.alias.as_ref().ok_or_else(|| {
                        ExecutionError::InvalidPlan(
                            "remote USE graph aggregate requires aliases".to_owned(),
                        )
                    })?;
                    let expr = match (&agg.func, &agg.expr) {
                        (func, None) => format!("{func}(*)"),
                        (func, Some(expr)) => {
                            let distinct = if agg.distinct { "DISTINCT " } else { "" };
                            format!("{func}({distinct}{})", render_expr(expr)?)
                        }
                    };
                    aggregate_aliases.insert(alias.to_string(), expr);
                }
            }
            other => {
                return Err(ExecutionError::InvalidPlan(format!(
                    "remote USE graph cannot translate {} after CALL; only FILTER/PROPERTY FILTER/AGGREGATE/PROJECT/SORT/TOPK/LIMIT are supported",
                    remote_use_graph_op_name(other)
                )));
            }
        }
    }
    let project_columns = project_columns.ok_or_else(|| {
        ExecutionError::InvalidPlan("remote USE graph subplan requires PROJECT".to_owned())
    })?;
    let mut return_names = Vec::new();
    for column in project_columns {
        let rendered_col_expr = render_expr(&column.expr)?;
        let mut rendered_expr = rendered_col_expr.clone();
        let is_group_key = group_by_exprs.contains(&rendered_col_expr);
        let is_inline_aggregate = matches!(column.expr.kind, ExprKind::Aggregate { .. });
        if let ExprKind::Variable(name) = &column.expr.kind
            && let Some(agg_expr) = aggregate_aliases.get(name)
        {
            rendered_expr = agg_expr.clone();
        }
        if !aggregate_aliases.is_empty()
            && !is_group_key
            && rendered_expr == rendered_col_expr
            && !is_inline_aggregate
        {
            return Err(ExecutionError::InvalidPlan(format!(
                "project expression `{rendered_col_expr}` is not grouped or aggregated"
            )));
        }
        let rendered = if let Some(alias) = column.alias.as_ref() {
            format!("{rendered_expr} AS {alias}")
        } else {
            rendered_expr
        };
        return_names.push(rendered);
    }
    let return_clause = return_names.join(", ");
    let where_clause = if where_exprs.is_empty() {
        String::new()
    } else {
        format!(" WHERE {}", where_exprs.join(" AND "))
    };
    let group_by_clause = if group_by_exprs.is_empty() {
        String::new()
    } else {
        let items = group_by_exprs
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join(", ");
        format!(" GROUP BY {items}")
    };
    let having_clause = if having_exprs.is_empty() {
        String::new()
    } else {
        format!(" HAVING {}", having_exprs.join(" AND "))
    };
    let order_by_clause = if order_by_items.is_empty() {
        String::new()
    } else {
        format!(" ORDER BY {}", order_by_items.join(", "))
    };
    let distinct_clause = if project_distinct { "DISTINCT " } else { "" };
    let limit_clause = match (limit_count, limit_offset) {
        (Some(count), Some(offset)) => format!(" LIMIT {count} OFFSET {offset}"),
        (Some(count), None) => format!(" LIMIT {count}"),
        (None, Some(offset)) => format!(" OFFSET {offset}"),
        (None, None) => String::new(),
    };
    Ok(format!(
        "{route_prefix}{where_clause} RETURN {distinct_clause}{return_clause}{group_by_clause}{having_clause}{order_by_clause}{limit_clause}"
    ))
}

enum RemoteSubplanRoot<'a> {
    Call {
        name: &'a [std::rc::Rc<str>],
        args: &'a [Expr],
        yield_columns: Option<&'a [gleaph_gql_planner::plan::YieldColumn]>,
        optional: bool,
    },
    Match {
        pattern: String,
        where_exprs: Vec<String>,
        consumed_ops: usize,
    },
}

fn translate_remote_subplan_root(sub_plan: &[PlanOp]) -> ExecutionResultExt<RemoteSubplanRoot<'_>> {
    match &sub_plan[0] {
        PlanOp::CallProcedure {
            name,
            args,
            yield_columns,
            optional,
        } => Ok(RemoteSubplanRoot::Call {
            name,
            args,
            yield_columns: yield_columns.as_deref(),
            optional: *optional,
        }),
        PlanOp::NodeScan {
            variable, label, ..
        } => {
            let mut pattern = match label {
                Some(label) => format!("MATCH ({variable}:{label})"),
                None => format!("MATCH ({variable})"),
            };
            let mut where_exprs = Vec::new();
            let mut consumed_ops = 1;
            let mut current_src = variable.as_ref().to_owned();

            while let Some(next) = sub_plan.get(consumed_ops) {
                match next {
                    PlanOp::Expand {
                        src,
                        edge,
                        dst,
                        direction,
                        label,
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        ..
                    } if src.as_ref() == current_src => {
                        pattern.push_str(&render_simple_expand_pattern(
                            edge,
                            dst,
                            *direction,
                            label.as_deref(),
                            label_expr.is_some(),
                            remote_expand_var_len_unsupported(var_len.as_ref()),
                            indexed_edge_equality.is_some(),
                        )?);
                        current_src = dst.as_ref().to_owned();
                        consumed_ops += 1;
                    }
                    PlanOp::ExpandFilter {
                        src,
                        edge,
                        dst,
                        direction,
                        label,
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        dst_filter,
                        ..
                    } if src.as_ref() == current_src => {
                        pattern.push_str(&render_simple_expand_pattern(
                            edge,
                            dst,
                            *direction,
                            label.as_deref(),
                            label_expr.is_some(),
                            remote_expand_var_len_unsupported(var_len.as_ref()),
                            indexed_edge_equality.is_some(),
                        )?);
                        where_exprs.extend(
                            dst_filter
                                .iter()
                                .map(render_expr)
                                .collect::<ExecutionResultExt<Vec<_>>>()?,
                        );
                        current_src = dst.as_ref().to_owned();
                        consumed_ops += 1;
                    }
                    _ => break,
                }
            }

            Ok(RemoteSubplanRoot::Match {
                pattern,
                where_exprs,
                consumed_ops,
            })
        }
        PlanOp::IndexScan {
            variable,
            property,
            value,
            cmp,
            ..
        } => {
            let mut pattern = format!("MATCH ({variable})");
            let mut where_exprs = vec![format!(
                "{variable}.{property} {} {}",
                render_cmp_op(*cmp),
                render_scan_value(value)?
            )];
            let mut consumed_ops = 1;
            let mut current_src = variable.as_ref().to_owned();

            while let Some(next) = sub_plan.get(consumed_ops) {
                match next {
                    PlanOp::Expand {
                        src,
                        edge,
                        dst,
                        direction,
                        label,
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        ..
                    } if src.as_ref() == current_src => {
                        pattern.push_str(&render_simple_expand_pattern(
                            edge,
                            dst,
                            *direction,
                            label.as_deref(),
                            label_expr.is_some(),
                            remote_expand_var_len_unsupported(var_len.as_ref()),
                            indexed_edge_equality.is_some(),
                        )?);
                        current_src = dst.as_ref().to_owned();
                        consumed_ops += 1;
                    }
                    PlanOp::ExpandFilter {
                        src,
                        edge,
                        dst,
                        direction,
                        label,
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        dst_filter,
                        ..
                    } if src.as_ref() == current_src => {
                        pattern.push_str(&render_simple_expand_pattern(
                            edge,
                            dst,
                            *direction,
                            label.as_deref(),
                            label_expr.is_some(),
                            remote_expand_var_len_unsupported(var_len.as_ref()),
                            indexed_edge_equality.is_some(),
                        )?);
                        where_exprs.extend(
                            dst_filter
                                .iter()
                                .map(render_expr)
                                .collect::<ExecutionResultExt<Vec<_>>>()?,
                        );
                        current_src = dst.as_ref().to_owned();
                        consumed_ops += 1;
                    }
                    _ => break,
                }
            }

            Ok(RemoteSubplanRoot::Match {
                pattern,
                where_exprs,
                consumed_ops,
            })
        }
        PlanOp::EdgeIndexScan {
            variable,
            property,
            value,
            ..
        } => {
            let Some(PlanOp::EdgeBindEndpoints {
                edge,
                near,
                far,
                direction,
                label,
                ..
            }) = sub_plan.get(1)
            else {
                return Err(ExecutionError::InvalidPlan(
                    "remote USE graph requires EDGE BIND ENDPOINTS immediately after EDGE INDEX SCAN"
                        .to_owned(),
                ));
            };
            let mut pattern =
                render_edge_index_root_pattern(edge, near, far, *direction, label.as_deref())?;
            let mut consumed_ops = 2;
            let mut current_src = far.as_ref().to_owned();
            let mut where_exprs = vec![format!(
                "{variable}.{property} = {}",
                render_scan_value(value)?
            )];

            while let Some(next) = sub_plan.get(consumed_ops) {
                match next {
                    PlanOp::Expand {
                        src,
                        edge,
                        dst,
                        direction,
                        label,
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        ..
                    } if src.as_ref() == current_src => {
                        pattern.push_str(&render_simple_expand_pattern(
                            edge,
                            dst,
                            *direction,
                            label.as_deref(),
                            label_expr.is_some(),
                            remote_expand_var_len_unsupported(var_len.as_ref()),
                            indexed_edge_equality.is_some(),
                        )?);
                        current_src = dst.as_ref().to_owned();
                        consumed_ops += 1;
                    }
                    PlanOp::ExpandFilter {
                        src,
                        edge,
                        dst,
                        direction,
                        label,
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        dst_filter,
                        ..
                    } if src.as_ref() == current_src => {
                        pattern.push_str(&render_simple_expand_pattern(
                            edge,
                            dst,
                            *direction,
                            label.as_deref(),
                            label_expr.is_some(),
                            remote_expand_var_len_unsupported(var_len.as_ref()),
                            indexed_edge_equality.is_some(),
                        )?);
                        where_exprs.extend(
                            dst_filter
                                .iter()
                                .map(render_expr)
                                .collect::<ExecutionResultExt<Vec<_>>>()?,
                        );
                        current_src = dst.as_ref().to_owned();
                        consumed_ops += 1;
                    }
                    _ => break,
                }
            }
            Ok(RemoteSubplanRoot::Match {
                pattern,
                where_exprs,
                consumed_ops,
            })
        }
        other => Err(ExecutionError::InvalidPlan(format!(
            "remote USE graph requires CALL, NODE SCAN, INDEX SCAN, or EDGE INDEX SCAN as first op, got {}",
            remote_use_graph_op_name(other)
        ))),
    }
}

pub(crate) fn render_expr(expr: &Expr) -> ExecutionResultExt<String> {
    render_expr_with_aliases(expr, &BTreeMap::new())
}

fn render_expr_with_aliases(
    expr: &Expr,
    aggregate_aliases: &BTreeMap<String, String>,
) -> ExecutionResultExt<String> {
    match &expr.kind {
        ExprKind::Literal(v) => render_literal(v),
        ExprKind::Variable(name) => Ok(aggregate_aliases
            .get(name)
            .cloned()
            .unwrap_or_else(|| name.clone())),
        ExprKind::Parameter(name) => Ok(format!("${name}")),
        ExprKind::PropertyAccess { expr, property } => {
            Ok(format!(
                "{}.{}",
                render_expr_with_aliases(expr, aggregate_aliases)?,
                property
            ))
        }
        ExprKind::Paren(inner) => Ok(format!(
            "({})",
            render_expr_with_aliases(inner, aggregate_aliases)?
        )),
        ExprKind::UnaryOp { op, expr } => {
            Ok(format!("{op}{}", render_expr_with_aliases(expr, aggregate_aliases)?))
        }
        ExprKind::BinaryOp { left, op, right } => Ok(format!(
            "{} {op} {}",
            render_expr_with_aliases(left, aggregate_aliases)?,
            render_expr_with_aliases(right, aggregate_aliases)?
        )),
        ExprKind::Compare { left, op, right } => Ok(format!(
            "{} {op} {}",
            render_expr_with_aliases(left, aggregate_aliases)?,
            render_expr_with_aliases(right, aggregate_aliases)?
        )),
        ExprKind::And(left, right) => Ok(format!(
            "{} AND {}",
            render_expr_with_aliases(left, aggregate_aliases)?,
            render_expr_with_aliases(right, aggregate_aliases)?
        )),
        ExprKind::Or(left, right) => Ok(format!(
            "{} OR {}",
            render_expr_with_aliases(left, aggregate_aliases)?,
            render_expr_with_aliases(right, aggregate_aliases)?
        )),
        ExprKind::Xor(left, right) => Ok(format!(
            "{} XOR {}",
            render_expr_with_aliases(left, aggregate_aliases)?,
            render_expr_with_aliases(right, aggregate_aliases)?
        )),
        ExprKind::Not(inner) => Ok(format!(
            "NOT {}",
            render_expr_with_aliases(inner, aggregate_aliases)?
        )),
        ExprKind::FunctionCall {
            name,
            args,
            distinct,
        } => {
            let func_name = name.parts.join(".");
            let rendered_args = args
                .iter()
                .map(|arg| render_expr_with_aliases(arg, aggregate_aliases))
                .collect::<ExecutionResultExt<Vec<_>>>()?
                .join(", ");
            let distinct_prefix = if *distinct { "DISTINCT " } else { "" };
            Ok(format!("{func_name}({distinct_prefix}{rendered_args})"))
        }
        ExprKind::Aggregate {
            func,
            expr,
            expr2,
            distinct,
            ..
        } => {
            let func_name = render_aggregate_func(*func);
            let distinct_prefix = if *distinct { "DISTINCT " } else { "" };
            let rendered = match (expr, expr2) {
                (None, None) => format!("{func_name}(*)"),
                (Some(e), None) => format!(
                    "{func_name}({distinct_prefix}{})",
                    render_expr_with_aliases(e, aggregate_aliases)?
                ),
                (Some(e1), Some(e2)) => format!(
                    "{func_name}({distinct_prefix}{}, {})",
                    render_expr_with_aliases(e1, aggregate_aliases)?,
                    render_expr_with_aliases(e2, aggregate_aliases)?
                ),
                (None, Some(_)) => {
                    return Err(ExecutionError::InvalidPlan(
                        "aggregate expression shape is invalid for routed translation".to_owned(),
                    ))
                }
            };
            Ok(rendered)
        }
        ExprKind::Coalesce(items) => Ok(format!(
            "COALESCE({})",
            items
                .iter()
                .map(|item| render_expr_with_aliases(item, aggregate_aliases))
                .collect::<ExecutionResultExt<Vec<_>>>()?
                .join(", ")
        )),
        ExprKind::NullIf(left, right) => Ok(format!(
            "NULLIF({}, {})",
            render_expr_with_aliases(left, aggregate_aliases)?,
            render_expr_with_aliases(right, aggregate_aliases)?
        )),
        ExprKind::CaseSimple {
            operand,
            when_clauses,
            else_clause,
        } => {
            let mut parts = Vec::new();
            parts.push(format!(
                "CASE {}",
                render_expr_with_aliases(operand, aggregate_aliases)?
            ));
            for w in when_clauses {
                parts.push(format!(
                    "WHEN {} THEN {}",
                    render_expr_with_aliases(&w.condition, aggregate_aliases)?,
                    render_expr_with_aliases(&w.result, aggregate_aliases)?
                ));
            }
            if let Some(else_expr) = else_clause {
                parts.push(format!(
                    "ELSE {}",
                    render_expr_with_aliases(else_expr, aggregate_aliases)?
                ));
            }
            parts.push("END".to_owned());
            Ok(parts.join(" "))
        }
        ExprKind::CaseSearched {
            when_clauses,
            else_clause,
        } => {
            let mut parts = vec!["CASE".to_owned()];
            for w in when_clauses {
                parts.push(format!(
                    "WHEN {} THEN {}",
                    render_expr_with_aliases(&w.condition, aggregate_aliases)?,
                    render_expr_with_aliases(&w.result, aggregate_aliases)?
                ));
            }
            if let Some(else_expr) = else_clause {
                parts.push(format!(
                    "ELSE {}",
                    render_expr_with_aliases(else_expr, aggregate_aliases)?
                ));
            }
            parts.push("END".to_owned());
            Ok(parts.join(" "))
        }
        ExprKind::ListLiteral(items) => Ok(format!(
            "[{}]",
            items
                .iter()
                .map(|item| render_expr_with_aliases(item, aggregate_aliases))
                .collect::<ExecutionResultExt<Vec<_>>>()?
                .join(", ")
        )),
        _ => Err(ExecutionError::InvalidPlan(
            "remote USE graph currently supports literal/variable/property/basic operator expressions only"
                .to_owned(),
        )),
    }
}

fn render_aggregate_func(func: gleaph_gql::ast::AggregateFunc) -> &'static str {
    match func {
        gleaph_gql::ast::AggregateFunc::Count => "count",
        gleaph_gql::ast::AggregateFunc::CountStar => "count",
        gleaph_gql::ast::AggregateFunc::Sum => "sum",
        gleaph_gql::ast::AggregateFunc::Avg => "avg",
        gleaph_gql::ast::AggregateFunc::Min => "min",
        gleaph_gql::ast::AggregateFunc::Max => "max",
        gleaph_gql::ast::AggregateFunc::Collect => "collect",
        gleaph_gql::ast::AggregateFunc::StddevSamp => "stddev_samp",
        gleaph_gql::ast::AggregateFunc::StddevPop => "stddev_pop",
        gleaph_gql::ast::AggregateFunc::PercentileCont => "percentile_cont",
        gleaph_gql::ast::AggregateFunc::PercentileDisc => "percentile_disc",
    }
}

fn render_cmp_op(op: gleaph_gql::ast::CmpOp) -> &'static str {
    match op {
        gleaph_gql::ast::CmpOp::Eq => "=",
        gleaph_gql::ast::CmpOp::Ne => "<>",
        gleaph_gql::ast::CmpOp::Lt => "<",
        gleaph_gql::ast::CmpOp::Le => "<=",
        gleaph_gql::ast::CmpOp::Gt => ">",
        gleaph_gql::ast::CmpOp::Ge => ">=",
    }
}

fn render_scan_value(value: &ScanValue) -> ExecutionResultExt<String> {
    match value {
        ScanValue::Literal(value) => render_literal(value),
        ScanValue::Parameter(name) => Ok(format!("${name}")),
    }
}

/// `true` when variable-length differs from a single fixed hop (`{1,1}`).
fn remote_expand_var_len_unsupported(var_len: Option<&VarLenSpec>) -> bool {
    match var_len {
        None => false,
        Some(v) => v.min != 1 || v.max != Some(1),
    }
}

fn render_simple_expand_pattern(
    edge: &std::rc::Rc<str>,
    dst: &std::rc::Rc<str>,
    direction: gleaph_gql::types::EdgeDirection,
    label: Option<&str>,
    has_label_expr: bool,
    has_var_len: bool,
    has_indexed_edge_equality: bool,
) -> ExecutionResultExt<String> {
    if has_label_expr || has_var_len || has_indexed_edge_equality {
        return Err(ExecutionError::InvalidPlan(
            "remote USE graph traversal pushdown supports only single-hop expand without label expressions, variable-length bounds, or indexed edge equality"
                .to_owned(),
        ));
    }
    let edge_segment = match label {
        Some(label) => format!("[{edge}:{label}]"),
        None => format!("[{edge}]"),
    };
    let pattern = match direction {
        gleaph_gql::types::EdgeDirection::PointingRight => {
            format!("-{edge_segment}->({dst})")
        }
        gleaph_gql::types::EdgeDirection::PointingLeft => {
            format!("<-{edge_segment}-({dst})")
        }
        gleaph_gql::types::EdgeDirection::AnyDirection => {
            format!("-{edge_segment}-({dst})")
        }
        gleaph_gql::types::EdgeDirection::LeftOrRight => {
            format!("<-{edge_segment}->({dst})")
        }
        gleaph_gql::types::EdgeDirection::Undirected => {
            format!("~{edge_segment}~({dst})")
        }
        gleaph_gql::types::EdgeDirection::LeftOrUndirected => {
            format!("<~{edge_segment}~({dst})")
        }
        gleaph_gql::types::EdgeDirection::UndirectedOrRight => {
            format!("~{edge_segment}~>({dst})")
        }
    };
    Ok(pattern)
}

fn render_edge_index_root_pattern(
    edge: &std::rc::Rc<str>,
    near: &std::rc::Rc<str>,
    far: &std::rc::Rc<str>,
    direction: gleaph_gql::types::EdgeDirection,
    label: Option<&str>,
) -> ExecutionResultExt<String> {
    let edge_segment = match label {
        Some(label) => format!("[{edge}:{label}]"),
        None => format!("[{edge}]"),
    };
    let pattern = match direction {
        gleaph_gql::types::EdgeDirection::PointingRight => {
            format!("MATCH ({near})-{edge_segment}->({far})")
        }
        gleaph_gql::types::EdgeDirection::PointingLeft => {
            format!("MATCH ({near})<-{edge_segment}-({far})")
        }
        gleaph_gql::types::EdgeDirection::AnyDirection => {
            format!("MATCH ({near})-{edge_segment}-({far})")
        }
        gleaph_gql::types::EdgeDirection::LeftOrRight => {
            format!("MATCH ({near})<-{edge_segment}->({far})")
        }
        gleaph_gql::types::EdgeDirection::Undirected => {
            format!("MATCH ({near})~{edge_segment}~({far})")
        }
        gleaph_gql::types::EdgeDirection::LeftOrUndirected => {
            format!("MATCH ({near})<~{edge_segment}~({far})")
        }
        gleaph_gql::types::EdgeDirection::UndirectedOrRight => {
            format!("MATCH ({near})~{edge_segment}~>({far})")
        }
    };
    Ok(pattern)
}

fn remote_use_graph_op_name(op: &PlanOp) -> &'static str {
    match op {
        PlanOp::NodeScan { .. } => "NODE SCAN",
        PlanOp::IndexScan { .. } => "INDEX SCAN",
        PlanOp::EdgeIndexScan { .. } => "EDGE INDEX SCAN",
        PlanOp::EdgeBindEndpoints { .. } => "EDGE BIND ENDPOINTS",
        PlanOp::ConditionalIndexScan { .. } => "CONDITIONAL INDEX SCAN",
        PlanOp::PropertyFilter { .. } => "PROPERTY FILTER",
        PlanOp::Expand { .. } => "EXPAND",
        PlanOp::ExpandFilter { .. } => "EXPAND FILTER",
        PlanOp::ShortestPath { .. } => "SHORTEST PATH",
        PlanOp::Let { .. } => "LET",
        PlanOp::For { .. } => "FOR",
        PlanOp::Filter { .. } => "FILTER",
        PlanOp::CallProcedure { .. } => "CALL",
        PlanOp::InlineProcedureCall { .. } => "INLINE CALL",
        PlanOp::UseGraph { .. } => "USE GRAPH",
        PlanOp::HashJoin { .. } => "HASH JOIN",
        PlanOp::CartesianProduct { .. } => "CARTESIAN PRODUCT",
        PlanOp::Aggregate { .. } => "AGGREGATE",
        PlanOp::Project { .. } => "PROJECT",
        PlanOp::Sort { .. } => "SORT",
        PlanOp::Limit { .. } => "LIMIT",
        PlanOp::SetOperation { .. } => "SET OPERATION",
        PlanOp::TopK { .. } => "TOPK",
        PlanOp::OptionalMatch { .. } => "OPTIONAL MATCH",
        PlanOp::IndexIntersection { .. } => "INDEX INTERSECTION",
        PlanOp::WorstCaseOptimalJoin { .. } => "WORST-CASE OPTIMAL JOIN",
        PlanOp::Materialize { .. } => "MATERIALIZE",
        PlanOp::InsertVertex { .. } => "INSERT VERTEX",
        PlanOp::InsertEdge { .. } => "INSERT EDGE",
        PlanOp::SetProperties { .. } => "SET PROPERTIES",
        PlanOp::RemoveProperties { .. } => "REMOVE PROPERTIES",
        PlanOp::DeleteVertex { .. } => "DELETE VERTEX",
        PlanOp::DetachDeleteVertex { .. } => "DETACH DELETE VERTEX",
        PlanOp::DeleteEdge { .. } => "DELETE EDGE",
    }
}

fn render_literal(value: &Value) -> ExecutionResultExt<String> {
    match value {
        Value::Null => Ok("NULL".to_owned()),
        Value::Bool(v) => Ok(if *v { "TRUE" } else { "FALSE" }.to_owned()),
        Value::Int8(v) => Ok(v.to_string()),
        Value::Int16(v) => Ok(v.to_string()),
        Value::Int32(v) => Ok(v.to_string()),
        Value::Int64(v) => Ok(v.to_string()),
        Value::Int128(v) => Ok(v.to_string()),
        Value::Uint8(v) => Ok(v.to_string()),
        Value::Uint16(v) => Ok(v.to_string()),
        Value::Uint32(v) => Ok(v.to_string()),
        Value::Uint64(v) => Ok(v.to_string()),
        Value::Uint128(v) => Ok(v.to_string()),
        Value::Float16(v) => Ok(f32::from(*v).to_string()),
        Value::Float32(v) => Ok(v.to_string()),
        Value::Float64(v) => Ok(v.to_string()),
        Value::Text(s) => Ok(format!("'{}'", s.replace('\'', "''"))),
        _ => Err(ExecutionError::InvalidPlan(
            "remote USE graph currently supports scalar literal args only".to_owned(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{BinaryOp, CmpOp};
    use gleaph_gql_planner::plan::ProjectColumn;

    #[test]
    fn render_expr_supports_basic_operators() {
        let expr = Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::BinaryOp {
                left: Box::new(Expr::new(ExprKind::Variable("n.age".to_owned()))),
                op: BinaryOp::Add,
                right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(1)))),
            })),
            op: CmpOp::Gt,
            right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(20)))),
        });
        let rendered = render_expr(&expr).expect("render");
        assert!(rendered.contains("+"));
        assert!(rendered.contains(">"));
    }

    #[test]
    fn subplan_to_routed_query_accepts_non_alias_project_expr() {
        let plan = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![Expr::new(ExprKind::Literal(Value::Int64(1)))],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("label".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];
        let query = subplan_to_routed_query(&plan).expect("query");
        assert!(query.contains("CALL db.labels(1)"));
        assert!(query.contains("RETURN label"));
    }

    #[test]
    fn subplan_to_routed_query_optional_call_prefix() {
        let plan = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: true,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("label".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];
        let query = subplan_to_routed_query(&plan).expect("query");
        assert!(query.starts_with("OPTIONAL CALL db.labels()"));
    }

    #[test]
    fn subplan_to_routed_query_supports_filter_and_limit() {
        let plan = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Filter {
                condition: Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::Variable("label".to_owned()))),
                    op: CmpOp::Ne,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Text("x".to_owned())))),
                }),
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("label".to_owned())),
                    alias: Some("label".into()),
                }],
                distinct: true,
            },
            PlanOp::Limit {
                count: Some(Expr::new(ExprKind::Literal(Value::Int64(5)))),
                offset: Some(Expr::new(ExprKind::Literal(Value::Int64(2)))),
            },
        ];
        let query = subplan_to_routed_query(&plan).expect("query");
        assert!(query.contains("WHERE label <> 'x'"));
        assert!(query.contains("RETURN DISTINCT label"));
        assert!(query.contains("LIMIT 5 OFFSET 2"));
    }

    #[test]
    fn subplan_to_routed_query_supports_sort() {
        let plan = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("label".to_owned())),
                    alias: Some("label".into()),
                }],
                distinct: false,
            },
            PlanOp::Sort {
                order_by: gleaph_gql::ast::OrderByClause {
                    span: gleaph_gql::token::Span::DUMMY,
                    items: vec![gleaph_gql::ast::SortItem {
                        span: gleaph_gql::token::Span::DUMMY,
                        expr: Expr::new(ExprKind::Variable("label".to_owned())),
                        direction: Some(SortDirection::Desc),
                        null_order: Some(NullOrder::Last),
                    }],
                },
            },
        ];
        let query = subplan_to_routed_query(&plan).expect("query");
        assert!(query.contains("ORDER BY label DESC NULLS LAST"));
    }

    #[test]
    fn subplan_to_routed_query_supports_topk() {
        let plan = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("label".to_owned())),
                    alias: Some("label".into()),
                }],
                distinct: false,
            },
            PlanOp::TopK {
                order_by: gleaph_gql::ast::OrderByClause {
                    span: gleaph_gql::token::Span::DUMMY,
                    items: vec![gleaph_gql::ast::SortItem {
                        span: gleaph_gql::token::Span::DUMMY,
                        expr: Expr::new(ExprKind::Variable("label".to_owned())),
                        direction: Some(SortDirection::Desc),
                        null_order: None,
                    }],
                },
                k: Expr::new(ExprKind::Literal(Value::Int64(3))),
                offset: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
            },
        ];
        let query = subplan_to_routed_query(&plan).expect("query");
        assert!(query.contains("ORDER BY label DESC"));
        assert!(query.contains("LIMIT 3 OFFSET 1"));
    }

    #[test]
    fn subplan_to_routed_query_supports_aggregate_alias_expansion() {
        let plan = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![gleaph_gql_planner::plan::AggregateSpec {
                    func: "count".into(),
                    expr: Some(Expr::new(ExprKind::Variable("label".to_owned()))),
                    distinct: true,
                    alias: Some("cnt".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("cnt".to_owned())),
                    alias: Some("total".into()),
                }],
                distinct: false,
            },
        ];
        let query = subplan_to_routed_query(&plan).expect("query");
        assert!(query.contains("RETURN count(DISTINCT label) AS total"));
    }

    #[test]
    fn subplan_to_routed_query_validates_group_by_projection() {
        let valid = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Aggregate {
                group_by: vec![Expr::new(ExprKind::Variable("label".to_owned()))],
                aggregates: vec![gleaph_gql_planner::plan::AggregateSpec {
                    func: "count".into(),
                    expr: None,
                    distinct: false,
                    alias: Some("cnt".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![
                    ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("label".to_owned())),
                        alias: Some("label".into()),
                    },
                    ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("cnt".to_owned())),
                        alias: Some("cnt".into()),
                    },
                ],
                distinct: false,
            },
        ];
        let valid_query = subplan_to_routed_query(&valid).expect("valid grouped projection");
        assert!(valid_query.contains("label AS label"));
        assert!(valid_query.contains("count(*) AS cnt"));

        let invalid = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![gleaph_gql_planner::plan::AggregateSpec {
                    func: "count".into(),
                    expr: None,
                    distinct: false,
                    alias: Some("cnt".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![
                    ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("label".to_owned())),
                        alias: Some("label".into()),
                    },
                    ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("cnt".to_owned())),
                        alias: Some("cnt".into()),
                    },
                ],
                distinct: false,
            },
        ];
        let err = subplan_to_routed_query(&invalid).expect_err("must reject ungrouped projection");
        assert!(matches!(
            err,
            ExecutionError::InvalidPlan(msg) if msg.contains("not grouped or aggregated")
        ));
    }

    #[test]
    fn subplan_to_routed_query_uses_having_after_aggregate() {
        let plan = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Aggregate {
                group_by: vec![Expr::new(ExprKind::Variable("label".to_owned()))],
                aggregates: vec![gleaph_gql_planner::plan::AggregateSpec {
                    func: "count".into(),
                    expr: None,
                    distinct: false,
                    alias: Some("cnt".into()),
                }],
            },
            PlanOp::Filter {
                condition: Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::Variable("cnt".to_owned()))),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(1)))),
                }),
            },
            PlanOp::Project {
                columns: vec![
                    ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("label".to_owned())),
                        alias: Some("label".into()),
                    },
                    ProjectColumn {
                        expr: Expr::new(ExprKind::Variable("cnt".to_owned())),
                        alias: Some("cnt".into()),
                    },
                ],
                distinct: false,
            },
        ];
        let query = subplan_to_routed_query(&plan).expect("query");
        assert!(query.contains("GROUP BY label"));
        assert!(query.contains("HAVING count(*) > 1"));
        assert!(!query.contains("WHERE count(*) > 1"));
    }

    #[test]
    fn subplan_to_routed_query_expands_aggregate_alias_in_order_by() {
        let plan = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![gleaph_gql_planner::plan::AggregateSpec {
                    func: "count".into(),
                    expr: None,
                    distinct: false,
                    alias: Some("cnt".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("cnt".to_owned())),
                    alias: Some("cnt".into()),
                }],
                distinct: false,
            },
            PlanOp::Sort {
                order_by: gleaph_gql::ast::OrderByClause {
                    span: gleaph_gql::token::Span::DUMMY,
                    items: vec![gleaph_gql::ast::SortItem {
                        span: gleaph_gql::token::Span::DUMMY,
                        expr: Expr::new(ExprKind::Variable("cnt".to_owned())),
                        direction: Some(SortDirection::Desc),
                        null_order: None,
                    }],
                },
            },
        ];
        let query = subplan_to_routed_query(&plan).expect("query");
        assert!(query.contains("ORDER BY count(*) DESC"));
    }

    #[test]
    fn render_expr_supports_aggregate_expr() {
        let agg = Expr::new(ExprKind::Aggregate {
            func: gleaph_gql::ast::AggregateFunc::CountStar,
            expr: None,
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        });
        let rendered = render_expr(&agg).expect("render agg");
        assert_eq!(rendered, "count(*)");
    }

    #[test]
    fn render_expr_supports_function_call_and_coalesce() {
        let func = Expr::new(ExprKind::FunctionCall {
            name: gleaph_gql::ast::ObjectName::qualified(vec!["math".to_owned(), "abs".to_owned()]),
            args: vec![Expr::new(ExprKind::Literal(Value::Int64(-7)))],
            distinct: false,
        });
        let coalesce = Expr::new(ExprKind::Coalesce(vec![
            Expr::new(ExprKind::Variable("n.name".to_owned())),
            Expr::new(ExprKind::Literal(Value::Text("unknown".to_owned()))),
        ]));
        let rendered_func = render_expr(&func).expect("render func");
        let rendered_coalesce = render_expr(&coalesce).expect("render coalesce");
        assert_eq!(rendered_func, "math.abs(-7)");
        assert!(rendered_coalesce.starts_with("COALESCE("));
        assert!(rendered_coalesce.contains("n.name"));
    }

    #[test]
    fn render_expr_supports_nullif_and_case() {
        let nullif_expr = Expr::new(ExprKind::NullIf(
            Box::new(Expr::new(ExprKind::Variable("n.kind".to_owned()))),
            Box::new(Expr::new(ExprKind::Literal(Value::Text("".to_owned())))),
        ));
        let case_simple = Expr::new(ExprKind::CaseSimple {
            operand: Box::new(Expr::new(ExprKind::Variable("n.score".to_owned()))),
            when_clauses: vec![gleaph_gql::ast::WhenClause {
                span: gleaph_gql::token::Span::DUMMY,
                condition: Expr::new(ExprKind::Literal(Value::Int64(10))),
                result: Expr::new(ExprKind::Literal(Value::Text("ten".to_owned()))),
            }],
            else_clause: Some(Box::new(Expr::new(ExprKind::Literal(Value::Text(
                "other".to_owned(),
            ))))),
        });
        let case_searched = Expr::new(ExprKind::CaseSearched {
            when_clauses: vec![gleaph_gql::ast::WhenClause {
                span: gleaph_gql::token::Span::DUMMY,
                condition: Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::Variable("n.score".to_owned()))),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(0)))),
                }),
                result: Expr::new(ExprKind::Literal(Value::Text("pos".to_owned()))),
            }],
            else_clause: None,
        });

        assert!(
            render_expr(&nullif_expr)
                .expect("nullif")
                .starts_with("NULLIF(")
        );
        assert!(
            render_expr(&case_simple)
                .expect("case simple")
                .contains("CASE n.score")
        );
        assert!(
            render_expr(&case_searched)
                .expect("case searched")
                .contains("WHEN n.score > 0 THEN")
        );
    }

    #[test]
    fn subplan_to_routed_query_reports_unsupported_first_op() {
        let plan = vec![
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: None,
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let err = subplan_to_routed_query(&plan).expect_err("unsupported first op must fail");
        assert!(matches!(
            err,
            ExecutionError::InvalidPlan(message)
                if message.contains("requires CALL, NODE SCAN, INDEX SCAN, or EDGE INDEX SCAN as first op")
                    && message.contains("EXPAND")
        ));
    }

    #[test]
    fn subplan_to_routed_query_supports_node_scan_root() {
        let plan = vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::Filter {
                condition: Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("n".to_owned()))),
                        property: "age".to_owned(),
                    })),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
                }),
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("n".to_owned()))),
                        property: "name".to_owned(),
                    }),
                    alias: Some("name".into()),
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("node scan query");
        assert!(query.starts_with("MATCH (n:User)"));
        assert!(query.contains("WHERE n.age > 18"));
        assert!(query.contains("RETURN n.name AS name"));
    }

    #[test]
    fn subplan_to_routed_query_supports_index_scan_root() {
        let plan = vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "uid".into(),
                value: ScanValue::Parameter("uid".into()),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("n".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("index scan query");
        assert!(query.starts_with("MATCH (n)"));
        assert!(query.contains("WHERE n.uid = $uid"));
        assert!(query.contains("RETURN n"));
    }

    #[test]
    fn subplan_to_routed_query_supports_multi_hop_index_scan_chain() {
        let plan = vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "uid".into(),
                value: ScanValue::Parameter("uid".into()),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::Expand {
                src: "n".into(),
                edge: "e1".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Expand {
                src: "b".into(),
                edge: "e2".into(),
                dst: "c".into(),
                direction: gleaph_gql::types::EdgeDirection::AnyDirection,
                label: Some("LIKES".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("c".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("index-scan multi-hop query");
        assert!(query.starts_with("MATCH (n)-[e1:KNOWS]->(b)-[e2:LIKES]-(c)"));
        assert!(query.contains("WHERE n.uid = $uid"));
        assert!(query.contains("RETURN c"));
    }

    #[test]
    fn subplan_to_routed_query_supports_multi_hop_index_scan_filter_chain() {
        let plan = vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "uid".into(),
                value: ScanValue::Parameter("uid".into()),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::ExpandFilter {
                src: "n".into(),
                edge: "e1".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                dst_filter: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("b".to_owned()))),
                        property: "age".to_owned(),
                    })),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
                })],
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Expand {
                src: "b".into(),
                edge: "e2".into(),
                dst: "c".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("LIKES".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("c".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("index-scan filter chain query");
        assert!(query.starts_with("MATCH (n)-[e1:KNOWS]->(b)-[e2:LIKES]->(c)"));
        assert!(query.contains("WHERE n.uid = $uid AND b.age > 18"));
        assert!(query.contains("RETURN c"));
    }

    #[test]
    fn subplan_to_routed_query_supports_edge_index_scan_root() {
        let plan = vec![
            PlanOp::EdgeIndexScan {
                variable: "e".into(),
                property: "weight".into(),
                value: ScanValue::Literal(Value::Int64(7)),
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: "e".into(),
                near: "__pending_src".into(),
                far: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("REL".into()),
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("edge index query");
        assert!(query.starts_with("MATCH (__pending_src)-[e:REL]->(b)"));
        assert!(query.contains("WHERE e.weight = 7"));
        assert!(query.contains("RETURN b"));
    }

    #[test]
    fn subplan_to_routed_query_supports_any_direction_edge_index_root() {
        let plan = vec![
            PlanOp::EdgeIndexScan {
                variable: "e".into(),
                property: "weight".into(),
                value: ScanValue::Literal(Value::Int64(7)),
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: "e".into(),
                near: "a".into(),
                far: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::AnyDirection,
                label: Some("REL".into()),
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("any-direction edge index query");
        assert!(query.starts_with("MATCH (a)-[e:REL]-(b)"));
        assert!(query.contains("WHERE e.weight = 7"));
        assert!(query.contains("RETURN b"));
    }

    #[test]
    fn subplan_to_routed_query_supports_left_or_right_edge_index_root() {
        let plan = vec![
            PlanOp::EdgeIndexScan {
                variable: "e".into(),
                property: "weight".into(),
                value: ScanValue::Literal(Value::Int64(7)),
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: "e".into(),
                near: "a".into(),
                far: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::LeftOrRight,
                label: Some("REL".into()),
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("left-or-right edge index query");
        assert!(query.starts_with("MATCH (a)<-[e:REL]->(b)"));
        assert!(query.contains("WHERE e.weight = 7"));
        assert!(query.contains("RETURN b"));
    }

    #[test]
    fn subplan_to_routed_query_supports_undirected_edge_index_root() {
        let plan = vec![
            PlanOp::EdgeIndexScan {
                variable: "e".into(),
                property: "weight".into(),
                value: ScanValue::Literal(Value::Int64(7)),
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: "e".into(),
                near: "a".into(),
                far: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::Undirected,
                label: Some("REL".into()),
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("undirected edge index query");
        assert!(query.starts_with("MATCH (a)~[e:REL]~(b)"));
        assert!(query.contains("WHERE e.weight = 7"));
        assert!(query.contains("RETURN b"));
    }

    #[test]
    fn subplan_to_routed_query_supports_multi_hop_edge_index_chain() {
        let plan = vec![
            PlanOp::EdgeIndexScan {
                variable: "e".into(),
                property: "weight".into(),
                value: ScanValue::Literal(Value::Int64(7)),
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: "e".into(),
                near: "a".into(),
                far: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("REL".into()),
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Expand {
                src: "b".into(),
                edge: "e2".into(),
                dst: "c".into(),
                direction: gleaph_gql::types::EdgeDirection::AnyDirection,
                label: Some("LIKES".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("c".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("edge-index multi-hop query");
        assert!(query.starts_with("MATCH (a)-[e:REL]->(b)-[e2:LIKES]-(c)"));
        assert!(query.contains("WHERE e.weight = 7"));
        assert!(query.contains("RETURN c"));
    }

    #[test]
    fn subplan_to_routed_query_supports_multi_hop_edge_index_filter_chain() {
        let plan = vec![
            PlanOp::EdgeIndexScan {
                variable: "e".into(),
                property: "weight".into(),
                value: ScanValue::Literal(Value::Int64(7)),
                property_projection: None,
            },
            PlanOp::EdgeBindEndpoints {
                edge: "e".into(),
                near: "a".into(),
                far: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("REL".into()),
                near_property_projection: None,
                far_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::ExpandFilter {
                src: "b".into(),
                edge: "e2".into(),
                dst: "c".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("LIKES".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                dst_filter: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("c".to_owned()))),
                        property: "score".to_owned(),
                    })),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(10)))),
                })],
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("c".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("edge-index filter chain query");
        assert!(query.starts_with("MATCH (a)-[e:REL]->(b)-[e2:LIKES]->(c)"));
        assert!(query.contains("WHERE e.weight = 7 AND c.score > 10"));
        assert!(query.contains("RETURN c"));
    }

    #[test]
    fn subplan_to_routed_query_rejects_edge_index_without_bind_endpoints() {
        let plan = vec![
            PlanOp::EdgeIndexScan {
                variable: "e".into(),
                property: "weight".into(),
                value: ScanValue::Literal(Value::Int64(7)),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("e".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let err =
            subplan_to_routed_query(&plan).expect_err("edge index root without bind must fail");
        assert!(matches!(
            err,
            ExecutionError::InvalidPlan(message)
                if message.contains("EDGE BIND ENDPOINTS immediately after EDGE INDEX SCAN")
        ));
    }

    #[test]
    fn subplan_to_routed_query_supports_single_hop_expand_from_node_scan() {
        let plan = vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("single hop expand query");
        assert!(query.starts_with("MATCH (a:User)-[e:KNOWS]->(b)"));
        assert!(query.contains("RETURN b"));
    }

    #[test]
    fn subplan_to_routed_query_supports_any_direction_single_hop_expand() {
        let plan = vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::AnyDirection,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("any-direction query");
        assert!(query.starts_with("MATCH (a:User)-[e:KNOWS]-(b)"));
        assert!(query.contains("RETURN b"));
    }

    #[test]
    fn subplan_to_routed_query_supports_left_or_right_single_hop_expand() {
        let plan = vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::LeftOrRight,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("left-or-right query");
        assert!(query.starts_with("MATCH (a:User)<-[e:KNOWS]->(b)"));
        assert!(query.contains("RETURN b"));
    }

    #[test]
    fn subplan_to_routed_query_supports_undirected_single_hop_expand() {
        let plan = vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::Undirected,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("undirected query");
        assert!(query.starts_with("MATCH (a:User)~[e:KNOWS]~(b)"));
        assert!(query.contains("RETURN b"));
    }

    #[test]
    fn subplan_to_routed_query_supports_multi_hop_expand_chain() {
        let plan = vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e1".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Expand {
                src: "b".into(),
                edge: "e2".into(),
                dst: "c".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("LIKES".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("c".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("multi-hop query");
        assert!(query.starts_with("MATCH (a:User)-[e1:KNOWS]->(b)-[e2:LIKES]->(c)"));
        assert!(query.contains("RETURN c"));
    }

    #[test]
    fn subplan_to_routed_query_supports_multi_hop_expand_filter_chain() {
        let plan = vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::ExpandFilter {
                src: "a".into(),
                edge: "e1".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                dst_filter: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(Expr::new(ExprKind::PropertyAccess {
                        expr: Box::new(Expr::new(ExprKind::Variable("b".to_owned()))),
                        property: "age".to_owned(),
                    })),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
                })],
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Expand {
                src: "b".into(),
                edge: "e2".into(),
                dst: "c".into(),
                direction: gleaph_gql::types::EdgeDirection::AnyDirection,
                label: Some("LIKES".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("c".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("multi-hop filter query");
        assert!(query.starts_with("MATCH (a:User)-[e1:KNOWS]->(b)-[e2:LIKES]-(c)"));
        assert!(query.contains("WHERE b.age > 18"));
        assert!(query.contains("RETURN c"));
    }

    #[test]
    fn subplan_to_routed_query_rejects_var_len_expand_pushdown() {
        let plan = vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: Some(gleaph_gql_planner::plan::VarLenSpec {
                    min: 1,
                    max: Some(2),
                }),
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let err = subplan_to_routed_query(&plan).expect_err("var-len expand must be rejected");
        assert!(matches!(
            err,
            ExecutionError::InvalidPlan(message)
                if message.contains("single-hop expand")
        ));
    }

    #[test]
    fn subplan_to_routed_query_accepts_trivial_var_len_one_one_expand() {
        let plan = vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("User".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: gleaph_gql::types::EdgeDirection::PointingRight,
                label: Some("KNOWS".into()),
                label_expr: None,
                var_len: Some(gleaph_gql_planner::plan::VarLenSpec {
                    min: 1,
                    max: Some(1),
                }),
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("b".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let query = subplan_to_routed_query(&plan).expect("trivial {1,1} var-len is single hop");
        assert!(query.starts_with("MATCH (a:User)-[e:KNOWS]->(b)"));
    }

    #[test]
    fn subplan_to_routed_query_reports_nested_use_graph_as_unsupported() {
        let plan = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::UseGraph {
                graph_name: vec!["other".into()],
                sub_plan: None,
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("label".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let err = subplan_to_routed_query(&plan).expect_err("nested USE GRAPH must fail");
        assert!(matches!(
            err,
            ExecutionError::InvalidPlan(message)
                if message.contains("cannot translate USE GRAPH after CALL")
        ));
    }

    #[test]
    fn subplan_to_routed_query_reports_join_as_unsupported() {
        let plan = vec![
            PlanOp::CallProcedure {
                name: vec!["db".into(), "labels".into()],
                args: vec![],
                yield_columns: Some(vec![gleaph_gql_planner::plan::YieldColumn {
                    name: "label".into(),
                    alias: None,
                }]),
                optional: false,
            },
            PlanOp::HashJoin {
                left: vec![],
                right: vec![],
                join_keys: vec!["label".into()],
            },
            PlanOp::Project {
                columns: vec![ProjectColumn {
                    expr: Expr::new(ExprKind::Variable("label".to_owned())),
                    alias: None,
                }],
                distinct: false,
            },
        ];

        let err = subplan_to_routed_query(&plan).expect_err("HASH JOIN must fail");
        assert!(matches!(
            err,
            ExecutionError::InvalidPlan(message)
                if message.contains("cannot translate HASH JOIN after CALL")
        ));
    }

    #[test]
    fn remote_param_sets_allow_multiple_scalar_rows() {
        let row1: BindingRow = [(
            std::rc::Rc::<str>::from("tenant"),
            BindingValue::Scalar(Value::Text("a".to_owned())),
        )]
        .into_iter()
        .collect();
        let row2: BindingRow = [(
            std::rc::Rc::<str>::from("tenant"),
            BindingValue::Scalar(Value::Text("b".to_owned())),
        )]
        .into_iter()
        .collect();

        let params = remote_param_sets_from_rows(&[row1, row2]).expect("scalar rows");
        assert_eq!(params.len(), 2);
        assert_eq!(
            params[0]
                .as_ref()
                .and_then(|row| row.get("tenant"))
                .map(Value::from),
            Some(Value::Text("a".to_owned()))
        );
        assert_eq!(
            params[1]
                .as_ref()
                .and_then(|row| row.get("tenant"))
                .map(Value::from),
            Some(Value::Text("b".to_owned()))
        );
    }

    #[test]
    fn remote_param_sets_reject_graph_bindings() {
        let row: BindingRow = [(
            std::rc::Rc::<str>::from("n"),
            BindingValue::Node(gleaph_graph_kernel::NodeRecord {
                id: gleaph_graph_kernel::NodeId::from(1_u32),
                labels: vec!["User".to_owned()],
                properties: BTreeMap::new(),
            }),
        )]
        .into_iter()
        .collect();

        let err = remote_param_sets_from_rows(&[row]).expect_err("node bindings must be rejected");
        assert!(matches!(
            err,
            ExecutionError::InvalidPlan(message)
                if message.contains("scalar bindings")
        ));
    }
}

impl GraphRegistryResolver for IcGraphRegistryResolver {
    fn resolve(
        &self,
        requested_graph: &str,
        caller: Option<&Value>,
    ) -> ExecutionResultExt<GraphResolution> {
        let _caller = extract_principal_caller(caller)?;
        let now = Self::cache_now_ns();
        {
            let guard = self.cache.read().map_err(|_| {
                ExecutionError::InvalidPlan("graph registry cache unavailable".to_owned())
            })?;
            if let Some(hit) = guard.get(requested_graph) {
                let fresh = match self.ttl_ns {
                    None => true,
                    Some(ttl) => now.saturating_sub(hit.fetched_at_ns) <= ttl,
                };
                if fresh {
                    return Ok(hit.resolution.clone());
                }
            }
        }
        let resolved = block_on(resolve_graph_via_canister(
            self.registry_canister_id,
            requested_graph,
        ))?;
        let mut guard = self.cache.write().map_err(|_| {
            ExecutionError::InvalidPlan("graph registry cache unavailable".to_owned())
        })?;
        guard.insert(
            requested_graph.to_owned(),
            CachedGraphResolution {
                resolution: resolved.clone(),
                fetched_at_ns: now,
            },
        );
        Ok(resolved)
    }
}

#[derive(Clone, Debug, CandidType, Deserialize)]
struct RegistryGraphResolution {
    graph_name: String,
    canister_id: Principal,
}

#[derive(Clone, Debug, CandidType, Deserialize)]
enum RegistryResolveError {
    NotFound(String),
    Conflict(String),
    Forbidden,
    InvalidName(String),
    Unavailable(String),
    ManagementError(String),
}

async fn resolve_graph_via_canister(
    registry_canister_id: Principal,
    graph_name: &str,
) -> ExecutionResultExt<GraphResolution> {
    let response: Result<RegistryGraphResolution, RegistryResolveError> =
        Call::bounded_wait(registry_canister_id, "resolve_graph")
            .with_arg(graph_name.to_owned())
            .await
            .map_err(|e| ExecutionError::InvalidPlan(format!("registry call failed: {e}")))?
            .candid()
            .map_err(|e| ExecutionError::InvalidPlan(format!("registry decode failed: {e}")))?;

    match response {
        Ok(resolved) => Ok(GraphResolution {
            graph_name: resolved.graph_name,
            canister_id: Some(resolved.canister_id.to_text()),
        }),
        Err(RegistryResolveError::NotFound(name)) => Err(ExecutionError::InvalidPlan(format!(
            "unknown graph in USE: {name}"
        ))),
        Err(RegistryResolveError::Conflict(name)) => Err(ExecutionError::InvalidPlan(format!(
            "conflicting graph mapping: {name}"
        ))),
        Err(RegistryResolveError::Forbidden) => Err(ExecutionError::InvalidPlan(
            "forbidden graph in USE".to_owned(),
        )),
        Err(RegistryResolveError::InvalidName(name)) => Err(ExecutionError::InvalidPlan(format!(
            "invalid graph name in USE: {name}"
        ))),
        Err(RegistryResolveError::Unavailable(msg)) => Err(ExecutionError::InvalidPlan(format!(
            "graph unavailable in USE: {msg}"
        ))),
        Err(RegistryResolveError::ManagementError(msg)) => Err(ExecutionError::InvalidPlan(
            format!("graph registry unavailable: {msg}"),
        )),
    }
}

fn extract_principal_caller(caller: Option<&Value>) -> ExecutionResultExt<Principal> {
    match caller {
        Some(Value::Extension(ext)) => ext.as_any().downcast_ref::<PrincipalValue>().map(|p| p.0),
        _ => None,
    }
    .ok_or(ExecutionError::InvalidPlan(
        "graph registry resolve requires principal caller".to_owned(),
    ))
}

fn map_registry_error(err: GraphRegistryError) -> ExecutionError {
    match err {
        GraphRegistryError::NotFound(name) => {
            ExecutionError::InvalidPlan(format!("unknown graph in USE: {name}"))
        }
        GraphRegistryError::Forbidden => {
            ExecutionError::InvalidPlan("forbidden graph in USE".to_owned())
        }
        GraphRegistryError::Conflict(name) => {
            ExecutionError::InvalidPlan(format!("conflicting graph mapping: {name}"))
        }
        GraphRegistryError::Unavailable => {
            ExecutionError::InvalidPlan("graph registry unavailable".to_owned())
        }
    }
}
