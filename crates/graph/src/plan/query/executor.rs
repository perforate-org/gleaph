use super::aggregate;
use super::error::PlanQueryError;
use super::sort_keys::compare_sort_keys;
use crate::facade::{EdgeHandle, GraphStore, canonical_undirected_owner};
use crate::index::lookup::PropertyIndexLookup;
use crate::plan::expr_evaluator::{
    eval_and_expr, eval_binary_expr, eval_compare_expr, eval_concat_expr, eval_not_expr,
    eval_or_expr, eval_unary_expr, eval_xor_expr, truthy,
};
use gleaph_gql::ast::{CmpOp, Expr, ExprKind, OrderByClause, TruthValue};
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql::{Value, hash_value_for_join};
use gleaph_gql_planner::plan::{
    AggregateSpec, ConditionalScanCandidate, IndexScanSpec, PhysicalPlan, PlanOp, ProjectColumn,
    ScanValue, Str,
};
use gleaph_graph_kernel::entry::{Edge, LabelId};
use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest, value_to_index_key_bytes};
use ic_stable_lara::VertexId;
use ic_stable_lara::traits::CsrVertexTombstone;
use nohash_hasher::IntMap;
use rapidhash::fast::RapidHasher;
use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::hash::Hasher;
use std::pin::Pin;

