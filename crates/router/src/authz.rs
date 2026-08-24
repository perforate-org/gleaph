//! Plan-time data-plane enforcement (ADR 0074 slice 2b).
//!
//! Walks the built [`PhysicalPlan`] of one resolved graph on the Router and extracts the
//! exact data-plane privilege rows the plan demands ([`RequirementSet`]), then evaluates
//! those rows against the caller's effective privileges (`caller ∪ PUBLIC`, expiry-aware;
//! ownership through the implicit-root coverage arm, ADR 0074 §3 invariant 3). Any
//! uncovered demand fails the whole query with the uniform non-disclosing
//! [`RouterError::Forbidden`] that never names the missing privilege or resource.
//!
//! Boundary rules:
//! - `gleaph-gql` / `gleaph-gql-planner` stay generic; this module lives in the Router and
//!   consumes only their public types. Catalog ids are resolved through the graph-scoped
//!   catalogs ([`RouterStore`]) at extraction time.
//! - Every [`PlanOp`] variant is matched exhaustively with no wildcard op arm, so a future
//!   planner variant fails compilation here instead of silently bypassing enforcement.
//! - Administrative capabilities are not an input to evaluation anywhere in this module
//!   (ADR 0074 invariant 1: authority never implies data access).
//! - The extracted [`RequirementSet`] is also the durable artifact of ADR 0074 slice 3: it
//!   is embedded in the prepared-query record at registration, gates invariant-7-bounded
//!   publication, and is the primary checked set at prepared execution (the live walk
//!   remains the fallback). It is therefore `pub` and serializable while this module stays
//!   its single owner.
//!
//! Phase-1 attribution contract (fail closed where the plan cannot name a resource):
//! - Labeled scans demand `MATCH` on the vertex label; a retained full property map
//!   (`property_projection: None`) additionally demands `READ` on that label; an explicit
//!   non-empty projection demands `READ_PROPERTY` per listed key; an empty projection
//!   demands nothing beyond `MATCH`.
//! - Traversal demands derive from pattern direction × schema directedness: declared
//!   directed labels probe one directional row per orientation (undirected patterns over a
//!   directed label need both), while undirected or undeclared labels take the unoriented
//!   row — mirroring how GRANT lowers rows (`gql_grants::lower_privileges`). Edge-inline
//!   property bytes ride the edge-label traversal row; dedicated edge-property resources
//!   do not exist in Phase 1.
//! - Reads that cannot be attributed to exactly one label (unlabeled scans, ambiguous
//!   variables, unresolvable open-schema names, `NOT`/wildcard label expressions) mark the
//!   demand set tenancy-only: owners and admins proceed, everyone else is denied. Phase 1
//!   grants enumerate labels, so a wildcard read is not expressible as a grant.

use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;

use candid::{CandidType, Principal};
use gleaph_auth::{
    Direction, GrantSubject, GraphOperation, GraphPrivilege, GraphResource, Privilege,
};
use gleaph_gql::ast::{Expr, ExprKind};
use gleaph_gql::type_check::PropertySchema as _;
use gleaph_gql::types::{EdgeDirection, LabelExpr};
use gleaph_gql_planner::expr_children::for_each_immediate_child_expr;
use gleaph_gql_planner::plan::{
    EdgeLabelRef, NodeLabelRef, PhysicalPlan, PlanOp, RemovePlanItem, SearchProviderPlan,
    SetPlanItem, ShortestPathCost,
};
use gleaph_graph_kernel::entry::GraphId;

use crate::facade::stable::graph_catalog;
use crate::facade::store::RouterStore;
use crate::state::RouterError;

// ──── Catalog resolution ────

/// Graph-scoped name → catalog-id resolution used while extracting requirements.
///
/// Abstracted so host unit tests can drive the walker without Router stable state.
pub(crate) trait PlanCatalogView {
    fn graph_by_name(&self, name: &str) -> Option<GraphId>;
    fn vertex_label(&self, graph: GraphId, name: &str) -> Option<u32>;
    /// `(label id, declared UNDIRECTED)`; `None` for the boolean when the graph type
    /// schema does not declare the label's directedness (open-schema graphs).
    fn edge_label(&self, graph: GraphId, name: &str) -> Option<(u32, Option<bool>)>;
    fn property(&self, graph: GraphId, name: &str) -> Option<u32>;
    /// Source facts of a registered vector index (ADR 0078 §1): the creation-fixed label
    /// set it spans and the property id of its embedding source when that embedding name is
    /// backed by a projected graph property. `None` for an unregistered index name.
    fn vector_index_source(&self, graph: GraphId, index_name: &str) -> Option<VectorIndexSource>;
}

/// Vector-index search source facts for layer-1 requirement extraction (ADR 0078 §1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct VectorIndexSource {
    /// Creation-fixed label set the index spans, as `(label id, label name)` pairs so the
    /// walker can both demand rows on the ids and record the names as scope label facts.
    pub(crate) labels: Vec<(u32, String)>,
    /// Property id of the projected embedding source property (`CREATE VECTOR INDEX … ON
    /// d.embedding` interns the embedding under the property identifier). `None` for an
    /// ingestion-fed embedding field that is not a graph property — no value read exists
    /// to demand in that case.
    pub(crate) embedding_property_id: Option<u32>,
}

/// Production view over the Router graph-scoped catalogs.
struct RouterCatalogView<'a> {
    store: &'a RouterStore,
}

impl PlanCatalogView for RouterCatalogView<'_> {
    fn graph_by_name(&self, name: &str) -> Option<GraphId> {
        graph_catalog::lookup_graph_id(name)
    }

    fn vertex_label(&self, graph: GraphId, name: &str) -> Option<u32> {
        self.store
            .lookup_vertex_label_id(graph, name)
            .ok()
            .map(|id| u32::from(id.raw()))
    }

    fn edge_label(&self, graph: GraphId, name: &str) -> Option<(u32, Option<bool>)> {
        let id = self.store.lookup_edge_label_id(graph, name).ok()?;
        let undirected =
            crate::facade::stable::graph_type_catalog::try_property_schema_for_graph_id(graph)
                .ok()
                .flatten()
                .and_then(|schema| schema.edge_is_undirected(name));
        Some((u32::from(id.raw()), undirected))
    }

    fn property(&self, graph: GraphId, name: &str) -> Option<u32> {
        self.store
            .lookup_property_id(graph, name)
            .ok()
            .map(|id| id.raw())
    }

    fn vector_index_source(&self, graph: GraphId, index_name: &str) -> Option<VectorIndexSource> {
        let index_name_id =
            crate::facade::stable::index_name_catalog::lookup_index_name_id(graph, index_name)?;
        let def = crate::facade::stable::vector_index_catalog::get_vector_index_by_name_id(
            graph,
            index_name_id,
        )?;
        // The embedding source property exists only when the embedding name is the
        // projected property identifier of a semantic `CREATE VECTOR INDEX`; legacy
        // ingestion-fed field names resolve to no property and demand no value read.
        let embedding_property_id = crate::facade::stable::embedding_name_catalog::embedding_name(
            graph,
            def.embedding_name_id,
        )
        .and_then(|name| self.property(graph, &name));
        let labels = def
            .labels
            .iter()
            .filter_map(|label_id| {
                self.store
                    .reverse_vertex_label_name(graph, *label_id)
                    .ok()
                    .map(|name| (u32::from(label_id.raw()), name))
            })
            .collect();
        Some(VectorIndexSource {
            labels,
            embedding_property_id,
        })
    }
}

// ──── Requirement model ────

/// One conjunctive group of rows. Inside [`GraphDemands::alternatives`] the groups are
/// OR-alternatives (e.g. `-[:A|B]->`): satisfying any one group satisfies the demand.
type RowGroup = Vec<Privilege>;

/// One graph's extracted data-plane demands.
#[derive(
    Default, Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize,
)]
struct GraphDemands {
    /// Rows that must ALL be covered.
    conjunctive: Vec<Privilege>,
    /// Alternation groups: at least one fully covered group admits the demand.
    alternatives: Vec<RowGroup>,
    /// Set when some read could not be attributed to exactly one grantable resource; only
    /// implicit-root (registry) coverage admits such a plan (Phase 1 grants enumerate labels).
    unattributed: bool,
}

/// Every data-plane demand of one plan, grouped by target graph.
///
/// Single representation of a plan's static authorization requirements (ADR 0074 §4):
/// produced by [`extract`], embedded in the prepared-query record at registration
/// (slice 3), evaluated by [`authorize_requirements`] at enforcement and publication time.
#[derive(
    Default, Clone, Debug, PartialEq, Eq, CandidType, serde::Serialize, serde::Deserialize,
)]
pub struct RequirementSet {
    graphs: BTreeMap<u32, GraphDemands>,
    /// Set when a demand cannot even be pinned to a graph (unresolvable `USE GRAPH`
    /// target); admitted only through ownership on the executing root graph.
    root_unattributed: bool,
}

impl RequirementSet {
    fn demands(&mut self, graph: GraphId) -> &mut GraphDemands {
        self.graphs.entry(graph.raw()).or_default()
    }

    fn require(&mut self, graph: GraphId, privilege: Privilege) {
        self.demands(graph).conjunctive.push(privilege);
    }

    fn require_all(&mut self, graph: GraphId, privileges: RowGroup) {
        self.demands(graph).conjunctive.extend(privileges);
    }

    fn require_alternatives(&mut self, graph: GraphId, groups: Vec<RowGroup>) {
        self.demands(graph).alternatives.extend(groups);
    }

    fn require_unattributed(&mut self, graph: GraphId) {
        self.demands(graph).unattributed = true;
    }
}

fn vertex_row(graph: u32, operation: GraphOperation, label_id: u32) -> Privilege {
    Privilege::Graph(GraphPrivilege {
        graph,
        operation,
        resource: GraphResource::VertexLabel(label_id),
    })
}

fn vertex_property_row(graph: u32, label_id: u32, property_id: u32) -> Privilege {
    Privilege::Graph(GraphPrivilege {
        graph,
        operation: GraphOperation::ReadProperty,
        resource: GraphResource::VertexProperty {
            label: label_id,
            property: property_id,
        },
    })
}

fn edge_row(graph: u32, operation: GraphOperation, label_id: u32) -> Privilege {
    Privilege::Graph(GraphPrivilege {
        graph,
        operation,
        resource: GraphResource::EdgeLabel(label_id),
    })
}

// ──── Extraction ────

