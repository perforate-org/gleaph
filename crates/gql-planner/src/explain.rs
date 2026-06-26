//! Human-readable plan explanation output.
//!
//! Produces a textual representation of a [`PhysicalPlan`] for debugging
//! and query analysis.

use std::fmt::Write;

use crate::plan::*;

struct ExplainView<'a> {
    anchor: Option<&'a AnchorInfo>,
    estimated_cost: Option<f64>,
    estimated_rows: Option<f64>,
    limit_pushdown_applied: bool,
    filter_pushdown_stages: &'a [usize],
    join_order: Option<&'a [usize]>,
    has_aggregate: bool,
    indexable_properties: Option<&'a [Str]>,
    has_dml: bool,
    dml_errors: &'a [PlannerDiagnostic],
    dml_warnings: &'a [PlannerDiagnostic],
    type_warnings: &'a [TypeDiagnostic],
    ev_fusion_applied: bool,
    late_project_applied: bool,
    cyclic_patterns: Option<&'a [CyclicPattern]>,
    predicate_reordering_applied: bool,
    common_subexpressions: Option<&'a [Str]>,
    reoptimize_after_rows: Option<u64>,
    cardinality_check_points: &'a [usize],
    statically_contradictory: bool,
    use_graph_pushdown: &'a [UseGraphPushdownInfo],
}

impl<'a> ExplainView<'a> {
    fn from_plan(plan: &'a PhysicalPlan) -> Self {
        Self {
            anchor: plan.annotations.optimizer.anchor.as_ref(),
            estimated_cost: plan.annotations.optimizer.estimated_cost,
            estimated_rows: plan.annotations.optimizer.estimated_rows,
            limit_pushdown_applied: plan.annotations.optimizer.limit_pushdown_applied,
            filter_pushdown_stages: &plan.annotations.optimizer.filter_pushdown_stages,
            join_order: plan.annotations.optimizer.join_order.as_deref(),
            has_aggregate: plan.annotations.semantic.has_aggregate,
            indexable_properties: plan.annotations.semantic.indexable_properties.as_deref(),
            has_dml: plan.has_dml(),
            dml_errors: &plan.diagnostics.dml_errors,
            dml_warnings: &plan.diagnostics.dml_warnings,
            type_warnings: &plan.diagnostics.type_warnings,
            ev_fusion_applied: plan.annotations.optimizer.ev_fusion_applied,
            late_project_applied: plan.annotations.optimizer.late_project_applied,
            cyclic_patterns: plan.annotations.optimizer.cyclic_patterns.as_deref(),
            predicate_reordering_applied: plan.annotations.optimizer.predicate_reordering_applied,
            common_subexpressions: plan.annotations.optimizer.common_subexpressions.as_deref(),
            reoptimize_after_rows: plan.annotations.optimizer.reoptimize_after_rows,
            cardinality_check_points: &plan.annotations.optimizer.cardinality_check_points,
            statically_contradictory: plan.annotations.optimizer.statically_contradictory,
            use_graph_pushdown: &plan.annotations.optimizer.use_graph_pushdown,
        }
    }
}

