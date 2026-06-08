use std::collections::BTreeMap;

use gleaph_gql::ast::CmpOp;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_planner::plan::{ConditionalScanCandidate, IndexScanSpec, ScanValue, Str};
use gleaph_graph_kernel::index::{
    IndexEqualSpec, IndexIntersectionRequest, PostingHit, PostingRangeRequest,
};
use ic_stable_lara::VertexId;
use ic_stable_lara::traits::CsrVertexTombstone;

use crate::facade::GraphStore;
use crate::gql_execution_context::GqlExecutionContext;
use crate::index::lookup::PropertyIndexLookup;
use crate::index::placement;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::PlanBinding;
use crate::plan::query::row::PlanRow;

fn property_id_for_scan(store: &GraphStore, property_name: &str) -> Result<u32, PlanQueryError> {
    store
        .property_id(property_name)
        .map(|p| p.raw())
        .ok_or(PlanQueryError::UnsupportedOp("IndexScan.unknown_property"))
}

pub(crate) fn resolve_scan_payload_bytes(
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
    let Some(bytes) = resolve_scan_payload_bytes(scan_value, parameters)? else {
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
    fallback_label: Option<&str>,
    fallback_variable: &Str,
    execution: &GqlExecutionContext,
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
    execute_node_scan(store, rows, fallback_variable, fallback_label, execution)
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
    let mut specs = Vec::with_capacity(scans.len());
    for spec in scans {
        if spec.cmp != CmpOp::Eq {
            return Err(PlanQueryError::UnsupportedOp("IndexIntersection.cmp"));
        }
        let pid = property_id_for_scan(store, spec.property.as_ref())?;
        let Some(bytes) = resolve_scan_payload_bytes(&spec.value, parameters)? else {
            return Ok(Vec::new());
        };
        specs.push(IndexEqualSpec {
            property_id: pid,
            value: bytes,
        });
    }
    if specs.len() < 2 {
        return Ok(Vec::new());
    }
    let hits = ix
        .lookup_intersection(&IndexIntersectionRequest { specs })
        .await?;
    let mut out = Vec::new();
    for row in rows {
        for hit in &hits {
            if hit.shard_id != local_shard {
                continue;
            }
            let vertex_id = VertexId::from(hit.vertex_id);
            if store.vertex(vertex_id).is_none() {
                continue;
            }
            out.push(row.fork([(variable, PlanBinding::Vertex(vertex_id))]));
        }
    }
    Ok(out)
}

pub(crate) fn execute_node_scan(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    variable: &Str,
    label: Option<&str>,
    execution: &GqlExecutionContext,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let label_id = match label {
        Some(label) => execution
            .resolved_vertex_label_id(label)
            .map(Some)
            .ok_or_else(|| PlanQueryError::MissingResolvedLabel {
                namespace: "node",
                name: label.to_owned(),
            })?,
        None => None,
    };

    let mut out = Vec::new();
    for row in rows {
        for raw in 0..u32::from(store.vertex_count()) {
            #[cfg(test)]
            super::NODE_SCAN_VISITS.with(|visits| visits.set(visits.get() + 1));
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
