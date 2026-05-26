use gleaph_gql::ast::{Expr, OrderByClause};
use gleaph_gql::types::LabelExpr;

use crate::plan::{
    AggregateSpec, PlanOp, ProjectColumn, PropertyAssignment, SetPlanItem, ShortestPathCost,
    WcojEdge,
};

use super::helpers::{
    encode_conditional_candidate, encode_edge_value_predicate, encode_edge_vector_predicate,
    encode_index_scan_spec, encode_indexed_edge_equality, encode_remove_item, encode_scan_value,
    encode_yield_column, opt_str_opt, opt_str_slice, shortest_mode_to_wire, var_len_to_wire,
    vec_str,
};
use super::physical_plan_to_wire;
use super::pools::{rkyv_encode_expr, rkyv_encode_label_expr, rkyv_encode_order_by};
use super::types::*;

#[derive(Default)]
pub(super) struct Encoder {
    pub(super) expr_pool: Vec<Vec<u8>>,
    pub(super) label_expr_pool: Vec<Vec<u8>>,
    pub(super) order_by_pool: Vec<Vec<u8>>,
}

impl Encoder {
    fn intern_expr(&mut self, expr: &Expr) -> Result<u32, String> {
        let id = u32::try_from(self.expr_pool.len()).map_err(|_| "expr_pool overflow")?;
        self.expr_pool.push(rkyv_encode_expr(expr)?);
        Ok(id)
    }

    fn intern_exprs(&mut self, exprs: &[Expr]) -> Result<Vec<u32>, String> {
        exprs.iter().map(|e| self.intern_expr(e)).collect()
    }

    fn intern_label_expr(&mut self, expr: &LabelExpr) -> Result<u32, String> {
        let id =
            u32::try_from(self.label_expr_pool.len()).map_err(|_| "label_expr_pool overflow")?;
        self.label_expr_pool.push(rkyv_encode_label_expr(expr)?);
        Ok(id)
    }

    fn intern_order_by(&mut self, ob: &OrderByClause) -> Result<u32, String> {
        let id = u32::try_from(self.order_by_pool.len()).map_err(|_| "order_by_pool overflow")?;
        self.order_by_pool.push(rkyv_encode_order_by(ob)?);
        Ok(id)
    }

    pub(super) fn encode_ops(&mut self, ops: &[PlanOp]) -> Result<Vec<PlanOpWire>, String> {
        ops.iter().map(|op| self.encode_op(op)).collect()
    }

