use std::collections::BTreeMap;

use candid::Principal;
use gleaph_gql::Value;
use gleaph_gql::ast::Expr;
use gleaph_gql::types::EdgeDirection;
use gleaph_gql_planner::plan::{EdgeValuePredicate, EdgeVectorPredicate, ScanValue, Str};
use gleaph_graph_kernel::entry::{Edge, EdgeLabelId, PreparedWeightDecoder};
use gleaph_graph_kernel::federation::{
    FederatedExpandArgs, FederatedExpandDirection, FederatedExpandNeighbor,
};
use ic_stable_lara::BucketLabelKey as LaraLabelId;
use ic_stable_lara::VertexId;
use ic_stable_lara::labeled::LabeledEdgeValueBatchScratch;

use super::candidates::{expand_candidates_into, expand_vector_dst_only_rows_into};
use super::predicates::PreparedEdgeVectorThreshold;
use super::{
    EdgeEqualityStreamFilter, ExpandDst, build_expanded_row, csr_offset_fast_path_for_expand,
    edge_binding_for_expand, edge_equality_stream_filter, edge_matches_stream_filter,
    expand_accepts_remote_dst, expand_dst_binding, visit_csr_expand_fast_path,
};
use crate::facade::GraphStore;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::bindings::{
    edge_binding_for_federated_expand_hit, federated_expand_label_id_raw,
};
use crate::plan::query::executor::context::{ExecuteCtx, QueryExprEvaluator};
use crate::plan::query::executor::{
    EdgeSequenceOrder, PlanBinding, dst_filter_is_dst_vertex_only, federation_routing,
    resolve_federated_traversal_vertex, row_matches_all, vertex_binding_for_traversal,
    vertex_row_matches_dst_filters,
};
use crate::plan::query::row::PlanRow;