/// Produce a human-readable explanation of a physical plan.
pub fn explain_plan(plan: &PhysicalPlan) -> String {
    let view = ExplainView::from_plan(plan);
    let mut out = String::new();

    writeln!(out, "Plan:").unwrap();
    for (i, op) in plan.ops.iter().enumerate() {
        writeln!(out, "  {}. {}", i + 1, format_op(op)).unwrap();
    }

    // Annotations.
    writeln!(out).unwrap();
    if let Some(anchor) = view.anchor {
        writeln!(
            out,
            "Anchor: {} ({})",
            anchor.variable,
            format_anchor_source(&anchor.source)
        )
        .unwrap();
    }

    if let Some(cost) = view.estimated_cost {
        writeln!(out, "Estimated cost: {:.1}", cost).unwrap();
    }

    if let Some(rows) = view.estimated_rows {
        writeln!(out, "Estimated rows: ~{:.0}", rows).unwrap();
    }

    if view.limit_pushdown_applied {
        writeln!(out, "Optimization: limit pushdown applied").unwrap();
    }

    if !view.filter_pushdown_stages.is_empty() {
        writeln!(
            out,
            "Optimization: filter pushdown to stages {:?}",
            view.filter_pushdown_stages
        )
        .unwrap();
    }

    if let Some(order) = view.join_order {
        writeln!(out, "Join order: {:?}", order).unwrap();
    }

    if view.has_aggregate {
        writeln!(out, "Aggregation: yes").unwrap();
    }

    if let Some(props) = view.indexable_properties {
        writeln!(out, "Indexable properties in WHERE: {}", props.join(", ")).unwrap();
    }

    for info in view.use_graph_pushdown {
        match (&info.supported, &info.reason) {
            (true, _) => {
                writeln!(
                    out,
                    "Remote USE GRAPH pushdown: {} = supported",
                    info.graph_name
                )
                .unwrap();
            }
            (false, Some(reason)) => {
                writeln!(
                    out,
                    "Remote USE GRAPH pushdown: {} = unsupported ({})",
                    info.graph_name, reason
                )
                .unwrap();
            }
            (false, None) => {
                writeln!(
                    out,
                    "Remote USE GRAPH pushdown: {} = unsupported",
                    info.graph_name
                )
                .unwrap();
            }
        }
    }

    if view.has_dml {
        writeln!(out, "Data modification: yes").unwrap();
    }

    for error in view.dml_errors {
        writeln!(
            out,
            "DML error [{}] at {}..{}: {}",
            error.code, error.span.start, error.span.end, error.message
        )
        .unwrap();
    }

    for warning in view.dml_warnings {
        writeln!(
            out,
            "DML warning [{}] at {}..{}: {}",
            warning.code, warning.span.start, warning.span.end, warning.message
        )
        .unwrap();
    }

    for warning in view.type_warnings {
        let code = warning.code.unwrap_or("TYPE");
        writeln!(
            out,
            "Type warning [{}] at {}..{}: {}",
            code, warning.span.start, warning.span.end, warning.message
        )
        .unwrap();
    }

    if view.ev_fusion_applied {
        writeln!(out, "Optimization: EVFusion applied").unwrap();
    }

    if view.late_project_applied {
        writeln!(out, "Optimization: late projection applied").unwrap();
    }

    if let Some(cycles) = view.cyclic_patterns {
        for cycle in cycles {
            writeln!(out, "Cyclic pattern: [{}]", cycle.variables.join(", ")).unwrap();
        }
    }

    if view.predicate_reordering_applied {
        writeln!(out, "Optimization: predicate reordering applied").unwrap();
    }

    if let Some(cses) = view.common_subexpressions
        && !cses.is_empty()
    {
        writeln!(out, "Common subexpressions: [{}]", cses.join(", ")).unwrap();
    }

    if let Some(reopt) = view.reoptimize_after_rows {
        writeln!(out, "Reoptimization hint: after {} rows", reopt).unwrap();
    }

    if !view.cardinality_check_points.is_empty() {
        writeln!(
            out,
            "Cardinality check points: {:?}",
            view.cardinality_check_points
        )
        .unwrap();
    }

    if view.statically_contradictory {
        writeln!(
            out,
            "Warning: statically contradictory (no results possible)"
        )
        .unwrap();
    }

    out
}

fn format_property_projection(names: Option<&[crate::plan::Str]>) -> String {
    match names {
        None => String::new(),
        Some([]) => " props=[]".to_owned(),
        Some(s) => {
            let joined: Vec<&str> = s.iter().map(|x| x.as_ref()).collect();
            format!(" props=[{}]", joined.join(", "))
        }
    }
}