/// Per-scope variable label facts accumulated while walking one op list.
struct Scope {
    graph: GraphId,
    /// Vertex variable → label-name set seen at binding or `IS LABELED` sites.
    var_labels: BTreeMap<Rc<str>, BTreeSet<String>>,
    /// Edge variable → edge-label-name set seen at binding sites.
    edge_labels: BTreeMap<Rc<str>, BTreeSet<String>>,
    /// Reads deferred to scope end, when label facts are complete.
    pending: Vec<PendingRead>,
}

enum PendingRead {
    /// A vertex-property value read through `variable`.
    VertexProperty { variable: Rc<str>, property: String },
    /// An edge-inline-property read through the edge `variable`; covered by the edge
    /// label's traversal row once that label is known.
    EdgeProperty { variable: Rc<str> },
}

impl Scope {
    fn new(graph: GraphId) -> Self {
        Self {
            graph,
            var_labels: BTreeMap::new(),
            edge_labels: BTreeMap::new(),
            pending: Vec::new(),
        }
    }

    fn note_vertex_label(&mut self, variable: &str, label: &str) {
        self.var_labels
            .entry(Rc::from(variable))
            .or_default()
            .insert(label.to_owned());
    }

    fn note_edge_label(&mut self, variable: &str, label: &str) {
        self.edge_labels
            .entry(Rc::from(variable))
            .or_default()
            .insert(label.to_owned());
    }

    /// The single label name of a vertex variable, or `None` when unknown or ambiguous
    /// (both fail closed at resolution).
    fn unique_vertex_label(&self, variable: &str) -> Option<&String> {
        match self.var_labels.get(variable) {
            Some(labels) if labels.len() == 1 => labels.iter().next(),
            _ => None,
        }
    }

    fn unique_edge_label(&self, variable: &str) -> Option<&String> {
        match self.edge_labels.get(variable) {
            Some(labels) if labels.len() == 1 => labels.iter().next(),
            _ => None,
        }
    }

    fn defer_vertex_property(&mut self, variable: &str, property: &str) {
        self.pending.push(PendingRead::VertexProperty {
            variable: Rc::from(variable),
            property: property.to_owned(),
        });
    }

    fn defer_edge_property(&mut self, variable: &str) {
        self.pending.push(PendingRead::EdgeProperty {
            variable: Rc::from(variable),
        });
    }
}

/// Extract the complete requirement set of `plan` rooted at `root_graph`.
pub(crate) fn extract(
    plan: &PhysicalPlan,
    root_graph: GraphId,
    catalog: &dyn PlanCatalogView,
) -> RequirementSet {
    let mut reqs = RequirementSet::default();
    let mut scope = Scope::new(root_graph);
    walk_ops(&plan.ops, &mut scope, &mut reqs, catalog);
    resolve_pending(&mut scope, &mut reqs, catalog);
    reqs
}

fn walk_ops(
    ops: &[PlanOp],
    scope: &mut Scope,
    reqs: &mut RequirementSet,
    catalog: &dyn PlanCatalogView,
) {
    for op in ops {
        walk_op(op, scope, reqs, catalog);
    }
}

/// Canonical grant rows demanded by traversing one declared edge label.
///
/// Mirrors grant lowering (`gql_grants::lower_privileges`): undirected or undeclared
/// labels carry only the unoriented row; declared directed labels carry directional rows.
fn traversal_rows(
    graph: u32,
    label_id: u32,
    undirected: Option<bool>,
    direction: EdgeDirection,
) -> RowGroup {
    let row = |direction: Option<Direction>| {
        edge_row(graph, GraphOperation::Traverse(direction), label_id)
    };
    match undirected {
        Some(true) | None => vec![row(None)],
        Some(false) => match direction_demand(direction) {
            DirectionDemand::Outgoing => vec![row(Some(Direction::Outgoing))],
            DirectionDemand::Incoming => vec![row(Some(Direction::Incoming))],
            DirectionDemand::Both => vec![
                row(Some(Direction::Outgoing)),
                row(Some(Direction::Incoming)),
            ],
        },
    }
}

/// Directional modifier requirement of one pattern direction. Undirected shapes over a
/// directed label require BOTH rows (ADR 0074 §2).
enum DirectionDemand {
    Outgoing,
    Incoming,
    Both,
}

fn direction_demand(direction: EdgeDirection) -> DirectionDemand {
    match direction {
        EdgeDirection::PointingRight => DirectionDemand::Outgoing,
        EdgeDirection::PointingLeft => DirectionDemand::Incoming,
        EdgeDirection::LeftOrRight
        | EdgeDirection::Undirected
        | EdgeDirection::LeftOrUndirected
        | EdgeDirection::UndirectedOrRight
        | EdgeDirection::AnyDirection => DirectionDemand::Both,
    }
}

/// Demand the traversal rows for one fixed edge-label reference.
fn require_edge_traversal_for_label(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    label: &EdgeLabelRef,
    direction: EdgeDirection,
) {
    let Some((id, undirected)) = catalog.edge_label(graph, &label.name) else {
        reqs.require_unattributed(graph);
        return;
    };
    reqs.require_all(
        graph,
        traversal_rows(graph.raw(), id, undirected, direction),
    );
}

/// Demand the traversal rows for a general edge label expression: OR-alternatives where
/// decomposable, tenancy-only otherwise (`NOT` / wildcard labels are not grantable).
fn require_edge_traversal_for_expr(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    label_expr: &LabelExpr,
    direction: EdgeDirection,
) {
    match traversal_alternatives(label_expr, graph, catalog, direction) {
        Some(groups) => reqs.require_alternatives(graph, groups),
        None => reqs.require_unattributed(graph),
    }
}

/// Demand traversal rows for one hop given the plan's optional fixed label and optional
/// general label predicate. Neither being present is an unattributable wildcard traversal.
fn require_traversal(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    label: Option<&EdgeLabelRef>,
    label_expr: Option<&LabelExpr>,
    direction: EdgeDirection,
) {
    match (label, label_expr) {
        (Some(label), _) => {
            require_edge_traversal_for_label(reqs, graph, catalog, label, direction)
        }
        (None, Some(label_expr)) => {
            require_edge_traversal_for_expr(reqs, graph, catalog, label_expr, direction);
        }
        (None, None) => reqs.require_unattributed(graph),
    }
}

/// OR-alternative traversal groups for a label expression; `None` when the expression
/// cannot be decomposed into positive label names (`NOT`, `%`).
fn traversal_alternatives(
    expr: &LabelExpr,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    direction: EdgeDirection,
) -> Option<Vec<RowGroup>> {
    match expr {
        LabelExpr::Name(name) => {
            let (id, undirected) = catalog.edge_label(graph, name)?;
            Some(vec![traversal_rows(graph.raw(), id, undirected, direction)])
        }
        LabelExpr::Or(left, right) => {
            let mut left = traversal_alternatives(left, graph, catalog, direction)?;
            let right = traversal_alternatives(right, graph, catalog, direction)?;
            left.extend(right);
            Some(left)
        }
        LabelExpr::And(left, right) => {
            let left = traversal_alternatives(left, graph, catalog, direction)?;
            let right = traversal_alternatives(right, graph, catalog, direction)?;
            let mut combined = Vec::with_capacity(left.len() * right.len());
            for l in &left {
                for r in &right {
                    let mut group = l.clone();
                    group.extend(r.iter().cloned());
                    combined.push(group);
                }
            }
            Some(combined)
        }
        LabelExpr::Not(_) | LabelExpr::Wildcard => None,
    }
}

/// Demand the scan-side rows for a vertex scan: `MATCH`, plus value-read coverage for the
/// retained property map (`READ`) or each explicitly projected key (`READ_PROPERTY`).
fn require_vertex_scan_rows(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    label: Option<&NodeLabelRef>,
    property_projection: Option<&[Rc<str>]>,
) {
    let Some(label) = label else {
        // Unlabeled scans enumerate every vertex; Phase 1 grants enumerate labels, so
        // only registry-tenant coverage admits them.
        reqs.require_unattributed(graph);
        return;
    };
    let Some(label_id) = catalog.vertex_label(graph, &label.name) else {
        reqs.require_unattributed(graph);
        return;
    };
    let graph_raw = graph.raw();
    reqs.require(
        graph,
        vertex_row(graph_raw, GraphOperation::Match, label_id),
    );
    match property_projection {
        None => reqs.require(graph, vertex_row(graph_raw, GraphOperation::Read, label_id)),
        Some([]) => {}
        Some(projected) => {
            for name in projected {
                let Some(property_id) = catalog.property(graph, name) else {
                    reqs.require_unattributed(graph);
                    continue;
                };
                reqs.require(graph, vertex_property_row(graph_raw, label_id, property_id));
            }
        }
    }
}

/// Demand the scan-side rows for a vertex search (ADR 0078 §1 layer 1): `MATCH` on every
/// label the vector index spans plus value-read coverage of the projected embedding source
/// property per label. The label rows are conjunctive — per-candidate visibility has no
/// row-level grant filter, so a caller must be able to read the whole spanned label set,
/// not just one of its labels. Label names are recorded as scope facts for the searched
/// binding so downstream deferred reads attribute like an equivalent labeled scan; a
/// multi-label span leaves those reads ambiguous (fail closed), and an unknown index name
/// is unattributed.
fn require_vector_search_rows(
    reqs: &mut RequirementSet,
    scope: &mut Scope,
    catalog: &dyn PlanCatalogView,
    binding: &str,
    index_name: &str,
) {
    let Some(source) = catalog.vector_index_source(scope.graph, index_name) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    let graph_raw = scope.graph.raw();
    for (label_id, label_name) in &source.labels {
        scope.note_vertex_label(binding, label_name);
        reqs.require(
            scope.graph,
            vertex_row(graph_raw, GraphOperation::Match, *label_id),
        );
        if let Some(property_id) = source.embedding_property_id {
            reqs.require(
                scope.graph,
                vertex_property_row(graph_raw, *label_id, property_id),
            );
        }
    }
}

/// Demand one mutation row on a labeled vertex; open-schema names that stopped resolving
/// degrade to tenancy-only coverage.
fn require_vertex_mutation(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    label: &NodeLabelRef,
    operation: GraphOperation,
) {
    let Some(label_id) = catalog.vertex_label(graph, &label.name) else {
        reqs.require_unattributed(graph);
        return;
    };
    reqs.require(graph, vertex_row(graph.raw(), operation, label_id));
}

/// `CREATE` rows for freshly inserted vertices.
fn require_create_labels(
    reqs: &mut RequirementSet,
    graph: GraphId,
    catalog: &dyn PlanCatalogView,
    labels: &[NodeLabelRef],
) {
    for label in labels {
        require_vertex_mutation(reqs, graph, catalog, label, GraphOperation::Create);
    }
}

