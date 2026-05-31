use gleaph_gql::ast::{Expr, OrderByClause};
use gleaph_gql::types::LabelExpr;

use crate::plan::{
    AggregateSpec, PlanOp, ProjectColumn, PropertyAssignment, SetPlanItem, WcojEdge,
};
use gleaph_gql::ast::LetBinding;
use gleaph_gql::token::Span;

use super::PhysicalPlanWire;
use super::helpers::{
    decode_conditional_candidate, decode_edge_payload_predicate, decode_edge_vector_predicate,
    decode_index_scan_spec, decode_indexed_edge_equality, decode_remove_item, decode_scan_value,
    decode_str_slice, decode_yield_column, opt_rc_str, rc_str, shortest_mode_from_wire,
    var_len_from_wire, vec_rc_str,
};
use super::physical_plan_from_wire;
use super::pools::{rkyv_decode_expr, rkyv_decode_label_expr, rkyv_decode_order_by};
use super::types::*;
use crate::plan::ShortestPathCost;

pub(super) struct Decoder<'a> {
    wire: &'a PhysicalPlanWire,
}

impl<'a> Decoder<'a> {
    pub(super) fn new(wire: &'a PhysicalPlanWire) -> Self {
        Self { wire }
    }

    fn expr(&self, id: u32) -> Result<Expr, String> {
        let bytes = self
            .wire
            .expr_pool
            .get(id as usize)
            .ok_or_else(|| format!("expr id {id} out of range"))?;
        rkyv_decode_expr(bytes)
    }

    fn opt_expr(&self, id: Option<u32>) -> Result<Option<Expr>, String> {
        id.map(|i| self.expr(i)).transpose()
    }

    fn label_expr(&self, id: u32) -> Result<LabelExpr, String> {
        let bytes = self
            .wire
            .label_expr_pool
            .get(id as usize)
            .ok_or_else(|| format!("label_expr id {id} out of range"))?;
        rkyv_decode_label_expr(bytes)
    }

    fn order_by(&self, id: u32) -> Result<OrderByClause, String> {
        let bytes = self
            .wire
            .order_by_pool
            .get(id as usize)
            .ok_or_else(|| format!("order_by id {id} out of range"))?;
        rkyv_decode_order_by(bytes)
    }

    pub(super) fn decode_ops(&self, ops: &[PlanOpWire]) -> Result<Vec<PlanOp>, String> {
        ops.iter().map(|op| self.decode_op(op)).collect()
    }

