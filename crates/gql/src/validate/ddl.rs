use crate::ast::*;

use super::graph_type::validate_graph_type_definition;
use super::{VResult, validate_catalog_object_name, verr};

pub(super) fn validate_create_schema(create: &CreateSchemaStatement) -> VResult {
    if create.name.parts.is_empty() {
        return Err(verr("CREATE SCHEMA requires a non-empty name"));
    }
    validate_catalog_object_name(&create.name)
}

pub(super) fn validate_create_graph(create: &CreateGraphStatement) -> VResult {
    if create.if_not_exists && create.or_replace {
        return Err(verr(
            "CREATE GRAPH: IF NOT EXISTS and OR REPLACE are mutually exclusive",
        ));
    }
    if create.name.parts.is_empty() {
        return Err(verr("CREATE GRAPH requires a non-empty name"));
    }
    validate_catalog_object_name(&create.name)?;
    // ISO/IEC 39075:2024 12.4: the format requires `{ <open graph type> | <of graph type> }`; only
    // the trailing `<graph source>` (`AS COPY OF`) is optional, so a bare `CREATE GRAPH g` is not
    // a statement the grammar can produce.
    if create.graph_type.is_none() && create.copy_of.is_none() {
        return Err(verr(
            "CREATE GRAPH: expected a graph type ('ANY', 'LIKE …', 'COPY OF …', 'TYPED …', or a nested '{ … }')",
        ));
    }
    if let Some(GraphTypeSpec::Inline(def)) = &create.graph_type {
        validate_graph_type_definition(def)?;
    }
    Ok(())
}

pub(super) fn validate_create_graph_type(create: &CreateGraphTypeStatement) -> VResult {
    if create.if_not_exists && create.or_replace {
        return Err(verr(
            "CREATE GRAPH TYPE: IF NOT EXISTS and OR REPLACE are mutually exclusive",
        ));
    }
    if create.name.parts.is_empty() {
        return Err(verr("CREATE GRAPH TYPE requires a non-empty name"));
    }
    validate_catalog_object_name(&create.name)?;
    match &create.source {
        GraphTypeSource::Nested { definition, .. } => validate_graph_type_definition(definition),
        GraphTypeSource::CopyOf { .. } | GraphTypeSource::Like(_) => Ok(()),
    }
}

pub(super) fn validate_drop_name(name: &ObjectName, stmt_label: &str) -> VResult {
    if name.parts.is_empty() {
        return Err(verr(&format!("{stmt_label} requires a non-empty name")));
    }
    validate_catalog_object_name(name)
}
