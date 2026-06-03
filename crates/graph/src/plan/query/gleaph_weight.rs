//! Preparation for `GLEAPH.WEIGHT` traversal intrinsics.
//!
//! Decoded inline weights are returned as `FLOAT32` at execution time; cost expressions may widen
//! the value via casts or arithmetic.

use std::collections::{BTreeMap, BTreeSet};

use gleaph_gql::ast::{Expr, ExprKind, LetBinding, ObjectName};
use gleaph_gql::types::LabelExpr;
use gleaph_gql_planner::plan::{
    PlanOp, ProjectColumn, ScanValue, ShortestPathCost, Str, VarLenSpec,
};
use gleaph_graph_kernel::entry::{
    DecodedEdgePayload, EdgePayloadProfileError, PreparedEdgePayloadDecoder, PreparedWeightDecoder,
    WeightProfilePrepareError, decode_edge_payload,
};

use crate::facade::{EdgeHandle, GraphStore, catalog_edge_label_from_wire};
use crate::gql_execution_context::GqlExecutionContext;

use super::error::PlanQueryError;

/// Decodes a traversal edge's stored bytes into a non-negative `f32` weight.
pub(crate) fn decode_traversal_edge_weight(
    store: &GraphStore,
    handle: EdgeHandle,
    payload_len: usize,
    payload_bytes: &[u8],
) -> Result<f32, PlanQueryError> {
    if let Some(catalog) = catalog_edge_label_from_wire(handle.label_id)
        && let Some(profile) = store.edge_label_payload_profile(catalog)
    {
        let decoder = profile.prepare().map_err(
            |e: gleaph_graph_kernel::entry::EdgePayloadProfileError| PlanQueryError::GleaphWeight {
                message: format!("edge payload profile decode prepare failed: {e}"),
            },
        )?;
        let expected_width = profile.required_byte_width();
        if payload_len != usize::from(expected_width)
            || payload_bytes.len() != usize::from(expected_width)
        {
            return Err(PlanQueryError::GleaphWeight {
                message: format!(
                    "edge payload width mismatch: profile expects {expected_width} bytes, edge stores {payload_len}"
                ),
            });
        }
        let decoded = decode_edge_payload(&decoder, payload_bytes).map_err(|e| {
            PlanQueryError::GleaphWeight {
                message: format!("edge payload decode failed: {e}"),
            }
        })?;
        return decoded_edge_payload_to_weight(decoded);
    }
    Err(PlanQueryError::GleaphWeight {
        message: format!(
            "edge label row has no payload profile (stored width {} bytes)",
            payload_len
        ),
    })
}

fn decoded_edge_payload_to_weight(decoded: DecodedEdgePayload) -> Result<f32, PlanQueryError> {
    match decoded {
        DecodedEdgePayload::Weight(w) => Ok(w),
        other => Err(PlanQueryError::GleaphWeight {
            message: format!("edge payload encoding {other:?} is not a traversal weight"),
        }),
    }
}

/// Per-edge-variable prepared decoders for `GLEAPH.WEIGHT`.
pub(crate) fn prepare_gleaph_weight_decoders(
    store: &GraphStore,
    execution: &GqlExecutionContext,
    ops: &[PlanOp],
) -> Result<Option<BTreeMap<String, PreparedWeightDecoder>>, PlanQueryError> {
    let mut edge_vars = BTreeSet::new();
    for_each_expr_in_ops(ops, &mut |expr| {
        if let Some(ev) = gleaph_weight_edge_var(expr) {
            edge_vars.insert(ev);
        }
    });

    if edge_vars.is_empty() {
        return Ok(None);
    }

    let mut out = BTreeMap::new();
    for edge_var in edge_vars {
        let decoder = decoder_for_gleaph_weight_edge(store, execution, ops, &edge_var)?;
        out.insert(edge_var, decoder);
    }
    Ok(Some(out))
}

pub(crate) fn is_gleaph_weight_call(name: &ObjectName, distinct: bool) -> bool {
    if distinct {
        return false;
    }
    name.parts.len() == 2
        && name.parts[0].eq_ignore_ascii_case("gleaph")
        && name.parts[1].eq_ignore_ascii_case("weight")
}

pub(crate) fn gleaph_weight_single_arg(args: &[Expr]) -> Option<&Expr> {
    if args.len() == 1 {
        Some(&args[0])
    } else {
        None
    }
}

pub(crate) fn gleaph_weight_arg_edge_var(expr: &Expr) -> Option<String> {
    match &expr.kind {
        ExprKind::Paren(inner) => gleaph_weight_arg_edge_var(inner),
        ExprKind::Variable(v) => Some(v.clone()),
        _ => None,
    }
}