    fn decode_op(&self, op: &PlanOpWire) -> Result<PlanOp, String> {
        Ok(match op {
            PlanOpWire::NodeScan {
                variable,
                label,
                property_projection,
            } => PlanOp::NodeScan {
                variable: rc_str(variable),
                label: opt_rc_str(label),
                property_projection: decode_str_slice(property_projection),
            },
            PlanOpWire::IndexScan {
                variable,
                property,
                value,
                cmp,
                property_projection,
            } => PlanOp::IndexScan {
                variable: rc_str(variable),
                property: rc_str(property),
                value: decode_scan_value(value)?,
                cmp: *cmp,
                property_projection: decode_str_slice(property_projection),
            },
            PlanOpWire::EdgeIndexScan {
                variable,
                property,
                value,
                property_projection,
            } => PlanOp::EdgeIndexScan {
                variable: rc_str(variable),
                property: rc_str(property),
                value: decode_scan_value(value)?,
                property_projection: decode_str_slice(property_projection),
            },
            PlanOpWire::EdgeBindEndpoints {
                edge,
                near,
                far,
                direction,
                label,
                near_property_projection,
                far_property_projection,
                hop_aux_binding,
            } => PlanOp::EdgeBindEndpoints {
                edge: rc_str(edge),
                near: rc_str(near),
                far: rc_str(far),
                direction: *direction,
                label: opt_rc_str(label),
                near_property_projection: decode_str_slice(near_property_projection),
                far_property_projection: decode_str_slice(far_property_projection),
                hop_aux_binding: opt_rc_str(hop_aux_binding),
            },
            PlanOpWire::ConditionalIndexScan {
                candidates,
                fallback_label,
                fallback_variable,
                property_projection,
            } => PlanOp::ConditionalIndexScan {
                candidates: candidates
                    .iter()
                    .map(decode_conditional_candidate)
                    .collect(),
                fallback_label: opt_rc_str(fallback_label),
                fallback_variable: rc_str(fallback_variable),
                property_projection: decode_str_slice(property_projection),
            },
            PlanOpWire::PropertyFilter { predicates, stage } => PlanOp::PropertyFilter {
                predicates: predicates
                    .iter()
                    .map(|id| self.expr(*id))
                    .collect::<Result<_, _>>()?,
                stage: *stage,
            },
            PlanOpWire::Expand {
                src,
                edge,
                dst,
                direction,
                label,
                label_expr,
                var_len,
                indexed_edge_equality,
                edge_payload_predicate,
                edge_vector_predicate,
                edge_property_projection,
                dst_property_projection,
                hop_aux_binding,
                emit_edge_binding,
            } => PlanOp::Expand {
                src: rc_str(src),
                edge: rc_str(edge),
                dst: rc_str(dst),
                direction: *direction,
                label: opt_rc_str(label),
                label_expr: decode_opt_label_expr(self, *label_expr)?,
                var_len: var_len.map(var_len_from_wire),
                indexed_edge_equality: decode_indexed_edge_equality(indexed_edge_equality)?,
                edge_payload_predicate: decode_edge_payload_predicate(edge_payload_predicate)?,
                edge_vector_predicate: decode_edge_vector_predicate(edge_vector_predicate)?,
                edge_property_projection: decode_str_slice(edge_property_projection),
                dst_property_projection: decode_str_slice(dst_property_projection),
                hop_aux_binding: opt_rc_str(hop_aux_binding),
                emit_edge_binding: *emit_edge_binding,
            },
            PlanOpWire::ExpandFilter {
                src,
                edge,
                dst,
                direction,
                label,
                label_expr,
                var_len,
                indexed_edge_equality,
                edge_payload_predicate,
                edge_vector_predicate,
                dst_filter,
                edge_property_projection,
                dst_property_projection,
                hop_aux_binding,
                emit_edge_binding,
            } => PlanOp::ExpandFilter {
                src: rc_str(src),
                edge: rc_str(edge),
                dst: rc_str(dst),
                direction: *direction,
                label: opt_rc_str(label),
                label_expr: decode_opt_label_expr(self, *label_expr)?,
                var_len: var_len.map(var_len_from_wire),
                indexed_edge_equality: decode_indexed_edge_equality(indexed_edge_equality)?,
                edge_payload_predicate: decode_edge_payload_predicate(edge_payload_predicate)?,
                edge_vector_predicate: decode_edge_vector_predicate(edge_vector_predicate)?,
                dst_filter: dst_filter
                    .iter()
                    .map(|id| self.expr(*id))
                    .collect::<Result<_, _>>()?,
                edge_property_projection: decode_str_slice(edge_property_projection),
                dst_property_projection: decode_str_slice(dst_property_projection),
                hop_aux_binding: opt_rc_str(hop_aux_binding),
                emit_edge_binding: *emit_edge_binding,
            },
            PlanOpWire::ShortestPath {
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
            } => PlanOp::ShortestPath {
                src: rc_str(src),
                dst: rc_str(dst),
                edge: rc_str(edge),
                path_var: opt_rc_str(path_var),
                emit_edge_binding: *emit_edge_binding,
                emit_path_binding: *emit_path_binding,
                mode: shortest_mode_from_wire(*mode),
                direction: *direction,
                label: opt_rc_str(label),
                label_expr: decode_opt_label_expr(self, *label_expr)?,
                var_len: var_len.map(var_len_from_wire),
                cost: decode_shortest_path_cost(self, cost)?,
            },
            PlanOpWire::Let { bindings } => PlanOp::Let {
                bindings: bindings
                    .iter()
                    .map(|b| {
                        Ok(LetBinding {
                            span: Span::DUMMY,
                            variable: b.variable.clone(),
                            value: self.expr(b.value)?,
                        })
                    })
                    .collect::<Result<_, String>>()?,
            },
            PlanOpWire::For {
                variable,
                list,
                ordinality,
            } => PlanOp::For {
                variable: rc_str(variable),
                list: self.expr(*list)?,
                ordinality: opt_rc_str(ordinality),
            },
            PlanOpWire::Filter { condition } => PlanOp::Filter {
                condition: self.expr(*condition)?,
            },
            PlanOpWire::CallProcedure {
                name,
                args,
                yield_columns,
                optional,
            } => PlanOp::CallProcedure {
                name: vec_rc_str(name),
                args: args
                    .iter()
                    .map(|id| self.expr(*id))
                    .collect::<Result<_, _>>()?,
                yield_columns: yield_columns
                    .as_ref()
                    .map(|cols| cols.iter().map(decode_yield_column).collect()),
                optional: *optional,
            },
            PlanOpWire::InlineProcedureCall {
                sub_plan,
                scope_vars,
                optional,
            } => PlanOp::InlineProcedureCall {
                sub_plan: Box::new(physical_plan_from_wire(sub_plan)?),
                scope_vars: vec_rc_str(scope_vars),
                optional: *optional,
            },
            PlanOpWire::UseGraph {
                graph_name,
                sub_plan,
            } => PlanOp::UseGraph {
                graph_name: vec_rc_str(graph_name),
                sub_plan: sub_plan
                    .as_ref()
                    .map(|ops| self.decode_ops(ops))
                    .transpose()?,
            },
            PlanOpWire::HashJoin {
                left,
                right,
                join_keys,
            } => PlanOp::HashJoin {
                left: self.decode_ops(left)?,
                right: self.decode_ops(right)?,
                join_keys: vec_rc_str(join_keys),
            },
            PlanOpWire::CartesianProduct { left, right } => PlanOp::CartesianProduct {
                left: self.decode_ops(left)?,
                right: self.decode_ops(right)?,
            },
            PlanOpWire::Aggregate {
                group_by,
                aggregates,
            } => PlanOp::Aggregate {
                group_by: group_by
                    .iter()
                    .map(|id| self.expr(*id))
                    .collect::<Result<_, _>>()?,
                aggregates: aggregates
                    .iter()
                    .map(|a| self.decode_aggregate_spec(a))
                    .collect::<Result<_, _>>()?,
            },
            PlanOpWire::Project { columns, distinct } => PlanOp::Project {
                columns: columns
                    .iter()
                    .map(|c| self.decode_project_column(c))
                    .collect::<Result<_, _>>()?,
                distinct: *distinct,
            },
            PlanOpWire::Sort { order_by } => PlanOp::Sort {
                order_by: self.order_by(*order_by)?,
            },
            PlanOpWire::Limit { count, offset } => PlanOp::Limit {
                count: self.opt_expr(*count)?,
                offset: self.opt_expr(*offset)?,
            },
            PlanOpWire::SetOperation { op, right } => PlanOp::SetOperation {
                op: *op,
                right: Box::new(physical_plan_from_wire(right)?),
            },
            PlanOpWire::OptionalMatch { sub_plan } => PlanOp::OptionalMatch {
                sub_plan: self.decode_ops(sub_plan)?,
            },
            PlanOpWire::IndexIntersection {
                variable,
                scans,
                property_projection,
            } => PlanOp::IndexIntersection {
                variable: rc_str(variable),
                scans: scans
                    .iter()
                    .map(decode_index_scan_spec)
                    .collect::<Result<_, _>>()?,
                property_projection: decode_str_slice(property_projection),
            },
            PlanOpWire::WorstCaseOptimalJoin { variables, edges } => PlanOp::WorstCaseOptimalJoin {
                variables: vec_rc_str(variables),
                edges: edges
                    .iter()
                    .map(|e| self.decode_wcoj_edge(e))
                    .collect::<Result<_, _>>()?,
            },
            PlanOpWire::TopK {
                order_by,
                k,
                offset,
            } => PlanOp::TopK {
                order_by: self.order_by(*order_by)?,
                k: self.expr(*k)?,
                offset: self.opt_expr(*offset)?,
            },
            PlanOpWire::Materialize { columns, distinct } => PlanOp::Materialize {
                columns: columns
                    .iter()
                    .map(|c| self.decode_project_column(c))
                    .collect::<Result<_, _>>()?,
                distinct: *distinct,
            },
            PlanOpWire::InsertVertex {
                variable,
                labels,
                properties,
            } => PlanOp::InsertVertex {
                variable: opt_rc_str(variable),
                labels: vec_rc_str(labels),
                properties: properties
                    .iter()
                    .map(|p| self.decode_property_assignment(p))
                    .collect::<Result<_, _>>()?,
            },
            PlanOpWire::InsertEdge {
                variable,
                src,
                dst,
                direction,
                labels,
                properties,
            } => PlanOp::InsertEdge {
                variable: opt_rc_str(variable),
                src: rc_str(src),
                dst: rc_str(dst),
                direction: *direction,
                labels: vec_rc_str(labels),
                properties: properties
                    .iter()
                    .map(|p| self.decode_property_assignment(p))
                    .collect::<Result<_, _>>()?,
            },
            PlanOpWire::SetProperties { items } => PlanOp::SetProperties {
                items: items
                    .iter()
                    .map(|i| self.decode_set_item(i))
                    .collect::<Result<_, _>>()?,
            },
            PlanOpWire::RemoveProperties { items } => PlanOp::RemoveProperties {
                items: items.iter().map(decode_remove_item).collect(),
            },
            PlanOpWire::DeleteVertex { variable } => PlanOp::DeleteVertex {
                variable: rc_str(variable),
            },
            PlanOpWire::DetachDeleteVertex { variable } => PlanOp::DetachDeleteVertex {
                variable: rc_str(variable),
            },
            PlanOpWire::DeleteEdge { variable } => PlanOp::DeleteEdge {
                variable: rc_str(variable),
            },
        })
    }

