//! GQL graph type catalog on router stable memory (ADR 0013).

use std::collections::BTreeMap;
use std::collections::BTreeSet;

use gleaph_gql::ast::{
    GraphTypeDefinition, GraphTypeElement, GraphTypeSpec, Statement, StatementBlock,
};
use gleaph_gql::type_check::{GraphTypePropertySchema, NoSchema, PropertySchema};
use gleaph_gql::validate::SessionGraphSeed;
use gleaph_graph_catalog::{CatalogError, GraphNameLookup, object_name_key};
use gleaph_graph_kernel::entry::GraphId;
use gleaph_graph_kernel::plan_exec::EdgeOrderingPolicy;

use super::ROUTER_GQL_GRAPH_CATALOG;
use super::ROUTER_GRAPH_TYPE_CATALOG;
use super::graph_catalog::lookup_graph_id;
use super::graph_type_name_catalog::RouterGraphTypeLookup;
use crate::facade::store::RouterStore;
use crate::state::RouterError;

struct RouterGraphNameLookup;

impl GraphNameLookup for RouterGraphNameLookup {
    fn lookup_graph_id(&self, graph_name: &str) -> Option<GraphId> {
        lookup_graph_id(graph_name)
    }
}

pub(crate) fn catalog_error_to_router(err: CatalogError) -> RouterError {
    match err {
        CatalogError::GraphTypeExists(name) => RouterError::Conflict(name),
        CatalogError::GraphTypeNotFound(name) => RouterError::NotFound(name),
        CatalogError::GraphNotRegistered(name) => {
            RouterError::NotFound(format!("graph `{name}` is not registered"))
        }
        CatalogError::Unsupported(msg) => RouterError::InvalidArgument(msg),
        CatalogError::InvalidDefinition(msg) => RouterError::InvalidArgument(msg),
    }
}

pub(crate) fn block_has_catalog_ddl(block: &StatementBlock) -> bool {
    block.iter_statements().any(is_catalog_ddl_statement)
}

pub(crate) fn block_is_catalog_ddl_only(block: &StatementBlock) -> bool {
    block.iter_statements().all(is_catalog_ddl_statement)
}

fn is_catalog_ddl_statement(stmt: &Statement) -> bool {
    matches!(
        stmt,
        Statement::CreateGraphType(_)
            | Statement::CreateGraph(_)
            | Statement::DropGraphType(_)
            | Statement::DropGraph(_)
    )
}

pub(crate) fn apply_catalog_statement_block(
    block: &StatementBlock,
    source: &str,
) -> Result<(), RouterError> {
    for stmt in block.iter_statements() {
        match stmt {
            Statement::CreateGraph(create) => {
                if let Some(gleaph_gql::ast::GraphTypeSpec::Inline(definition)) = &create.graph_type
                {
                    validate_inline_graph_type_schemas(definition)?;
                    validate_edge_ordering_policies(definition)?;
                }
            }
            Statement::CreateGraphType(create) => {
                validate_edge_ordering_policies(&create.definition)?;
            }
            _ => {}
        }
    }
    enforce_policy_change_restrictions(block)?;
    ROUTER_GRAPH_TYPE_CATALOG.with_borrow_mut(|type_catalog| {
        ROUTER_GQL_GRAPH_CATALOG.with_borrow_mut(|catalog| {
            let mut type_lookup = RouterGraphTypeLookup::new(type_catalog);
            catalog
                .apply_statement_block(block, source, &RouterGraphNameLookup, &mut type_lookup)
                .map_err(catalog_error_to_router)?;
            for stmt in block.iter_statements() {
                if let Statement::CreateGraph(create) = stmt {
                    let graph_name = object_name_key(&create.name);
                    if let Some(graph_id) = lookup_graph_id(&graph_name)
                        && let Some(definition) = catalog
                            .graph_type_definition_for_graph_id(graph_id)
                            .map_err(catalog_error_to_router)?
                    {
                        RouterStore::commit_intern_graph_type_vocabulary(graph_id, &definition)?;
                    }
                }
            }
            Ok(())
        })
    })
}

