use crate::ast::*;

use super::{validate_catalog_object_name, VResult, verr};

pub(super) fn validate_session_command(cmd: &SessionCommand) -> VResult {
    match cmd {
        SessionCommand::Set(set) => validate_session_set(set),
        // SESSION RESET and SESSION CLOSE have no semantic constraints beyond parsing.
        _ => Ok(()),
    }
}

pub(super) fn validate_session_set(set: &SessionSetCommand) -> VResult {
    match set {
        SessionSetCommand::Schema(on) | SessionSetCommand::Graph { name: on, .. } => {
            validate_catalog_object_name(on)
        }
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
