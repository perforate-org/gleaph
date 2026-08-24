//! GRANT/REVOKE execution on the Router control path ([ADR 0074] §5).
//!
//! The statements parse generically in `gleaph-gql` behind its `gleaph` feature; this
//! module is the integration layer that binds them to Router reality (ADR 0034):
//! registry ownership is the only grant authority for graph-scoped rows, `PRINCIPAL`
//! literals bind to IC principals, and label/property names resolve through the target
//! graph's catalogs. Every statement in a block is validated and lowered **before** the
//! first stable write, so no validation failure can leave a partially applied block.
//!
//! Slice 3 adds the prepared-query publication form (`EXECUTE ON PREPARED QUERY`,
//! ADR 0074 §1b): authority is the resolved graph's registry owner (implicit root,
//! invariant 3) or a `PREPARE_REGISTER` caps holder, and a GRANT additionally requires
//! the granter's effective privileges to cover every row of the record's statically
//! extracted requirement set (invariant 7: PUBLIC never exceeds its publisher).
//!
//! Grant rows are canonical `(subject × privilege)` facts owned by [`crate::facade::auth`]'s
//! grant state. Plan-time enforcement against these rows lives in [`crate::authz`].
//!
//! [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md

use candid::Principal;
use gleaph_auth::{
    AdminCaps, CompiledPredicate, Direction, GrantSubject, GraphOperation, GraphPrivilege,
    GraphResource, MAX_PREDICATE_CONJUNCTS, MetadataScope, PredicateComparison, PredicateLiteral,
    PredicateOp, PredicateValue, Privilege,
};
use gleaph_gql::ast::{
    CompositeQueryExpr, GrantCondition, GrantConditionSelector, GrantDirection, GrantMetadataScope,
    GrantPrivilege, GrantResourceSelector, GrantStatement, GrantSubjectLiteral, GrantTarget,
    GrantValueExpr, LinearQueryStatement, ProcedureBindingInitializer, RevokeStatement,
    SimpleQueryStatement, Statement, StatementBlock, ValueType,
};
use gleaph_graph_kernel::entry::{EdgeLabelId, GraphId, PropertyId, VertexLabelId};
use std::rc::Rc;

use crate::facade::auth;
use crate::facade::stable::graph_catalog;
use crate::facade::stable::graph_type_catalog::try_property_schema_for_graph_id;
use crate::facade::stable::prepared_catalog::{PreparedPlanKey, get_prepared_plan};
use crate::facade::store::RouterStore;
use crate::state::RouterError;
use crate::types::{
    ElevationScopeView, ElevationSummary, GrantDirectionView, GrantEvidenceView,
    GrantOperationView, GrantResourceKindView, GrantResourceView, GrantSubjectView,
    GraphGrantSummary,
};

// ──── Block detection ────

/// Whether every statement in `block` is pure GRANT/REVOKE content (`USE GRAPH` scoping
/// allowed) and at least one authorization statement is present — the shape the control
/// path executes without graph dispatch.
pub(crate) fn block_is_authorization_only(block: &StatementBlock) -> bool {
    let mut saw_authorization = false;
    for stmt in block.iter_statements() {
        let Statement::Query(query) = stmt else {
            return false;
        };
        let queries = std::iter::once(&query.left).chain(query.rest.iter().map(|(_, lq)| lq));
        for lq in queries {
            if !linear_is_authorization_only(lq) {
                return false;
            }
            saw_authorization |= linear_contains_authorization(lq);
        }
    }
    saw_authorization
}

/// Whether authorization statements appear anywhere in `block`, including mixed into
/// programs with other executable content (which must be rejected before dispatch).
pub(crate) fn block_contains_authorization_modification(block: &StatementBlock) -> bool {
    block.iter_statements().any(|stmt| match stmt {
        Statement::Query(query) => std::iter::once(&query.left)
            .chain(query.rest.iter().map(|(_, lq)| lq))
            .any(linear_contains_authorization),
        _ => false,
    })
}

fn linear_is_authorization_only(lq: &LinearQueryStatement) -> bool {
    lq.parts.iter().all(part_is_authorization_only)
}

fn linear_contains_authorization(lq: &LinearQueryStatement) -> bool {
    lq.parts.iter().any(part_contains_authorization)
        || lq
            .prefix_bindings
            .iter()
            .any(|binding| match &binding.initializer {
                ProcedureBindingInitializer::Query(q) => composite_contains_authorization(q),
                ProcedureBindingInitializer::Object(_) | ProcedureBindingInitializer::Expr(_) => {
                    false
                }
            })
}

fn composite_contains_authorization(expr: &CompositeQueryExpr) -> bool {
    linear_contains_authorization(&expr.left)
        || expr
            .rest
            .iter()
            .any(|(_, lq)| linear_contains_authorization(lq))
}

fn part_is_authorization_only(part: &SimpleQueryStatement) -> bool {
    match part {
        SimpleQueryStatement::Grant(_) | SimpleQueryStatement::Revoke(_) => true,
        SimpleQueryStatement::Focused { body, .. } => body
            .as_deref()
            .map(part_is_authorization_only)
            .unwrap_or(false),
        _ => false,
    }
}

fn part_contains_authorization(part: &SimpleQueryStatement) -> bool {
    match part {
        SimpleQueryStatement::Grant(_) | SimpleQueryStatement::Revoke(_) => true,
        SimpleQueryStatement::Focused { body, .. } => {
            body.as_deref().is_some_and(part_contains_authorization)
        }
        SimpleQueryStatement::InlineProcedureCall(ipc) => {
            composite_contains_authorization(&ipc.body)
        }
        _ => false,
    }
}

// ──── Execution ────

/// One fully validated statement, ready to apply.
enum PlannedAuthorization {
    Grant {
        subject: GrantSubject,
        privileges: Vec<Privilege>,
        /// Compiled conditional-policy predicate ([ADR 0075] §1), attached to every
        /// row this statement lowers to (one logical rule may normalize into several
        /// rows, e.g. a `READ` with a property list).
        predicate: Option<Rc<CompiledPredicate>>,
    },
    Revoke {
        subject: GrantSubject,
        privileges: Vec<Privilege>,
    },
}

/// Execute an authorization-only statement block: validate and lower every statement,
/// then apply all writes. Validation completes strictly before the first mutation, so a
/// rejected block leaves zero rows behind.
pub(crate) fn execute_authorization_block(
    block: &StatementBlock,
    caller: Principal,
) -> Result<(), RouterError> {
    let mut planned = Vec::new();
    {
        let store = RouterStore::new();
        for stmt in block.iter_statements() {
            let Statement::Query(query) = stmt else {
                return Err(RouterError::Internal(
                    "non-query statement reached authorization execution".into(),
                ));
            };
            let queries = std::iter::once(&query.left).chain(query.rest.iter().map(|(_, lq)| lq));
            for lq in queries {
                for part in &lq.parts {
                    if let Some(plan) = plan_part(part, &store, caller)? {
                        planned.push(plan);
                    }
                }
            }
        }
    }
    // Past this point no failure is possible: every statement was validated above and
    // nothing else writes within one message.
    for action in planned {
        apply(action)?;
    }
    Ok(())
}

fn plan_part(
    part: &SimpleQueryStatement,
    store: &RouterStore,
    caller: Principal,
) -> Result<Option<PlannedAuthorization>, RouterError> {
    match part {
        SimpleQueryStatement::Grant(stmt) => plan_grant(store, caller, stmt).map(Some),
        SimpleQueryStatement::Revoke(stmt) => plan_revoke(store, caller, stmt).map(Some),
        SimpleQueryStatement::Focused {
            body: Some(inner), ..
        } => plan_part(inner, store, caller),
        // Only reachable via `Focused` bodies; mixed content was rejected by
        // `block_is_authorization_only` before execution.
        _ => Ok(None),
    }
}

/// Resolve the graph and enforce the Phase-1 authority rule: registry owner only
/// (ADR 0074 §5). Non-tenants receive the ADR 0028 indistinguishable `NotFound`.
/// Returns `(graph_id, owner)` so introspection can synthesize the owner marker.
fn authorize_graph_owner(
    store: &RouterStore,
    graph_name: &str,
    caller: Principal,
) -> Result<(GraphId, Principal), RouterError> {
    let entry = store.get_graph_operator(graph_name, caller)?;
    if entry.owner != caller {
        return Err(RouterError::Forbidden);
    }
    Ok((entry.graph_id, entry.owner))
}

fn bind_subject(literal: &GrantSubjectLiteral) -> Result<GrantSubject, RouterError> {
    match literal {
        GrantSubjectLiteral::Public => Ok(GrantSubject::Public),
        GrantSubjectLiteral::Principal(text) => {
            let principal = Principal::from_text(text).map_err(|_| {
                RouterError::InvalidArgument(format!("invalid principal literal '{text}'"))
            })?;
            if principal == Principal::anonymous() {
                return Err(RouterError::InvalidArgument(
                    "the anonymous principal cannot hold a stored grant; grant PUBLIC instead"
                        .into(),
                ));
            }
            Ok(GrantSubject::Principal(principal))
        }
    }
}

