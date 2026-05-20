//! Physical plan ↔ wire conversion (`GPL` bundle statement payloads).

use std::rc::Rc;

use gleaph_gql::Value;
use gleaph_gql::ast::{AggregateFunc, CmpOp, Expr, LetBinding, OrderByClause, SetOp};
use gleaph_gql::token::Span;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use rkyv::rancor;

use crate::plan::{
    AggregateSpec, ConditionalScanCandidate, IndexScanSpec, PhysicalPlan, PlanAnnotations,
    PlanDiagnostics, PlanOp, ProjectColumn, PropertyAssignment, RemovePlanItem, ScanValue,
    SetPlanItem, ShortestMode, ShortestPathCost, Str, VarLenSpec, WcojEdge, YieldColumn,
};

// ════════════════════════════════════════════════════════════════════════════════
// Wire root
// ════════════════════════════════════════════════════════════════════════════════

/// Rkyv wire image of a [`PhysicalPlan`] (ops + interned expression pools).
#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(
    serialize_bounds(
        __S: rkyv::ser::Writer + rkyv::ser::Allocator,
        __S::Error: rkyv::rancor::Source,
    ),
    deserialize_bounds(__D::Error: rkyv::rancor::Source),
    bytecheck(bounds(
        __C: rkyv::validation::ArchiveContext,
        __C::Error: rkyv::rancor::Source,
    ))
)]
pub struct PhysicalPlanWire {
    #[rkyv(omit_bounds)]
    pub ops: Vec<PlanOpWire>,
    /// Rkyv [`Expr`] blobs (`gleaph-gql` `ast-rkyv-no-span`).
    pub expr_pool: Vec<Vec<u8>>,
    /// Rkyv [`LabelExpr`] blobs.
    pub label_expr_pool: Vec<Vec<u8>>,
    /// Rkyv [`OrderByClause`] blobs.
    pub order_by_pool: Vec<Vec<u8>>,
}

pub fn physical_plan_to_wire(plan: &PhysicalPlan) -> Result<PhysicalPlanWire, String> {
    let mut enc = Encoder::default();
    let ops = enc.encode_ops(&plan.ops)?;
    Ok(PhysicalPlanWire {
        ops,
        expr_pool: enc.expr_pool,
        label_expr_pool: enc.label_expr_pool,
        order_by_pool: enc.order_by_pool,
    })
}

pub fn physical_plan_from_wire(wire: &PhysicalPlanWire) -> Result<PhysicalPlan, String> {
    let dec = Decoder::new(wire);
    let ops = dec.decode_ops(&wire.ops)?;
    let output = crate::output_schema::derive_output_schema(&ops);
    Ok(PhysicalPlan {
        ops,
        diagnostics: PlanDiagnostics::default(),
        annotations: PlanAnnotations::default(),
        output,
    })
}