/// Attribute a mutation on a bound variable to its single known vertex label, falling back
/// to tenancy-only when the variable is unlabeled or multi-labeled.
fn require_mutation_on_variable(
    reqs: &mut RequirementSet,
    scope: &Scope,
    catalog: &dyn PlanCatalogView,
    variable: &str,
    operation: GraphOperation,
) {
    let Some(label_name) = scope.unique_vertex_label(variable) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    let Some(label_id) = catalog.vertex_label(scope.graph, label_name) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    reqs.require(
        scope.graph,
        vertex_row(scope.graph.raw(), operation, label_id),
    );
}

/// Attribute a deletion of an edge variable to its single known edge label.
fn require_edge_deletion_on_variable(
    reqs: &mut RequirementSet,
    scope: &Scope,
    catalog: &dyn PlanCatalogView,
    variable: &str,
) {
    let Some(label_name) = scope.unique_edge_label(variable) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    let Some((id, _)) = catalog.edge_label(scope.graph, label_name) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    reqs.require(
        scope.graph,
        edge_row(scope.graph.raw(), GraphOperation::Delete, id),
    );
}

/// Record an expand hop's destination binding and deferred destination projections.
fn note_expand_dst(scope: &mut Scope, dst: &str, dst_property_projection: Option<&[Rc<str>]>) {
    for name in dst_property_projection.iter().flat_map(|p| p.iter()) {
        scope.defer_vertex_property(dst, name);
    }
}

/// Note the traversed edge variable's label when the executor binds it.
fn note_bound_edge(
    scope: &mut Scope,
    edge: &str,
    label: Option<&EdgeLabelRef>,
    emit_edge_binding: bool,
) {
    if emit_edge_binding && let Some(label) = label {
        scope.note_edge_label(edge, &label.name);
    }
}

fn walk_op(
    op: &PlanOp,
    scope: &mut Scope,
    reqs: &mut RequirementSet,
    catalog: &dyn PlanCatalogView,
) {
    let graph = scope.graph;
    match op {
        // ──── Scan ────
        PlanOp::NodeScan {
            variable,
            label,
            property_projection,
        } => {
            require_vertex_scan_rows(
                reqs,
                graph,
                catalog,
                label.as_ref(),
                property_projection.as_deref(),
            );
            if let Some(label) = label {
                scope.note_vertex_label(variable, &label.name);
            }
        }

        PlanOp::IndexScan {
            variable,
            property,
            property_projection,
            ..
        } => {
            // The equality/range probe reads one property value of the scanned variable;
            // its label fact arrives later (post-anchor filters or scans).
            scope.defer_vertex_property(variable, property);
            for name in property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_vertex_property(variable, name);
            }
        }

        PlanOp::EdgeIndexScan {
            variable,
            property,
            property_projection,
            ..
        } => {
            scope.defer_edge_property(property);
            for name in property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_edge_property(name);
            }
            let _ = variable;
        }

        PlanOp::EdgeBindEndpoints {
            edge,
            near,
            far,
            direction,
            label,
            near_property_projection,
            far_property_projection,
            ..
        } => {
            require_traversal(reqs, graph, catalog, label.as_ref(), None, *direction);
            if let Some(label) = label {
                scope.note_edge_label(edge, &label.name);
            }
            for (variable, projection) in [
                (near, near_property_projection),
                (far, far_property_projection),
            ] {
                for name in projection.iter().flat_map(|p| p.iter()) {
                    scope.defer_vertex_property(variable, name);
                }
            }
        }

        PlanOp::ConditionalIndexScan {
            candidates,
            fallback_label,
            fallback_variable,
            property_projection,
        } => {
            for candidate in candidates {
                scope.defer_vertex_property(&candidate.variable, &candidate.property);
            }
            if let Some(label) = fallback_label {
                scope.note_vertex_label(fallback_variable, &label.name);
            }
            for name in property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_vertex_property(fallback_variable, name);
            }
        }

        // ──── Filter ────
        PlanOp::PropertyFilter { predicates, .. } => {
            for predicate in predicates {
                collect_expr_reads(predicate, scope);
            }
        }

        // ──── Traversal ────
        PlanOp::Expand {
            dst,
            edge,
            direction,
            label,
            label_expr,
            edge_property_projection,
            dst_property_projection,
            emit_edge_binding,
            ..
        } => {
            require_traversal(
                reqs,
                graph,
                catalog,
                label.as_ref(),
                label_expr.as_ref(),
                *direction,
            );
            note_bound_edge(scope, edge, label.as_ref(), *emit_edge_binding);
            for name in edge_property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_edge_property(edge);
                let _ = name;
            }
            note_expand_dst(scope, dst, dst_property_projection.as_deref());
        }

        PlanOp::ExpandFilter {
            dst,
            edge,
            direction,
            label,
            label_expr,
            dst_filter,
            edge_property_projection,
            dst_property_projection,
            emit_edge_binding,
            ..
        } => {
            require_traversal(
                reqs,
                graph,
                catalog,
                label.as_ref(),
                label_expr.as_ref(),
                *direction,
            );
            note_bound_edge(scope, edge, label.as_ref(), *emit_edge_binding);
            for name in edge_property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_edge_property(edge);
                let _ = name;
            }
            for predicate in dst_filter {
                collect_expr_reads(predicate, scope);
            }
            note_expand_dst(scope, dst, dst_property_projection.as_deref());
        }

        PlanOp::ShortestPath {
            edge,
            emit_edge_binding,
            direction,
            label,
            label_expr,
            cost,
            ..
        } => {
            require_traversal(
                reqs,
                graph,
                catalog,
                label.as_ref(),
                label_expr.as_ref(),
                *direction,
            );
            note_bound_edge(scope, edge, label.as_ref(), *emit_edge_binding);
            if let ShortestPathCost::EdgeCostExpr { expr, .. } = cost {
                collect_expr_reads(expr, scope);
            }
        }

        // ──── GQL-specific ────
        PlanOp::Let { bindings } => {
            for binding in bindings {
                collect_expr_reads(&binding.value, scope);
            }
        }

        PlanOp::For { list, .. } => collect_expr_reads(list, scope),

        PlanOp::Filter { condition } => collect_expr_reads(condition, scope),

        PlanOp::Search {
            binding, provider, ..
        } => {
            collect_expr_reads(provider.query(), scope);
            collect_expr_reads(provider.limit(), scope);
            if let Some(filter) = provider.filter() {
                collect_expr_reads(filter, scope);
            }
            // ADR 0078 §1 layer 1: the search itself demands the same rows as a scan of
            // its source labels (plus the projected embedding source property), so
            // unauthorized callers are rejected before any ANN dispatch.
            let SearchProviderPlan::VectorIndex { index_name, .. } = provider;
            let index_name = index_name
                .iter()
                .map(|part| part.as_ref())
                .collect::<Vec<_>>()
                .join(".");
            require_vector_search_rows(reqs, scope, catalog, binding.as_ref(), &index_name);
        }

        // ──── Procedure calls ────
        PlanOp::CallProcedure { .. } => {
            // Named procedures stay behind the CALL_PROCEDURE capability gate upstream
            // (ADR 0074 §1b); no data-plane resource exists for them yet.
        }

        PlanOp::InlineProcedureCall { sub_plan, .. } => {
            walk_ops(&sub_plan.ops, scope, reqs, catalog);
        }

        PlanOp::UseGraph {
            graph_name,
            sub_plan,
        } => {
            let Some(sub_plan) = sub_plan else { return };
            match use_graph_target(graph_name, scope.graph, catalog) {
                Some(target) => {
                    // Fresh scope: another graph's catalogs own this segment's names, so
                    // outer label facts must not leak into its attribution.
                    let mut child = Scope::new(target);
                    walk_ops(sub_plan, &mut child, reqs, catalog);
                    resolve_pending(&mut child, reqs, catalog);
                }
                None => reqs.root_unattributed = true,
            }
        }

        // ──── Join operators ────
        PlanOp::HashJoin { left, right, .. } => {
            walk_ops(left, scope, reqs, catalog);
            walk_ops(right, scope, reqs, catalog);
        }

        PlanOp::CartesianProduct { left, right } => {
            walk_ops(left, scope, reqs, catalog);
            walk_ops(right, scope, reqs, catalog);
        }

        // ──── Aggregation / Output ────
        PlanOp::Aggregate {
            group_by,
            aggregates,
        } => {
            for expr in group_by {
                collect_expr_reads(expr, scope);
            }
            for spec in aggregates {
                for expr in [&spec.expr, &spec.expr2, &spec.filter]
                    .into_iter()
                    .flatten()
                {
                    collect_expr_reads(expr, scope);
                }
                if let Some(order_by) = &spec.order_by {
                    for item in &order_by.items {
                        collect_expr_reads(&item.expr, scope);
                    }
                }
            }
        }

        PlanOp::Project { columns, .. } => {
            for column in columns {
                collect_expr_reads(&column.expr, scope);
            }
        }

        PlanOp::Sort { order_by } => {
            for item in &order_by.items {
                collect_expr_reads(&item.expr, scope);
            }
        }

        PlanOp::Limit { count, offset } => {
            for expr in [count, offset].into_iter().flatten() {
                collect_expr_reads(expr, scope);
            }
        }

        PlanOp::SetOperation { right, .. } => walk_ops(&right.ops, scope, reqs, catalog),

        PlanOp::OptionalMatch { sub_plan } => walk_ops(sub_plan, scope, reqs, catalog),

        PlanOp::IndexIntersection {
            variable,
            scans,
            property_projection,
        } => {
            for spec in scans {
                scope.defer_vertex_property(variable, &spec.property);
            }
            for name in property_projection.iter().flat_map(|p| p.iter()) {
                scope.defer_vertex_property(variable, name);
            }
        }

        PlanOp::WorstCaseOptimalJoin { edges, .. } => {
            for wcoj in edges {
                require_traversal(
                    reqs,
                    graph,
                    catalog,
                    wcoj.label.as_ref(),
                    wcoj.label_expr.as_ref(),
                    wcoj.direction,
                );
                note_bound_edge(
                    scope,
                    &wcoj.variable,
                    wcoj.label.as_ref(),
                    /* hop edges are always bound */ true,
                );
                for predicate in &wcoj.dst_filter {
                    collect_expr_reads(predicate, scope);
                }
                note_expand_dst(scope, &wcoj.dst, None);
            }
        }

        PlanOp::TopK {
            order_by,
            k,
            offset,
        } => {
            for item in &order_by.items {
                collect_expr_reads(&item.expr, scope);
            }
            collect_expr_reads(k, scope);
            if let Some(offset) = offset {
                collect_expr_reads(offset, scope);
            }
        }

        // ──── Pipeline boundaries ────
        PlanOp::Materialize { columns, .. } => {
            for column in columns {
                collect_expr_reads(&column.expr, scope);
            }
        }

        // ──── DML ────
        PlanOp::InsertVertex {
            labels, properties, ..
        } => {
            require_create_labels(reqs, graph, catalog, labels);
            for assignment in properties {
                collect_expr_reads(&assignment.value, scope);
            }
        }

        PlanOp::InsertEdge {
            direction,
            labels,
            properties,
            ..
        } => {
            for label in labels {
                let Some((id, _)) = catalog.edge_label(graph, &label.name) else {
                    reqs.require_unattributed(graph);
                    continue;
                };
                reqs.require(graph, edge_row(graph.raw(), GraphOperation::Create, id));
            }
            let _ = direction; // insertion orientation carries no additional privilege
            for assignment in properties {
                collect_expr_reads(&assignment.value, scope);
            }
        }

        PlanOp::SetProperties { items } => {
            for item in items {
                match item {
                    SetPlanItem::Property {
                        variable, value, ..
                    } => {
                        require_mutation_on_variable(
                            reqs,
                            scope,
                            catalog,
                            variable,
                            GraphOperation::Update,
                        );
                        collect_expr_reads(value, scope);
                    }
                    SetPlanItem::AllProperties { variable, value } => {
                        require_mutation_on_variable(
                            reqs,
                            scope,
                            catalog,
                            variable,
                            GraphOperation::Update,
                        );
                        collect_expr_reads(value, scope);
                    }
                    SetPlanItem::Label { variable, label } => {
                        require_mutation_on_variable(
                            reqs,
                            scope,
                            catalog,
                            variable,
                            GraphOperation::Update,
                        );
                        // Adding a label mutates membership in the added vocabulary too.
                        require_vertex_mutation(
                            reqs,
                            graph,
                            catalog,
                            label,
                            GraphOperation::Update,
                        );
                    }
                }
            }
        }

        PlanOp::RemoveProperties { items } => {
            for item in items {
                match item {
                    RemovePlanItem::Property { variable, .. } => {
                        require_mutation_on_variable(
                            reqs,
                            scope,
                            catalog,
                            variable,
                            GraphOperation::Update,
                        );
                    }
                    RemovePlanItem::Label { variable, label } => {
                        require_mutation_on_variable(
                            reqs,
                            scope,
                            catalog,
                            variable,
                            GraphOperation::Update,
                        );
                        require_vertex_mutation(
                            reqs,
                            graph,
                            catalog,
                            label,
                            GraphOperation::Update,
                        );
                    }
                }
            }
        }

        PlanOp::DeleteVertex { variable } => {
            require_mutation_on_variable(reqs, scope, catalog, variable, GraphOperation::Delete);
        }

        PlanOp::DetachDeleteVertex { variable } => {
            require_mutation_on_variable(reqs, scope, catalog, variable, GraphOperation::Delete);
            // Detach also deletes incident edges whose labels the plan never enumerates.
            reqs.require_unattributed(graph);
        }

        PlanOp::DeleteEdge { variable } => {
            require_edge_deletion_on_variable(reqs, scope, catalog, variable);
        }
    }
}