/// Catalog-resolved selector of one statement.
enum ResolvedResource {
    VertexLabel(u32),
    EdgeLabel {
        id: u32,
        /// From the graph type schema: `Some(true)` UNDIRECTED, `Some(false)` DIRECTED,
        /// `None` undeclared (open graph or label absent from the binding).
        undirected: Option<bool>,
    },
}

fn resolve_resource(
    store: &RouterStore,
    graph_id: GraphId,
    resource: &GrantResourceSelector,
) -> Result<ResolvedResource, RouterError> {
    match resource {
        GrantResourceSelector::Vertex { label } => Ok(ResolvedResource::VertexLabel(u32::from(
            store.lookup_vertex_label_id(graph_id, label)?.raw(),
        ))),
        GrantResourceSelector::Edge { label } => {
            let id = store.lookup_edge_label_id(graph_id, label)?;
            // Directedness comes from the graph type schema (`PropertySchema` trait).
            use gleaph_gql::type_check::PropertySchema as _;
            let undirected = try_property_schema_for_graph_id(graph_id)?
                .and_then(|schema| schema.edge_is_undirected(label));
            Ok(ResolvedResource::EdgeLabel {
                id: u32::from(id.raw()),
                undirected,
            })
        }
    }
}

fn plan_grant(
    store: &RouterStore,
    caller: Principal,
    stmt: &GrantStatement,
) -> Result<PlannedAuthorization, RouterError> {
    match &stmt.target {
        GrantTarget::Graph {
            privilege,
            graph,
            resource,
            condition,
        } => {
            let graph_name = gleaph_graph_catalog::object_name_key(graph);
            let (graph_id, _) = authorize_graph_owner(store, &graph_name, caller)?;
            let subject = bind_subject(&stmt.subject)?;
            let resource = resolve_resource(store, graph_id, resource)?;
            let property_ids = resolve_property_ids(store, graph_id, privilege)?;
            let predicate = match condition {
                None => None,
                Some(condition) => Some(compile_condition(
                    store, graph_id, privilege, &resource, condition,
                )?),
            };
            let privileges = lower_privileges(graph_id.raw(), privilege, &resource, &property_ids)?;
            Ok(PlannedAuthorization::Grant {
                subject,
                privileges,
                predicate,
            })
        }
        GrantTarget::PreparedQuery { name } => {
            let (subject, privilege) =
                plan_prepared_publication(store, caller, name, &stmt.subject, true)?;
            Ok(PlannedAuthorization::Grant {
                subject,
                privileges: vec![privilege],
                predicate: None,
            })
        }
        GrantTarget::Metadata { scope } => {
            // Grammar-written metadata rows ([ADR 0080] §5) are standing grants: the
            // windowed, evidence-complete form flows exclusively through
            // `elevate_request`/`elevate_approve`. Authority mirrors the publication
            // family — the named graph's registry owner (implicit root over their own
            // graph's plane) or `MANAGE_AUTHORIZATION`; cross-graph `CONTROL PLANE`
            // rows have no owner and are cap-gated.
            let subject = bind_subject(&stmt.subject)?;
            let resolved = resolve_metadata_scope(store, scope)?;
            authorize_metadata_grant_authority(&caller, &resolved)?;
            Ok(PlannedAuthorization::Grant {
                subject,
                privileges: vec![Privilege::Metadata(resolved)],
                predicate: None,
            })
        }
    }
}

fn plan_revoke(
    store: &RouterStore,
    caller: Principal,
    stmt: &RevokeStatement,
) -> Result<PlannedAuthorization, RouterError> {
    match &stmt.target {
        GrantTarget::Graph {
            privilege,
            graph,
            resource,
            // The conditional selector is accepted syntactically but intentionally not
            // compiled here: revocation addresses the canonical `(privilege, subject)`
            // row — at most one row per key exists, so its condition cannot select
            // among rows ([ADR 0075] §1: REVOKE removes rule and condition together).
            // Re-resolving catalogs for the condition text could only reject a valid
            // revoke after vocabulary drift.
            condition: _,
        } => {
            let graph_name = gleaph_graph_catalog::object_name_key(graph);
            let (graph_id, _) = authorize_graph_owner(store, &graph_name, caller)?;
            let subject = bind_subject(&stmt.subject)?;
            let resource = resolve_resource(store, graph_id, resource)?;
            let property_ids = resolve_property_ids(store, graph_id, privilege)?;
            let privileges = lower_privileges(graph_id.raw(), privilege, &resource, &property_ids)?;
            // Revoke preflight (read-only): every lowered row must exist, else nothing is removed
            // and the exact missing key is reported. Expired-but-stored rows still count as
            // present (`contains`, not `holds`) because revoke addresses stored state.
            for privilege in &privileges {
                if !auth::grant_contains(subject, privilege) {
                    return Err(RouterError::NotFound(format!(
                        "no stored grant row for {}",
                        privilege_key_description(subject, privilege)
                    )));
                }
            }
            Ok(PlannedAuthorization::Revoke {
                subject,
                privileges,
            })
        }
        GrantTarget::PreparedQuery { name } => {
            // Revocation is symmetric with publication authority but never gated by
            // invariant 7: removing a row cannot widen any subject's privileges.
            let (subject, privilege) =
                plan_prepared_publication(store, caller, name, &stmt.subject, false)?;
            if !auth::grant_contains(subject, &privilege) {
                return Err(RouterError::NotFound(format!(
                    "no stored grant row for {}",
                    privilege_key_description(subject, &privilege)
                )));
            }
            Ok(PlannedAuthorization::Revoke {
                subject,
                privileges: vec![privilege],
            })
        }
        GrantTarget::Metadata { scope } => {
            let subject = bind_subject(&stmt.subject)?;
            let resolved = resolve_metadata_scope(store, scope)?;
            authorize_metadata_grant_authority(&caller, &resolved)?;
            let privilege = Privilege::Metadata(resolved);
            if !auth::grant_contains(subject, &privilege) {
                return Err(RouterError::NotFound(format!(
                    "no stored grant row for {}",
                    privilege_key_description(subject, &privilege)
                )));
            }
            Ok(PlannedAuthorization::Revoke {
                subject,
                privileges: vec![privilege],
            })
        }
    }
}

/// Resolve a parsed metadata scope against Router reality ([ADR 0080] §1): the graph
/// name must exist (ADR 0028 non-disclosure applies), `CONTROL PLANE` is name-free.
fn resolve_metadata_scope(
    store: &RouterStore,
    scope: &GrantMetadataScope,
) -> Result<MetadataScope, RouterError> {
    match scope {
        GrantMetadataScope::Graph(name) => {
            let graph_name = gleaph_graph_catalog::object_name_key(name);
            let graph_id = store.resolve_graph_id(&graph_name)?;
            Ok(MetadataScope::Graph(graph_id.raw()))
        }
        GrantMetadataScope::ControlPlane => Ok(MetadataScope::ControlPlane),
    }
}

/// Authority over grammar-written metadata rows ([ADR 0080] §5): the named graph's
/// registry owner or a `MANAGE_AUTHORIZATION` holder for graph scopes;
/// `MANAGE_AUTHORIZATION` only for the owner-less cross-graph scope. Evaluated before
/// any write like every other authorization gate.
fn authorize_metadata_grant_authority(
    caller: &Principal,
    resolved: &MetadataScope,
) -> Result<(), RouterError> {
    match resolved {
        MetadataScope::Graph(graph_raw) => {
            if auth::has_cap(caller, AdminCaps::MANAGE_AUTHORIZATION) {
                return Ok(());
            }
            match graph_catalog::graph_entry(GraphId::from_raw(*graph_raw)) {
                Some(entry) if entry.owner == *caller => Ok(()),
                // A visible non-owner learns only the uniform denial; an invisible
                // caller never reached the name resolution above.
                _ => Err(RouterError::Forbidden),
            }
        }
        MetadataScope::ControlPlane => auth::require_cap(caller, AdminCaps::MANAGE_AUTHORIZATION),
    }
}

/// Plan one `EXECUTE ON PREPARED QUERY` grant/revoke (ADR 0074 §1b).
///
/// Gates, all evaluated before the single row write:
///
/// 1. The prepared record must exist under its Router-global canonical name.
/// 2. Authority: the caller owns the query's resolved bound graph — ownership is the
///    implicit root of data-plane authority (ADR 0074 §3 invariant 3) — or holds
///    `PREPARE_REGISTER` (the registration capability). Graph admins are not publishers.
/// 3. When `invariant_7_gate` is set (GRANT only): the granter's effective privileges
///    (`caller ∪ PUBLIC ∪ ownership`) must cover every row of the record's stored static
///    requirement set — PUBLIC never exceeds its publisher. The check reuses the same
///    evaluation as plan-time enforcement; a granter missing even one requirement is
///    denied with the uniform non-disclosing [`RouterError::Forbidden`].
fn plan_prepared_publication(
    store: &RouterStore,
    caller: Principal,
    name: &str,
    subject_literal: &GrantSubjectLiteral,
    invariant_7_gate: bool,
) -> Result<(GrantSubject, Privilege), RouterError> {
    let record = get_prepared_plan(&PreparedPlanKey::new(name))
        .ok_or_else(|| RouterError::NotFound(format!("prepared query {name:?}")))?;
    let v1 = record.as_v1();
    let entry = graph_catalog::graph_entry(v1.graph_id)
        .ok_or_else(|| RouterError::NotFound(format!("prepared query {name:?}")))?;
    if entry.owner != caller && !auth::has_cap(&caller, AdminCaps::PREPARE_REGISTER) {
        return Err(RouterError::Forbidden);
    }
    if invariant_7_gate
        && !crate::authz::requirements_cover(
            &v1.required_privileges,
            v1.graph_id.raw(),
            &caller,
            store,
        )
    {
        return Err(RouterError::Forbidden);
    }
    let subject = bind_subject(subject_literal)?;
    Ok((
        subject,
        Privilege::ExecutePreparedQuery {
            name: name.to_owned(),
        },
    ))
}