// ════════════════════════════════════════════════════════════════════════════════
// PlanOp wire
// ════════════════════════════════════════════════════════════════════════════════

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
#[rkyv(
    serialize_bounds(
        __S: rkyv::ser::Writer + rkyv::ser::Allocator,
        __S::Error: rkyv::rancor::Source,
    ),
    deserialize_bounds(__D::Error: rkyv::rancor::Source),
    bytecheck(bounds(
        __C: rkyv::validation::ArchiveContext,
        __C::Error: rkyv::rancor::Source,
    ))
)]
pub enum PlanOpWire {
    NodeScan {
        variable: String,
        label: Option<String>,
        property_projection: Option<Vec<String>>,
    },
    IndexScan {
        variable: String,
        property: String,
        value: ScanValueWire,
        cmp: CmpOp,
        property_projection: Option<Vec<String>>,
    },
    EdgeIndexScan {
        variable: String,
        property: String,
        value: ScanValueWire,
        property_projection: Option<Vec<String>>,
    },
    EdgeBindEndpoints {
        edge: String,
        near: String,
        far: String,
        direction: EdgeDirection,
        label: Option<String>,
        near_property_projection: Option<Vec<String>>,
        far_property_projection: Option<Vec<String>>,
        hop_aux_binding: Option<String>,
    },
    ConditionalIndexScan {
        candidates: Vec<ConditionalScanCandidateWire>,
        fallback_label: Option<String>,
        fallback_variable: String,
        property_projection: Option<Vec<String>>,
    },
    PropertyFilter {
        predicates: Vec<u32>,
        stage: usize,
    },
    Expand {
        src: String,
        edge: String,
        dst: String,
        direction: EdgeDirection,
        label: Option<String>,
        label_expr: Option<u32>,
        var_len: Option<VarLenSpecWire>,
        indexed_edge_equality: Option<(String, ScanValueWire)>,
        edge_property_projection: Option<Vec<String>>,
        dst_property_projection: Option<Vec<String>>,
        hop_aux_binding: Option<String>,
        emit_edge_binding: bool,
    },
    ExpandFilter {
        src: String,
        edge: String,
        dst: String,
        direction: EdgeDirection,
        label: Option<String>,
        label_expr: Option<u32>,
        var_len: Option<VarLenSpecWire>,
        indexed_edge_equality: Option<(String, ScanValueWire)>,
        dst_filter: Vec<u32>,
        edge_property_projection: Option<Vec<String>>,
        dst_property_projection: Option<Vec<String>>,
        hop_aux_binding: Option<String>,
        emit_edge_binding: bool,
    },
    ShortestPath {
        src: String,
        dst: String,
        edge: String,
        path_var: Option<String>,
        emit_edge_binding: bool,
        emit_path_binding: bool,
        mode: ShortestModeWire,
        direction: EdgeDirection,
        label: Option<String>,
        label_expr: Option<u32>,
        var_len: Option<VarLenSpecWire>,
        cost: ShortestPathCostWire,
    },
    Let {
        bindings: Vec<LetBindingWire>,
    },
    For {
        variable: String,
        list: u32,
        ordinality: Option<String>,
    },
    Filter {
        condition: u32,
    },
    CallProcedure {
        name: Vec<String>,
        args: Vec<u32>,
        yield_columns: Option<Vec<YieldColumnWire>>,
        optional: bool,
    },
    InlineProcedureCall {
        #[rkyv(omit_bounds)]
        sub_plan: Box<PhysicalPlanWire>,
        scope_vars: Vec<String>,
        optional: bool,
    },
    UseGraph {
        graph_name: Vec<String>,
        #[rkyv(omit_bounds)]
        sub_plan: Option<Vec<PlanOpWire>>,
    },
    HashJoin {
        #[rkyv(omit_bounds)]
        left: Vec<PlanOpWire>,
        #[rkyv(omit_bounds)]
        right: Vec<PlanOpWire>,
        join_keys: Vec<String>,
    },
    CartesianProduct {
        #[rkyv(omit_bounds)]
        left: Vec<PlanOpWire>,
        #[rkyv(omit_bounds)]
        right: Vec<PlanOpWire>,
    },
    Aggregate {
        group_by: Vec<u32>,
        aggregates: Vec<AggregateSpecWire>,
    },
    Project {
        columns: Vec<ProjectColumnWire>,
        distinct: bool,
    },
    Sort {
        order_by: u32,
    },
    Limit {
        count: Option<u32>,
        offset: Option<u32>,
    },
    SetOperation {
        op: SetOp,
        #[rkyv(omit_bounds)]
        right: Box<PhysicalPlanWire>,
    },
    OptionalMatch {
        #[rkyv(omit_bounds)]
        sub_plan: Vec<PlanOpWire>,
    },
    IndexIntersection {
        variable: String,
        scans: Vec<IndexScanSpecWire>,
        property_projection: Option<Vec<String>>,
    },
    WorstCaseOptimalJoin {
        variables: Vec<String>,
        edges: Vec<WcojEdgeWire>,
    },
    TopK {
        order_by: u32,
        k: u32,
        offset: Option<u32>,
    },
    Materialize {
        columns: Vec<ProjectColumnWire>,
        distinct: bool,
    },
    InsertVertex {
        variable: Option<String>,
        labels: Vec<String>,
        properties: Vec<PropertyAssignmentWire>,
    },
    InsertEdge {
        variable: Option<String>,
        src: String,
        dst: String,
        direction: EdgeDirection,
        labels: Vec<String>,
        properties: Vec<PropertyAssignmentWire>,
    },
    SetProperties {
        items: Vec<SetPlanItemWire>,
    },
    RemoveProperties {
        items: Vec<RemovePlanItemWire>,
    },
    DeleteVertex {
        variable: String,
    },
    DetachDeleteVertex {
        variable: String,
    },
    DeleteEdge {
        variable: String,
    },
}