/// Validates the per-label ordering policy declarations of a Graph Type definition
/// (ADR 0052 §1/§4): every `ORDER BY` key must be `INSERTION`, and a runtime label
/// declared by multiple edge elements must not carry conflicting policies. Runs
/// before any catalog mutation so an invalid definition fails closed without
/// partial state.
pub(crate) fn validate_edge_ordering_policies(
    definition: &gleaph_gql::ast::GraphTypeDefinition,
) -> Result<(), RouterError> {
    let mut declared: BTreeMap<&str, Option<&str>> = BTreeMap::new();
    for element in &definition.elements {
        let gleaph_gql::ast::GraphTypeElement::Edge(edge) = element else {
            continue;
        };
        let Some(label_set) = edge.label_set.as_ref() else {
            if edge.storage_order.is_some() {
                return Err(RouterError::InvalidArgument(
                    "edge ORDER BY clause requires LABEL(S) in the same declaration".into(),
                ));
            }
            continue;
        };
        let key = edge
            .storage_order
            .as_ref()
            .map(|clause| clause.key.as_str());
        for label in &label_set.labels {
            if let Some(prev) = declared.insert(label.as_str(), key)
                && prev != key
            {
                return Err(RouterError::InvalidArgument(format!(
                    "edge label '{label}' is declared with conflicting ORDER BY policies"
                )));
            }
            if let Some(k) = key
                && k != "INSERTION"
            {
                return Err(RouterError::InvalidArgument(format!(
                    "edge label '{label}' declares unsupported ORDER BY key '{k}'"
                )));
            }
        }
    }
    Ok(())
}

/// Resolves the ordering policy declared for `edge_label_name` inside
/// `definition` (ADR 0052 §1/§4). Returns `None` for undeclared labels;
/// unsupported `ORDER BY` keys and conflicting per-label declarations fail
/// closed. Shared by [`crate::facade::store::RouterStore::lookup_edge_ordering_policy`]
/// and the §10 DDL-time policy-change check.
pub(crate) fn edge_ordering_policy_in_definition(
    definition: &GraphTypeDefinition,
    edge_label_name: &str,
) -> Result<Option<EdgeOrderingPolicy>, RouterError> {
    let mut found: Option<EdgeOrderingPolicy> = None;
    for element in &definition.elements {
        let GraphTypeElement::Edge(edge) = element else {
            continue;
        };
        let Some(label_set) = edge.label_set.as_ref() else {
            continue;
        };
        if !label_set
            .labels
            .iter()
            .any(|label| label == edge_label_name)
        {
            continue;
        }
        let policy = match edge
            .storage_order
            .as_ref()
            .map(|clause| clause.key.as_str())
        {
            Some("INSERTION") => EdgeOrderingPolicy::Insertion,
            Some(other) => {
                return Err(RouterError::InvalidArgument(format!(
                    "edge label '{edge_label_name}' declares unsupported ORDER BY key '{other}'"
                )));
            }
            None => EdgeOrderingPolicy::Unordered,
        };
        if let Some(prev) = found
            && prev != policy
        {
            return Err(RouterError::InvalidArgument(format!(
                "edge label '{edge_label_name}' is declared with conflicting ORDER BY policies"
            )));
        }
        found = Some(policy);
    }
    Ok(found)
}

/// ADR 0052 §10: rejects a per-label ordering-policy change when the label has
/// live edges (aggregated `ROUTER_EDGE_LABEL_STATS` live_count > 0). Runs before
/// any catalog mutation so the DDL fails closed without partial state.
///
/// The label-stats projection is Telemetry-class event-sourced state, so a
/// projection-lag window fails open in the initial implementation (recorded in
/// ADR 0052 §10).
fn enforce_policy_change_restrictions(block: &StatementBlock) -> Result<(), RouterError> {
    let store = RouterStore::new();
    for stmt in block.iter_statements() {
        match stmt {
            Statement::CreateGraph(create) => {
                if !create.or_replace {
                    continue;
                }
                let Some(GraphTypeSpec::Inline(new_definition)) = &create.graph_type else {
                    continue;
                };
                let graph_name = object_name_key(&create.name);
                let Some(graph_id) = lookup_graph_id(&graph_name) else {
                    continue;
                };
                let Some(old_definition) = parsed_graph_type_definition_for_graph_id(graph_id)?
                else {
                    continue;
                };
                reject_changed_policies_with_live_edges(
                    &store,
                    graph_id,
                    &old_definition,
                    new_definition,
                )?;
            }
            Statement::CreateGraphType(create) => {
                if !create.or_replace {
                    continue;
                }
                let type_name = object_name_key(&create.name);
                let Some(type_id) =
                    ROUTER_GRAPH_TYPE_CATALOG.with_borrow(|catalog| catalog.get_id(&type_name))
                else {
                    continue;
                };
                let graph_ids = ROUTER_GQL_GRAPH_CATALOG
                    .with_borrow(|catalog| catalog.graphs_bound_to_type(type_id));
                for graph_id in graph_ids {
                    let Some(old_definition) = parsed_graph_type_definition_for_graph_id(graph_id)?
                    else {
                        continue;
                    };
                    reject_changed_policies_with_live_edges(
                        &store,
                        graph_id,
                        &old_definition,
                        &create.definition,
                    )?;
                }
            }
            _ => {}
        }
    }
    Ok(())
}

