//! Vertex/index scan operators and limit+offset streaming execution.

use std::collections::BTreeMap;

use candid::Principal;
use gleaph_gql::ast::{CmpOp, Expr};
use gleaph_gql::types::EdgeDirection;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_planner::plan::{
    AggregateSpec, ConditionalScanCandidate, EdgeValuePredicate, EdgeVectorPredicate,
    IndexScanSpec, PlanOp, ScanValue, Str,
};
use gleaph_graph_kernel::entry::{Edge, PreparedWeightDecoder};
use gleaph_graph_kernel::index::{PostingHit, PostingRangeRequest};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::traits::CsrVertexTombstone;
use nohash_hasher::IntSet;

use super::super::error::PlanQueryError;
use super::super::row::PlanRow;
use super::context::QueryExprEvaluator;
use super::expand::{
    EdgeEqualityStreamFilter, ExpandDst, build_expanded_row, csr_offset_fast_path_for_expand,
    edge_binding_for_expand, edge_equality_stream_filter, edge_matches_stream_filter,
    expand_accepts_remote_dst, expand_candidates_into, expand_dst_matches_prebound_vertex,
    visit_csr_expand_fast_path,
};
use super::{
    EdgeSequenceOrder, PlanBinding, dst_filter_is_dst_vertex_only, ensure_simple_expand,
    limit_value, project_row, row_matches_all, vertex_row_matches_dst_filters,
};
use crate::facade::GraphStore;
use crate::facade::migration::{migration_visibility_filter_needed, vertex_visible_to_query};
use crate::index::lookup::PropertyIndexLookup;
use crate::index::placement;

pub(crate) const LIMITED_STREAMING_REMOTE_EXPAND_SOURCE: &str =
    "LimitedStreamingPrefix.remote_expand_source";

#[cfg(test)]
mod test_counters {
    use std::cell::Cell;

    thread_local! {
        pub(crate) static NODE_SCAN_VISITS: Cell<usize> = const { Cell::new(0) };
        pub(crate) static EDGE_STREAM_VISITS: Cell<usize> = const { Cell::new(0) };
    }
}

#[cfg(test)]
pub(crate) use test_counters::{EDGE_STREAM_VISITS, NODE_SCAN_VISITS};

pub(crate) struct LimitedStreamingPrefixResult {
    pub(crate) rows: Vec<PlanRow>,
    pub(crate) clears_active_aggregate: bool,
}

struct LimitedRows {
    offset_remaining: usize,
    take_remaining: usize,
    rows: Vec<PlanRow>,
}

impl LimitedRows {
    fn new(offset: usize, count: usize) -> Self {
        Self {
            offset_remaining: offset,
            take_remaining: count,
            rows: Vec::new(),
        }
    }

    fn is_done(&self) -> bool {
        self.take_remaining == 0
    }

    fn push(&mut self, row: PlanRow) -> bool {
        if self.offset_remaining > 0 {
            self.offset_remaining -= 1;
            return false;
        }
        if self.take_remaining == 0 {
            return true;
        }
        self.rows.push(row);
        self.take_remaining -= 1;
        self.take_remaining == 0
    }
}

fn streaming_ops_preserve_row_cardinality_after(ops: &[PlanOp], start: usize) -> bool {
    let mut i = start;
    while i < ops.len() {
        match &ops[i] {
            PlanOp::Project { distinct, .. } if !distinct => i += 1,
            PlanOp::Let { .. } => i += 1,
            _ => return false,
        }
    }
    true
}

pub(crate) fn limited_streaming_prefix_limit_idx(
    ops: &[PlanOp],
    start_idx: usize,
) -> Option<usize> {
    for (idx, op) in ops.iter().enumerate().skip(start_idx) {
        match op {
            PlanOp::Limit { count: Some(_), .. } => return Some(idx),
            PlanOp::Limit { count: None, .. } => return None,
            op if streaming_prefix_op_supported(op) => {}
            _ => return None,
        }
    }
    None
}

