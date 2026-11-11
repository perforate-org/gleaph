use crate::ast::*;

use super::{VResult, validate_catalog_object_name, verr};

pub(super) fn validate_session_command(cmd: &SessionCommand) -> VResult {
    match cmd {
        SessionCommand::Set(set) => validate_session_set(set),
        // SESSION RESET and SESSION CLOSE have no semantic constraints beyond parsing.
        _ => Ok(()),
    }
}

pub(super) fn validate_session_set(set: &SessionSetCommand) -> VResult {
    match set {
        SessionSetCommand::Schema(reference) => validate_schema_reference(reference),
        SessionSetCommand::Graph { name: on, .. } => validate_catalog_object_name(on),
        SessionSetCommand::Parameter { name, .. }
        | SessionSetCommand::GraphParameter { name, .. }
        | SessionSetCommand::BindingTableParameter { name, .. } => {
            if name.is_empty() {
                return Err(verr("SESSION SET parameter name must not be empty"));
            }
            Ok(())
        }
        SessionSetCommand::TimeZone(_) => Ok(()),
    }
}

/// Validates a GQL §17.1 `schemaReference`.
///
/// A schema reference is not one catalog object name (`HOME_SCHEMA`,
/// `CURRENT_SCHEMA`, `.`, relative/absolute paths, or `$$param`), so each path
/// segment is checked with the catalog-name limits instead of reusing
/// [`validate_catalog_object_name`] on a single name.
fn validate_schema_reference(reference: &SchemaReference) -> VResult {
    match reference {
        SchemaReference::Current(_) => Ok(()),
        SchemaReference::Absolute(parts) | SchemaReference::Relative(parts) => {
            for part in parts {
                crate::name_limits::validate_catalog_name_part(part)
                    .map_err(|e| verr(&e.to_string()))?;
            }
            Ok(())
        }
        SchemaReference::Parameter(name) => {
            if name.is_empty() {
                return Err(verr(
                    "SESSION SET SCHEMA parameter reference must not be empty",
                ));
            }
            Ok(())
        }
    }
}