fn reject_changed_policies_with_live_edges(
    store: &RouterStore,
    graph_id: GraphId,
    old_definition: &GraphTypeDefinition,
    new_definition: &GraphTypeDefinition,
) -> Result<(), RouterError> {
    let mut labels: BTreeSet<&str> = BTreeSet::new();
    collect_edge_labels(old_definition, &mut labels);
    collect_edge_labels(new_definition, &mut labels);
    for label in labels {
        let old = edge_ordering_policy_in_definition(old_definition, label)?
            .unwrap_or(EdgeOrderingPolicy::Unordered);
        let new = edge_ordering_policy_in_definition(new_definition, label)?
            .unwrap_or(EdgeOrderingPolicy::Unordered);
        if old == new {
            continue;
        }
        // A label that was never interned for the graph cannot have live edges;
        // only interned labels reach the live-count check.
        let Ok(label_id) = store.lookup_edge_label_id(graph_id, label) else {
            continue;
        };
        let live_count = store.edge_label_stats(graph_id, label_id).live_count;
        if live_count > 0 {
            return Err(RouterError::InvalidArgument(format!(
                "edge label '{label}' changes its ORDER BY policy while it has {live_count} live edge(s); drop the edges first or keep the declared policy (ADR 0052 §10)"
            )));
        }
    }
    Ok(())
}

fn collect_edge_labels<'a>(definition: &'a GraphTypeDefinition, out: &mut BTreeSet<&'a str>) {
    for element in &definition.elements {
        let GraphTypeElement::Edge(edge) = element else {
            continue;
        };
        let Some(label_set) = edge.label_set.as_ref() else {
            continue;
        };
        out.extend(label_set.labels.iter().map(|label| label.as_str()));
    }
}

/// Returns the parsed Graph Type definition bound to the graph, if any.
/// Parsed definitions are heap caches rebuilt from source strings (ADR 0013);
/// this is derived state and never a second source of truth.
pub(crate) fn parsed_graph_type_definition_for_graph_id(
    graph_id: GraphId,
) -> Result<Option<gleaph_gql::ast::GraphTypeDefinition>, RouterError> {
    ROUTER_GQL_GRAPH_CATALOG.with_borrow(|catalog| {
        catalog
            .graph_type_definition_for_graph_id(graph_id)
            .map_err(catalog_error_to_router)
    })
}

fn validate_inline_graph_type_schemas(
    definition: &gleaph_gql::ast::GraphTypeDefinition,
) -> Result<(), RouterError> {
    for element in &definition.elements {
        let gleaph_gql::ast::GraphTypeElement::Edge(edge) = element else {
            continue;
        };
        let count = edge
            .properties
            .iter()
            .filter(|property| property.inline)
            .count();
        if count > 1 {
            return Err(RouterError::InvalidArgument(format!(
                "edge type `{}` declares more than one INLINE property",
                edge.name.as_deref().unwrap_or("<unnamed>")
            )));
        }
    }
    Ok(())
}

pub(crate) fn try_property_schema_for_graph_id(
    graph_id: GraphId,
) -> Result<Option<GraphTypePropertySchema>, RouterError> {
    ROUTER_GQL_GRAPH_CATALOG.with_borrow(|catalog| {
        catalog
            .try_property_schema_for_graph_id(graph_id)
            .map_err(catalog_error_to_router)
    })
}

/// Rebuild parsed graph-type definitions into heap caches after upgrade.
pub(crate) fn rebuild_caches_after_upgrade() {
    ROUTER_GQL_GRAPH_CATALOG.with_borrow(|catalog| {
        catalog.rebuild_caches_after_upgrade();
    });
}

/// Resolve the [`PropertySchema`] passed to the planner for one logical graph.
pub(crate) fn property_schema_for_planning<'a>(
    graph_id: GraphId,
    open: &'a NoSchema,
    typed: &'a mut Option<GraphTypePropertySchema>,
) -> Result<&'a dyn PropertySchema, RouterError> {
    *typed = try_property_schema_for_graph_id(graph_id)?;
    Ok(if let Some(schema) = typed {
        schema as &dyn PropertySchema
    } else {
        open as &dyn PropertySchema
    })
}

/// Schema-aware validation for the defocused ingress block (ADR 0013 §4).
pub(crate) fn validate_block_schema_for_graph(
    block: &StatementBlock,
    seed: &SessionGraphSeed,
    graph_id: GraphId,
) -> Result<(), RouterError> {
    let open = NoSchema;
    let mut typed = None;
    let schema = property_schema_for_planning(graph_id, &open, &mut typed)?;
    gleaph_gql::validate::validate_block_schema_with_seed(block, Some(seed), schema)
        .map_err(|e| RouterError::InvalidArgument(e.to_string()))
}