    fn decode_aggregate_spec(&self, spec: &AggregateSpecWire) -> Result<AggregateSpec, String> {
        Ok(AggregateSpec {
            func: spec.func,
            expr: self.opt_expr(spec.expr)?,
            expr2: self.opt_expr(spec.expr2)?,
            distinct: spec.distinct,
            filter: self.opt_expr(spec.filter)?,
            order_by: spec.order_by.map(|id| self.order_by(id)).transpose()?,
            alias: opt_rc_str(&spec.alias),
        })
    }

    fn decode_project_column(&self, col: &ProjectColumnWire) -> Result<ProjectColumn, String> {
        Ok(ProjectColumn {
            expr: self.expr(col.expr)?,
            alias: opt_rc_str(&col.alias),
        })
    }

    fn decode_property_assignment(
        &self,
        pa: &PropertyAssignmentWire,
    ) -> Result<PropertyAssignment, String> {
        Ok(PropertyAssignment {
            name: rc_str(&pa.name),
            value: self.expr(pa.value)?,
        })
    }

    fn decode_set_item(&self, item: &SetPlanItemWire) -> Result<SetPlanItem, String> {
        Ok(match item {
            SetPlanItemWire::Property {
                variable,
                property,
                value,
            } => SetPlanItem::Property {
                variable: rc_str(variable),
                property: rc_str(property),
                value: self.expr(*value)?,
            },
            SetPlanItemWire::AllProperties { variable, value } => SetPlanItem::AllProperties {
                variable: rc_str(variable),
                value: self.expr(*value)?,
            },
            SetPlanItemWire::Label { variable, label } => SetPlanItem::Label {
                variable: rc_str(variable),
                label: rc_str(label),
            },
        })
    }

