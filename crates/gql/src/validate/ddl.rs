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
    validate_graph_type_definition(&create.definition)
}

pub(super) fn validate_drop_name(name: &ObjectName, stmt_label: &str) -> VResult {
    if name.parts.is_empty() {
        return Err(verr(&format!("{stmt_label} requires a non-empty name")));
    }
    validate_catalog_object_name(name)
}