fn resolve_property_ids(
    store: &RouterStore,
    graph_id: GraphId,
    privilege: &GrantPrivilege,
) -> Result<Vec<u32>, RouterError> {
    let GrantPrivilege::Read { properties } = privilege else {
        return Ok(Vec::new());
    };
    properties
        .iter()
        .map(|name| Ok(store.lookup_property_id(graph_id, name)?.raw()))
        .collect()
}

// ──── Conditional policy compilation (ADR 0075 §1–§2) ────

/// Compile one parsed conditional selector into the canonical stored predicate.
///
/// Catalog-checked at GRANT time ([ADR 0075] §2): the selector must be the vertex form
/// matching the granted label, every comparison property must exist in the graph's
/// property catalog, and literal kinds must be compatible with the property's declared
/// scalar type (undeclared properties stay open-world). Pure over its resolved inputs
/// so failures happen strictly before any stable write.
fn compile_condition(
    store: &RouterStore,
    graph_id: GraphId,
    privilege: &GrantPrivilege,
    resource: &ResolvedResource,
    condition: &GrantCondition,
) -> Result<Rc<CompiledPredicate>, RouterError> {
    // Phase-2a lowering binds conditions to vertex scans; edge-form selectors are a
    // recognized-but-deferred surface with their own distinct error.
    let GrantConditionSelector::Vertex { variable: _, label } = &condition.selector else {
        return Err(RouterError::InvalidArgument(
            "conditional policies bind vertex labels in this phase; \
             FOR ()-[e:Label]-() selectors are not supported yet"
                .into(),
        ));
    };
    if !matches!(
        privilege,
        GrantPrivilege::Match | GrantPrivilege::Read { .. }
    ) {
        return Err(RouterError::InvalidArgument(
            "conditional policies apply only to MATCH or READ grants in this phase".into(),
        ));
    }
    // A vertex-form selector paired with an edge resource cannot bind.
    let ResolvedResource::VertexLabel(label_raw) = resource else {
        return Err(RouterError::InvalidArgument(
            "conditional selectors require a NODES/VERTICES resource; \
             edge-label grants stay unconditional in this phase"
                .into(),
        ));
    };
    let Ok(label_u16) = u16::try_from(*label_raw) else {
        return Err(RouterError::Internal(
            "conditional selector label id exceeds catalog space".into(),
        ));
    };
    let label_name = store.reverse_vertex_label_name(
        graph_id,
        gleaph_graph_kernel::entry::VertexLabelId::from_raw(label_u16),
    )?;
    if label != &label_name {
        return Err(RouterError::InvalidArgument(format!(
            "conditional selector label '{label}' does not match the granted label \
             '{label_name}'"
        )));
    }

    // Declared scalar types for this label, for compatibility checking. Open graphs or
    // labels absent from the binding keep the open world: any literal kind is accepted.
    use gleaph_gql::type_check::PropertySchema as _;
    let declared: std::collections::BTreeMap<String, ValueType> =
        try_property_schema_for_graph_id(graph_id)?
            .map(|schema| {
                schema
                    .node_property_types(std::slice::from_ref(&label_name))
                    .into_iter()
                    .map(|(name, value_type, _required)| (name, value_type))
                    .collect()
            })
            .unwrap_or_default();

    let conjunct_count = condition.predicate.conjuncts.len();
    if conjunct_count == 0 || conjunct_count > MAX_PREDICATE_CONJUNCTS {
        return Err(RouterError::InvalidArgument(format!(
            "conditional policy conjunction must hold 1..={MAX_PREDICATE_CONJUNCTS} comparisons"
        )));
    }

    let mut conjuncts = Vec::with_capacity(conjunct_count);
    for comparison in &condition.predicate.conjuncts {
        let property_id = store
            .lookup_property_id(graph_id, &comparison.property)?
            .raw();
        let declared_type = declared.get(&comparison.property);
        let value = match &comparison.value {
            GrantValueExpr::MsgCaller => {
                check_literal_compatibility(
                    declared_type,
                    &PredicateValue::MsgCaller,
                    &comparison.property,
                )?;
                PredicateValue::MsgCaller
            }
            GrantValueExpr::Literal(value) => {
                let literal = literal_to_predicate_literal(value, &comparison.property)?;
                let value = PredicateValue::Literal(literal);
                check_literal_compatibility(declared_type, &value, &comparison.property)?;
                value
            }
        };
        conjuncts.push(PredicateComparison {
            property: property_id,
            op: predicate_op(comparison.op),
            value,
        });
    }
    Ok(Rc::new(CompiledPredicate {
        label: *label_raw,
        conjuncts,
    }))
}

/// The resolved resource a conditional selector compiles against; always the statement's
/// vertex resource (callers guarantee the selector is the vertex form).
fn predicate_op(op: gleaph_gql::ast::CmpOp) -> PredicateOp {
    match op {
        gleaph_gql::ast::CmpOp::Eq => PredicateOp::Eq,
        gleaph_gql::ast::CmpOp::Ne => PredicateOp::Ne,
        gleaph_gql::ast::CmpOp::Lt => PredicateOp::Lt,
        gleaph_gql::ast::CmpOp::Le => PredicateOp::Le,
        gleaph_gql::ast::CmpOp::Gt => PredicateOp::Gt,
        gleaph_gql::ast::CmpOp::Ge => PredicateOp::Ge,
    }
}

/// Converts an AST literal into the canonical predicate literal, enforcing the string
/// encoding bound of the stable grant-row format.
fn literal_to_predicate_literal(
    value: &gleaph_gql::Value,
    property: &str,
) -> Result<PredicateLiteral, RouterError> {
    match value {
        gleaph_gql::Value::Bool(b) => Ok(PredicateLiteral::Bool(*b)),
        gleaph_gql::Value::Int64(i) => Ok(PredicateLiteral::Int(*i)),
        gleaph_gql::Value::Float64(f) => Ok(PredicateLiteral::Float(*f)),
        gleaph_gql::Value::Text(text) => {
            if text.len() > 15 {
                return Err(RouterError::InvalidArgument(format!(
                    "string literal for conditional policy on {property:?} exceeds the \
                     15-byte bound ({text:?})"
                )));
            }
            Ok(PredicateLiteral::String(text.clone()))
        }
        other => Err(RouterError::InvalidArgument(format!(
            "unsupported literal for conditional policy on {property:?}: {other:?}; \
             conditional policies compare Bool, integer, float, and short string scalars"
        ))),
    }
}

/// Scalar-type compatibility between a literal and the property's declared type
/// ([ADR 0075] §2). Undeclared properties are open-world and accept everything.
fn check_literal_compatibility(
    declared: Option<&ValueType>,
    value: &PredicateValue,
    property: &str,
) -> Result<(), RouterError> {
    let Some(value_type) = declared else {
        return Ok(());
    };
    let compatible = |class: fn(&ValueType) -> bool| class(value_type);
    let ok = match value {
        PredicateValue::MsgCaller => is_character_string_type(value_type),
        PredicateValue::Literal(PredicateLiteral::Bool(_)) => compatible(is_boolean_type),
        PredicateValue::Literal(PredicateLiteral::Int(_)) => compatible(is_exact_integer_type),
        PredicateValue::Literal(PredicateLiteral::Float(_)) => {
            compatible(is_approximate_or_decimal_type)
        }
        PredicateValue::Literal(PredicateLiteral::String(_)) => {
            compatible(is_character_string_type)
        }
    };
    if ok {
        return Ok(());
    }
    Err(RouterError::InvalidArgument(format!(
        "conditional policy literal for {property:?} is incompatible with the property's \
         declared type"
    )))
}

fn is_boolean_type(value_type: &ValueType) -> bool {
    matches!(value_type, ValueType::Bool { .. })
}

fn is_character_string_type(value_type: &ValueType) -> bool {
    matches!(
        value_type,
        ValueType::String { .. } | ValueType::Char { .. } | ValueType::Varchar { .. }
    )
}

fn is_exact_integer_type(value_type: &ValueType) -> bool {
    matches!(
        value_type,
        ValueType::Int8 { .. }
            | ValueType::Int16 { .. }
            | ValueType::Int32 { .. }
            | ValueType::Int64 { .. }
            | ValueType::IntPrecision { .. }
            | ValueType::Int128 { .. }
            | ValueType::Int256 { .. }
            | ValueType::Uint8 { .. }
            | ValueType::Uint16 { .. }
            | ValueType::Uint32 { .. }
            | ValueType::Uint64 { .. }
            | ValueType::UintPrecision { .. }
            | ValueType::Uint128 { .. }
            | ValueType::Uint256 { .. }
    )
}