    fn encode_op(&mut self, op: &PlanOp) -> Result<PlanOpWire, String> {
        Ok(match op {
            PlanOp::NodeScan {
                variable,
                label,
                property_projection,
            } => PlanOpWire::NodeScan {
                variable: variable.to_string(),
                label: opt_str_opt(label),
                property_projection: opt_str_slice(property_projection),
            },
            PlanOp::IndexScan {
                variable,
                property,
                value,
                cmp,
                property_projection,
            } => PlanOpWire::IndexScan {
                variable: variable.to_string(),
                property: property.to_string(),
                value: encode_scan_value(value)?,
                cmp: *cmp,
                property_projection: opt_str_slice(property_projection),
            },
            PlanOp::EdgeIndexScan {
                variable,
                property,
                value,
                property_projection,
            } => PlanOpWire::EdgeIndexScan {
                variable: variable.to_string(),
                property: property.to_string(),
                value: encode_scan_value(value)?,
                property_projection: opt_str_slice(property_projection),
            },
            PlanOp::EdgeBindEndpoints {
                edge,
                near,
                far,
                direction,
                label,
                near_property_projection,
                far_property_projection,
                hop_aux_binding,
            } => PlanOpWire::EdgeBindEndpoints {
                edge: edge.to_string(),
                near: near.to_string(),
                far: far.to_string(),
                direction: *direction,
                label: opt_str_opt(label),
                near_property_projection: opt_str_slice(near_property_projection),
                far_property_projection: opt_str_slice(far_property_projection),
                hop_aux_binding: opt_str_opt(hop_aux_binding),
            },
            PlanOp::ConditionalIndexScan {
                candidates,
                fallback_label,
                fallback_variable,
                property_projection,
            } => PlanOpWire::ConditionalIndexScan {
                candidates: candidates
                    .iter()
                    .map(encode_conditional_candidate)
                    .collect(),
                fallback_label: opt_str_opt(fallback_label),
                fallback_variable: fallback_variable.to_string(),
                property_projection: opt_str_slice(property_projection),
            },
            PlanOp::PropertyFilter { predicates, stage } => PlanOpWire::PropertyFilter {
                predicates: self.intern_exprs(predicates)?,
                stage: *stage,
            },
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
            } => PlanOpWire::Expand {
                src: src.to_string(),
                edge: edge.to_string(),
                dst: dst.to_string(),
                direction: *direction,
                label: opt_str_opt(label),
                label_expr: opt_label_expr_id(self, label_expr.as_ref())?,
                var_len: var_len.map(var_len_to_wire),
                indexed_edge_equality: encode_indexed_edge_equality(indexed_edge_equality)?,
                edge_value_predicate: encode_edge_value_predicate(edge_value_predicate)?,
                edge_vector_predicate: encode_edge_vector_predicate(edge_vector_predicate)?,
                edge_property_projection: opt_str_slice(edge_property_projection),
                dst_property_projection: opt_str_slice(dst_property_projection),
                hop_aux_binding: opt_str_opt(hop_aux_binding),
                emit_edge_binding: *emit_edge_binding,
            },
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
            } => PlanOpWire::ExpandFilter {
                src: src.to_string(),
                edge: edge.to_string(),
                dst: dst.to_string(),
                direction: *direction,
                label: opt_str_opt(label),
                label_expr: opt_label_expr_id(self, label_expr.as_ref())?,
                var_len: var_len.map(var_len_to_wire),
                indexed_edge_equality: encode_indexed_edge_equality(indexed_edge_equality)?,
                edge_value_predicate: encode_edge_value_predicate(edge_value_predicate)?,
                edge_vector_predicate: encode_edge_vector_predicate(edge_vector_predicate)?,
                dst_filter: self.intern_exprs(dst_filter)?,
                edge_property_projection: opt_str_slice(edge_property_projection),
                dst_property_projection: opt_str_slice(dst_property_projection),
                hop_aux_binding: opt_str_opt(hop_aux_binding),
                emit_edge_binding: *emit_edge_binding,
            },
            PlanOp::ShortestPath {
                src,
                dst,
                edge,
                path_var,
                emit_edge_binding,
                emit_path_binding,
                mode,
                direction,
                label,
                label_expr,
                var_len,
                cost,
            } => PlanOpWire::ShortestPath {
                src: src.to_string(),
                dst: dst.to_string(),
                edge: edge.to_string(),
                path_var: opt_str_opt(path_var),
                emit_edge_binding: *emit_edge_binding,
                emit_path_binding: *emit_path_binding,
                mode: shortest_mode_to_wire(*mode),
                direction: *direction,
                label: opt_str_opt(label),
                label_expr: opt_label_expr_id(self, label_expr.as_ref())?,
                var_len: var_len.map(var_len_to_wire),
                cost: encode_shortest_path_cost(self, cost)?,
            },
            PlanOp::Let { bindings } => PlanOpWire::Let {
                bindings: bindings
                    .iter()
                    .map(|b| {
                        Ok(LetBindingWire {
                            variable: b.variable.clone(),
                            value: self.intern_expr(&b.value)?,
                        })
                    })
                    .collect::<Result<_, String>>()?,
            },
            PlanOp::For {
                variable,
                list,
                ordinality,
            } => PlanOpWire::For {
                variable: variable.to_string(),
                list: self.intern_expr(list)?,
                ordinality: opt_str_opt(ordinality),
            },
            PlanOp::Filter { condition } => PlanOpWire::Filter {
                condition: self.intern_expr(condition)?,
            },
            PlanOp::CallProcedure {
                name,
                args,
                yield_columns,
                optional,
            } => PlanOpWire::CallProcedure {
                name: vec_str(name),
                args: self.intern_exprs(args)?,
                yield_columns: yield_columns
                    .as_ref()
                    .map(|cols| cols.iter().map(encode_yield_column).collect()),
                optional: *optional,
            },
            PlanOp::InlineProcedureCall {
                sub_plan,
                scope_vars,
                optional,
            } => PlanOpWire::InlineProcedureCall {
                sub_plan: Box::new(physical_plan_to_wire(sub_plan)?),
                scope_vars: vec_str(scope_vars),
                optional: *optional,
            },
            PlanOp::UseGraph {
                graph_name,
                sub_plan,
            } => PlanOpWire::UseGraph {
                graph_name: vec_str(graph_name),
                sub_plan: sub_plan
                    .as_ref()
                    .map(|ops| self.encode_ops(ops))
                    .transpose()?,
            },
            PlanOp::HashJoin {
                left,
                right,
                join_keys,
            } => PlanOpWire::HashJoin {
                left: self.encode_ops(left)?,
                right: self.encode_ops(right)?,
                join_keys: vec_str(join_keys),
            },
            PlanOp::CartesianProduct { left, right } => PlanOpWire::CartesianProduct {
                left: self.encode_ops(left)?,
                right: self.encode_ops(right)?,
            },
            PlanOp::Aggregate {
                group_by,
                aggregates,
            } => PlanOpWire::Aggregate {
                group_by: self.intern_exprs(group_by)?,
                aggregates: aggregates
                    .iter()
                    .map(|a| self.encode_aggregate_spec(a))
                    .collect::<Result<_, _>>()?,
            },
            PlanOp::Project { columns, distinct } => PlanOpWire::Project {
                columns: columns
                    .iter()
                    .map(|c| self.encode_project_column(c))
                    .collect::<Result<_, _>>()?,
                distinct: *distinct,
            },
            PlanOp::Sort { order_by } => PlanOpWire::Sort {
                order_by: self.intern_order_by(order_by)?,
            },
            PlanOp::Limit { count, offset } => PlanOpWire::Limit {
                count: opt_expr_id(self, count.as_ref())?,
                offset: opt_expr_id(self, offset.as_ref())?,
            },
            PlanOp::SetOperation { op, right } => PlanOpWire::SetOperation {
                op: *op,
                right: Box::new(physical_plan_to_wire(right)?),
            },
            PlanOp::OptionalMatch { sub_plan } => PlanOpWire::OptionalMatch {
                sub_plan: self.encode_ops(sub_plan)?,
            },
            PlanOp::IndexIntersection {
                variable,
                scans,
                property_projection,
            } => PlanOpWire::IndexIntersection {
                variable: variable.to_string(),
                scans: scans
                    .iter()
                    .map(|s| encode_index_scan_spec(s))
                    .collect::<Result<_, _>>()?,
                property_projection: opt_str_slice(property_projection),
            },
            PlanOp::WorstCaseOptimalJoin { variables, edges } => PlanOpWire::WorstCaseOptimalJoin {
                variables: vec_str(variables),
                edges: edges
                    .iter()
                    .map(|e| self.encode_wcoj_edge(e))
                    .collect::<Result<_, _>>()?,
            },
            PlanOp::TopK {
                order_by,
                k,
                offset,
            } => PlanOpWire::TopK {
                order_by: self.intern_order_by(order_by)?,
                k: self.intern_expr(k)?,
                offset: opt_expr_id(self, offset.as_ref())?,
            },
            PlanOp::Materialize { columns, distinct } => PlanOpWire::Materialize {
                columns: columns
                    .iter()
                    .map(|c| self.encode_project_column(c))
                    .collect::<Result<_, _>>()?,
                distinct: *distinct,
            },
            PlanOp::InsertVertex {
                variable,
                labels,
                properties,
            } => PlanOpWire::InsertVertex {
                variable: opt_str_opt(variable),
                labels: vec_str(labels),
                properties: properties
                    .iter()
                    .map(|p| self.encode_property_assignment(p))
                    .collect::<Result<_, _>>()?,
            },
            PlanOp::InsertEdge {
                variable,
                src,
                dst,
                direction,
                labels,
                properties,
            } => PlanOpWire::InsertEdge {
                variable: opt_str_opt(variable),
                src: src.to_string(),
                dst: dst.to_string(),
                direction: *direction,
                labels: vec_str(labels),
                properties: properties
                    .iter()
                    .map(|p| self.encode_property_assignment(p))
                    .collect::<Result<_, _>>()?,
            },
            PlanOp::SetProperties { items } => PlanOpWire::SetProperties {
                items: items
                    .iter()
                    .map(|i| self.encode_set_item(i))
                    .collect::<Result<_, _>>()?,
            },
            PlanOp::RemoveProperties { items } => PlanOpWire::RemoveProperties {
                items: items.iter().map(encode_remove_item).collect(),
            },
            PlanOp::DeleteVertex { variable } => PlanOpWire::DeleteVertex {
                variable: variable.to_string(),
            },
            PlanOp::DetachDeleteVertex { variable } => PlanOpWire::DetachDeleteVertex {
                variable: variable.to_string(),
            },
            PlanOp::DeleteEdge { variable } => PlanOpWire::DeleteEdge {
                variable: variable.to_string(),
            },
        })
    }

    fn encode_aggregate_spec(&mut self, spec: &AggregateSpec) -> Result<AggregateSpecWire, String> {
        Ok(AggregateSpecWire {
            func: spec.func,
            expr: opt_expr_id(self, spec.expr.as_ref())?,
            expr2: opt_expr_id(self, spec.expr2.as_ref())?,
            distinct: spec.distinct,
            filter: opt_expr_id(self, spec.filter.as_ref())?,
            order_by: spec
                .order_by
                .as_ref()
                .map(|ob| self.intern_order_by(ob))
                .transpose()?,
            alias: opt_str_opt(&spec.alias),
        })
    }

    fn encode_project_column(&mut self, col: &ProjectColumn) -> Result<ProjectColumnWire, String> {
        Ok(ProjectColumnWire {
            expr: self.intern_expr(&col.expr)?,
            alias: opt_str_opt(&col.alias),
        })
    }

    fn encode_property_assignment(
        &mut self,
        pa: &PropertyAssignment,
    ) -> Result<PropertyAssignmentWire, String> {
        Ok(PropertyAssignmentWire {
            name: pa.name.to_string(),
            value: self.intern_expr(&pa.value)?,
        })
    }

    fn encode_set_item(&mut self, item: &SetPlanItem) -> Result<SetPlanItemWire, String> {
        Ok(match item {
            SetPlanItem::Property {
                variable,
                property,
                value,
            } => SetPlanItemWire::Property {
                variable: variable.to_string(),
                property: property.to_string(),
                value: self.intern_expr(value)?,
            },
            SetPlanItem::AllProperties { variable, value } => SetPlanItemWire::AllProperties {
                variable: variable.to_string(),
                value: self.intern_expr(value)?,
            },
            SetPlanItem::Label { variable, label } => SetPlanItemWire::Label {
                variable: variable.to_string(),
                label: label.to_string(),
            },
        })
    }

    fn encode_wcoj_edge(&mut self, edge: &WcojEdge) -> Result<WcojEdgeWire, String> {
        Ok(WcojEdgeWire {
            src: edge.src.to_string(),
            dst: edge.dst.to_string(),
            variable: edge.variable.to_string(),
            label: opt_str_opt(&edge.label),
            label_expr: opt_label_expr_id(self, edge.label_expr.as_ref())?,
            direction: edge.direction,
            var_len: edge.var_len.map(var_len_to_wire),
            indexed_edge_equality: encode_indexed_edge_equality(&edge.indexed_edge_equality)?,
            dst_filter: self.intern_exprs(&edge.dst_filter)?,
            hop_aux_binding: opt_str_opt(&edge.hop_aux_binding),
        })
    }
}

fn opt_expr_id(enc: &mut Encoder, expr: Option<&Expr>) -> Result<Option<u32>, String> {
    expr.map(|e| enc.intern_expr(e)).transpose()
}

fn opt_label_expr_id(enc: &mut Encoder, expr: Option<&LabelExpr>) -> Result<Option<u32>, String> {
    expr.map(|e| enc.intern_label_expr(e)).transpose()
}

pub(super) fn encode_shortest_path_cost(
    enc: &mut Encoder,
    cost: &ShortestPathCost,
) -> Result<super::types::ShortestPathCostWire, String> {
    use super::types::ShortestPathCostWire;
    Ok(match cost {
        ShortestPathCost::HopCount => ShortestPathCostWire::HopCount,
        ShortestPathCost::EdgeCostExpr { edge_var, expr } => ShortestPathCostWire::EdgeCostExpr {
            edge_var: edge_var.to_string(),
            expr: enc.intern_expr(expr)?,
        },
    })
}