/// Resolve a `USE GRAPH` segment target; `None` marks the segment unattributable.
fn use_graph_target(
    graph_name: &[Rc<str>],
    current: GraphId,
    catalog: &dyn PlanCatalogView,
) -> Option<GraphId> {
    let [name] = graph_name else {
        return None;
    };
    if name.as_ref() == "CURRENT_GRAPH" {
        return Some(current);
    }
    catalog.graph_by_name(name)
}

/// Collect property-value reads and `IS LABELED` label facts from one expression tree.
fn collect_expr_reads(expr: &Expr, scope: &mut Scope) {
    match &expr.kind {
        ExprKind::PropertyAccess {
            expr: base,
            property,
        } => {
            // Only property accesses whose immediate base is a graph variable read graph
            // properties; nested suffixes address fields of returned records.
            if let ExprKind::Variable(name) = &base.kind {
                scope.defer_vertex_property(name, property);
            }
        }
        ExprKind::IsLabeled {
            expr: base, label, ..
        } => {
            if let ExprKind::Variable(name) = &base.kind {
                let mut names = Vec::new();
                let mut decidable = true;
                collect_label_names(label, &mut names, &mut decidable);
                if decidable {
                    for label_name in names {
                        scope.note_vertex_label(name, &label_name);
                    }
                }
            }
        }
        _ => {}
    }
    for_each_immediate_child_expr(expr, |child| collect_expr_reads(child, scope));
}

/// Positive label names of a label expression; `decidable` stays true only when the whole
/// expression is built from `Name` / `And` / `Or`.
fn collect_label_names(expr: &LabelExpr, names: &mut Vec<String>, decidable: &mut bool) {
    match expr {
        LabelExpr::Name(name) => names.push(name.clone()),
        LabelExpr::And(left, right) | LabelExpr::Or(left, right) => {
            collect_label_names(left, names, decidable);
            collect_label_names(right, names, decidable);
        }
        LabelExpr::Not(_) | LabelExpr::Wildcard => *decidable = false,
    }
}

/// Resolve deferred reads once a scope's label facts are complete.
fn resolve_pending(scope: &mut Scope, reqs: &mut RequirementSet, catalog: &dyn PlanCatalogView) {
    let pending = std::mem::take(&mut scope.pending);
    for read in pending {
        match read {
            PendingRead::VertexProperty { variable, property } => {
                resolve_vertex_property_read(scope, reqs, catalog, &variable, &property);
            }
            PendingRead::EdgeProperty { variable } => {
                // An attributed edge-property read is covered by the edge label's
                // traversal row (Phase 1 has no separate edge-property resource); an
                // unattributable one degrades to tenancy-only coverage.
                if scope.unique_edge_label(&variable).is_none() {
                    reqs.require_unattributed(scope.graph);
                }
            }
        }
    }
}

fn resolve_vertex_property_read(
    scope: &Scope,
    reqs: &mut RequirementSet,
    catalog: &dyn PlanCatalogView,
    variable: &str,
    property: &str,
) {
    let Some(label_name) = scope.unique_vertex_label(variable) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    let Some(label_id) = catalog.vertex_label(scope.graph, label_name) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    let Some(property_id) = catalog.property(scope.graph, property) else {
        reqs.require_unattributed(scope.graph);
        return;
    };
    reqs.require(
        scope.graph,
        vertex_property_row(scope.graph.raw(), label_id, property_id),
    );
}

// ──── Evaluation ────

/// Coverage probe over the caller's effective privileges (`caller ∪ PUBLIC`,
/// expiry-aware). Deliberately exposes no administrative-capability input.
pub(crate) trait EffectiveGrants {
    fn covers(&self, caller: &Principal, privilege: &Privilege) -> bool;
}

/// Implicit-root coverage (ADR 0074 §3 invariant 3): the registry owner's (and admins')
/// authority over a graph is evaluated at enforcement time from the registry and is never
/// materialized as grant rows.
pub(crate) trait GraphTenancy {
    fn is_tenant(&self, graph_raw: u32, caller: &Principal) -> bool;
}

/// Whether `reqs` is fully covered for `caller`. Uniform denial: the caller learns only
/// that the query may not run, never which demand failed.
pub(crate) fn authorize_requirements(
    reqs: &RequirementSet,
    root_graph_raw: u32,
    caller: &Principal,
    grants: &dyn EffectiveGrants,
    tenancy: &dyn GraphTenancy,
) -> bool {
    if reqs.root_unattributed && !tenancy.is_tenant(root_graph_raw, caller) {
        return false;
    }
    for (graph_raw, demands) in &reqs.graphs {
        if tenancy.is_tenant(*graph_raw, caller) {
            continue;
        }
        if demands.unattributed {
            return false;
        }
        if !demands
            .conjunctive
            .iter()
            .all(|privilege| grants.covers(caller, privilege))
        {
            return false;
        }
        // Alternation groups: any ONE fully covered group satisfies the demand; no
        // groups means no alternation demand.
        if !demands.alternatives.is_empty()
            && !demands.alternatives.iter().any(|group| {
                group
                    .iter()
                    .all(|privilege| grants.covers(caller, privilege))
            })
        {
            return false;
        }
    }
    true
}

/// Stored-grant coverage probe (`ROUTER_AUTH_GRANTS`, expiry-aware).
struct StoredGrants;

impl EffectiveGrants for StoredGrants {
    fn covers(&self, caller: &Principal, privilege: &Privilege) -> bool {
        let now_ns = crate::facade::store::ic_time_ns();
        crate::facade::stable::ROUTER_AUTH_GRANTS.with_borrow(|grants| {
            grants.holds(GrantSubject::effective_for(caller), privilege, now_ns)
                || grants.holds(GrantSubject::Public, privilege, now_ns)
        })
    }
}

/// Registry-backed ownership probe.
struct StoreTenancy<'a> {
    store: &'a RouterStore,
}

impl GraphTenancy for StoreTenancy<'_> {
    fn is_tenant(&self, graph_raw: u32, caller: &Principal) -> bool {
        self.store
            .is_graph_tenant(GraphId::from_raw(graph_raw), *caller)
    }
}

/// Whether `reqs` is covered for `caller` against live Router state (`caller ∪ PUBLIC`
/// grant rows plus implicit ownership root). The single seam every enforcement surface
/// uses, so evaluation semantics cannot drift between call sites.
pub(crate) fn requirements_cover(
    reqs: &RequirementSet,
    root_graph_raw: u32,
    caller: &Principal,
    store: &RouterStore,
) -> bool {
    authorize_requirements(
        reqs,
        root_graph_raw,
        caller,
        &StoredGrants,
        &StoreTenancy { store },
    )
}

/// Extract the requirement set of `plan` against live Router catalogs.
///
/// Used by ad-hoc enforcement and by prepared registration, where the extracted set is
/// stored on the record (ADR 0074 slice 3).
pub(crate) fn extract_live(
    store: &RouterStore,
    plan: &PhysicalPlan,
    root_graph: GraphId,
) -> RequirementSet {
    extract(plan, root_graph, &RouterCatalogView { store })
}

/// Enforce plan-time data-plane authorization for `plan` executing on `root_graph`.
///
/// Single failure contract (ADR 0074 §4): a uniform [`RouterError::Forbidden`] that names
/// neither the missing privilege nor the resource.
pub(crate) fn enforce_data_plane_authorization(
    store: &RouterStore,
    caller: &Principal,
    root_graph: GraphId,
    plan: &PhysicalPlan,
) -> Result<(), RouterError> {
    let reqs = extract_live(store, plan, root_graph);
    if requirements_cover(&reqs, root_graph.raw(), caller, store) {
        Ok(())
    } else {
        Err(RouterError::Forbidden)
    }
}