// ════════════════════════════════════════════════════════════════════════════════
// Supporting wire types
// ════════════════════════════════════════════════════════════════════════════════

#[derive(Clone, Debug, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum ScanValueWire {
    /// Rkyv-encoded [`Value`].
    Literal(Vec<u8>),
    Parameter(String),
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct PropertyAssignmentWire {
    pub name: String,
    pub value: u32,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum SetPlanItemWire {
    Property {
        variable: String,
        property: String,
        value: u32,
    },
    AllProperties {
        variable: String,
        value: u32,
    },
    Label {
        variable: String,
        label: String,
    },
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum RemovePlanItemWire {
    Property { variable: String, property: String },
    Label { variable: String, label: String },
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct ProjectColumnWire {
    pub expr: u32,
    pub alias: Option<String>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct AggregateSpecWire {
    pub func: AggregateFunc,
    pub expr: Option<u32>,
    pub expr2: Option<u32>,
    pub distinct: bool,
    pub filter: Option<u32>,
    pub order_by: Option<u32>,
    pub alias: Option<String>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct ConditionalScanCandidateWire {
    pub param_name: String,
    pub property: String,
    pub variable: String,
    pub cmp: CmpOp,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct IndexScanSpecWire {
    pub property: String,
    pub value: ScanValueWire,
    pub cmp: CmpOp,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct WcojEdgeWire {
    pub src: String,
    pub dst: String,
    pub variable: String,
    pub label: Option<String>,
    pub label_expr: Option<u32>,
    pub direction: EdgeDirection,
    pub var_len: Option<VarLenSpecWire>,
    pub indexed_edge_equality: Option<(String, ScanValueWire)>,
    pub dst_filter: Vec<u32>,
    pub hop_aux_binding: Option<String>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct YieldColumnWire {
    pub name: String,
    pub alias: Option<String>,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum ShortestPathCostWire {
    HopCount,
    EdgeCostExpr { edge_var: String, expr: u32 },
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct LetBindingWire {
    pub variable: String,
    pub value: u32,
}

/// Wire image of [`VarLenSpec`] (copy type).
#[derive(Clone, Copy, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct VarLenSpecWire {
    pub min: u64,
    pub max: Option<u64>,
}

/// Wire image of [`ShortestMode`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum ShortestModeWire {
    AnyShortest,
    AllShortest,
    ShortestK(u64),
}

// ════════════════════════════════════════════════════════════════════════════════
// Rkyv pool helpers
// ════════════════════════════════════════════════════════════════════════════════

fn rkyv_encode_expr(value: &Expr) -> Result<Vec<u8>, String> {
    rkyv::to_bytes::<rancor::Error>(value)
        .map(|b| b.into_vec())
        .map_err(|e| e.to_string())
}

fn rkyv_decode_expr(bytes: &[u8]) -> Result<Expr, String> {
    rkyv::from_bytes::<Expr, rancor::Error>(bytes).map_err(|e| e.to_string())
}

fn rkyv_encode_label_expr(value: &LabelExpr) -> Result<Vec<u8>, String> {
    rkyv::to_bytes::<rancor::Error>(value)
        .map(|b| b.into_vec())
        .map_err(|e| e.to_string())
}

fn rkyv_decode_label_expr(bytes: &[u8]) -> Result<LabelExpr, String> {
    rkyv::from_bytes::<LabelExpr, rancor::Error>(bytes).map_err(|e| e.to_string())
}

fn rkyv_encode_order_by(value: &OrderByClause) -> Result<Vec<u8>, String> {
    rkyv::to_bytes::<rancor::Error>(value)
        .map(|b| b.into_vec())
        .map_err(|e| e.to_string())
}

fn rkyv_decode_order_by(bytes: &[u8]) -> Result<OrderByClause, String> {
    rkyv::from_bytes::<OrderByClause, rancor::Error>(bytes).map_err(|e| e.to_string())
}

// ════════════════════════════════════════════════════════════════════════════════
// Encoder
// ════════════════════════════════════════════════════════════════════════════════

#[derive(Default)]
struct Encoder {
    expr_pool: Vec<Vec<u8>>,
    label_expr_pool: Vec<Vec<u8>>,
    order_by_pool: Vec<Vec<u8>>,
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

    fn encode_ops(&mut self, ops: &[PlanOp]) -> Result<Vec<PlanOpWire>, String> {
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

// ════════════════════════════════════════════════════════════════════════════════
// Decoder
// ════════════════════════════════════════════════════════════════════════════════

struct Decoder<'a> {
    wire: &'a PhysicalPlanWire,
}

impl<'a> Decoder<'a> {
    fn new(wire: &'a PhysicalPlanWire) -> Self {
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

    fn decode_ops(&self, ops: &[PlanOpWire]) -> Result<Vec<PlanOp>, String> {
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

// ════════════════════════════════════════════════════════════════════════════════
// Small conversions
// ════════════════════════════════════════════════════════════════════════════════

fn rc_str(s: &str) -> Str {
    s.into()
}

fn opt_str_opt(s: &Option<Str>) -> Option<String> {
    s.as_ref().map(|x| x.to_string())
}

fn opt_rc_str(s: &Option<String>) -> Option<Str> {
    s.as_ref().map(|x| x.as_str().into())
}

fn vec_str(v: &[Str]) -> Vec<String> {
    v.iter().map(|s| s.to_string()).collect()
}

fn vec_rc_str(v: &[String]) -> Vec<Str> {
    v.iter().map(|s| s.as_str().into()).collect()
}

fn opt_str_slice(s: &Option<Rc<[Str]>>) -> Option<Vec<String>> {
    s.as_ref()
        .map(|rc| rc.iter().map(|x| x.to_string()).collect())
}

fn decode_str_slice(s: &Option<Vec<String>>) -> Option<Rc<[Str]>> {
    s.as_ref().map(|names| {
        names
            .iter()
            .map(|n| n.as_str().into())
            .collect::<Vec<Str>>()
            .into()
    })
}

fn encode_scan_value(v: &ScanValue) -> Result<ScanValueWire, String> {
    Ok(match v {
        ScanValue::Literal(lit) => ScanValueWire::Literal(rkyv_encode_value(lit)?),
        ScanValue::Parameter(p) => ScanValueWire::Parameter(p.to_string()),
    })
}

fn decode_scan_value(v: &ScanValueWire) -> Result<ScanValue, String> {
    Ok(match v {
        ScanValueWire::Literal(bytes) => ScanValue::Literal(rkyv_decode_value(bytes)?),
        ScanValueWire::Parameter(p) => ScanValue::Parameter(p.as_str().into()),
    })
}

fn rkyv_encode_value(value: &Value) -> Result<Vec<u8>, String> {
    rkyv::to_bytes::<rancor::Error>(value)
        .map(|b| b.into_vec())
        .map_err(|e| e.to_string())
}

fn rkyv_decode_value(bytes: &[u8]) -> Result<Value, String> {
    rkyv::from_bytes::<Value, rancor::Error>(bytes).map_err(|e| e.to_string())
}

fn encode_indexed_edge_equality(
    eq: &Option<(Str, ScanValue)>,
) -> Result<Option<(String, ScanValueWire)>, String> {
    match eq {
        None => Ok(None),
        Some((prop, val)) => Ok(Some((prop.to_string(), encode_scan_value(val)?))),
    }
}

fn decode_indexed_edge_equality(
    eq: &Option<(String, ScanValueWire)>,
) -> Result<Option<(Str, ScanValue)>, String> {
    match eq {
        None => Ok(None),
        Some((prop, val)) => Ok(Some((prop.as_str().into(), decode_scan_value(val)?))),
    }
}

fn encode_conditional_candidate(c: &ConditionalScanCandidate) -> ConditionalScanCandidateWire {
    ConditionalScanCandidateWire {
        param_name: c.param_name.to_string(),
        property: c.property.to_string(),
        variable: c.variable.to_string(),
        cmp: c.cmp,
    }
}

fn decode_conditional_candidate(c: &ConditionalScanCandidateWire) -> ConditionalScanCandidate {
    ConditionalScanCandidate {
        param_name: c.param_name.as_str().into(),
        property: c.property.as_str().into(),
        variable: c.variable.as_str().into(),
        cmp: c.cmp,
    }
}

fn encode_index_scan_spec(s: &IndexScanSpec) -> Result<IndexScanSpecWire, String> {
    Ok(IndexScanSpecWire {
        property: s.property.to_string(),
        value: encode_scan_value(&s.value)?,
        cmp: s.cmp,
    })
}

fn decode_index_scan_spec(s: &IndexScanSpecWire) -> Result<IndexScanSpec, String> {
    Ok(IndexScanSpec {
        property: s.property.as_str().into(),
        value: decode_scan_value(&s.value)?,
        cmp: s.cmp,
    })
}

fn encode_yield_column(c: &YieldColumn) -> YieldColumnWire {
    YieldColumnWire {
        name: c.name.to_string(),
        alias: opt_str_opt(&c.alias),
    }
}

fn decode_yield_column(c: &YieldColumnWire) -> YieldColumn {
    YieldColumn {
        name: c.name.as_str().into(),
        alias: opt_rc_str(&c.alias),
    }
}

fn encode_remove_item(item: &RemovePlanItem) -> RemovePlanItemWire {
    match item {
        RemovePlanItem::Property { variable, property } => RemovePlanItemWire::Property {
            variable: variable.to_string(),
            property: property.to_string(),
        },
        RemovePlanItem::Label { variable, label } => RemovePlanItemWire::Label {
            variable: variable.to_string(),
            label: label.to_string(),
        },
    }
}

fn decode_remove_item(item: &RemovePlanItemWire) -> RemovePlanItem {
    match item {
        RemovePlanItemWire::Property { variable, property } => RemovePlanItem::Property {
            variable: rc_str(variable),
            property: rc_str(property),
        },
        RemovePlanItemWire::Label { variable, label } => RemovePlanItem::Label {
            variable: rc_str(variable),
            label: rc_str(label),
        },
    }
}

fn opt_expr_id(enc: &mut Encoder, expr: Option<&Expr>) -> Result<Option<u32>, String> {
    expr.map(|e| enc.intern_expr(e)).transpose()
}

fn opt_label_expr_id(enc: &mut Encoder, expr: Option<&LabelExpr>) -> Result<Option<u32>, String> {
    expr.map(|e| enc.intern_label_expr(e)).transpose()
}

fn encode_shortest_path_cost(
    enc: &mut Encoder,
    cost: &ShortestPathCost,
) -> Result<ShortestPathCostWire, String> {
    Ok(match cost {
        ShortestPathCost::HopCount => ShortestPathCostWire::HopCount,
        ShortestPathCost::EdgeCostExpr { edge_var, expr } => ShortestPathCostWire::EdgeCostExpr {
            edge_var: edge_var.to_string(),
            expr: enc.intern_expr(expr)?,
        },
    })
}

fn decode_shortest_path_cost(
    dec: &Decoder<'_>,
    cost: &ShortestPathCostWire,
) -> Result<ShortestPathCost, String> {
    Ok(match cost {
        ShortestPathCostWire::HopCount => ShortestPathCost::HopCount,
        ShortestPathCostWire::EdgeCostExpr { edge_var, expr } => ShortestPathCost::EdgeCostExpr {
            edge_var: rc_str(edge_var),
            expr: dec.expr(*expr)?,
        },
    })
}

fn decode_opt_label_expr(dec: &Decoder<'_>, id: Option<u32>) -> Result<Option<LabelExpr>, String> {
    id.map(|i| dec.label_expr(i)).transpose()
}

fn var_len_to_wire(v: VarLenSpec) -> VarLenSpecWire {
    VarLenSpecWire {
        min: v.min,
        max: v.max,
    }
}

fn var_len_from_wire(v: VarLenSpecWire) -> VarLenSpec {
    VarLenSpec {
        min: v.min,
        max: v.max,
    }
}

fn shortest_mode_to_wire(m: ShortestMode) -> ShortestModeWire {
    match m {
        ShortestMode::AnyShortest => ShortestModeWire::AnyShortest,
        ShortestMode::AllShortest => ShortestModeWire::AllShortest,
        ShortestMode::ShortestK(k) => ShortestModeWire::ShortestK(k),
    }
}

fn shortest_mode_from_wire(m: ShortestModeWire) -> ShortestMode {
    match m {
        ShortestModeWire::AnyShortest => ShortestMode::AnyShortest,
        ShortestModeWire::AllShortest => ShortestMode::AllShortest,
        ShortestModeWire::ShortestK(k) => ShortestMode::ShortestK(k),
    }
}
