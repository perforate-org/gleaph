//! GQL graph type catalog on router stable memory (ADR 0013).

use gleaph_gql::ast::{Statement, StatementBlock};
use gleaph_gql::type_check::{GraphTypePropertySchema, NoSchema, PropertySchema};
use gleaph_gql::validate::SessionGraphSeed;
use gleaph_graph_catalog::{CatalogError, GraphNameLookup};
use gleaph_graph_kernel::entry::GraphId;

use super::ROUTER_GQL_GRAPH_CATALOG;
use super::ROUTER_GRAPH_TYPE_CATALOG;
use super::graph_catalog::lookup_graph_id;
use super::graph_type_name_catalog::RouterGraphTypeLookup;
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

pub(crate) fn apply_catalog_statement_block(block: &StatementBlock) -> Result<(), RouterError> {
    ROUTER_GRAPH_TYPE_CATALOG.with_borrow_mut(|type_catalog| {
        ROUTER_GQL_GRAPH_CATALOG.with_borrow_mut(|catalog| {
            let mut type_lookup = RouterGraphTypeLookup::new(type_catalog);
            catalog
                .apply_statement_block(block, &RouterGraphNameLookup, &mut type_lookup)
                .map_err(catalog_error_to_router)
        })
    })
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