fn expand_rows_from_federated_expand_hits(
    store: &GraphStore,
    row: &PlanRow,
    hits: &[FederatedExpandNeighbor],
    dst: &str,
    edge: &str,
    emit_edge_binding: bool,
    dst_property_projection: Option<&[Str]>,
    dst_filter: &[Expr],
    parameters: &BTreeMap<String, Value>,
    caller: Option<Principal>,
    gleaph_weight_decoders: Option<&BTreeMap<String, PreparedWeightDecoder>>,
    evaluator: &QueryExprEvaluator<'_>,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let routing = federation_routing(store)?;
    let dst_only_prefilter = dst_filter_is_dst_vertex_only(dst_filter, dst);
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let mut out = Vec::with_capacity(hits.len());
    for hit in hits {
        let dst_binding = if hit.shard_id == routing.shard_id {
            expand_dst_binding(
                store,
                ExpandDst::Local(VertexId::from(hit.neighbor_local_vertex_id)),
                dst_property_projection,
            )?
        } else {
            if dst_property_projection.is_some_and(|props| !props.is_empty()) {
                return Err(PlanQueryError::InvalidExpressionValue {
                    expression: "property projection on remote vertex binding".into(),
                });
            }
            PlanBinding::RemoteVertex(hit.neighbor_logical_vertex_id)
        };

        if let PlanBinding::Vertex(dst_id) = &dst_binding
            && dst_only_prefilter
            && !vertex_row_matches_dst_filters(
                store,
                parameters,
                &Str::from(dst),
                *dst_id,
                dst_filter,
                caller,
                gleaph_weight_decoders,
            )?
        {
            continue;
        }

        let expanded = if let Some(edge_key) = edge_key.as_ref() {
            let edge_binding = PlanBinding::Edge(edge_binding_for_federated_expand_hit(
                store,
                hit,
                routing.shard_id,
            )?);
            row.fork([(dst, dst_binding), (edge_key.as_str(), edge_binding)])
        } else {
            row.fork([(dst, dst_binding)])
        };
        if !dst_only_prefilter && !row_matches_all(evaluator, &expanded, dst_filter)? {
            continue;
        }
        out.push(expanded);
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_expand(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
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
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let store = ctx.store;
    let parameters = ctx.parameters;
    let caller = ctx.caller();
    let gleaph_weight_decoders = ctx.gleaph_weight_decoders;
    let label_id = label.and_then(|label| store.edge_label_id(label.as_ref()));
    if label.is_some() && label_id.is_none() {
        return Ok(Vec::new());
    }

    let evaluator = ctx.expr_evaluator(None);
    let dst_only_prefilter = dst_filter_is_dst_vertex_only(dst_filter, dst.as_ref());
    let edge_key = emit_edge_binding.then(|| edge.to_string());
    let dst_key = dst.to_string();
    let csr_expand_fast_path = (edge_value_predicate.is_none() && edge_vector_predicate.is_none())
        .then(|| csr_offset_fast_path_for_expand(direction, label_id, sequence_order))
        .flatten();
    let prepared_vector_dst_only_predicate = prepare_vector_dst_only_expand_predicate(
        store,
        label_id,
        direction,
        emit_edge_binding,
        indexed_edge_equality,
        edge_value_predicate,
        edge_vector_predicate,
        edge_property_projection,
        parameters,
    )?;
    let edge_equality_filter = if csr_expand_fast_path.is_some() {
        let filter = edge_equality_stream_filter(store, indexed_edge_equality, parameters)?;
        if matches!(filter, EdgeEqualityStreamFilter::NoMatches) {
            return Ok(Vec::new());
        }
        Some(filter)
    } else {
        None
    };
    let mut out = Vec::with_capacity(rows.len());
    let mut candidates = Vec::new();
    let mut vector_batch_scratch = LabeledEdgeValueBatchScratch::default();
    let mut vector_matches = Vec::new();
    for row in rows {
        if matches!(direction, EdgeDirection::PointingLeft)
            && let Some(PlanBinding::RemoteVertex(logical)) = row.get(src.as_ref())
            && matches!(
                resolve_federated_traversal_vertex(store, *logical, Some(direction)).await,
                Err(PlanQueryError::UnsupportedOp(_))
            )
        {
            let label_id_raw = federated_expand_label_id_raw(label_id, direction);
            let hits = crate::facade::federation_expand::federated_expand_coordinator(
                store,
                FederatedExpandArgs {
                    logical_vertex_id: *logical,
                    direction: FederatedExpandDirection::Incoming,
                    label_id_raw,
                },
            )
            .await
            .map_err(|e| PlanQueryError::FederatedIndexCall {
                op: "federated_expand",
                detail: e.to_string(),
            })?;
            out.extend(expand_rows_from_federated_expand_hits(
                store,
                &row,
                &hits,
                dst.as_ref(),
                edge.as_ref(),
                emit_edge_binding,
                dst_property_projection,
                dst_filter,
                parameters,
                caller,
                gleaph_weight_decoders,
                &evaluator,
            )?);
            continue;
        }

        if matches!(direction, EdgeDirection::PointingRight)
            && let Some(PlanBinding::RemoteVertex(logical)) = row.get(src.as_ref())
            && matches!(
                resolve_federated_traversal_vertex(store, *logical, Some(direction)).await,
                Err(PlanQueryError::UnsupportedOp(_))
            )
        {
            let label_id_raw = federated_expand_label_id_raw(label_id, direction);
            let hits = crate::facade::federation_expand::federated_expand_coordinator(
                store,
                FederatedExpandArgs {
                    logical_vertex_id: *logical,
                    direction: FederatedExpandDirection::Outgoing,
                    label_id_raw,
                },
            )
            .await
            .map_err(|e| PlanQueryError::FederatedIndexCall {
                op: "federated_expand",
                detail: e.to_string(),
            })?;
            out.extend(expand_rows_from_federated_expand_hits(
                store,
                &row,
                &hits,
                dst.as_ref(),
                edge.as_ref(),
                emit_edge_binding,
                dst_property_projection,
                dst_filter,
                parameters,
                caller,
                gleaph_weight_decoders,
                &evaluator,
            )?);
            continue;
        }

        if matches!(direction, EdgeDirection::Undirected)
            && let Some(PlanBinding::RemoteVertex(logical)) = row.get(src.as_ref())
            && matches!(
                resolve_federated_traversal_vertex(store, *logical, Some(direction)).await,
                Err(PlanQueryError::UnsupportedOp(_))
            )
        {
            let label_id_raw = federated_expand_label_id_raw(label_id, direction);
            let hits = crate::facade::federation_expand::federated_expand_coordinator(
                store,
                FederatedExpandArgs {
                    logical_vertex_id: *logical,
                    direction: FederatedExpandDirection::Undirected,
                    label_id_raw,
                },
            )
            .await
            .map_err(|e| PlanQueryError::FederatedIndexCall {
                op: "federated_expand",
                detail: e.to_string(),
            })?;
            out.extend(expand_rows_from_federated_expand_hits(
                store,
                &row,
                &hits,
                dst.as_ref(),
                edge.as_ref(),
                emit_edge_binding,
                dst_property_projection,
                dst_filter,
                parameters,
                caller,
                gleaph_weight_decoders,
                &evaluator,
            )?);
            continue;
        }

        let Some(src_id) = (match row.get(src.as_ref()) {
            Some(PlanBinding::RemoteVertex(logical)) => {
                resolve_federated_traversal_vertex(store, *logical, Some(direction)).await?
            }
            _ => vertex_binding_for_traversal(store, &row, src, Some(direction)).await?,
        }) else {
            continue;
        };
        if let Some(fast_path) = csr_expand_fast_path {
            let mut offset_slot = 0;
            let mut visit = |edge: Edge| {
                let Some(edge_dst) = ExpandDst::from_edge(store, &edge)? else {
                    return Ok(false);
                };
                let edge_binding = edge_binding_for_expand(store, src_id, direction, edge)?;
                if !edge_matches_stream_filter(
                    store,
                    edge_equality_filter
                        .as_ref()
                        .expect("filter exists with fast path"),
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
                if !dst_only_prefilter && !row_matches_all(&evaluator, &expanded, dst_filter)? {
                    return Ok(false);
                }
                out.push(expanded);
                Ok(false)
            };
            let res =
                visit_csr_expand_fast_path(store, src_id, fast_path, &mut offset_slot, &mut visit);
            match res {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => return Err(e),
                Err(e) => return Err(e.into()),
            }
            continue;
        }
        if let Some((edge_label_id, predicate)) = prepared_vector_dst_only_predicate.as_ref() {
            expand_vector_dst_only_rows_into(
                store,
                &row,
                src_id,
                direction,
                *edge_label_id,
                sequence_order,
                dst,
                dst_key.as_str(),
                dst_filter,
                dst_only_prefilter,
                dst_property_projection,
                parameters,
                caller,
                gleaph_weight_decoders,
                &evaluator,
                predicate,
                &mut out,
                &mut vector_batch_scratch,
                &mut vector_matches,
            )?;
            continue;
        }
        candidates.clear();
        expand_candidates_into(
            store,
            src_id,
            direction,
            label_id,
            sequence_order,
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
            if !dst_only_prefilter && !row_matches_all(&evaluator, &expanded, dst_filter)? {
                continue;
            }
            out.push(expanded);
        }
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn prepare_vector_dst_only_expand_predicate(
    store: &GraphStore,
    label_id: Option<EdgeLabelId>,
    direction: EdgeDirection,
    emit_edge_binding: bool,
    indexed_edge_equality: Option<&(Str, ScanValue)>,
    edge_value_predicate: Option<&EdgeValuePredicate>,
    edge_vector_predicate: Option<&EdgeVectorPredicate>,
    edge_property_projection: Option<&[Str]>,
    parameters: &BTreeMap<String, Value>,
) -> Result<Option<(EdgeLabelId, PreparedEdgeVectorThreshold)>, PlanQueryError> {
    if emit_edge_binding
        || indexed_edge_equality.is_some()
        || edge_value_predicate.is_some()
        || edge_vector_predicate.is_none()
        || edge_property_projection.is_some_and(|props| !props.is_empty())
        || !matches!(
            direction,
            EdgeDirection::PointingRight | EdgeDirection::PointingLeft
        )
    {
        return Ok(None);
    }
    let Some(edge_label_id) = label_id else {
        return Ok(None);
    };
    let Some(predicate) = PreparedEdgeVectorThreshold::prepare(
        store,
        edge_label_id,
        edge_vector_predicate.expect("checked above"),
        parameters,
    )?
    else {
        return Ok(None);
    };
    Ok(Some((edge_label_id, predicate)))
}

pub(crate) fn expand_dst_matches_prebound_vertex(
    row: &PlanRow,
    dst: &Str,
    edge_dst: ExpandDst,
) -> bool {
    match (row.get(dst.as_ref()), edge_dst) {
        (Some(PlanBinding::Vertex(id)), ExpandDst::Local(dst_id)) => *id == dst_id,
        (Some(PlanBinding::RemoteVertex(logical)), ExpandDst::Remote(dst_logical)) => {
            *logical == dst_logical
        }
        (None, _) => true,
        _ => false,
    }
}