fn is_approximate_or_decimal_type(value_type: &ValueType) -> bool {
    matches!(
        value_type,
        ValueType::Float16 { .. }
            | ValueType::Float32 { .. }
            | ValueType::Float64 { .. }
            | ValueType::Float128
            | ValueType::Float256
            | ValueType::FloatPrecision { .. }
            | ValueType::Decimal { .. }
    )
}

/// Lower one validated AST privilege onto canonical storage rows ([ADR 0074] §2).
///
/// - A vertex `READ` with a property list yields one `READ` row plus one `READ_PROPERTY`
///   row per listed property.
/// - An omitted direction on a declared DIRECTED edge label normalizes to BOTH directional
///   rows (evaluation probes one direction at a time); undirected or undeclared labels
///   reject directional modifiers and store the unoriented form.
///
/// Pure over its inputs so catalog resolution stays outside.
fn lower_privileges(
    graph: u32,
    privilege: &GrantPrivilege,
    resource: &ResolvedResource,
    property_ids: &[u32],
) -> Result<Vec<Privilege>, RouterError> {
    fn op(graph: u32, operation: GraphOperation, resource: GraphResource) -> Privilege {
        Privilege::Graph(GraphPrivilege {
            graph,
            operation,
            resource,
        })
    }
    match privilege {
        GrantPrivilege::Match => Ok(vec![op(
            graph,
            GraphOperation::Match,
            whole_label_resource(resource)?,
        )]),
        GrantPrivilege::Create => Ok(vec![op(
            graph,
            GraphOperation::Create,
            whole_label_resource(resource)?,
        )]),
        GrantPrivilege::Update => Ok(vec![op(
            graph,
            GraphOperation::Update,
            whole_label_resource(resource)?,
        )]),
        GrantPrivilege::Delete => Ok(vec![op(
            graph,
            GraphOperation::Delete,
            whole_label_resource(resource)?,
        )]),
        GrantPrivilege::Read { .. } => {
            let vertex_label = match resource {
                ResolvedResource::VertexLabel(id) => *id,
                ResolvedResource::EdgeLabel { .. } => {
                    return Err(RouterError::InvalidArgument(
                        "property lists attach only to vertex selectors".into(),
                    ));
                }
            };
            let mut out = vec![op(
                graph,
                GraphOperation::Read,
                GraphResource::VertexLabel(vertex_label),
            )];
            for property in property_ids {
                out.push(op(
                    graph,
                    GraphOperation::ReadProperty,
                    GraphResource::VertexProperty {
                        label: vertex_label,
                        property: *property,
                    },
                ));
            }
            Ok(out)
        }
        GrantPrivilege::Traverse { direction } => match resource {
            ResolvedResource::VertexLabel(id) => {
                if direction.is_some() {
                    return Err(RouterError::InvalidArgument(
                        "directional modifiers apply only to edge selectors".into(),
                    ));
                }
                Ok(vec![op(
                    graph,
                    GraphOperation::Traverse(None),
                    GraphResource::VertexLabel(*id),
                )])
            }
            ResolvedResource::EdgeLabel { id, undirected } => {
                let res = GraphResource::EdgeLabel(*id);
                match (*undirected, direction) {
                    // Declared DIRECTED: an omitted modifier grants BOTH directions, stored
                    // as two directional rows so evaluation probes each exactly.
                    (Some(false), None) => Ok(vec![
                        op(
                            graph,
                            GraphOperation::Traverse(Some(Direction::Outgoing)),
                            res,
                        ),
                        op(
                            graph,
                            GraphOperation::Traverse(Some(Direction::Incoming)),
                            res,
                        ),
                    ]),
                    (Some(false), Some(ast_direction)) => Ok(vec![op(
                        graph,
                        GraphOperation::Traverse(Some(auth_direction(*ast_direction))),
                        res,
                    )]),
                    // Declared UNDIRECTED: directional modifiers are meaningless and rejected.
                    (Some(true), Some(_)) => Err(RouterError::InvalidArgument(
                        "edge label is declared UNDIRECTED; TRAVERSE OUTGOING/INCOMING are invalid"
                            .into(),
                    )),
                    (Some(true), None) | (None, None) => {
                        Ok(vec![op(graph, GraphOperation::Traverse(None), res)])
                    }
                    // Undeclared directedness: fail closed rather than guess.
                    (None, Some(_)) => Err(RouterError::InvalidArgument(
                        "directedness of the edge label is not declared by the graph schema; \
                         directional traversal cannot be granted"
                            .into(),
                    )),
                }
            }
        },
    }
}

fn whole_label_resource(resource: &ResolvedResource) -> Result<GraphResource, RouterError> {
    match resource {
        ResolvedResource::VertexLabel(id) => Ok(GraphResource::VertexLabel(*id)),
        ResolvedResource::EdgeLabel { id, .. } => Ok(GraphResource::EdgeLabel(*id)),
    }
}

fn auth_direction(direction: GrantDirection) -> Direction {
    match direction {
        GrantDirection::Outgoing => Direction::Outgoing,
        GrantDirection::Incoming => Direction::Incoming,
    }
}

fn apply(action: PlannedAuthorization) -> Result<(), RouterError> {
    match action {
        PlannedAuthorization::Grant {
            subject,
            privileges,
            predicate,
        } => {
            for privilege in &privileges {
                auth::add_grant(subject, privilege, None, predicate.clone())
                    .map_err(|err| RouterError::Internal(format!("grant write rejected: {err}")))?;
            }
            Ok(())
        }
        PlannedAuthorization::Revoke {
            subject,
            privileges,
        } => {
            for privilege in &privileges {
                if !auth::remove_grant(subject, privilege) {
                    // Unreachable: the planning phase checked every row exists and no other
                    // write runs between planning and application within one message.
                    return Err(RouterError::Internal(format!(
                        "revoked row vanished mid-transaction: {}",
                        privilege_key_description(subject, privilege)
                    )));
                }
            }
            Ok(())
        }
    }
}

// ──── Introspection ────

/// Owner-or-caps-authority gate for grant introspection ([ADR 0074] §5 observability,
/// review stage of [ADR 0080] §3). The registry owner always sees their graph's rows; a
/// `MANAGE_AUTHORIZATION` holder sees them too because reviewing elevation evidence is
/// the governing capability's duty. Invisible callers keep the ADR 0028
/// indistinguishable `NotFound` (enforced by `get_graph_operator`).
///
/// This is deliberately separate from [`authorize_graph_owner`]: writing rows stays
/// owner/cap-gated per statement kind, while reading authorization state is the broader
/// review audience.
fn authorize_graph_introspection(
    store: &RouterStore,
    graph_name: &str,
    caller: Principal,
) -> Result<GraphId, RouterError> {
    let entry = store.get_graph_operator(graph_name, caller)?;
    if entry.owner == caller || auth::has_cap(&caller, AdminCaps::MANAGE_AUTHORIZATION) {
        Ok(entry.graph_id)
    } else {
        Err(RouterError::Forbidden)
    }
}

/// Evidence projection of one stored row.
fn evidence_view(evidence: &Option<gleaph_auth::ElevationEvidence>) -> Option<GrantEvidenceView> {
    evidence.as_ref().map(|e| GrantEvidenceView {
        approver: Some(e.approver.to_text()),
        justification: Some(e.justification.clone()),
        emergency: e.emergency,
    })
}

