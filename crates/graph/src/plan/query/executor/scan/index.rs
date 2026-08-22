use std::collections::BTreeMap;

use gleaph_gql::ValueIndexKeyError;
use gleaph_gql::ast::CmpOp;
use gleaph_gql::{Value, value_to_index_key_bytes};
use gleaph_gql_planner::plan::{ConditionalScanCandidate, IndexScanSpec, ScanValue, Str};
use gleaph_graph_kernel::index::{
    IndexEqualSpec, IndexIntersectionRequest, MAX_INDEX_VALUE_KEY_BYTES, PostingRangeRequest,
    validate_index_value_key_bytes,
};

use crate::facade::GraphStore;
use crate::federation::FederationPort;
use crate::gql_execution_context::GqlExecutionContext;
use crate::plan::query::error::PlanQueryError;
use crate::plan::query::executor::context::ExecuteCtx;
use crate::plan::query::row::PlanRow;

fn property_id_for_scan(
    execution: &GqlExecutionContext,
    property_name: &str,
) -> Result<u32, PlanQueryError> {
    execution
        .resolved_property_id(property_name)
        .map(|p| p.raw())
        .ok_or_else(|| PlanQueryError::MissingResolvedProperty {
            name: property_name.to_owned(),
        })
}

/// Resolve the scan literal/parameter to its [`Value`].
fn resolve_scan_bound_value(
    sv: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<Value, PlanQueryError> {
    match sv {
        ScanValue::Literal(val) => Ok(val.clone()),
        ScanValue::Parameter(name) => parameters
            .get(crate::plan::query::param_map_key(name.as_ref()))
            .cloned()
            .ok_or_else(|| PlanQueryError::MissingParameter {
                name: name.to_string(),
            }),
    }
}

fn validated_index_payload_bytes(value: &Value) -> Result<Option<Vec<u8>>, PlanQueryError> {
    value_to_index_key_bytes(value)
        .map_err(|_| PlanQueryError::InvalidExpressionValue {
            expression: "index scan value encoding".to_owned(),
        })
        .and_then(|bytes| {
            let Some(bytes) = bytes else {
                return Ok(None);
            };
            validate_index_value_key_bytes(&bytes).map_err(|_| {
                PlanQueryError::InvalidExpressionValue {
                    expression: "index value key exceeds maximum encoded size".to_owned(),
                }
            })?;
            Ok(Some(bytes))
        })
}

pub(crate) fn resolve_scan_payload_bytes(
    sv: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<Option<Vec<u8>>, PlanQueryError> {
    let v = resolve_scan_bound_value(sv, parameters)?;
    validated_index_payload_bytes(&v)
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn execute_index_scan(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    variable: &str,
    property_name: &str,
    scan_value: &ScanValue,
    cmp: CmpOp,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let Some(ix) = ctx.index else {
        return Err(PlanQueryError::UnsupportedOp("IndexScan(no index client)"));
    };
    let pid = property_id_for_scan(&ctx.execution, property_name)?;
    let value = resolve_scan_bound_value(scan_value, ctx.parameters)?;
    let Some(bytes) = validated_index_payload_bytes(&value)? else {
        return Ok(Vec::new());
    };
    let physical_index_id = crate::index::catalog_context::unique_active_vertex_physical_index_id(
        gleaph_graph_kernel::entry::PropertyId::from_raw(pid),
    )
    .ok_or(PlanQueryError::UnsupportedOp(
        "IndexScan(no unique physical index namespace)",
    ))?;
    let hits = if cmp == CmpOp::Eq {
        ix.lookup_equal(physical_index_id, pid, bytes).await?
    } else {
        match gleaph_gql::value_index_key::range_bounds(&value, cmp) {
            Ok((low, high)) if low < high => {
                for bound in [&low, &high] {
                    if bound.len() > MAX_INDEX_VALUE_KEY_BYTES {
                        return Err(PlanQueryError::InvalidExpressionValue {
                            expression: "index range bound exceeds maximum encoded size".to_owned(),
                        });
                    }
                }
                ix.lookup_range(
                    physical_index_id,
                    pid,
                    &PostingRangeRequest::Between { low, high },
                )
                .await?
            }
            // The domain-clamped interval is empty: no posting can satisfy the comparison.
            Ok(_) => Vec::new(),
            // Unsupported comparison domain (Bool/List/Record/Path/Extension/Duration): do not
            // push down; the plan's residual filter enforces the predicate over the node scan.
            Err(ValueIndexKeyError::UnsupportedRangeDomain) => {
                let variable_str = Str::from(variable);
                return execute_node_scan(ctx.store, rows, &variable_str, None, &ctx.execution);
            }
            Err(err) => {
                return Err(PlanQueryError::InvalidExpressionValue {
                    expression: format!("index scan range bound: {err}"),
                });
            }
        }
    };
    Ok(ctx
        .federation
        .bind_index_hits(ctx.store, &rows, variable, &hits))
}

pub(crate) async fn execute_conditional_index_scan(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    candidates: &[ConditionalScanCandidate],
    fallback_label: Option<&str>,
    fallback_variable: &Str,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    for c in candidates {
        let pv = ctx
            .parameters
            .get(c.param_name.as_ref())
            .cloned()
            .unwrap_or(Value::Null);
        if pv != Value::Null {
            // One-sided ranges push down only their domain-clamped interval; anything else
            // (unsupported comparison domain, unencodable or oversized value, empty clamped
            // interval) takes the non-index filter path via the labeled node-scan fallback.
            let range_request = if c.cmp == CmpOp::Eq {
                None
            } else {
                match gleaph_gql::value_index_key::range_bounds(&pv, c.cmp) {
                    Ok((low, high))
                        if low < high
                            && low.len() <= MAX_INDEX_VALUE_KEY_BYTES
                            && high.len() <= MAX_INDEX_VALUE_KEY_BYTES =>
                    {
                        Some(PostingRangeRequest::Between { low, high })
                    }
                    _ => break,
                }
            };
            let Some(bytes) = value_to_index_key_bytes(&pv).ok().flatten() else {
                break;
            };
            if validate_index_value_key_bytes(&bytes).is_err() {
                break;
            }
            let Some(ix) = ctx.index else {
                return Err(PlanQueryError::UnsupportedOp(
                    "ConditionalIndexScan(no index client)",
                ));
            };
            let pid = property_id_for_scan(&ctx.execution, c.property.as_ref())?;
            let physical_index_id =
                crate::index::catalog_context::unique_active_vertex_physical_index_id(
                    gleaph_graph_kernel::entry::PropertyId::from_raw(pid),
                )
                .ok_or(PlanQueryError::UnsupportedOp(
                    "ConditionalIndexScan(no unique physical index namespace)",
                ))?;
            let hits = match range_request {
                Some(req) => ix.lookup_range(physical_index_id, pid, &req).await?,
                None => ix.lookup_equal(physical_index_id, pid, bytes).await?,
            };
            return Ok(ctx.federation.bind_index_hits(
                ctx.store,
                &rows,
                c.variable.as_ref(),
                &hits,
            ));
        }
    }
    execute_node_scan(
        ctx.store,
        rows,
        fallback_variable,
        fallback_label,
        &ctx.execution,
    )
}

pub(crate) async fn execute_index_intersection(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    variable: &str,
    scans: &[IndexScanSpec],
) -> Result<Vec<PlanRow>, PlanQueryError> {
    let Some(ix) = ctx.index else {
        return Err(PlanQueryError::UnsupportedOp(
            "IndexIntersection(no index client)",
        ));
    };
    let mut specs = Vec::with_capacity(scans.len());
    for spec in scans {
        if spec.cmp != CmpOp::Eq {
            return Err(PlanQueryError::UnsupportedOp("IndexIntersection.cmp"));
        }
        let pid = property_id_for_scan(&ctx.execution, spec.property.as_ref())?;
        let Some(bytes) = resolve_scan_payload_bytes(&spec.value, ctx.parameters)? else {
            return Ok(Vec::new());
        };
        let physical_index_id =
            crate::index::catalog_context::unique_active_vertex_physical_index_id(
                gleaph_graph_kernel::entry::PropertyId::from_raw(pid),
            )
            .ok_or(PlanQueryError::UnsupportedOp(
                "IndexIntersection(no unique physical index namespace)",
            ))?;
        specs.push(IndexEqualSpec::vertex(physical_index_id, pid, bytes));
    }
    if specs.len() < 2 {
        return Ok(Vec::new());
    }
    let result = ix
        .lookup_intersection(&IndexIntersectionRequest { specs })
        .await?;
    let hits = result.vertices();
    Ok(ctx
        .federation
        .bind_index_hits(ctx.store, &rows, variable, &hits))
}

pub(crate) fn execute_node_scan(
    store: &GraphStore,
    rows: Vec<PlanRow>,
    variable: &Str,
    label: Option<&str>,
    execution: &GqlExecutionContext,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    use ic_stable_lara::VertexId;
    use ic_stable_lara::traits::CsrVertexTombstone;

    use crate::plan::query::executor::PlanBinding;

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
