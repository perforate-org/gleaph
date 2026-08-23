//! GRANT/REVOKE execution on the Router control path ([ADR 0074] §5).
//!
//! The statements parse generically in `gleaph-gql` behind its `gleaph` feature; this
//! module is the integration layer that binds them to Router reality (ADR 0034):
//! registry ownership is the only grant authority, `PRINCIPAL` literals bind to IC
//! principals, and label/property names resolve through the target graph's catalogs.
//! Every statement in a block is validated and lowered **before** the first stable
//! write, so no validation failure can leave a partially applied block.
//!
//! Grant rows are canonical `(subject × privilege)` facts owned by [`crate::facade::auth`]'s
//! grant state. Plan-time enforcement against these rows is slice 2b — this slice only
//! makes rows creatable, listable, and revocable without changing query outcomes.
//!
//! [ADR 0074]: https://github.com/gleaph/gleaph/blob/main/design/adr/0074-data-plane-authorization-core.md

use candid::Principal;
use gleaph_auth::{
    Direction, GrantSubject, GraphOperation, GraphPrivilege, GraphResource, Privilege,
};
use gleaph_gql::ast::{
    CompositeQueryExpr, GrantDirection, GrantPrivilege, GrantResourceSelector, GrantStatement,
    GrantSubjectLiteral, LinearQueryStatement, ProcedureBindingInitializer, RevokeStatement,
    SimpleQueryStatement, Statement, StatementBlock,
};
use gleaph_graph_kernel::entry::{EdgeLabelId, GraphId, PropertyId, VertexLabelId};

use crate::facade::auth;
use crate::facade::stable::graph_type_catalog::try_property_schema_for_graph_id;
use crate::facade::store::RouterStore;
use crate::state::RouterError;
use crate::types::{
    GrantDirectionView, GrantOperationView, GrantResourceKindView, GrantResourceView,
    GrantSubjectView, GraphGrantSummary,
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
fn authorize_graph_owner(
    store: &RouterStore,
    graph_name: &str,
    caller: Principal,
) -> Result<GraphId, RouterError> {
    let entry = store.get_graph_operator(graph_name, caller)?;
    if entry.owner != caller {
        return Err(RouterError::Forbidden);
    }
    Ok(entry.graph_id)
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
    let graph_name = gleaph_graph_catalog::object_name_key(&stmt.graph);
    let graph_id = authorize_graph_owner(store, &graph_name, caller)?;
    let subject = bind_subject(&stmt.subject)?;
    let resource = resolve_resource(store, graph_id, &stmt.resource)?;
    let property_ids = resolve_property_ids(store, graph_id, &stmt.privilege)?;
    let privileges = lower_privileges(graph_id.raw(), &stmt.privilege, &resource, &property_ids)?;
    Ok(PlannedAuthorization::Grant {
        subject,
        privileges,
    })
}

fn plan_revoke(
    store: &RouterStore,
    caller: Principal,
    stmt: &RevokeStatement,
) -> Result<PlannedAuthorization, RouterError> {
    let graph_name = gleaph_graph_catalog::object_name_key(&stmt.graph);
    let graph_id = authorize_graph_owner(store, &graph_name, caller)?;
    let subject = bind_subject(&stmt.subject)?;
    let resource = resolve_resource(store, graph_id, &stmt.resource)?;
    let property_ids = resolve_property_ids(store, graph_id, &stmt.privilege)?;
    let privileges = lower_privileges(graph_id.raw(), &stmt.privilege, &resource, &property_ids)?;
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
        } => {
            for privilege in &privileges {
                auth::add_grant(subject, privilege, None)
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

/// Owner-only listing of one graph's stored grant rows (ADR 0074 §5 observability).
///
/// Rows referencing vocabulary that no longer resolves are skipped: ids are never reused
/// (ADR 0074 invariant 4), but cascade invalidation of dropped labels arrives with slice
/// 2b, so stale rows cannot be named until then.
pub(crate) fn list_graph_grants(
    graph_name: &str,
    caller: Principal,
) -> Result<Vec<GraphGrantSummary>, RouterError> {
    let store = RouterStore::new();
    let graph_id = authorize_graph_owner(&store, graph_name, caller)?;
    let mut summaries = Vec::new();
    for row in auth::grant_rows() {
        let Privilege::Graph(gp) = row.privilege else {
            continue;
        };
        if gp.graph != graph_id.raw() {
            continue;
        }
        let Some((operation, direction)) = operation_view(gp.operation) else {
            continue;
        };
        let Some(resource) = resource_view(&store, graph_id, gp.resource) else {
            continue;
        };
        summaries.push(GraphGrantSummary {
            subject: subject_view(row.subject),
            operation,
            direction,
            resource,
            expires_at_ns: row.expires_at_ns,
        });
    }
    Ok(summaries)
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
}