/// Listing of one graph's stored grant rows (ADR 0074 §5 observability), including the
/// graph-scoped metadata elevation rows ([ADR 0080] §4).
///
/// The first entry is always the synthesized implicit-root marker of the registry owner
/// (ADR 0074 §3 invariant 3): ownership is the root of data-plane authority but is never
/// materialized as a grant row, so introspection surfaces it explicitly instead of
/// presenting an apparently empty list. Stored rows follow in canonical key order.
///
/// Viewers: the registry owner plus `MANAGE_AUTHORIZATION` holders (the elevation review
/// audience). Non-tenants receive `NotFound` (ADR 0028 non-disclosure); visible
/// non-owners without the capability receive `Forbidden`.
///
/// Rows referencing vocabulary that no longer resolves are skipped rather than misnamed:
/// since the ADR 0074 §3 invariant-4 cascade landed, dropped-vocabulary rows are swept at
/// the drop site (`purge_graph_vocabulary_partitions`), so a skipped row here can only be
/// an unrepresentable or otherwise unresolved leftover, surfaced as absent.
pub(crate) fn list_graph_grants(
    graph_name: &str,
    caller: Principal,
) -> Result<Vec<GraphGrantSummary>, RouterError> {
    let store = RouterStore::new();
    let graph_id = authorize_graph_introspection(&store, graph_name, caller)?;
    let mut summaries = vec![GraphGrantSummary {
        subject: GrantSubjectView::Principal(
            store
                .get_graph_operator(graph_name, caller)?
                .owner
                .to_text(),
        ),
        operation: GrantOperationView::ImplicitRoot,
        direction: None,
        resource: GrantResourceView {
            kind: GrantResourceKindView::Graph,
            label: graph_name.to_owned(),
            property: None,
        },
        expires_at_ns: None,
        predicate: None,
        evidence: None,
    }];
    for row in auth::grant_rows() {
        match &row.privilege {
            Privilege::Metadata(MetadataScope::Graph(target)) => {
                if *target != graph_id.raw() {
                    continue;
                }
                summaries.push(GraphGrantSummary {
                    subject: subject_view(row.subject),
                    operation: GrantOperationView::ReadMetadata,
                    direction: None,
                    resource: GrantResourceView {
                        kind: GrantResourceKindView::Graph,
                        label: graph_name.to_owned(),
                        property: None,
                    },
                    expires_at_ns: row.expires_at_ns,
                    predicate: None,
                    evidence: evidence_view(&row.evidence),
                });
            }
            Privilege::Graph(gp) => {
                if gp.graph != graph_id.raw() {
                    continue;
                }
                let Some((operation, direction)) = operation_view(gp.operation) else {
                    continue;
                };
                let Some(resource) = resource_view(&store, graph_id, gp.resource) else {
                    continue;
                };
                // Inline condition text ([ADR 0075] §1): property names resolve through the
                // catalogs; unresolved ids print as `<property N>` so introspection stays
                // truthful after renames.
                let predicate = row.predicate.as_ref().map(|compiled| {
                    compiled.display_conditions(|property_id| {
                        store
                            .reverse_property_name(graph_id, PropertyId::from_raw(property_id))
                            .ok()
                    })
                });
                summaries.push(GraphGrantSummary {
                    subject: subject_view(row.subject),
                    operation,
                    direction,
                    resource,
                    expires_at_ns: row.expires_at_ns,
                    predicate,
                    evidence: evidence_view(&row.evidence),
                });
            }
            // Prepared-query EXECUTE rows and cross-graph scopes list through their own
            // surfaces (`list_prepared` / `list_elevations`).
            Privilege::ExecutePreparedQuery { .. }
            | Privilege::Metadata(MetadataScope::ControlPlane) => {}
        }
    }
    Ok(summaries)
}

/// All stored metadata elevation rows ([ADR 0080] §3–§4), active and recently-expired,
/// in canonical key order — the caps-gated review surface. Expired rows remain stored
/// until GC, so post-use review sees them flagged inactive.
pub(crate) fn list_elevations(caller: Principal) -> Result<Vec<ElevationSummary>, RouterError> {
    auth::require_cap(&caller, AdminCaps::MANAGE_AUTHORIZATION)?;
    let now_ns = crate::facade::store::ic_time_ns();
    Ok(auth::grant_rows()
        .into_iter()
        .filter_map(|row| match row.privilege {
            Privilege::Metadata(scope) => {
                let scope_view = match scope {
                    MetadataScope::Graph(graph_raw) => ElevationScopeView::Graph(
                        graph_catalog::graph_name(GraphId::from_raw(graph_raw))
                            .unwrap_or_else(|| format!("<graph {graph_raw}>")),
                    ),
                    MetadataScope::ControlPlane => ElevationScopeView::ControlPlane,
                };
                row.expires_at_ns.map(|expires_at_ns| ElevationSummary {
                    requester: subject_view(row.subject),
                    scope: scope_view,
                    expires_at_ns,
                    active: expires_at_ns >= now_ns,
                    evidence: evidence_view(&row.evidence),
                })
            }
            _ => None,
        })
        .collect())
}

fn operation_view(
    operation: GraphOperation,
) -> Option<(GrantOperationView, Option<GrantDirectionView>)> {
    match operation {
        GraphOperation::Match => Some((GrantOperationView::Match, None)),
        GraphOperation::Traverse(Some(Direction::Outgoing)) => Some((
            GrantOperationView::Traverse,
            Some(GrantDirectionView::Outgoing),
        )),
        GraphOperation::Traverse(Some(Direction::Incoming)) => Some((
            GrantOperationView::Traverse,
            Some(GrantDirectionView::Incoming),
        )),
        GraphOperation::Traverse(None) => Some((GrantOperationView::Traverse, None)),
        GraphOperation::Read => Some((GrantOperationView::Read, None)),
        GraphOperation::ReadProperty => Some((GrantOperationView::ReadProperty, None)),
        GraphOperation::Create => Some((GrantOperationView::Create, None)),
        GraphOperation::Update => Some((GrantOperationView::Update, None)),
        GraphOperation::Delete => Some((GrantOperationView::Delete, None)),
    }
}

fn subject_view(subject: GrantSubject) -> GrantSubjectView {
    match subject {
        GrantSubject::Principal(p) => GrantSubjectView::Principal(p.to_text()),
        GrantSubject::Public => GrantSubjectView::Public,
    }
}

fn resource_view(
    store: &RouterStore,
    graph_id: GraphId,
    resource: GraphResource,
) -> Option<GrantResourceView> {
    /// Reverse-resolve a stored label id; `None` when the id exceeds the u16 catalog space
    /// (unrepresentable rows are skipped like unresolvable ones).
    fn label_raw(id: u32) -> Option<u16> {
        u16::try_from(id).ok()
    }
    match resource {
        GraphResource::VertexLabel(id) => Some(GrantResourceView {
            kind: GrantResourceKindView::Vertex,
            label: store
                .reverse_vertex_label_name(graph_id, VertexLabelId::from_raw(label_raw(id)?))
                .ok()?,
            property: None,
        }),
        GraphResource::EdgeLabel(id) => Some(GrantResourceView {
            kind: GrantResourceKindView::Edge,
            label: store
                .reverse_edge_label_name(graph_id, EdgeLabelId::from_raw(label_raw(id)?))
                .ok()?,
            property: None,
        }),
        GraphResource::VertexProperty { label, property } => Some(GrantResourceView {
            kind: GrantResourceKindView::Vertex,
            label: store
                .reverse_vertex_label_name(graph_id, VertexLabelId::from_raw(label_raw(label)?))
                .ok()?,
            property: Some(
                store
                    .reverse_property_name(graph_id, PropertyId::from_raw(property))
                    .ok()?,
            ),
        }),
    }
}

/// Canonical-key description of one stored privilege for exact revoke-miss errors.
fn privilege_key_description(subject: GrantSubject, privilege: &Privilege) -> String {
    let subject_text = match subject {
        GrantSubject::Public => "PUBLIC".to_owned(),
        GrantSubject::Principal(p) => format!("PRINCIPAL '{}'", p.to_text()),
    };
    match privilege {
        Privilege::ExecutePreparedQuery { name } => {
            format!("EXECUTE ON PREPARED QUERY {name} for subject {subject_text}")
        }
        Privilege::Metadata(MetadataScope::Graph(graph)) => {
            format!("READ_METADATA ON GRAPH <{graph}> for subject {subject_text}")
        }
        Privilege::Metadata(MetadataScope::ControlPlane) => {
            format!("READ_METADATA ON CONTROL PLANE for subject {subject_text}")
        }
        Privilege::Graph(gp) => {
            let operation = match gp.operation {
                GraphOperation::Match => "MATCH",
                GraphOperation::Traverse(Some(Direction::Outgoing)) => "TRAVERSE OUTGOING",
                GraphOperation::Traverse(Some(Direction::Incoming)) => "TRAVERSE INCOMING",
                GraphOperation::Traverse(None) => "TRAVERSE",
                GraphOperation::Read => "READ",
                GraphOperation::ReadProperty => "READ_PROPERTY",
                GraphOperation::Create => "CREATE",
                GraphOperation::Update => "UPDATE",
                GraphOperation::Delete => "DELETE",
            };
            let resource = match gp.resource {
                GraphResource::VertexLabel(id) => format!("NODES <label {id}>"),
                GraphResource::EdgeLabel(id) => format!("EDGES <label {id}>"),
                GraphResource::VertexProperty { label, property } => {
                    format!("NODES <label {label}> {{ property <{property}> }}")
                }
            };
            format!(
                "{operation} ON GRAPH <{}> {resource} for subject {subject_text}",
                gp.graph
            )
        }
    }
}
#[cfg(test)]
mod publication_tests {
    use super::*;
    use crate::facade::store::RouterStore;
    use candid::Principal;
    use gleaph_gql_ic::graph_registry::{GraphRegistryEntry, GraphStatus, ProvisioningState};