fn gleaph_weight_edge_var(expr: &Expr) -> Option<String> {
    let ExprKind::FunctionCall {
        name,
        args,
        distinct,
    } = &expr.kind
    else {
        return None;
    };
    if !is_gleaph_weight_call(name, *distinct) {
        return None;
    }
    let arg = gleaph_weight_single_arg(args)?;
    gleaph_weight_arg_edge_var(arg)
}

fn decoder_for_gleaph_weight_edge(
    store: &GraphStore,
    execution: &GqlExecutionContext,
    ops: &[PlanOp],
    edge_var: &str,
) -> Result<PreparedWeightDecoder, PlanQueryError> {
    let producer = first_edge_producer_for_var(ops, edge_var).ok_or_else(|| PlanQueryError::GleaphWeight {
        message: format!(
            "GLEAPH.WEIGHT({edge_var}): no Expand/ExpandFilter/ShortestPath binds variable '{edge_var}'"
        ),
    })?;

    match producer {
        EdgeProducer::Expand {
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
        }
        | EdgeProducer::ExpandFilter {
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
        } => {
            if label_expr.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH.WEIGHT({edge_var}): edge pattern must use a single fixed label, not a label expression"
                    ),
                });
            }
            if var_len.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH.WEIGHT({edge_var}): variable-length edge patterns are not supported"
                    ),
                });
            }
            if indexed_edge_equality.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH.WEIGHT({edge_var}): indexed edge equality expansion is not supported"
                    ),
                });
            }
            if hop_aux_binding.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH.WEIGHT({edge_var}): hop auxiliary bindings are not supported"
                    ),
                });
            }
            let label_name = label.ok_or_else(|| PlanQueryError::GleaphWeight {
                message: format!(
                    "GLEAPH.WEIGHT({edge_var}): edge pattern must have exactly one fixed edge label"
                ),
            })?;
            finish_decoder_from_label_name(store, execution, edge_var, label_name.as_ref())
        }
        EdgeProducer::ShortestPath {
            label,
            label_expr,
            var_len: _,
        } => {
            if label_expr.is_some() {
                return Err(PlanQueryError::GleaphWeight {
                    message: format!(
                        "GLEAPH.WEIGHT({edge_var}): shortest-path edge pattern must use a single fixed label"
                    ),
                });
            }
            let label_name = label.ok_or_else(|| PlanQueryError::GleaphWeight {
                message: format!(
                    "GLEAPH.WEIGHT({edge_var}): shortest-path must have exactly one fixed edge label"
                ),
            })?;
            finish_decoder_from_label_name(store, execution, edge_var, label_name.as_ref())
        }
    }
}

fn finish_decoder_from_label_name(
    store: &GraphStore,
    execution: &GqlExecutionContext,
    edge_var: &str,
    label_name: &str,
) -> Result<PreparedWeightDecoder, PlanQueryError> {
    let label_id = execution
        .resolved_edge_label_id(label_name)
        .ok_or_else(|| PlanQueryError::MissingResolvedLabel {
            namespace: "edge",
            name: label_name.to_owned(),
        })?;
    if !label_id.is_catalog_allocatable() {
        return Err(PlanQueryError::GleaphWeight {
            message: format!(
                "GLEAPH.WEIGHT({edge_var}): label '{label_name}' is not a catalog edge label id"
            ),
        });
    }
    if let Some(profile) = store.edge_label_payload_profile(label_id) {
        let decoder = profile.prepare().map_err(|e: EdgePayloadProfileError| {
            PlanQueryError::GleaphWeight {
                message: format!("GLEAPH.WEIGHT({edge_var}): invalid payload profile: {e}"),
            }
        })?;
        ensure_edge_payload_decoder_is_weight(edge_var, label_name, &decoder)?;
        return Ok(PreparedWeightDecoder::RawU16);
    }

    let profile = store.edge_label_weight_profile(label_id).ok_or_else(|| {
        PlanQueryError::GleaphWeight {
            message: format!(
                "GLEAPH.WEIGHT({edge_var}): edge label '{label_name}' has no weight profile configured"
            ),
        }
    })?;
    profile.prepare().map_err(
        |e: WeightProfilePrepareError| PlanQueryError::GleaphWeight {
            message: format!("GLEAPH.WEIGHT({edge_var}): invalid weight profile: {e}"),
        },
    )
}