/// Enforce prepared-query data-plane authorization (ADR 0074 slice 3).
///
/// The registration-time [`RequirementSet`] stored on the record is the primary checked
/// artifact. When it denies, the live walk of the actually-dispatched `plan` decides as
/// the fallback: catalogs may have drifted since registration, and derived sorted plans
/// carry demands the stored base-plan set does not describe. Both evaluations fail closed
/// and share one semantics ([`requirements_cover`]); admitting only when either admits is
/// safe because catalog ids are monotonic (ADR 0074 invariant 4), so a stored demand can
/// never re-bind to different vocabulary.
///
/// **Stored-set staleness is fail-closed by design.** When vocabulary named by a stored
/// requirement set is dropped (today: whole-graph teardown via
/// `purge_graph_vocabulary_partitions`, which also sweeps the graph's grant rows), the
/// dead-id demands can never be covered again — grants resolve through live catalogs whose
/// ids are monotonic, so no future grant row can match a dropped id. Stored requirement
/// sets are therefore *not* recomputed or patched in place; queries referencing dropped
/// vocabulary stay uniformly denied until the prepared query is re-registered against the
/// current catalogs (re-registration replaces the whole record).
pub(crate) fn enforce_prepared_data_plane_authorization(
    store: &RouterStore,
    caller: &Principal,
    root_graph: GraphId,
    stored: &RequirementSet,
    plan: &PhysicalPlan,
) -> Result<(), RouterError> {
    if requirements_cover(stored, root_graph.raw(), caller, store) {
        return Ok(());
    }
    enforce_data_plane_authorization(store, caller, root_graph, plan)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gleaph_auth::GrantState;
    use gleaph_gql_planner::plan::Str;
    use ic_stable_structures::DefaultMemoryImpl;
    use std::collections::BTreeSet;

    const NOW_NS: u64 = 1_000;
    const GRAPH_RAW: u32 = 7;

    fn graph() -> GraphId {
        GraphId::from_raw(GRAPH_RAW)
    }

    fn node_label(name: &str) -> Option<NodeLabelRef> {
        Some(NodeLabelRef::new(name))
    }

    /// Fixed vocabulary: Person=1, Employee=2; age=20, name=21, secret=22;
    /// KNOWS=10 directed, LIKES=11 undirected, MENTIONS=12 undeclared.
    fn catalog() -> FakeCatalog {
        FakeCatalog {
            vertices: vec![("Person", 1), ("Employee", 2)],
            edges: vec![
                ("KNOWS", 10, Some(false)),
                ("LIKES", 11, Some(true)),
                ("MENTIONS", 12, None),
            ],
            properties: vec![("age", 20), ("name", 21), ("secret", 22)],
            vector_indexes: Vec::new(),
        }
    }

    struct FakeCatalog {
        vertices: Vec<(&'static str, u32)>,
        edges: Vec<(&'static str, u32, Option<bool>)>,
        properties: Vec<(&'static str, u32)>,
        vector_indexes: Vec<(&'static str, VectorIndexSource)>,
    }

    impl PlanCatalogView for FakeCatalog {
        fn graph_by_name(&self, name: &str) -> Option<GraphId> {
            match name {
                "other" => Some(GraphId::from_raw(9)),
                _ => None,
            }
        }

        fn vertex_label(&self, graph: GraphId, name: &str) -> Option<u32> {
            if graph.raw() != GRAPH_RAW {
                return None;
            }
            self.vertices
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, id)| *id)
        }

        fn edge_label(&self, graph: GraphId, name: &str) -> Option<(u32, Option<bool>)> {
            if graph.raw() != GRAPH_RAW {
                return None;
            }
            self.edges
                .iter()
                .find(|(candidate, _, _)| *candidate == name)
                .map(|(_, id, undirected)| (*id, *undirected))
        }

        fn property(&self, graph: GraphId, name: &str) -> Option<u32> {
            if graph.raw() != GRAPH_RAW {
                return None;
            }
            self.properties
                .iter()
                .find(|(candidate, _)| *candidate == name)
                .map(|(_, id)| *id)
        }

        fn vector_index_source(
            &self,
            _graph: GraphId,
            index_name: &str,
        ) -> Option<VectorIndexSource> {
            self.vector_indexes
                .iter()
                .find(|(candidate, _)| *candidate == index_name)
                .map(|(_, source)| source.clone())
        }
    }

    /// Real [`GrantState`] storage semantics behind the evaluation trait, mirroring
    /// `StoredGrants` (caller ∪ PUBLIC, expiry-aware) at a fixed `now`.
    struct GrantTable {
        state: GrantState<DefaultMemoryImpl>,
    }

    impl Default for GrantTable {
        fn default() -> Self {
            Self {
                state: GrantState::init(DefaultMemoryImpl::default()),
            }
        }
    }

    impl GrantTable {
        fn grant(&mut self, subject: GrantSubject, privilege: &Privilege) {
            self.state
                .grant(subject, privilege, None, None)
                .expect("grant");
        }

        fn grant_expiring(&mut self, subject: GrantSubject, privilege: &Privilege, at: u64) {
            self.state
                .grant(subject, privilege, Some(at), None)
                .expect("grant");
        }

        fn traverse(&mut self, subject: GrantSubject, direction: Option<Direction>, label: u32) {
            self.grant(
                subject,
                &Privilege::Graph(GraphPrivilege {
                    graph: GRAPH_RAW,
                    operation: GraphOperation::Traverse(direction),
                    resource: GraphResource::EdgeLabel(label),
                }),
            );
        }

        fn vertex(&mut self, subject: GrantSubject, operation: GraphOperation, label: u32) {
            self.grant(
                subject,
                &Privilege::Graph(GraphPrivilege {
                    graph: GRAPH_RAW,
                    operation,
                    resource: GraphResource::VertexLabel(label),
                }),
            );
        }

        fn vertex_property(&mut self, subject: GrantSubject, label: u32, property: u32) {
            self.grant(
                subject,
                &Privilege::Graph(GraphPrivilege {
                    graph: GRAPH_RAW,
                    operation: GraphOperation::ReadProperty,
                    resource: GraphResource::VertexProperty { label, property },
                }),
            );
        }
    }

    impl EffectiveGrants for GrantTable {
        fn covers(&self, caller: &Principal, privilege: &Privilege) -> bool {
            self.state
                .holds(GrantSubject::effective_for(caller), privilege, NOW_NS)
                || self.state.holds(GrantSubject::Public, privilege, NOW_NS)
        }
    }

    struct TenantSet(BTreeSet<u32>);

    impl GraphTenancy for TenantSet {
        fn is_tenant(&self, graph_raw: u32, _caller: &Principal) -> bool {
            self.0.contains(&graph_raw)
        }
    }

    fn principal(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    fn allowed(
        reqs: &RequirementSet,
        caller: &Principal,
        grants: &dyn EffectiveGrants,
        tenants: &[u32],
    ) -> bool {
        authorize_requirements(
            reqs,
            GRAPH_RAW,
            caller,
            grants,
            &TenantSet(tenants.iter().copied().collect()),
        )
    }

    /// Deliberately wrong evaluator used as a probe: it must NOT satisfy the denial
    /// assertions that the real evaluation passes.
    struct AlwaysAllow;

    impl EffectiveGrants for AlwaysAllow {
        fn covers(&self, _: &Principal, _: &Privilege) -> bool {
            true
        }
    }

    fn extract_ops(ops: Vec<PlanOp>) -> RequirementSet {
        extract(&PhysicalPlan::from_ops(ops), graph(), &catalog())
    }

    fn projection(keys: &[&str]) -> Option<Rc<[Str]>> {
        Some(keys.iter().map(|k| Rc::from(*k)).collect())
    }

    /// Scan with an explicit (possibly empty) hydration list.
    fn scan(var: &str, label: Option<&str>, projection_keys: &[&str]) -> PlanOp {
        PlanOp::NodeScan {
            variable: var.into(),
            label: label.map(NodeLabelRef::new),
            property_projection: Some(
                projection_keys
                    .iter()
                    .map(|k| Rc::from(*k))
                    .collect::<Rc<[Str]>>(),
            ),
        }
    }

    /// Scan retaining the full property map (`property_projection: None`).
    fn full_map_scan(var: &str, label: Option<&str>) -> PlanOp {
        PlanOp::NodeScan {
            variable: var.into(),
            label: label.map(NodeLabelRef::new),
            property_projection: None,
        }
    }

    /// Expand hop with only the fields under test populated.
    fn expand(direction: EdgeDirection, label: Option<&str>) -> PlanOp {
        expand_with(direction, label, None)
    }

    fn expand_with(
        direction: EdgeDirection,
        label: Option<&str>,
        label_expr: Option<LabelExpr>,
    ) -> PlanOp {
        PlanOp::Expand {
            src: "a".into(),
            edge: "__e".into(),
            dst: "b".into(),
            direction,
            label: label.map(EdgeLabelRef::new),
            label_expr,
            var_len: None,
            indexed_edge_equality: None,
            edge_inline_property_predicate: None,
            edge_inline_vector_predicate: None,
            edge_property_projection: projection(&[]),
            dst_property_projection: None,
            hop_aux_binding: None,
            emit_edge_binding: true,
            near_group_var: None,
            far_group_var: None,
            path_var: None,
            emit_path_binding: false,
        }
    }

    fn traverse_row(direction: Option<Direction>, label: u32) -> Privilege {
        Privilege::Graph(GraphPrivilege {
            graph: GRAPH_RAW,
            operation: GraphOperation::Traverse(direction),
            resource: GraphResource::EdgeLabel(label),
        })
    }

    /// The traversal rows a plan demands, as a sorted multiset for exact assertions.
    fn demanded_traversal_rows(ops: Vec<PlanOp>) -> Vec<Privilege> {
        let reqs = extract_ops(ops);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("graph demands present");
        assert!(!demands.unattributed, "traversal must be attributable");
        let mut rows = demands.conjunctive.clone();
        rows.sort_by(|a, b| format!("{a:?}").cmp(&format!("{b:?}")));
        rows
    }

    #[test]
    fn direction_matrix_over_directed_label() {
        // Declared DIRECTED label: one directional row per orientation; every
        // undirected pattern shape requires BOTH rows (ADR 0074 §2).
        let expected: [(EdgeDirection, Vec<Privilege>); 7] = [
            (
                EdgeDirection::PointingRight,
                vec![traverse_row(Some(Direction::Outgoing), 10)],
            ),
            (
                EdgeDirection::PointingLeft,
                vec![traverse_row(Some(Direction::Incoming), 10)],
            ),
            (
                EdgeDirection::LeftOrRight,
                vec![
                    traverse_row(Some(Direction::Incoming), 10),
                    traverse_row(Some(Direction::Outgoing), 10),
                ],
            ),
            (
                EdgeDirection::Undirected,
                vec![
                    traverse_row(Some(Direction::Incoming), 10),
                    traverse_row(Some(Direction::Outgoing), 10),
                ],
            ),
            (
                EdgeDirection::LeftOrUndirected,
                vec![
                    traverse_row(Some(Direction::Incoming), 10),
                    traverse_row(Some(Direction::Outgoing), 10),
                ],
            ),
            (
                EdgeDirection::UndirectedOrRight,
                vec![
                    traverse_row(Some(Direction::Incoming), 10),
                    traverse_row(Some(Direction::Outgoing), 10),
                ],
            ),
            (
                EdgeDirection::AnyDirection,
                vec![
                    traverse_row(Some(Direction::Incoming), 10),
                    traverse_row(Some(Direction::Outgoing), 10),
                ],
            ),
        ];
        for (direction, want) in expected {
            assert_eq!(
                demanded_traversal_rows(vec![expand(direction, Some("KNOWS"))]),
                want,
                "direction {direction:?}"
            );
        }
    }

    #[test]
    fn direction_matrix_over_undirected_label() {
        // Declared UNDIRECTED label: only the unoriented row exists, for every shape.
        for direction in [
            EdgeDirection::PointingRight,
            EdgeDirection::PointingLeft,
            EdgeDirection::LeftOrRight,
            EdgeDirection::Undirected,
            EdgeDirection::LeftOrUndirected,
            EdgeDirection::UndirectedOrRight,
            EdgeDirection::AnyDirection,
        ] {
            assert_eq!(
                demanded_traversal_rows(vec![expand(direction, Some("LIKES"))]),
                vec![traverse_row(None, 11)],
                "direction {direction:?}"
            );
        }
    }

    #[test]
    fn undeclared_directedness_takes_unoriented_row() {
        // Grant-side fail-closure means only the unoriented row can exist for labels
        // whose directedness the schema does not declare.
        assert_eq!(
            demanded_traversal_rows(vec![expand(EdgeDirection::PointingRight, Some("MENTIONS"))]),
            vec![traverse_row(None, 12)]
        );
    }

    #[test]
    fn reverse_traversal_denied_like_any_other_demand() {
        let forward = extract_ops(vec![
            scan("a", Some("Person"), &[]),
            expand(EdgeDirection::PointingRight, Some("KNOWS")),
        ]);
        let reverse = extract_ops(vec![
            scan("a", Some("Person"), &[]),
            expand(EdgeDirection::PointingLeft, Some("KNOWS")),
        ]);
        let mut grants = GrantTable::default();
        let caller = principal(1);
        grants.vertex(GrantSubject::Principal(caller), GraphOperation::Match, 1);
        grants.traverse(
            GrantSubject::Principal(caller),
            Some(Direction::Outgoing),
            10,
        );

        // Forward pattern admitted...
        assert!(allowed(&forward, &caller, &grants, &[]));
        // ...reverse pattern uniformly denied: same boolean outcome that maps to the
        // same Forbidden error as any other uncovered demand.
        assert!(!allowed(&reverse, &caller, &grants, &[]));
        // Wrong-implementation probe: an evaluator granting everything cannot satisfy
        // these assertions — it would admit the reverse pattern too.
        assert!(allowed(&reverse, &caller, &AlwaysAllow, &[]));
    }

    #[test]
    fn unlabeled_scan_is_tenancy_only() {
        let reqs = extract_ops(vec![full_map_scan("n", None)]);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(demands.unattributed);
        // Even a grantee of every listed row cannot pass; tenancy does.
        let everything = grants_state_with_all_listed_rows();
        assert!(!allowed(&reqs, &principal(1), &everything, &[]));
        assert!(allowed(&reqs, &principal(1), &everything, &[GRAPH_RAW]));
    }

    fn grants_state_with_all_listed_rows() -> GrantTable {
        let mut grants = GrantTable::default();
        grants.vertex(GrantSubject::Public, GraphOperation::Match, 1);
        grants.vertex(GrantSubject::Public, GraphOperation::Read, 1);
        grants.vertex_property(GrantSubject::Public, 1, 21);
        grants.traverse(GrantSubject::Public, Some(Direction::Outgoing), 10);
        grants
    }

    #[test]
    fn labeled_scan_projection_contract() {
        let full_map = extract_ops(vec![full_map_scan("n", Some("Person"))]);
        let empty_projection = extract_ops(vec![scan("n", Some("Person"), &[])]);
        let explicit_name = extract_ops(vec![scan("n", Some("Person"), &["name"])]);

        let mut grants = GrantTable::default();
        let reader = principal(9);
        grants.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);

        // MATCH alone admits only the no-hydration projection.
        assert!(allowed(&empty_projection, &reader, &grants, &[]));
        assert!(!allowed(&full_map, &reader, &grants, &[]));
        assert!(!allowed(&explicit_name, &reader, &grants, &[]));

        // READ covers the whole-map demand but NOT an enumerated property demand.
        grants.vertex(GrantSubject::Principal(reader), GraphOperation::Read, 1);
        assert!(allowed(&full_map, &reader, &grants, &[]));
        assert!(!allowed(&explicit_name, &reader, &grants, &[]));

        // The enumerated READ_PROPERTY row admits exactly its own key.
        grants.vertex_property(GrantSubject::Principal(reader), 1, 21);
        assert!(allowed(&explicit_name, &reader, &grants, &[]));
        // ...and a different projected key stays denied.
        let explicit_secret = extract_ops(vec![scan("n", Some("Person"), &["secret"])]);
        assert!(!allowed(&explicit_secret, &reader, &grants, &[]));
    }

    #[test]
    fn filter_and_project_reads_are_attributed_through_label_facts() {
        let reqs = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::PropertyFilter {
                predicates: vec![property_access("n", "age")],
                stage: 0,
            },
            PlanOp::Project {
                columns: vec![gleaph_gql_planner::plan::ProjectColumn {
                    expr: property_access("n", "name"),
                    alias: None,
                }],
                distinct: false,
            },
        ]);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(!demands.unattributed);
        let rendered: BTreeSet<String> = demands
            .conjunctive
            .iter()
            .map(|p| format!("{p:?}"))
            .collect();
        assert!(rendered.contains(&format!(
            "{:?}",
            vertex_row(GRAPH_RAW, GraphOperation::Match, 1)
        )));
        assert!(rendered.contains(&format!("{:?}", vertex_property_row(GRAPH_RAW, 1, 20))));
        assert!(rendered.contains(&format!("{:?}", vertex_property_row(GRAPH_RAW, 1, 21))));

        // A read through a variable bound to two different labels degrades to
        // tenancy-only instead of guessing one label.
        let ambiguous = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            scan("n", Some("Employee"), &[]),
            PlanOp::PropertyFilter {
                predicates: vec![property_access("n", "age")],
                stage: 0,
            },
        ]);
        assert!(ambiguous.graphs.get(&GRAPH_RAW).unwrap().unattributed);
    }

    fn property_access(var: &str, property: &str) -> Expr {
        use gleaph_gql::ast::ExprKind;
        Expr::new(ExprKind::Compare {
            left: Box::new(Expr::new(ExprKind::PropertyAccess {
                expr: Box::new(Expr::new(ExprKind::Variable(var.into()))),
                property: property.into(),
            })),
            op: gleaph_gql::ast::CmpOp::Gt,
            right: Box::new(Expr::new(ExprKind::Variable("x".into()))),
        })
    }

    #[test]
    fn public_row_enables_anonymous_adhoc_reads() {
        let reqs = extract_ops(vec![
            scan("n", Some("Person"), &["name"]),
            expand(EdgeDirection::PointingRight, Some("KNOWS")),
        ]);
        let grants = grants_state_with_all_listed_rows();
        assert!(
            allowed(&reqs, &Principal::anonymous(), &grants, &[]),
            "anonymous evaluation resolves the PUBLIC baseline"
        );
        // Expiry awareness: the same rows expired before `now` cover nothing.
        let mut expired = GrantTable::default();
        expired.grant_expiring(
            GrantSubject::Public,
            &vertex_row(GRAPH_RAW, GraphOperation::Match, 1),
            NOW_NS - 1,
        );
        expired.grant_expiring(
            GrantSubject::Public,
            &vertex_property_row(GRAPH_RAW, 1, 21),
            NOW_NS - 1,
        );
        expired.grant_expiring(
            GrantSubject::Public,
            &traverse_row(Some(Direction::Outgoing), 10),
            NOW_NS - 1,
        );
        let minimal = extract_ops(vec![scan("n", Some("Person"), &[])]);
        assert!(!allowed(&minimal, &Principal::anonymous(), &expired, &[]));
    }

    #[test]
    fn mutation_matrix_demands_operation_specific_rows() {
        let reader = principal(3);
        let insert_vertex = extract_ops(vec![PlanOp::InsertVertex {
            variable: Some("n".into()),
            labels: vec![NodeLabelRef::new("Person")],
            properties: vec![assignment("name", "n")],
        }]);
        let insert_edge = extract_ops(vec![PlanOp::InsertEdge {
            variable: Some("e".into()),
            src: "a".into(),
            dst: "b".into(),
            direction: EdgeDirection::PointingRight,
            labels: vec![EdgeLabelRef::new("KNOWS")],
            properties: vec![assignment("since", "e")],
        }]);
        let set_property = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::SetProperties {
                items: vec![SetPlanItem::Property {
                    variable: "n".into(),
                    property: "age".into(),
                    value: literal_text("5"),
                }],
            },
        ]);
        let set_label = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::SetProperties {
                items: vec![SetPlanItem::Label {
                    variable: "n".into(),
                    label: NodeLabelRef::new("Employee"),
                }],
            },
        ]);
        let remove_property = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::RemoveProperties {
                items: vec![RemovePlanItem::Property {
                    variable: "n".into(),
                    property: "age".into(),
                }],
            },
        ]);
        let remove_label = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::RemoveProperties {
                items: vec![RemovePlanItem::Label {
                    variable: "n".into(),
                    label: NodeLabelRef::new("Employee"),
                }],
            },
        ]);
        let delete_vertex = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::DeleteVertex {
                variable: "n".into(),
            },
        ]);
        let detach_delete = extract_ops(vec![
            scan("n", Some("Person"), &[]),
            PlanOp::DetachDeleteVertex {
                variable: "n".into(),
            },
        ]);
        let delete_edge = extract_ops(vec![
            scan("a", Some("Person"), &[]),
            PlanOp::Expand {
                src: "a".into(),
                edge: "k".into(),
                dst: "b".into(),
                direction: EdgeDirection::PointingRight,
                label: node_label_of_edge("KNOWS"),
                label_expr: None,
                var_len: None,
                indexed_edge_equality: None,
                edge_inline_property_predicate: None,
                edge_inline_vector_predicate: None,
                edge_property_projection: projection(&[]),
                dst_property_projection: None,
                hop_aux_binding: None,
                emit_edge_binding: true,
                near_group_var: None,
                far_group_var: None,
                path_var: None,
                emit_path_binding: false,
            },
            PlanOp::DeleteEdge {
                variable: "k".into(),
            },
        ]);

        // Nothing is covered without rows.
        let empty = GrantTable::default();
        for (name, reqs) in [
            ("insert_vertex", &insert_vertex),
            ("insert_edge", &insert_edge),
            ("set_property", &set_property),
            ("set_label", &set_label),
            ("remove_property", &remove_property),
            ("remove_label", &remove_label),
            ("delete_vertex", &delete_vertex),
            ("detach_delete", &detach_delete),
            ("delete_edge", &delete_edge),
        ] {
            assert!(
                !allowed(reqs, &reader, &empty, &[]),
                "{name} must be denied without rows"
            );
        }

        // Detach-delete additionally destroys incident edges of unlisted labels, so even
        // full per-label rows stay tenancy-only.
        let mut full_rows = GrantTable::default();
        fully_granted(&mut full_rows);
        assert!(
            !allowed(&detach_delete, &reader, &full_rows, &[]),
            "DETACH DELETE is tenancy-only in Phase 1"
        );

        // Each operation is admitted by exactly its own row(s).
        let mut g = GrantTable::default();
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Create, 1);
        assert!(allowed(&insert_vertex, &reader, &g, &[]));
        let mut g = GrantTable::default();
        g.grant(
            GrantSubject::Principal(reader),
            &edge_row(GRAPH_RAW, GraphOperation::Create, 10),
        );
        assert!(allowed(&insert_edge, &reader, &g, &[]));
        let mut g = GrantTable::default();
        // The anchor scan needs MATCH; the mutation itself needs UPDATE.
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Update, 1);
        assert!(allowed(&set_property, &reader, &g, &[]));
        assert!(allowed(&remove_property, &reader, &g, &[]));
        let mut g = GrantTable::default();
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Update, 1);
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Update, 2);
        assert!(allowed(&set_label, &reader, &g, &[]));
        assert!(allowed(&remove_label, &reader, &g, &[]));
        let mut g = GrantTable::default();
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Delete, 1);
        assert!(allowed(&delete_vertex, &reader, &g, &[]));
        let mut g = GrantTable::default();
        g.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);
        g.grant(
            GrantSubject::Principal(reader),
            &edge_row(GRAPH_RAW, GraphOperation::Delete, 10),
        );
        g.traverse(
            GrantSubject::Principal(reader),
            Some(Direction::Outgoing),
            10,
        );
        assert!(allowed(&delete_edge, &reader, &g, &[]));
    }

    fn node_label_of_edge(name: &str) -> Option<EdgeLabelRef> {
        Some(EdgeLabelRef::new(name))
    }

    fn fully_granted(grants: &mut GrantTable) -> &GrantTable {
        grants.vertex(
            GrantSubject::Principal(principal(3)),
            GraphOperation::Match,
            1,
        );
        grants.vertex(
            GrantSubject::Principal(principal(3)),
            GraphOperation::Delete,
            1,
        );
        grants
    }

    fn assignment(name: &str, _var: &str) -> gleaph_gql_planner::plan::PropertyAssignment {
        gleaph_gql_planner::plan::PropertyAssignment {
            name: name.into(),
            // Literal values keep insert demands exactly CREATE; no expression reads.
            value: literal_text(name),
        }
    }

    fn literal_text(text: &str) -> Expr {
        Expr::new(ExprKind::Literal(gleaph_gql::Value::Text(text.into())))
    }

    #[test]
    fn caps_are_not_an_input_to_evaluation() {
        // Structural guarantee (ADR 0074 invariant 1): the evaluation surface accepts a
        // caller identity and probe traits only. There is no capability parameter to
        // leak, so this test pins the denial of a caps-shaped-but-grantless caller.
        let reqs = extract_ops(vec![scan("n", Some("Person"), &[])]);
        assert!(!allowed(&reqs, &principal(4), &GrantTable::default(), &[]));
    }

    #[test]
    fn permissive_stub_evaluator_fails_the_denial_matrix() {
        // Wrong-implementation probe required by the plan: a stub evaluator that grants
        // everything must fail this matrix. Every denial assertion above would break if
        // the real evaluator behaved like `AlwaysAllow`; this asserts the discriminator
        // directly.
        let denied_without_rows =
            extract_ops(vec![expand(EdgeDirection::PointingLeft, Some("KNOWS"))]);
        let real = GrantTable::default();
        assert!(!allowed(&denied_without_rows, &principal(5), &real, &[]));
        assert!(allowed(
            &denied_without_rows,
            &principal(5),
            &AlwaysAllow,
            &[]
        ));
    }

    #[test]
    fn label_expression_alternatives_and_conjunctions() {
        // `-[:KNOWS|LIKES]->`: either alternative suffices.
        let or_plan = extract_ops(vec![expand_with(
            EdgeDirection::PointingRight,
            None,
            Some(LabelExpr::Or(
                Box::new(LabelExpr::Name("KNOWS".into())),
                Box::new(LabelExpr::Name("LIKES".into())),
            )),
        )]);
        let mut grants = GrantTable::default();
        grants.traverse(
            GrantSubject::Principal(principal(6)),
            Some(Direction::Outgoing),
            10,
        );
        assert!(allowed(&or_plan, &principal(6), &grants, &[]));
        let mut other = GrantTable::default();
        other.traverse(GrantSubject::Principal(principal(6)), None, 11);
        assert!(allowed(&or_plan, &principal(6), &other, &[]));
        let none = GrantTable::default();
        assert!(!allowed(&or_plan, &principal(6), &none, &[]));

        // `AND` demands both labels conjunctively.
        let and_plan = extract_ops(vec![conjunction_expand()]);
        let mut both = GrantTable::default();
        both.traverse(
            GrantSubject::Principal(principal(6)),
            Some(Direction::Outgoing),
            10,
        );
        both.traverse(GrantSubject::Principal(principal(6)), None, 11);
        assert!(allowed(&and_plan, &principal(6), &both, &[]));
        assert!(!allowed(&and_plan, &principal(6), &grants, &[]));

        // `NOT` / wildcard are not decomposable: tenancy-only.
        let not_plan = extract_ops(vec![negation_expand()]);
        assert!(not_plan.graphs.get(&GRAPH_RAW).unwrap().unattributed);
        let wildcard_plan = extract_ops(vec![wildcard_expand()]);
        assert!(wildcard_plan.graphs.get(&GRAPH_RAW).unwrap().unattributed);
    }

    fn conjunction_expand() -> PlanOp {
        expand_with(
            EdgeDirection::PointingRight,
            None,
            Some(LabelExpr::And(
                Box::new(LabelExpr::Name("KNOWS".into())),
                Box::new(LabelExpr::Name("LIKES".into())),
            )),
        )
    }

    fn negation_expand() -> PlanOp {
        expand_with(
            EdgeDirection::PointingRight,
            None,
            Some(LabelExpr::Not(Box::new(LabelExpr::Name("KNOWS".into())))),
        )
    }

    fn wildcard_expand() -> PlanOp {
        expand_with(
            EdgeDirection::PointingRight,
            None,
            Some(LabelExpr::Wildcard),
        )
    }

    #[test]
    fn shortest_path_demands_traversal_rows() {
        let reqs = extract_ops(vec![PlanOp::ShortestPath {
            src: "a".into(),
            dst: "b".into(),
            edge: "__sp".into(),
            path_var: None,
            emit_edge_binding: false,
            emit_path_binding: false,
            mode: gleaph_gql_planner::plan::ShortestMode::AnyShortest,
            direction: EdgeDirection::PointingRight,
            label: node_label_of_edge("KNOWS"),
            label_expr: None,
            var_len: None,
            cost: gleaph_gql_planner::plan::ShortestPathCost::HopCount,
        }]);
        let demands = reqs.graphs.get(&GRAPH_RAW).unwrap();
        assert_eq!(
            demands.conjunctive,
            vec![traverse_row(Some(Direction::Outgoing), 10)]
        );
        // Reverse shortest paths demand the reverse row.
        let reverse = extract_ops(vec![PlanOp::ShortestPath {
            src: "a".into(),
            dst: "b".into(),
            edge: "__sp".into(),
            path_var: None,
            emit_edge_binding: false,
            emit_path_binding: false,
            mode: gleaph_gql_planner::plan::ShortestMode::AnyShortest,
            direction: EdgeDirection::PointingLeft,
            label: node_label_of_edge("KNOWS"),
            label_expr: None,
            var_len: None,
            cost: gleaph_gql_planner::plan::ShortestPathCost::HopCount,
        }]);
        assert_eq!(
            reverse.graphs.get(&GRAPH_RAW).unwrap().conjunctive,
            vec![traverse_row(Some(Direction::Incoming), 10)]
        );
    }

    #[test]
    fn use_graph_child_resolves_against_the_target_graph() {
        // The child segment resolves names against graph 9's catalogs; graph-7 rows do
        // not carry over.
        let child_scan = PlanOp::NodeScan {
            variable: "n".into(),
            label: node_label("Person"),
            property_projection: projection(&[]),
        };
        let reqs = extract(
            &PhysicalPlan::from_ops(vec![PlanOp::UseGraph {
                graph_name: vec!["other".into()],
                sub_plan: Some(vec![child_scan]),
            }]),
            graph(),
            &CrossGraphCatalog,
        );
        let demands = reqs.graphs.get(&9).expect("target-graph demands");
        assert_eq!(
            demands.conjunctive,
            vec![Privilege::Graph(GraphPrivilege {
                graph: 9,
                operation: GraphOperation::Match,
                resource: GraphResource::VertexLabel(1),
            })]
        );

        struct CrossGraphCatalog;
        impl PlanCatalogView for CrossGraphCatalog {
            fn graph_by_name(&self, name: &str) -> Option<GraphId> {
                (name == "other").then(|| GraphId::from_raw(9))
            }
            fn vertex_label(&self, graph: GraphId, name: &str) -> Option<u32> {
                ((graph.raw() == 9 && name == "Person")
                    || (graph.raw() == GRAPH_RAW && name == "Person"))
                    .then_some(1)
            }
            fn edge_label(&self, _graph: GraphId, _name: &str) -> Option<(u32, Option<bool>)> {
                None
            }
            fn property(&self, _graph: GraphId, _name: &str) -> Option<u32> {
                None
            }
            fn vector_index_source(
                &self,
                _graph: GraphId,
                _index_name: &str,
            ) -> Option<VectorIndexSource> {
                None
            }
        }
    }

    #[test]
    fn unresolvable_use_graph_target_is_tenancy_only() {
        let reqs = extract_ops(vec![PlanOp::UseGraph {
            graph_name: vec!["nowhere".into()],
            sub_plan: Some(vec![scan("n", Some("Person"), &[])]),
        }]);
        assert!(reqs.root_unattributed);
        assert!(!allowed(
            &reqs,
            &principal(7),
            &grants_state_with_all_listed_rows(),
            &[]
        ));
        assert!(allowed(
            &reqs,
            &principal(7),
            &grants_state_with_all_listed_rows(),
            &[GRAPH_RAW]
        ));
    }

    // ───── ADR 0078 §1 layer 1: vector-search requirement extraction ─────

    fn vector_search_op(binding: &str, index_name: &str) -> PlanOp {
        use gleaph_gql_planner::plan::{SearchOutputKind, SearchOutputPlan, SearchProviderPlan};
        PlanOp::Search {
            binding: binding.into(),
            provider: SearchProviderPlan::VectorIndex {
                index_name: vec![Str::from(index_name)],
                query: Expr::new(ExprKind::Literal(gleaph_gql::Value::Bytes(vec![0; 4]))),
                limit: Expr::int(10),
                filter: None,
            },
            output: SearchOutputPlan {
                kind: SearchOutputKind::Distance,
                alias: "distance".into(),
            },
        }
    }

    /// Catalog extended with `doc_vec`: one-label span (Person=1) whose embedding name is
    /// backed by the projected graph property age=20.
    fn catalog_with_doc_vec() -> FakeCatalog {
        let mut c = catalog();
        c.vector_indexes.push((
            "doc_vec",
            VectorIndexSource {
                labels: vec![(1, "Person".to_owned())],
                embedding_property_id: Some(20),
            },
        ));
        c
    }

    fn extract_in(catalog: &FakeCatalog, ops: Vec<PlanOp>) -> RequirementSet {
        extract(&PhysicalPlan::from_ops(ops), graph(), catalog)
    }

    fn grant_reader_of_doc_vec(subject: GrantSubject) -> GrantTable {
        let mut grants = GrantTable::default();
        grants.vertex(subject, GraphOperation::Match, 1);
        grants.vertex_property(subject, 1, 20);
        grants
    }

    #[test]
    fn vector_search_demands_scan_equivalent_rows() {
        let reqs = extract_in(
            &catalog_with_doc_vec(),
            vec![vector_search_op("d", "doc_vec")],
        );
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(!demands.unattributed);
        assert_eq!(
            demands.conjunctive,
            vec![
                vertex_row(GRAPH_RAW, GraphOperation::Match, 1),
                vertex_property_row(GRAPH_RAW, 1, 20),
            ]
        );
    }

    #[test]
    fn vector_search_admission_matches_row_coverage_exactly() {
        let reqs = extract_in(
            &catalog_with_doc_vec(),
            vec![vector_search_op("d", "doc_vec")],
        );
        let reader = principal(2);

        // Both rows covered -> admitted...
        assert!(allowed(
            &reqs,
            &reader,
            &grant_reader_of_doc_vec(GrantSubject::Principal(reader)),
            &[]
        ));
        // ...MATCH alone is not (the embedding read is a separate demand)...
        let mut match_only = GrantTable::default();
        match_only.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 1);
        assert!(!allowed(&reqs, &reader, &match_only, &[]));
        // ...a different label's rows are not...
        let mut wrong_label = GrantTable::default();
        wrong_label.vertex(GrantSubject::Principal(reader), GraphOperation::Match, 2);
        wrong_label.vertex_property(GrantSubject::Principal(reader), 2, 20);
        assert!(!allowed(&reqs, &reader, &wrong_label, &[]));
        // ...and PUBLIC rows admit anonymous callers like an equivalent scan.
        assert!(allowed(
            &reqs,
            &Principal::anonymous(),
            &grant_reader_of_doc_vec(GrantSubject::Public),
            &[]
        ));

        // Wrong-implementation probe: an evaluator granting everything admits the
        // MATCH-only caller too, so the denial assertion above discriminates the real
        // evaluator from a stub.
        assert!(allowed(&reqs, &reader, &AlwaysAllow, &[]));
    }

    #[test]
    fn vector_search_multi_label_span_is_conjunctive() {
        let mut c = catalog();
        c.vector_indexes.push((
            "wide_vec",
            VectorIndexSource {
                labels: vec![(1, "Person".to_owned()), (2, "Employee".to_owned())],
                embedding_property_id: Some(21),
            },
        ));
        let reqs = extract_in(&c, vec![vector_search_op("d", "wide_vec")]);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert_eq!(
            demands.conjunctive,
            vec![
                vertex_row(GRAPH_RAW, GraphOperation::Match, 1),
                vertex_property_row(GRAPH_RAW, 1, 21),
                vertex_row(GRAPH_RAW, GraphOperation::Match, 2),
                vertex_property_row(GRAPH_RAW, 2, 21),
            ]
        );

        // A caller able to read only one spanned label is denied: per-candidate
        // visibility has no row-level filter, so the whole span must be readable.
        let half_reader = principal(8);
        let mut grants = GrantTable::default();
        grants.vertex(
            GrantSubject::Principal(half_reader),
            GraphOperation::Match,
            1,
        );
        grants.vertex_property(GrantSubject::Principal(half_reader), 1, 21);
        assert!(!allowed(&reqs, &half_reader, &grants, &[]));
        assert!(allowed(&reqs, &half_reader, &grants, &[GRAPH_RAW]));
    }

    #[test]
    fn vector_search_ingestion_fed_embedding_demands_no_property_row() {
        let mut c = catalog();
        c.vector_indexes.push((
            "fed_vec",
            VectorIndexSource {
                labels: vec![(1, "Person".to_owned())],
                embedding_property_id: None,
            },
        ));
        let reqs = extract_in(&c, vec![vector_search_op("d", "fed_vec")]);
        assert_eq!(
            reqs.graphs.get(&GRAPH_RAW).unwrap().conjunctive,
            vec![vertex_row(GRAPH_RAW, GraphOperation::Match, 1)]
        );
    }

    #[test]
    fn unknown_vector_index_name_is_tenancy_only() {
        let reqs = extract_in(&catalog(), vec![vector_search_op("d", "ghost_vec")]);
        let demands = reqs.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(demands.unattributed);
        assert!(!allowed(
            &reqs,
            &principal(2),
            &grant_reader_of_doc_vec(GrantSubject::Public),
            &[]
        ));
        assert!(allowed(
            &reqs,
            &principal(2),
            &grant_reader_of_doc_vec(GrantSubject::Public),
            &[GRAPH_RAW]
        ));
    }

    // ───── ADR 0078 §6: edge-subject (GLEAPH.VECTOR.*) visibility ─────
    //
    // Edge-inline byte vectors are fused fixed-label edge predicates evaluated during
    // ordinary traversal execution. Their subjects are edges, so the identical visibility
    // rule translates to: the matching edge privilege INCLUDING DIRECTION covers the
    // inline vector read, and an invisible edge never reaches candidate evaluation.

    /// Expand hop carrying a fused `GLEAPH.VECTOR.*` edge-inline vector predicate.
    fn vector_predicate_expand(direction: EdgeDirection, label: Option<&str>) -> PlanOp {
        use gleaph_gql_planner::plan::{EdgeVectorMetric, ScanValue};
        let mut op = expand(direction, label);
        if let PlanOp::Expand {
            edge_inline_vector_predicate,
            ..
        } = &mut op
        {
            *edge_inline_vector_predicate = Some(gleaph_gql_planner::plan::EdgeInlineVectorPredicate {
                metric: EdgeVectorMetric::L2Squared,
                query: ScanValue::Parameter("q".into()),
                op: gleaph_gql::ast::CmpOp::Le,
                threshold: ScanValue::Literal(gleaph_gql::Value::Float32(0.5)),
            });
        }
        op
    }

    #[test]
    fn edge_inline_vector_rides_the_direction_aware_traversal_row() {
        // The fused vector predicate adds NO extra demand and grants NO bypass: the
        // demanded rows are exactly the plain traversal rows for the same direction.
        for direction in [
            EdgeDirection::PointingRight,
            EdgeDirection::PointingLeft,
            EdgeDirection::AnyDirection,
        ] {
            let plain = extract_ops(vec![expand(direction, Some("KNOWS"))]);
            let fused = extract_ops(vec![vector_predicate_expand(direction, Some("KNOWS"))]);
            assert_eq!(
                fused, plain,
                "edge-inline vector predicate must ride the traversal row ({direction:?})"
            );

            // Direction-aware coverage: a caller holding only the OUTGOING row cannot run
            // the incoming shape, so its inline edge bytes stay invisible.
            let reader = principal(11);
            let mut grants = GrantTable::default();
            grants.traverse(
                GrantSubject::Principal(reader),
                Some(Direction::Outgoing),
                10,
            );
            let outgoing = matches!(direction, EdgeDirection::PointingRight);
            assert_eq!(
                allowed(&fused, &reader, &grants, &[]),
                outgoing,
                "direction-aware coverage must gate fused vector reads ({direction:?})"
            );
            // Wrong-implementation probe: an evaluator granting every direction admits
            // the incoming shape too, so the assertion above discriminates.
            assert!(allowed(&fused, &reader, &AlwaysAllow, &[]));
        }
    }

    #[test]
    fn edge_inline_vector_without_label_stays_tenancy_only() {
        // Unlabeled fused shapes cannot be attributed to a grantable resource: tenancy
        // gates them like any other wildcard traversal.
        let fused = extract_ops(vec![vector_predicate_expand(
            EdgeDirection::PointingRight,
            None,
        )]);
        let demands = fused.graphs.get(&GRAPH_RAW).expect("demands");
        assert!(demands.unattributed);
        assert!(!allowed(&fused, &principal(11), &GrantTable::default(), &[]));
        assert!(allowed(&fused, &principal(11), &GrantTable::default(), &[GRAPH_RAW]));
    }
}