    /// Plan 0303 / GAP-2026-08-24-008 contract lock: the element-id-bearing demo
    /// projection extracts exactly the per-key property-read rows that a brace-form
    /// `GRANT READ … { … }` covers — no unattributed demand, so a non-owner holding the
    /// PUBLIC property-list grant executes the op.
    #[test]
    fn element_id_projection_demands_are_coverable_property_reads() {
        let owner = Principal::from_slice(&[7; 29]);
        let registrar = Principal::from_slice(&[255; 29]); // has MANAGE_CATALOG via grant_admins
        let graph = owned_graph(owner, "diag_eid");
        let store = crate::facade::store::RouterStore::new();
        store
            .admin_intern_vertex_label(registrar, "diag_eid", "Concept")
            .expect("intern Concept");
        store
            .admin_intern_edge_label(registrar, "diag_eid", "RELATED_TO")
            .expect("intern RELATED_TO");
        store
            .admin_intern_properties(
                registrar,
                "diag_eid",
                &["name".to_owned(), "definition".to_owned()],
            )
            .expect("intern properties");

        let source = "MATCH (a:Concept {name: 'Graph databases'})<-[e:RELATED_TO]-{1,3}(b:Concept) \
                      RETURN DISTINCT b.name AS concept, ELEMENT_ID(b) AS concept_id";
        let (cache, bound_graph) =
            crate::prepared::build_prepared_cache(source, owner, Some(graph)).expect("plan probeG");
        assert_eq!(bound_graph, graph);
        let reqs = crate::authz::extract_live(&store, &cache.plan, bound_graph);
        println!("DIAG RequirementSet for probeG:\n{reqs:#?}");
        let (unattributed, conj, alts) = reqs
            .test_demand_summary(graph.raw())
            .expect("graph demands present");
        assert!(!unattributed, "element-id reads must stay attributable");
        assert_eq!(conj, 5, "Match + Read + Traverse + ReadProperty×2");
        assert_eq!(alts, 0);
    }

    fn principal(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    /// Registers `graph_name` as the calling owner's HOME graph (a caps-less tenant
    /// principal) so prepared registration resolves graph context; the registrar holds
    /// the topology/catalog capabilities, mirroring provisioned flows.
    pub(crate) fn owned_graph(owner: Principal, graph_name: &str) -> GraphId {
        let store = RouterStore::new();
        let registrar = principal(u8::MAX);
        crate::facade::auth::grant_admins(&[registrar]);
        store
            .admin_register_graph(
                registrar,
                GraphRegistryEntry {
                    graph_id: GraphId::from_raw(0),
                    canister_id: Principal::management_canister(),
                    owner,
                    admins: Default::default(),
                    status: GraphStatus::Active,
                    version: 1,
                    updated_at_ns: 0,
                    provisioning_state: ProvisioningState::None,
                    is_home: true,
                },
                graph_name,
            )
            .expect("register fixture graph");
        graph_catalog::lookup_graph_id(graph_name).expect("interned graph id")
    }

    /// Interns the vocabulary the publication fixtures demand (MANAGE_CATALOG surface).
    pub(crate) fn intern_vocabulary(graph_name: &str) {
        let admin = principal(u8::MAX);
        let store = RouterStore::new();
        store
            .admin_intern_vertex_label(admin, graph_name, "Pub")
            .expect("intern Pub");
        store
            .admin_intern_edge_label(admin, graph_name, "KNOWS")
            .expect("intern KNOWS");
    }

    /// Grants exactly one capability bit to a fixture principal.
    fn upsert_caps(p: Principal, caps: gleaph_auth::AdminCaps) {
        crate::facade::stable::ROUTER_AUTH_STATE.with_borrow_mut(|state| {
            state.upsert_caps(p, caps).expect("non-anonymous principal");
        });
    }

    /// Executes one authorization statement through the production control path.
    pub(crate) fn execute_as(caller: Principal, text: &str) -> Result<(), RouterError> {
        let program = gleaph_gql::parser::parse(text).expect("parse authorization statement");
        let tx = program.transaction_activity.expect("transaction");
        execute_authorization_block(tx.body.as_ref().expect("block"), caller)
    }

    fn stored(subject: GrantSubject, name: &str) -> bool {
        auth::grant_contains(
            subject,
            &Privilege::ExecutePreparedQuery {
                name: name.to_owned(),
            },
        )
    }

    /// Registers one prepared record through the production batch core. Registration
    /// plans under `owner`'s graph context (the owner is the fixture's home-graph
    /// tenant); only the ingress `PREPARE_REGISTER` gate is bypassed.
    fn prepare_fixture(owner: Principal, name: &str, query: &str) {
        crate::prepared::prepare_batch_core(
            &[gleaph_prepared_api::PreparedRegistration {
                name: name.to_owned(),
                query: query.to_owned(),
                metadata: None,
            }],
            owner,
        )
        .expect("fixture registration");
    }

    #[test]
    fn owner_publishes_public_row_and_revoke_removes_exactly_it() {
        let owner = principal(1);
        owned_graph(owner, "publication_basic");
        prepare_fixture(owner, "pub_q", "RETURN 'x' AS tag");

        // Owner authority + trivially-covered requirement set (no data access).
        execute_as(owner, "GRANT EXECUTE ON PREPARED QUERY pub_q TO PUBLIC")
            .expect("owner publishes");
        assert!(stored(GrantSubject::Public, "pub_q"));

        // Exact-key revoke: removes exactly the targeted row.
        execute_as(owner, "REVOKE EXECUTE ON PREPARED QUERY pub_q FROM PUBLIC")
            .expect("owner revokes");
        assert!(!stored(GrantSubject::Public, "pub_q"));
        // Revoking a missing row is an exact-key miss and removes nothing else.
        let err = execute_as(owner, "REVOKE EXECUTE ON PREPARED QUERY pub_q FROM PUBLIC")
            .expect_err("second revoke must miss");
        assert!(matches!(err, RouterError::NotFound(_)));
    }

    #[test]
    fn non_owner_without_requirements_cannot_publish_but_constant_only_is_trivially_bounded() {
        let owner = principal(2);
        owned_graph(owner, "publication_gates");
        let publisher = principal(3); // PREPARE_REGISTER only: no tenancy, no grant rows
        upsert_caps(publisher, gleaph_auth::AdminCaps::PREPARE_REGISTER);

        prepare_fixture(owner, "scan_q", "MATCH (n) RETURN n");
        // Invariant 7 negative: the unlabeled scan demands tenancy-only coverage the
        // non-owner granter cannot present. Authority alone does not publish.
        let err = execute_as(
            publisher,
            "GRANT EXECUTE ON PREPARED QUERY scan_q TO PUBLIC",
        )
        .expect_err("uncovered requirements must deny publication");
        assert!(matches!(err, RouterError::Forbidden));
        assert!(!stored(GrantSubject::Public, "scan_q"));

        // A constant-only query demands nothing, so the bounded set is trivially covered:
        // the same PREPARE_REGISTER-only publisher may publish it.
        prepare_fixture(owner, "const_q", "RETURN 'x' AS tag");
        execute_as(
            publisher,
            "GRANT EXECUTE ON PREPARED QUERY const_q TO PUBLIC",
        )
        .expect("empty requirement set is publishable");
        assert!(stored(GrantSubject::Public, "const_q"));
    }

    #[test]
    fn owner_implicit_root_passes_invariant_7_without_stored_rows() {
        let owner = principal(4);
        owned_graph(owner, "publication_root");
        prepare_fixture(owner, "root_q", "MATCH (n) RETURN n");
        // The registry owner holds no caps and no grant rows at all — ownership IS the
        // implicit root of data-plane authority (ADR 0074 §3 invariant 3), evaluated at
        // publication time through the same coverage evaluation enforcement uses.
        assert_eq!(auth::caps_of(&owner), gleaph_auth::AdminCaps::empty());
        execute_as(owner, "GRANT EXECUTE ON PREPARED QUERY root_q TO PUBLIC")
            .expect("implicit ownership root admits publication");
        assert!(stored(GrantSubject::Public, "root_q"));
    }

    #[test]
    fn publication_converges_only_once_the_requirement_set_is_fully_covered() {
        let owner = principal(5);
        owned_graph(owner, "publication_coverage");
        intern_vocabulary("publication_coverage");
        let publisher = principal(6);
        upsert_caps(publisher, gleaph_auth::AdminCaps::PREPARE_REGISTER);
        prepare_fixture(owner, "pattern_q", "MATCH (:Pub)-[:KNOWS]->(:Pub)");

        // The publisher's rows arrive through the ordinary GRANT grammar (the owner
        // grants them) so coverage evaluation sees exactly what enforcement sees.
        let grant_publisher = |statement: String| {
            execute_as(owner, &statement)
                .unwrap_or_else(|e| panic!("grant failed for {statement}: {e:?}"));
        };
        let self_ref = format!("PRINCIPAL '{}'", publisher.to_text());

        // Nothing covered: denied.
        let err = execute_as(
            publisher,
            "GRANT EXECUTE ON PREPARED QUERY pattern_q TO PUBLIC",
        )
        .expect_err("empty coverage must deny");
        assert!(matches!(err, RouterError::Forbidden));

        // Partial coverage (no traversal rows yet): still denied — invariant 7 checks
        // every row of the static set, not merely that the granter holds some privilege.
        grant_publisher(format!(
            "GRANT MATCH ON GRAPH publication_coverage NODES Pub TO {self_ref}"
        ));
        grant_publisher(format!(
            "GRANT READ ON GRAPH publication_coverage NODES Pub TO {self_ref}"
        ));
        let err = execute_as(
            publisher,
            "GRANT EXECUTE ON PREPARED QUERY pattern_q TO PUBLIC",
        )
        .expect_err("partial coverage must deny");
        assert!(matches!(err, RouterError::Forbidden));

        // Complete coverage: publication succeeds.
        grant_publisher(format!(
            "GRANT TRAVERSE ON GRAPH publication_coverage EDGES KNOWS TO {self_ref}"
        ));
        execute_as(
            publisher,
            "GRANT EXECUTE ON PREPARED QUERY pattern_q TO PUBLIC",
        )
        .expect("full coverage admits publication");
        assert!(stored(GrantSubject::Public, "pattern_q"));
    }

    #[test]
    fn covered_requirements_never_substitute_for_publication_authority() {
        let owner = principal(7);
        owned_graph(owner, "publication_authority");
        prepare_fixture(owner, "auth_q", "RETURN 'x' AS tag");
        // An unrelated capability is not publication authority: no ownership, no
        // PREPARE_REGISTER — even though a constant-only set is trivially coverable.
        let stranger = principal(8);
        upsert_caps(stranger, gleaph_auth::AdminCaps::CALL_PROCEDURE);
        let err = execute_as(stranger, "GRANT EXECUTE ON PREPARED QUERY auth_q TO PUBLIC")
            .expect_err("requirement coverage never substitutes for authority");
        assert!(matches!(err, RouterError::Forbidden));
        assert!(!stored(GrantSubject::Public, "auth_q"));
    }

    #[test]
    fn anonymous_subject_literal_is_rejected_before_any_write() {
        let owner = principal(11);
        owned_graph(owner, "publication_anon");
        prepare_fixture(owner, "anon_q", "RETURN 'x' AS tag");
        let statement = format!(
            "GRANT EXECUTE ON PREPARED QUERY anon_q TO PRINCIPAL '{}'",
            Principal::anonymous().to_text()
        );
        let err = execute_as(owner, &statement)
            .expect_err("the anonymous principal cannot hold a stored grant");
        assert!(
            matches!(err, RouterError::InvalidArgument(_)),
            "got {err:?}"
        );
        assert!(!stored(GrantSubject::Public, "anon_q"));
    }
    #[test]
    fn listing_synthesizes_the_owner_implicit_root_marker_first() {
        let owner = principal(12);
        owned_graph(owner, "publication_introspection");
        intern_vocabulary("publication_introspection");
        prepare_fixture(owner, "intro_q", "RETURN 'x' AS tag");
        execute_as(owner, "GRANT EXECUTE ON PREPARED QUERY intro_q TO PUBLIC").expect("publish");
        // One stored graph row for contrast.
        execute_as(
            owner,
            "GRANT MATCH ON GRAPH publication_introspection NODES Pub TO PUBLIC",
        )
        .expect("grant graph row");

        let summaries = list_graph_grants("publication_introspection", owner).expect("list");
        assert_eq!(
            summaries[0],
            GraphGrantSummary {
                subject: GrantSubjectView::Principal(owner.to_text()),
                operation: GrantOperationView::ImplicitRoot,
                direction: None,
                resource: GrantResourceView {
                    kind: GrantResourceKindView::Graph,
                    label: "publication_introspection".to_owned(),
                    property: None,
                },
                expires_at_ns: None,
                predicate: None,
                evidence: None,
            },
            "the implicit-root marker leads the listing"
        );
        // Stored graph-scoped rows follow; EXECUTE publication rows are not
        // graph-scoped and are therefore not part of this graph listing.
        assert_eq!(summaries.len(), 2, "marker plus one stored graph row");

        // A stranger holding no row gets NotFound (ADR 0028); here PUBLIC graph rows make
        // the graph visible, so a visible non-owner gets the ordinary Forbidden instead.
        let stranger = principal(13);
        assert!(matches!(
            list_graph_grants("publication_introspection", stranger),
            Err(RouterError::Forbidden)
        ));
    }
}

#[cfg(test)]
mod conditional_policy_tests {
    //! Statement-level compilation of conditional selectors ([ADR 0075] §1–§2): every
    //! rejection happens before any stable write, stored predicates ride grant rows, and
    //! introspection prints the condition inline.