    fn decode_wcoj_edge(&self, edge: &WcojEdgeWire) -> Result<WcojEdge, String> {
        Ok(WcojEdge {
            src: rc_str(&edge.src),
            dst: rc_str(&edge.dst),
            variable: rc_str(&edge.variable),
            label: opt_rc_str(&edge.label),
            label_expr: decode_opt_label_expr(self, edge.label_expr)?,
            direction: edge.direction,
            var_len: edge.var_len.map(var_len_from_wire),
            indexed_edge_equality: decode_indexed_edge_equality(&edge.indexed_edge_equality)?,
            dst_filter: edge
                .dst_filter
                .iter()
                .map(|id| self.expr(*id))
                .collect::<Result<_, _>>()?,
            hop_aux_binding: opt_rc_str(&edge.hop_aux_binding),
        })
    }
}

fn decode_opt_label_expr(dec: &Decoder<'_>, id: Option<u32>) -> Result<Option<LabelExpr>, String> {
    id.map(|i| dec.label_expr(i)).transpose()
}

pub(super) fn decode_shortest_path_cost(
    dec: &Decoder<'_>,
    cost: &super::types::ShortestPathCostWire,
) -> Result<ShortestPathCost, String> {
    use super::types::ShortestPathCostWire;
    Ok(match cost {
        ShortestPathCostWire::HopCount => ShortestPathCost::HopCount,
        ShortestPathCostWire::EdgeCostExpr { edge_var, expr } => ShortestPathCost::EdgeCostExpr {
            edge_var: rc_str(edge_var),
            expr: dec.expr(*expr)?,
        },
    })
}
