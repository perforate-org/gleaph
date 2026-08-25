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
use crate::index::lookup::PropertyIndexLookup;
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
pub(crate) fn resolve_scan_bound_value(
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
        // IN lists are expanded into per-element probes before resolution.
        ScanValue::InList(_) => Err(PlanQueryError::UnsupportedOp(
            "IndexScan(unexpanded IN-list scan value)",
        )),
        // Prefix bounds are lowered into a Between interval by the dedicated
        // text-prefix path before the pattern itself would be resolved.
        ScanValue::TextPrefix(_) => Err(PlanQueryError::UnsupportedOp(
            "IndexScan(unlowered text-prefix scan value)",
        )),
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

/// Equality probe payloads for an edge-index bound: one independently resolved
/// payload per IN-list element, or the single payload of a literal/parameter
/// bound. `None` payloads are Null bounds that can never satisfy equality; a
/// missing parameter fails closed instead of narrowing the union silently.
pub(crate) fn resolve_edge_equality_probe_payloads(
    sv: &ScanValue,
    parameters: &BTreeMap<String, Value>,
) -> Result<Vec<Option<Vec<u8>>>, PlanQueryError> {
    match sv {
        ScanValue::InList(elements) => elements
            .iter()
            .map(|element| resolve_scan_payload_bytes(element, parameters))
            .collect(),
        single => Ok(vec![resolve_scan_payload_bytes(single, parameters)?]),
    }
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
    // A text-prefix anchor lowers into one Between interval over the encoded
    // TEXT domain; it shares the one-sided range path's size validation, empty
    // handling, and unsupported-domain node-scan fallback.
    if let ScanValue::TextPrefix(pattern) = scan_value {
        if cmp != CmpOp::Eq {
            return Err(PlanQueryError::UnsupportedOp(
                "IndexScan(text-prefix scan with non-equality comparison)",
            ));
        }
        let pattern_value = resolve_scan_bound_value(pattern, ctx.parameters)?;
        return execute_text_prefix_index_scan(ctx, rows, ix, pid, variable, pattern_value).await;
    }
    // An IN-list anchor unions one equality probe per element; every other scan
    // value probes exactly one bound. Elements resolve independently so a
    // missing parameter fails closed instead of narrowing the union silently.
    let probe_values: Vec<Value> = match scan_value {
        ScanValue::InList(elements) => {
            if cmp != CmpOp::Eq {
                return Err(PlanQueryError::UnsupportedOp(
                    "IndexScan(IN list with non-equality comparison)",
                ));
            }
            let mut resolved = Vec::with_capacity(elements.len());
            for element in elements {
                resolved.push(resolve_scan_bound_value(element, ctx.parameters)?);
            }
            resolved
        }
        single => vec![resolve_scan_bound_value(single, ctx.parameters)?],
    };
    let mut probe_keys = Vec::with_capacity(probe_values.len());
    for value in &probe_values {
        probe_keys.push(validated_index_payload_bytes(value)?);
    }
    let physical_index_id = crate::index::catalog_context::unique_active_vertex_physical_index_id(
        gleaph_graph_kernel::entry::PropertyId::from_raw(pid),
    )
    .ok_or(PlanQueryError::UnsupportedOp(
        "IndexScan(no unique physical index namespace)",
    ))?;

    if cmp == CmpOp::Eq {
        let hits = if probe_keys.len() > 1 {
            // Union of point probes, deduplicated on (shard, vertex): a vertex
            // whose property equals two list elements must bind exactly one row.
            // Blocks are concatenated in encoded payload byte order (ADR 0081
            // §4): encoded byte order equals domain order by construction, so an
            // ordered-delivery consumer sees globally ascending keys. Null bounds
            // contribute no block because no posting can equal them.
            let mut ordered_blocks: Vec<&Vec<u8>> = probe_keys.iter().flatten().collect();
            ordered_blocks.sort_unstable();
            ordered_blocks.dedup();
            let mut seen = std::collections::BTreeSet::<(u32, u32)>::new();
            let mut merged = Vec::new();
            for bytes in ordered_blocks {
                for hit in ix
                    .lookup_equal(physical_index_id, pid, bytes.clone())
                    .await?
                {
                    if seen.insert((hit.shard_id.raw(), hit.vertex_id)) {
                        merged.push(hit);
                    }
                }
            }
            merged
        } else {
            let Some(bytes) = probe_keys.into_iter().flatten().next() else {
                // A Null bound (or an empty IN list) can never satisfy equality.
                return Ok(Vec::new());
            };
            ix.lookup_equal(physical_index_id, pid, bytes).await?
        };
        return Ok(ctx
            .federation
            .bind_index_hits(ctx.store, &rows, variable, &hits));
    }

    // One-sided range bound. Only reachable with a single scan value.
    let value = probe_values
        .into_iter()
        .next()
        .ok_or_else(|| PlanQueryError::UnsupportedOp("IndexScan(range without bound)"))?;
    match gleaph_gql::value_index_key::range_bounds(&value, cmp) {
        Ok((low, high)) if low < high => {
            for bound in [&low, &high] {
                if bound.len() > MAX_INDEX_VALUE_KEY_BYTES {
                    return Err(PlanQueryError::InvalidExpressionValue {
                        expression: "index range bound exceeds maximum encoded size".to_owned(),
                    });
                }
            }
            let hits = ix
                .lookup_range(
                    physical_index_id,
                    pid,
                    &PostingRangeRequest::Between { low, high },
                )
                .await?;
            Ok(ctx
                .federation
                .bind_index_hits(ctx.store, &rows, variable, &hits))
        }
        // The domain-clamped interval is empty: no posting can satisfy the comparison.
        Ok(_) => Ok(Vec::new()),
        // Unsupported comparison domain (Bool/List/Record/Path/Extension/Duration): do not
        // push down; the plan's residual filter enforces the predicate over the node scan.
        Err(ValueIndexKeyError::UnsupportedRangeDomain) => {
            let variable_str = Str::from(variable);
            execute_node_scan(ctx.store, rows, &variable_str, None, &ctx.execution)
        }
        Err(err) => Err(PlanQueryError::InvalidExpressionValue {
            expression: format!("index scan range bound: {err}"),
        }),
    }
}

pub(crate) async fn execute_text_prefix_index_scan(
    ctx: &ExecuteCtx<'_>,
    rows: Vec<PlanRow>,
    ix: &dyn PropertyIndexLookup,
    pid: u32,
    variable: &str,
    pattern: Value,
) -> Result<Vec<PlanRow>, PlanQueryError> {
    match gleaph_gql::value_index_key::text_prefix_range_bounds(&pattern) {
        Ok((low, high)) => {
            for bound in [&low, &high] {
                if bound.len() > MAX_INDEX_VALUE_KEY_BYTES {
                    return Err(PlanQueryError::InvalidExpressionValue {
                        expression: "index range bound exceeds maximum encoded size".to_owned(),
                    });
                }
            }
            let physical_index_id =
                crate::index::catalog_context::unique_active_vertex_physical_index_id(
                    gleaph_graph_kernel::entry::PropertyId::from_raw(pid),
                )
                .ok_or(PlanQueryError::UnsupportedOp(
                    "IndexScan(no unique physical index namespace)",
                ))?;
            let raw_hits = ix
                .lookup_range(
                    physical_index_id,
                    pid,
                    &PostingRangeRequest::Between { low, high },
                )
                .await?;
            // Deduplicate on (shard, vertex), mirroring the IN-list union: one
            // vertex must bind exactly one row regardless of posting layout.
            let mut seen = std::collections::BTreeSet::<(u32, u32)>::new();
            let mut hits = Vec::with_capacity(raw_hits.len());
            for hit in raw_hits {
                if seen.insert((hit.shard_id.raw(), hit.vertex_id)) {
                    hits.push(hit);
                }
            }
            Ok(ctx
                .federation
                .bind_index_hits(ctx.store, &rows, variable, &hits))
        }
        // A Null pattern can never satisfy STARTS WITH under three-valued
        // logic, so the scan binds no rows.
        Err(ValueIndexKeyError::UnsupportedValue) if pattern == Value::Null => Ok(Vec::new()),
        // A non-TEXT resolved pattern cannot form a TEXT prefix interval: do not
        // push down; the plan's residual filter enforces the predicate over the
        // node scan.
        Err(ValueIndexKeyError::UnsupportedRangeDomain) => {
            let variable_str = Str::from(variable);
            execute_node_scan(ctx.store, rows, &variable_str, None, &ctx.execution)
        }
        Err(err) => Err(PlanQueryError::InvalidExpressionValue {
            expression: format!("index scan prefix bound: {err}"),
        }),
    }
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