    use super::publication_tests::{execute_as, intern_vocabulary, owned_graph};
    use super::*;
    use crate::facade::auth;
    use crate::types::{GrantOperationView, GrantSubjectView};

    fn principal(byte: u8) -> Principal {
        Principal::from_slice(&[byte; 29])
    }

    fn graph_rows(graph_name: &str, owner: Principal) -> Vec<GraphGrantSummary> {
        list_graph_grants(graph_name, owner).expect("owner listing")
    }

    fn stored_predicates() -> Vec<(gleaph_auth::GrantSubject, Rc<CompiledPredicate>)> {
        auth::grant_rows()
            .into_iter()
            .filter_map(|row| row.predicate.map(|p| (row.subject, p)))
            .collect()
    }

    fn intern_post_vocabulary(graph_name: &str) {
        intern_vocabulary(graph_name);
        let admin = principal(u8::MAX);
        let store = RouterStore::new();
        store
            .admin_intern_properties(
                admin,
                graph_name,
                &[
                    "visibility".to_owned(),
                    "owner".to_owned(),
                    "stars".to_owned(),
                ],
            )
            .expect("intern properties");
    }

    #[test]
    fn conditional_grant_stores_compiled_predicate_and_prints_it_inline() {
        let owner = principal(21);
        owned_graph(owner, "policy_store");
        intern_post_vocabulary("policy_store");

        execute_as(
            owner,
            "GRANT READ ON GRAPH policy_store NODES Pub \
             FOR (p:Pub) WHERE p.visibility = 'public' AND p.owner = MSG_CALLER() TO PUBLIC",
        )
        .expect("conditional grant");

        // The compiled predicate rides the stored row ([ADR 0075] §1).
        let stored = stored_predicates();
        assert_eq!(stored.len(), 1, "one conditional row");
        assert_eq!(stored[0].0, GrantSubject::Public);
        let predicate = stored[0].1.as_ref();
        assert_eq!(predicate.label, 1, "interned Pub label id");
        assert_eq!(predicate.conjuncts.len(), 2);
        assert_eq!(
            predicate.conjuncts[0].value,
            PredicateValue::Literal(PredicateLiteral::String("public".to_owned()))
        );
        assert_eq!(predicate.conjuncts[1].value, PredicateValue::MsgCaller);

        // Introspection prints the condition inline with catalog-resolved names.
        let summaries = graph_rows("policy_store", owner);
        assert_eq!(summaries.len(), 2, "implicit root + one stored row");
        assert_eq!(
            summaries[1].predicate.as_deref(),
            Some("WHERE visibility = 'public' AND owner = MSG_CALLER()")
        );
        assert_eq!(summaries[1].operation, GrantOperationView::Read);
        assert_eq!(summaries[1].subject, GrantSubjectView::Public);
        assert_eq!(summaries[0].predicate, None);
    }

    #[test]
    fn unknown_property_rejects_before_any_write() {
        let owner = principal(22);
        owned_graph(owner, "policy_unknown_prop");
        intern_post_vocabulary("policy_unknown_prop");

        let err = execute_as(
            owner,
            "GRANT READ ON GRAPH policy_unknown_prop NODES Pub \
             FOR (p:Pub) WHERE p.nosuch = 1 TO PUBLIC",
        )
        .expect_err("unknown property must reject");
        assert!(matches!(err, RouterError::NotFound(_)), "got {err:?}");
        assert!(stored_predicates().is_empty(), "no partial write");
        assert_eq!(crate::facade::auth::grant_rows().len(), 0);
    }

    #[test]
    fn selector_label_mismatch_is_rejected() {
        let owner = principal(23);
        owned_graph(owner, "policy_label_mismatch");
        intern_post_vocabulary("policy_label_mismatch");

        let err = execute_as(
            owner,
            "GRANT MATCH ON GRAPH policy_label_mismatch NODES Pub \
             FOR (p:Knows) WHERE p.visibility = 'public' TO PUBLIC",
        )
        .expect_err("selector label must match the granted label");
        assert!(
            matches!(err, RouterError::InvalidArgument(_)),
            "got {err:?}"
        );
        assert!(stored_predicates().is_empty());
    }

    #[test]
    fn edge_form_selector_and_edge_resource_are_rejected_distinctly() {
        let owner = principal(24);
        owned_graph(owner, "policy_edge_form");
        intern_post_vocabulary("policy_edge_form");

        // Edge-pattern selector on a vertex resource: recognized syntax, distinct error.
        let err = execute_as(
            owner,
            "GRANT READ ON GRAPH policy_edge_form NODES Pub \
             FOR ()-[e:KNOWS]-() WHERE e.weight = 1 TO PUBLIC",
        )
        .expect_err("edge-form selectors are a later phase");
        let text = format!("{err}");
        assert!(text.contains("vertex"), "distinct boundary error: {text}");
        assert!(stored_predicates().is_empty());

        // Vertex selector on an edge resource cannot bind either.
        let err = execute_as(
            owner,
            "GRANT READ ON GRAPH policy_edge_form EDGES KNOWS \
             FOR (p:Pub) WHERE p.visibility = 'public' TO PUBLIC",
        )
        .expect_err("edge resources stay unconditional in this phase");
        assert!(format!("{err}").contains("NODES/VERTICES"));
    }