fn streaming_prefix_op_supported(op: &PlanOp) -> bool {
    match op {
        PlanOp::NodeScan { .. }
        | PlanOp::PropertyFilter { .. }
        | PlanOp::Let { .. }
        | PlanOp::Filter { .. }
        | PlanOp::Expand { .. }
        | PlanOp::ExpandFilter { .. } => true,
        PlanOp::Project { distinct, .. } => !distinct,
        _ => false,
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) fn execute_limited_streaming_prefix(
    store: &GraphStore,
    ops: &[PlanOp],
    initial_rows: Vec<PlanRow>,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    aggregate_specs: Option<&[AggregateSpec]>,
) -> Result<LimitedStreamingPrefixResult, PlanQueryError> {
    let Some((PlanOp::Limit { count, offset }, streaming_ops)) = ops.split_last() else {
        return Ok(LimitedStreamingPrefixResult {
            rows: initial_rows,
            clears_active_aggregate: false,
        });
    };
    let evaluator = QueryExprEvaluator {
        store,
        parameters,
        aggregate_specs,
        caller,
        gleaph_weight_decoders,
    };
    let offset = match offset {
        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
        None => 0,
    };
    let count = match count {
        Some(expr) => limit_value(&evaluator.eval_expr(&PlanRow::new(), expr)?)?,
        None => {
            return Ok(LimitedStreamingPrefixResult {
                rows: initial_rows,
                clears_active_aggregate: false,
            });
        }
    };
    let mut sink = LimitedRows::new(offset, count);
    let mut clears_active_aggregate = false;
    if sink.is_done() {
        return Ok(LimitedStreamingPrefixResult {
            rows: sink.rows,
            clears_active_aggregate,
        });
    }

    for row in initial_rows {
        if stream_row_through_ops(
            store,
            streaming_ops,
            0,
            row,
            parameters,
            caller,
            gleaph_weight_decoders,
            &evaluator,
            &mut sink,
            &mut clears_active_aggregate,
        )? {
            break;
        }
    }

    Ok(LimitedStreamingPrefixResult {
        rows: sink.rows,
        clears_active_aggregate,
    })
}

#[allow(clippy::too_many_arguments)]
fn stream_row_through_ops(
    store: &GraphStore,
    ops: &[PlanOp],
    op_idx: usize,
    row: PlanRow,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    sink: &mut LimitedRows,
    clears_active_aggregate: &mut bool,
) -> Result<bool, PlanQueryError> {
    let Some(op) = ops.get(op_idx) else {
        return Ok(sink.push(row));
    };
    match op {
        PlanOp::NodeScan {
            variable,
            label,
            property_projection: _,
        } => stream_node_scan(
            store,
            ops,
            op_idx,
            row,
            variable,
            label.as_ref(),
            parameters,
            caller,
            gleaph_weight_decoders,
            evaluator,
            sink,
            clears_active_aggregate,
        ),
        PlanOp::PropertyFilter { predicates, .. } => {
            if row_matches_all(evaluator, &row, predicates)? {
                stream_row_through_ops(
                    store,
                    ops,
                    op_idx + 1,
                    row,
                    parameters,
                    caller,
                    gleaph_weight_decoders,
                    evaluator,
                    sink,
                    clears_active_aggregate,
                )
            } else {
                Ok(false)
            }
        }
        PlanOp::Let { bindings } => {
            let mut row = row;
            for binding in bindings {
                let value = evaluator.eval_expr(&row, &binding.value)?;
                row.insert(binding.variable.clone(), PlanBinding::Value(value));
            }
            stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                row,
                parameters,
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        }
        PlanOp::Filter { condition } => {
            if row_matches_all(evaluator, &row, std::slice::from_ref(condition))? {
                stream_row_through_ops(
                    store,
                    ops,
                    op_idx + 1,
                    row,
                    parameters,
                    caller,
                    gleaph_weight_decoders,
                    evaluator,
                    sink,
                    clears_active_aggregate,
                )
            } else {
                Ok(false)
            }
        }
        PlanOp::Project { columns, distinct } => {
            debug_assert!(!distinct);
            let projected = project_row(evaluator, &row, columns)?;
            *clears_active_aggregate = true;
            stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                projected,
                parameters,
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        }
        PlanOp::Expand {
            src,
            edge,
            dst,
            direction,
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            edge_value_predicate,
            edge_vector_predicate,
            edge_property_projection,
            dst_property_projection,
            hop_aux_binding,
            emit_edge_binding,
        } => {
            ensure_simple_expand(label_expr, var_len, hop_aux_binding)?;
            stream_expand(
                store,
                ops,
                op_idx,
                row,
                parameters,
                src,
                edge,
                dst,
                *direction,
                label.as_ref(),
                EdgeSequenceOrder::Descending,
                &[],
                *emit_edge_binding,
                indexed_edge_equality.as_ref(),
                edge_value_predicate.as_ref(),
                edge_vector_predicate.as_ref(),
                edge_property_projection.as_deref(),
                dst_property_projection.as_deref(),
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
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
            edge_value_predicate,
            edge_vector_predicate,
            dst_filter,
            edge_property_projection,
            dst_property_projection,
            hop_aux_binding,
            emit_edge_binding,
        } => {
            ensure_simple_expand(label_expr, var_len, hop_aux_binding)?;
            stream_expand(
                store,
                ops,
                op_idx,
                row,
                parameters,
                src,
                edge,
                dst,
                *direction,
                label.as_ref(),
                EdgeSequenceOrder::Descending,
                dst_filter,
                *emit_edge_binding,
                indexed_edge_equality.as_ref(),
                edge_value_predicate.as_ref(),
                edge_vector_predicate.as_ref(),
                edge_property_projection.as_deref(),
                dst_property_projection.as_deref(),
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        }
        _ => unreachable!("limited streaming prefix only contains supported operators"),
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_node_scan(
    store: &GraphStore,
    ops: &[PlanOp],
    op_idx: usize,
    row: PlanRow,
    variable: &Str,
    label: Option<&Str>,
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    sink: &mut LimitedRows,
    clears_active_aggregate: &mut bool,
) -> Result<bool, PlanQueryError> {
    let label_id = label.and_then(|label| store.vertex_label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(false);
    }

    let filter_migration_visibility = migration_visibility_filter_needed();
    for raw in 0..u32::from(store.vertex_count()) {
        #[cfg(test)]
        NODE_SCAN_VISITS.with(|visits| visits.set(visits.get() + 1));
        let vertex_id = VertexId::from(raw);
        let Some(vertex) = store.vertex(vertex_id) else {
            continue;
        };
        if vertex.is_tombstone()
            || (filter_migration_visibility && !vertex_visible_to_query(vertex_id))
        {
            continue;
        }
        if let Some(filter) = label_id
            && !store.vertex_has_label(vertex_id, vertex, filter)
        {
            continue;
        }
        let scanned = row.fork([(variable.as_ref(), PlanBinding::Vertex(vertex_id))]);
        if stream_row_through_ops(
            store,
            ops,
            op_idx + 1,
            scanned,
            parameters,
            caller,
            gleaph_weight_decoders,
            evaluator,
            sink,
            clears_active_aggregate,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn local_vertex_binding_for_limited_streaming_expand(
    row: &PlanRow,
    variable: &str,
) -> Result<Option<VertexId>, PlanQueryError> {
    match row.get(variable) {
        Some(PlanBinding::Value(Value::Null)) => Ok(None),
        Some(PlanBinding::Vertex(vertex_id)) => Ok(Some(*vertex_id)),
        Some(PlanBinding::RemoteVertex(_)) => Err(PlanQueryError::UnsupportedOp(
            LIMITED_STREAMING_REMOTE_EXPAND_SOURCE,
        )),
        Some(_) | None => Err(PlanQueryError::MissingBinding {
            variable: variable.to_owned(),
        }),
    }
}

#[allow(clippy::too_many_arguments)]
fn stream_expand(
    store: &GraphStore,
    ops: &[PlanOp],
    op_idx: usize,
    row: PlanRow,
    parameters: &BTreeMap<String, Value>,
    src: &Str,
    edge: &Str,
    dst: &Str,
    direction: EdgeDirection,
    label: Option<&Str>,
    sequence_order: EdgeSequenceOrder,
    dst_filter: &[Expr],
    emit_edge_binding: bool,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_value_predicate: Option<&EdgeValuePredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    edge_property_projection: Option<&[Str]>,
    dst_property_projection: Option<&[Str]>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
    sink: &mut LimitedRows,
    clears_active_aggregate: &mut bool,
) -> Result<bool, PlanQueryError> {
    let label_id = label.and_then(|label| store.edge_label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(false);
    }

    let Some(src_id) = local_vertex_binding_for_limited_streaming_expand(&row, src.as_ref())?
    else {
        return Ok(false);
    };
    let dst_only_prefilter = dst_filter_is_dst_vertex_only(dst_filter, dst.as_ref());
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let dst_key = dst.to_string();
    let csr_expand_fast_path = (edge_value_predicate.is_none() && edge_vector_predicate.is_none())
        .then(|| csr_offset_fast_path_for_expand(direction, label_id, sequence_order))
        .flatten();

    let csr_offset_fast_path = (indexed_edge_equality.is_none()
        && edge_value_predicate.is_none()
        && edge_vector_predicate.is_none()
        && dst_filter.is_empty()
        && !matches!(
            row.get(dst.as_ref()),
            Some(PlanBinding::Vertex(_)) | Some(PlanBinding::RemoteVertex(_))
        )
        && streaming_ops_preserve_row_cardinality_after(ops, op_idx + 1))
    .then_some(csr_expand_fast_path)
    .flatten();

    if let Some(fast_path) = csr_offset_fast_path {
        let mut offset_slot = sink.offset_remaining;
        let mut visit = |edge: Edge| {
            #[cfg(test)]
            EDGE_STREAM_VISITS.with(|visits| visits.set(visits.get() + 1));
            // `skip_then_visit_each_*` applies the global OFFSET inside the CSR iterator; clear
            // the sink-side skip before downstream `LimitedRows::push`.
            sink.offset_remaining = 0;
            let Some(edge_dst) = ExpandDst::from_edge(store, &edge)? else {
                return Ok(false);
            };
            if !expand_accepts_remote_dst(dst_only_prefilter, dst_property_projection)
                && !edge_dst.requires_local_vertex_data()
            {
                return Ok(false);
            }
            let edge_binding = edge_binding_for_expand(store, src_id, direction, edge)?;
            let expanded = build_expanded_row(
                None,
                store,
                &row,
                edge_key.as_deref(),
                dst_key.as_str(),
                edge_dst,
                edge_binding,
                edge_property_projection,
                dst_property_projection,
            )?;
            stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                expanded,
                parameters,
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        };
        let res =
            visit_csr_expand_fast_path(store, src_id, fast_path, &mut offset_slot, &mut visit);
        sink.offset_remaining = offset_slot;
        return match res {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.into()),
        };
    }

    if let Some(fast_path) = csr_expand_fast_path {
        let edge_equality_filter =
            edge_equality_stream_filter(store, indexed_edge_equality, parameters)?;
        if matches!(edge_equality_filter, EdgeEqualityStreamFilter::NoMatches) {
            return Ok(false);
        }
        let mut offset_slot = 0;
        let mut visit = |edge: Edge| {
            #[cfg(test)]
            EDGE_STREAM_VISITS.with(|visits| visits.set(visits.get() + 1));
            let Some(edge_dst) = ExpandDst::from_edge(store, &edge)? else {
                return Ok(false);
            };
            let edge_binding = edge_binding_for_expand(store, src_id, direction, edge)?;
            if !edge_matches_stream_filter(
                store,
                &edge_equality_filter,
                direction,
                edge_binding.handle.owner_vertex_id,
                LaraLabelId::from_raw(edge.label_id),
                edge.edge_slot_index,
            )? {
                return Ok(false);
            }
            if !expand_dst_matches_prebound_vertex(&row, dst, edge_dst) {
                return Ok(false);
            }
            if let ExpandDst::Local(dst_id) = edge_dst {
                if dst_only_prefilter
                    && !vertex_row_matches_dst_filters(
                        store,
                        parameters,
                        dst,
                        dst_id,
                        dst_filter,
                        caller,
                        gleaph_weight_decoders,
                    )?
                {
                    return Ok(false);
                }
            } else if !expand_accepts_remote_dst(dst_only_prefilter, dst_property_projection) {
                return Ok(false);
            }
            let expanded = build_expanded_row(
                None,
                store,
                &row,
                edge_key.as_deref(),
                dst_key.as_str(),
                edge_dst,
                edge_binding,
                edge_property_projection,
                dst_property_projection,
            )?;
            if !dst_only_prefilter && !row_matches_all(evaluator, &expanded, dst_filter)? {
                return Ok(false);
            }
            stream_row_through_ops(
                store,
                ops,
                op_idx + 1,
                expanded,
                parameters,
                caller,
                gleaph_weight_decoders,
                evaluator,
                sink,
                clears_active_aggregate,
            )
        };
        let res =
            visit_csr_expand_fast_path(store, src_id, fast_path, &mut offset_slot, &mut visit);
        return match res {
            Ok(Ok(done)) => Ok(done),
            Ok(Err(e)) => Err(e),
            Err(e) => Err(e.into()),
        };
    }

    let mut candidates = Vec::new();
    expand_candidates_into(
        store,
        src_id,
        direction,
        label_id,
        EdgeSequenceOrder::Descending,
        indexed_edge_equality,
        edge_value_predicate,
        edge_vector_predicate,
        parameters,
        &mut candidates,
    )?;
    for (edge_dst, edge_binding) in candidates.iter().copied() {
        if !expand_dst_matches_prebound_vertex(&row, dst, edge_dst) {
            continue;
        }
        if let ExpandDst::Local(dst_id) = edge_dst {
            if dst_only_prefilter
                && !vertex_row_matches_dst_filters(
                    store,
                    parameters,
                    dst,
                    dst_id,
                    dst_filter,
                    caller,
                    gleaph_weight_decoders,
                )?
            {
                continue;
            }
        } else if !expand_accepts_remote_dst(dst_only_prefilter, dst_property_projection) {
            continue;
        }
        let expanded = build_expanded_row(
            None,
            store,
            &row,
            edge_key.as_deref(),
            dst_key.as_str(),
            edge_dst,
            edge_binding,
            edge_property_projection,
            dst_property_projection,
        )?;
        if !dst_only_prefilter && !row_matches_all(evaluator, &expanded, dst_filter)? {
            continue;
        }
        if stream_row_through_ops(
            store,
            ops,
            op_idx + 1,
            expanded,
            parameters,
            caller,
            gleaph_weight_decoders,
            evaluator,
            sink,
            clears_active_aggregate,
        )? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn property_id_for_scan(store: &GraphStore, property_name: &str) -> Result<u32, PlanQueryError> {
    store
        .property_id(property_name)
        .map(|p| p.raw())
        .ok_or(PlanQueryError::UnsupportedOp("IndexScan.unknown_property"))
}

pub(crate) fn resolve_scan_value_bytes(
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

pub(crate) fn federation_routing(
    store: &GraphStore,
) -> Result<crate::facade::FederationRouting, PlanQueryError> {
    store
        .federation_routing()
        .ok_or(PlanQueryError::UnsupportedOp("IndexScan(no shard routing)"))
}

async fn materialize_federated_index_hits(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    variable: &str,
    hits: &[PostingHit],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let routing = federation_routing(store)?;
    let local_shard = routing.shard_id;
    let mut logical_cache: std::collections::HashMap<
        gleaph_graph_kernel::federation::PhysicalPlacementKey,
        Option<gleaph_graph_kernel::federation::LogicalVertexId>,
    > = std::collections::HashMap::new();
    let mut out = Vec::new();
    for row in rows {
        for hit in hits {
            let binding = if hit.shard_id == local_shard {
                let vertex_id = VertexId::from(hit.vertex_id);
                let Some(vertex) = store.vertex(vertex_id) else {
                    continue;
                };
                if vertex.is_tombstone() {
                    continue;
                }
                PlanBinding::Vertex(vertex_id)
            } else {
                let key = gleaph_graph_kernel::federation::PhysicalPlacementKey::from_posting_hit(
                    hit.shard_id,
                    hit.vertex_id,
                );
                let logical = match logical_cache.get(&key) {
                    Some(cached) => *cached,
                    None => {
                        let resolved = placement::resolve_logical_at(
                            routing.router_canister,
                            hit.shard_id,
                            hit.vertex_id,
                        )
                        .await
                        .map_err(|e| {
                            PlanQueryError::FederatedIndexCall {
                                op: "resolve_logical_at",
                                detail: e.to_string(),
                            }
                        })?;
                        logical_cache.insert(key, resolved);
                        resolved
                    }
                };
                let Some(logical_vertex_id) = logical else {
                    continue;
                };
                PlanBinding::RemoteVertex(logical_vertex_id)
            };
            out.push(row.fork([(variable, binding)]));
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_index_scan(
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
    materialize_federated_index_hits(store, rows, variable, &hits).await
}

pub(crate) async fn execute_conditional_index_scan(
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
            let pid = property_id_for_scan(store, c.property.as_ref())?;
            let hits = if c.cmp == CmpOp::Eq {
                ix.lookup_equal(pid, bytes).await?
            } else {
                let req = cmp_to_posting_range_request(c.cmp, bytes)?;
                ix.lookup_range(pid, &req).await?
            };
            return materialize_federated_index_hits(store, rows, c.variable.as_ref(), &hits).await;
        }
    }
    execute_node_scan(store, rows, fallback_variable, fallback_label)
}

pub(crate) async fn execute_index_intersection(
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
    let routing = federation_routing(store)?;
    let local_shard = routing.shard_id;
    let mut sets: Vec<IntSet<u64>> = Vec::with_capacity(scans.len());
    for spec in scans {
        if spec.cmp != CmpOp::Eq {
            return Err(PlanQueryError::UnsupportedOp("IndexIntersection.cmp"));
        }
        let pid = property_id_for_scan(store, spec.property.as_ref())?;
        let Some(bytes) = resolve_scan_value_bytes(&spec.value, parameters)? else {
            return Ok(Vec::new());
        };
        let hits = ix.lookup_equal(pid, bytes).await?;
        let mut hs = IntSet::default();
        let mut logical_cache = std::collections::HashMap::new();
        for h in hits {
            let key = if h.shard_id == local_shard {
                let vid = VertexId::from(h.vertex_id);
                let Some(vertex) = store.vertex(vid) else {
                    continue;
                };
                if vertex.is_tombstone() {
                    continue;
                }
                (1u64 << 63) | u64::from(u32::from(vid))
            } else {
                let physical =
                    gleaph_graph_kernel::federation::PhysicalPlacementKey::from_posting_hit(
                        h.shard_id,
                        h.vertex_id,
                    );
                let logical = match logical_cache.get(&physical) {
                    Some(cached) => *cached,
                    None => {
                        let resolved = placement::resolve_logical_at(
                            routing.router_canister,
                            h.shard_id,
                            h.vertex_id,
                        )
                        .await
                        .map_err(|e| {
                            PlanQueryError::FederatedIndexCall {
                                op: "resolve_logical_at",
                                detail: e.to_string(),
                            }
                        })?;
                        logical_cache.insert(physical, resolved);
                        resolved
                    }
                };
                let Some(logical) = logical else {
                    continue;
                };
                logical
            };
            hs.insert(key);
        }
        sets.push(hs);
    }
    let mut intersection: Option<IntSet<u64>> = None;
    for s in sets {
        intersection = Some(match intersection {
            None => s,
            Some(prev) => prev.intersection(&s).copied().collect::<IntSet<_>>(),
        });
    }
    let Some(ids) = intersection else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for row in rows {
        for id in &ids {
            let binding = if *id >> 63 != 0 {
                PlanBinding::Vertex(VertexId::from((*id & !(1u64 << 63)) as u32))
            } else {
                PlanBinding::RemoteVertex(*id)
            };
            out.push(row.fork([(variable, binding)]));
        }
    }
    Ok(out)
}

pub(crate) fn execute_node_scan(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    variable: &Str,
    label: Option<&Str>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = label.and_then(|label| store.vertex_label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(Vec::new());
    }

    let mut out = Vec::new();
    for row in rows {
        for raw in 0..u32::from(store.vertex_count()) {
            #[cfg(test)]
            NODE_SCAN_VISITS.with(|visits| visits.set(visits.get() + 1));
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
            out.push(row.fork([(variable.as_ref(), PlanBinding::Vertex(vertex_id))]));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use pollster;
    #[test]
    fn federated_index_scan_materializes_foreign_shard_hit_as_remote_vertex() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let _ = store
            .insert_vertex_named(["ForeignIndexScanSeed"], [("age", Value::Uint8(1))])
            .expect("register age property");
        native_test_register_physical_placement(9, 42, 9001);
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 9,
            vertex_id: 42,
        });
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "age".into(),
            value: ScanValue::Literal(Value::Int64(5)),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let rows = pollster::block_on(execute_plan_query_bindings(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute federated index scan");

        assert_eq!(rows.len(), 1);
        assert!(matches!(
            rows[0].get("n"),
            Some(PlanBinding::RemoteVertex(9001))
        ));
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

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
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
    fn equality_index_scan_unifies_decimal_and_integer_key_with_final_filter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let price = gleaph_gql::types::Decimal::parse("5.00").expect("decimal");
        let vid = store
            .insert_vertex_named(["IndexScanDecimalEq"], [("price", Value::Decimal(price))])
            .expect("insert vertex");
        let pid = store.property_id("price").expect("price property").raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "price".into(),
                value: ScanValue::Literal(Value::Int64(5)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "price")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Int64(5)))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute decimal equality index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&Value::Decimal(price))
                .unwrap()
                .unwrap()
        );
    }
    #[test]
    fn equality_index_scan_unifies_float_and_decimal_key_with_final_filter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let bound = gleaph_gql::types::Decimal::parse("1.5").expect("decimal");
        let vid = store
            .insert_vertex_named(["IndexScanFloatEq"], [("score", Value::Float64(1.5))])
            .expect("insert vertex");
        let pid = store.property_id("score").expect("score property").raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "score".into(),
                value: ScanValue::Literal(Value::Decimal(bound)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "score")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(bound)))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute float equality index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&Value::Float64(1.5))
                .unwrap()
                .unwrap()
        );
    }
    #[test]
    fn equality_index_scan_final_filter_drops_inexact_float_decimal_candidate() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let bound = gleaph_gql::types::Decimal::parse("0.1").expect("decimal");
        let vid = store
            .insert_vertex_named(["IndexScanFloatInexact"], [("score", Value::Float64(0.1))])
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "score".into(),
                value: ScanValue::Literal(Value::Decimal(bound)),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "score")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Decimal(bound)))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute inexact float equality index scan");

        assert!(result.rows.is_empty());
    }
    #[test]
    fn equality_index_scan_matches_list_valued_posting() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let stored = Value::List(vec![Value::Uint8(1), Value::Text("a".into())]);
        let bound = Value::List(vec![Value::Int64(1), Value::Text("a".into())]);
        let vid = store
            .insert_vertex_named(["IndexScanListEq"], [("tags", stored.clone())])
            .expect("insert vertex");
        let pid = store.property_id("tags").expect("tags property").raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "tags".into(),
                value: ScanValue::Literal(bound.clone()),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "tags")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(bound))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute list equality index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&stored).unwrap().unwrap()
        );
    }
    #[test]
    fn equality_index_scan_matches_record_valued_posting_independent_of_field_order() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let stored = Value::Record(vec![
            ("b".into(), Value::Int64(2)),
            ("a".into(), Value::Int64(1)),
        ]);
        let bound = Value::Record(vec![
            ("a".into(), Value::Int64(1)),
            ("b".into(), Value::Int64(2)),
        ]);
        let vid = store
            .insert_vertex_named(["IndexScanRecordEq"], [("profile", stored.clone())])
            .expect("insert vertex");
        let pid = store
            .property_id("profile")
            .expect("profile property")
            .raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "profile".into(),
                value: ScanValue::Literal(bound.clone()),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "profile")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(bound))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute record equality index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&stored).unwrap().unwrap()
        );
    }
    #[test]
    fn equality_index_scan_final_filter_drops_inexact_nested_numeric_candidate() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let stored = Value::Record(vec![("score".into(), Value::Float64(0.1))]);
        let bound = Value::Record(vec![(
            "score".into(),
            Value::Decimal(gleaph_gql::types::Decimal::parse("0.1").expect("decimal")),
        )]);
        let vid = store
            .insert_vertex_named(["IndexScanRecordInexact"], [("profile", stored)])
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "profile".into(),
                value: ScanValue::Literal(bound.clone()),
                cmp: CmpOp::Eq,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "profile")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(bound))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute record inexact equality index scan");

        assert!(result.rows.is_empty());
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

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
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
    fn executes_list_range_index_scan_with_lookup_range() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let hit = store
            .insert_vertex_named(
                ["IndexScanListRange"],
                [("tags", Value::List(vec![Value::Int64(2)]))],
            )
            .expect("insert hit");
        let miss = store
            .insert_vertex_named(
                ["IndexScanListRange"],
                [("tags", Value::List(vec![Value::Int64(0)]))],
            )
            .expect("insert miss");
        let pid = store.property_id("tags").expect("tags property").raw();
        let index = MockPropertyIndex::default();
        index.range_hits.borrow_mut().extend([
            PostingHit {
                shard_id: 7,
                vertex_id: u32::try_from(u64::from(hit)).unwrap(),
            },
            PostingHit {
                shard_id: 7,
                vertex_id: u32::try_from(u64::from(miss)).unwrap(),
            },
        ]);
        let bound = Value::List(vec![Value::Int64(1)]);
        let plan = plan(vec![
            PlanOp::IndexScan {
                variable: "n".into(),
                property: "tags".into(),
                value: ScanValue::Literal(bound.clone()),
                cmp: CmpOp::Ge,
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "tags")),
                    op: CmpOp::Ge,
                    right: Box::new(Expr::new(ExprKind::Literal(bound.clone()))),
                })],
                stage: 0,
            },
        ]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute list range index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.range_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert!(matches!(
            &calls[0].1,
            PostingRangeRequest::Ge(bytes)
                if bytes == &value_to_index_key_bytes(&bound).unwrap().unwrap()
        ));
        assert!(index.equal_calls.borrow().is_empty());
    }
    #[test]
    fn executes_record_range_index_scan_with_lookup_range() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let hit = store
            .insert_vertex_named(
                ["IndexScanRecordRange"],
                [(
                    "profile",
                    Value::Record(vec![
                        ("a".into(), Value::Int64(1)),
                        ("b".into(), Value::Int64(1)),
                    ]),
                )],
            )
            .expect("insert hit");
        let pid = store
            .property_id("profile")
            .expect("profile property")
            .raw();
        let index = MockPropertyIndex::default();
        index.range_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(hit)).unwrap(),
        });
        let bound = Value::Record(vec![
            ("b".into(), Value::Int64(2)),
            ("a".into(), Value::Int64(1)),
        ]);
        let canonical_bound = Value::Record(vec![
            ("a".into(), Value::Int64(1)),
            ("b".into(), Value::Int64(2)),
        ]);
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "profile".into(),
            value: ScanValue::Literal(bound),
            cmp: CmpOp::Lt,
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute record range index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.range_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert!(matches!(
            &calls[0].1,
            PostingRangeRequest::Lt(bytes)
                if bytes == &value_to_index_key_bytes(&canonical_bound).unwrap().unwrap()
        ));
        assert!(index.equal_calls.borrow().is_empty());
    }
    #[test]
    fn executes_orderable_extension_equality_index_scan() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let value = orderable_ext(7);
        store
            .insert_vertex_named(
                ["IndexScanExtensionEqCatalog"],
                [("principal", Value::Text("catalog".into()))],
            )
            .expect("insert catalog vertex");
        let vid = store
            .insert_vertex_named(["IndexScanExtensionEq"], Vec::<(&str, Value)>::new())
            .expect("insert vertex");
        let pid = store.property_id("principal").expect("property").raw();
        let index = MockPropertyIndex::default();
        index.equal_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "principal".into(),
            value: ScanValue::Literal(value.clone()),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute extension equality index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.equal_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert_eq!(
            calls[0].1,
            value_to_index_key_bytes(&value).unwrap().unwrap()
        );
        assert!(index.range_calls.borrow().is_empty());
    }
    #[test]
    fn executes_orderable_extension_range_index_scan() {
        let store = GraphStore::new();
        configure_test_index(&store);
        let bound = orderable_ext(7);
        store
            .insert_vertex_named(
                ["IndexScanExtensionRangeCatalog"],
                [("principal", Value::Text("catalog".into()))],
            )
            .expect("insert catalog vertex");
        let vid = store
            .insert_vertex_named(["IndexScanExtensionRange"], Vec::<(&str, Value)>::new())
            .expect("insert vertex");
        let pid = store.property_id("principal").expect("property").raw();
        let index = MockPropertyIndex::default();
        index.range_hits.borrow_mut().push(PostingHit {
            shard_id: 7,
            vertex_id: u32::try_from(u64::from(vid)).unwrap(),
        });
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "principal".into(),
            value: ScanValue::Literal(bound.clone()),
            cmp: CmpOp::Ge,
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &params(),
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("execute extension range index scan");

        assert_eq!(result.rows.len(), 1);
        let calls = index.range_calls.borrow();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, pid);
        assert!(matches!(
            &calls[0].1,
            PostingRangeRequest::Ge(bytes)
                if bytes == &value_to_index_key_bytes(&bound).unwrap().unwrap()
        ));
        assert!(index.equal_calls.borrow().is_empty());
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
        parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "tags".into(),
            value: ScanValue::Parameter("tags".into()),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let err = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect_err("unsupported parameter should fail");

        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }
    #[test]
    fn index_scan_rejects_non_orderable_extension_parameter_value() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(
                ["IndexScanBadExtensionParam"],
                [("principal", Value::Text("catalog".into()))],
            )
            .expect("insert catalog vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("principal".into(), non_orderable_ext());
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "principal".into(),
            value: ScanValue::Parameter("principal".into()),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let err = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect_err("non-orderable extension parameter should fail");

        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }
    #[test]
    fn range_index_scan_rejects_unsupported_nested_parameter_value() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(["IndexScanBadRangeParam"], [("tags", Value::List(vec![]))])
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "tags".into(),
            value: ScanValue::Parameter("tags".into()),
            cmp: CmpOp::Ge,
            property_projection: None,
        }]);

        let err = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect_err("unsupported range parameter should fail");

        assert!(matches!(err, PlanQueryError::InvalidExpressionValue { .. }));
    }
    #[test]
    fn index_scan_rejects_non_finite_float_parameter_value() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(["IndexScanBadFloatParam"], [("score", Value::Float64(1.0))])
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("score".into(), Value::Float64(f64::INFINITY));
        let plan = plan(vec![PlanOp::IndexScan {
            variable: "n".into(),
            property: "score".into(),
            value: ScanValue::Parameter("score".into()),
            cmp: CmpOp::Eq,
            property_projection: None,
        }]);

        let err = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect_err("non-finite parameter should fail");

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
        parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
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

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("conditional fallback");

        assert_eq!(result.rows.len(), 1);
        assert!(index.equal_calls.borrow().is_empty());
        assert!(index.range_calls.borrow().is_empty());
    }
    #[test]
    fn conditional_index_scan_falls_back_for_non_orderable_extension_parameter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(
                ["IndexScanConditionalExtensionFallback"],
                Vec::<(&str, Value)>::new(),
            )
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("principal".into(), non_orderable_ext());
        let plan = plan(vec![PlanOp::ConditionalIndexScan {
            candidates: vec![ConditionalScanCandidate {
                param_name: "principal".into(),
                property: "principal".into(),
                variable: "n".into(),
                cmp: CmpOp::Eq,
            }],
            fallback_label: Some("IndexScanConditionalExtensionFallback".into()),
            fallback_variable: "n".into(),
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("conditional fallback");

        assert_eq!(result.rows.len(), 1);
        assert!(index.equal_calls.borrow().is_empty());
        assert!(index.range_calls.borrow().is_empty());
    }
    #[test]
    fn conditional_range_index_scan_falls_back_for_unsupported_nested_parameter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(
                ["IndexScanConditionalRangeFallback"],
                [("tags", Value::List(vec![]))],
            )
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("tags".into(), Value::List(vec![non_orderable_ext()]));
        let plan = plan(vec![PlanOp::ConditionalIndexScan {
            candidates: vec![ConditionalScanCandidate {
                param_name: "tags".into(),
                property: "tags".into(),
                variable: "n".into(),
                cmp: CmpOp::Ge,
            }],
            fallback_label: Some("IndexScanConditionalRangeFallback".into()),
            fallback_variable: "n".into(),
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("conditional fallback");

        assert_eq!(result.rows.len(), 1);
        assert!(index.equal_calls.borrow().is_empty());
        assert!(index.range_calls.borrow().is_empty());
    }
    #[test]
    fn conditional_index_scan_falls_back_for_non_finite_float_parameter() {
        let store = GraphStore::new();
        configure_test_index(&store);
        store
            .insert_vertex_named(
                ["IndexScanConditionalFloatFallback"],
                [("score", Value::Float64(1.0))],
            )
            .expect("insert vertex");
        let index = MockPropertyIndex::default();
        let mut parameters = params();
        parameters.insert("score".into(), Value::Float64(f64::NAN));
        let plan = plan(vec![PlanOp::ConditionalIndexScan {
            candidates: vec![ConditionalScanCandidate {
                param_name: "score".into(),
                property: "score".into(),
                variable: "n".into(),
                cmp: CmpOp::Eq,
            }],
            fallback_label: Some("IndexScanConditionalFloatFallback".into()),
            fallback_variable: "n".into(),
            property_projection: None,
        }]);

        let result = pollster::block_on(execute_plan_query(
            &store,
            &plan,
            &parameters,
            Some(&index),
            GqlExecutionContext::default(),
        ))
        .expect("conditional fallback");

        assert_eq!(result.rows.len(), 1);
        assert!(index.equal_calls.borrow().is_empty());
        assert!(index.range_calls.borrow().is_empty());
    }
    #[test]
    fn planner_limit_stops_node_scan_after_enough_rows() {
        let store = GraphStore::new();
        store
            .insert_vertex_named(
                ["PlannerQueryLazyLimit"],
                [("name", Value::Text("first".into()))],
            )
            .expect("insert first");
        for i in 0..64 {
            store
                .insert_vertex_named(
                    ["PlannerQueryLazyLimit"],
                    [("name", Value::Text(format!("tail {i}")))],
                )
                .expect("insert tail");
        }
        let plan = plan_gql("MATCH (n:PlannerQueryLazyLimit) RETURN n.name LIMIT 1");

        reset_node_scan_visits();
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(text_column(&result, "n.name"), vec!["first"]);
        assert_eq!(node_scan_visits(), 1);
    }
    #[test]
    fn planner_limit_stops_after_filter_accepts_enough_rows() {
        let store = GraphStore::new();
        for i in 0..10 {
            store
                .insert_vertex_named(
                    ["PlannerQueryLazyFilterLimit"],
                    [
                        ("name", Value::Text(format!("drop {i}"))),
                        ("keep", Value::Bool(false)),
                    ],
                )
                .expect("insert dropped");
        }
        for name in ["keep a", "keep b"] {
            store
                .insert_vertex_named(
                    ["PlannerQueryLazyFilterLimit"],
                    [
                        ("name", Value::Text(name.into())),
                        ("keep", Value::Bool(true)),
                    ],
                )
                .expect("insert kept");
        }
        for i in 0..32 {
            store
                .insert_vertex_named(
                    ["PlannerQueryLazyFilterLimit"],
                    [
                        ("name", Value::Text(format!("unvisited {i}"))),
                        ("keep", Value::Bool(true)),
                    ],
                )
                .expect("insert tail");
        }
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "n".into(),
                label: Some("PlannerQueryLazyFilterLimit".into()),
                property_projection: None,
            },
            PlanOp::PropertyFilter {
                predicates: vec![Expr::new(ExprKind::Compare {
                    left: Box::new(prop("n", "keep")),
                    op: CmpOp::Eq,
                    right: Box::new(Expr::new(ExprKind::Literal(Value::Bool(true)))),
                })],
                stage: 0,
            },
            PlanOp::Limit {
                count: Some(Expr::new(ExprKind::Literal(Value::Int64(2)))),
                offset: None,
            },
            PlanOp::Project {
                columns: vec![project(prop("n", "name"), "n.name")],
                distinct: false,
            },
        ]);

        reset_node_scan_visits();
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(text_column(&result, "n.name"), vec!["keep a", "keep b"]);
        assert_eq!(node_scan_visits(), 12);
    }
    #[test]
    fn order_by_limit_remains_a_materializing_barrier() {
        let store = GraphStore::new();
        for name in ["c", "a", "b"] {
            store
                .insert_vertex_named(
                    ["PlannerQueryLazyLimitSort"],
                    [("name", Value::Text(name.into()))],
                )
                .expect("insert vertex");
        }
        let plan =
            plan_gql("MATCH (n:PlannerQueryLazyLimitSort) RETURN n.name ORDER BY n.name LIMIT 1");

        reset_node_scan_visits();
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute planned query");

        assert_eq!(text_column(&result, "n.name"), vec!["a"]);
        assert_eq!(node_scan_visits(), 3);
    }
    #[test]
    fn labeled_expand_limit_offset_pages_latest_edges() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["LazyEdgePageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for i in 0..5 {
            let dst = store
                .insert_vertex_named(
                    ["LazyEdgePageTarget"],
                    [("name", Value::Text(format!("edge {i}")))],
                )
                .expect("insert target");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Some("LazyEdgePageRel"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let first_page = plan_gql(
            "MATCH (a:LazyEdgePageSource)-[:LazyEdgePageRel]->(b) RETURN b.name LIMIT 2 OFFSET 0",
        );
        let second_page = plan_gql(
            "MATCH (a:LazyEdgePageSource)-[:LazyEdgePageRel]->(b) RETURN b.name LIMIT 2 OFFSET 2",
        );

        reset_edge_stream_visits();
        let first = store
            .execute_plan_query(&first_page, &params(), GqlExecutionContext::default())
            .expect("execute first page");
        assert_eq!(edge_stream_visits(), 2);

        reset_edge_stream_visits();
        let second = store
            .execute_plan_query(&second_page, &params(), GqlExecutionContext::default())
            .expect("execute second page");
        assert_eq!(edge_stream_visits(), 2);

        assert_eq!(text_column(&first, "b.name"), vec!["edge 4", "edge 3"]);
        assert_eq!(text_column(&second, "b.name"), vec!["edge 2", "edge 1"]);
    }
    #[test]
    fn gleaph_sequence_asc_pages_labeled_edges_in_insertion_order() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["SeqAscPageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for i in 0..5 {
            let dst = store
                .insert_vertex_named(
                    ["SeqAscPageTarget"],
                    [("name", Value::Text(format!("seq edge {i}")))],
                )
                .expect("insert target");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Some("SeqAscPageRel"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page = plan_gql(
            "MATCH (a:SeqAscPageSource)-[e:SeqAscPageRel]->(b) \
             ORDER BY GLEAPH.SEQUENCE(e) ASC LIMIT 2 OFFSET 1 RETURN b.name",
        );

        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute asc page");

        assert_eq!(
            text_column(&result, "b.name"),
            vec!["seq edge 1", "seq edge 2"]
        );
    }
    #[test]
    fn gleaph_sequence_desc_matches_default_labeled_edge_order() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["SeqDescPageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for i in 0..4 {
            let dst = store
                .insert_vertex_named(
                    ["SeqDescPageTarget"],
                    [("name", Value::Text(format!("seq desc edge {i}")))],
                )
                .expect("insert target");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Some("SeqDescPageRel"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page = plan_gql(
            "MATCH (a:SeqDescPageSource)-[e:SeqDescPageRel]->(b) \
             ORDER BY GLEAPH.SEQUENCE(e) DESC LIMIT 2 RETURN b.name",
        );

        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute desc page");

        assert_eq!(
            text_column(&result, "b.name"),
            vec!["seq desc edge 3", "seq desc edge 2"]
        );
    }
    #[test]
    fn gleaph_sequence_rejects_unlabeled_edge_pattern() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["SeqNoLabelSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        let dst = store
            .insert_vertex_named(["SeqNoLabelTarget"], Vec::<(&str, Value)>::new())
            .expect("insert target");
        store
            .insert_directed_edge_named(src, dst, Option::<&str>::None, Vec::<(&str, Value)>::new())
            .expect("insert edge");

        let page = plan_gql(
            "MATCH (a:SeqNoLabelSource)-[e]->(b) \
             ORDER BY GLEAPH.SEQUENCE(e) ASC RETURN b",
        );

        let err = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect_err("unlabeled sequence order should fail");

        assert!(err.to_string().contains("single fixed edge label"), "{err}");
    }
    #[test]
    fn unlabeled_directed_expand_limit_offset_uses_latest_edges() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["LazyUnlabeledPageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for i in 0..5 {
            let dst = store
                .insert_vertex_named(
                    ["LazyUnlabeledPageTarget"],
                    [("name", Value::Text(format!("unlabeled edge {i}")))],
                )
                .expect("insert target");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Option::<&str>::None,
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page =
            plan_gql("MATCH (a:LazyUnlabeledPageSource)-[]->(b) RETURN b.name LIMIT 2 OFFSET 2");

        reset_edge_stream_visits();
        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute page");

        assert_eq!(
            text_column(&result, "b.name"),
            vec!["unlabeled edge 2", "unlabeled edge 1"]
        );
        assert_eq!(edge_stream_visits(), 2);
    }
    #[test]
    fn reverse_expand_limit_offset_uses_latest_in_edges() {
        let store = GraphStore::new();
        let dst = store
            .insert_vertex_named(["LazyReversePageTarget"], Vec::<(&str, Value)>::new())
            .expect("insert target");
        for i in 0..5 {
            let src = store
                .insert_vertex_named(
                    ["LazyReversePageSource"],
                    [("name", Value::Text(format!("reverse edge {i}")))],
                )
                .expect("insert source");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Some("LazyReversePageRel"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page = plan_gql(
            "MATCH (b:LazyReversePageTarget)<-[:LazyReversePageRel]-(a) RETURN a.name LIMIT 2 OFFSET 2",
        );

        reset_edge_stream_visits();
        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute page");

        assert_eq!(
            text_column(&result, "a.name"),
            vec!["reverse edge 2", "reverse edge 1"]
        );
        assert_eq!(edge_stream_visits(), 2);
    }
    #[test]
    fn undirected_expand_limit_offset_uses_latest_edges() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["LazyUndirectedPageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for i in 0..5 {
            let dst = store
                .insert_vertex_named(
                    ["LazyUndirectedPageTarget"],
                    [("name", Value::Text(format!("undirected edge {i}")))],
                )
                .expect("insert target");
            store
                .insert_undirected_edge_named(
                    src,
                    dst,
                    Option::<&str>::None,
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page =
            plan_gql("MATCH (a:LazyUndirectedPageSource)~[]~(b) RETURN b.name LIMIT 2 OFFSET 2");

        reset_edge_stream_visits();
        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute page");

        assert_eq!(
            text_column(&result, "b.name"),
            vec!["undirected edge 2", "undirected edge 1"]
        );
        assert_eq!(edge_stream_visits(), 2);
    }
    #[test]
    fn filtered_expand_limit_offset_skips_only_matching_edges() {
        let store = GraphStore::new();
        let src = store
            .insert_vertex_named(["LazyFilteredPageSource"], Vec::<(&str, Value)>::new())
            .expect("insert source");
        for (i, keep) in [
            (0, true),
            (1, false),
            (2, true),
            (3, false),
            (4, true),
            (5, true),
        ] {
            let dst = store
                .insert_vertex_named(
                    ["LazyFilteredPageTarget"],
                    [
                        ("name", Value::Text(format!("filtered edge {i}"))),
                        ("keep", Value::Bool(keep)),
                    ],
                )
                .expect("insert target");
            store
                .insert_directed_edge_named(
                    src,
                    dst,
                    Some("LazyFilteredPageRel"),
                    Vec::<(&str, Value)>::new(),
                )
                .expect("insert edge");
        }

        let page = plan_gql(
            "MATCH (a:LazyFilteredPageSource)-[:LazyFilteredPageRel]->(b) \
             WHERE b.keep = true RETURN b.name LIMIT 2 OFFSET 1",
        );

        reset_edge_stream_visits();
        let result = store
            .execute_plan_query(&page, &params(), GqlExecutionContext::default())
            .expect("execute page");

        assert_eq!(
            text_column(&result, "b.name"),
            vec!["filtered edge 4", "filtered edge 2"]
        );
        assert_eq!(edge_stream_visits(), 4);
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
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("execute query");

        assert_eq!(result.rows.len(), 1);
        assert_eq!(
            result.rows[0].get("name"),
            Some(&Value::Text("Node Alice".into()))
        );
    }
    #[test]
    fn indexed_expand_limit_offset_skips_only_matching_edges() {
        let store = GraphStore::new();
        let a = store
            .insert_vertex_named(["IdxEqPageA"], Vec::<(&str, Value)>::new())
            .expect("a");
        for (i, weight) in [(0, 5), (1, 9), (2, 5), (3, 9), (4, 5), (5, 5)] {
            let b = store
                .insert_vertex_named(
                    ["IdxEqPageB"],
                    [("name", Value::Text(format!("indexed edge {i}")))],
                )
                .expect("b");
            store
                .insert_directed_edge_named(
                    a,
                    b,
                    Some("IdxEqPageRel"),
                    [("weight", Value::Int64(weight))],
                )
                .expect("edge");
        }
        let plan = plan(vec![
            PlanOp::NodeScan {
                variable: "a".into(),
                label: Some("IdxEqPageA".into()),
                property_projection: None,
            },
            PlanOp::Expand {
                src: "a".into(),
                edge: "e".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: Some("IdxEqPageRel".into()),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: Some(("weight".into(), ScanValue::Literal(Value::Int64(5)))),
                edge_value_predicate: None,
                edge_vector_predicate: None,
                edge_property_projection: None,
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
            },
            PlanOp::Project {
                columns: vec![project(prop("b", "name"), "name")],
                distinct: false,
            },
            PlanOp::Limit {
                count: Some(Expr::new(ExprKind::Literal(Value::Int64(2)))),
                offset: Some(Expr::new(ExprKind::Literal(Value::Int64(1)))),
            },
        ]);

        reset_edge_stream_visits();
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("indexed expand");

        assert_eq!(
            text_column(&result, "name"),
            vec!["indexed edge 4", "indexed edge 2"]
        );
        assert_eq!(edge_stream_visits(), 4);
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
        let result = store
            .execute_plan_query(&plan, &params(), GqlExecutionContext::default())
            .expect("count");
        assert_eq!(result.rows.len(), 1);
        assert_eq!(result.rows[0].get("cnt"), Some(&Value::Int64(2)));
    }
}