fn ensure_edge_payload_decoder_is_weight(
    edge_var: &str,
    label_name: &str,
    decoder: &PreparedEdgePayloadDecoder,
) -> Result<(), PlanQueryError> {
    if matches!(
        decoder,
        PreparedEdgePayloadDecoder::WeightRawU16
            | PreparedEdgePayloadDecoder::WeightLinear { .. }
            | PreparedEdgePayloadDecoder::WeightLog { .. }
            | PreparedEdgePayloadDecoder::WeightBinary16
    ) {
        return Ok(());
    }
    Err(PlanQueryError::GleaphWeight {
        message: format!(
            "GLEAPH.WEIGHT({edge_var}): edge label '{label_name}' payload profile is not a weight encoding"
        ),
    })
}

enum EdgeProducer<'a> {
    Expand {
        label: Option<&'a str>,
        label_expr: &'a Option<LabelExpr>,
        var_len: &'a Option<VarLenSpec>,
        indexed_edge_equality: &'a Option<(Str, ScanValue)>,
        hop_aux_binding: &'a Option<Str>,
    },
    ExpandFilter {
        label: Option<&'a str>,
        label_expr: &'a Option<LabelExpr>,
        var_len: &'a Option<VarLenSpec>,
        indexed_edge_equality: &'a Option<(Str, ScanValue)>,
        hop_aux_binding: &'a Option<Str>,
    },
    ShortestPath {
        label: Option<&'a str>,
        label_expr: &'a Option<LabelExpr>,
        var_len: &'a Option<VarLenSpec>,
    },
}

fn first_edge_producer_for_var<'a>(ops: &'a [PlanOp], edge_var: &str) -> Option<EdgeProducer<'a>> {
    for op in ops {
        if let Some(p) = edge_producer_from_op(op, edge_var) {
            return Some(p);
        }
    }
    None
}

fn edge_producer_from_op<'a>(op: &'a PlanOp, edge_var: &str) -> Option<EdgeProducer<'a>> {
    match op {
        PlanOp::Expand {
            edge,
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
            ..
        } if edge.as_ref() == edge_var => Some(EdgeProducer::Expand {
            label: label.as_deref(),
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
        }),
        PlanOp::ExpandFilter {
            edge,
            label,
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
            ..
        } if edge.as_ref() == edge_var => Some(EdgeProducer::ExpandFilter {
            label: label.as_deref(),
            label_expr,
            var_len,
            indexed_edge_equality,
            hop_aux_binding,
        }),
        PlanOp::ShortestPath {
            edge,
            label,
            label_expr,
            var_len,
            ..
        } if edge.as_ref() == edge_var => Some(EdgeProducer::ShortestPath {
            label: label.as_deref(),
            label_expr,
            var_len,
        }),
        PlanOp::HashJoin { left, right, .. } => first_edge_producer_for_var(left, edge_var)
            .or_else(|| first_edge_producer_for_var(right, edge_var)),
        PlanOp::CartesianProduct { left, right } => first_edge_producer_for_var(left, edge_var)
            .or_else(|| first_edge_producer_for_var(right, edge_var)),
        PlanOp::OptionalMatch { sub_plan } => first_edge_producer_for_var(sub_plan, edge_var),
        PlanOp::UseGraph {
            sub_plan: Some(sub),
            ..
        } => first_edge_producer_for_var(sub, edge_var),
        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            first_edge_producer_for_var(&sub_plan.ops, edge_var)
        }
        PlanOp::SetOperation { right, .. } => first_edge_producer_for_var(&right.ops, edge_var),
        _ => None,
    }
}

fn for_each_expr_in_ops(ops: &[PlanOp], f: &mut impl FnMut(&Expr)) {
    for op in ops {
        for_each_expr_in_op(op, f);
    }
}