#[cfg(not(target_family = "wasm"))]
pub trait PlanQueryExecutor {
    fn execute_plan_query(
        &self,
        plan: &PhysicalPlan,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<PlanQueryResult, PlanQueryError>;
}

#[cfg(not(target_family = "wasm"))]
impl PlanQueryExecutor for GraphStore {
    fn execute_plan_query(
        &self,
        plan: &PhysicalPlan,
        parameters: &BTreeMap<String, Value>,
    ) -> Result<PlanQueryResult, PlanQueryError> {
        pollster::block_on(execute_plan_query(self, plan, parameters, None))
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct PlanQueryResult {
    pub rows: Vec<BTreeMap<String, Value>>,
}

#[derive(Clone, Debug, PartialEq)]
pub enum PlanBinding {
    Vertex(VertexId),
    Edge(EdgeHandle),
    Value(Value),
}

pub(crate) type PlanRow = BTreeMap<String, PlanBinding>;

/// Bindings for each hash-join key column (planner order), used for equality and hashing.
type HashJoinKey = Vec<PlanBinding>;

/// Left subplan rows that share the same exact [`HashJoinKey`] within one hash bucket.
type HashJoinBucketEntry = (HashJoinKey, Vec<PlanRow>);

type HashJoinBuckets = IntMap<u64, Vec<HashJoinBucketEntry>>;

pub async fn execute_plan_query(
    store: &GraphStore,
    plan: &PhysicalPlan,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
) -> Result<PlanQueryResult, PlanQueryError> {
    let rows = execute_ops(store, &plan.ops, parameters, index).await?;
    Ok(PlanQueryResult {
        rows: rows
            .iter()
            .map(|row| value_row(store, row))
            .collect::<Result<Vec<_>, _>>()?,
    })
}

async fn execute_ops(
    store: &GraphStore,
    ops: &[PlanOp],
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    execute_ops_from(store, ops, parameters, vec![PlanRow::new()], index).await
}

/// Variables that operators in `ops` may bind (used to NULL-pad `OptionalMatch` miss rows).
///
/// Downstream note: padded graph variables use [`PlanBinding::Value`]`(Value::Null)`; mandatory
/// [`Expand`] on such a variable still fails in [`vertex_binding`] until semantics define row drop
/// or optional chaining.
fn subplan_written_vars(ops: &[PlanOp]) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for op in ops {
        extend_subplan_written_vars_from_op(op, &mut out);
    }
    out
}

fn extend_subplan_written_vars_from_op(op: &PlanOp, out: &mut BTreeSet<String>) {
    match op {
        PlanOp::NodeScan { variable, .. }
        | PlanOp::IndexScan { variable, .. }
        | PlanOp::EdgeIndexScan { variable, .. }
        | PlanOp::IndexIntersection { variable, .. } => {
            out.insert(variable.to_string());
        }
        PlanOp::ConditionalIndexScan {
            candidates,
            fallback_variable,
            ..
        } => {
            out.insert(fallback_variable.to_string());
            for c in candidates {
                out.insert(c.variable.to_string());
            }
        }
        PlanOp::EdgeBindEndpoints {
            edge,
            near,
            far,
            hop_aux_binding,
            ..
        } => {
            out.insert(edge.to_string());
            out.insert(near.to_string());
            out.insert(far.to_string());
            if let Some(h) = hop_aux_binding {
                out.insert(h.to_string());
            }
        }
        PlanOp::Expand {
            edge,
            dst,
            hop_aux_binding,
            ..
        }
        | PlanOp::ExpandFilter {
            edge,
            dst,
            hop_aux_binding,
            ..
        } => {
            out.insert(edge.to_string());
            out.insert(dst.to_string());
            if let Some(h) = hop_aux_binding {
                out.insert(h.to_string());
            }
        }
        PlanOp::ShortestPath { edge, path_var, .. } => {
            out.insert(edge.to_string());
            if let Some(p) = path_var {
                out.insert(p.to_string());
            }
        }
        PlanOp::Let { bindings } => {
            for b in bindings {
                out.insert(b.variable.clone());
            }
        }
        PlanOp::For {
            variable,
            ordinality,
            ..
        } => {
            out.insert(variable.to_string());
            if let Some(o) = ordinality {
                out.insert(o.to_string());
            }
        }
        PlanOp::WorstCaseOptimalJoin { variables, .. } => {
            for v in variables {
                out.insert(v.to_string());
            }
        }
        PlanOp::OptionalMatch { sub_plan }
        | PlanOp::UseGraph {
            sub_plan: Some(sub_plan),
            ..
        } => {
            for child in sub_plan {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::UseGraph { sub_plan: None, .. } => {}
        PlanOp::HashJoin { left, right, .. } | PlanOp::CartesianProduct { left, right } => {
            for child in left {
                extend_subplan_written_vars_from_op(child, out);
            }
            for child in right {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            for child in &sub_plan.ops {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::SetOperation { right, .. } => {
            for child in &right.ops {
                extend_subplan_written_vars_from_op(child, out);
            }
        }
        PlanOp::InsertVertex { variable, .. } => {
            if let Some(v) = variable {
                out.insert(v.to_string());
            }
        }
        PlanOp::InsertEdge { variable, .. } => {
            if let Some(v) = variable {
                out.insert(v.to_string());
            }
        }
        PlanOp::PropertyFilter { .. }
        | PlanOp::Filter { .. }
        | PlanOp::CallProcedure { .. }
        | PlanOp::Aggregate { .. }
        | PlanOp::Project { .. }
        | PlanOp::Sort { .. }
        | PlanOp::Limit { .. }
        | PlanOp::TopK { .. }
        | PlanOp::Materialize { .. }
        | PlanOp::SetProperties { .. }
        | PlanOp::RemoveProperties { .. }
        | PlanOp::DeleteVertex { .. }
        | PlanOp::DetachDeleteVertex { .. }
        | PlanOp::DeleteEdge { .. } => {}
    }
}

async fn execute_optional_match(
    store: &GraphStore,
    parameters: &BTreeMap<String, Value>,
    rows: Vec<PlanRow>,
    sub_plan: &[PlanOp],
    written: &BTreeSet<String>,
    index: Option<&dyn PropertyIndexLookup>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut out = Vec::new();
    for row in rows {
        let extended =
            execute_ops_from(store, sub_plan, parameters, vec![row.clone()], index).await?;
        if extended.is_empty() {
            let mut padded = row;
            for v in written {
                if !padded.contains_key(v) {
                    padded.insert(v.clone(), PlanBinding::Value(Value::Null));
                }
            }
            out.push(padded);
        } else {
            out.extend(extended);
        }
    }
    Ok(out)
}

fn execute_ops_from<'a>(
    store: &'a GraphStore,
    ops: &'a [PlanOp],
    parameters: &'a BTreeMap<String, Value>,
    initial_rows: Vec<PlanRow>,
    index: Option<&'a dyn PropertyIndexLookup>,
) -> Pin<Box<dyn std::future::Future<Output = Result<Vec<PlanRow>, PlanQueryError>> + 'a>> {
    Box::pin(async move {
        let mut rows = initial_rows;
        // Index of the nearest preceding `PlanOp::Aggregate` for resolving
        // `ExprKind::Aggregate` in post-aggregate ops (e.g. `HAVING`).
        let mut active_aggregate_op_idx: Option<usize> = None;

        for (op_idx, op) in ops.iter().enumerate() {
            let aggregate_specs = active_aggregate_op_idx.and_then(|idx| match &ops[idx] {
                PlanOp::Aggregate { aggregates, .. } => Some(aggregates.as_slice()),
                _ => None,
            });
            let evaluator = QueryExprEvaluator {
                store,
                parameters,
                aggregate_specs,
            };
            rows = match op {
                PlanOp::NodeScan {
                    variable,
                    label,
                    property_projection: _,
                } => execute_node_scan(store, rows, variable, label.as_ref())?,
                PlanOp::IndexScan {
                    variable,
                    property,
                    value,
                    cmp,
                    property_projection: _,
                } => {
                    execute_index_scan(
                        store,
                        rows,
                        parameters,
                        index,
                        variable.as_ref(),
                        property.as_ref(),
                        value,
                        *cmp,
                    )
                    .await?
                }
                PlanOp::ConditionalIndexScan {
                    candidates,
                    fallback_label,
                    fallback_variable,
                    property_projection: _,
                } => {
                    execute_conditional_index_scan(
                        store,
                        rows,
                        parameters,
                        index,
                        candidates,
                        fallback_label.as_ref(),
                        &fallback_variable,
                    )
                    .await?
                }
                PlanOp::IndexIntersection {
                    variable,
                    scans,
                    property_projection: _,
                } => {
                    execute_index_intersection(
                        store,
                        rows,
                        parameters,
                        index,
                        variable.as_ref(),
                        scans,
                    )
                    .await?
                }
                PlanOp::PropertyFilter { predicates, .. } => rows
                    .into_iter()
                    .filter_map(|row| match row_matches_all(&evaluator, &row, predicates) {
                        Ok(true) => Some(Ok(row)),
                        Ok(false) => None,
                        Err(err) => Some(Err(err)),
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                PlanOp::Let { bindings } => rows
                    .into_iter()
                    .map(|mut row| -> Result<PlanRow, PlanQueryError> {
                        for binding in bindings {
                            let value = evaluator.eval_expr(&row, &binding.value)?;
                            row.insert(binding.variable.clone(), PlanBinding::Value(value));
                        }
                        Ok(row)
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                PlanOp::Filter { condition } => rows
                    .into_iter()
                    .filter_map(|row| {
                        match row_matches_all(&evaluator, &row, std::slice::from_ref(condition)) {
                            Ok(true) => Some(Ok(row)),
                            Ok(false) => None,
                            Err(err) => Some(Err(err)),
                        }
                    })
                    .collect::<Result<Vec<_>, _>>()?,
                PlanOp::Expand {
                    src,
                    edge,
                    dst,
                    direction,
                    label,
                    label_expr,
                    var_len,
                    indexed_edge_equality,
                    edge_property_projection: _,
                    dst_property_projection: _,
                    hop_aux_binding,
                } => {
                    ensure_simple_expand(
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        hop_aux_binding,
                    )?;
                    execute_expand(
                        store,
                        rows,
                        parameters,
                        src,
                        edge,
                        dst,
                        *direction,
                        label.as_ref(),
                        &[],
                    )?
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
                    edge_property_projection: _,
                    dst_property_projection: _,
                    hop_aux_binding,
                } => {
                    ensure_simple_expand(
                        label_expr,
                        var_len,
                        indexed_edge_equality,
                        hop_aux_binding,
                    )?;
                    execute_expand(
                        store,
                        rows,
                        parameters,
                        src,
                        edge,
                        dst,
                        *direction,
                        label.as_ref(),
                        dst_filter,
                    )?
                }
                PlanOp::Aggregate {
                    group_by,
                    aggregates,
                } => {
                    let agg_evaluator = QueryExprEvaluator {
                        store,
                        parameters,
                        aggregate_specs: None,
                    };
                    let out =
                        aggregate::execute_aggregate(rows, group_by, aggregates, &agg_evaluator)?;
                    active_aggregate_op_idx = Some(op_idx);
                    out
                }
                PlanOp::Project { columns, distinct } => {
                    let proj_evaluator = QueryExprEvaluator {
                        store,
                        parameters,
                        aggregate_specs,
                    };
                    let mut projected = rows
                        .iter()
                        .map(|row| project_row(&proj_evaluator, row, columns))
                        .collect::<Result<Vec<_>, _>>()?;
                    if *distinct {
                        dedup_rows(&mut projected);
                    }
                    active_aggregate_op_idx = None;
                    projected
                }
                PlanOp::Limit { count, offset } => {
                    let offset = match offset {
                        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
                        None => 0,
                    };
                    let count = match count {
                        Some(expr) => {
                            Some(limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?)
                        }
                        None => None,
                    };
                    rows.into_iter()
                        .skip(offset)
                        .take(count.unwrap_or(usize::MAX))
                        .collect()
                }
                PlanOp::Sort { order_by } => sort_rows(&evaluator, rows, order_by)?,
                PlanOp::TopK {
                    order_by,
                    k,
                    offset,
                } => {
                    let offset = match offset {
                        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
                        None => 0,
                    };
                    let k = limit_value(&evaluator.eval_expr(&PlanRow::new(), k)?)?;
                    sort_rows(&evaluator, rows, order_by)?
                        .into_iter()
                        .skip(offset)
                        .take(k)
                        .collect()
                }
                PlanOp::Materialize { columns, distinct } => {
                    let mut materialized = rows
                        .iter()
                        .map(|row| project_row(&evaluator, row, columns))
                        .collect::<Result<Vec<_>, _>>()?;
                    if *distinct {
                        dedup_rows(&mut materialized);
                    }
                    materialized
                }
                PlanOp::UseGraph {
                    graph_name: _,
                    sub_plan: Some(sub_plan),
                } => {
                    // v1 has a single physical GraphStore; USE scopes its sub-plan
                    // but does not route to a separate graph store yet.
                    execute_ops_from(store, sub_plan, parameters, rows, index).await?
                }
                PlanOp::UseGraph {
                    graph_name: _,
                    sub_plan: None,
                } => {
                    // Same single-store v1 behavior: a bare USE marker is metadata.
                    rows
                }
                PlanOp::CartesianProduct { left, right } => {
                    execute_cartesian_product(store, parameters, rows, left, right, index).await?
                }
                PlanOp::HashJoin {
                    left,
                    right,
                    join_keys,
                } => {
                    execute_hash_join(store, parameters, rows, left, right, join_keys, index)
                        .await?
                }
                PlanOp::OptionalMatch { sub_plan } => {
                    let written = subplan_written_vars(sub_plan);
                    execute_optional_match(store, parameters, rows, sub_plan, &written, index)
                        .await?
                }
                other if other.is_dml() => {
                    return Err(PlanQueryError::UnsupportedOp(plan_op_name(other)));
                }
                other => return Err(PlanQueryError::UnsupportedOp(plan_op_name(other))),
            };
        }

        Ok(rows)
    })
}

async fn execute_cartesian_product(
    store: &GraphStore,
    parameters: &BTreeMap<String, Value>,
    rows: Vec<PlanRow>,
    left: &[PlanOp],
    right: &[PlanOp],
    index: Option<&dyn PropertyIndexLookup>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut out = Vec::new();
    for row in rows {
        let left_rows = execute_ops_from(store, left, parameters, vec![row.clone()], index).await?;
        let right_rows = execute_ops_from(store, right, parameters, vec![row], index).await?;
        for left_row in &left_rows {
            for right_row in &right_rows {
                if let Some(merged) = merge_rows(left_row, right_row) {
                    out.push(merged);
                }
            }
        }
    }
    Ok(out)
}

fn merge_rows(left: &PlanRow, right: &PlanRow) -> Option<PlanRow> {
    let mut merged = left.clone();
    for (name, right_binding) in right {
        match merged.get(name) {
            Some(left_binding) if left_binding != right_binding => return None,
            Some(_) => {}
            None => {
                merged.insert(name.clone(), right_binding.clone());
            }
        }
    }
    Some(merged)
}

async fn execute_hash_join(
    store: &GraphStore,
    parameters: &BTreeMap<String, Value>,
    rows: Vec<PlanRow>,
    left: &[PlanOp],
    right: &[PlanOp],
    join_keys: &[Str],
    index: Option<&dyn PropertyIndexLookup>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    if join_keys.is_empty() {
        return Err(PlanQueryError::UnsupportedOp("HashJoin(empty join_keys)"));
    }

    let mut out = Vec::new();
    for row in rows {
        let left_rows = execute_ops_from(store, left, parameters, vec![row.clone()], index).await?;
        let right_rows = execute_ops_from(store, right, parameters, vec![row], index).await?;

        let mut buckets: HashJoinBuckets = IntMap::default();
        for lr in left_rows {
            let key = extract_join_key(&lr, join_keys)?;
            insert_join_bucket(&mut buckets, key, lr);
        }

        for rr in right_rows {
            let key = extract_join_key(&rr, join_keys)?;
            let h = hash_join_mix(&key);
            let Some(bucket) = buckets.get(&h) else {
                continue;
            };
            for (left_key, left_matches) in bucket {
                if left_key == &key {
                    for lr in left_matches {
                        if let Some(merged) = merge_rows(lr, &rr) {
                            out.push(merged);
                        }
                    }
                }
            }
        }
    }
    Ok(out)
}

fn extract_join_key(row: &PlanRow, join_keys: &[Str]) -> Result<HashJoinKey, PlanQueryError> {
    join_keys
        .iter()
        .map(|k| {
            row.get(k.as_ref())
                .cloned()
                .ok_or_else(|| PlanQueryError::MissingBinding {
                    variable: k.as_ref().to_owned(),
                })
        })
        .collect()
}

fn insert_join_bucket(buckets: &mut HashJoinBuckets, key: HashJoinKey, row: PlanRow) {
    let h = hash_join_mix(&key);
    let bucket = buckets.entry(h).or_default();
    if let Some((_, rows)) = bucket.iter_mut().find(|(k, _)| k == &key) {
        rows.push(row);
    } else {
        bucket.push((key, vec![row]));
    }
}

/// Mix join-key bindings for hash buckets; must satisfy `a == b ⇒ mix(a) == mix(b)` for [`PlanBinding`].
fn hash_join_mix(bindings: &[PlanBinding]) -> u64 {
    let mut hasher = RapidHasher::default();
    for b in bindings {
        hash_plan_binding_for_join(b, &mut hasher);
    }
    hasher.finish()
}

fn hash_plan_binding_for_join(binding: &PlanBinding, hasher: &mut RapidHasher<'_>) {
    match binding {
        PlanBinding::Vertex(v) => {
            hasher.write_u8(1);
            hasher.write_u32(u32::from(*v));
        }
        PlanBinding::Edge(e) => {
            hasher.write_u8(2);
            hasher.write_u32(u32::from(e.owner_vertex_id));
            hasher.write_u32(e.vertex_edge_id.raw());
        }
        PlanBinding::Value(v) => {
            hasher.write_u8(3);
            hash_value_for_join(v, hasher);
        }
    }
}

fn property_id_for_scan(store: &GraphStore, property_name: &str) -> Result<u32, PlanQueryError> {
    store
        .property_id(property_name)
        .map(|p| p.raw())
        .ok_or(PlanQueryError::UnsupportedOp("IndexScan.unknown_property"))
}

fn resolve_scan_value_bytes(
    sv: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<Option<Vec<u8>>, PlanQueryError> {
    let v = match sv {
        ScanValue::Literal(val) => val.clone(),
        ScanValue::Parameter(name) => parameters.get(name.as_ref()).cloned().ok_or_else(|| {
            PlanQueryError::MissingParameter {
                name: name.to_string(),
            }
        })?,
    };
    value_to_index_key_bytes(&v).map_err(|_| PlanQueryError::InvalidExpressionValue {
        expression: "index scan value encoding".to_owned(),
    })
}

fn cmp_to_posting_range_request(
    cmp: CmpOp,
    bound_bytes: Vec<u8>,
) -> Result<PostingRangeRequest, PlanQueryError> {
    Ok(match cmp {
        CmpOp::Lt => PostingRangeRequest::Lt(bound_bytes),
        CmpOp::Le => PostingRangeRequest::Le(bound_bytes),
        CmpOp::Gt => PostingRangeRequest::Gt(bound_bytes),
        CmpOp::Ge => PostingRangeRequest::Ge(bound_bytes),
        CmpOp::Eq | CmpOp::Ne => {
            return Err(PlanQueryError::UnsupportedOp(
                "IndexScan.range(internal CmpOp)",
            ));
        }
    })
}

fn local_shard_filter_id() -> Result<u64, PlanQueryError> {
    GraphStore::new()
        .index_routing()
        .map(|r| r.shard_id)
        .ok_or(PlanQueryError::UnsupportedOp("IndexScan(no shard routing)"))
}

fn filter_hits_for_local_shard(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    variable: &str,
    hits: &[PostingHit],
    shard: u64,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut out = Vec::new();
    for row in rows {
        for h in hits {
            if h.shard_id != shard {
                continue;
            }
            let vid = VertexId::from_le_bytes(h.vertex_id.to_le_bytes());
            let Some(vertex) = store.vertex(vid) else {
                continue;
            };
            if vertex.is_tombstone() {
                continue;
            }
            let mut r = row.clone();
            r.insert(variable.to_string(), PlanBinding::Vertex(vid));
            out.push(r);
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
async fn execute_index_scan(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    variable: &str,
    property_name: &str,
    scan_value: &ScanValue,
    cmp: CmpOp,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let Some(ix) = index else {
        return Err(PlanQueryError::UnsupportedOp("IndexScan(no index client)"));
    };
    let shard = local_shard_filter_id()?;
    let pid = property_id_for_scan(store, property_name)?;
    let Some(bytes) = resolve_scan_value_bytes(scan_value, parameters)? else {
        return Ok(Vec::new());
    };
    let hits = if cmp == CmpOp::Eq {
        ix.lookup_equal(pid, bytes).await?
    } else {
        let req = cmp_to_posting_range_request(cmp, bytes)?;
        ix.lookup_range(pid, &req).await?
    };
    filter_hits_for_local_shard(store, rows, variable, &hits, shard)
}

async fn execute_conditional_index_scan(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    candidates: &[ConditionalScanCandidate],
    fallback_label: Option<&Str>,
    fallback_variable: &Str,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    for c in candidates {
        let pv = parameters
            .get(c.param_name.as_ref())
            .cloned()
            .unwrap_or(Value::Null);
        if pv != Value::Null {
            let Some(bytes) = value_to_index_key_bytes(&pv).ok().flatten() else {
                break;
            };
            let Some(ix) = index else {
                return Err(PlanQueryError::UnsupportedOp(
                    "ConditionalIndexScan(no index client)",
                ));
            };
            let shard = local_shard_filter_id()?;
            let pid = property_id_for_scan(store, c.property.as_ref())?;
            let hits = if c.cmp == CmpOp::Eq {
                ix.lookup_equal(pid, bytes).await?
            } else {
                let req = cmp_to_posting_range_request(c.cmp, bytes)?;
                ix.lookup_range(pid, &req).await?
            };
            return filter_hits_for_local_shard(store, rows, c.variable.as_ref(), &hits, shard);
        }
    }
    execute_node_scan(store, rows, fallback_variable, fallback_label)
}

async fn execute_index_intersection(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    index: Option<&dyn PropertyIndexLookup>,
    variable: &str,
    scans: &[IndexScanSpec],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let Some(ix) = index else {
        return Err(PlanQueryError::UnsupportedOp(
            "IndexIntersection(no index client)",
        ));
    };
    let shard = local_shard_filter_id()?;
    let mut sets: Vec<HashSet<VertexId>> = Vec::with_capacity(scans.len());
    for spec in scans {
        if spec.cmp != CmpOp::Eq {
            return Err(PlanQueryError::UnsupportedOp("IndexIntersection.cmp"));
        }
        let pid = property_id_for_scan(store, spec.property.as_ref())?;
        let Some(bytes) = resolve_scan_value_bytes(&spec.value, parameters)? else {
            return Ok(Vec::new());
        };
        let hits = ix.lookup_equal(pid, bytes).await?;
        let mut hs = HashSet::new();
        for h in hits {
            if h.shard_id != shard {
                continue;
            }
            let vid = VertexId::from_le_bytes(h.vertex_id.to_le_bytes());
            if let Some(vertex) = store.vertex(vid)
                && !vertex.is_tombstone()
            {
                hs.insert(vid);
            }
        }
        sets.push(hs);
    }
    let mut intersection: Option<HashSet<VertexId>> = None;
    for s in sets {
        intersection = Some(match intersection {
            None => s,
            Some(prev) => prev.intersection(&s).copied().collect(),
        });
    }
    let Some(ids) = intersection else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for row in rows {
        for vid in &ids {
            let mut r = row.clone();
            r.insert(variable.to_string(), PlanBinding::Vertex(*vid));
            out.push(r);
        }
    }
    Ok(out)
}

fn execute_node_scan(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    variable: &Str,
    label: Option<&Str>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = label.and_then(|label| store.label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for row in rows {
        for raw in 0..u32::from(store.vertex_count()) {
            let vertex_id = VertexId::from(raw);
            let Some(vertex) = store.vertex(vertex_id) else {
                continue;
            };
            if vertex.is_tombstone() {
                continue;
            }
            if let Some(filter) = label_id
                && !store.vertex_has_label(vertex_id, vertex, filter)
            {
                continue;
            }
            let mut row = row.clone();
            row.insert(variable.to_string(), PlanBinding::Vertex(vertex_id));
            out.push(row);
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn execute_expand(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    src: &Str,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label: Option<&Str>,
    dst_filter: &[Expr],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = label.and_then(|label| store.label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(Vec::new());
    }

    let evaluator = QueryExprEvaluator {
        store,
        parameters,
        aggregate_specs: None,
    };
    let mut out = Vec::new();
    for row in rows {
        let src_id = vertex_binding(&row, src)?;
        let candidates = expand_candidates(store, src_id, direction, label_id)?;
        for (dst_id, handle, _edge_record) in candidates {
            let mut expanded = row.clone();
            expanded.insert(edge.to_string(), PlanBinding::Edge(handle));
            expanded.insert(dst.to_string(), PlanBinding::Vertex(dst_id));
            if !row_matches_all(&evaluator, &expanded, dst_filter)? {
                continue;
            }
            out.push(expanded);
        }
    }
    Ok(out)
}

fn expand_candidates(
    store: &GraphStore,
    src_id: VertexId,
    direction: EdgeDirection,
    edge_label_id: Option<LabelId>,
) -> Result<Vec<(VertexId, EdgeHandle, Edge)>, PlanQueryError> {
    let mut out = Vec::new();
    match direction {
        EdgeDirection::PointingRight => {
            for edge in store
                .out_edges(src_id)
                .map_err(crate::facade::GraphStoreError::from)?
            {
                if edge.meta.is_undirected() {
                    continue;
                }
                if let Some(expected) = edge_label_id
                    && edge.meta.label_id() != expected.raw()
                {
                    continue;
                }
                out.push((
                    edge.target,
                    EdgeHandle {
                        owner_vertex_id: src_id,
                        vertex_edge_id: edge.vertex_edge_id,
                    },
                    edge,
                ));
            }
        }
        EdgeDirection::PointingLeft => {
            for edge in store
                .in_edges(src_id)
                .map_err(crate::facade::GraphStoreError::from)?
            {
                if edge.meta.is_undirected() {
                    continue;
                }
                if let Some(expected) = edge_label_id
                    && edge.meta.label_id() != expected.raw()
                {
                    continue;
                }
                out.push((
                    edge.target,
                    EdgeHandle {
                        owner_vertex_id: edge.target,
                        vertex_edge_id: edge.vertex_edge_id,
                    },
                    edge,
                ));
            }
        }
        EdgeDirection::Undirected => {
            for edge in store
                .out_edges(src_id)
                .map_err(crate::facade::GraphStoreError::from)?
            {
                if !edge.meta.is_undirected() {
                    continue;
                }
                if let Some(expected) = edge_label_id
                    && edge.meta.label_id() != expected.raw()
                {
                    continue;
                }
                out.push((
                    edge.target,
                    EdgeHandle {
                        owner_vertex_id: canonical_undirected_owner(src_id, edge.target),
                        vertex_edge_id: edge.vertex_edge_id,
                    },
                    edge,
                ));
            }
        }
        other => return Err(PlanQueryError::UnsupportedDirection(other)),
    }
    Ok(out)
}

fn ensure_simple_expand(
    label_expr: &Option<LabelExpr>,
    var_len: &Option<gleaph_gql_planner::plan::VarLenSpec>,
    indexed_edge_equality: &Option<(Str, gleaph_gql_planner::plan::ScanValue)>,
    hop_aux_binding: &Option<Str>,
) -> Result<(), PlanQueryError> {
    if label_expr.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.label_expr"));
    }
    if var_len.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.var_len"));
    }
    if indexed_edge_equality.is_some() {
        return Err(PlanQueryError::UnsupportedOp(
            "Expand.indexed_edge_equality",
        ));
    }
    if hop_aux_binding.is_some() {
        return Err(PlanQueryError::UnsupportedOp("Expand.hop_aux_binding"));
    }
    Ok(())
}

fn row_matches_all(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    predicates: &[Expr],
) -> Result<bool, PlanQueryError> {
    for predicate in predicates {
        let value = evaluator.eval_expr(row, predicate)?;
        if truthy(value).map_err(PlanQueryError::from)? != Some(true) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn sort_rows(
    evaluator: &QueryExprEvaluator<'_>,
    rows: Vec<PlanRow>,
    order_by: &OrderByClause,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let mut keyed_rows = rows
        .into_iter()
        .map(|row| {
            let keys = order_by
                .items
                .iter()
                .map(|item| eval_sort_expr(evaluator, &row, &item.expr))
                .collect::<Result<Vec<_>, _>>()?;
            Ok((keys, row))
        })
        .collect::<Result<Vec<_>, PlanQueryError>>()?;

    for left_idx in 0..keyed_rows.len() {
        for right_idx in (left_idx + 1)..keyed_rows.len() {
            compare_sort_keys(&keyed_rows[left_idx].0, &keyed_rows[right_idx].0, order_by)?;
        }
    }

    keyed_rows.sort_by(|(left_keys, _), (right_keys, _)| {
        compare_sort_keys(left_keys, right_keys, order_by)
            .expect("sort keys are pre-validated before sorting")
    });

    Ok(keyed_rows.into_iter().map(|(_, row)| row).collect())
}

fn eval_sort_expr(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    expr: &Expr,
) -> Result<Value, PlanQueryError> {
    match evaluator.eval_expr(row, expr) {
        Ok(value) => Ok(value),
        Err(PlanQueryError::MissingBinding { .. }) => {
            let projected_name = expression_name(expr);
            match row.get(&projected_name) {
                Some(PlanBinding::Value(value)) => Ok(value.clone()),
                Some(binding) => binding_to_value(evaluator.store, binding),
                None => Err(PlanQueryError::MissingBinding {
                    variable: projected_name,
                }),
            }
        }
        Err(err) => Err(err),
    }
}

struct QueryExprEvaluator<'a> {
    store: &'a GraphStore,
    parameters: &'a BTreeMap<String, Value>,
    /// When set, `ExprKind::Aggregate` reads precomputed results from the row
    /// (see [`aggregate_slot_key`]). Sourced from the active preceding
    /// [`PlanOp::Aggregate`] (not necessarily `ops[op_idx - 1]`, e.g. when `HAVING`
    /// inserts a [`PlanOp::Filter`] between aggregate and project).
    aggregate_specs: Option<&'a [AggregateSpec]>,
}

impl QueryExprEvaluator<'_> {
    fn eval_expr(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        match &expr.kind {
            ExprKind::Literal(value) => Ok(value.clone()),
            ExprKind::Paren(inner) => self.eval_expr(row, inner),
            ExprKind::Variable(name) => binding_to_value(
                self.store,
                row.get(name)
                    .ok_or_else(|| PlanQueryError::MissingBinding {
                        variable: name.clone(),
                    })?,
            ),
            ExprKind::Parameter(name) => self
                .parameters
                .get(name)
                .cloned()
                .ok_or_else(|| PlanQueryError::MissingParameter { name: name.clone() }),
            ExprKind::PropertyAccess { expr, property } => self.eval_property(row, expr, property),
            ExprKind::UnaryOp { op, expr } => {
                let value = self.eval_expr(row, expr)?;
                eval_unary_expr(*op, value).map_err(PlanQueryError::from)
            }
            ExprKind::BinaryOp { left, op, right } => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_binary_expr(left, *op, right).map_err(PlanQueryError::from)
            }
            ExprKind::Not(expr) => {
                let value = self.eval_expr(row, expr)?;
                eval_not_expr(value).map_err(PlanQueryError::from)
            }
            ExprKind::And(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_and_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Or(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_or_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Xor(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_xor_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Compare { left, op, right } => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_compare_expr(left, *op, right).map_err(PlanQueryError::from)
            }
            ExprKind::IsNull(expr) => Ok(Value::Bool(self.eval_expr(row, expr)? == Value::Null)),
            ExprKind::IsNotNull(expr) => Ok(Value::Bool(self.eval_expr(row, expr)? != Value::Null)),
            ExprKind::IsTruth {
                expr,
                value,
                negated,
            } => {
                let evaluated = self.eval_expr(row, expr)?;
                let matched = matches!(
                    (evaluated, *value),
                    (Value::Bool(true), TruthValue::True)
                        | (Value::Bool(false), TruthValue::False)
                        | (Value::Null, TruthValue::Unknown),
                );
                Ok(Value::Bool(if *negated { !matched } else { matched }))
            }
            ExprKind::Concat(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                eval_concat_expr(left, right).map_err(PlanQueryError::from)
            }
            ExprKind::Coalesce(exprs) => {
                for expr in exprs {
                    let value = self.eval_expr(row, expr)?;
                    if value != Value::Null {
                        return Ok(value);
                    }
                }
                Ok(Value::Null)
            }
            ExprKind::NullIf(left, right) => {
                let left = self.eval_expr(row, left)?;
                let right = self.eval_expr(row, right)?;
                if left == Value::Null || right == Value::Null {
                    return Ok(left);
                }
                let equal = eval_compare_expr(left.clone(), gleaph_gql::ast::CmpOp::Eq, right)
                    .map_err(PlanQueryError::from)?;
                if equal == Value::Bool(true) {
                    Ok(Value::Null)
                } else {
                    Ok(left)
                }
            }
            ExprKind::ListLiteral(items) | ExprKind::ListConstructor { items, .. } => items
                .iter()
                .map(|expr| self.eval_expr(row, expr))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::List),
            ExprKind::RecordLiteral(fields) | ExprKind::RecordConstructor(fields) => fields
                .iter()
                .map(|(name, expr)| self.eval_expr(row, expr).map(|value| (name.clone(), value)))
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Record),
            ExprKind::Aggregate { .. } => {
                let Some(specs) = self.aggregate_specs else {
                    return Err(PlanQueryError::UnsupportedExpression {
                        expression: "aggregate".to_owned(),
                    });
                };
                aggregate::resolve_aggregate_from_row(row, expr, specs)
            }
            _ => Err(PlanQueryError::UnsupportedExpression {
                expression: format!("{:?}", expr.kind),
            }),
        }
    }

    fn eval_property(
        &self,
        row: &PlanRow,
        expr: &Expr,
        property: &str,
    ) -> Result<Value, PlanQueryError> {
        if let ExprKind::Variable(name) = &expr.kind {
            return match row.get(name) {
                Some(PlanBinding::Vertex(vertex_id)) => self
                    .store
                    .property_id(property)
                    .and_then(|property_id| self.store.vertex_property(*vertex_id, property_id))
                    .map_or(Ok(Value::Null), Ok),
                Some(PlanBinding::Edge(edge)) => self
                    .store
                    .property_id(property)
                    .and_then(|property_id| {
                        self.store.edge_property(
                            edge.owner_vertex_id,
                            edge.vertex_edge_id,
                            property_id,
                        )
                    })
                    .map_or(Ok(Value::Null), Ok),
                Some(PlanBinding::Value(value)) => Ok(record_property(value, property)),
                None => Err(PlanQueryError::MissingBinding {
                    variable: name.clone(),
                }),
            };
        }

        let value = self.eval_expr(row, expr)?;
        Ok(record_property(&value, property))
    }
}

impl aggregate::PlanRowExprEval for QueryExprEvaluator<'_> {
    fn eval_expr_for_row(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        QueryExprEvaluator::eval_expr(self, row, expr)
    }

    fn eval_sort_key_for_row(&self, row: &PlanRow, expr: &Expr) -> Result<Value, PlanQueryError> {
        eval_sort_expr(self, row, expr)
    }
}

fn project_row(
    evaluator: &QueryExprEvaluator<'_>,
    row: &PlanRow,
    columns: &[ProjectColumn],
) -> Result<PlanRow, PlanQueryError> {
    if columns.is_empty() {
        return row
            .iter()
            .map(|(name, binding)| {
                binding_to_value(evaluator.store, binding)
                    .map(|value| (name.clone(), PlanBinding::Value(value)))
            })
            .collect();
    }

    columns
        .iter()
        .map(|column| {
            let name = column
                .alias
                .as_ref()
                .map(Str::to_string)
                .unwrap_or_else(|| expression_name(&column.expr));
            evaluator
                .eval_expr(row, &column.expr)
                .map(|value| (name, PlanBinding::Value(value)))
        })
        .collect()
}

fn expression_name(expr: &Expr) -> String {
    match &expr.kind {
        ExprKind::Variable(name) => name.clone(),
        ExprKind::PropertyAccess { expr, property } => {
            format!("{}.{}", expression_name(expr), property)
        }
        _ => "expr".to_owned(),
    }
}

fn value_row(store: &GraphStore, row: &PlanRow) -> Result<BTreeMap<String, Value>, PlanQueryError> {
    row.iter()
        .map(|(name, binding)| binding_to_value(store, binding).map(|value| (name.clone(), value)))
        .collect()
}

fn binding_to_value(store: &GraphStore, binding: &PlanBinding) -> Result<Value, PlanQueryError> {
    match binding {
        PlanBinding::Vertex(vertex_id) => vertex_to_value(store, *vertex_id),
        PlanBinding::Edge(edge) => edge_to_value(store, *edge),
        PlanBinding::Value(value) => Ok(value.clone()),
    }
}

fn vertex_to_value(store: &GraphStore, vertex_id: VertexId) -> Result<Value, PlanQueryError> {
    let vertex = store
        .vertex(vertex_id)
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: format!("vertex {vertex_id:?}"),
        })?;
    Ok(Value::Record(vec![
        ("id".to_owned(), Value::Uint64(u64::from(vertex_id))),
        (
            "labels".to_owned(),
            Value::List(
                store
                    .vertex_labels(vertex_id, vertex)
                    .into_iter()
                    .map(|label| {
                        store
                            .label_name(label)
                            .map(Value::Text)
                            .unwrap_or_else(|| Value::Uint64(u64::from(label.raw())))
                    })
                    .collect(),
            ),
        ),
        (
            "properties".to_owned(),
            properties_to_record(
                store
                    .vertex_properties(vertex_id)
                    .into_iter()
                    .map(|(property, value)| {
                        (store.property_name(property), property.raw(), value)
                    }),
            ),
        ),
    ]))
}

fn edge_to_value(store: &GraphStore, handle: EdgeHandle) -> Result<Value, PlanQueryError> {
    let edge = store
        .out_edges(handle.owner_vertex_id)
        .map_err(crate::facade::GraphStoreError::from)?
        .into_iter()
        .find(|edge| edge.vertex_edge_id == handle.vertex_edge_id)
        .ok_or_else(|| PlanQueryError::MissingBinding {
            variable: format!("edge {:?}", handle),
        })?;
    let label = LabelId::from_raw(edge.meta.label_id());
    Ok(Value::Record(vec![
        (
            "owner_vertex_id".to_owned(),
            Value::Uint64(u64::from(handle.owner_vertex_id)),
        ),
        (
            "vertex_edge_id".to_owned(),
            Value::Uint64(u64::from(handle.vertex_edge_id.raw())),
        ),
        (
            "label".to_owned(),
            if label.raw() == 0 {
                Value::Null
            } else {
                store
                    .label_name(label)
                    .map(Value::Text)
                    .unwrap_or(Value::Null)
            },
        ),
        (
            "undirected".to_owned(),
            Value::Bool(edge.meta.is_undirected()),
        ),
        (
            "properties".to_owned(),
            properties_to_record(
                store
                    .edge_properties(handle.owner_vertex_id, handle.vertex_edge_id)
                    .into_iter()
                    .map(|(property, value)| {
                        (store.property_name(property), property.raw(), value)
                    }),
            ),
        ),
    ]))
}

fn properties_to_record(
    properties: impl IntoIterator<Item = (Option<String>, u32, Value)>,
) -> Value {
    Value::Record(
        properties
            .into_iter()
            .map(|(name, id, value)| (name.unwrap_or_else(|| id.to_string()), value))
            .collect(),
    )
}

fn record_property(value: &Value, property: &str) -> Value {
    match value {
        Value::Record(fields) => fields
            .iter()
            .find(|(name, _)| name == property)
            .map(|(_, value)| value.clone())
            .unwrap_or(Value::Null),
        _ => Value::Null,
    }
}

fn vertex_binding(row: &PlanRow, variable: &str) -> Result<VertexId, PlanQueryError> {
    match row.get(variable) {
        Some(PlanBinding::Vertex(vertex_id)) => Ok(*vertex_id),
        Some(_) | None => Err(PlanQueryError::MissingBinding {
            variable: variable.to_owned(),
        }),
    }
}

fn limit_value(value: &Value) -> Result<usize, PlanQueryError> {
    match value {
        Value::Int8(v) if *v >= 0 => Ok(*v as usize),
        Value::Int16(v) if *v >= 0 => Ok(*v as usize),
        Value::Int32(v) if *v >= 0 => Ok(*v as usize),
        Value::Int64(v) if *v >= 0 => {
            usize::try_from(*v).map_err(|_| PlanQueryError::InvalidLimit {
                value: value.clone(),
            })
        }
        Value::Uint8(v) => Ok(*v as usize),
        Value::Uint16(v) => Ok(*v as usize),
        Value::Uint32(v) => Ok(*v as usize),
        Value::Uint64(v) => usize::try_from(*v).map_err(|_| PlanQueryError::InvalidLimit {
            value: value.clone(),
        }),
        _ => Err(PlanQueryError::InvalidLimit {
            value: value.clone(),
        }),
    }
}

fn dedup_rows(rows: &mut Vec<PlanRow>) {
    let mut unique = Vec::with_capacity(rows.len());
    for row in rows.drain(..) {
        if !unique.contains(&row) {
            unique.push(row);
        }
    }
    *rows = unique;
}

fn plan_op_name(op: &PlanOp) -> &'static str {
    match op {
        PlanOp::NodeScan { .. } => "NodeScan",
        PlanOp::IndexScan { .. } => "IndexScan",
        PlanOp::EdgeIndexScan { .. } => "EdgeIndexScan",
        PlanOp::EdgeBindEndpoints { .. } => "EdgeBindEndpoints",
        PlanOp::ConditionalIndexScan { .. } => "ConditionalIndexScan",
        PlanOp::PropertyFilter { .. } => "PropertyFilter",
        PlanOp::Expand { .. } => "Expand",
        PlanOp::ExpandFilter { .. } => "ExpandFilter",
        PlanOp::ShortestPath { .. } => "ShortestPath",
        PlanOp::Let { .. } => "Let",
        PlanOp::For { .. } => "For",
        PlanOp::Filter { .. } => "Filter",
        PlanOp::CallProcedure { .. } => "CallProcedure",
        PlanOp::InlineProcedureCall { .. } => "InlineProcedureCall",
        PlanOp::UseGraph { .. } => "UseGraph",
        PlanOp::HashJoin { .. } => "HashJoin",
        PlanOp::CartesianProduct { .. } => "CartesianProduct",
        PlanOp::Aggregate { .. } => "Aggregate",
        PlanOp::Project { .. } => "Project",
        PlanOp::Sort { .. } => "Sort",
        PlanOp::Limit { .. } => "Limit",
        PlanOp::SetOperation { .. } => "SetOperation",
        PlanOp::OptionalMatch { .. } => "OptionalMatch",
        PlanOp::IndexIntersection { .. } => "IndexIntersection",
        PlanOp::WorstCaseOptimalJoin { .. } => "WorstCaseOptimalJoin",
        PlanOp::TopK { .. } => "TopK",
        PlanOp::Materialize { .. } => "Materialize",
        PlanOp::InsertVertex { .. } => "InsertVertex",
        PlanOp::InsertEdge { .. } => "InsertEdge",
        PlanOp::SetProperties { .. } => "SetProperties",
        PlanOp::RemoveProperties { .. } => "RemoveProperties",
        PlanOp::DeleteVertex { .. } => "DeleteVertex",
        PlanOp::DetachDeleteVertex { .. } => "DetachDeleteVertex",
        PlanOp::DeleteEdge { .. } => "DeleteEdge",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facade::IndexRouting;
    use crate::facade::mutation_executor::GraphMutationExecutor;
    use crate::index::lookup::PropertyIndexLookup;
    use async_trait::async_trait;
    use candid::Principal;
    use gleaph_gql::ast::{
        AggregateFunc, CmpOp, Expr, ExprKind, NullOrder, OrderByClause, SetOp, SortDirection,
        SortItem, Statement,
    };
    use gleaph_gql::parser;
    use gleaph_gql::token::Span;
    use gleaph_gql_planner::build_plan;
    use gleaph_gql_planner::plan::{
        AggregateSpec, ConditionalScanCandidate, PlanAnnotations, PlanDiagnostics, ScanValue,
        ShortestMode, WcojEdge,
    };
    use std::cell::RefCell;

    #[derive(Default)]
    struct MockPropertyIndex {
        equal_hits: RefCell<Vec<PostingHit>>,
        range_hits: RefCell<Vec<PostingHit>>,
        equal_calls: RefCell<Vec<(u32, Vec<u8>)>>,
        range_calls: RefCell<Vec<(u32, PostingRangeRequest)>>,
    }

    #[async_trait(?Send)]
    impl PropertyIndexLookup for MockPropertyIndex {
        async fn lookup_equal(
            &self,
            property_id: u32,
            value: Vec<u8>,
        ) -> Result<Vec<PostingHit>, PlanQueryError> {
            self.equal_calls.borrow_mut().push((property_id, value));
            Ok(self.equal_hits.borrow().clone())
        }

        async fn lookup_range(
            &self,
            property_id: u32,
            req: &PostingRangeRequest,
        ) -> Result<Vec<PostingHit>, PlanQueryError> {
            self.range_calls
                .borrow_mut()
                .push((property_id, req.clone()));
            Ok(self.range_hits.borrow().clone())
        }

        async fn posting_insert(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }

        async fn posting_remove(
            &self,
            _property_id: u32,
            _value: Vec<u8>,
            _vertex_id: u32,
        ) -> Result<(), PlanQueryError> {
            Ok(())
        }
    }

    fn plan(ops: Vec<PlanOp>) -> PhysicalPlan {
        PhysicalPlan {
            ops,
            diagnostics: PlanDiagnostics::default(),
            annotations: PlanAnnotations::default(),
        }
    }

    fn plan_gql(input: &str) -> PhysicalPlan {
        let program = parser::parse(input).unwrap_or_else(|err| panic!("parse error: {err}"));
        let tx = program
            .transaction_activity
            .expect("expected transaction activity");
        let block = tx.body.expect("expected statement block");
        let Statement::Query(composite) = &block.first else {
            panic!("expected query statement");
        };
        build_plan(&composite.left, None).expect("plan should build")
    }

    fn prop(variable: &str, property: &str) -> Expr {
        Expr::new(ExprKind::PropertyAccess {
            expr: Box::new(Expr::new(ExprKind::Variable(variable.to_owned()))),
            property: property.to_owned(),
        })
    }

    fn var(variable: &str) -> Expr {
        Expr::new(ExprKind::Variable(variable.to_owned()))
    }

    fn order_by(items: Vec<SortItem>) -> OrderByClause {
        OrderByClause {
            span: Span::DUMMY,
            items,
        }
    }

    fn sort_item(
        expr: Expr,
        direction: Option<SortDirection>,
        null_order: Option<NullOrder>,
    ) -> SortItem {
        SortItem {
            span: Span::DUMMY,
            expr,
            direction,
            null_order,
        }
    }

    fn project(expr: Expr, alias: &str) -> ProjectColumn {
        ProjectColumn {
            expr,
            alias: Some(alias.into()),
        }
    }

    fn params() -> BTreeMap<String, Value> {
        BTreeMap::new()
    }

    /// Minimal [`AggregateSpec`] for tests (no `expr2` / `filter` / `order_by`).
    fn agg_spec(
        func: AggregateFunc,
        expr: Option<Expr>,
        distinct: bool,
        alias: Option<&str>,
    ) -> AggregateSpec {
        AggregateSpec {
            func,
            expr,
            expr2: None,
            distinct,
            filter: None,
            order_by: None,
            alias: alias.map(|a| a.into()),
        }
    }

    fn text_column(result: &PlanQueryResult, column: &str) -> Vec<String> {
        result
            .rows
            .iter()
            .map(|row| match row.get(column) {
                Some(Value::Text(value)) => value.clone(),
                other => panic!("expected text column {column}, got {other:?}"),
            })
            .collect()
    }

    fn configure_test_index(store: &GraphStore) {
        store
            .set_index_routing(Some(IndexRouting {
                index_canister: Principal::management_canister(),
                shard_id: 7,
            }))
            .expect("set index routing");
    }

    #[test]
    fn executes_equality_index_scan_with_sortable_key() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let vid = store
            .insert_vertex_named(["IndexScanEq"], [("age", Value::Uint8(5))])
            .expect("insert vertex");
        let pid = store.property_id("age").expect("age property").raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "age".into(),
            value: ScanValue::Literal(Value::Int64(5)),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(&store, &plan, &params(), Some(&index)))
            .expect("execute index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&Value::Uint8(5)).unwrap().unwrap()
        );
        assert!(index.range_calls.borrow().is_empty());
    }

    #[test]
    fn executes_range_index_scan_with_lookup_range() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let low = store
            .insert_vertex_named(["IndexScanRange"], [("age", Value::Int64(1))])
            .expect("insert low");
        let high = store
            .insert_vertex_named(["IndexScanRange"], [("age", Value::Int64(9))])
            .expect("insert high");
        let pid = store.property_id("age").expect("age property").raw();
        let index = MockPropertyIndex::default();
        index.range_hits.borrow_mut().extend([
            PostingHit {
                shard_id: 7,
                vertex_id: u32::try_from(u64::from(low)).unwrap(),
            },
            PostingHit {
                shard_id: 7,
                vertex_id: u32::try_from(u64::from(high)).unwrap(),
            },
        ]);
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "age".into(),
            value: ScanValue::Literal(Value::Int64(5)),
            cmp: CmpOp::Ge,
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(&store, &plan, &params(), Some(&index)))
            .expect("execute range index scan");

        assert_eq!(result.rows.len(), 2);
        assert!(index.equal_calls.borrow().is_empty());
        let calls = index.range_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert!(matches!(
            &calls[0].1,
            PostingRangeRequest::Ge(bytes)
                if bytes == &value_to_index_key_bytes(&Value::Int64(5)).unwrap().unwrap()
        ));
    }

    #[test]
    fn index_scan_rejects_unsupported_parameter_value() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(["IndexScanBadParam"], [("tags", Value::List(vec![]))])
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("tags".into(), Value::List(vec![]));
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "tags".into(),
            value: ScanValue::Parameter("tags".into()),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let err = pollster::block_on(execute_plan_query(&store, &plan, &parameters, Some(&index)))
            .expect_err("unsupported parameter should fail");

        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }

    #[test]
    fn conditional_index_scan_falls_back_for_null_or_unsupported_parameter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(
                ["IndexScanConditionalFallback"],
                [("tags", Value::List(vec![]))],
            )
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("tags".into(), Value::List(vec![]));
        let plan = plan(vec![PlanOp::ConditionalIndexScan {
            candidates: vec![ConditionalScanCandidate {
                param_name: "tags".into(),
                property: "tags".into(),
                variable: "n".into(),
                cmp: CmpOp::Eq,
            }],
            fallback_label: Some("IndexScanConditionalFallback".into()),
            fallback_variable: "n".into(),
            property_projection: None,
        }]);

        let result =
            pollster::block_on(execute_plan_query(&store, &plan, &parameters, Some(&index)))
                .expect("conditional fallback");

        assert_eq!(result.rows.len(), 1);
        assert!(index.equal_calls.borrow().is_empty());
        assert!(index.range_calls.borrow().is_empty());
    }

    #[test]
    fn executes_planner_match_return_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryPersonReturn"],
                [("name", Value::Text("Planner Alice".into()))],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["PlannerQueryOtherReturn"],
                [("name", Value::Text("Planner Bob".into()))],
            )
            .expect("insert non-matching vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryPersonReturn) RETURN n.name AS name");

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner Alice".into()))
        );
    }

    #[test]
    fn executes_planner_property_filter() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryPersonFilter"],
                [
                    ("name", Value::Text("Planner Filter Ada".into())),
                    ("age", Value::Int64(37)),
                ],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["PlannerQueryPersonFilter"],
                [
                    ("name", Value::Text("Planner Filter Bob".into())),
                    ("age", Value::Int64(12)),
                ],
            )
            .expect("insert non-matching vertex");
        let plan =
            plan_gql("MATCH (n:PlannerQueryPersonFilter) WHERE n.age > 18 RETURN n.name AS name");

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner Filter Ada".into()))
        );
    }

    #[test]
    fn executes_planner_let_binding() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerQueryLetAge"], [("age", Value::Int64(36))])
            .expect("insert vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryLetAge) LET x = n.age + 1 RETURN x");

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("x"), Some(&Value::Int64(37)));
    }

    #[test]
    fn executes_planner_let_binding_dependency_order() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerQueryLetChain"], [("k", Value::Int64(10))])
            .expect("insert vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryLetChain) LET x = n.k + 1, y = x * 2 RETURN y");

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("y"), Some(&Value::Int64(22)));
    }

    #[test]
    fn executes_planner_standalone_filter() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryStandaloneFilter"],
                [
                    ("name", Value::Text("Active Ada".into())),
                    ("active", Value::Bool(true)),
                ],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["PlannerQueryStandaloneFilter"],
                [
                    ("name", Value::Text("Inactive Bob".into())),
                    ("active", Value::Bool(false)),
                ],
            )
            .expect("insert non-matching vertex");
        let plan = plan_gql(
            "MATCH (n:PlannerQueryStandaloneFilter) FILTER n.active RETURN n.name AS name",
        );

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Active Ada".into()))
        );
    }

    #[test]
    fn executes_planner_one_hop_expand() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(
                ["PlannerQueryExpandSource"],
                [("name", Value::Text("Planner Expand Alice".into()))],
            )
            .expect("insert source");
        let b = store
            .insert_vertex_named(
                ["PlannerQueryExpandTarget"],
                [("name", Value::Text("Planner Expand Bob".into()))],
            )
            .expect("insert target");
        let unrelated = store
            .insert_vertex_named(
                ["PlannerQueryExpandTarget"],
                [("name", Value::Text("Planner Expand Carol".into()))],
            )
            .expect("insert unrelated target");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("PlannerQueryKnows"),
                [("since", Value::Int64(2026))],
            )
            .expect("insert matching edge");
        store
            .insert_directed_edge_named(
                a,
                unrelated,
                Some("PlannerQueryIgnores"),
                [("since", Value::Int64(2025))],
            )
            .expect("insert non-matching edge");
        let plan = plan_gql(
            "MATCH (a:PlannerQueryExpandSource)-[e:PlannerQueryKnows]->(b:PlannerQueryExpandTarget) \
             RETURN a.name AS a_name, b.name AS b_name, e.since AS since",
        );

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("a_name"),
            Some(&Value::Text("Planner Expand Alice".into()))
        );
        assert_eq!(
            result.rows[0].get("b_name"),
            Some(&Value::Text("Planner Expand Bob".into()))
        );
        assert_eq!(result.rows[0].get("since"), Some(&Value::Int64(2026)));
    }

    #[test]
    fn executes_planner_order_by() {
        let store = GraphStore::new();
        for name in ["Planner Sort C", "Planner Sort A", "Planner Sort B"] {
            store
                .insert_vertex_named(["PlannerQuerySort"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan_gql("MATCH (n:PlannerQuerySort) RETURN n.name ORDER BY n.name");

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(
            text_column(&result, "n.name"),
            vec!["Planner Sort A", "Planner Sort B", "Planner Sort C"]
        );
    }

    #[test]
    fn executes_planner_order_by_limit_topk() {
        let store = GraphStore::new();
        for name in [
            "Planner TopK D",
            "Planner TopK A",
            "Planner TopK C",
            "Planner TopK B",
        ] {
            store
                .insert_vertex_named(["PlannerQueryTopK"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan_gql("MATCH (n:PlannerQueryTopK) RETURN n.name ORDER BY n.name LIMIT 2");
        assert!(plan.ops.iter().any(|op| matches!(op, PlanOp::TopK { .. })));

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(
            text_column(&result, "n.name"),
            vec!["Planner TopK A", "Planner TopK B"]
        );
    }

    #[test]
    fn executes_planner_return_star() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryReturnStar"],
                [("name", Value::Text("Planner Star".into()))],
            )
            .expect("insert vertex");
        let plan = plan_gql("MATCH (n:PlannerQueryReturnStar) RETURN *");

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert!(matches!(result.rows[0].get("n"), Some(Value::Record(_))));
    }

    #[test]
    fn executes_planner_limit() {
        let store = GraphStore::new();
        for name in ["Planner Limit A", "Planner Limit B"] {
            store
                .insert_vertex_named(["PlannerQueryLimit"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan_gql("MATCH (n:PlannerQueryLimit) RETURN n.name LIMIT 1");

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
    }

    #[test]
    fn executes_planner_expand_filter() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(
                ["PlannerQueryExpandFilterSource"],
                [("name", Value::Text("Planner EF A".into()))],
            )
            .expect("insert source");
        let keep = store
            .insert_vertex_named(
                ["PlannerQueryExpandFilterTarget"],
                [
                    ("name", Value::Text("Planner EF Keep".into())),
                    ("age", Value::Int64(30)),
                ],
            )
            .expect("insert keep target");
        let drop = store
            .insert_vertex_named(
                ["PlannerQueryExpandFilterTarget"],
                [
                    ("name", Value::Text("Planner EF Drop".into())),
                    ("age", Value::Int64(12)),
                ],
            )
            .expect("insert drop target");
        store
            .insert_directed_edge_named(
                a,
                keep,
                Some("PlannerQueryExpandFilterRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert keep edge");
        store
            .insert_directed_edge_named(
                a,
                drop,
                Some("PlannerQueryExpandFilterRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert drop edge");
        let plan = plan_gql(
            "MATCH (a:PlannerQueryExpandFilterSource)-[e:PlannerQueryExpandFilterRel]->\
             (b:PlannerQueryExpandFilterTarget) WHERE b.age > 18 \
             RETURN a.name AS a_name, b.name AS b_name",
        );
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::ExpandFilter { .. }))
        );

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("b_name"),
            Some(&Value::Text("Planner EF Keep".into()))
        );
    }

    #[test]
    fn executes_planner_use_graph_as_single_store_pass_through() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryUseGraph"],
                [("name", Value::Text("Planner UseGraph".into()))],
            )
            .expect("insert vertex");
        let plan = plan_gql("USE myGraph MATCH (n:PlannerQueryUseGraph) RETURN n.name AS name");

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Planner UseGraph".into()))
        );
    }

    #[test]
    fn executes_planner_cartesian_product_for_independent_matches() {
        let store = GraphStore::new();
        for name in ["Planner CP Alice", "Planner CP Bob"] {
            store
                .insert_vertex_named(
                    ["PlannerQueryCartesianPerson"],
                    [("name", Value::Text(name.into()))],
                )
                .expect("insert person");
        }
        for city in ["Planner CP Tokyo", "Planner CP Paris"] {
            store
                .insert_vertex_named(
                    ["PlannerQueryCartesianCity"],
                    [("name", Value::Text(city.into()))],
                )
                .expect("insert city");
        }
        let plan = plan_gql(
            "MATCH (a:PlannerQueryCartesianPerson) MATCH (b:PlannerQueryCartesianCity) \
             RETURN a.name AS person, b.name AS city",
        );
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::CartesianProduct { .. }))
        );

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute planned query");

        assert_eq!(result.rows.len(), 4);
        assert!(result.rows.iter().any(|row| {
            row.get("person") == Some(&Value::Text("Planner CP Alice".into()))
                && row.get("city") == Some(&Value::Text("Planner CP Tokyo".into()))
        }));
        assert!(result.rows.iter().any(|row| {
            row.get("person") == Some(&Value::Text("Planner CP Bob".into()))
                && row.get("city") == Some(&Value::Text("Planner CP Paris".into()))
        }));
    }

    #[test]
    fn node_scan_projects_vertex_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryPersonNodeScan"],
                [("name", Value::Text("Node Alice".into()))],
            )
            .expect("insert vertex");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryPersonNodeScan".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Node Alice".into()))
        );
    }

    #[test]
    fn property_filter_keeps_matching_vertices() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryPersonFilter"],
                [
                    ("name", Value::Text("Filter Ada".into())),
                    ("age", Value::Int64(37)),
                ],
            )
            .expect("insert matching vertex");
        store
            .insert_vertex_named(
                ["QueryPersonFilter"],
                [
                    ("name", Value::Text("Filter Bob".into())),
                    ("age", Value::Int64(12)),
                ],
            )
            .expect("insert non-matching vertex");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryPersonFilter".into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "age")),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
                })],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Filter Ada".into()))
        );
    }

    #[test]
    fn sort_orders_projected_scalars_ascending_and_descending() {
        let store = GraphStore::new();
        for name in ["Sort Scalar C", "Sort Scalar A", "Sort Scalar B"] {
            store
                .insert_vertex_named(["QuerySortScalar"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let scan_project = || {
            vec![
                PlanOp::NodeScan {
                    variable: "n".into(),
                    label: Some("QuerySortScalar".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![project(prop("n", "name"), "name")],
                    distinct: false,
                },
            ]
        };
        let asc = plan(
            scan_project()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(var("name"), None, None)]),
                }])
                .collect(),
        );
        let desc = plan(
            scan_project()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(
                        var("name"),
                        Some(SortDirection::Desc),
                        None,
                    )]),
                }])
                .collect(),
        );

        let asc_result = store
            .execute_plan_query(&asc, &params())
            .expect("execute ascending sort");
        let desc_result = store
            .execute_plan_query(&desc, &params())
            .expect("execute descending sort");

        assert_eq!(
            text_column(&asc_result, "name"),
            vec!["Sort Scalar A", "Sort Scalar B", "Sort Scalar C"]
        );
        assert_eq!(
            text_column(&desc_result, "name"),
            vec!["Sort Scalar C", "Sort Scalar B", "Sort Scalar A"]
        );
    }

    #[test]
    fn sort_orders_multiple_keys() {
        let store = GraphStore::new();
        for (group, name) in [
            (Value::Int64(2), "Multi B"),
            (Value::Int64(1), "Multi B"),
            (Value::Int64(1), "Multi A"),
            (Value::Int64(2), "Multi A"),
        ] {
            store
                .insert_vertex_named(
                    ["QuerySortMulti"],
                    [("group", group), ("name", Value::Text(name.into()))],
                )
                .expect("insert vertex");
        }
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QuerySortMulti".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("n", "group"), "group"),
                    project(prop("n", "name"), "name"),
                ],
                distinct: false,
            },
            PlanOp::Sort {
                order_by: order_by(vec![
                    sort_item(var("group"), None, None),
                    sort_item(var("name"), Some(SortDirection::Desc), None),
                ]),
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute multi-key sort");

        assert_eq!(
            text_column(&result, "name"),
            vec!["Multi B", "Multi A", "Multi B", "Multi A"]
        );
    }

    #[test]
    fn sort_honors_explicit_null_ordering() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["QuerySortNulls"], Vec::<(&str, Value)>::new())
            .expect("insert null vertex");
        for name in ["Null Ada", "Null Bob"] {
            store
                .insert_vertex_named(["QuerySortNulls"], [("name", Value::Text(name.into()))])
                .expect("insert named vertex");
        }
        let base_ops = || {
            vec![
                PlanOp::NodeScan {
                    variable: "n".into(),
                    label: Some("QuerySortNulls".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![project(prop("n", "name"), "name")],
                    distinct: false,
                },
            ]
        };
        let nulls_first = plan(
            base_ops()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(var("name"), None, Some(NullOrder::First))]),
                }])
                .collect(),
        );
        let nulls_last = plan(
            base_ops()
                .into_iter()
                .chain([PlanOp::Sort {
                    order_by: order_by(vec![sort_item(var("name"), None, Some(NullOrder::Last))]),
                }])
                .collect(),
        );

        let first = store
            .execute_plan_query(&nulls_first, &params())
            .expect("execute nulls first sort");
        let last = store
            .execute_plan_query(&nulls_last, &params())
            .expect("execute nulls last sort");

        assert_eq!(first.rows[0].get("name"), Some(&Value::Null));
        assert_eq!(last.rows[2].get("name"), Some(&Value::Null));
    }

    #[test]
    fn sort_rejects_incomparable_keys() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QuerySortIncomparable"],
                [("key", Value::Text("x".into()))],
            )
            .expect("insert text vertex");
        store
            .insert_vertex_named(["QuerySortIncomparable"], [("key", Value::Int64(1))])
            .expect("insert int vertex");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QuerySortIncomparable".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "key"), "key")],
                distinct: false,
            },
            PlanOp::Sort {
                order_by: order_by(vec![sort_item(var("key"), None, None)]),
            },
        ]);

        let err = store
            .execute_plan_query(&plan, &params())
            .expect_err("incomparable keys should fail");

        assert!(matches!(err, PlanQueryError::IncomparableSortValues { .. }));
    }

    #[test]
    fn topk_sorts_then_applies_offset_and_k() {
        let store = GraphStore::new();
        for name in ["TopK D", "TopK A", "TopK C", "TopK B"] {
            store
                .insert_vertex_named(["QueryTopK"], [("name", Value::Text(name.into()))])
                .expect("insert vertex");
        }
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryTopK".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
            PlanOp::TopK {
                order_by: order_by(vec![sort_item(var("name"), None, None)]),
                k: Expr::new(ExprKind::Literal(Value::Int64(2))),
                offset: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute topk");

        assert_eq!(text_column(&result, "name"), vec!["TopK B", "TopK C"]);
    }

    #[test]
    fn cartesian_product_combines_independent_subplans() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryCartesianLeft"],
                [("name", Value::Text("Left A".into()))],
            )
            .expect("insert left a");
        store
            .insert_vertex_named(
                ["QueryCartesianLeft"],
                [("name", Value::Text("Left B".into()))],
            )
            .expect("insert left b");
        store
            .insert_vertex_named(
                ["QueryCartesianRight"],
                [("name", Value::Text("Right A".into()))],
            )
            .expect("insert right a");
        store
            .insert_vertex_named(
                ["QueryCartesianRight"],
                [("name", Value::Text("Right B".into()))],
            )
            .expect("insert right b");
        let plan = plan(vec![
            PlanOp::CartesianProduct {
                left: vec![PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("QueryCartesianLeft".into()),
                    property_projection: None,
                }],
                right: vec![PlanOp::NodeScan {
                    variable: "b".into(),
                    label: Some("QueryCartesianRight".into()),
                    property_projection: None,
                }],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "left"),
                    project(prop("b", "name"), "right"),
                ],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute cartesian product");

        assert_eq!(result.rows.len(), 4);
    }

    #[test]
    fn cartesian_product_drops_conflicting_bindings() {
        let left = PlanRow::from([("x".to_owned(), PlanBinding::Value(Value::Int64(1)))]);
        let same = PlanRow::from([("x".to_owned(), PlanBinding::Value(Value::Int64(1)))]);
        let different = PlanRow::from([("x".to_owned(), PlanBinding::Value(Value::Int64(2)))]);

        assert_eq!(merge_rows(&left, &same), Some(left.clone()));
        assert_eq!(merge_rows(&left, &different), None);
    }

    #[test]
    fn hash_join_matches_planned_two_match() {
        let store = GraphStore::new();
        let alice = store
            .insert_vertex_named(
                ["QueryHashJoinUser"],
                [("name", Value::Text("HJ Alice".into()))],
            )
            .expect("insert alice");
        let bob = store
            .insert_vertex_named(
                ["QueryHashJoinTarget"],
                [("name", Value::Text("HJ Bob".into()))],
            )
            .expect("insert bob");
        store
            .insert_directed_edge_named(
                alice,
                bob,
                Some("QueryHashJoinKnows"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert edge");

        let gql = "MATCH (a:QueryHashJoinUser) MATCH (a)-[r:QueryHashJoinKnows]->(b:QueryHashJoinTarget) \
                   RETURN a.name AS an, b.name AS bn";
        let sequential = plan_gql(gql);
        let seq_result = store
            .execute_plan_query(&sequential, &params())
            .expect("sequential two-match");

        let hash_plan = plan(vec![
            PlanOp::HashJoin {
                left: vec![PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("QueryHashJoinUser".into()),
                    property_projection: None,
                }],
                right: vec![
                    PlanOp::NodeScan {
                        variable: "a".into(),
                        label: Some("QueryHashJoinUser".into()),
                        property_projection: None,
                    },
                    PlanOp::Expand {
                        src: "a".into(),
                        edge: "r".into(),
                        dst: "b".into(),
                        direction: EdgeDirection::PointingRight,
                        label: Some("QueryHashJoinKnows".into()),
                        label_expr: None,
                        var_len: None,
                        indexed_edge_equality: None,
                        edge_property_projection: None,
                        dst_property_projection: None,
                        hop_aux_binding: None,
                    },
                ],
                join_keys: vec!["a".into()],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "an"),
                    project(prop("b", "name"), "bn"),
                ],
                distinct: false,
            },
        ]);

        let hj_result = store
            .execute_plan_query(&hash_plan, &params())
            .expect("hash join");

        assert_eq!(hj_result.rows.len(), seq_result.rows.len());
        assert_eq!(hj_result.rows, seq_result.rows);
    }

    #[test]
    fn hash_join_joins_equivalent_decimal_scales() {
        use gleaph_gql::types::Decimal;

        let lit_decimal = |s: &str| {
            Expr::new(ExprKind::Literal(Value::Decimal(
                Decimal::parse(s).expect("decimal literal"),
            )))
        };
        let lit_text = |t: &str| Expr::new(ExprKind::Literal(Value::Text(t.into())));

        let plan = plan(vec![PlanOp::HashJoin {
            left: vec![PlanOp::Project {
                columns: vec![
                    ProjectColumn {
                        expr: lit_decimal("1.0"),
                        alias: Some("k".into()),
                    },
                    ProjectColumn {
                        expr: lit_text("L"),
                        alias: Some("left_tag".into()),
                    },
                ],
                distinct: false,
            }],
            right: vec![PlanOp::Project {
                columns: vec![
                    ProjectColumn {
                        expr: lit_decimal("1.00"),
                        alias: Some("k".into()),
                    },
                    ProjectColumn {
                        expr: lit_text("R"),
                        alias: Some("right_tag".into()),
                    },
                ],
                distinct: false,
            }],
            join_keys: vec!["k".into()],
        }]);

        let store = GraphStore::new();
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("decimal hash join");

        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        assert_eq!(row.get("left_tag"), Some(&Value::Text("L".into())));
        assert_eq!(row.get("right_tag"), Some(&Value::Text("R".into())));
        assert_eq!(
            row.get("k"),
            Some(&Value::Decimal(Decimal::parse("1.0").expect("k")))
        );
    }

    /// Two left + three right rows share the same join key → six merged rows (L×R multiplicity).
    /// Also checks `right_id` survives `merge_rows` (right-only binding) alongside `left_id`.
    #[test]
    fn hash_join_same_key_row_multiplicity_2x3() {
        let store = GraphStore::new();
        for (left_id, tag) in [(0i64, "L0"), (1, "L1")] {
            store
                .insert_vertex_named(
                    ["QueryHashJoinDupL"],
                    [
                        ("jk", Value::Int64(7)),
                        ("left_id", Value::Int64(left_id)),
                        ("left_tag", Value::Text(tag.into())),
                    ],
                )
                .expect("insert left");
        }
        for (right_id, tag) in [(0i64, "R0"), (1, "R1"), (2, "R2")] {
            store
                .insert_vertex_named(
                    ["QueryHashJoinDupR"],
                    [
                        ("jk", Value::Int64(7)),
                        ("right_id", Value::Int64(right_id)),
                        ("right_tag", Value::Text(tag.into())),
                    ],
                )
                .expect("insert right");
        }

        let scan_project_l = || {
            vec![
                PlanOp::NodeScan {
                    variable: "nl".into(),
                    label: Some("QueryHashJoinDupL".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("nl", "jk"), "jk"),
                        project(prop("nl", "left_id"), "left_id"),
                        project(prop("nl", "left_tag"), "left_tag"),
                    ],
                    distinct: false,
                },
            ]
        };
        let scan_project_r = || {
            vec![
                PlanOp::NodeScan {
                    variable: "nr".into(),
                    label: Some("QueryHashJoinDupR".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("nr", "jk"), "jk"),
                        project(prop("nr", "right_id"), "right_id"),
                        project(prop("nr", "right_tag"), "right_tag"),
                    ],
                    distinct: false,
                },
            ]
        };

        let plan = plan(vec![PlanOp::HashJoin {
            left: scan_project_l(),
            right: scan_project_r(),
            join_keys: vec!["jk".into()],
        }]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("hash join multiplicity");

        assert_eq!(result.rows.len(), 6);
        let mut pairs: Vec<(i64, i64)> = result
            .rows
            .iter()
            .map(|row| {
                let li = match row.get("left_id") {
                    Some(Value::Int64(x)) => *x,
                    other => panic!("expected int left_id, got {other:?}"),
                };
                let ri = match row.get("right_id") {
                    Some(Value::Int64(x)) => *x,
                    other => panic!("expected int right_id, got {other:?}"),
                };
                (li, ri)
            })
            .collect();
        pairs.sort();
        assert_eq!(pairs, vec![(0, 0), (0, 1), (0, 2), (1, 0), (1, 1), (1, 2),]);
        for row in &result.rows {
            assert_eq!(row.get("jk"), Some(&Value::Int64(7)));
            assert!(row.get("left_tag").is_some());
            assert!(row.get("right_tag").is_some());
        }
    }

    #[test]
    fn hash_join_two_join_keys_excludes_partial_match() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyL"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(2)),
                    ("lt", Value::Text("L12".into())),
                ],
            )
            .expect("insert L12");
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyL"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(3)),
                    ("lt", Value::Text("L13".into())),
                ],
            )
            .expect("insert L13");
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyR"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(2)),
                    ("rt", Value::Text("R12".into())),
                ],
            )
            .expect("insert R12");
        store
            .insert_vertex_named(
                ["QueryHashJoin2KeyR"],
                [
                    ("ka", Value::Int64(1)),
                    ("kb", Value::Int64(99)),
                    ("rt", Value::Text("R199".into())),
                ],
            )
            .expect("insert R199");

        let plan = plan(vec![PlanOp::HashJoin {
            left: vec![
                PlanOp::NodeScan {
                    variable: "l".into(),
                    label: Some("QueryHashJoin2KeyL".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("l", "ka"), "ka"),
                        project(prop("l", "kb"), "kb"),
                        project(prop("l", "lt"), "lt"),
                    ],
                    distinct: false,
                },
            ],
            right: vec![
                PlanOp::NodeScan {
                    variable: "r".into(),
                    label: Some("QueryHashJoin2KeyR".into()),
                    property_projection: None,
                },
                PlanOp::Project {
                    columns: vec![
                        project(prop("r", "ka"), "ka"),
                        project(prop("r", "kb"), "kb"),
                        project(prop("r", "rt"), "rt"),
                    ],
                    distinct: false,
                },
            ],
            join_keys: vec!["ka".into(), "kb".into()],
        }]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("two-key hash join");

        assert_eq!(result.rows.len(), 1);
        let row = &result.rows[0];
        assert_eq!(row.get("ka"), Some(&Value::Int64(1)));
        assert_eq!(row.get("kb"), Some(&Value::Int64(2)));
        assert_eq!(row.get("lt"), Some(&Value::Text("L12".into())));
        assert_eq!(row.get("rt"), Some(&Value::Text("R12".into())));
    }

    #[test]
    fn hash_join_matches_sequential_on_branching_graph() {
        let store = GraphStore::new();
        let alice = store
            .insert_vertex_named(
                ["QueryHashJoinBranchUser"],
                [("name", Value::Text("Branch Alice".into()))],
            )
            .expect("insert user");
        let bob = store
            .insert_vertex_named(
                ["QueryHashJoinBranchTarget"],
                [("name", Value::Text("Branch Bob".into()))],
            )
            .expect("insert bob");
        let carol = store
            .insert_vertex_named(
                ["QueryHashJoinBranchTarget"],
                [("name", Value::Text("Branch Carol".into()))],
            )
            .expect("insert carol");
        store
            .insert_directed_edge_named(
                alice,
                bob,
                Some("QueryHashJoinBranchRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("edge to bob");
        store
            .insert_directed_edge_named(
                alice,
                carol,
                Some("QueryHashJoinBranchRel"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("edge to carol");

        let gql = "MATCH (a:QueryHashJoinBranchUser) MATCH (a)-[r:QueryHashJoinBranchRel]->(b:QueryHashJoinBranchTarget) \
                   RETURN a.name AS an, b.name AS bn";
        let sequential = plan_gql(gql);
        let seq_result = store
            .execute_plan_query(&sequential, &params())
            .expect("sequential two-match branching");

        let hash_plan = plan(vec![
            PlanOp::HashJoin {
                left: vec![PlanOp::NodeScan {
                    variable: "a".into(),
                    label: Some("QueryHashJoinBranchUser".into()),
                    property_projection: None,
                }],
                right: vec![
                    PlanOp::NodeScan {
                        variable: "a".into(),
                        label: Some("QueryHashJoinBranchUser".into()),
                        property_projection: None,
                    },
                    PlanOp::Expand {
                        src: "a".into(),
                        edge: "r".into(),
                        dst: "b".into(),
                        direction: EdgeDirection::PointingRight,
                        label: Some("QueryHashJoinBranchRel".into()),
                        label_expr: None,
                        var_len: None,
                        indexed_edge_equality: None,
                        edge_property_projection: None,
                        dst_property_projection: None,
                        hop_aux_binding: None,
                    },
                ],
                join_keys: vec!["a".into()],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "an"),
                    project(prop("b", "name"), "bn"),
                ],
                distinct: false,
            },
        ]);

        let hj_result = store
            .execute_plan_query(&hash_plan, &params())
            .expect("hash join branching");

        assert_eq!(hj_result.rows.len(), seq_result.rows.len());
        fn pair_key(row: &std::collections::BTreeMap<String, Value>) -> (String, String) {
            let an = match row.get("an") {
                Some(Value::Text(s)) => s.clone(),
                other => panic!("expected text an, got {other:?}"),
            };
            let bn = match row.get("bn") {
                Some(Value::Text(s)) => s.clone(),
                other => panic!("expected text bn, got {other:?}"),
            };
            (an, bn)
        }
        let mut hj_keys: Vec<_> = hj_result.rows.iter().map(pair_key).collect();
        hj_keys.sort();
        let mut seq_keys: Vec<_> = seq_result.rows.iter().map(pair_key).collect();
        seq_keys.sort();
        assert_eq!(hj_keys, seq_keys);
    }

    #[test]
    fn directed_expand_projects_endpoint_and_edge_properties() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(
                ["QueryExpandSource"],
                [("name", Value::Text("Expand Alice".into()))],
            )
            .expect("insert source");
        let b = store
            .insert_vertex_named(
                ["QueryExpandTarget"],
                [("name", Value::Text("Expand Bob".into()))],
            )
            .expect("insert target");
        store
            .insert_directed_edge_named(a, b, Some("QueryKnows"), [("since", Value::Int64(2026))])
            .expect("insert edge");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("QueryExpandSource".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("QueryKnows".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("a", "name"), "a_name"),
                    project(prop("b", "name"), "b_name"),
                    project(prop("e", "since"), "since"),
                ],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("a_name"),
            Some(&Value::Text("Expand Alice".into()))
        );
        assert_eq!(
            result.rows[0].get("b_name"),
            Some(&Value::Text("Expand Bob".into()))
        );
        assert_eq!(result.rows[0].get("since"), Some(&Value::Int64(2026)));
    }

    #[test]
    fn expand_filter_applies_destination_predicate() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["QueryExpandFilterSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        let keep = store
            .insert_vertex_named(["QueryExpandFilterTarget"], [("age", Value::Int64(44))])
            .expect("insert keep target");
        let drop = store
            .insert_vertex_named(["QueryExpandFilterTarget"], [("age", Value::Int64(10))])
            .expect("insert drop target");
        store
            .insert_directed_edge_named(
                a,
                keep,
                Some("QueryExpandFilterEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert keep edge");
        store
            .insert_directed_edge_named(
                a,
                drop,
                Some("QueryExpandFilterEdge"),
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert drop edge");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("QueryExpandFilterSource".into()),
                property_projection: None,
            },
            PlanOp::ExpandFilter {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("QueryExpandFilterEdge".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                dst_filter: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("b", "age")),
                    op: CmpOp::Gt,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(18)))),
                })],
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("b", "age"), "age")],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("age"), Some(&Value::Int64(44)));
    }

    #[test]
    fn return_star_projects_vertex_and_edge_records() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(
                ["QueryReturnStarSource"],
                [("name", Value::Text("Star A".into()))],
            )
            .expect("insert source");
        let b = store
            .insert_vertex_named(
                ["QueryReturnStarTarget"],
                [("name", Value::Text("Star B".into()))],
            )
            .expect("insert target");
        store
            .insert_directed_edge_named(
                a,
                b,
                Some("QueryReturnStarEdge"),
                [("since", Value::Int64(1))],
            )
            .expect("insert edge");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("QueryReturnStarSource".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("QueryReturnStarEdge".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
            },
            PlanOp::Project {
                columns: vec![],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert!(matches!(result.rows[0].get("a"), Some(Value::Record(_))));
        assert!(matches!(result.rows[0].get("b"), Some(Value::Record(_))));
        assert!(matches!(result.rows[0].get("e"), Some(Value::Record(_))));
    }

    #[test]
    fn materialize_and_limit_shape_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["QueryLimitPerson"],
                [("name", Value::Text("Limit A".into()))],
            )
            .expect("insert first");
        store
            .insert_vertex_named(
                ["QueryLimitPerson"],
                [("name", Value::Text("Limit B".into()))],
            )
            .expect("insert second");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("QueryLimitPerson".into()),
                property_projection: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "name")],
                distinct: false,
            },
            PlanOp::Materialize {
                columns: vec![],
                distinct: false,
            },
            PlanOp::Limit {
                count: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
                offset: None,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Limit A".into()))
        );
    }

    #[test]
    fn optional_match_planner_null_padding_when_no_edge() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptMatchA"], [("name", Value::Text("solo".into()))])
            .expect("insert vertex");
        let gql = "MATCH (n:OptMatchA) OPTIONAL MATCH (n)-[e:OptMatchRel]->(m:OptMatchB) \
                   RETURN n.name AS nn, m.name AS mn";
        let plan = plan_gql(gql);
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::OptionalMatch { .. })),
            "expected OptionalMatch in plan: {:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute optional match");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("nn"), Some(&Value::Text("solo".into())));
        assert_eq!(result.rows[0].get("mn"), Some(&Value::Null));
    }

    #[test]
    fn optional_match_planner_returns_m_when_edge_exists() {
        let store = GraphStore::new();
        let n = store
            .insert_vertex_named(["OptMatchA2"], [("name", Value::Text("a".into()))])
            .expect("insert n");
        let m = store
            .insert_vertex_named(["OptMatchB2"], [("name", Value::Text("buddy".into()))])
            .expect("insert m");
        store
            .insert_directed_edge_named(n, m, Some("OptMatchRel2"), Vec::<(&str, Value)>::new())
            .expect("insert edge");
        let gql = "MATCH (n:OptMatchA2) OPTIONAL MATCH (n)-[e:OptMatchRel2]->(m:OptMatchB2) \
                   RETURN m.name AS mn";
        let plan = plan_gql(gql);
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute optional match");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("mn"), Some(&Value::Text("buddy".into())));
    }

    #[test]
    fn optional_match_leading_empty_graph_null_binds_pattern_var() {
        let store = GraphStore::new();
        let gql = "OPTIONAL MATCH (n:OptMatchLeading) RETURN n IS NULL AS is_n_null";
        let plan = plan_gql(gql);
        assert!(
            plan.ops
                .iter()
                .any(|op| matches!(op, PlanOp::OptionalMatch { .. })),
            "expected OptionalMatch: {:?}",
            plan.ops
        );
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute leading optional");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("is_n_null"), Some(&Value::Bool(true)));
    }

    #[test]
    fn optional_match_manual_null_padding_edge_and_dst() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["OptManualN"], Vec::<(&str, Value)>::new())
            .expect("insert n");
        let expand = PlanOp::Expand {
            src: "n".into(),
            edge: "e".into(),
            dst: "m".into(),
            direction: EdgeDirection::PointingRight,
            label: Some("OptManualRel".into()),
            label_expr: None,
            var_len: None,
            indexed_edge_equality: None,
            edge_property_projection: None,
            dst_property_projection: None,
            hop_aux_binding: None,
        };
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("OptManualN".into()),
                property_projection: None,
            },
            PlanOp::OptionalMatch {
                sub_plan: vec![expand],
            },
            PlanOp::Project {
                columns: vec![
                    project(Expr::new(ExprKind::IsNull(Box::new(var("e")))), "e_null"),
                    project(Expr::new(ExprKind::IsNull(Box::new(var("m")))), "m_null"),
                ],
                distinct: false,
            },
        ]);

        let result = store
            .execute_plan_query(&plan, &params())
            .expect("execute manual optional");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("e_null"), Some(&Value::Bool(true)));
        assert_eq!(result.rows[0].get("m_null"), Some(&Value::Bool(true)));
    }

    fn agg_count_star() -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::CountStar,
            expr: None,
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    fn agg_sum_expr(inner: Expr, distinct: bool) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Sum,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct,
            order_by: None,
            filter: None,
        })
    }

    fn agg_min_expr(inner: Expr) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Min,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    fn agg_max_expr(inner: Expr) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Max,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    fn agg_avg_expr(inner: Expr) -> Expr {
        Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Avg,
            expr: Some(Box::new(inner)),
            expr2: None,
            distinct: false,
            order_by: None,
            filter: None,
        })
    }

    #[test]
    fn aggregate_count_star_empty_graph_after_scan() {
        let store = GraphStore::new();
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("NoVerticesForAgg".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("cnt"))],
            },
            PlanOp::Project {
                columns: vec![project(agg_count_star(), "cnt")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("global aggregate on empty match");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(0)));
    }

    #[test]
    fn aggregate_count_star_after_node_scan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggScanLbl"], [("x", Value::Int64(1))])
            .expect("v1");
        store
            .insert_vertex_named(["AggScanLbl"], [("x", Value::Int64(2))])
            .expect("v2");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggScanLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("cnt"))],
            },
            PlanOp::Project {
                columns: vec![project(agg_count_star(), "cnt")],
                distinct: false,
            },
        ]);
        let result = store.execute_plan_query(&plan, &params()).expect("count");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
    }

    #[test]
    fn aggregate_groups_by_property_and_counts_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggGrpLbl"], [("dept", Value::Text("S".into()))])
            .expect("a");
        store
            .insert_vertex_named(["AggGrpLbl"], [("dept", Value::Text("S".into()))])
            .expect("b");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggGrpLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![prop("n", "dept")],
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("c"))],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("n", "dept"), "d"),
                    project(agg_count_star(), "c"),
                ],
                distinct: false,
            },
        ]);
        let result = store.execute_plan_query(&plan, &params()).expect("grouped");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("d"), Some(&Value::Text("S".into())));
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(2)));
    }

    #[test]
    fn aggregate_sum_min_max_avg_numeric_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggNumLbl"], [("v", Value::Int64(10))])
            .expect("a");
        store
            .insert_vertex_named(["AggNumLbl"], [("v", Value::Int64(20))])
            .expect("b");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggNumLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![
                    agg_spec(AggregateFunc::Sum, Some(prop("n", "v")), false, Some("s")),
                    agg_spec(AggregateFunc::Min, Some(prop("n", "v")), false, Some("mn")),
                    agg_spec(AggregateFunc::Max, Some(prop("n", "v")), false, Some("mx")),
                    agg_spec(AggregateFunc::Avg, Some(prop("n", "v")), false, Some("a")),
                ],
            },
            PlanOp::Project {
                columns: vec![
                    project(agg_sum_expr(prop("n", "v"), false), "s"),
                    project(agg_min_expr(prop("n", "v")), "mn"),
                    project(agg_max_expr(prop("n", "v")), "mx"),
                    project(agg_avg_expr(prop("n", "v")), "a"),
                ],
                distinct: false,
            },
        ]);
        let result = store.execute_plan_query(&plan, &params()).expect("agg");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("s"), Some(&Value::Int64(30)));
        assert_eq!(result.rows[0].get("mn"), Some(&Value::Int64(10)));
        assert_eq!(result.rows[0].get("mx"), Some(&Value::Int64(20)));
        assert_eq!(result.rows[0].get("a"), Some(&Value::Int64(15)));
    }

    #[test]
    fn aggregate_count_distinct_property() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["AggDistLbl"], [("k", Value::Int64(1))])
            .expect("a");
        store
            .insert_vertex_named(["AggDistLbl"], [("k", Value::Int64(1))])
            .expect("b");
        let count_distinct = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::Count,
            expr: Some(Box::new(prop("n", "k"))),
            expr2: None,
            distinct: true,
            order_by: None,
            filter: None,
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("AggDistLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: Vec::new(),
                aggregates: vec![agg_spec(
                    AggregateFunc::Count,
                    Some(prop("n", "k")),
                    true,
                    Some("c"),
                )],
            },
            PlanOp::Project {
                columns: vec![project(count_distinct, "c")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("distinct");
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(1)));
    }

    #[test]
    fn aggregate_grouped_empty_input_yields_no_rows() {
        let store = GraphStore::new();
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("NoSuchAggLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![prop("n", "dept")],
                aggregates: vec![agg_spec(AggregateFunc::CountStar, None, false, Some("c"))],
            },
            PlanOp::Project {
                columns: vec![
                    project(prop("n", "dept"), "d"),
                    project(agg_count_star(), "c"),
                ],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("empty groups");
        assert!(result.rows.is_empty());
    }

    #[test]
    fn aggregate_count_star_with_filter_manual_plan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["FiltAggLbl"], [("ok", Value::Bool(false))])
            .expect("v0");
        store
            .insert_vertex_named(["FiltAggLbl"], [("ok", Value::Bool(true))])
            .expect("v1");
        let filter = Expr::new(ExprKind::Compare {
            left: Box::new(prop("n", "ok")),
            op: CmpOp::Eq,
            right: Box::new(Expr::new(ExprKind::Literal(Value::Bool(true)))),
        });
        let count_star_filtered = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::CountStar,
            expr: None,
            expr2: None,
            distinct: false,
            order_by: None,
            filter: Some(Box::new(filter.clone())),
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("FiltAggLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::CountStar,
                    expr: None,
                    expr2: None,
                    distinct: false,
                    filter: Some(filter),
                    order_by: None,
                    alias: Some("c".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![project(count_star_filtered, "c")],
                distinct: false,
            },
        ]);
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("filtered");
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(1)));
    }

    #[test]
    fn aggregate_collect_list_manual_plan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["CollLbl"], [("v", Value::Int64(3))])
            .expect("a");
        store
            .insert_vertex_named(["CollLbl"], [("v", Value::Int64(1))])
            .expect("b");
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("CollLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::Collect,
                    expr: Some(prop("n", "v")),
                    expr2: None,
                    distinct: false,
                    filter: None,
                    order_by: None,
                    alias: Some("xs".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![project(
                    Expr::new(ExprKind::Aggregate {
                        func: AggregateFunc::Collect,
                        expr: Some(Box::new(prop("n", "v"))),
                        expr2: None,
                        distinct: false,
                        order_by: None,
                        filter: None,
                    }),
                    "xs",
                )],
                distinct: false,
            },
        ]);
        let result = store.execute_plan_query(&plan, &params()).expect("collect");
        match result.rows[0].get("xs") {
            Some(Value::List(xs)) => {
                assert_eq!(xs.len(), 2);
            }
            other => panic!("expected list: {other:?}"),
        }
    }

    #[test]
    fn aggregate_percentile_cont_manual_plan() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PctLbl"], [("v", Value::Int64(10))])
            .expect("a");
        store
            .insert_vertex_named(["PctLbl"], [("v", Value::Int64(30))])
            .expect("b");
        let p = Expr::new(ExprKind::Literal(Value::Float64(0.5)));
        let agg = Expr::new(ExprKind::Aggregate {
            func: AggregateFunc::PercentileCont,
            expr: Some(Box::new(prop("n", "v"))),
            expr2: Some(Box::new(p.clone())),
            distinct: false,
            order_by: None,
            filter: None,
        });
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("PctLbl".into()),
                property_projection: None,
            },
            PlanOp::Aggregate {
                group_by: vec![],
                aggregates: vec![AggregateSpec {
                    func: AggregateFunc::PercentileCont,
                    expr: Some(prop("n", "v")),
                    expr2: Some(p),
                    distinct: false,
                    filter: None,
                    order_by: None,
                    alias: Some("m".into()),
                }],
            },
            PlanOp::Project {
                columns: vec![project(agg, "m")],
                distinct: false,
            },
        ]);
        let result = store.execute_plan_query(&plan, &params()).expect("pct");
        match result.rows[0].get("m") {
            Some(Value::Float64(f)) => assert!((f - 20.0).abs() < 1e-9),
            other => panic!("expected float median: {other:?}"),
        }
    }

    #[test]
    fn aggregate_sum_with_expr2_is_rejected() {
        let store = GraphStore::new();
        let plan = plan(vec![PlanOp::Aggregate {
            group_by: Vec::new(),
            aggregates: vec![AggregateSpec {
                func: AggregateFunc::Sum,
                expr: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
                expr2: Some(Expr::new(ExprKind::Literal(Value::Int64(2)))),
                distinct: false,
                filter: None,
                order_by: None,
                alias: None,
            }],
        }]);
        let err = store
            .execute_plan_query(&plan, &params())
            .expect_err("sum with expr2");
        assert!(
            matches!(err, PlanQueryError::UnsupportedOp(name) if name == "Aggregate.expr2"),
            "{err:?}"
        );
    }

    #[test]
    fn executes_planner_match_return_count_star() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerAggCntLbl"], Vec::<(&str, Value)>::new())
            .expect("vertex");
        let plan = plan_gql("MATCH (n:PlannerAggCntLbl) RETURN count(*) AS c");
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("planner aggregate");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(1)));
    }

    #[test]
    fn executes_planner_match_return_count_star_plus_literal() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(["PlannerAggPlus"], Vec::<(&str, Value)>::new())
            .expect("v1");
        store
            .insert_vertex_named(["PlannerAggPlus"], Vec::<(&str, Value)>::new())
            .expect("v2");
        let plan = plan_gql("MATCH (n:PlannerAggPlus) RETURN count(*) + 1 AS c");
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("nested aggregate expr");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("c"), Some(&Value::Int64(3)));
    }

    #[test]
    fn executes_planner_avg_nested_in_arithmetic() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerAggAvgArith"], [("x", Value::Int64(10))]);
        let _ = store.insert_vertex_named(["PlannerAggAvgArith"], [("x", Value::Int64(30))]);
        let plan = plan_gql("MATCH (n:PlannerAggAvgArith) RETURN avg(n.x) * 2 AS doubled");
        let result = store.execute_plan_query(&plan, &params()).expect("avg * 2");
        assert_eq!(result.rows.len(), 1);
        match result.rows[0].get("doubled") {
            Some(Value::Float64(f)) => assert!((f - 40.0).abs() < 1e-6),
            Some(Value::Int64(i)) => assert_eq!(*i, 40),
            other => panic!("expected numeric doubled: {other:?}"),
        }
    }

    #[test]
    fn executes_planner_group_by_having_count_filter() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(2))]);
        let plan = plan_gql(
            "MATCH (n:PlannerHavingK) RETURN n.k, count(*) AS cnt GROUP BY n.k HAVING count(*) > 1",
        );
        let result = store.execute_plan_query(&plan, &params()).expect("having");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("n.k"), Some(&Value::Int64(1)));
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
    }

    #[test]
    fn executes_planner_group_by_having_count_return_alias() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerHavingK"], [("k", Value::Int64(2))]);
        let plan = plan_gql(
            "MATCH (n:PlannerHavingK) RETURN n.k, count(*) AS cnt GROUP BY n.k HAVING cnt > 1",
        );
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("having with return alias");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("n.k"), Some(&Value::Int64(1)));
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
    }

    #[test]
    fn executes_planner_collect_list_names() {
        let store = GraphStore::new();
        let _ =
            store.insert_vertex_named(["PlannerAggCollect"], [("name", Value::Text("a".into()))]);
        let _ =
            store.insert_vertex_named(["PlannerAggCollect"], [("name", Value::Text("b".into()))]);
        let plan = plan_gql("MATCH (n:PlannerAggCollect) RETURN COLLECT_LIST(n.name) AS names");
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("collect_list");
        assert_eq!(result.rows.len(), 1);
        let list = result.rows[0].get("names").expect("names column");
        let Value::List(items) = list else {
            panic!("expected list, got {list:?}");
        };
        assert_eq!(items.len(), 2);
        let mut texts: Vec<String> = items
            .iter()
            .map(|v| match v {
                Value::Text(t) => t.clone(),
                _ => panic!("expected text in list: {v:?}"),
            })
            .collect();
        texts.sort();
        assert_eq!(texts, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn executes_planner_stddev_pop_two_values() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerAggStd"], [("v", Value::Int64(1))]);
        let _ = store.insert_vertex_named(["PlannerAggStd"], [("v", Value::Int64(3))]);
        let plan = plan_gql("MATCH (n:PlannerAggStd) RETURN STDDEV_POP(n.v) AS s");
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("stddev_pop");
        assert_eq!(result.rows.len(), 1);
        match result.rows[0].get("s") {
            Some(Value::Float64(f)) => assert!((f - 1.0).abs() < 1e-6),
            other => panic!("expected float stddev: {other:?}"),
        }
    }

    #[test]
    fn executes_planner_percentile_cont_planned() {
        let store = GraphStore::new();
        let _ = store.insert_vertex_named(["PlannerAggPct"], [("v", Value::Int64(10))]);
        let _ = store.insert_vertex_named(["PlannerAggPct"], [("v", Value::Int64(20))]);
        let _ = store.insert_vertex_named(["PlannerAggPct"], [("v", Value::Int64(30))]);
        let plan = plan_gql("MATCH (n:PlannerAggPct) RETURN PERCENTILE_CONT(n.v, 0.5) AS m");
        let result = store
            .execute_plan_query(&plan, &params())
            .expect("percentile");
        assert_eq!(result.rows.len(), 1);
        match result.rows[0].get("m") {
            Some(Value::Float64(f)) => assert!((f - 20.0).abs() < 1e-6),
            other => panic!("expected float median: {other:?}"),
        }
    }

    #[test]
    fn unsupported_operator_returns_stable_error() {
        let store = GraphStore::new();
        let cases = vec![
            (
                PlanOp::EdgeIndexScan {
                    variable: "e".into(),
                    property: "w".into(),
                    value: ScanValue::Literal(Value::Int64(1)),
                    property_projection: None,
                },
                "EdgeIndexScan",
            ),
            (
                PlanOp::SetOperation {
                    op: SetOp::Union,
                    right: Box::new(plan(Vec::new())),
                },
                "SetOperation",
            ),
            (
                PlanOp::ShortestPath {
                    src: "a".into(),
                    dst: "b".into(),
                    edge: "e".into(),
                    path_var: None,
                    mode: ShortestMode::AnyShortest,
                    direction: EdgeDirection::PointingRight,
                    label: None,
                    label_expr: None,
                    var_len: None,
                },
                "ShortestPath",
            ),
            (
                PlanOp::CallProcedure {
                    name: vec!["db".into(), "labels".into()],
                    args: Vec::new(),
                    yield_columns: None,
                    optional: false,
                },
                "CallProcedure",
            ),
            (
                PlanOp::WorstCaseOptimalJoin {
                    variables: Vec::new(),
                    edges: Vec::<WcojEdge>::new(),
                },
                "WorstCaseOptimalJoin",
            ),
        ];

        for (op, expected_name) in cases {
            let plan = plan(vec![op]);
            let err = store
                .execute_plan_query(&plan, &params())
                .expect_err("operator should be unsupported in v1");

            assert!(
                matches!(err, PlanQueryError::UnsupportedOp(name) if name == expected_name),
                "expected UnsupportedOp({expected_name}), got {err:?}"
            );
        }
    }
}
