use gleaph_gql::ast::{AggregateFunc, CmpOp, SetOp};
use gleaph_gql::types::EdgeDirection;

use super::PhysicalPlanWire;

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
        edge_inline_value_predicate: Option<EdgeInlineValuePredicateWire>,
        edge_inline_vector_predicate: Option<EdgeInlineVectorPredicateWire>,
        edge_property_projection: Option<Vec<String>>,
        dst_property_projection: Option<Vec<String>>,
        hop_aux_binding: Option<String>,
        emit_edge_binding: bool,
        near_group_var: Option<String>,
        far_group_var: Option<String>,
        path_var: Option<String>,
        emit_path_binding: bool,
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
        edge_inline_value_predicate: Option<EdgeInlineValuePredicateWire>,
        edge_inline_vector_predicate: Option<EdgeInlineVectorPredicateWire>,
        dst_filter: Vec<u32>,
        edge_property_projection: Option<Vec<String>>,
        dst_property_projection: Option<Vec<String>>,
        hop_aux_binding: Option<String>,
        emit_edge_binding: bool,
        near_group_var: Option<String>,
        far_group_var: Option<String>,
        path_var: Option<String>,
        emit_path_binding: bool,
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
        offset_keyword: bool,
    },
    Filter {
        condition: u32,
    },
    Search {
        binding: String,
        provider: SearchProviderWire,
        output: SearchOutputWire,
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
        scope: InlineProcedureScopeWire,
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

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum SearchProviderWire {
    VectorIndex {
        index_name: Vec<String>,
        query: u32,
        limit: u32,
        filter: Option<u32>,
    },
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct SearchOutputWire {
    pub kind: SearchOutputKindWire,
    pub alias: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum SearchOutputKindWire {
    Score,
    Distance,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub enum InlineProcedureScopeWire {
    ImplicitAll,
    Explicit(Vec<String>),
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

#[derive(Clone, Debug, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct EdgeInlineValuePredicateWire {
    pub op: u8,
    pub value: ScanValueWire,
}

#[derive(Clone, Debug, rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
pub struct EdgeInlineVectorPredicateWire {
    pub metric: u8,
    pub query: ScanValueWire,
    pub op: u8,
    pub threshold: ScanValueWire,
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
    ShortestKGroup(u64),
}