fn format_op(op: &PlanOp) -> String {
    match op {
        PlanOp::NodeScan {
            variable,
            label,
            property_projection,
        } => {
            let base = if let Some(l) = label {
                format!("NodeScan({}, label={})", variable, l)
            } else {
                format!("NodeScan({})", variable)
            };
            format!(
                "{}{}",
                base,
                format_property_projection(property_projection.as_deref())
            )
        }

        PlanOp::IndexScan {
            variable,
            property,
            value,
            cmp,
            property_projection,
        } => {
            format!(
                "IndexScan({}, {} {} {}){}",
                variable,
                property,
                format_cmp(cmp),
                format_scan_value(value),
                format_property_projection(property_projection.as_deref())
            )
        }

        PlanOp::EdgeIndexScan {
            variable,
            property,
            value,
            property_projection,
        } => {
            format!(
                "EdgeIndexScan({}, {} = {}){}",
                variable,
                property,
                format_scan_value(value),
                format_property_projection(property_projection.as_deref())
            )
        }

        PlanOp::EdgeBindEndpoints {
            edge,
            near,
            far,
            direction,
            label,
            near_property_projection,
            far_property_projection,
            hop_aux_binding,
        } => {
            let arrow = format_direction(direction);
            let label_str = label
                .as_ref()
                .map(|l| format!(":{}", l))
                .unwrap_or_default();
            let near_pp = format_property_projection(near_property_projection.as_deref());
            let far_pp = format_property_projection(far_property_projection.as_deref());
            let hop_aux_pp = hop_aux_binding
                .as_ref()
                .map(|s| format!(" hop_aux={}", s))
                .unwrap_or_default();
            format!(
                "EdgeBindEndpoints({}{} {} near={}{} far={}{}{})",
                edge, label_str, arrow.0, near, near_pp, far, far_pp, hop_aux_pp
            )
        }

        PlanOp::ConditionalIndexScan {
            candidates,
            fallback_variable,
            fallback_label,
            property_projection,
        } => {
            let cands: Vec<String> = candidates
                .iter()
                .map(|c| format!("{}={}", c.property, c.param_name))
                .collect();
            format!(
                "ConditionalIndexScan({}, candidates=[{}], fallback={}){}",
                fallback_variable,
                cands.join(", "),
                fallback_label.as_deref().unwrap_or("*"),
                format_property_projection(property_projection.as_deref())
            )
        }

        PlanOp::PropertyFilter { predicates, stage } => {
            format!(
                "PropertyFilter(stage={}, {} predicate{})",
                stage,
                predicates.len(),
                if predicates.len() == 1 { "" } else { "s" }
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
            edge_property_projection,
            dst_property_projection,
            hop_aux_binding,
            ..
        } => {
            let arrow = format_direction(direction);
            let label_str = label
                .as_ref()
                .map(|l| format!(":{}", l))
                .unwrap_or_default();
            let label_expr_str = label_expr
                .as_ref()
                .map(|e| format!(" labelExpr={e:?}"))
                .unwrap_or_default();
            let var_len_str = var_len
                .map(|vl| {
                    if let Some(max) = vl.max {
                        format!("*{}..{}", vl.min, max)
                    } else {
                        format!("*{}..", vl.min)
                    }
                })
                .unwrap_or_default();
            let idx = indexed_edge_equality
                .as_ref()
                .map(|(p, _)| format!(" edgeIdx={}", p))
                .unwrap_or_default();
            let edge_pp = format_property_projection(edge_property_projection.as_deref());
            let dst_pp = format_property_projection(dst_property_projection.as_deref());
            let hop_aux_pp = hop_aux_binding
                .as_ref()
                .map(|v| format!(" hopAux={v}"))
                .unwrap_or_default();
            format!(
                "Expand({} {}[{}{}]{} {}{}{}{}{}{})",
                src,
                arrow.0,
                edge,
                label_str,
                var_len_str,
                dst,
                label_expr_str,
                idx,
                edge_pp,
                dst_pp,
                hop_aux_pp
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
            dst_filter,
            edge_property_projection,
            dst_property_projection,
            hop_aux_binding,
            ..
        } => {
            let arrow = format_direction(direction);
            let label_str = label
                .as_ref()
                .map(|l| format!(":{}", l))
                .unwrap_or_default();
            let label_expr_str = label_expr
                .as_ref()
                .map(|e| format!(" labelExpr={e:?}"))
                .unwrap_or_default();
            let var_len_str = var_len
                .map(|vl| {
                    if let Some(max) = vl.max {
                        format!("*{}..{}", vl.min, max)
                    } else {
                        format!("*{}..", vl.min)
                    }
                })
                .unwrap_or_default();
            let fc = dst_filter.len();
            let idx = indexed_edge_equality
                .as_ref()
                .map(|(p, _)| format!(" edgeIdx={}", p))
                .unwrap_or_default();
            let edge_pp = format_property_projection(edge_property_projection.as_deref());
            let dst_pp = format_property_projection(dst_property_projection.as_deref());
            let hop_aux_pp = hop_aux_binding
                .as_ref()
                .map(|v| format!(" hopAux={v}"))
                .unwrap_or_default();
            format!(
                "ExpandFilter({} {}[{}{}]{} {} | {} filter{}{}{}{}{}{})",
                src,
                arrow.0,
                edge,
                label_str,
                var_len_str,
                dst,
                fc,
                if fc == 1 { "" } else { "s" },
                idx,
                label_expr_str,
                edge_pp,
                dst_pp,
                hop_aux_pp
            )
        }

        PlanOp::ShortestPath {
            src,
            dst,
            mode,
            direction,
            label,
            label_expr,
            var_len,
            edge,
            cost,
            ..
        } => {
            let arrow = format_direction(direction);
            let label_str = label
                .as_deref()
                .map(|l| format!(":{l}"))
                .unwrap_or_default();
            let label_expr_str = label_expr
                .as_ref()
                .map(|e| format!(" labelExpr={e:?}"))
                .unwrap_or_default();
            let bounds = var_len
                .as_ref()
                .map(|v| {
                    if v.max == Some(v.min) {
                        format!("{{{}}}", v.min)
                    } else if let Some(m) = v.max {
                        format!("{{{}, {}}}", v.min, m)
                    } else {
                        format!("{{{}, }}", v.min)
                    }
                })
                .unwrap_or_default();
            let cost_str = match cost {
                ShortestPathCost::HopCount => String::new(),
                ShortestPathCost::EdgeCostExpr { edge_var, .. } => {
                    format!(", cost=edge({edge_var})")
                }
            };
            let mode_str = match mode {
                ShortestMode::AnyShortest => "ANY SHORTEST",
                ShortestMode::AllShortest => "ALL SHORTEST",
                ShortestMode::ShortestK(k) => {
                    return format!(
                        "ShortestPath({} {}[{}{}{}]{} {}{}, SHORTEST {}{cost_str})",
                        src, arrow.0, edge, label_str, label_expr_str, bounds, arrow.1, dst, k
                    );
                }
                ShortestMode::ShortestKGroup(k) => {
                    return format!(
                        "ShortestPath({} {}[{}{}{}]{} {}{}, SHORTEST {} GROUP{cost_str})",
                        src, arrow.0, edge, label_str, label_expr_str, bounds, arrow.1, dst, k
                    );
                }
            };
            format!(
                "ShortestPath({} {}[{}{}{}]{} {}{}, {}{cost_str})",
                src, arrow.0, edge, label_str, label_expr_str, bounds, arrow.1, dst, mode_str
            )
        }

        PlanOp::Let { bindings } => {
            let vars: Vec<&str> = bindings.iter().map(|b| b.variable.as_str()).collect();
            format!("Let({})", vars.join(", "))
        }

        PlanOp::For {
            variable,
            ordinality,
            offset_keyword,
            ..
        } => {
            if let Some(ord) = ordinality {
                if *offset_keyword {
                    format!("For({} WITH OFFSET {})", variable, ord)
                } else {
                    format!("For({} WITH ORDINALITY {})", variable, ord)
                }
            } else {
                format!("For({})", variable)
            }
        }

        PlanOp::Filter { condition } => {
            format!("Filter({})", format_expr(condition))
        }

        PlanOp::Search {
            binding,
            provider,
            output,
        } => {
            let (provider_name, index_name, query, limit, filter) = match provider {
                crate::plan::SearchProviderPlan::VectorIndex {
                    index_name,
                    query,
                    limit,
                    filter,
                } => (
                    "VectorIndex",
                    index_name.join("."),
                    format_expr(query),
                    format_expr(limit),
                    filter.as_ref().map(format_expr),
                ),
            };
            let kind = match output.kind {
                crate::plan::SearchOutputKind::Score => "SCORE",
                crate::plan::SearchOutputKind::Distance => "DISTANCE",
            };
            let filter_str = filter.map(|f| format!(", WHERE {f}")).unwrap_or_default();
            format!(
                "Search({}, {} index={}, FOR={}, LIMIT={}{}) {} AS {}",
                binding, provider_name, index_name, query, limit, filter_str, kind, output.alias
            )
        }

        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            let gb: Vec<String> = group_by.iter().map(format_expr).collect();
            let aggs: Vec<String> = aggregates
                .iter()
                .map(|a| {
                    if a.distinct {
                        format!("{:?}(DISTINCT)", a.func)
                    } else {
                        format!("{:?}()", a.func)
                    }
                })
                .collect();
            format!(
                "Aggregate(GROUP BY [{}], [{}])",
                gb.join(", "),
                aggs.join(", ")
            )
        }

        PlanOp::Project { columns, distinct } => {
            let d = if *distinct { " DISTINCT" } else { "" };
            if columns.is_empty() {
                format!("Project(*{})", d)
            } else {
                format!(
                    "Project({} column{}{})",
                    columns.len(),
                    if columns.len() == 1 { "" } else { "s" },
                    d
                )
            }
        }

        PlanOp::Sort { order_by } => {
            format!(
                "Sort({} key{})",
                order_by.items.len(),
                if order_by.items.len() == 1 { "" } else { "s" }
            )
        }

        PlanOp::Limit { count, offset } => {
            let c = count.as_ref().map(format_expr).unwrap_or_default();
            let o = offset
                .as_ref()
                .map(|e| format!(" OFFSET {}", format_expr(e)))
                .unwrap_or_default();
            format!("Limit({}{})", c, o)
        }

        PlanOp::SetOperation { op, right } => {
            let op_str = format_set_op(op);
            let right_ops: Vec<String> = right.ops.iter().map(format_op).collect();
            format!("{} [{}]", op_str, right_ops.join(" -> "))
        }

        PlanOp::OptionalMatch { sub_plan } => {
            let sub_ops: Vec<String> = sub_plan.iter().map(format_op).collect();
            format!("OptionalMatch [{}]", sub_ops.join(" -> "))
        }

        PlanOp::IndexIntersection {
            variable,
            scans,
            property_projection,
        } => {
            let specs: Vec<String> = scans
                .iter()
                .map(|s| format!("{} {:?} {:?}", s.property, s.cmp, s.value))
                .collect();
            format!(
                "IndexIntersection({}, [{}]){}",
                variable,
                specs.join(", "),
                format_property_projection(property_projection.as_deref())
            )
        }

        PlanOp::WorstCaseOptimalJoin { variables, edges } => {
            let vars = variables.join(", ");
            let edge_parts: Vec<String> = edges
                .iter()
                .map(|e| {
                    let mut s = e.label.as_deref().unwrap_or("*").to_string();
                    if let Some(h) = e.hop_aux_binding.as_deref() {
                        s.push_str(&format!(" hop_aux={h}"));
                    }
                    s
                })
                .collect();
            format!("WCOJ([{}], edges=[{}])", vars, edge_parts.join("->"))
        }

        PlanOp::TopK {
            order_by,
            k,
            offset,
        } => {
            let keys = order_by.items.len();
            let k_str = format!("{:?}", k.kind);
            let offset_str = offset
                .as_ref()
                .map(|o| format!(", OFFSET {:?}", o.kind))
                .unwrap_or_default();
            format!(
                "TopK({} key{}, k={}{})",
                keys,
                if keys == 1 { "" } else { "s" },
                k_str,
                offset_str
            )
        }

        PlanOp::Materialize { columns, distinct } => {
            let dist = if *distinct { " DISTINCT" } else { "" };
            if columns.is_empty() {
                format!("Materialize(*{})", dist)
            } else {
                format!(
                    "Materialize({} column{}{})",
                    columns.len(),
                    if columns.len() == 1 { "" } else { "s" },
                    dist
                )
            }
        }

        // ──── DML ────
        PlanOp::InsertVertex {
            variable,
            labels,
            properties,
        } => {
            let var = variable.as_deref().unwrap_or("_");
            let labels_str = if labels.is_empty() {
                String::new()
            } else {
                format!(
                    ", labels=[{}]",
                    labels
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            };
            format!(
                "InsertVertex({}{}, {} prop{})",
                var,
                labels_str,
                properties.len(),
                if properties.len() == 1 { "" } else { "s" }
            )
        }

        PlanOp::InsertEdge {
            variable,
            src,
            dst,
            labels,
            properties,
            ..
        } => {
            let var = variable.as_deref().unwrap_or("_");
            let labels_str = if labels.is_empty() {
                String::new()
            } else {
                format!(
                    ":{}",
                    labels
                        .iter()
                        .map(ToString::to_string)
                        .collect::<Vec<_>>()
                        .join(":")
                )
            };
            format!(
                "InsertEdge({} -[{}{}]-> {}, {} prop{})",
                src,
                var,
                labels_str,
                dst,
                properties.len(),
                if properties.len() == 1 { "" } else { "s" }
            )
        }

        PlanOp::SetProperties { items } => {
            format!(
                "SetProperties({} item{})",
                items.len(),
                if items.len() == 1 { "" } else { "s" }
            )
        }

        PlanOp::RemoveProperties { items } => {
            format!(
                "RemoveProperties({} item{})",
                items.len(),
                if items.len() == 1 { "" } else { "s" }
            )
        }

        PlanOp::DeleteVertex { variable } => {
            format!("DeleteVertex({})", variable)
        }

        PlanOp::DetachDeleteVertex { variable } => {
            format!("DetachDeleteVertex({})", variable)
        }

        PlanOp::DeleteEdge { variable } => {
            format!("DeleteEdge({})", variable)
        }

        // ──── Procedure / Context ────
        PlanOp::CallProcedure {
            name,
            args,
            yield_columns,
            optional,
        } => {
            let opt = if *optional { "OPTIONAL " } else { "" };
            let proc_name = name.join(".");
            let yield_str = yield_columns
                .as_ref()
                .map(|cols| {
                    let items: Vec<String> = cols
                        .iter()
                        .map(|c| {
                            if let Some(alias) = &c.alias {
                                format!("{} AS {}", c.name, alias)
                            } else {
                                c.name.to_string()
                            }
                        })
                        .collect();
                    format!(" YIELD {}", items.join(", "))
                })
                .unwrap_or_default();
            format!(
                "{}CallProcedure({}, {} arg{}{})",
                opt,
                proc_name,
                args.len(),
                if args.len() == 1 { "" } else { "s" },
                yield_str
            )
        }

        PlanOp::InlineProcedureCall {
            sub_plan,
            scope,
            optional,
        } => {
            let opt = if *optional { "OPTIONAL " } else { "" };
            let sub_ops: Vec<String> = sub_plan.ops.iter().map(format_op).collect();
            let scope = match scope {
                InlineProcedureScope::ImplicitAll => String::new(),
                InlineProcedureScope::Explicit(vars) => format!(" scope=[{}]", vars.join(", ")),
            };
            format!(
                "{}InlineProcedureCall{} [{}]",
                opt,
                scope,
                sub_ops.join(" -> ")
            )
        }

        PlanOp::UseGraph {
            graph_name,
            sub_plan,
        } => {
            let name = graph_name.join(".");
            if let Some(sub_ops) = sub_plan {
                let inner: Vec<String> = sub_ops.iter().map(format_op).collect();
                format!("UseGraph({}) [{}]", name, inner.join(" -> "))
            } else {
                format!("UseGraph({})", name)
            }
        }

        // ──── Join ────
        PlanOp::HashJoin {
            left,
            right,
            join_keys,
        } => {
            let l: Vec<String> = left.iter().map(format_op).collect();
            let r: Vec<String> = right.iter().map(format_op).collect();
            format!(
                "HashJoin(keys=[{}], left=[{}], right=[{}])",
                join_keys.join(", "),
                l.join(" -> "),
                r.join(" -> ")
            )
        }

        PlanOp::CartesianProduct { left, right } => {
            let l: Vec<String> = left.iter().map(format_op).collect();
            let r: Vec<String> = right.iter().map(format_op).collect();
            format!(
                "CartesianProduct(left=[{}], right=[{}])",
                l.join(" -> "),
                r.join(" -> ")
            )
        }
    }
}

fn format_cmp(cmp: &CmpOp) -> &str {
    match cmp {
        CmpOp::Eq => "=",
        CmpOp::Ne => "!=",
        CmpOp::Lt => "<",
        CmpOp::Le => "<=",
        CmpOp::Gt => ">",
        CmpOp::Ge => ">=",
    }
}

fn format_scan_value(val: &ScanValue) -> String {
    match val {
        ScanValue::Literal(v) => format!("{:?}", v),
        ScanValue::Parameter(p) => format!("${}", p),
    }
}

fn format_anchor_source(source: &AnchorSource) -> String {
    match source {
        AnchorSource::PropertyEquality { property } => {
            format!("property-equality on {}", property)
        }
        AnchorSource::InlinePropertyEquality { property } => {
            format!("inline-property-equality on {}", property)
        }
        AnchorSource::PropertyRange { property, cmp, .. } => {
            format!("property-range on {} {:?}", property, cmp)
        }
        AnchorSource::LabelCardinality { label } => {
            format!("label-cardinality: {}", label)
        }
        AnchorSource::SchemaEndpoint => "schema-endpoint".to_string(),
        AnchorSource::FullScan => "full-scan".to_string(),
    }
}

fn format_direction(dir: &gleaph_gql::types::EdgeDirection) -> (&str, &str) {
    use gleaph_gql::types::EdgeDirection;
    match dir {
        EdgeDirection::PointingRight => ("-", "->"),
        EdgeDirection::PointingLeft => ("<-", "-"),
        EdgeDirection::LeftOrRight => ("<-", "->"),
        EdgeDirection::Undirected => ("~", "~"),
        _ => ("-", "-"),
    }
}

fn format_set_op(op: &gleaph_gql::ast::SetOp) -> &str {
    use gleaph_gql::ast::SetOp;
    match op {
        SetOp::Union => "UNION",
        SetOp::UnionAll => "UNION ALL",
        SetOp::UnionDistinct => "UNION DISTINCT",
        SetOp::Except => "EXCEPT",
        SetOp::ExceptAll => "EXCEPT ALL",
        SetOp::ExceptDistinct => "EXCEPT DISTINCT",
        SetOp::Intersect => "INTERSECT",
        SetOp::IntersectAll => "INTERSECT ALL",
        SetOp::IntersectDistinct => "INTERSECT DISTINCT",
        SetOp::Otherwise => "OTHERWISE",
    }
}

/// Format a GQL expression for human-readable output.
fn format_expr(expr: &gleaph_gql::ast::Expr) -> String {
    use gleaph_gql::ast::ExprKind;
    match &expr.kind {
        ExprKind::Variable(v) => v.clone(),
        ExprKind::Parameter(p) => format!("${}", p),
        ExprKind::Literal(v) => format!("{:?}", v),
        ExprKind::PropertyAccess { expr, property } => {
            format!("{}.{}", format_expr(expr), property)
        }
        ExprKind::BinaryOp { left, op, right } => {
            let op_str = match op {
                gleaph_gql::ast::BinaryOp::Add => "+",
                gleaph_gql::ast::BinaryOp::Sub => "-",
                gleaph_gql::ast::BinaryOp::Mul => "*",
                gleaph_gql::ast::BinaryOp::Div => "/",
            };
            format!("{} {} {}", format_expr(left), op_str, format_expr(right))
        }
        ExprKind::Compare { left, op, right } => {
            format!(
                "{} {} {}",
                format_expr(left),
                format_cmp(op),
                format_expr(right)
            )
        }
        ExprKind::And(l, r) => format!("{} AND {}", format_expr(l), format_expr(r)),
        ExprKind::Or(l, r) => format!("{} OR {}", format_expr(l), format_expr(r)),
        ExprKind::Not(e) => format!("NOT {}", format_expr(e)),
        ExprKind::IsNull(e) => format!("{} IS NULL", format_expr(e)),
        ExprKind::IsNotNull(e) => format!("{} IS NOT NULL", format_expr(e)),
        ExprKind::FunctionCall { name, args, .. } => {
            let args_str: Vec<String> = args.iter().map(format_expr).collect();
            format!("{}({})", name.parts.join("."), args_str.join(", "))
        }
        ExprKind::Aggregate {
            func,
            expr: agg_expr,
            distinct,
            ..
        } => {
            let d = if *distinct { "DISTINCT " } else { "" };
            let arg = agg_expr
                .as_ref()
                .map(|e| format_expr(e))
                .unwrap_or("*".to_string());
            format!("{:?}({}{})", func, d, arg)
        }
        ExprKind::Paren(e) => format!("({})", format_expr(e)),
        ExprKind::UnaryOp { op, expr } => {
            let op_str = match op {
                gleaph_gql::ast::UnaryOp::Neg => "-",
                gleaph_gql::ast::UnaryOp::Pos => "+",
            };
            format!("{}{}", op_str, format_expr(expr))
        }
        _ => "...".to_string(),
    }
}