    #[test]
    fn non_match_read_privileges_cannot_carry_conditions() {
        let owner = principal(25);
        owned_graph(owner, "policy_privilege_scope");
        intern_post_vocabulary("policy_privilege_scope");

        let err = execute_as(
            owner,
            "GRANT UPDATE ON GRAPH policy_privilege_scope NODES Pub \
             FOR (p:Pub) WHERE p.visibility = 'public' TO PUBLIC",
        )
        .expect_err("conditions apply to MATCH/READ only in this phase");
        assert!(format!("{err}").contains("MATCH or READ"));
        assert!(stored_predicates().is_empty());

        // The unconditional form stays grantable.
        execute_as(
            owner,
            "GRANT UPDATE ON GRAPH policy_privilege_scope NODES Pub TO PUBLIC",
        )
        .expect("unconditional UPDATE still grants");
        assert!(stored_predicates().is_empty());
    }

    #[test]
    fn regranting_the_same_key_replaces_the_condition_last_write_wins() {
        let owner = principal(26);
        owned_graph(owner, "policy_replace");
        intern_post_vocabulary("policy_replace");
        let member = format!("PRINCIPAL '{}'", principal(28).to_text());

        // First condition: stars threshold.
        execute_as(
            owner,
            &format!(
                "GRANT READ ON GRAPH policy_replace NODES Pub \
                 FOR (p:Pub) WHERE p.stars >= 3 TO {member}"
            ),
        )
        .expect("conditional grant");

        // Re-granting the same canonical key replaces the stored condition.
        execute_as(
            owner,
            &format!(
                "GRANT READ ON GRAPH policy_replace NODES Pub \
                 FOR (p:Pub) WHERE p.visibility = 'public' AND p.owner = MSG_CALLER() TO {member}"
            ),
        )
        .expect("regrant replaces the condition");

        let rows = auth::grant_rows();
        assert_eq!(rows.len(), 1, "one rule per canonical key");
        let predicate = rows[0].predicate.as_ref().expect("conditional row");
        assert_eq!(predicate.conjuncts.len(), 2, "latest condition wins");
        assert_eq!(
            predicate.conjuncts[0].value,
            PredicateValue::Literal(PredicateLiteral::String("public".to_owned()))
        );

        // REVOKE removes rule and condition together; the condition text does not
        // address the key.
        execute_as(
            owner,
            &format!("REVOKE READ ON GRAPH policy_replace NODES Pub FROM {member}"),
        )
        .expect("exact-key revoke");
        assert!(auth::grant_rows().is_empty());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn traverse(direction: Option<Direction>, edge: u32) -> Privilege {
        Privilege::Graph(GraphPrivilege {
            graph: 7,
            operation: GraphOperation::Traverse(direction),
            resource: GraphResource::EdgeLabel(edge),
        })
    }

    #[test]
    fn omitted_direction_on_directed_edge_lowers_to_both_rows() {
        let lowered = lower_privileges(
            7,
            &GrantPrivilege::Traverse { direction: None },
            &ResolvedResource::EdgeLabel {
                id: 3,
                undirected: Some(false),
            },
            &[],
        )
        .expect("lower");
        assert_eq!(
            lowered,
            vec![
                traverse(Some(Direction::Outgoing), 3),
                traverse(Some(Direction::Incoming), 3),
            ]
        );
    }

    #[test]
    fn explicit_direction_on_directed_edge_lowers_to_one_row() {
        let lowered = lower_privileges(
            7,
            &GrantPrivilege::Traverse {
                direction: Some(GrantDirection::Incoming),
            },
            &ResolvedResource::EdgeLabel {
                id: 3,
                undirected: Some(false),
            },
            &[],
        )
        .expect("lower");
        assert_eq!(lowered, vec![traverse(Some(Direction::Incoming), 3)]);
    }

    #[test]
    fn direction_on_undirected_edge_is_rejected_unoriented_form_still_grantable() {
        let err = lower_privileges(
            7,
            &GrantPrivilege::Traverse {
                direction: Some(GrantDirection::Outgoing),
            },
            &ResolvedResource::EdgeLabel {
                id: 3,
                undirected: Some(true),
            },
            &[],
        )
        .expect_err("undirected rejects modifiers");
        assert!(
            matches!(err, RouterError::InvalidArgument(_)),
            "expected InvalidArgument, got {err:?}"
        );
        assert!(
            lower_privileges(
                7,
                &GrantPrivilege::Traverse { direction: None },
                &ResolvedResource::EdgeLabel {
                    id: 3,
                    undirected: Some(true),
                },
                &[],
            )
            .is_ok()
        );
    }

    #[test]
    fn direction_with_undeclared_directedness_fails_closed() {
        let err = lower_privileges(
            7,
            &GrantPrivilege::Traverse {
                direction: Some(GrantDirection::Outgoing),
            },
            &ResolvedResource::EdgeLabel {
                id: 3,
                undirected: None,
            },
            &[],
        )
        .expect_err("undeclared directedness must reject modifiers");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
    }

    #[test]
    fn direction_on_vertex_selector_is_rejected() {
        let err = lower_privileges(
            7,
            &GrantPrivilege::Traverse {
                direction: Some(GrantDirection::Outgoing),
            },
            &ResolvedResource::VertexLabel(9),
            &[],
        )
        .expect_err("directional modifiers apply to edges only");
        assert!(matches!(err, RouterError::InvalidArgument(_)));
    }

    #[test]
    fn read_with_properties_lowers_to_read_plus_read_property_rows() {
        let lowered = lower_privileges(
            7,
            &GrantPrivilege::Read {
                properties: vec!["a".to_owned(), "b".to_owned()],
            },
            &ResolvedResource::VertexLabel(9),
            &[30, 31],
        )
        .expect("lower");
        assert_eq!(
            lowered,
            vec![
                Privilege::Graph(GraphPrivilege {
                    graph: 7,
                    operation: GraphOperation::Read,
                    resource: GraphResource::VertexLabel(9),
                }),
                Privilege::Graph(GraphPrivilege {
                    graph: 7,
                    operation: GraphOperation::ReadProperty,
                    resource: GraphResource::VertexProperty {
                        label: 9,
                        property: 30
                    },
                }),
                Privilege::Graph(GraphPrivilege {
                    graph: 7,
                    operation: GraphOperation::ReadProperty,
                    resource: GraphResource::VertexProperty {
                        label: 9,
                        property: 31
                    },
                }),
            ]
        );
    }

    // ── Conditional policy compilation (ADR 0075 §1–§2) ──

    fn keyword(text: &str) -> gleaph_gql::ast::Keyword {
        gleaph_gql::ast::Keyword(text.to_owned())
    }

    #[test]
    fn literal_type_compatibility_classes() {
        use gleaph_gql::ast::ValueType as VT;
        let kw = || keyword("T");
        let string_ty = VT::String {
            min_length: None,
            max_length: None,
        };
        let bool_ty = VT::Bool { keyword: kw() };
        let int_ty = VT::Int64 { keyword: kw() };
        let float_ty = VT::Float64 { keyword: kw() };
        let decimal_ty = VT::Decimal {
            keyword: kw(),
            precision: None,
            scale: None,
        };
        let date_ty = VT::Date;

        let lit = |l| PredicateValue::Literal(l);
        let prop = "p";

        // Undeclared properties are open-world.
        for value in [
            PredicateValue::MsgCaller,
            lit(PredicateLiteral::Bool(true)),
            lit(PredicateLiteral::Int(1)),
            lit(PredicateLiteral::Float(1.5)),
            lit(PredicateLiteral::String("x".into())),
        ] {
            assert!(
                check_literal_compatibility(None, &value, prop).is_ok(),
                "undeclared property accepts {value:?}"
            );
        }

        // Declared types accept only their compatible classes.
        assert!(
            check_literal_compatibility(Some(&bool_ty), &lit(PredicateLiteral::Bool(true)), prop)
                .is_ok()
        );
        assert!(
            check_literal_compatibility(Some(&bool_ty), &lit(PredicateLiteral::Int(1)), prop)
                .is_err()
        );

        assert!(
            check_literal_compatibility(Some(&int_ty), &lit(PredicateLiteral::Int(-3)), prop)
                .is_ok()
        );
        assert!(
            check_literal_compatibility(Some(&int_ty), &lit(PredicateLiteral::Float(1.0)), prop)
                .is_err()
        );
        assert!(
            check_literal_compatibility(Some(&int_ty), &PredicateValue::MsgCaller, prop).is_err()
        );

        assert!(
            check_literal_compatibility(Some(&float_ty), &lit(PredicateLiteral::Float(2.5)), prop)
                .is_ok()
        );
        assert!(
            check_literal_compatibility(Some(&decimal_ty), &lit(PredicateLiteral::Int(7)), prop)
                .is_err()
        );

        assert!(
            check_literal_compatibility(
                Some(&string_ty),
                &lit(PredicateLiteral::String("v".into())),
                prop
            )
            .is_ok()
        );
        assert!(
            check_literal_compatibility(Some(&string_ty), &PredicateValue::MsgCaller, prop).is_ok()
        );

        // Temporal types accept no conditional-policy literal kind.
        assert!(
            check_literal_compatibility(Some(&date_ty), &lit(PredicateLiteral::Int(1)), prop)
                .is_err()
        );
        assert!(
            check_literal_compatibility(
                Some(&date_ty),
                &lit(PredicateLiteral::String("d".into())),
                prop
            )
            .is_err()
        );
    }
}