fn for_each_expr_in_op(op: &PlanOp, f: &mut impl FnMut(&Expr)) {
    match op {
        PlanOp::PropertyFilter { predicates, .. } => {
            for p in predicates {
                visit_expr(p, f);
            }
        }
        PlanOp::Filter { condition } => visit_expr(condition, f),
        PlanOp::ExpandFilter { dst_filter, .. } => {
            for p in dst_filter {
                visit_expr(p, f);
            }
        }
        PlanOp::Let { bindings } => {
            for LetBinding { value, .. } in bindings {
                visit_expr(value, f);
            }
        }
        PlanOp::For { list, .. } => visit_expr(list, f),
        PlanOp::Project { columns, .. } => {
            for ProjectColumn { expr, .. } in columns {
                visit_expr(expr, f);
            }
        }
        PlanOp::Sort { order_by } => {
            for item in &order_by.items {
                visit_expr(&item.expr, f);
            }
        }
        PlanOp::Limit { count, offset } => {
            if let Some(e) = count {
                visit_expr(e, f);
            }
            if let Some(e) = offset {
                visit_expr(e, f);
            }
        }
        PlanOp::TopK {
            order_by,
            k,
            offset,
        } => {
            for item in &order_by.items {
                visit_expr(&item.expr, f);
            }
            visit_expr(k, f);
            if let Some(e) = offset {
                visit_expr(e, f);
            }
        }
        PlanOp::Materialize { columns, .. } => {
            for ProjectColumn { expr, .. } in columns {
                visit_expr(expr, f);
            }
        }
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            for e in group_by {
                visit_expr(e, f);
            }
            for spec in aggregates {
                if let Some(e) = &spec.expr {
                    visit_expr(e, f);
                }
                if let Some(e2) = &spec.expr2 {
                    visit_expr(e2, f);
                }
                if let Some(fe) = &spec.filter {
                    visit_expr(fe, f);
                }
                if let Some(ob) = &spec.order_by {
                    for item in &ob.items {
                        visit_expr(&item.expr, f);
                    }
                }
            }
        }
        PlanOp::CallProcedure { args, .. } => {
            for a in args {
                visit_expr(a, f);
            }
        }
        PlanOp::ShortestPath {
            cost: ShortestPathCost::EdgeCostExpr { expr, .. },
            ..
        } => {
            visit_expr(expr, f);
        }
        PlanOp::HashJoin { left, right, .. } => {
            for_each_expr_in_ops(left, f);
            for_each_expr_in_ops(right, f);
        }
        PlanOp::CartesianProduct { left, right } => {
            for_each_expr_in_ops(left, f);
            for_each_expr_in_ops(right, f);
        }
        PlanOp::OptionalMatch { sub_plan } => for_each_expr_in_ops(sub_plan, f),
        PlanOp::UseGraph {
            sub_plan: Some(sub),
            ..
        } => for_each_expr_in_ops(sub, f),
        PlanOp::InlineProcedureCall { sub_plan, .. } => for_each_expr_in_ops(&sub_plan.ops, f),
        PlanOp::SetOperation { right, .. } => for_each_expr_in_ops(&right.ops, f),
        _ => {}
    }
}

fn visit_expr(expr: &Expr, f: &mut impl FnMut(&Expr)) {
    f(expr);
    gleaph_gql_planner::for_each_immediate_child_expr(expr, |child| visit_expr(child, f));
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_gql::ast::{ExprKind, ObjectName};

    fn paren(expr: Expr) -> Expr {
        Expr::new(ExprKind::Paren(Box::new(expr)))
    }

    #[test]
    fn gleaph_weight_arg_edge_var_unwraps_nested_parens() {
        let expr = paren(paren(paren(Expr::var("e"))));
        assert_eq!(gleaph_weight_arg_edge_var(&expr), Some("e".into()));
    }

    #[test]
    fn gleaph_weight_arg_edge_var_rejects_non_variable() {
        use gleaph_gql::value::Value;
        let expr = paren(Expr::new(ExprKind::Literal(Value::Float32(1.0))));
        assert_eq!(gleaph_weight_arg_edge_var(&expr), None);
    }

    #[test]
    fn recognizes_only_dotted_gleaph_weight_name() {
        let dotted = ObjectName::qualified(vec!["GLEAPH".into(), "WEIGHT".into()]);
        assert!(is_gleaph_weight_call(&dotted, false));
        assert!(!is_gleaph_weight_call(
            &ObjectName::simple("gleaph_weight"),
            false
        ));
        assert!(!is_gleaph_weight_call(&ObjectName::simple("other"), false));
    }

    #[test]
    fn decode_traversal_edge_weight_uses_edge_payload_profile() {
        use crate::facade::EdgeHandle;
        use gleaph_graph_kernel::entry::{
            EdgeDirectedness, EdgePayloadEncoding, EdgePayloadProfile,
        };
        use ic_stable_lara::{VertexId, labeled::BucketLabelKey as LaraLabelId};

        let store = GraphStore::new();
        let label_id = crate::test_labels::edge_label_id_for_name("DecodeTraversalWgt");
        store
            .install_edge_label_payload_profile_at_init(
                label_id,
                EdgePayloadProfile {
                    byte_width: 2,
                    encoding: EdgePayloadEncoding::WeightRawU16,
                },
            )
            .expect("payload profile");
        let wire = label_id.pack(EdgeDirectedness::Directed);
        let handle = EdgeHandle {
            owner_vertex_id: VertexId::from(0),
            label_id: LaraLabelId::from_raw(wire.raw()),
            slot_index: 0,
        };
        let w = decode_traversal_edge_weight(&store, handle, 2, &[9, 0]).expect("decode");
        assert_eq!(w, 9.0);

        let err = decode_traversal_edge_weight(&store, handle, 0, &[]).expect_err("no bytes");
        assert!(
            err.to_string().contains("edge payload width mismatch"),
            "unexpected error: {err}"
        );
    }
}
